package model

import "time"

// AgentCard NCC Agent Share（内网节点侧）：把「我设计好的 Agent」点到点交给指定的人。
//
// 与云端 ncc-platform 的同名记录**同形**（同一份 `ncc agent` 客户端两边都能用），
// 只在一个地方刻意不同：这里按本仓的规矩，token **只存 sha256**（与分享链接、接入票据一致），
// 明文只在创建那一次回显。
//
// 一张名片同时承载两种用法（接受方可以只做一半）：把包**装到本机** + 把节点**收进连接表**。
// 红线（与平台侧逐条对齐，见 ncc-platform/prd/ncc-agent-share.md §6）：
//  1. 名片 ≠ 分发渠道：不展示、不检索、没有全站列表；token 是秘密，可撤销 / 可过期 / 可限次。
//  2. 名片 ≠ 授权：收下只代表**找得到**；私有制品 / 私有节点仍要 Grant。
//  3. 收下 ≠ 执行：只投递字节，平台不代跑。
//  4. 字节即事实：sha256 与 hur.json 都由服务端从收到的字节里算/读。
type AgentCard struct {
	ID        string `gorm:"primaryKey"`
	TokenHash string `gorm:"not null;uniqueIndex"`
	TokenHint string `gorm:"not null;default:''"` // token 前 6 位，便于在列表里认人
	OwnerID   string `gorm:"not null;index"`
	Name      string `gorm:"not null"`
	Note      string `gorm:"not null;default:''"`
	// BlobName 字节在 `s.Blob` 里的对象名（本节点自己持有的那份 .hur）
	BlobName string `gorm:"not null"`
	Size     int64  `gorm:"not null;default:0"`
	SHA256   string `gorm:"not null;default:''"`
	// Manifest 包内 `hur.json` 原文（**从字节里读出来的**，不是客户端报的）
	Manifest     string `gorm:"not null;default:''"`
	AgentID      string `gorm:"not null;default:''"`
	AgentVersion string `gorm:"not null;default:''"`
	AgentKind    string `gorm:"not null;default:''"`
	AgentProfile string `gorm:"not null;default:''"`
	// 节点那一侧（作者想让人连上本节点上的哪台）；ref 为空表示这张名片只有包。
	NodeRef   string `gorm:"not null;default:''"`
	NodeID    string `gorm:"not null;default:''"`
	NodeKind  string `gorm:"not null;default:''"`
	NodeLabel string `gorm:"not null;default:''"`
	// 访问口令：同样只存盐化哈希（口令与 token 是两回事 —— token 在链接里，口令是人另给的）
	PassHash string `gorm:"not null;default:''"`
	PassSalt string `gorm:"not null;default:''"`
	MaxUses  int64  `gorm:"not null;default:0"` // 0 = 不限次
	Uses     int64  `gorm:"not null;default:0"`
	ExpiresAt *time.Time `gorm:"default:null"`
	RevokedAt *time.Time `gorm:"default:null"`
	Views     int64      `gorm:"not null;default:0"`
	CreatedAt time.Time  `gorm:"autoCreateTime"`
	UpdatedAt time.Time  `gorm:"autoUpdateTime"`
}

func (AgentCard) TableName() string { return "agent_cards" }

// Usable 现在还能用吗 —— 与 `ArtifactShare.Usable` 同一套判定：撤销 / 过期 / 用尽
// 三个独立条件，缺一即失效。
//
// ⚠️ 「用完」只该拦 `accept`：读名片与取字节不能被它拦（额度是在 accept 那一刻扣的，
// 扣完就不让取字节，等于把已经拿到名额的人关在门外；平台侧实测踩过这个坑）。
func (c *AgentCard) Usable(now time.Time) bool {
	if c.RevokedAt != nil {
		return false
	}
	if c.ExpiresAt != nil && now.After(*c.ExpiresAt) {
		return false
	}
	if c.MaxUses > 0 && c.Uses >= c.MaxUses {
		return false
	}
	return true
}

// Revoked 已撤销（读名片 / 取字节要用它，而不是 Usable）。
func (c *AgentCard) Revoked() bool { return c.RevokedAt != nil }

// Expired 已过期（同上）。
func (c *AgentCard) Expired(now time.Time) bool {
	return c.ExpiresAt != nil && now.After(*c.ExpiresAt)
}

// Exhausted 名额已用完。
func (c *AgentCard) Exhausted() bool { return c.MaxUses > 0 && c.Uses >= c.MaxUses }

// RemainingUses 剩余名额：0 = 不限次（与 MaxUses 的 0 同义，便于展示）。
func (c *AgentCard) RemainingUses() int64 {
	if c.MaxUses <= 0 {
		return 0
	}
	if left := c.MaxUses - c.Uses; left > 0 {
		return left
	}
	return 0
}
