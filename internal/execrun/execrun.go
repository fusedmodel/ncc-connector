// Package execrun 远程执行的**执行器**：把一条任务变成一次真跑。
//
// 三层边界（谁在跑、跑什么、能不能跑）：
//  1. `Capability`（本机事实）——哪些引擎**真的可用**（runner 在不在、容器运行时在不在），
//     以及为什么不可用。`GET /api/exec/kinds` 直接把它讲给调用方，**不靠猜标签**。
//  2. `Plan`（这次要跑什么）——包（wasm）或一条命令（process / container）。
//  3. `Run`（跑起来）——独立工作目录、最小环境变量、墙上限、日志上限、进程组回收。
//
// 刻意不做的事：
//   - **不继承服务端环境变量**：那里有 JWT secret、库路径、blob 目录。子进程只拿
//     PATH/LANG 这类必需品 + `NCC_EXEC_*` 元信息（谁提交的、什么引擎）。
//   - **不把任务转派给别人**（不做任务经纪）：本服务只跑在本机。
//   - **不默认打开任意命令**：`process` / `container` 必须运维显式放行（见 config）。
package execrun

import (
	"context"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"

	"github.com/fusedmodel/ncc-registry/config"
	"github.com/fusedmodel/ncc-registry/model"
)

// Kind 一种引擎的可用性（本机事实 + 为什么）。
type Kind struct {
	ID       string `json:"id"`
	Enabled  bool   `json:"enabled"`
	Provider string `json:"provider"` // 谁来跑（人读一句话）
	Why      string `json:"why"`      // 不可用时：为什么 / 要什么才能开
}

// RunnerInfo 跑 wasm 的 HUR 运行时（默认从 PATH 找 `ncc`）。
type RunnerInfo struct {
	Path    string `json:"path"`
	OK      bool   `json:"ok"`
	Note    string `json:"note"`
	Bin     string `json:"bin"` // 配置里写的名字（未解析前）
}

// Capability 这台机器的执行能力（Probe 的产物，无副作用）。
type Capability struct {
	Allow     []string   `json:"allow"` // 运维放行的引擎（NCCR_EXEC_ALLOW，默认 wasm）
	Kinds     []Kind     `json:"kinds"`
	Runner    RunnerInfo `json:"runner"`
	Shell     string     `json:"shell"`
	Docker    string     `json:"docker"`
	Images    []string   `json:"images"` // container 允许的镜像（空 = 不限制）
	Tags      []string   `json:"tags"`
	TimeoutMS int64      `json:"timeoutMs"` // 单任务默认墙上限
	MaxOutput int64      `json:"maxOutputBytes"`
	WorkRoot  string     `json:"workRoot"`
}

// allowed 该引擎是否在放行清单里。
func (c Capability) allowed(engine string) bool {
	for _, e := range c.Allow {
		if e == engine {
			return true
		}
	}
	return false
}

// Allowed 该引擎是否被运维放行（导出给 HTTP 层：没放行的引擎连字节都不收）。
func (c Capability) Allowed(engine string) bool { return c.allowed(engine) }

// Enabled 该引擎现在能不能跑（既放行、又真的具备条件）。
func (c Capability) Enabled(engine string) bool {
	for _, k := range c.Kinds {
		if k.ID == engine {
			return k.Enabled
		}
	}
	return false
}

