// Package model 定义 ncc-registry（内网托管节点）的领域模型。
//
// 三层概念，别混：
//
//	制品（Artifact）   被托管与分发的东西：skill / mcp / harness / api …
//	托管节点（HostedNode）用户内网里跑的东西（Agent / 服务 / 被分配的 Agent），
//	                    向 Registry 声明自己是什么、在哪、能干什么 —— 注册与心跳合并。
//	集群节点（ClusterWorker）跑 ncc-registry 的实例本身（master / worker），
//	                    是「多节点托管」的基础设施层。
package model

import "time"

/* ---------------- 制品 ---------------- */

// ArtifactKinds 允许的制品类型（与 CLI / 平台目录保持一致）。
var ArtifactKinds = []string{
	"api", "harness", "hur", "skill", "mcp", "plugin", "scaffold", "docker-image", "benchmark", "living",
}

// KindMeta 类型的中文标签与说明（/api/registry/kinds 用）。
var KindMeta = map[string][2]string{
	"api":          {"API", "可调用的服务接口"},
	"harness":      {"Harness", "按契约可装载的能力封装（含 loader/entry）"},
	"hur":          {"HUR", "Harness-Use Runtime 官方包（kind=agent 的包就是一个 Agent）"},
	"skill":        {"Skill", "给 Agent 的操作手册（SKILL.md）"},
	"mcp":          {"MCP", "Model Context Protocol 服务"},
	"plugin":       {"Plugin", "宿主应用的插件"},
	"scaffold":     {"Scaffold", "项目脚手架"},
	"docker-image": {"Docker Image", "容器镜像"},
	"benchmark":    {"Benchmark", "评测基准"},
	"living":       {"Living", "活体节点描述"},
}

// ValidStatuses 制品状态。
var ValidStatuses = []string{"draft", "published", "archived"}

// ValidVisibility 可见性。
var ValidVisibility = []string{"public", "private"}

// ValidKind 校验制品类型。
func ValidKind(k string) bool {
	for _, v := range ArtifactKinds {
		if v == k {
			return true
		}
	}
	return false
}

// ValidStatus 校验状态。
func ValidStatus(s string) bool {
	for _, v := range ValidStatuses {
		if v == s {
			return true
		}
	}
	return false
}

// 用户计划（内网版默认全员免费；保留字段以便与平台对齐）。
const (
	PlanFree = "free"
	PlanPro  = "pro"
)

// User 账户。
//
// IsAdmin 是本节点的治理权（首个注册用户自动获得，见 model/admin.go）；
// Disabled 一旦为真，**旧令牌立即失效**（authMiddleware 每次都回查一次库）。
type User struct {
	ID          string     `gorm:"primaryKey"`
	Email       string     `gorm:"uniqueIndex;not null"`
	Name        string     `gorm:"not null"`
	PassHash    string     `gorm:"not null"`
	Plan        string     `gorm:"not null;default:free"`
	IsAdmin     bool       `gorm:"not null;default:false;index"`
	Disabled    bool       `gorm:"not null;default:false;index"`
	DisabledAt  *time.Time `gorm:"default:null"`
	AdminNote   string     `gorm:"not null;default:''"`
	LastLoginAt *time.Time `gorm:"default:null"`
	CreatedAt   time.Time  `gorm:"autoCreateTime"`
}

// Namespace 命名空间：个人（account）或组织（org）。
type Namespace struct {
	ID         string    `gorm:"primaryKey"`
	Slug       string    `gorm:"uniqueIndex;not null"`
	Name       string    `gorm:"not null"`
	Type       string    `gorm:"not null;default:account"`
	OwnerID    string    `gorm:"index;not null"`
	Visibility string    `gorm:"not null;default:public"`
	CreatedAt  time.Time `gorm:"autoCreateTime"`
}

// NsMember 成员（owner 之外的可写成员）。
type NsMember struct {
	NamespaceID string `gorm:"primaryKey"`
	UserID      string `gorm:"primaryKey"`
	Role        string `gorm:"not null;default:member"`
}

// ApiKey 机器凭据（ncc_<prefix>_<secret>，只存哈希）。
type ApiKey struct {
	ID         string     `gorm:"primaryKey"`
	UserID     string     `gorm:"not null;index"`
	Label      string     `gorm:"not null;default:''"`
	Prefix     string     `gorm:"not null"`
	SecretHash string     `gorm:"not null"`
	Scopes     string     `gorm:"not null;default:[]"`
	CreatedAt  time.Time  `gorm:"autoCreateTime"`
	LastUsedAt *time.Time `gorm:"default:null"`
}

