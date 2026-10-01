package model

import "time"

// 节点治理层（Admin）：一台内网 registry 自己的管理面。
//
// 谁能当管理员 —— 两条路，等价：
//
//	人     User.IsAdmin = true。本节点的**第一个注册用户**自动获得（内网部署是
//	       「谁先装谁是主人」，不引入外部账号系统）。
//	机器   AdminKey（AK-xxxxxx + secret，库里只存 sha256），给 CI / 运维脚本用；
//	       管理员首次出现时自动签发一份，之后可 `ncc registry admin rotate` 轮换。
//
// 管什么：用户（禁用 / 启用、重置密码）、节点（摘除）、服务（节点侧 service 与
// 制品侧 api）。每个动作都写 AuditLog —— 内网里「谁把谁的号停了」必须有据可查。

// AdminKey 节点级机器管理凭据。
//
// 形态与接入票据（AccessTicket）刻意保持一致：短 key 可念、secret 只显示一次、
// 库里只有哈希。区别在语义：票据兑换出**受限节点令牌**，admin key 换来的是**治理权**。
type AdminKey struct {
	ID         string     `gorm:"primaryKey"`
	Label      string     `gorm:"not null;default:''"`
	Key        string     `gorm:"not null;uniqueIndex"`
	SecretHash string     `gorm:"not null"`
	CreatedBy  string     `gorm:"not null;default:''"`
	CreatedAt  time.Time  `gorm:"autoCreateTime"`
	LastUsedAt *time.Time `gorm:"default:null"`
	RevokedAt  *time.Time `gorm:"default:null"`
}

func (AdminKey) TableName() string { return "admin_keys" }

// Active 是否可用（未被轮换 / 撤销）。
func (k *AdminKey) Active() bool { return k.RevokedAt == nil }

// AuditLog 管理动作审计。
type AuditLog struct {
	ID         string    `gorm:"primaryKey"`
	ActorKind  string    `gorm:"not null;default:'user'"` // user | admin_key
	ActorID    string    `gorm:"not null;default:''"`
	ActorName  string    `gorm:"not null;default:''"`
	Action     string    `gorm:"not null;index"` // 见 ActXxx 常量
	Target     string    `gorm:"not null;default:''"`
	TargetName string    `gorm:"not null;default:''"`
	Summary    string    `gorm:"not null;default:''"`
	Detail     string    `gorm:"not null;default:'{}'"`
	IP         string    `gorm:"not null;default:''"`
	CreatedAt  time.Time `gorm:"autoCreateTime;index"`
}

func (AuditLog) TableName() string { return "audit_logs" }

// 审计的 actor 类型。
const (
	ActorUser     = "user"
	ActorAdminKey = "admin_key"
)

// 审计动作名（统一在这里定义，别在 handler 里写自由字符串）。
const (
	ActUserDisable    = "user.disable"
	ActUserEnable     = "user.enable"
	ActUserPasswd     = "user.password.reset"
	ActNodeDelete     = "node.delete"
	ActServiceArchive = "service.archive"
	ActServiceDelete  = "service.delete"
	ActShareCreate    = "share.create"
	ActShareRevoke    = "share.revoke"
	ActKeyRotate      = "admin.key.rotate"
	// 远程执行（Remote Cloud Computer）：谁让这台机器跑了什么，必须留账。
	ActExecSubmit = "exec.submit"
	ActExecCancel = "exec.cancel"
)

// AdminServiceArtifactKinds 「服务」在制品侧的取值：kind=api 的条目就是对外可调用的服务接口。
var AdminServiceArtifactKinds = []string{"api"}
