// Package model：NCC State —— Agent 的三样状态：知识库（kb）/ 记忆（mem）/ 检查点（ckpt）。
//
// 为什么这三样**不做成 hur 包**，而要单独作为一等资源：
//
//	包（package/HUR）  是**能力**：代码 + 清单 + 确定性字节 + 签名，消费方式 = 装上去跑。
//	                    它的身份是"我能干什么"，发一版就是一个新的不可变产物。
//	状态（kb/mem/ckpt）是**数据**：会被反复写、会持续变大、默认私有、
//	                    生命周期与包版本无关（包升级了，记忆还在）。
//
// 但两者不是没关系：**包声明它需要哪些状态**（`hur.json` 的 `state{}`，规则 R11），
// 节点负责存。于是"这个 Agent 用哪些数据"是一句**可签名、可分发、可核对**的话 ——
// 这正是用户问的"kb/ckpt/mem 能不能被表示为一个 hur 包"的正确答案：
// **状态不能被表示成包，但可以被包声明。**
//
// 三者的形状本来就不同，别硬凑成一张表：
//
//	kb    文档型语料：内容进库、可检索、改一次留一版（与 config 的区别是"多且要搜"）。
//	mem   小条目键值：(命名空间, subject, key) 唯一，可 TTL、可追溯来源。
//	ckpt  不可变快照：**字节进 blob**、元数据进库、有血缘（parent）、可签名下载。
package model

import (
	"strconv"
	"strings"
	"time"
)

/* ============================ 知识库 kb ============================ */

// KbKindMeta 文档类型：id → [中文, 英文, 中文说明, 英文说明]。
var KbKindMeta = map[string][4]string{
	"doc":        {"文档", "Document", "普通文档/说明", "A general document"},
	"faq":        {"问答", "FAQ", "一问一答，适合直接命中", "Question/answer pairs that should hit directly"},
	"notes":      {"笔记", "Notes", "随手记/会议记录", "Scratch notes, meeting minutes"},
	"spec":       {"规范", "Spec", "接口/协议/约定", "Interface, protocol or convention"},
	"transcript": {"对话记录", "Transcript", "人与 Agent 的对话导出", "Exported conversations"},
}

// KbKinds 文档类型（顺序即展示顺序）。
var KbKinds = []string{"doc", "faq", "notes", "spec", "transcript"}

// KbKindLabel 类型的中英标签。
func KbKindLabel(k, lang string) string {
	meta, ok := KbKindMeta[k]
	if !ok {
		return k
	}
	if lang == "en" {
		return meta[1]
	}
	return meta[0]
}

// ValidKbKind 校验类型。
func ValidKbKind(k string) bool {
	_, ok := KbKindMeta[k]
	return ok
}

// KbFormats 允许的内容格式。
var KbFormats = []string{"markdown", "text", "json", "yaml"}

// ValidKbFormat 校验格式。
func ValidKbFormat(f string) bool {
	for _, v := range KbFormats {
		if v == f {
			return true
		}
	}
	return false
}

// KbFormatExt 格式对应的扩展名（`ncc kb pull` 落盘用）。
func KbFormatExt(f string) string {
	switch f {
	case "markdown":
		return ".md"
	case "text":
		return ".txt"
	case "json":
		return ".json"
	case "yaml":
		return ".yaml"
	default:
		return ".txt"
	}
}

// 可见性 / 状态（与配置同一套语义：KB 默认私有）。
const (
	KbPrivate = "private"
	KbPublic  = "public"
)

const (
	KbActive   = "active"
	KbArchived = "archived"
)

// ValidKbVisibility / ValidKbStatus 校验。
func ValidKbVisibility(v string) bool { return v == KbPrivate || v == KbPublic }
func ValidKbStatus(s string) bool     { return s == KbActive || s == KbArchived }

// KbMaxBytes 单篇文档上限。
//
// KB 是**语料**（要进库、要能检索），不是大文件 —— 大文件请走制品（blob）。
// 1 MB 足够放一本手册的一章；整本书该拆成多篇。
const KbMaxBytes = 1 << 20

// KbMaxTags 单篇标签数上限。
const KbMaxTags = 32