// Artifact 被托管的制品条目。
type Artifact struct {
	ID              string `gorm:"primaryKey"`
	NamespaceID     string `gorm:"not null;index;uniqueIndex:idx_art_ns_slug"`
	Slug            string `gorm:"not null;uniqueIndex:idx_art_ns_slug"`
	Kind            string `gorm:"not null;index"`
	Name            string `gorm:"not null"`
	Version         string `gorm:"not null;default:1.0.0"`
	Summary         string `gorm:"not null;default:''"`
	Tags            string `gorm:"not null;default:[]"`
	Visibility      string `gorm:"not null;default:public"`
	Status          string `gorm:"not null;default:draft;index"`
	Manifest        string `gorm:"not null;default:''"`
	StorageProvider string `gorm:"not null;default:local"` // local | byo
	StorageURL      string `gorm:"not null"`               // 对外下载地址（BYO 直链或本节点 /blobs/…）
	BlobName        string `gorm:"not null;default:''"`    // local 驱动下的字节文件名（用于本节点/代理读取）
	SHA256          string `gorm:"not null;default:''"`
	Size            int64  `gorm:"not null;default:0"`
	// Origin/OriginRef 标出这份条目的来历：local（本节点自己发布的权威记录）
	// 或 replica（由 master 分发下来的副本，本地不可改，master 回收时整份删掉）。
	Origin    string    `gorm:"not null;default:local;index"`
	OriginRef string    `gorm:"not null;default:'';index"`
	CreatedBy string    `gorm:"not null"`
	Downloads int64     `gorm:"not null;default:0"`
	CreatedAt time.Time `gorm:"autoCreateTime"`
	UpdatedAt time.Time `gorm:"autoUpdateTime"`
}

func (Artifact) TableName() string { return "artifacts" }

// IsReplica 是否为跨节点分发下来的副本。
func (a *Artifact) IsReplica() bool { return a.Origin == OriginReplica }

// 制品来历。
const (
	OriginLocal   = "local"   // 本节点权威记录
	OriginReplica = "replica" // master 分发下来的副本
)

// MirrorOwner 镜像命名空间的归属标记：副本落在自动建的镜像空间里，
// 本地没人「拥有」它（本地不可改），只有 master 能回收。
const MirrorOwner = "cluster"

// Ref 规范引用：@namespace/slug。
func (a *Artifact) Ref(nsSlug string) string { return "@" + nsSlug + "/" + a.Slug }

/* ---------------- 托管节点（用户内网的 Agent / 服务） ---------------- */

// 节点自己声明的类型。
const (
	NodeService  = "service"  // 对外提供能力的服务
	NodeAgent    = "agent"    // 为人服务的 Agent
	NodeAssigned = "assigned" // 被分配的 Agent
)

// NodeKinds 允许的节点类型。
var NodeKinds = []string{NodeService, NodeAgent, NodeAssigned}

// ValidNodeKind 校验节点类型。
func ValidNodeKind(k string) bool {
	for _, v := range NodeKinds {
		if v == k {
			return true
		}
	}
	return false
}

// HostedNode 被托管的节点：注册与心跳合并（同一 namespace+slug 续租 LastSeen）。
type HostedNode struct {
	ID           string `gorm:"primaryKey"`
	NamespaceID  string `gorm:"not null;index;uniqueIndex:idx_node_ns_slug"`
	Slug         string `gorm:"not null;uniqueIndex:idx_node_ns_slug"`
	Name         string `gorm:"not null"`
	Kind         string `gorm:"not null;default:service;index"`
	Region       string `gorm:"not null;default:'';index"`
	URL          string `gorm:"not null;default:''"`
	OS           string `gorm:"not null;default:''"`
	Arch         string `gorm:"not null;default:''"`
	Version      string `gorm:"not null;default:''"`
	Agent        string `gorm:"not null;default:''"` // 跑在哪个 Agent 形态里（可选）
	Capabilities string `gorm:"not null;default:[]"`
	// OffersVerified JSON string[] —— 节点能**自证**的提供能力（本机事实推导，不是自选的）。
	// 检索用 `?can=<id>@verified`，见 model.ParseOfferQuery。
	OffersVerified string    `gorm:"not null;default:[]"`
	Visibility     string    `gorm:"not null;default:public"`
	LastSeen       time.Time `gorm:"index"`
	CreatedAt      time.Time `gorm:"autoCreateTime"`
	UpdatedAt      time.Time `gorm:"autoUpdateTime"`
}

