// Package model：NCC Feedback —— 跨 Agent、跨用户的反馈。
//
// 为什么它既不是「评分」也不是「评论」：
//
//	评分（profile rating）  是**一个人对另一个人**的稳定看法，一人一条、可改，用来做信任信号。
//	反馈（feedback）        是**一次使用之后说的一句话**：谁（人或 Agent）、对哪个东西、什么时候、
//	                        是好话还是问题、有没有打分、从哪次运行来的。
//	                        它**只追加、不改**，因为它是证据；要补充就再回一条。
//
// 于是四个身份分得很开，谁都不能冒充谁：
//
//	author    是谁说的（人；人不在场时由 Agent 代说 —— 那时 agent 字段必须写清楚）
//	agent     哪个 Agent 说的（空 = 人自己说的）—— 「跨 Agent」就是这一栏
//	about     说的是什么东西（制品 / 节点 / 服务 / 名片 / 一次运行 / 一个词条）
//	owner     那东西是谁的（**服务端解析后写**，不由客户端声明）—— 私有可见与「谁能处置」按它判
//
// 为什么它会往上走（CLI → node → hub）：
//
//	反馈的**归属**跟着东西走：制品在内网节点上，反馈就落在节点上；
//	东西也在云端发布过，才谈得上把**公开**的那部分带上去。
//	带上去的路上，**谁是搬运者就以谁为准**（服务端记 relay 者的身份），
//	原话作者只作为转述写进 origin —— 「证明来源，不证明内容真实」，与网关审计同一条规矩。
package model

import (
	"strconv"
	"strings"
	"time"
)

/* ---------------- 词表 ---------------- */

// FbAboutKinds 反馈说的是**什么东西**。顺序即展示顺序。
//
//	artifact 制品（@命名空间/slug，可带版本）
//	node     节点（ND-… 或节点 id）
//	service  对外服务（@提供方/slug）
//	profile  名片（@handle）
//	run      一次运行（轨迹 id）—— 最常见的"跑完顺手说一句"
//	agent    一个 Agent（AG-… / 名字）
//	topic    还不是一个具体东西（一句话、一个词条）—— 别人还没法处理的就先记在这
var FbAboutKinds = []string{"artifact", "node", "service", "profile", "run", "agent", "topic"}

// FbAboutMeta 类型：id → [中文, 英文, 中文说明, 英文说明]。
var FbAboutMeta = map[string][4]string{
	"artifact": {"制品", "Artifact", "对某个制品（skill/mcp/hur/镜像…）的反馈", "Feedback on an artifact"},
	"node":     {"节点", "Node", "对一台主机 / 服务的反馈（离线、慢、连不上…）", "Feedback on a machine or node"},
	"service":  {"服务", "Service", "对某个对外服务的反馈（会走到提供方那里）", "Feedback on a service offering"},
	"profile":  {"名片", "Profile", "对某个人的名片/作品的反馈", "Feedback about a person's profile"},
	"run":      {"一次运行", "Run", "一次运行之后的反馈（最常用，带 traceRef）", "Feedback after a run"},
	"agent":    {"Agent", "Agent", "对某个 Agent 的反馈", "Feedback on an agent"},
	"topic":    {"词条", "Topic", "还没归到具体东西上的话", "Not attached to anything specific yet"},
}

// FbKinds 反馈的性质：报告问题 / 表扬 / 提需求 / 纠正 / 打分。
var FbKinds = []string{"report", "praise", "request", "correction", "rating"}

// FbKindMeta 性质：id → [中文, 英文, 说明zh, 说明en]。
var FbKindMeta = map[string][4]string{
	"report":     {"问题", "Report", "这里有毛病（能复现就写清怎么复现）", "Something is broken"},
	"praise":     {"表扬", "Praise", "这儿挺好用（比抱怨稀有，值钱）", "This worked well"},
	"request":    {"需求", "Request", "我希望它能……", "I wish it could…"},
	"correction": {"纠正", "Correction", "上一条说法不对 / 文档与行为不一致", "A previous claim was wrong"},
	"rating":     {"打分", "Rating", "带分数的评价（1~5）", "A scored evaluation (1-5)"},
}

// FbStatuses 处置状态。**只有目标的拥有者能改**，而且改的是"处置"不是"内容"。
var FbStatuses = []string{"open", "ack", "resolved", "wontfix"}

// 处置状态的常量（代码里引用它们，别在别处硬写字符串）。
const (
	FbOpen     = "open"
	FbAck      = "ack"
	FbResolved = "resolved"
	FbWontfix  = "wontfix"
)

