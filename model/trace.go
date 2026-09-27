// Package model：NCC Trace —— Agent / HUR 运行轨迹的采集与托管。
//
// 为什么轨迹要作为一等资源，而不是「再发布一个 kind=trace 的制品」：
//
//	制品 Artifact   可分发的能力包：字节进 blob，sha256 校验，判公开后可匿名下载。
//	轨迹 Trace      一次**运行发生过什么**的记录：给评测（能力好不好）与后训练
//	                （拿真实轨迹做 SFT / 偏好 / 奖励）用。它是「会持续增长的行为
//	                数据」，不是「一份要发给别人安装的包」。
//
// 三条设计红线（写实现时别破）：
//
//  1. **内容默认不上传**：轨迹天然含提示词、工具入参、模型输出 —— 那是业务数据。
//     所以每条轨迹带 `payload` 级别：`digest`（默认，只有哈希与结构）/ `preview`
//     （截断预览）/ `full`（原文）。**级别由采集方（CLI）决定**，服务端只如实记录，
//     绝不"顺手补全"。
//  2. **默认私有**：轨迹默认 `private`，只有归属者与同命名空间可见；跨人看要显式
//     grant（`trace`），与制品/配置/节点同一套语义。
//  3. **平台永不中转**：轨迹只进**你自己部署的节点**。云端 ncc-platform 不会因为
//     你跑了一次 `ncc trace push` 就拿到你的轨迹 —— 地址是你指定的。
//
// 轨迹与「执行留痕」的关系：`ncc hur run` 本来就会写 `harness-use-run-trace/v1`
// （燃料/内存/墙钟/结论），那是**执行器的证据**；trace 是它的**收敛格式**——
// 把「执行留痕」「Agent 的 LLM / 工具调用」「人工评分」放进同一个文档，
// 这样评测与训练拿到的是同一种东西。
package model

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"sort"
	"strconv"
	"strings"
	"time"
)

/* ---------------- 规范与词表 ---------------- */

// TraceSpec 轨迹文档规范号。跨语言（Rust CLI ↔ Go 服务端）写死的字符串。
const TraceSpec = "ncc-trace/v1"

// 轨迹种类：一次 HUR/harness 执行，或一段 Agent 会话。
const (
	TraceKindHurRun = "hur-run"
	TraceKindAgent  = "agent"
)

// TraceKinds 全部种类（顺序即展示顺序）。
var TraceKinds = []string{TraceKindHurRun, TraceKindAgent}

// ValidTraceKind 校验种类。
func ValidTraceKind(k string) bool {
	for _, v := range TraceKinds {
		if v == k {
			return true
		}
	}
	return false
}

// TraceKindLabel 种类标签（lang = zh|en）。
func TraceKindLabel(k, lang string) string {
	switch k {
	case TraceKindHurRun:
		if lang == "en" {
			return "HUR run"
		}
		return "HUR 执行"
	case TraceKindAgent:
		if lang == "en" {
			return "Agent session"
		}
		return "Agent 会话"
	}
	return k
}

// 结论。`cancelled` 与 `error` 分开：被主动中止不是失败，评测时要分开算。
const (
	TraceOK        = "ok"
	TraceError     = "error"
	TraceCancelled = "cancelled"
)

// TraceStatuses 全部结论。
var TraceStatuses = []string{TraceOK, TraceError, TraceCancelled}

// ValidTraceStatus 校验结论。
func ValidTraceStatus(s string) bool {
	for _, v := range TraceStatuses {
		if v == s {
			return true
		}
	}
	return false
}

// 内容级别 —— **默认 digest**（只有哈希与结构，不含任何原文）。
const (
	TracePayloadDigest  = "digest"
	TracePayloadPreview = "preview"
	TracePayloadFull    = "full"
)

// TracePayloadLevels 全部内容级别。
var TracePayloadLevels = []string{TracePayloadDigest, TracePayloadPreview, TracePayloadFull}

// ValidTracePayload 校验内容级别。
func ValidTracePayload(p string) bool {
	for _, v := range TracePayloadLevels {
		if v == p {
			return true
		}
	}
	return false
}

// TracePayloadRank 级别强弱（用于「这条轨迹里最强的内容级别」统计）。
func TracePayloadRank(p string) int {
	switch p {
	case TracePayloadFull:
		return 3
	case TracePayloadPreview:
		return 2
	case TracePayloadDigest:
		return 1
	}
	return 0
}