// KbDoc 一篇托管文档。引用写作 `@命名空间/slug`（与制品/配置同一套写法）。
type KbDoc struct {
	ID          string `gorm:"primaryKey"`
	NamespaceID string `gorm:"not null;index;uniqueIndex:idx_kb_ns_slug"`
	Slug        string `gorm:"not null;uniqueIndex:idx_kb_ns_slug"`
	Title       string `gorm:"not null"`
	Kind        string `gorm:"not null;default:doc;index"`
	Format      string `gorm:"not null;default:markdown"`
	Summary     string `gorm:"not null;default:''"`
	Tags        string `gorm:"not null;default:'[]'"`
	Visibility  string `gorm:"not null;default:private;index"`
	Status      string `gorm:"not null;default:active;index"`
	Revision    int64  `gorm:"not null;default:1"`
	Content     string `gorm:"not null;default:''"`
	Checksum    string `gorm:"not null;default:''"`
	Size        int64  `gorm:"not null;default:0"`
	// Source 这篇知识从哪来（url / traceId / ckpt ref / 人手写）——
	// 知识可追溯，才谈得上"这条结论是哪来的"。
	Source string `gorm:"not null;default:''"`
	// SearchText 小写正文（检索用）：SQLite 的 LIKE 对大小写不敏感但没有索引友好性，
	// 与其依赖 NOCASE 的细节，不如自己存一份小写正文。
	SearchText string    `gorm:"not null;default:''"`
	CreatedBy  string    `gorm:"not null;default:''"`
	UpdatedBy  string    `gorm:"not null;default:''"`
	CreatedAt  time.Time `gorm:"autoCreateTime"`
	UpdatedAt  time.Time `gorm:"autoUpdateTime"`
}

func (KbDoc) TableName() string { return "kb_docs" }

// IsPublic 是否公开可读。
func (k *KbDoc) IsPublic() bool { return k.Visibility == KbPublic }

// KbRevision 一次内容快照（每次写入追加一行，永不改写历史 —— 与配置同一规矩）。
type KbRevision struct {
	ID        string    `gorm:"primaryKey"`
	DocID     string    `gorm:"not null;index;uniqueIndex:idx_kb_rev_uk"`
	Revision  int64     `gorm:"not null;uniqueIndex:idx_kb_rev_uk"`
	Title     string    `gorm:"not null;default:''"`
	Content   string    `gorm:"not null;default:''"`
	Checksum  string    `gorm:"not null;default:''"`
	Size      int64     `gorm:"not null;default:0"`
	Note      string    `gorm:"not null;default:''"`
	AuthorID  string    `gorm:"not null;default:''"`
	Author    string    `gorm:"not null;default:''"`
	CreatedAt time.Time `gorm:"autoCreateTime"`
}

func (KbRevision) TableName() string { return "kb_revisions" }

// KbValidate 写入前的自检（返回错误清单，空 = 通过）。
//
// 这里**不校验 slug 的合法性**：slug 由服务端从 title 或调用方给的规范引用里取得，
// 与制品/配置共用同一套清洗（见 store 侧）。校验的是内容面：标题、类型、格式、大小。
func KbValidate(title, kind, format, visibility, content string, tags []string) []string {
	var out []string
	if strings.TrimSpace(title) == "" {
		out = append(out, "缺 title（检索结果里先看到的就是它）")
	}
	if !ValidKbKind(kind) {
		out = append(out, "kind 必须是 "+strings.Join(KbKinds, "|")+"，当前是 "+strconv.Quote(kind))
	}
	if !ValidKbFormat(format) {
		out = append(out, "format 必须是 "+strings.Join(KbFormats, "|")+"，当前是 "+strconv.Quote(format))
	}
	if !ValidKbVisibility(visibility) {
		out = append(out, "visibility 必须是 private|public，当前是 "+strconv.Quote(visibility))
	}
	if len(content) > KbMaxBytes {
		out = append(out, "内容太大（"+strconv.Itoa(len(content))+" 字节，上限 "+strconv.Itoa(KbMaxBytes)+"）—— 大文件请走制品")
	}
	if len(tags) > KbMaxTags {
		out = append(out, "tags 太多（上限 "+strconv.Itoa(KbMaxTags)+"）")
	}
	return out
}