// FbStatusMeta 处置：id → [中文, 英文, 说明zh, 说明en]。
var FbStatusMeta = map[string][4]string{
	"open":     {"待处理", "Open", "还没人回应", "Nobody has responded yet"},
	"ack":      {"已确认", "Acknowledged", "看到了，会处理 / 会考虑", "Seen, will look at it"},
	"resolved": {"已解决", "Resolved", "改完了（最好说清改在哪个版本）", "Fixed (say which version)"},
	"wontfix":  {"不处理", "Won't fix", "明确不做，说清为什么", "Deliberately not doing it, with a reason"},
}

// ValidFbAboutKind / ValidFbKind / ValidFbStatus / ValidFbVisibility 校验。
func ValidFbAboutKind(k string) bool { return fbIn(FbAboutKinds, k) }
func ValidFbKind(k string) bool      { return fbIn(FbKinds, k) }
func ValidFbStatus(s string) bool    { return fbIn(FbStatuses, s) }

// FbLabel 取中英标签（找不到就原样返回）。
func FbLabel(catalog map[string][4]string, k, lang string) string {
	meta, ok := catalog[k]
	if !ok {
		return k
	}
	if lang == "en" {
		return meta[1]
	}
	return meta[0]
}

// FbAboutLabel / FbKindLabel / FbStatusLabel 三个目录的标签。
func FbAboutLabel(k, lang string) string { return FbLabel(FbAboutMeta, k, lang) }
func FbKindLabel(k, lang string) string  { return FbLabel(FbKindMeta, k, lang) }
func FbStatusLabel(k, lang string) string {
	return FbLabel(FbStatusMeta, k, lang)
}

func fbIn(list []string, v string) bool {
	for _, x := range list {
		if x == v {
			return true
		}
	}
	return false
}

/* ---------------- 可见性与上限 ---------------- */

// 可见性。**默认私有**：反馈是给目标拥有者看的，要公开得作者显式说。
//
//	private  只有作者 + 目标拥有者（+ 管理员）看得到
//	public   谁都能看（也是唯一一种会被 relay 带上去的）
const (
	FbPrivate = "private"
	FbPublic  = "public"
)

// ValidFbVisibility 校验可见性。
func ValidFbVisibility(v string) bool { return v == FbPrivate || v == FbPublic }

const (
	// FbMaxBody 一句话的长度上限。反馈是话，不是文档 —— 长文请走 kb。
	FbMaxBody = 4000
	// FbMaxTags 标签数上限。
	FbMaxTags = 8
	// FbMaxHops 链路长度上限（防止把 hop 当日志写；正常一两跳）。
	FbMaxHops = 8
	// FbMaxStateRefs 关联状态引用上限（只引用，不搬内容）。
	FbMaxStateRefs = 8
	// FbMaxRef 引用字符串长度上限。
	FbMaxRef = 256
	// FbScoreMin / FbScoreMax 打分范围（0 = 没打分）。
	FbScoreMin = 0
	FbScoreMax = 5
)

/* ---------------- 记录 ---------------- */

// Feedback 一条反馈。
//
// 列表字段（Tags / Hops / StateRefs）落库是 JSON 数组文本 —— 与 kb 的 tags、
// trace 的 labels 同一套写法（SQLite 里不做关联表，读的时候一次解析）。
type Feedback struct {
	ID string `gorm:"primaryKey"`
	// OwnerID 被反馈的东西的拥有者，**服务端解析后写**。
	// 客户端说自己是给谁发的没用：私有可见与「谁能改 status」都按这一栏判。
	OwnerID string `gorm:"not null;default:'';index"`
	// AboutKind / AboutRef 说的是什么。
	//
	// ⚠️ 这里刻意**没有唯一约束**：同一个东西本来就可以被说很多次（那才是反馈）。
	AboutKind string `gorm:"not null;default:topic;index"`
	AboutRef  string `gorm:"not null;default:'';index"`
	// Kind 反馈的性质；Score 只有 rating 才有意义（0 = 没打分）。
	Kind  string `gorm:"not null;default:report;index"`
	Score int    `gorm:"not null;default:0"`
	Body  string `gorm:"not null;default:''"`
	Tags  string `gorm:"not null;default:''"`
	// AuthorID 谁说的；AgentID 哪个 Agent 说的（空 = 人自己说的）。
	AuthorID     string `gorm:"not null;default:'';index"`
	AuthorHandle string `gorm:"not null;default:''"`
	AgentID      string `gorm:"not null;default:'';index"`
	// ParentID 回复指向的那条（**回复也是一条反馈**，不改原记录）。
	ParentID string `gorm:"not null;default:'';index"`
	// Hops 链路，只追加：`cli:host,node:office,hub`。
	Hops string `gorm:"not null;default:''"`
	// Visibility / Status。
	Visibility string `gorm:"not null;default:private;index"`
	Status     string `gorm:"not null;default:open;index"`
	// TraceRef 这条反馈是从哪次运行来的；StateRefs 关联的状态引用（`mem:key`）。
	TraceRef  string `gorm:"not null;default:''"`
	StateRefs string `gorm:"not null;default:''"`
	// Origin / OriginID：从别的端搬上来的才写（`node:office` + 那边的反馈 id）。
	//
	// 这里只是**普通复合索引**（查得快），**不是唯一约束** —— 幂等靠 handler 里的
	// FindFeedbackByOrigin 判（本地反馈的 origin 是空串，用唯一索引会把它们全撞在一起：
	// 踩过，表现是「第二条反馈写不进去」，见 CHANGELOG）。
	Origin    string    `gorm:"not null;default:'';index:idx_fb_origin,priority:1"`
	OriginID  string    `gorm:"not null;default:'';index:idx_fb_origin,priority:2"`
	CreatedAt time.Time `gorm:"autoCreateTime"`
}