// 步骤类型。
const (
	TraceStepLLM   = "llm"
	TraceStepTool  = "tool"
	TraceStepIO    = "io"
	TraceStepNote  = "note"
	TraceStepGuard = "guard"
)

// TraceStepTypes 全部步骤类型。
var TraceStepTypes = []string{TraceStepLLM, TraceStepTool, TraceStepIO, TraceStepNote, TraceStepGuard}

// ValidTraceStepType 校验步骤类型。
func ValidTraceStepType(t string) bool {
	for _, v := range TraceStepTypes {
		if v == t {
			return true
		}
	}
	return false
}

// 可跨人读取的授权种类里新增的 `trace`（与 artifact / node / config 并列）。
// 值的定义在 model.go 的 GrantTrace（与其它授权种类放一起）。

/* ---------------- 上限 ---------------- */

const (
	// TraceMaxBytes 单条轨迹文档上限（含步骤与预览）。再大的数据请放制品。
	TraceMaxBytes = 2 * 1024 * 1024
	// TraceMaxSteps 单条轨迹的步骤数上限（防止把日志塞进来）。
	TraceMaxSteps = 2000
	// TraceBatchMax 一次上报的条数上限。
	TraceBatchMax = 500
	// TracePreviewMax 预览级别的单字段截断长度（字节，按 rune 安全截断）。
	TracePreviewMax = 512
	// TraceMaxTags 标签数上限。
	TraceMaxTags = 32
)

/* ---------------- 文档（wire 格式） ---------------- */

// TraceSource 这条轨迹从哪来：哪台机器、哪个 Agent、哪个用户。
//
// 全部是**声明**（采集方填），服务端如实记录不做校验 —— 与节点上报同一种诚实：
// 自报的东西只用于检索与归因，不是安全边界。
type TraceSource struct {
	Node   string `json:"node,omitempty"`
	Host   string `json:"host,omitempty"`
	CLI    string `json:"cli,omitempty"`
	Agent  string `json:"agent,omitempty"`
	User   string `json:"user,omitempty"`
	Region string `json:"region,omitempty"`
}

// TraceSubject 这条轨迹是**关于谁**的：哪个制品（HUR 包 / skill / agent 包）的哪一版。
//
// 评测的落点就在这里：同一个 ref 的不同 version 放在一起看，才知道「改版之后能力
// 是变好了还是变差了」。
type TraceSubject struct {
	Ref     string `json:"ref,omitempty"`
	Kind    string `json:"kind,omitempty"`
	Version string `json:"version,omitempty"`
	Digest  string `json:"digest,omitempty"`
	Engine  string `json:"engine,omitempty"`
	Policy  string `json:"policy,omitempty"`
}

// TraceModel 用到的模型（跨步骤汇总；逐步的模型写在步骤的 meta 里）。
type TraceModel struct {
	Provider string `json:"provider,omitempty"`
	Name     string `json:"name,omitempty"`
	Calls    int64  `json:"calls,omitempty"`
}

// TraceUsage 用量。**金额用整数微美元**（`costUsdMicros`）：
// 浮点数在不同语言里序列化结果不一致（`1.0` vs `1`），会毁掉跨语言摘要一致性。
type TraceUsage struct {
	InputTokens   int64 `json:"inputTokens,omitempty"`
	OutputTokens  int64 `json:"outputTokens,omitempty"`
	CostUsdMicros int64 `json:"costUsdMicros,omitempty"`
}

// TraceStep 一步：一次 LLM 调用 / 一次工具调用 / 一次 IO / 一条备注。
//
// 内容永远以**摘要**为主：`inDigest` / `outDigest` 是必填的（哪怕级别是 full），
// 这样"这条轨迹到底跑没跑过这一步"在只有摘要时也能核对。
type TraceStep struct {
	I         int    `json:"i"`
	Type      string `json:"type"`
	Name      string `json:"name,omitempty"`
	Ms        int64  `json:"ms,omitempty"`
	Status    string `json:"status,omitempty"`
	InDigest  string `json:"inDigest,omitempty"`
	OutDigest string `json:"outDigest,omitempty"`
	// In / Out 只在 payload=preview|full 时出现（preview 由采集方截断）。
	In   string         `json:"in,omitempty"`
	Out  string         `json:"out,omitempty"`
	Meta map[string]any `json:"meta,omitempty"`
}

// TraceRedaction 脱敏记录：**如实记下做过什么**，这样拿到数据集的人知道
// 「这份数据被处理成什么样」，而不是猜。
type TraceRedaction struct {
	Applied bool     `json:"applied"`
	Level   string   `json:"level,omitempty"`
	Rules   []string `json:"rules,omitempty"`
}