/* ============================ 记忆 mem ============================ */

// MemKindMeta 记忆种类：id → [中文, 英文, 说明zh, 说明en]。
var MemKindMeta = map[string][4]string{
	"fact":       {"事实", "Fact", "关于世界/业务的稳定事实", "A stable fact about the world or the business"},
	"preference": {"偏好", "Preference", "这个人喜欢什么（对人不对事）", "What this person prefers"},
	"episode":    {"经历", "Episode", "上次发生了什么（常与 trace 关联）", "What happened last time (often tied to a trace)"},
	"summary":    {"小结", "Summary", "对一段历史的压缩结论", "A compressed conclusion about a stretch of history"},
	"pointer":    {"指针", "Pointer", "指向别处（kb / ckpt / trace id）", "Points elsewhere (kb / ckpt / trace id)"},
}

// MemKinds 记忆种类（顺序即展示顺序）。
var MemKinds = []string{"fact", "preference", "episode", "summary", "pointer"}

// MemKindLabel 种类标签。
func MemKindLabel(k, lang string) string {
	meta, ok := MemKindMeta[k]
	if !ok {
		return k
	}
	if lang == "en" {
		return meta[1]
	}
	return meta[0]
}

// ValidMemKind 校验种类。
func ValidMemKind(k string) bool {
	_, ok := MemKindMeta[k]
	return ok
}

// MemMaxValueBytes 单条记忆上限（记忆是"小结论"，不是文档）。
//
// 64 KB 是有意的：如果一条记忆需要更大，说明它其实是 kb 文档或者 ckpt。
const MemMaxValueBytes = 64 * 1024

// MemMaxKeyLen / MemMaxSubjectLen 键与主体长度上限。
const (
	MemMaxKeyLen     = 128
	MemMaxSubjectLen = 64
)

// MemEntry 一条记忆。
//
// 唯一性在 `(命名空间, subject, key)`：同一个键反复写就是**更新**（记忆本来就该覆盖自己），
// 但每次更新都 `Revision+1`，且可追溯来源 —— 否则"这条记忆哪来的"永远说不清。
type MemEntry struct {
	ID          string `gorm:"primaryKey"`
	NamespaceID string `gorm:"not null;index;uniqueIndex:idx_mem_key"`
	// Subject 谁的记忆：`self`（这个包共一份）或具体流水线/角色名。
	Subject string `gorm:"not null;uniqueIndex:idx_mem_key"`
	Key     string `gorm:"not null;uniqueIndex:idx_mem_key"`
	Value   string `gorm:"not null;default:''"`
	Kind    string `gorm:"not null;default:fact;index"`
	Tags    string `gorm:"not null;default:'[]'"`
	// Source 这条记忆从哪来（traceId / ckpt ref / kb ref / 人手写）。
	Source string `gorm:"not null;default:''"`
	// Confidence 置信度（千分位 0..1000；与轨迹的 score 同一口径，避免浮点）。
	Confidence int64 `gorm:"not null;default:0"`
	Pinned     bool  `gorm:"not null;default:false"`
	Revision   int64 `gorm:"not null;default:1"`
	// ExpiresAt 为空 = 不过期。**读时判定**过期（过期即视为不存在），
	// 另提供 gc 真正删掉（读时判定让"过期"立刻生效，不必等清理任务）。
	ExpiresAt *time.Time `gorm:"index"`
	CreatedBy string     `gorm:"not null;default:''"`
	UpdatedBy string     `gorm:"not null;default:''"`
	CreatedAt time.Time  `gorm:"autoCreateTime"`
	UpdatedAt time.Time  `gorm:"autoUpdateTime"`
}

func (MemEntry) TableName() string { return "mem_entries" }

// Expired 在给定时刻是否已过期。
func (m *MemEntry) Expired(now time.Time) bool {
	return m.ExpiresAt != nil && !m.ExpiresAt.After(now)
}

