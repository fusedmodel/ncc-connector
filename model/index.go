package model

import (
	"strings"
	"time"
)

// NCC Index（节点侧副本）：平台上登记的索引**推一份到内网节点**，于是内网里的 Agent
// 不必出网也能用一句需求检索到能办事的人。
//
// 三条边界：
//
//  1. **平台权威、节点副本**：这台节点上的条目是从平台推过来的。节点不签不改，
//     同一条（同一个来源 id）重复推是**覆盖**，不是新增。
//  2. **索引 ≠ 授权**：节点上能检索到，不等于能取到字节 —— 制品照旧走 grant，
//     私有服务照旧要 service 授权。内网节点本来就有自己的一套门禁，这里不绕过。
//  3. **节点不算信誉权重**：评分（star）只存在于平台，是平台的内部匹配权重；
//     节点侧匹配只有相关度 —— 副本不该假装自己知道全网信誉。
type IndexEntry struct {
	ID string `gorm:"primaryKey"`
	// SourceID 是平台那条索引的 id（`IX-…`）。幂等键：同一条重推就覆盖。
	SourceID string `gorm:"not null;index;uniqueIndex:idx_index_src"`
	// OwnerKey 登记人在平台上的用户 id —— 节点不认识平台账号，但按人聚合时要能分组。
	OwnerKey string `gorm:"not null;index"`
	// Owner 是展示用的登记人身份（平台 handle / 显示名），没有名片时是账号名。
	Owner        string `gorm:"not null;default:''"`
	DisplayName  string `gorm:"not null;default:''"`
	ProviderKind string `gorm:"not null;default:'';index"`
	Region       string `gorm:"not null;default:'';index"`

	Kind        string `gorm:"not null;index"` // service | capability | need
	Channel     string `gorm:"not null;index"`
	Slug        string `gorm:"not null;default:''"`
	Ref         string `gorm:"not null;default:''"` // 原件引用（@handle/slug）
	Title       string `gorm:"not null"`
	Summary     string `gorm:"not null;default:''"`
	Description string `gorm:"not null;default:''"`
	Category    string `gorm:"not null;default:'';index"`
	Tags        string `gorm:"not null;default:[]"`
	Intents     string `gorm:"not null;default:[]"`
	Languages   string `gorm:"not null;default:[]"`
	Protocol    string `gorm:"not null;default:''"`
	Endpoint    string `gorm:"not null;default:''"`
	Visibility  string `gorm:"not null;default:public;index"`
	Status      string `gorm:"not null;default:active;index"`
	Hits        int64  `gorm:"not null;default:0"`

	// PushedAt 平台那条索引的更新时刻（副本的「这份是哪一版」）。
	PushedAt   time.Time `gorm:"default:null"`
	ReceivedAt time.Time `gorm:"autoCreateTime"`
	UpdatedAt  time.Time `gorm:"autoUpdateTime"`
}

func (IndexEntry) TableName() string { return "index_entries" }

// IndexKindNeed 需求侧（与平台同一套词表）。
const IndexKindNeed = "need"

// KindIsNeed 这条索引是不是需求。
func (e *IndexEntry) KindIsNeed() bool { return e.Kind == IndexKindNeed }

// RegionMatch 区域是否覆盖（口径与平台一致：空 / 全国 / 不限 = 覆盖任何区域）。
func (e *IndexEntry) RegionMatch(region string) bool {
	region = strings.TrimSpace(region)
	if region == "" {
		return false
	}
	r := strings.TrimSpace(strings.ToLower(e.Region))
	for _, g := range []string{"", "全国", "不限", "any", "global", "all"} {
		if r == strings.ToLower(g) {
			return true
		}
	}
	return strings.Contains(e.Region, region) || strings.Contains(region, e.Region)
}