// TraceDoc 一条轨迹（wire 与落库都是这份 JSON）。
//
// 字段全部是字符串 / 整数 / 布尔 / 数组 / 对象 —— **没有浮点**，理由见 TraceUsage。
type TraceDoc struct {
	Spec       string         `json:"spec"`
	ID         string         `json:"id"`
	Kind       string         `json:"kind"`
	At         string         `json:"at"`
	DurationMs int64          `json:"durationMs,omitempty"`
	Status     string         `json:"status"`
	Source     TraceSource    `json:"source,omitempty"`
	Subject    TraceSubject   `json:"subject,omitempty"`
	Model      TraceModel     `json:"model,omitempty"`
	Usage      TraceUsage     `json:"usage,omitempty"`
	Steps      []TraceStep    `json:"steps,omitempty"`
	Labels     map[string]any `json:"labels,omitempty"`
	Tags       []string       `json:"tags,omitempty"`
	// Payload 声明这条轨迹里 In/Out 的**最强**内容级别。
	Payload   string         `json:"payload"`
	Redaction TraceRedaction `json:"redaction,omitempty"`
	Notes     string         `json:"notes,omitempty"`
	// Digest = TraceDigest(doc without digest)。采集方算，服务端**重算核对**：
	// 对不上就是「这份轨迹在传输中被改过」，直接拒。
	Digest string `json:"digest,omitempty"`
}

/* ---------------- 摘要（跨语言一致，别改规则） ---------------- */

// TraceDigest 轨迹摘要：`sha256:<hex>`，覆盖**身份 + 结构 + 标签**，不覆盖原文。
//
// 为什么不用「序列化整个 JSON 再哈希」：JSON 的浮点/转义/键序在不同语言里不保证
// 一致（Go 会把 `<` 转成 `\u003c`、Rust 不会；`1.0` 与 `1` 也不同），跨语言一对比
// 就假报"被篡改"。所以这里用**长度前缀拼接**这种最笨也最确定的编码：
//
//	每段写作 "<字节长度>:<内容>"，段间用 '\n'，内容原样（不做转义、不做规范化）。
//
// 覆盖范围：规范号、id、种类、时刻、用时、结论、来源、主体、模型、用量、
// 每步的 (序号/类型/名字/耗时/结论/入摘要/出摘要)、标签（键排序后的 k=v）、
// 标签数组、内容级别。**不含 In/Out 原文**（脱敏会改它，且可能很大）。
func TraceDigest(d *TraceDoc) string {
	h := sha256.Sum256([]byte(TraceDigestCore(d)))
	return "sha256:" + hex.EncodeToString(h[:])
}

// TraceDigestCore 返回被摘要的那段规范文本（测试与排查用；跨语言必须逐字节一致）。
func TraceDigestCore(d *TraceDoc) string {
	var b strings.Builder
	seg := func(k, v string) {
		b.WriteString(k)
		b.WriteByte('=')
		b.WriteString(strconv.Itoa(len(v)))
		b.WriteByte(':')
		b.WriteString(v)
		b.WriteByte('\n')
	}
	segi := func(k string, v int64) { seg(k, strconv.FormatInt(v, 10)) }

	seg("spec", d.Spec)
	seg("id", d.ID)
	seg("kind", d.Kind)
	seg("at", d.At)
	segi("durationMs", d.DurationMs)
	seg("status", d.Status)
	seg("source.node", d.Source.Node)
	seg("source.host", d.Source.Host)
	seg("source.cli", d.Source.CLI)
	seg("source.agent", d.Source.Agent)
	seg("source.user", d.Source.User)
	seg("source.region", d.Source.Region)
	seg("subject.ref", d.Subject.Ref)
	seg("subject.kind", d.Subject.Kind)
	seg("subject.version", d.Subject.Version)
	seg("subject.digest", d.Subject.Digest)
	seg("subject.engine", d.Subject.Engine)
	seg("subject.policy", d.Subject.Policy)
	seg("model.provider", d.Model.Provider)
	seg("model.name", d.Model.Name)
	segi("model.calls", d.Model.Calls)
	segi("usage.inputTokens", d.Usage.InputTokens)
	segi("usage.outputTokens", d.Usage.OutputTokens)
	segi("usage.costUsdMicros", d.Usage.CostUsdMicros)
	seg("payload", d.Payload)
	seg("redaction.level", d.Redaction.Level)
	segi("steps", int64(len(d.Steps)))
	for _, s := range d.Steps {
		segi("step.i", int64(s.I))
		seg("step.type", s.Type)
		seg("step.name", s.Name)
		segi("step.ms", s.Ms)
		seg("step.status", s.Status)
		seg("step.inDigest", s.InDigest)
		seg("step.outDigest", s.OutDigest)
	}
	segi("labels", int64(len(d.Labels)))
	for _, k := range sortedLabelKeys(d.Labels) {
		seg("label.k", k)
		seg("label.v", traceLabelValue(d.Labels[k]))
	}
	segi("tags", int64(len(d.Tags)))
	for _, t := range d.Tags {
		seg("tag", t)
	}
	return b.String()
}