// MemValidate 写入前的自检。
func MemValidate(subject, key, kind, value string, confidence int64, ttlDays int) []string {
	var out []string
	if strings.TrimSpace(subject) == "" {
		out = append(out, "缺 subject（谁的记忆；整个包共一份就写 self）")
	}
	if len(subject) > MemMaxSubjectLen {
		out = append(out, "subject 太长（上限 "+strconv.Itoa(MemMaxSubjectLen)+"）")
	}
	if strings.TrimSpace(key) == "" {
		out = append(out, "缺 key（记忆的键）")
	}
	if len(key) > MemMaxKeyLen {
		out = append(out, "key 太长（上限 "+strconv.Itoa(MemMaxKeyLen)+"）")
	}
	if !ValidMemKind(kind) {
		out = append(out, "kind 必须是 "+strings.Join(MemKinds, "|")+"，当前是 "+strconv.Quote(kind))
	}
	if len(value) > MemMaxValueBytes {
		out = append(out, "value 太大（"+strconv.Itoa(len(value))+" 字节，上限 "+strconv.Itoa(MemMaxValueBytes)+"）—— 更大的内容该是 kb 或 ckpt")
	}
	if confidence < 0 || confidence > 1000 {
		out = append(out, "confidence 是千分位 0..1000（收到 "+strconv.FormatInt(confidence, 10)+"）")
	}
	if ttlDays < 0 || ttlDays > 3650 {
		out = append(out, "ttl_days 要在 0..3650（0 = 不过期）")
	}
	return out
}

/* ============================ 检查点 ckpt ============================ */

// CkptLabelMeta 打点粒度：id → [中文, 英文, 说明zh, 说明en]。
var CkptLabelMeta = map[string][4]string{
	"episode": {"回合", "Episode", "一个完整任务回合结束", "After one complete task episode"},
	"step":    {"步", "Step", "第 N 步（细粒度，量大）", "The Nth step (fine-grained, high volume)"},
	"run":     {"运行", "Run", "一次完整运行结束", "After one full run"},
	"release": {"发布", "Release", "与某个制品版本对齐", "Aligned with a published version"},
	"handoff": {"交接", "Handoff", "交给别人/别的 Agent 接管", "Handing over to someone else"},
	"manual":  {"手动", "Manual", "人手动打的点", "A checkpoint a human took"},
}

// CkptLabels 打点粒度（顺序即展示顺序）。
var CkptLabels = []string{"episode", "step", "run", "release", "handoff", "manual"}

// CkptLabelText 粒度标签。
func CkptLabelText(k, lang string) string {
	meta, ok := CkptLabelMeta[k]
	if !ok {
		return k
	}
	if lang == "en" {
		return meta[1]
	}
	return meta[0]
}

// ValidCkptLabel 校验粒度。
func ValidCkptLabel(l string) bool {
	_, ok := CkptLabelMeta[l]
	return ok
}

// CkptMaxBytes 单个检查点上限（512 MB）。
//
// 检查点是**快照**：字节进 blob，元数据进库。上限存在的意义是防止把"模型权重"
// 这种大家伙塞进来 —— 那种该走专门的制品/对象存储，而不是"Agent 的检查点"。
const CkptMaxBytes = 512 << 20

// 检查点状态。
const (
	CkptActive = "active"
	CkptPruned = "pruned"
)

// Checkpoint 一个不可变快照。
//
// **不可变**：没有 PATCH。要改就再打一个点（打点是记录"当时是什么样"，
// 允许修改就等于没有历史）。删除是 `Status=pruned` + 删字节（元数据留下，
// 这样"这里曾经有个检查点、后来被清理了"仍然可查）。
type Checkpoint struct {
	ID          string `gorm:"primaryKey"`
	NamespaceID string `gorm:"not null;index"`
	// SubjectRef 这个点属于谁（制品引用 @命名空间/slug；可以留空 = 纯粹的一次运行快照）。
	SubjectRef     string `gorm:"not null;default:'';index"`
	SubjectVersion string `gorm:"not null;default:''"`
	Name           string `gorm:"not null;default:''"`
	Label          string `gorm:"not null;default:manual;index"`
	Step           int64  `gorm:"not null;default:0"`
	Summary        string `gorm:"not null;default:''"`
	Tags           string `gorm:"not null;default:'[]'"`
	Visibility     string `gorm:"not null;default:private;index"`
	Status         string `gorm:"not null;default:active;index"`
	// Parent 上一个检查点（血缘）：能从最新点一路回溯到起点。
	Parent string `gorm:"not null;default:'';index"`
	// ObjectKey blob 对象名；Digest 是字节 sha256（上传时服务端核对）。
	ObjectKey string `gorm:"not null;default:''"`
	Digest    string `gorm:"not null;default:'';index"`
	Size      int64  `gorm:"not null;default:0"`
	MediaType string `gorm:"not null;default:application/octet-stream"`
	// Meta 自由元数据（loss / score / 步数 …）；只读展示，不参与任何判断。
	Meta      string    `gorm:"not null;default:'{}'"`
	CreatedBy string    `gorm:"not null;default:''"`
	CreatedAt time.Time `gorm:"autoCreateTime"`
}