// Probe 探查本机能力。**只看事实，不改状态**：runner 在不在、docker 在不在、放行清单写了什么。
func Probe(cfg *config.Config) Capability {
	c := Capability{
		Allow:     cfg.ExecAllow,
		Shell:     cfg.ExecShell,
		Docker:    cfg.ExecDocker,
		Images:    cfg.ExecImages,
		Tags:      cfg.ExecTags,
		TimeoutMS: cfg.ExecTimeout.Milliseconds(),
		MaxOutput: cfg.ExecMaxOutput,
		WorkRoot:  cfg.ExecDir,
	}
	// wasm：要一个 HUR 运行时（默认 `ncc`）。
	bin := strings.TrimSpace(cfg.ExecRunner)
	if bin == "" {
		bin = "ncc"
	}
	c.Runner = RunnerInfo{Bin: bin}
	if p, err := exec.LookPath(bin); err == nil {
		c.Runner.Path, c.Runner.OK = p, true
	} else {
		c.Runner.Note = fmt.Sprintf("PATH 里找不到 %s —— 装上 ncc（或设 NCCR_EXEC_RUNNER 指到它）后 wasm 引擎才可用", bin)
	}

	// process：只要有 shell 就能跑（真正的门槛是运维放行，不是技术条件）。
	shellOK := false
	if p, err := exec.LookPath(cfg.ExecShell); err == nil {
		shellOK = true
		_ = p
	}
	// container：要有容器运行时。
	dockerOK := false
	if p, err := exec.LookPath(cfg.ExecDocker); err == nil {
		dockerOK = true
		_ = p
	}

	c.Kinds = []Kind{
		{
			ID: "wasm", Enabled: c.allowed("wasm") && c.Runner.OK,
			Provider: "本机 HUR 沙箱（`" + bin + " hur run --exec`，限额由包内策略算）",
			Why:      why(c.allowed("wasm"), c.Runner.OK, "wasm 不在 NCCR_EXEC_ALLOW 里", c.Runner.Note),
		},
		{
			ID: "process", Enabled: c.allowed("process") && shellOK,
			Provider: "本机进程（`" + cfg.ExecShell + " -c`）—— OS 敏感任务走这条：docker build / 编译 / apt",
			Why:      why(c.allowed("process"), shellOK, "process 需要运维显式放行：NCCR_EXEC_ALLOW=wasm,process", "PATH 里找不到 "+cfg.ExecShell),
		},
		{
			ID: "container", Enabled: c.allowed("container") && dockerOK && shellOK,
			Provider: "容器（`" + cfg.ExecDocker + " run --rm`，工作目录挂进 /work）—— 隔离更强；要构建镜像得自备 DinD",
			Why:      why(c.allowed("container") && dockerOK, shellOK, "container 需要运维显式放行：NCCR_EXEC_ALLOW=wasm,container", "PATH 里找不到 "+cfg.ExecDocker),
		},
	}
	return c
}

// why 拼出「为什么现在不能跑」——能跑就给空串。
func why(allowed, ready bool, needAllow, needReady string) string {
	switch {
	case !allowed && !ready:
		return needAllow + "；另外 " + needReady
	case !allowed:
		return needAllow
	case !ready:
		return needReady
	}
	return ""
}

// Plan 这次要跑什么。
type Plan struct {
	Engine  string
	Kind    string // package | cmd
	WorkDir string // 子进程的 cwd（每个任务一个临时目录）
	// Command 仅 kind=cmd
	Command string
	// Image 仅 engine=container
	Image string
	// PackageDir 仅 engine=wasm：解包后的包根目录
	PackageDir string
}

// Spec 人可读的载荷描述（写进任务记录，列表里一眼看懂）。
func (p Plan) Spec() string {
	if p.Kind == "package" {
		return p.PackageDir
	}
	if p.Engine == "container" && p.Image != "" {
		return "[" + p.Image + "] " + p.Command
	}
	return p.Command
}

// Build 把 Plan 变成要执行的 argv。**引擎不认识 / 没放行 / 缺条件一律报错**，
// 不做「尽力而为」的降级 —— 降级等于偷偷换一台机器或换一种隔离级别跑别人的东西。
func (c Capability) Build(p Plan) ([]string, error) {
	if !model.ValidExecEngine(p.Engine) {
		return nil, fmt.Errorf("不认识的引擎 %q（可选：%s）", p.Engine, strings.Join(model.ExecEngines, "/"))
	}
	if !c.allowed(p.Engine) {
		return nil, fmt.Errorf("这台机器没有放行 %s 引擎（运维要显式打开：NCCR_EXEC_ALLOW 加上 %s）", p.Engine, p.Engine)
	}
	if !c.Enabled(p.Engine) {
		for _, k := range c.Kinds {
			if k.ID == p.Engine {
				return nil, fmt.Errorf("%s 引擎现在不可用：%s", p.Engine, k.Why)
			}
		}
		return nil, fmt.Errorf("%s 引擎现在不可用", p.Engine)
	}
	switch p.Engine {
	case "wasm":
		if p.PackageDir == "" {
			return nil, fmt.Errorf("wasm 引擎要一个包（--package 或在 body 里传 .hur）")
		}
		return []string{c.Runner.Path, "hur", "run", p.PackageDir, "--exec", "--json"}, nil
	case "process":
		if strings.TrimSpace(p.Command) == "" {
			return nil, fmt.Errorf("process 引擎要一条命令")
		}
		return []string{c.Shell, "-c", p.Command}, nil
	case "container":
		if strings.TrimSpace(p.Command) == "" || strings.TrimSpace(p.Image) == "" {
			return nil, fmt.Errorf("container 引擎要 --image 与一条命令")
		}
		if len(c.Images) > 0 && !contains(c.Images, p.Image) {
			return nil, fmt.Errorf("镜像 %q 不在 NCCR_EXEC_IMAGES 允许清单里", p.Image)
		}
		return []string{c.Docker, "run", "--rm", "-v", p.WorkDir + ":/work", "-w", "/work", p.Image, "sh", "-c", p.Command}, nil
	}
	return nil, fmt.Errorf("引擎 %s 还没接上", p.Engine)
}