// sortedLabelKeys 标签键排序（map 迭代顺序随机，不排序摘要就不稳定）。
func sortedLabelKeys(m map[string]any) []string {
	if len(m) == 0 {
		return nil
	}
	ks := make([]string, 0, len(m))
	for k := range m {
		ks = append(ks, k)
	}
	// 手写插入排序：标签最多几十个，不值得引 sort（也别让顺序依赖 Go 版本行为）。
	for i := 1; i < len(ks); i++ {
		for j := i; j > 0 && ks[j] < ks[j-1]; j-- {
			ks[j], ks[j-1] = ks[j-1], ks[j]
		}
	}
	return ks
}

// traceLabelValue 标签值的规范文本：字符串原样，数字/布尔用字面量，
// 其它（对象/数组）用**紧凑 JSON**——这里允许用 encoding/json，因为
// 标签值不参与"必须逐字节相同"的强校验（真不一致只会导致摘要不同，
// 而摘要是同一条轨迹自洽用的，不会拿来做跨端比对）。
func traceLabelValue(v any) string {
	switch x := v.(type) {
	case nil:
		return ""
	case string:
		return x
	case bool:
		if x {
			return "true"
		}
		return "false"
	case float64:
		// JSON 解码出来的数字：整数就写成整数（避免 1 vs 1.0）。
		if x == float64(int64(x)) {
			return strconv.FormatInt(int64(x), 10)
		}
		return strconv.FormatFloat(x, 'g', -1, 64)
	default:
		b, err := json.Marshal(v)
		if err != nil {
			return fmt.Sprintf("%v", v)
		}
		return string(b)
	}
}

/* ---------------- 校验 ---------------- */

// TraceIssue 一条校验结论（与制品校验的 Issue 同形：级别 + 人话）。
type TraceIssue struct {
	Level string `json:"level"` // error | warn
	Msg   string `json:"msg"`
}