func (HostedNode) TableName() string { return "hosted_nodes" }

// NodeLink 一条连接：Owner 把节点收进自己的连接表，并给一个 Name 标签。
// 连接不需要对方审批（同一实例即信任域），也**不等于授权** —— 取私有节点/私有制品
// 仍然要 Grant。
type NodeLink struct {
	ID           string    `gorm:"primaryKey"`
	OwnerID      string    `gorm:"not null;index;uniqueIndex:idx_link_uk"`
	NodeID       string    `gorm:"not null;uniqueIndex:idx_link_uk"`
	TargetUserID string    `gorm:"not null;index"`
	Label        string    `gorm:"not null;default:''"`
	Note         string    `gorm:"not null;default:''"`
	CreatedAt    time.Time `gorm:"autoCreateTime"`
	UpdatedAt    time.Time `gorm:"autoUpdateTime"`
}

func (NodeLink) TableName() string { return "node_links" }

/* ---------------- 授权（Grant） ----------------

连接与授权是两件事，别混：

	NodeLink —— 「我能连到哪些 Agent 节点」：找得到
	Grant    —— 「谁能看/取我的东西」：拿得到

用连接隐式放行私有内容，等于「连一下就能下我全部私有制品」，边界太糊，
所以私有制品 / 私有节点的读取一律要求显式 Grant。
*/

// 授权种类。
const (
	GrantArtifact = "artifact" // 可拉取我命名空间下的私有 / 草稿制品
	GrantNode     = "node"     // 可看到并连接我的私有托管节点
	GrantConfig   = "config"   // 可读取我的非公开配置（配置托管）
	// GrantTrace 可读取我命名空间下的运行轨迹（轨迹默认私有，跨人看要显式授权）。
	GrantTrace = "trace"
	// GrantState 可读取我命名空间下的三样**状态**：知识库 / 记忆 / 检查点。
	//
	// 有意只开一个种类（而不是 kb/mem/ckpt 各一个）：这三样在语义上是"我的 Agent 的状态"，
	// 授权模型里先给它一个粒度；真要"只放 KB 不放记忆"再拆（PRD 里记为取舍）。
	GrantState = "state"
)

// GrantKinds 允许的授权种类（CLI 侧 `ncc grant set --kind` 的取值来源）。
var GrantKinds = []string{GrantArtifact, GrantNode, GrantConfig, GrantTrace, GrantState}

// ValidGrantKind 校验授权种类。
func ValidGrantKind(k string) bool {
	for _, v := range GrantKinds {
		if v == k {
			return true
		}
	}
	return false
}

// Grant 一条访问授权：Owner 授予 Grantee 某类资源的读取权。
// NamespaceID 为空 = Owner 的全部命名空间；非空则只放开该空间（仅 artifact 用）。
type Grant struct {
	ID            string    `gorm:"primaryKey"`
	OwnerID       string    `gorm:"not null;index;uniqueIndex:idx_grant_uk"`
	GranteeUserID string    `gorm:"not null;index;uniqueIndex:idx_grant_uk"`
	Kind          string    `gorm:"not null;uniqueIndex:idx_grant_uk"`
	NamespaceID   string    `gorm:"not null;default:'';uniqueIndex:idx_grant_uk"`
	Note          string    `gorm:"not null;default:''"`
	CreatedAt     time.Time `gorm:"autoCreateTime"`
	UpdatedAt     time.Time `gorm:"autoUpdateTime"`
}

func (Grant) TableName() string { return "grants" }

