package httpapi

// 远程执行（Remote Cloud Computer / 云电脑）—— 让别人把任务丢到**这台机器**上跑。
//
// 一句话分工：云电脑是**算力在客户自己机器上**的形态（数据面不经过 NCC）；
// 本服务只做三件事：说清自己能跑什么（kinds）、接活（runs）、如实回报（日志 + 退出码）。
//
// 红线（与 `prd/ncc-sandbox.md` 对齐，改代码前先读）：
//  1. **执行权是最高权限**：`wasm` 是沙箱；`process` / `container` 是真的在这台机器上
//     跑别人的命令 —— 默认**只放行 wasm**，其余要运维显式打开（NCCR_EXEC_ALLOW）。
//  2. **每条任务都要 reason**：日志与审计要能回答「谁让这台机器干了什么、为什么」。
//  3. **任务不是制品**：任务是一次动作 + 日志 + 退出码；制品仍走制品托管那套。
//  4. **不转派**（不做任务经纪）、**不代收业务数据**：本服务只跑在本机。
//  5. **取消要真停**：进程组整组回收（`internal/execrun`），不是把状态改成 canceled 了事。

import (
	"archive/zip"
	"bytes"
	"compress/gzip"
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/fusedmodel/ncc-registry/internal/execrun"
	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

const (
	maxExecUpload  = 32 << 20 // 包字节上限（32MB；比名片的 8MB 宽，因为要装真包）
	maxExecReason  = 400
	execLogTailMax = 64 << 10 // 状态接口里回带的日志尾巴上限
)

// execJob 正在跑的任务：留着 ctx 的取消函数 + 进程 pid，好让 DELETE 真的把它停掉。
type execJob struct {
	cancel context.CancelFunc
	pid    int
}

// execMu / execJobs 进程内的在跑任务表（重启即清空 —— 重启后 running 的行会被
// 启动时标成 failed，见 `staleExecRuns`）。
var (
	execMu   sync.Mutex
	execJobs = map[string]*execJob{}
)

func execSet(id string, j *execJob) {
	execMu.Lock()
	execJobs[id] = j
	execMu.Unlock()
}

func execGet(id string) *execJob {
	execMu.Lock()
	defer execMu.Unlock()
	return execJobs[id]
}

func execDrop(id string) {
	execMu.Lock()
	delete(execJobs, id)
	execMu.Unlock()
}

/* ---------------- 能力自述 ---------------- */

// Exec 这台机器的执行能力（每次都现探：runner / 容器运行时要能在装好之后立刻生效，
// 不该因为启动时没装就一直说"不可用"）。
func (s *Server) Exec() execrun.Capability { return execrun.Probe(s.Cfg) }

// execKinds GET /api/exec/kinds —— **这台机器到底能跑什么**（公开，不鉴权）。
//
// 为什么公开：路由要按「真的能跑」挑机器，而不是按节点自己贴的标签猜
// （标签是声明，这里回答的是本机事实）。里面没有秘密：只有引擎名、限额与标签。
func (s *Server) execKinds(c *gin.Context) {
	cap := s.Exec()
	anyOn := false
	for _, k := range cap.Kinds {
		if k.Enabled {
			anyOn = true
		}
	}
	ok(c, 200, gin.H{
		"ok":   true,
		"node": gin.H{"id": s.Cfg.NodeID, "name": s.Cfg.NodeName, "region": s.Cfg.NodeRegion, "url": s.Cfg.PublicURL},
		"exec": cap,
		"limits": gin.H{
			"timeoutMs":      cap.TimeoutMS,
			"maxOutputBytes": cap.MaxOutput,
			"maxUploadBytes": maxExecUpload,
		},
		"anyEnabled": anyOn,
		"note": "engine=wasm 跑 HUR 包（沙箱，限额由包内策略算）；" +
			"process/container 跑一条命令（OS 敏感任务，例如 docker build && docker push），" +
			"必须运维显式放行（NCCR_EXEC_ALLOW）—— 默认只放行 wasm。",
	})
}

/* ---------------- 接活 ---------------- */

type execCreateReq struct {
	Engine  string `json:"engine"`
	Kind    string `json:"kind"` // cmd | package
	Cmd     string `json:"cmd"`
	Image   string `json:"image"`
	Reason  string `json:"reason"`
	Timeout int64  `json:"timeoutSec"`
}

// createExecRun POST /api/exec/runs —— 接一个任务。
//
// 两种载荷：multipart `file`（.hur 包 → wasm 引擎）或 JSON（`engine` + `cmd`[+`image`]）。
func (s *Server) createExecRun(c *gin.Context) {
	a := authOf(c)
	if a == nil {
		fail(c, 401, "unauthorized", "未认证或凭据无效（先 ncc login / ncc sandbox init，或带 API-KEY）")
		return
	}
	cap := s.Exec()

	var (
		req  execCreateReq
		body []byte
		pkg  bool
	)
	if strings.HasPrefix(c.GetHeader("Content-Type"), "multipart/form-data") {
		fh, err := c.FormFile("file")
		if err != nil {
			fail(c, 400, "bad_request", "缺少 file 字段（或在 JSON body 里给 cmd）")
			return
		}
		if fh.Size > maxExecUpload {
			fail(c, 413, "payload_too_large", "包超过 32MB")
			return
		}
		f, err := fh.Open()
		if err != nil {
			fail(c, 500, "internal", "读取上传失败")
			return
		}
		defer f.Close()
		if body, err = io.ReadAll(io.LimitReader(f, maxExecUpload+1)); err != nil {
			fail(c, 500, "internal", "读取上传失败")
			return
		}
		req.Engine = firstNonEmpty(c.PostForm("engine"), "wasm")
		req.Reason = c.PostForm("reason")
		req.Timeout = int64(atoiDefault(c.PostForm("timeoutSec"), 0))
		pkg = true
	} else if strings.HasPrefix(c.GetHeader("Content-Type"), "application/octet-stream") {
		// 客户端（`ncc sandbox run --package`）走这条：原样字节 + 查询串。
		// 为什么不做 multipart：CLI 的 HTTP 层只支持 JSON 或原样字节两形态，
		// 为它一个人引一套 multipart 编码不划算（与 /api/agent-cards 同一取舍）。
		b, err := io.ReadAll(io.LimitReader(c.Request.Body, maxExecUpload+1))
		if err != nil {
			fail(c, 500, "internal", "读取上传失败")
			return
		}
		body = b
		req.Engine = firstNonEmpty(c.Query("engine"), "wasm")
		req.Reason = c.Query("reason")
		req.Timeout = int64(atoiDefault(c.Query("timeoutSec"), 0))
		pkg = true
	} else {
		if err := c.ShouldBindJSON(&req); err != nil {
			fail(c, 400, "bad_request", "请求体格式错误（JSON 需要 engine + cmd）")
			return
		}
		req.Engine = firstNonEmpty(strings.TrimSpace(req.Engine), "process")
	}

	if strings.TrimSpace(req.Engine) == "" {
		req.Engine = "process"
	}
	if !model.ValidExecEngine(req.Engine) {
		fail(c, 400, "bad_request", "不认识的引擎（可选："+strings.Join(model.ExecEngines, "/")+")")
		return
	}
	// ⚠️ reason 必填：与 R12 同一条规矩（写操作必须说明为什么）。
	if strings.TrimSpace(req.Reason) == "" {
		fail(c, 400, "bad_request", "缺少 reason：这台机器替谁跑、为什么跑，要留在账本里")
		return
	}
	// 放行检查放在最前面：没放行的引擎**连字节都不收**。
	if !cap.Allowed(req.Engine) {
		fail(c, 403, "engine_not_allowed",
			"这台机器没有放行 "+req.Engine+" 引擎（运维要显式打开：NCCR_EXEC_ALLOW 加上它）")
		return
	}
	if !cap.Enabled(req.Engine) {
		why := ""
		for _, k := range cap.Kinds {
			if k.ID == req.Engine {
				why = k.Why
			}
		}
		fail(c, 409, "engine_unavailable", req.Engine+" 引擎当前不可用："+why)
		return
	}

	timeout := s.Cfg.ExecTimeout
	if req.Timeout > 0 {
		want := time.Duration(req.Timeout) * time.Second
		if want > s.Cfg.ExecTimeout {
			fail(c, 400, "bad_request", fmt.Sprintf("timeoutSec 不能超过节点上限（%d 秒）", int64(s.Cfg.ExecTimeout.Seconds())))
			return
		}
		timeout = want
	}

	runID := store.NewID("ER")
	work := filepath.Join(s.Cfg.ExecDir, runID)
	if err := os.MkdirAll(filepath.Join(work, ".tmp"), 0o755); err != nil {
		fail(c, 500, "internal", "创建工作目录失败")
		return
	}
	plan := execrun.Plan{Engine: req.Engine, WorkDir: work}

	switch {
	case pkg:
		if req.Engine != "wasm" {
			fail(c, 400, "bad_request", "上传 .hur 只能配 wasm 引擎；要跑命令请用 --cmd + --engine")
			return
		}
		if len(body) == 0 {
			fail(c, 400, "bad_request", "上传内容为空")
			return
		}
		pkgDir := filepath.Join(work, "pkg")
		if err := unpackHur(body, pkgDir); err != nil {
			_ = os.RemoveAll(work)
			fail(c, 400, "bad_request", "不是一个能解开的 hur 包："+err.Error())
			return
		}
		plan.Kind, plan.PackageDir = "package", pkgDir
	default:
		if strings.TrimSpace(req.Cmd) == "" {
			fail(c, 400, "bad_request", "缺少 cmd（或上传一个 .hur 包）")
			return
		}
		plan.Kind, plan.Command, plan.Image = "cmd", req.Cmd, strings.TrimSpace(req.Image)
	}

	logPath := filepath.Join(work, "output.log")
	rec, err := s.St.CreateExecRun(store.ExecRunInput{
		OwnerID: a.UserID, RequestedBy: a.Email,
		Engine: req.Engine, Kind: plan.Kind, Spec: clampRunesCard(plan.Spec(), 400), Image: plan.Image,
		WorkDir: work, LogPath: logPath,
		Reason: clampRunesCard(req.Reason, maxExecReason), TimeoutSec: int64(timeout.Seconds()),
	})
	if err != nil {
		_ = os.RemoveAll(work)
		fail(c, 500, "internal", "创建任务失败")
		return
	}
	s.ensureAdmin(c)
	s.audit(c, model.ActExecSubmit, rec.ID, rec.Spec, "提交远程执行任务",
		gin.H{"engine": req.Engine, "kind": plan.Kind, "reason": rec.Reason, "timeoutSec": rec.TimeoutSec})

	go s.runExec(context.Background(), rec, plan, timeout, logPath)
	ok(c, 201, gin.H{"run": s.execJSON(rec), "kindsUrl": "/api/exec/kinds"})
}

// runExec 后台执行（goroutine）。状态一路如实落库。
func (s *Server) runExec(parent context.Context, rec *model.ExecRun, plan execrun.Plan, timeout time.Duration, logPath string) {
	ctx, cancel := context.WithTimeout(parent, timeout)
	defer cancel()
	j := &execJob{cancel: cancel}
	execSet(rec.ID, j)
	defer execDrop(rec.ID)

	_ = s.St.MarkExecRunning(rec.ID)
	cap := s.Exec()
	res := cap.Run(ctx, plan, logPath, func(pid int) {
		// 进程起来了：把 pid 登记进去，DELETE 才能真的把它停掉。
		execMu.Lock()
		if cur := execJobs[rec.ID]; cur != nil {
			cur.pid = pid
		}
		execMu.Unlock()
	})
	errMsg := ""
	if res.Err != nil {
		errMsg = res.Err.Error()
		if res.Status == model.ExecTimeout {
			errMsg = fmt.Sprintf("超过墙上限 %s 被终止", timeout)
		}
	}
	// FinishExecRun 会在「已被取消」时拒绝覆盖（取消是用户意志，不该被后到的完成事件改回去）。
	if err := s.St.FinishExecRun(rec.ID, res.Status, res.ExitCode, res.LogBytes, res.Truncated, errMsg); err != nil {
		logf("[ncc-registry] exec %s 落终态失败（可能已被取消）：%v", rec.ID, err)
	}
}

// listExecRuns GET /api/exec/runs —— 我的任务（管理员可 ?all=1 看全部）。
func (s *Server) listExecRuns(c *gin.Context) {
	a := authOf(c)
	owner := ""
	if a != nil {
		owner = a.UserID
	}
	if c.Query("all") == "1" {
		if !s.ensureAdmin(c) {
			fail(c, 403, "admin_required", "查看全部任务需要节点管理员身份")
			return
		}
		owner = ""
	} else if owner == "" {
		fail(c, 401, "unauthorized", "未认证或凭据无效（先 ncc login / ncc sandbox init）")
		return
	}
	limit, offset := pageParams(c)
	rows, err := s.St.ListExecRuns(owner, limit, offset)
	if err != nil {
		fail(c, 500, "internal", "服务内部错误")
		return
	}
	total, _ := s.St.CountExecRuns(owner)
	list := make([]gin.H, 0, len(rows))
	for i := range rows {
		list = append(list, s.execJSON(&rows[i]))
	}
	ok(c, 200, gin.H{"runs": list, "total": total, "limit": limit, "offset": offset})
}

// getExecRun GET /api/exec/runs/:id —— 状态（带日志尾巴，省一次请求）。
func (s *Server) getExecRun(c *gin.Context) {
	rec, ok2 := s.execFor(c)
	if !ok2 {
		return
	}
	out := s.execJSON(rec)
	if rec.LogPath != "" {
		if tail, truncated, err := readTail(rec.LogPath, execLogTailMax); err == nil {
			out["logTail"] = tail
			out["logTailTruncated"] = truncated
		}
	}
	ok(c, 200, gin.H{"run": out})
}

// execRunLog GET /api/exec/runs/:id/log —— 日志全文（text/plain）。
//
// 日志是任务的主要产物之一：CI 里 `curl` 一下就拿到，不必解析 JSON。
func (s *Server) execRunLog(c *gin.Context) {
	rec, ok2 := s.execFor(c)
	if !ok2 {
		return
	}
	b, err := os.ReadFile(rec.LogPath)
	if err != nil {
		c.String(404, "日志不存在（任务可能还没开始，或被清理了）")
		return
	}
	c.Header("X-NCC-Exec-Status", rec.Status)
	c.Header("X-NCC-Exec-Exit", strconv.Itoa(rec.ExitCode))
	if rec.LogTruncated {
		c.Header("X-NCC-Exec-Truncated", "1")
	}
	c.Data(200, "text/plain; charset=utf-8", b)
}

// execRunAction DELETE /api/exec/runs/:id —— 取消（还在跑）或删除（已结束，?purge=1 连工作目录）。
func (s *Server) execRunAction(c *gin.Context) {
	a := authOf(c)
	rec, err := s.St.FindExecRun(c.Param("id"))
	if err != nil {
		fail(c, 404, "not_found", "任务不存在")
		return
	}
	// 管理员可以处置任何任务（治理面）；普通用户只能动自己的。
	admin := s.ensureAdmin(c)
	if !admin && (a == nil || rec.OwnerID != a.UserID) {
		fail(c, 403, "forbidden", "只能处置自己提交的任务")
		return
	}
	if rec.Done() {
		if c.Query("purge") == "1" {
			_ = os.RemoveAll(rec.WorkDir)
			if err := s.St.DeleteExecRun(rec.ID, ""); err != nil {
				fail(c, 500, "internal", "删除失败")
				return
			}
			ok(c, 200, gin.H{"ok": true, "id": rec.ID, "deleted": true})
			return
		}
		ok(c, 200, gin.H{"ok": true, "id": rec.ID, "status": rec.Status, "note": "任务已结束，无需取消（?purge=1 可连日志一起删）"})
		return
	}
	if j := execGet(rec.ID); j != nil {
		j.cancel() // 进程由 execrun 的进程组回收负责
	}
	canceled, err := s.St.CancelExecRun(rec.ID, "")
	if err != nil {
		fail(c, 500, "internal", "取消失败")
		return
	}
	s.audit(c, model.ActExecCancel, rec.ID, rec.Spec, "取消远程执行任务", gin.H{"canceled": canceled})
	ok(c, 200, gin.H{"ok": true, "id": rec.ID, "canceled": canceled})
}

/* ---------------- 视图与内部工具 ---------------- */

func (s *Server) execFor(c *gin.Context) (*model.ExecRun, bool) {
	rec, err := s.St.FindExecRun(c.Param("id"))
	if err != nil {
		fail(c, 404, "not_found", "任务不存在")
		return nil, false
	}
	a := authOf(c)
	if a != nil && rec.OwnerID == a.UserID {
		return rec, true
	}
	if s.ensureAdmin(c) {
		return rec, true
	}
	// 任务详情里有日志（可能含敏感信息）→ 只有发起者与管理员能看。
	fail(c, 403, "forbidden", "只能看自己提交的任务（管理员可看全部）")
	return nil, false
}

func (s *Server) execJSON(r *model.ExecRun) gin.H {
	out := gin.H{
		"id": r.ID, "engine": r.Engine, "kind": r.Kind, "spec": r.Spec,
		"status": r.Status, "exitCode": r.ExitCode,
		"reason": r.Reason, "timeoutSec": r.TimeoutSec,
		"durationMs": r.DurationMs(),
		"logBytes":   r.LogBytes, "logTruncated": r.LogTruncated,
		"logUrl":  "/api/exec/runs/" + r.ID + "/log",
		"workDir": r.WorkDir,
		"startedAt":   r.StartedAt, "finishedAt": r.FinishedAt, "createdAt": r.CreatedAt,
	}
	if r.Error != "" {
		out["error"] = r.Error
	}
	if r.Image != "" {
		out["image"] = r.Image
	}
	if r.PackageID != "" {
		out["package"] = gin.H{"id": r.PackageID, "version": r.PackageVersion, "sha256": r.PackageSHA}
	}
	return out
}

// readTail 读文件最后 n 字节（日志尾巴）。返回内容、是否被截断（前面还有更多）。
func readTail(path string, n int64) (string, bool, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", false, err
	}
	defer f.Close()
	st, err := f.Stat()
	if err != nil {
		return "", false, err
	}
	size := st.Size()
	off := int64(0)
	cut := false
	if size > n {
		off, cut = size-n, true
	}
	if _, err := f.Seek(off, io.SeekStart); err != nil {
		return "", false, err
	}
	b, err := io.ReadAll(io.LimitReader(f, n))
	return string(b), cut, err
}