// ValidateTrace 上报前的校验。**宁可拒收，也不收一条算不出摘要的轨迹**：
// 数据集一旦混入坏行，训练与评测的结论就都不可信了。
func ValidateTrace(d *TraceDoc) []TraceIssue {
	var out []TraceIssue
	errf := func(f string, a ...any) { out = append(out, TraceIssue{Level: "error", Msg: fmt.Sprintf(f, a...)}) }
	warnf := func(f string, a ...any) { out = append(out, TraceIssue{Level: "warn", Msg: fmt.Sprintf(f, a...)}) }

	if d.Spec != TraceSpec {
		errf("spec 必须是 %s，当前是 %q", TraceSpec, d.Spec)
	}
	if strings.TrimSpace(d.ID) == "" {
		errf("缺 id（采集方生成，用于幂等去重）")
	} else if len(d.ID) > 128 {
		errf("id 太长（≤128）")
	}
	if !ValidTraceKind(d.Kind) {
		errf("kind 必须是 %s，当前是 %q", strings.Join(TraceKinds, "|"), d.Kind)
	}
	if !ValidTraceStatus(d.Status) {
		errf("status 必须是 %s，当前是 %q", strings.Join(TraceStatuses, "|"), d.Status)
	}
	if !ValidTracePayload(d.Payload) {
		errf("payload 必须是 %s，当前是 %q", strings.Join(TracePayloadLevels, "|"), d.Payload)
	}
	if strings.TrimSpace(d.At) == "" {
		errf("缺 at（RFC3339 时刻）")
	} else if _, err := time.Parse(time.RFC3339, d.At); err != nil {
		errf("at 不是 RFC3339：%v", err)
	}
	if d.DurationMs < 0 {
		errf("durationMs 不能为负")
	}
	if d.Usage.InputTokens < 0 || d.Usage.OutputTokens < 0 || d.Usage.CostUsdMicros < 0 {
		errf("usage 的计数不能为负")
	}
	if d.Model.Calls < 0 {
		errf("model.calls 不能为负")
	}
	if len(d.Steps) > TraceMaxSteps {
		errf("步骤太多：%d（上限 %d）", len(d.Steps), TraceMaxSteps)
	}
	if len(d.Tags) > TraceMaxTags {
		errf("tags 太多：%d（上限 %d）", len(d.Tags), TraceMaxTags)
	}
	for i, s := range d.Steps {
		if !ValidTraceStepType(s.Type) {
			errf("步骤 %d 的类型 %q 不合法（%s）", i, s.Type, strings.Join(TraceStepTypes, "|"))
		}
		if s.I != i {
			warnf("步骤 %d 的序号是 %d（建议与顺序一致，便于对照）", i, s.I)
		}
		if s.Ms < 0 {
			errf("步骤 %d 的 ms 不能为负", i)
		}
	}
	// 内容级别与 In/Out 必须自洽：声明 digest 却带原文 = 说话不算数。
	if d.Payload == TracePayloadDigest {
		for i, s := range d.Steps {
			if s.In != "" || s.Out != "" {
				errf("payload=digest 的轨迹不该带步骤 %d 的原文（要么删原文，要么把 payload 标成 preview/full）", i)
				break
			}
		}
	}
	if d.Payload == TracePayloadPreview {
		for i, s := range d.Steps {
			if len(s.In) > TracePreviewMax || len(s.Out) > TracePreviewMax {
				warnf("payload=preview，但步骤 %d 的预览超过 %d 字节（服务端不截断，只提醒）", i, TracePreviewMax)
				break
			}
		}
	}
	// 摘要：有值就核对（采集方必须算对；算错说明两端规范不一致，早点暴露）。
	if d.Digest != "" {
		want := TraceDigest(d)
		if want != d.Digest {
			errf("digest 与文档内容不符：文档算出 %s，收到 %s", want, d.Digest)
		}
	}
	if len(out) == 0 && d.Digest == "" {
		warnf("没带 digest（服务端会自己补上；带上更利于跨端核对）")
	}
	return out
}

// TraceHasError 校验结论里有没有 error（warn 不拦）。
func TraceHasError(issues []TraceIssue) bool {
	for _, i := range issues {
		if i.Level == "error" {
			return true
		}
	}
	return false
}

// TraceMaxPayload 计算轨迹里 In/Out 实际达到的内容级别（与声明的 payload 对照，防止"标低了"）。
func TraceMaxPayload(d *TraceDoc) string {
	max := TracePayloadDigest
	if d.Payload != "" {
		max = d.Payload
	}
	for _, s := range d.Steps {
		if s.In != "" || s.Out != "" {
			if lvl := maxLevelFor(len(s.In), len(s.Out)); TracePayloadRank(lvl) > TracePayloadRank(max) {
				max = lvl
			}
		}
	}
	return max
}

func maxLevelFor(n int, m int) string {
	longest := n
	if m > longest {
		longest = m
	}
	if longest == 0 {
		return TracePayloadDigest
	}
	if longest > TracePreviewMax {
		return TracePayloadFull
	}
	return TracePayloadPreview
}

// TraceID 由内容生成一个稳定 id（采集方没给 id 时用；两端一致）。
func TraceID(d *TraceDoc) string {
	h := sha256.Sum256([]byte(TraceDigestCore(d)))
	return "TRC-" + hex.EncodeToString(h[:])[:20]
}

// NormalizeTrace 补齐最小必需字段（id / digest），供服务端兜底使用。
// **不修改内容**：只补空字段。
func NormalizeTrace(d *TraceDoc) {
	if d.Spec == "" {
		d.Spec = TraceSpec
	}
	if d.ID == "" {
		d.ID = TraceID(d)
	}
	if d.Payload == "" {
		d.Payload = TracePayloadDigest
	}
	if d.Digest == "" {
		d.Digest = TraceDigest(d)
	}
}

/* ---------------- 落库的行 ---------------- */