func (Checkpoint) TableName() string { return "checkpoints" }

// CkptValidate 写入前的自检（字节面在服务端收到后再核对摘要）。
func CkptValidate(label, name, subjectRef, digest string, size int64, visibility string) []string {
	var out []string
	if !ValidCkptLabel(label) {
		out = append(out, "label 必须是 "+strings.Join(CkptLabels, "|")+"，当前是 "+strconv.Quote(label))
	}
	if strings.TrimSpace(name) == "" {
		out = append(out, "缺 name（列表里先看到的就是它）")
	}
	// 引用一律用规范写法 `@命名空间/slug`：写 `alice/agent` 也能存进去，但按
	// `--ref @alice/agent` 过滤时就匹配不上了 —— 与其以后让人对着"存进去了却查不到"发愣，
	// 不如现在就要求写对。
	if subjectRef != "" {
		if !strings.HasPrefix(subjectRef, "@") || strings.Count(subjectRef, "/") != 1 {
			out = append(out, "subject_ref 要么留空，要么写成 @命名空间/slug（收到 "+strconv.Quote(subjectRef)+"）")
		}
	}
	if !ValidKbVisibility(visibility) {
		out = append(out, "visibility 必须是 private|public，当前是 "+strconv.Quote(visibility))
	}
	if size < 0 {
		out = append(out, "size 不能为负")
	}
	if size > CkptMaxBytes {
		out = append(out, "太大（上限 "+strconv.Itoa(CkptMaxBytes)+" 字节）—— 模型权重该走对象存储/制品")
	}
	// size==0 且 digest 为空 = **先建元数据、字节稍后传**（合法的两步走：
	// 有的快照要先算很久，有的由另一个人上传字节）。这时两个字段必须**同时**空 ——
	// 只声明摘要却没有大小，或者有大小却没摘要，都是打架的。
	switch {
	case size == 0 && digest == "":
		// 元数据点：ok，字节走 PUT /api/ckpt/:id/blob（服务端那时才核对摘要）。
	case size > 0 && digest == "":
		out = append(out, "给了 size 就也要 digest（服务端要用上传的字节重新核对）")
	case size == 0 && digest != "":
		out = append(out, "给了 digest 就要给 size（或者两个都不给 = 先建元数据、字节稍后传）")
	case !strings.HasPrefix(digest, "sha256:") || len(digest) != len("sha256:")+64:
		out = append(out, "digest 必须是 sha256:<64 位十六进制>")
	}
	return out
}

/* ============================ 三样状态的共同词汇 ============================ */

// StateResources 三样状态的规范名（CLI / 能力词表 / 文档共用同一套字）。
var StateResources = []string{"kb", "mem", "ckpt"}

// StateResourceLabel 资源的中英标签。
func StateResourceLabel(r, lang string) string {
	en := map[string]string{"kb": "Knowledge base", "mem": "Memory", "ckpt": "Checkpoints"}[r]
	zh := map[string]string{"kb": "知识库", "mem": "记忆", "ckpt": "检查点"}[r]
	if lang == "en" {
		if en == "" {
			return r
		}
		return en
	}
	if zh == "" {
		return r
	}
	return zh
}

// StateOfKind 资源 ↔ 授权/作用域的短名（`kb` / `mem` / `ckpt`）。
func StateOfKind(k string) string {
	switch k {
	case "kb", "mem", "ckpt":
		return k
	}
	return ""
}