func contains(list []string, v string) bool {
	for _, x := range list {
		if x == v {
			return true
		}
	}
	return false
}

// Result 一次执行的结果。
type Result struct {
	Status    string // succeeded | failed | timeout | canceled
	ExitCode  int
	LogBytes  int64
	Truncated bool
	Err       error
}

// Run 真的跑。`onStart(pid)` 在进程起来后回调一次（外层用它登记，好支持取消）。
//
// 日志：stdout+stderr 合流写 logPath，超过 maxOutput 后**继续跑但不再写**
// （truncated=true）—— 截断要如实记下来，否则看到半截日志的人会以为程序只输出了这些。
func (c Capability) Run(ctx context.Context, p Plan, logPath string, onStart func(pid int)) Result {
	argv, err := c.Build(p)
	if err != nil {
		return Result{Status: model.ExecFailed, Err: err}
	}
	if err := os.MkdirAll(filepath.Dir(logPath), 0o755); err != nil {
		return Result{Status: model.ExecFailed, Err: err}
	}
	f, err := os.Create(logPath)
	if err != nil {
		return Result{Status: model.ExecFailed, Err: err}
	}
	defer f.Close()
	lw := &limitWriter{w: f, limit: c.MaxOutput}

	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Dir = p.WorkDir
	cmd.Env = childEnv(p)
	cmd.Stdout, cmd.Stderr = lw, lw
	setProcAttr(cmd)
	if err := cmd.Start(); err != nil {
		return Result{Status: model.ExecFailed, Err: err}
	}
	if onStart != nil {
		onStart(cmd.Process.Pid)
	}

	done := make(chan struct{})
	var waitErr error
	go func() { waitErr = cmd.Wait(); close(done) }()

	select {
	case <-done:
		code := 0
		if waitErr != nil {
			var ee *exec.ExitError
			if ok := asExitError(waitErr, &ee); ok {
				code = ee.ExitCode()
			} else {
				return Result{Status: model.ExecFailed, ExitCode: -1, LogBytes: lw.n, Truncated: lw.hit, Err: waitErr}
			}
		}
		st := model.ExecSucceeded
		if code != 0 {
			st = model.ExecFailed
		}
		return Result{Status: st, ExitCode: code, LogBytes: lw.n, Truncated: lw.hit}
	case <-ctx.Done():
		killGroup(cmd)
		<-done
		st := model.ExecTimeout
		if ctx.Err() == context.Canceled {
			st = model.ExecCanceled
		}
		return Result{Status: st, ExitCode: -1, LogBytes: lw.n, Truncated: lw.hit, Err: ctx.Err()}
	}
}

// childEnv 子进程环境：**白名单**，绝不继承服务端 env（那里面有 JWT secret、库路径）。
func childEnv(p Plan) []string {
	out := []string{
		"PATH=" + os.Getenv("PATH"),
		"HOME=" + p.WorkDir,
		"TMPDIR=" + filepath.Join(p.WorkDir, ".tmp"),
		"LANG=" + firstNonEmptyString(os.Getenv("LANG"), "C.UTF-8"),
		"NCC_EXEC_ENGINE=" + p.Engine,
		"NCC_EXEC_WORKDIR=" + p.WorkDir,
	}
	return out
}

func firstNonEmptyString(vals ...string) string {
	for _, v := range vals {
		if strings.TrimSpace(v) != "" {
			return v
		}
	}
	return ""
}

// limitWriter 写满 limit 后停止写入，但记下真实累计量与被截断这件事。
//
// 超出上限时**对外仍报"写成功"**：否则子进程会拿到 EPIPE 而异常退出 ——
// 那是我们限流造成的，不该让它看起来像任务失败。
type limitWriter struct {
	w     io.Writer
	limit int64
	n     int64
	hit   bool
}

func (l *limitWriter) Write(b []byte) (int, error) {
	before := l.n
	l.n += int64(len(b))
	left := l.limit - before
	if left <= 0 {
		l.hit = true
		return len(b), nil
	}
	chunk := b
	if int64(len(b)) > left {
		chunk = b[:left]
		l.hit = true
	}
	if _, err := l.w.Write(chunk); err != nil {
		return 0, err
	}
	return len(b), nil
}

// asExitError exec.Wait 的 err 里挖出退出码（*exec.ExitError 是唯一有码的那种）。
func asExitError(err error, out **exec.ExitError) bool {
	if ee, ok := err.(*exec.ExitError); ok {
		*out = ee
		return true
	}
	return false
}