// Trace 一条落库的轨迹。
//
// **Doc 是不可变的**（它带采集方算的 digest）；**评测标注是可变的**且单独存
// （`trace_labels`）：分类/打分/切分是「事后别人给的判断」，不该改写被判断的事实
// —— 否则每加一个标注就要重算摘要，幂等去重与「这份轨迹没被改过」就都不成立了。
//
// 行的列是 Doc 的**索引**（检索/聚合用），不替代 Doc：文档才是权威，列只是投影。
type Trace struct {
	ID          string `gorm:"primaryKey"`
	NamespaceID string `gorm:"not null;index"`
	// TraceID 采集方的 id（客户端幂等键：同一 ns 内同一 id 只收一份）。
	TraceID string `gorm:"not null;uniqueIndex:idx_trace_ns_tid"`

	Kind       string    `gorm:"not null;index"`
	Status     string    `gorm:"not null;index"`
	At         time.Time `gorm:"not null;index"`
	DurationMs int64     `gorm:"not null;default:0"`

	SubjectRef     string `gorm:"not null;default:'';index"`
	SubjectKind    string `gorm:"not null;default:''"`
	SubjectVersion string `gorm:"not null;default:'';index"`
	SubjectDigest  string `gorm:"not null;default:''"`
	SubjectEngine  string `gorm:"not null;default:''"`
	SubjectPolicy  string `gorm:"not null;default:''"`

	NodeName  string `gorm:"not null;default:'';index"`
	Host      string `gorm:"not null;default:''"`
	AgentName string `gorm:"not null;default:'';index"`
	UserName  string `gorm:"not null;default:''"`

	ModelProvider string `gorm:"not null;default:'';index"`
	ModelName     string `gorm:"not null;default:'';index"`
	ModelCalls    int64  `gorm:"not null;default:0"`
	InputTokens   int64  `gorm:"not null;default:0"`
	OutputTokens  int64  `gorm:"not null;default:0"`
	CostUsdMicros int64  `gorm:"not null;default:0"`

	StepCount    int    `gorm:"not null;default:0"`
	PayloadLevel string `gorm:"not null;default:digest;index"`
	Tags         string `gorm:"not null;default:'[]'"`
	// RunLabels 是**采集时**带的标签（属于不可变文档的一部分，进摘要）。
	RunLabels string `gorm:"not null;default:'{}'"`
	// 评测标注的最新值的投影（完整历史在 trace_labels 表）——只为检索与聚合快。
	EvalGrade  string `gorm:"not null;default:'';index"`
	EvalReward int64  `gorm:"not null;default:0"` // 千分位（1.0 → 1000），避免浮点
	EvalScore  int64  `gorm:"not null;default:0"` // 同上（0.85 → 850）
	EvalSplit  string `gorm:"not null;default:'';index"`
	LabelCount int    `gorm:"not null;default:0"`

	Digest    string    `gorm:"not null;index"`
	Doc       string    `gorm:"not null"`
	CreatedBy string    `gorm:"not null;default:''"`
	CreatedAt time.Time `gorm:"autoCreateTime"`
}

func (Trace) TableName() string { return "traces" }

// TraceLabel 一条评测标注（只追加，不改写）。
//
// 数值用**千分位整数**（`ValueNum`）：`--reward 1` → 1000、`--score 0.85` → 850。
// 不在库里存浮点，聚合与排序就不会因精度飘移而对不上。
type TraceLabel struct {
	ID        string    `gorm:"primaryKey"`
	TraceID   string    `gorm:"not null;index"`
	Key       string    `gorm:"not null;index"`
	Value     string    `gorm:"not null;default:''"`
	ValueNum  int64     `gorm:"not null;default:0"`
	HasNum    bool      `gorm:"not null;default:false"`
	By        string    `gorm:"not null;default:''"`
	Note      string    `gorm:"not null;default:''"`
	CreatedAt time.Time `gorm:"autoCreateTime"`
}

func (TraceLabel) TableName() string { return "trace_labels" }

/* ---------------- 标注词表 ---------------- */

// TraceLabelMeta 常用标注键：id → [中文, 英文, 中文说明, 英文说明]。
//
// 键**不封闭**（自由键照收），但常用键给词表：评测与训练管线靠它对齐，
// 否则一份数据集里 `grade` / `pass` / `is_good` 三种写法会同时存在。
var TraceLabelMeta = map[string][4]string{
	"grade": {
		"结论", "Grade",
		"人工/模型给的结论：pass | fail | partial",
		"Human or model verdict: pass | fail | partial",
	},
	"reward": {
		"奖励", "Reward",
		"标量奖励（后训练用，如 1 / 0 / -1）",
		"Scalar reward for training (e.g. 1 / 0 / -1)",
	},
	"score": {
		"得分", "Score",
		"评分（0~1，评测用）",
		"Score in 0..1 (evaluation)",
	},
	"task": {
		"任务", "Task",
		"这类轨迹属于哪个业务任务（如 book-hotel）",
		"Which business task this trace belongs to (e.g. book-hotel)",
	},
	"split": {
		"数据切分", "Split",
		"train | eval | holdout（训练/评测时别混）",
		"train | eval | holdout (keep them separate)",
	},
	"failure": {
		"失败原因", "Failure",
		"失败归类（如 tool_timeout / wrong_answer）",
		"Failure taxonomy (e.g. tool_timeout / wrong_answer)",
	},
	"reviewer": {
		"标注人", "Reviewer",
		"谁做的标注（人工回来复盘时要找得到人）",
		"Who labelled it (so a human can follow up)",
	},
	"note": {
		"备注", "Note",
		"一句人话说明",
		"A free-form human note",
	},
}

