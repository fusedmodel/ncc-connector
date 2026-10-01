package model

import "time"

// 远程执行（Remote Cloud Computer / 云电脑）—— 一台**替别人跑东西**的机器。
//
// 语义边界（与 `prd/ncc-sandbox.md` 一致，改代码前先读）：
//  1. **执行权是最高权限**：`wasm` 是沙箱（限额由策略算），`process` / `container` 是
//     真的在这台机器上跑别人的命令 —— 后者**必须运维显式放行**，不是默认能力。
//  2. **任务不是制品**：任务是一次动作 + 它的日志与退出码；制品/包仍然走制品托管那套。
//  3. **不代跑业务数据**：这是**执行面**（算力在客户自己的机器上），不是数据面中转。
//  4. **每一条任务都要 reason**：与 R12（写操作必须说明为什么）同一条规矩 ——
//     日志里要能回答「谁让这台机器干了什么、为什么」。

// ExecEngines 认识的引擎（写死的产品口径，与 `hur-core::policy::ENGINES` 对齐）。
//
// 这里只放**本服务真的能调度**的三种：wasm（内置 HUR 沙箱）、process（本机进程）、
// container（容器运行时）。`js` / `remote` 不在其中：前者由客户端托管，后者是
// 「再往下一跳」，本服务不替别人转派任务（不做任务经纪）。
var ExecEngines = []string{"wasm", "process", "container"}

// ValidExecEngine 引擎名是否认识。
func ValidExecEngine(e string) bool {
	for _, x := range ExecEngines {
		if x == e {
			return true
		}
	}
	return false
}

// 任务状态。`queued → running → succeeded|failed|timeout|canceled`。
const (
	ExecQueued    = "queued"
	ExecRunning   = "running"
	ExecSucceeded = "succeeded"
	ExecFailed    = "failed"
	ExecTimeout   = "timeout"
	ExecCanceled  = "canceled"
)

// ExecRun 一次远程执行。
//
// 载荷两种：`kind=package`（上传的 .hur，按包里的策略跑）与 `kind=cmd`（一条命令，
// OS 敏感任务的形态，例如 docker build && docker push）。
type ExecRun struct {
	ID      string `gorm:"primaryKey"`
	OwnerID string `gorm:"not null;index"` // 谁提交的（发起方用户）
	// RequestedBy 供审计用的展示名（邮箱/用户名快照，用户改名后这条记录仍可读）
	RequestedBy string `gorm:"not null;default:''"`
	Engine      string `gorm:"not null;index"` // wasm | process | container
	Kind        string `gorm:"not null;default:cmd"`
	// Spec 人可读的载荷描述（命令原文 / 包 id@版本）—— 列表里要一眼看懂
	Spec string `gorm:"not null;default:''"`
	// Image 仅 container 引擎用
	Image string `gorm:"not null;default:''"`
	// PackageID / PackageVersion / PackageSHA 仅 kind=package 用
	PackageID      string `gorm:"not null;default:''"`
	PackageVersion string `gorm:"not null;default:''"`
	PackageSHA     string `gorm:"not null;default:''"`
	// WorkDir 这次任务的工作目录（每次一个，跑完留着；日志在里面）
	WorkDir string `gorm:"not null;default:''"`
	LogPath string `gorm:"not null;default:''"`
	// Reason 提交理由（必填）：审计与排障都靠它
	Reason string `gorm:"not null;default:''"`
	// TimeoutSec 本次任务的墙上限（0 = 用节点默认）
	TimeoutSec int64 `gorm:"not null;default:0"`

	Status   string `gorm:"not null;default:queued;index"`
	ExitCode int    `gorm:"not null;default:0"`
	// LogBytes / LogTruncated：日志超过上限会被截断 —— 如实记下来，
	// 免得看到半截日志的人以为程序只输出了这些。
	LogBytes     int64  `gorm:"not null;default:0"`
	LogTruncated bool   `gorm:"not null;default:false"`
	Error        string `gorm:"not null;default:''"`
	CreatedAt    time.Time
	StartedAt    *time.Time
	FinishedAt   *time.Time
}

func (ExecRun) TableName() string { return "exec_runs" }

// Done 是否已经结束（终态）。
func (r *ExecRun) Done() bool {
	switch r.Status {
	case ExecSucceeded, ExecFailed, ExecTimeout, ExecCanceled:
		return true
	}
	return false
}

// DurationMs 耗时（未开始的给 0）。
func (r *ExecRun) DurationMs() int64 {
	if r.StartedAt == nil {
		return 0
	}
	end := time.Now()
	if r.FinishedAt != nil {
		end = *r.FinishedAt
	}
	return end.Sub(*r.StartedAt).Milliseconds()
}
