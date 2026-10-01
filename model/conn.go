package model

import "time"

// 连接（Connection）—— 通信基础设施的**会话层**：一条有状态、可复用、可审计的通道。
//
// 与「任务」（ExecRun）的区别（别把两者搞混）：
//
//	ExecRun    一次动作：跑完就结束，产物是日志 + 退出码。
//	Connection 一段会话：有**工作目录**、有时长、可以反复 exec / push / pull，
//	           关掉即失效。审计按连接聚合（这段通道上发生过什么）。
//
// 它不造第二套执行器：`conn exec` 内部走的就是 `internal/execrun`（同一套限额、同一套
// 环境变量白名单、同一套进程组回收），所以「连接」只是给执行加了一层**会话与文件面**。
//
// 红线（改代码前先读，与 `prd/ncc-conn.md` 一致）：
//  1. **这是最高权限**：通道上能跑任意命令、写文件 —— 与 exec 的 process/container 同档，
//     必须运维显式放行（`NCCR_CONN_ALLOW`，默认关）。
//  2. **文件面锁在工作目录里**：路径只能是相对的、不许 `..`、不许绝对路径 ——
//     否则一条通道就等于"能读能写整台机器"。
//  3. **每个动作都要 reason**：账本要能回答「谁在这条通道上干了什么、为什么」。
//  4. **有效期**：连接有 TTL，过期即失效；关闭即释放（可选连工作目录一起删）。
//  5. **通道走用户自己的线路**：目标机器是客户自己的（云电脑/内网节点），
//     云端托管面不在数据路径上（见 `ncc-agent-infra.md` §6.2）。
type Conn struct {
	ID          string `gorm:"primaryKey"`
	OwnerID     string `gorm:"not null;index"`
	RequestedBy string `gorm:"not null;default:''"` // 展示名快照（邮箱/用户名）
	Name        string `gorm:"not null;default:''"`
	Note        string `gorm:"not null;default:''"`
	// WorkDir 这条通道的工作目录（每条连接一个，文件推拉的边界也在这里）
	WorkDir string `gorm:"not null"`
	// Peer 建连时对方自报的身份（nodeId / product），便于审计里认人
	Peer string `gorm:"not null;default:''"`
	// Status 只有 open | closed —— **过期不写状态**，由 ExpiresAt 推导（Conn.State）。
	Status string `gorm:"not null;default:open;index"`
	TTLSec int64  `gorm:"not null;default:0"`
	// Counters：这条通道上发生过什么（列表里一眼看懂）
	ExecCount  int64 `gorm:"not null;default:0"`
	BytesUp    int64 `gorm:"not null;default:0"` // push 进来的
	BytesDown  int64 `gorm:"not null;default:0"` // pull 出去的
	PullCount  int64 `gorm:"not null;default:0"`
	ExpiresAt  time.Time
	LastUsedAt *time.Time
	CreatedAt  time.Time
	ClosedAt   *time.Time
}

func (Conn) TableName() string { return "conns" }

// Open 通道现在可用吗（关闭 / 过期两种，分开说）。
func (c *Conn) Open(now time.Time) bool {
	return c.Status == ConnOpen && now.Before(c.ExpiresAt)
}

// State 人读状态：open | closed | expired。
func (c *Conn) State(now time.Time) string {
	if c.Status == ConnClosed {
		return "closed"
	}
	if !now.Before(c.ExpiresAt) {
		return "expired"
	}
	return ConnOpen
}

const (
	ConnOpen   = "open"
	ConnClosed = "closed"
)