// TraceLabelKeys 常用标注键（顺序即展示顺序）。
var TraceLabelKeys = []string{"grade", "reward", "score", "task", "split", "failure", "reviewer", "note"}

// TraceGrades 建议的结论取值（不强制，但强烈建议用这三档）。
var TraceGrades = []string{"pass", "fail", "partial"}

// TraceSplits 建议的数据切分取值。
var TraceSplits = []string{"train", "eval", "holdout"}

/* ---------------- 聚合（能力评估） ---------------- */

// TraceStats 一组轨迹的聚合结论。
//
// 这就是「业务能力评估」的最小数据集：成功/失败/取消各多少、耗时分布、token 与
// 花费、按制品版本分组（改版前后对比）、标注覆盖率与结论分布、失败归类前几名。
type TraceStats struct {
	Total         int64            `json:"total"`
	ByStatus      map[string]int64 `json:"byStatus"`
	ByKind        map[string]int64 `json:"byKind"`
	ByAgent       map[string]int64 `json:"byAgent"`
	ByModel       map[string]int64 `json:"byModel"`
	ByPayload     map[string]int64 `json:"byPayload"`
	BySubjectVer  map[string]int64 `json:"bySubjectVersion"`
	ByTag         map[string]int64 `json:"byTag"`
	Grades        map[string]int64 `json:"grades"`
	Failures      map[string]int64 `json:"failures"`
	Splits        map[string]int64 `json:"splits"`
	DurationMs    TracePercentiles `json:"durationMs"`
	Steps         TracePercentiles `json:"steps"`
	InputTokens   int64            `json:"inputTokens"`
	OutputTokens  int64            `json:"outputTokens"`
	CostUsdMicros int64            `json:"costUsdMicros"`
	// Labeled / Unlabeled：**标注覆盖率**。没有它，「平均分」会被未标注的轨迹稀释。
	Labeled   int64 `json:"labeled"`
	Unlabeled int64 `json:"unlabeled"`
	// RewardSumMilli / ScoreSumMilli / ScoreAvgMilli：奖励与得分之和/均值（千分位）。
	RewardSumMilli int64  `json:"rewardSumMilli"`
	ScoreSumMilli  int64  `json:"scoreSumMilli"`
	ScoreAvgMilli  int64  `json:"scoreAvgMilli"`
	FirstAt        string `json:"firstAt,omitempty"`
	LastAt         string `json:"lastAt,omitempty"`
}

// TracePercentiles 一组耗时/步数的分位数（毫秒或步）。
type TracePercentiles struct {
	Min int64 `json:"min"`
	P50 int64 `json:"p50"`
	P90 int64 `json:"p90"`
	P99 int64 `json:"p99"`
	Max int64 `json:"max"`
	Avg int64 `json:"avg"`
}

// NewTraceStats 空的聚合结构（JSON 里给空对象而不是 null，调用方少一次判空）。
func NewTraceStats() *TraceStats {
	return &TraceStats{
		ByStatus:     map[string]int64{},
		ByKind:       map[string]int64{},
		ByAgent:      map[string]int64{},
		ByModel:      map[string]int64{},
		ByPayload:    map[string]int64{},
		BySubjectVer: map[string]int64{},
		ByTag:        map[string]int64{},
		Grades:       map[string]int64{},
		Failures:     map[string]int64{},
		Splits:       map[string]int64{},
	}
}

// Percentiles 从**已排序**的样本算分位数（最近邻，不插值）。
// 样本为空时全 0——「没有数据」与「数据是 0」在展示上要靠 Total 区分。
func Percentiles(sorted []int64) TracePercentiles {
	if len(sorted) == 0 {
		return TracePercentiles{}
	}
	at := func(q float64) int64 {
		if q <= 0 {
			return sorted[0]
		}
		idx := int(q*float64(len(sorted)-1) + 0.5)
		if idx < 0 {
			idx = 0
		}
		if idx >= len(sorted) {
			idx = len(sorted) - 1
		}
		return sorted[idx]
	}
	var sum int64
	for _, v := range sorted {
		sum += v
	}
	return TracePercentiles{
		Min: sorted[0], P50: at(0.50), P90: at(0.90), P99: at(0.99), Max: sorted[len(sorted)-1],
		Avg: sum / int64(len(sorted)),
	}
}