// unpackHur 把 `.hur`（gzip 包住的 zip，或裸 zip）解到 dest。
//
// ⚠️ 防目录穿越：只接受相对路径、且拒绝任何 `..` 段 —— 这是别人上传的字节，
// 解包写盘是唯一一处「上传的内容变成了这台机器上的文件」，必须按不可信处理。
func unpackHur(raw []byte, dest string) error {
	zipBytes := raw
	if len(raw) >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
		zr, err := gzip.NewReader(bytes.NewReader(raw))
		if err != nil {
			return fmt.Errorf("gzip 层打不开（不是有效的 .hur）")
		}
		defer zr.Close()
		// 解压上限 256MB：压缩炸弹在这里被挡住。
		b, err := io.ReadAll(io.LimitReader(zr, 256<<20))
		if err != nil {
			return fmt.Errorf("gzip 层解压失败")
		}
		zipBytes = b
	}
	zr, err := zip.NewReader(bytes.NewReader(zipBytes), int64(len(zipBytes)))
	if err != nil {
		return fmt.Errorf("既不是 gzip 也不是 zip —— 这不是一个 hur 包")
	}
	if err := os.MkdirAll(dest, 0o755); err != nil {
		return err
	}
	for _, f := range zr.File {
		name := f.Name
		clean := filepath.Clean("/" + name) // 归一后去掉开头的 /
		if strings.Contains(name, "..") || filepath.IsAbs(name) {
			return fmt.Errorf("包里有不安全的路径：%s", name)
		}
		target := filepath.Join(dest, clean)
		if !strings.HasPrefix(target, filepath.Clean(dest)+string(os.PathSeparator)) {
			return fmt.Errorf("包里有越界路径：%s", name)
		}
		if f.FileInfo().IsDir() {
			if err := os.MkdirAll(target, 0o755); err != nil {
				return err
			}
			continue
		}
		if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
			return err
		}
		rc, err := f.Open()
		if err != nil {
			return err
		}
		out, err := os.Create(target)
		if err != nil {
			rc.Close()
			return err
		}
		_, cerr := io.Copy(out, io.LimitReader(rc, 64<<20))
		rc.Close()
		out.Close()
		if cerr != nil {
			return cerr
		}
	}
	return nil
}

// staleExecRuns 启动时把「上次进程死掉时还在跑」的任务标成 failed。
//
// 不这么做的话，重启后那些行会永远停在 running —— 调用方会一直等一个已经不存在
// 的进程（`ncc sandbox run` 会等到超时才发现没人回话）。
func (s *Server) staleExecRuns() {
	rows, err := s.St.ListExecRuns("", 200, 0)
	if err != nil {
		return
	}
	for i := range rows {
		if rows[i].Status == model.ExecQueued || rows[i].Status == model.ExecRunning {
			_ = s.St.FinishExecRun(rows[i].ID, model.ExecFailed, -1, 0, false, "节点重启：任务在上一进程里中断")
		}
	}
}
