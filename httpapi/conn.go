package httpapi

// 通信基础设施：**连接（Connection）** —— 一条到 Cloud instance 的会话，通道上能
// 反复执行命令、推/拉文件，且全程留在账本里。
//
// 与 `/api/exec/*`（任务）的关系：任务是一次动作；连接是一段会话。`conn exec` **复用**
// `internal/execrun`（同一套限额 / 环境变量白名单 / 进程组回收），只是多了一层
// **会话与文件面**。所以这里不重复实现执行器。
//
// 红线（`prd/ncc-conn.md`，改代码前先读）：
//  1. **默认关**：通道能跑任意命令、写文件 —— `NCCR_CONN_ALLOW` 必须运维显式打开。
//  2. **文件锁在工作目录**：只收相对路径、拒绝 `..` 与绝对路径（否则一条通道 = 整台机器）。
//  3. **每个动作要 reason**（与 R12 同规矩）。
//  4. **通道有 TTL**，过期即失效；关闭可选连工作目录一起删。
//  5. 通道走**用户自己的线路**：目标机器是客户自己的，云端托管面不在数据路径上。

import (
	"fmt"
	"io"
	"os"
	"path"
	"path/filepath"
	"strings"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/fusedmodel/ncc-registry/internal/execrun"
	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

const (
	connDefaultTTL = time.Hour
	connMaxTTL     = 8 * time.Hour
	connMaxFile    = 32 << 20 // 单文件上限（与 exec 的包上限同档）
	connLogTail    = 32 << 10
)

/* ---------------- 建 / 列 / 看 / 关 ---------------- */

type connOpenReq struct {
	Name   string `json:"name"`
	Note   string `json:"note"`
	TTLSec int64  `json:"ttlSec"`
}

// openConn POST /api/conn/connections —— 建一条通道。
//
// 通道 = 一个**工作目录** + 一段有效期 + 一条审计线索。真正的执行在 `…/exec`，
// 文件在 `…/files`。
func (s *Server) openConn(c *gin.Context) {
	if !s.Cfg.ConnAllow {
		fail(c, 403, "conn_not_allowed",
			"这台机器没有开放连接通道（通道能跑任意命令、写文件，属最高权限）—— "+
				"运维要显式打开：NCCR_CONN_ALLOW=1")
		return
	}
	a := authOf(c)
	if a == nil {
		fail(c, 401, "unauthorized", "未认证或凭据无效（先 ncc login / ncc conn open --key …）")
		return
	}
	var body connOpenReq
	if err := c.ShouldBindJSON(&body); err != nil && c.Request.ContentLength > 0 {
		fail(c, 400, "bad_request", "请求体格式错误")
		return
	}
	ttl := s.Cfg.ConnTTL
	if ttl <= 0 {
		ttl = connDefaultTTL
	}
	if body.TTLSec > 0 {
		want := time.Duration(body.TTLSec) * time.Second
		if want > connMaxTTL {
			fail(c, 400, "bad_request", fmt.Sprintf("ttlSec 不能超过 %d 秒", int64(connMaxTTL.Seconds())))
			return
		}
		ttl = want
	}
	id := store.NewID("CN")
	work := filepath.Join(s.Cfg.ConnDir, id)
	if err := os.MkdirAll(filepath.Join(work, ".tmp"), 0o700); err != nil {
		fail(c, 500, "internal", "创建工作目录失败")
		return
	}
	peer := fmt.Sprintf("%s/%s", s.Cfg.NodeName, s.Cfg.NodeID)
	rec, err := s.St.CreateConn(store.ConnInput{
		ID: id, OwnerID: a.UserID, RequestedBy: a.Email,
		Name: clampRunesCard(body.Name, 60), Note: clampRunesCard(body.Note, 400),
		WorkDir: work, Peer: peer, TTLSec: int64(ttl.Seconds()),
		ExpiresAt: time.Now().Add(ttl),
	})
	if err != nil {
		_ = os.RemoveAll(work)
		fail(c, 500, "internal", "建立连接失败")
		return
	}
	s.ensureAdmin(c)
	s.audit(c, model.ActConnOpen, rec.ID, rec.Name, "建立连接通道",
		gin.H{"ttlSec": rec.TTLSec, "peer": peer})
	ok(c, 201, gin.H{
		"connection": s.connJSON(rec),
		"howto": gin.H{
			"exec":  "POST /api/conn/connections/" + rec.ID + "/exec  {\"cmd\":\"…\",\"reason\":\"…\"}",
			"push":  "POST /api/conn/connections/" + rec.ID + "/files?path=<相对路径>&reason=…  (body = 文件字节)",
			"pull":  "GET  /api/conn/connections/" + rec.ID + "/files?path=<相对路径>",
			"close": "DELETE /api/conn/connections/" + rec.ID + "（?purge=1 连工作目录删）",
			"note":  "文件路径只能是相对的（锁在这条通道的工作目录里）；每个动作都要 reason。",
		},
	})
}

// listConns GET /api/conn/connections —— 我的通道（管理员 ?all=1）。
func (s *Server) listConns(c *gin.Context) {
	a := authOf(c)
	owner := ""
	if a != nil {
		owner = a.UserID
	}
	if c.Query("all") == "1" {
		if !s.ensureAdmin(c) {
			fail(c, 403, "admin_required", "查看全部通道需要节点管理员身份")
			return
		}
		owner = ""
	} else if owner == "" {
		fail(c, 401, "unauthorized", "未认证或凭据无效")
		return
	}
	// 不用先「收尾」：过期是从 ExpiresAt 推导出来的（Conn.State），
	// 列表里会直接显示 expired，不必（也不该）把状态改写成 closed —— 那会丢掉区别。
	limit, offset := pageParams(c)
	rows, err := s.St.ListConns(owner, limit, offset)
	if err != nil {
		fail(c, 500, "internal", "服务内部错误")
		return
	}
	list := make([]gin.H, 0, len(rows))
	for i := range rows {
		list = append(list, s.connJSON(&rows[i]))
	}
	ok(c, 200, gin.H{"connections": list, "total": len(list), "limit": limit, "offset": offset})
}

// getConn GET /api/conn/connections/:id —— 状态 + 这条通道上跑过什么（任务列表尾巴）。
//
// **关掉/过期的也照样能看**：通道是账本，`close` 只是把「能动手」这条关掉，
// 不该把「这条通道上跑过什么」一起藏起来（归档 ≠ 删除，同一个道理）。
// 所以这里**不复用 connFor**（那个守卫会 410）—— 只做归属判断，状态原样回报。
func (s *Server) getConn(c *gin.Context) {
	rec, err := s.St.FindConn(c.Param("id"))
	if err != nil {
		fail(c, 404, "not_found", "连接不存在")
		return
	}
	if !s.Cfg.ConnAllow {
		fail(c, 403, "conn_not_allowed", "这台机器没有开放连接通道（NCCR_CONN_ALLOW=1 才开）")
		return
	}
	a := authOf(c)
	if !s.ensureAdmin(c) && (a == nil || rec.OwnerID != a.UserID) {
		fail(c, 403, "forbidden", "只能看自己建立的连接（管理员可看全部）")
		return
	}
	runs, _ := s.St.ListExecRuns(rec.OwnerID, 20, 0)
	onConn := make([]gin.H, 0, 8)
	for i := range runs {
		if runs[i].ConnID == rec.ID {
			onConn = append(onConn, s.execJSON(&runs[i]))
		}
	}
	out := s.connJSON(rec)
	out["execs"] = onConn
	// 不可用了就把原因一并给出（客户端不用自己猜 state 到 410 的映射）
	if !rec.Open(time.Now()) {
		out["blocked"] = rec.State(time.Now())
	}
	ok(c, 200, gin.H{"connection": out})
}

// closeConn DELETE /api/conn/connections/:id —— 关闭（?purge=1 连工作目录一起删）。
func (s *Server) closeConn(c *gin.Context) {
	rec, err := s.St.FindConn(c.Param("id"))
	if err != nil {
		fail(c, 404, "not_found", "连接不存在")
		return
	}
	admin := s.ensureAdmin(c)
	a := authOf(c)
	if !admin && (a == nil || rec.OwnerID != a.UserID) {
		fail(c, 403, "forbidden", "只能关闭自己建立的连接")
		return
	}
	wasOpen, err := s.St.CloseConn(rec.ID, "")
	if err != nil {
		fail(c, 500, "internal", "关闭失败")
		return
	}
	purged := false
	if c.Query("purge") == "1" {
		// 删工作目录前先确认它确实在这条连的目录里（别让一条脏数据删到别处）。
		if filepath.Dir(rec.WorkDir) == filepath.Clean(s.Cfg.ConnDir) {
			_ = os.RemoveAll(rec.WorkDir)
			purged = true
		}
	}
	s.audit(c, model.ActConnClose, rec.ID, rec.Name, "关闭连接通道", gin.H{"purge": purged})
	ok(c, 200, gin.H{"ok": true, "id": rec.ID, "wasOpen": wasOpen, "purged": purged})
}

/* ---------------- 通道上：执行 ---------------- */

type connExecReq struct {
	Cmd        string `json:"cmd"`
	Engine     string `json:"engine"`
	Reason     string `json:"reason"`
	Timeout    int64  `json:"timeoutSec"`
	Wait       *bool  `json:"wait"` // 默认 true：等它跑完（通道上多半是要看结果）
	WorkingDir string `json:"cwd"`  // 相对工作目录的子目录（可选）
}

// connExec POST /api/conn/connections/:id/exec —— 在这条通道上跑一条命令。
//
// **复用 execrun**：限额、环境变量白名单、进程组回收都与 `/api/exec/runs` 一致；
// 区别只是「工作目录挂在通道上」+ 记录带上 `connId`。
func (s *Server) connExec(c *gin.Context) {
	rec, ok2 := s.connFor(c)
	if !ok2 {
		return
	}
	var body connExecReq
	if err := c.ShouldBindJSON(&body); err != nil {
		fail(c, 400, "bad_request", "请求体格式错误（需要 cmd 与 reason）")
		return
	}
	if strings.TrimSpace(body.Cmd) == "" {
		fail(c, 400, "bad_request", "缺少 cmd")
		return
	}
	if strings.TrimSpace(body.Reason) == "" {
		fail(c, 400, "bad_request", "缺少 reason：这条通道上干了什么、为什么，要留在账本里")
		return
	}
	engine := firstNonEmpty(strings.TrimSpace(body.Engine), "process")
	if engine != "process" && engine != "container" {
		// 通道上不给 wasm：通道的语义是"在目标机上干活"（wasm 请走 /api/exec/runs 或 ncc sandbox run --package）
		fail(c, 400, "bad_request", "通道上只支持 process / container 引擎（wasm 请走 ncc sandbox run --package）")
		return
	}
	cap := s.Exec()
	if !cap.Allowed(engine) {
		fail(c, 403, "engine_not_allowed", "这台机器没有放行 "+engine+" 引擎（NCCR_EXEC_ALLOW 加上它）")
		return
	}
	if !cap.Enabled(engine) {
		why := ""
		for _, k := range cap.Kinds {
			if k.ID == engine {
				why = k.Why
			}
		}
		fail(c, 409, "engine_unavailable", engine+" 引擎当前不可用："+why)
		return
	}

	timeout := s.Cfg.ExecTimeout
	if body.Timeout > 0 {
		want := time.Duration(body.Timeout) * time.Second
		if want > s.Cfg.ExecTimeout {
			fail(c, 400, "bad_request", fmt.Sprintf("timeoutSec 不能超过节点上限（%d 秒）", int64(s.Cfg.ExecTimeout.Seconds())))
			return
		}
		timeout = want
	}

	// cwd：只能是工作目录里的子目录（同样防越界）。
	work := rec.WorkDir
	if sub := strings.TrimSpace(body.WorkingDir); sub != "" {
		p, err := safeJoin(rec.WorkDir, sub)
		if err != nil {
			fail(c, 400, "bad_request", err.Error())
			return
		}
		if err := os.MkdirAll(p, 0o700); err != nil {
			fail(c, 500, "internal", "创建子目录失败")
			return
		}
		work = p
	}

	runID := store.NewID("ER")
	taskDir := filepath.Join(work, ".ncc-exec", runID)
	if err := os.MkdirAll(filepath.Join(taskDir, ".tmp"), 0o700); err != nil {
		fail(c, 500, "internal", "创建任务目录失败")
		return
	}
	plan := execrun.Plan{Engine: engine, Kind: "cmd", WorkDir: work, Command: body.Cmd}
	logPath := filepath.Join(taskDir, "output.log")
	tk := authOf(c)
	reqBy := ""
	owner := rec.OwnerID
	if tk != nil {
		reqBy = tk.Email
		owner = tk.UserID
	}
	run, err := s.St.CreateExecRun(store.ExecRunInput{
		OwnerID: owner, RequestedBy: reqBy, ConnID: rec.ID,
		Engine: engine, Kind: "cmd", Spec: clampRunesCard(body.Cmd, 400),
		WorkDir: work, LogPath: logPath,
		Reason: clampRunesCard(body.Reason, maxExecReason), TimeoutSec: int64(timeout.Seconds()),
	})
	if err != nil {
		fail(c, 500, "internal", "创建任务失败")
		return
	}
	_ = s.St.TouchConn(rec.ID, 1, 0, 0, 0)
	s.ensureAdmin(c)
	s.audit(c, model.ActConnExec, rec.ID, rec.Name, "通道上执行命令",
		gin.H{"engine": engine, "task": run.ID, "reason": run.Reason})

	wait := body.Wait == nil || *body.Wait
	if !wait {
		go s.runExec(c.Request.Context(), run, plan, timeout, logPath)
		_ = s.St.MarkExecRunning(run.ID)
		ok(c, 202, gin.H{"exec": s.execJSON(run), "note": "已提交，用 GET /api/exec/runs/:id 看结果"})
		return
	}
	// 等它跑完：通道的用法多半是"跑一条、看结果、再跑下一条"，
	// 让调用方自己轮询会把简单的脚本推给每一个客户端重写一遍。
	s.runExecSync(c.Request.Context(), run, plan, timeout, logPath)
	done, _ := s.St.FindExecRun(run.ID)
	// 同步返回**带上日志尾巴**（与 GET /api/exec/runs/:id 同一份实现）：
	// 通道上一半的用法是「跑条命令看它说了什么」。
	ok(c, 200, gin.H{"exec": s.execJSONTail(done)})
}

/* ---------------- 通道上：文件 ---------------- */

// connPutFile POST /api/conn/connections/:id/files?path=<相对>&reason=… —— 推文件。
//
// body = 原样字节。路径锁在这条通道的工作目录内（`safeJoin`），
// 目标目录不存在就建；`?sha256=` 可带上让服务端核对（不一致就不落盘）。
func (s *Server) connPutFile(c *gin.Context) {
	rec, ok2 := s.connFor(c)
	if !ok2 {
		return
	}
	rel := c.Query("path")
	if strings.TrimSpace(rel) == "" {
		fail(c, 400, "bad_request", "缺少 path（相对工作目录的路径，如 app/deploy.sh）")
		return
	}
	if strings.TrimSpace(firstNonEmpty(c.Query("reason"), "")) == "" {
		fail(c, 400, "bad_request", "缺少 reason：往目标机写了什么、为什么，要留在账本里")
		return
	}
	dst, err := safeJoin(rec.WorkDir, rel)
	if err != nil {
		fail(c, 400, "bad_request", err.Error())
		return
	}
	if err := os.MkdirAll(filepath.Dir(dst), 0o700); err != nil {
		fail(c, 500, "internal", "创建目录失败")
		return
	}
	body, err := io.ReadAll(io.LimitReader(c.Request.Body, connMaxFile+1))
	if err != nil {
		fail(c, 500, "internal", "读取上传失败")
		return
	}
	if len(body) > connMaxFile {
		fail(c, 413, "payload_too_large", fmt.Sprintf("单文件上限 %dMB", connMaxFile>>20))
		return
	}
	if want := strings.TrimSpace(c.Query("sha256")); want != "" && cardSHA256(body) != want {
		fail(c, 400, "checksum_mismatch", "sha256 与声明的不一致 —— 一个字节都没落盘")
		return
	}
	mode := os.FileMode(0o600)
	if c.Query("mode") == "700" || c.Query("executable") == "1" {
		mode = 0o700
	}
	if err := os.WriteFile(dst, body, mode); err != nil {
		fail(c, 500, "internal", "写文件失败")
		return
	}
	sha := cardSHA256(body)
	_ = s.St.TouchConn(rec.ID, 0, int64(len(body)), 0, 0)
	s.ensureAdmin(c)
	s.audit(c, model.ActConnPut, rec.ID, rec.Name, "通道上推送文件",
		gin.H{"path": rel, "bytes": len(body), "sha256": sha, "reason": c.Query("reason")})
	ok(c, 201, gin.H{
		"path": rel, "bytes": len(body), "sha256": sha,
		"abs": dst, "mode": fmt.Sprintf("%o", mode),
	})
}

// connGetFile GET /api/conn/connections/:id/files?path=<相对> —— 拉文件（文本/二进制都行）。
func (s *Server) connGetFile(c *gin.Context) {
	rec, ok2 := s.connFor(c)
	if !ok2 {
		return
	}
	rel := c.Query("path")
	src, err := safeJoin(rec.WorkDir, rel)
	if err != nil {
		fail(c, 400, "bad_request", err.Error())
		return
	}
	st, err := os.Stat(src)
	if err != nil || st.IsDir() {
		fail(c, 404, "not_found", "文件不存在（或是个目录）")
		return
	}
	if st.Size() > connMaxFile {
		fail(c, 413, "payload_too_large", fmt.Sprintf("单文件上限 %dMB（大文件请分块或走制品托管）", connMaxFile>>20))
		return
	}
	b, err := os.ReadFile(src)
	if err != nil {
		fail(c, 500, "internal", "读文件失败")
		return
	}
	_ = s.St.TouchConn(rec.ID, 0, 0, int64(len(b)), 1)
	c.Header("X-NCC-Sha256", cardSHA256(b))
	c.Header("X-Content-Type-Options", "nosniff")
	c.Header("Content-Disposition", `attachment; filename="`+path.Base(rel)+`"`)
	c.Data(200, "application/octet-stream", b)
}

/* ---------------- 守卫与视图 ---------------- */

func (s *Server) connFor(c *gin.Context) (*model.Conn, bool) {
	rec, err := s.St.FindConn(c.Param("id"))
	if err != nil {
		fail(c, 404, "not_found", "连接不存在")
		return nil, false
	}
	a := authOf(c)
	if !s.Cfg.ConnAllow {
		fail(c, 403, "conn_not_allowed", "这台机器没有开放连接通道（NCCR_CONN_ALLOW=1 才开）")
		return nil, false
	}
	if a != nil && rec.OwnerID == a.UserID {
		return s.connUsable(rec, c)
	}
	if s.ensureAdmin(c) {
		return s.connUsable(rec, c)
	}
	// 通道里能跑任意命令、读文件 —— 只有当事人与管理员能碰。
	fail(c, 403, "forbidden", "只能使用自己建立的连接（管理员可看全部）")
	return nil, false
}

// connUsable 状态守卫（关闭 / 过期分开说 —— 用户要知道是哪一种）。
func (s *Server) connUsable(rec *model.Conn, c *gin.Context) (*model.Conn, bool) {
	switch rec.State(time.Now()) {
	case model.ConnClosed:
		fail(c, 410, "conn_closed", "这条连接已关闭（重新 ncc conn open 即可）")
		return nil, false
	case "expired":
		fail(c, 410, "conn_expired", "这条连接已过期（TTL 到了）——重新 ncc conn open，或建的时候就给更长的 --ttl")
		return nil, false
	}
	return rec, true
}

func (s *Server) connJSON(rec *model.Conn) gin.H {
	now := time.Now()
	must := gin.H{
		"id": rec.ID, "name": rec.Name, "note": rec.Note,
		"state": rec.State(now),
		// usable 把「state 不是 open」翻译成「能不能动手」：客户端拿来灰置、拿来做提示，
		// 免得每个调用方自己写一遍 state → 能不能用的映射。
		"usable":  rec.Open(now),
		"workDir": rec.WorkDir, "peer": rec.Peer,
		"ttlSec": rec.TTLSec, "expiresAt": rec.ExpiresAt,
		"execCount": rec.ExecCount, "bytesUp": rec.BytesUp, "bytesDown": rec.BytesDown, "pulls": rec.PullCount,
		"lastUsedAt": rec.LastUsedAt, "createdAt": rec.CreatedAt, "closedAt": rec.ClosedAt,
		"url": "/api/conn/connections/" + rec.ID,
	}
	return must
}

// safeJoin 把「工作目录 + 用户给的相对路径」拼成一个**保证在工作目录内**的绝对路径。
//
// 这是文件面的唯一一道门：`..`、绝对路径、`\x00`、软链之外的花样都在这里挡掉。
// 不做这道门，一条通道就等于「能读能写整台机器」。
func safeJoin(root, rel string) (string, error) {
	rel = strings.TrimSpace(rel)
	if rel == "" {
		return "", fmt.Errorf("路径不能为空")
	}
	if strings.ContainsAny(rel, "\x00") {
		return "", fmt.Errorf("路径里有非法字符")
	}
	if filepath.IsAbs(rel) || strings.HasPrefix(rel, "/") || strings.HasPrefix(rel, "\\") {
		return "", fmt.Errorf("只接受相对路径（锁在这条通道的工作目录里）：%s", rel)
	}
	clean := path.Clean("/" + strings.ReplaceAll(rel, "\\", "/"))
	if clean == "/" {
		return "", fmt.Errorf("路径不能是工作目录本身")
	}
	if strings.Contains(clean, "..") {
		return "", fmt.Errorf("路径不许跳出工作目录：%s", rel)
	}
	root = filepath.Clean(root)
	full := filepath.Join(root, filepath.FromSlash(strings.TrimPrefix(clean, "/")))
	if full != root && !strings.HasPrefix(full, root+string(os.PathSeparator)) {
		return "", fmt.Errorf("路径越界：%s", rel)
	}
	return full, nil
}

// staleConns 启动时数一下已经过期的通道（只打日志）。
//
// 状态是推导的，所以这里**没有需要修复的数据** —— 只是重启后给运维一个告警：
// 「你这台机器上还挂着 N 条 TTL 已过的通道」（它们要么被清理，要么就是在提醒你 TTL 太短）。
func (s *Server) staleConns() {
	if n, err := s.St.CountExpiredConns(time.Now()); err == nil && n > 0 {
		logf("[ncc-registry] 收尾 %d 条过期连接", n)
	}
}