// TraceStatsOf 由（投影列的）轨迹行算聚合结论。**纯函数**：喂同一批行，结论必须一样。
//
// 放在 model 层而不是 store：这是评测口径本身（成功怎么算、失败怎么归类、覆盖率怎么算），
// 与数据库无关；放这儿才能被单测钉住，也才能被将来的其它数据源复用。
func TraceStatsOf(rows []Trace) *TraceStats {
	st := NewTraceStats()
	durations := make([]int64, 0, len(rows))
	steps := make([]int64, 0, len(rows))
	for _, r := range rows {
		st.Total++
		st.ByStatus[r.Status]++
		st.ByKind[r.Kind]++
		st.ByPayload[r.PayloadLevel]++
		bump(st.ByAgent, r.AgentName)
		bump(st.ByModel, strings.Trim(strings.TrimSpace(r.ModelProvider+"/"+r.ModelName), "/"))
		bump(st.BySubjectVer, subjectKey(r.SubjectRef, r.SubjectVersion))
		for _, t := range decodeStringList(r.Tags) {
			bump(st.ByTag, t)
		}
		durations = append(durations, r.DurationMs)
		steps = append(steps, int64(r.StepCount))
		st.InputTokens += r.InputTokens
		st.OutputTokens += r.OutputTokens
		st.CostUsdMicros += r.CostUsdMicros
		if r.LabelCount > 0 {
			st.Labeled++
		} else {
			st.Unlabeled++
		}
		if r.EvalGrade != "" {
			bump(st.Grades, r.EvalGrade)
		}
		if r.EvalSplit != "" {
			bump(st.Splits, r.EvalSplit)
		}
		st.RewardSumMilli += r.EvalReward
		st.ScoreSumMilli += r.EvalScore
		// 失败归类：只有跑失败/取消的才读 run-time labels 里的原因。
		if r.Status != TraceOK {
			if f := failureOf(r); f != "" {
				bump(st.Failures, f)
			}
		}
		at := r.At.UTC().Format(time.RFC3339)
		if st.FirstAt == "" || at < st.FirstAt {
			st.FirstAt = at
		}
		if st.LastAt == "" || at > st.LastAt {
			st.LastAt = at
		}
	}
	sort.Slice(durations, func(i, j int) bool { return durations[i] < durations[j] })
	sort.Slice(steps, func(i, j int) bool { return steps[i] < steps[j] })
	st.DurationMs = Percentiles(durations)
	st.Steps = Percentiles(steps)
	// 平均分只对**打过分的**算：拿未标注的轨迹去稀释分母，是不诚实的平均数。
	if st.Labeled > 0 {
		st.ScoreAvgMilli = st.ScoreSumMilli / st.Labeled
	}
	return st
}

// failureOf 从 run-time labels 里取失败归类（`failure` / `failReason` / `error` 都认）。
func failureOf(r Trace) string {
	if strings.TrimSpace(r.RunLabels) == "" || r.RunLabels == "{}" {
		return ""
	}
	var m map[string]any
	if err := json.Unmarshal([]byte(r.RunLabels), &m); err != nil {
		return ""
	}
	for _, k := range []string{"failure", "failReason", "error"} {
		if v, ok := m[k]; ok {
			if s, ok := v.(string); ok && strings.TrimSpace(s) != "" {
				return s
			}
		}
	}
	return ""
}

func bump(m map[string]int64, k string) {
	if strings.TrimSpace(k) == "" {
		return
	}
	m[k]++
}

// subjectKey 把「哪个制品的哪一版」合成一个键：评测要按它分组比较。
func subjectKey(ref, version string) string {
	ref = strings.TrimSpace(ref)
	version = strings.TrimSpace(version)
	switch {
	case ref == "" && version == "":
		return ""
	case version == "":
		return ref
	case ref == "":
		return version
	}
	return ref + "@" + version
}

func decodeStringList(s string) []string {
	if strings.TrimSpace(s) == "" || s == "[]" {
		return nil
	}
	var out []string
	if err := json.Unmarshal([]byte(s), &out); err != nil {
		return nil
	}
	return out
}