// TableName 表名。
func (Feedback) TableName() string { return "feedback" }

// FeedbackValidate 返回错误清单（空 = 通过）。
//
// 校验只回答"这份记录本身合不合法"；**能不能对那个东西说话、能不能看**是授权问题，
// 在 HTTP 层判（与三样状态同一套）。
func FeedbackValidate(aboutKind, aboutRef, kind string, score int, body, visibility string, tags, hops, stateRefs []string) []string {
	var out []string
	if !ValidFbAboutKind(aboutKind) {
		out = append(out, "aboutKind 必须是 "+strings.Join(FbAboutKinds, "|")+"，当前是 "+strconv.Quote(aboutKind))
	}
	if strings.TrimSpace(aboutRef) == "" {
		out = append(out, "缺 aboutRef（说的是哪个东西 —— `@命名空间/slug` / 节点 id / 运行 id）")
	}
	if len(aboutRef) > FbMaxRef {
		out = append(out, "aboutRef 太长（上限 "+strconv.Itoa(FbMaxRef)+"）")
	}
	if !ValidFbKind(kind) {
		out = append(out, "kind 必须是 "+strings.Join(FbKinds, "|")+"，当前是 "+strconv.Quote(kind))
	}
	if score < FbScoreMin || score > FbScoreMax {
		out = append(out, "score 必须在 "+strconv.Itoa(FbScoreMin)+"~"+strconv.Itoa(FbScoreMax)+" 之间（0 = 不打分）")
	}
	if kind == "rating" && score == 0 {
		out = append(out, "kind=rating 就得给分（score 1~"+strconv.Itoa(FbScoreMax)+"）")
	}
	if strings.TrimSpace(body) == "" && score == 0 {
		out = append(out, "body 与 score 不能都是空的（一条什么都没说的反馈没有意义）")
	}
	if len(body) > FbMaxBody {
		out = append(out, "body 太长（"+strconv.Itoa(len(body))+" 字节，上限 "+strconv.Itoa(FbMaxBody)+"）—— 长文请走 kb")
	}
	if !ValidFbVisibility(visibility) {
		out = append(out, "visibility 必须是 private|public，当前是 "+strconv.Quote(visibility))
	}
	if len(tags) > FbMaxTags {
		out = append(out, "tags 太多（上限 "+strconv.Itoa(FbMaxTags)+"）")
	}
	if len(hops) > FbMaxHops {
		out = append(out, "hops 太长（上限 "+strconv.Itoa(FbMaxHops)+"）—— 链路是出处，不是日志")
	}
	if len(stateRefs) > FbMaxStateRefs {
		out = append(out, "stateRefs 太多（上限 "+strconv.Itoa(FbMaxStateRefs)+"）")
	}
	return out
}

// FbRedLines 三条不变量（`/api/feedback/kinds` 与文档、README 共用同一份说法）。
//
// 与 `SharedInvariants()` 一样：**一句话只写一遍**，免得文档与代码各说各的。
func FbRedLines() []string {
	return []string{
		"反馈只追加、不可改：要补充就再回一条（回复也是一条反馈）—— 改了就不是证据了",
		"默认私有，公开要作者显式说：relay 上云只搬公开的那些，私有的永不出机器",
		"搬上来的人以令牌为准：原作者只作为转述写进 origin —— 证明来源，不证明内容真实",
	}
}