/*
	---------------- 接入票据（AccessTicket） ----------------

把「一个内网 registry」加进 Agent 的两种方式，用的是同一张票据：

	key + secret   —— 手工填（key 短、可念；secret 只显示一次，库里只存哈希）
	接入短链       —— <publicURL>/j/<key>#<secret>（secret 放 fragment，
	                  不进服务端日志、不进 Referer；顺序上链接自带凭据，一键可用）

兑换（redeem）后拿到的是一枚**节点令牌**：只能做票据授权的事（默认只读 + 上报心跳），
不能发布 / 不能改别人的东西。票据可限次数、可过期、可停用。
*/
type AccessTicket struct {
	ID          string     `gorm:"primaryKey"`
	Key         string     `gorm:"uniqueIndex;not null"` // 短 key，如 NK-7F3A2C
	SecretHash  string     `gorm:"not null"`             // sha256(secret)，明文只在创建时返回一次
	Label       string     `gorm:"not null;default:''"`
	Scopes      string     `gorm:"not null;default:[]"` // 兑换后拿到的节点令牌作用域
	NamespaceID string     `gorm:"not null;default:''"` // 票据落到哪个命名空间（空 = 创建者的个人空间）
	CreatedBy   string     `gorm:"not null;index"`
	MaxUses     int64      `gorm:"not null;default:0"` // 0 = 不限次
	UsedCount   int64      `gorm:"not null;default:0"`
	ExpiresAt   *time.Time `gorm:"default:null"`
	Disabled    bool       `gorm:"not null;default:false;index"`
	LastUsedAt  *time.Time `gorm:"default:null"`
	CreatedAt   time.Time  `gorm:"autoCreateTime"`
}

func (AccessTicket) TableName() string { return "access_tickets" }

// Usable 票据当前是否可用。
func (t *AccessTicket) Usable(now time.Time) bool {
	if t.Disabled {
		return false
	}
	if t.ExpiresAt != nil && now.After(*t.ExpiresAt) {
		return false
	}
	if t.MaxUses > 0 && t.UsedCount >= t.MaxUses {
		return false
	}
	return true
}

/* ---------------- 集群（跑 ncc-registry 的实例） ---------------- */

// ClusterWorker 一个向 master 注册的 worker 节点（master 侧记录）。
type ClusterWorker struct {
	ID           string    `gorm:"primaryKey"` // 即 worker 自己的 NodeID
	Name         string    `gorm:"not null"`
	URL          string    `gorm:"not null"`
	Version      string    `gorm:"not null;default:''"`
	Region       string    `gorm:"not null;default:''"`
	Capabilities string    `gorm:"not null;default:[]"`
	Artifacts    int64     `gorm:"not null;default:0"`
	Nodes        int64     `gorm:"not null;default:0"`
	Users        int64     `gorm:"not null;default:0"`
	FirstSeen    time.Time `gorm:"autoCreateTime"`
	LastSeen     time.Time `gorm:"index"`
}

func (ClusterWorker) TableName() string { return "cluster_workers" }

// ArtifactAdvert worker 上报的「我这里有什么」—— master 据此做目录聚合与能力路由。
type ArtifactAdvert struct {
	ID            string    `gorm:"primaryKey"`
	WorkerID      string    `gorm:"not null;index;uniqueIndex:idx_advert_uk"`
	Ref           string    `gorm:"not null;uniqueIndex:idx_advert_uk"` // @ns/slug@version
	NamespaceSlug string    `gorm:"not null"`
	Slug          string    `gorm:"not null"`
	Kind          string    `gorm:"not null;index"`
	Name          string    `gorm:"not null"`
	Version       string    `gorm:"not null"`
	Summary       string    `gorm:"not null;default:''"`
	Tags          string    `gorm:"not null;default:[]"`
	SHA256        string    `gorm:"not null;default:''"`
	Size          int64     `gorm:"not null;default:0"`
	Downloads     int64     `gorm:"not null;default:0"`
	UpdatedAt     time.Time `gorm:"not null"`
	SeenAt        time.Time `gorm:"index"`
}

func (ArtifactAdvert) TableName() string { return "artifact_adverts" }

// ReplicaTarget master 侧一行「我把某份制品分发到了哪个 worker」的记录。
//
// 为什么不用 adverts 反推：adverts 来自 worker 的**心跳**，天然滞后（可能还没上报），
// 下架回收不能等心跳 —— 分发成功那一刻就把目标记下来，回收时以它为准（adverts 仅作兜底）。
type ReplicaTarget struct {
	ID         string    `gorm:"primaryKey"`
	Ref        string    `gorm:"not null;index;uniqueIndex:idx_rep_target_uk"`
	WorkerID   string    `gorm:"not null;uniqueIndex:idx_rep_target_uk"`
	WorkerName string    `gorm:"not null;default:''"`
	WorkerURL  string    `gorm:"not null;default:''"`
	SHA256     string    `gorm:"not null;default:''"`
	Size       int64     `gorm:"not null;default:0"`
	CreatedAt  time.Time `gorm:"autoCreateTime"`
}

func (ReplicaTarget) TableName() string { return "replica_targets" }
