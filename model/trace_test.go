package model

import (
	"testing"
	"time"
)

// vectorDoc 跨语言测试向量：**Rust CLI 里有一份逐字节相同的副本**
// （`cli/src/trace.rs` 的 `vector_doc()`），两边的摘要必须完全一致。
// 改这个向量就要同时改两边 —— 这正是它的用处：摘要规则一旦不一致，测试立刻红。
func vectorDoc() *TraceDoc {
	return &TraceDoc{
		Spec:       TraceSpec,
		ID:         "TRC-0123456789abcdef0123",
		Kind:       TraceKindAgent,
		At:         "2026-09-26T10:00:00Z",
		DurationMs: 1234,
		Status:     TraceOK,
		Source: TraceSource{
			Node: "office-master", Host: "mac-1", CLI: "ncc/0.1.3",
			Agent: "harness-use", User: "@me", Region: "shanghai-intranet",
		},
		Subject: TraceSubject{
			Ref: "@alice/hotel-skill", Kind: "skill", Version: "0.1.0",
			Digest: "sha256:aa11", Engine: "wasm", Policy: "strict",
		},
		Model: TraceModel{Provider: "openai", Name: "gpt-4o-mini", Calls: 2},
		Usage: TraceUsage{InputTokens: 120, OutputTokens: 45, CostUsdMicros: 2100},
		Steps: []TraceStep{
			{I: 0, Type: TraceStepLLM, Name: "plan", Ms: 400, Status: TraceOK, InDigest: "sha256:bb22", OutDigest: "sha256:cc33"},
			{I: 1, Type: TraceStepTool, Name: "kb.search", Ms: 30, Status: TraceOK, InDigest: "sha256:dd44", OutDigest: "sha256:ee55"},
		},
		Labels:  map[string]any{"task": "book-hotel", "grade": "pass", "reward": 1},
		Tags:    []string{"prod", "hotel"},
		Payload: TracePayloadPreview,
		Redaction: TraceRedaction{
			Applied: true, Level: "strict", Rules: []string{"email", "api_key"},
		},
		Notes: "示例轨迹",
	}
}

// TestTraceDigestVector 钉住跨语言摘要：**这里算出来的值必须与 Rust 侧一致**。
//
// 这两行是刻意"写死"的：摘要规则一旦改动（加字段、换分隔符），这里会立刻失败，
// 提醒你 Rust 那边的 `trace_digest_core` 也要同步改，否则服务端会开始拒收 CLI
// 上报的轨迹（报 digest_mismatch），而且是上线后才发现。
func TestTraceDigestVector(t *testing.T) {
	d := vectorDoc()
	got := TraceDigest(d)
	const want = "sha256:867eb490e45d2ec56189ee2dd5513eec184465754e3e334ffee71d032be78526"
	if got != want {
		t.Fatalf("摘要与跨语言基线不一致：\n  得到 %s\n  基线 %s\n（若这是有意改规则，请同步更新 cli/src/trace.rs 的向量）", got, want)
	}
}

// TestTraceDigestIgnoresPayloadContent 摘要是「身份 + 结构 + 标签」，**不含原文**。
//
// 这是有意的：脱敏会改原文，原文也可能很大；而摘要是用来核对"这条轨迹是不是我发的那条"
// 与做幂等去重的。把这条性质写成测试，免得以后有人"顺手"把原文也哈希进去 ——
// 那会让同一条轨迹在脱敏前后得到不同 id，去重直接失效。
func TestTraceDigestIgnoresPayloadContent(t *testing.T) {
	a := vectorDoc()
	b := vectorDoc()
	b.Steps[0].In = "原始提示词（含邮箱 a@b.com）"
	b.Steps[0].Out = "模型输出原文"
	if TraceDigest(a) != TraceDigest(b) {
		t.Fatal("原文不该进摘要：改原文不该改摘要")
	}
	// 但结构变了必须改摘要（否则"内容没被改过"就无从核对）。
	c := vectorDoc()
	c.Steps = append(c.Steps, TraceStep{I: 2, Type: TraceStepNote, Name: "extra"})
	if TraceDigest(a) == TraceDigest(c) {
		t.Fatal("多了一步却不改摘要：结构必须进摘要")
	}
	// 标签变了也要改（评测口径的一部分）。
	d := vectorDoc()
	d.Labels["grade"] = "fail"
	if TraceDigest(a) == TraceDigest(d) {
		t.Fatal("标签变了却不改摘要：标签必须进摘要")
	}
}

func TestTraceDigestIsOrderIndependentForLabels(t *testing.T) {
	a := vectorDoc()
	a.Labels = map[string]any{"task": "x", "grade": "pass", "reward": 1}
	b := vectorDoc()
	b.Labels = map[string]any{"reward": 1, "grade": "pass", "task": "x"}
	if TraceDigest(a) != TraceDigest(b) {
		t.Fatal("标签是 map，摘要必须对插入顺序不敏感")
	}
}

func TestValidateTraceCatchesTheProblemsThatMatter(t *testing.T) {
	good := vectorDoc()
	NormalizeTrace(good)
	if issues := ValidateTrace(good); TraceHasError(issues) {
		t.Fatalf("合规轨迹不该报错：%+v", issues)
	}

	cases := []struct {
		name string
		mut  func(d *TraceDoc)
	}{
		{"spec 不对", func(d *TraceDoc) { d.Spec = "ncc-trace/v0" }},
		{"缺 id", func(d *TraceDoc) { d.ID = "" }},
		{"kind 不认识", func(d *TraceDoc) { d.Kind = "chat" }},
		{"status 不认识", func(d *TraceDoc) { d.Status = "done" }},
		{"payload 不认识", func(d *TraceDoc) { d.Payload = "everything" }},
		{"at 不是 RFC3339", func(d *TraceDoc) { d.At = "2026-09-26 10:00" }},
		{"负耗时", func(d *TraceDoc) { d.DurationMs = -1 }},
		{"负 token", func(d *TraceDoc) { d.Usage.InputTokens = -5 }},
		{"步骤类型不认识", func(d *TraceDoc) { d.Steps[0].Type = "think" }},
		{"声明 digest 却带原文", func(d *TraceDoc) {
			d.Payload = TracePayloadDigest
			d.Steps[0].In = "原文"
		}},
		{"摘要被改过", func(d *TraceDoc) { d.Digest = "sha256:deadbeef" }},
	}
	for _, c := range cases {
		d := vectorDoc()
		NormalizeTrace(d)
		c.mut(d)
		issues := ValidateTrace(d)
		if !TraceHasError(issues) {
			t.Errorf("「%s」应当被拒，但校验通过了：%+v", c.name, issues)
		}
	}
}

// TestValidateTraceWarnsWithoutDigest：没带摘要只提醒（服务端会补），
// 因为"采集方没算"是常见情况，不该因此拒收整条轨迹。
func TestValidateTraceWarnsWithoutDigest(t *testing.T) {
	d := vectorDoc()
	NormalizeTrace(d)
	d.Digest = ""
	issues := ValidateTrace(d)
	if TraceHasError(issues) {
		t.Fatalf("没带摘要不该是错误：%+v", issues)
	}
	if len(issues) == 0 {
		t.Fatal("没带摘要应当有提醒")
	}
}

func TestTraceMaxPayloadNoticesUnderstatedLevel(t *testing.T) {
	d := vectorDoc()
	d.Payload = TracePayloadDigest
	d.Steps[0].Out = string(make([]byte, TracePreviewMax+1))
	if got := TraceMaxPayload(d); got != TracePayloadFull {
		t.Fatalf("实际内容级别应当是 full，得到 %q", got)
	}
}

func TestTraceStatsOfAggregatesTheEvaluationView(t *testing.T) {
	base := time.Date(2026, 9, 26, 10, 0, 0, 0, time.UTC)
	rows := []Trace{
		{ID: "TR-1", Status: TraceOK, Kind: TraceKindAgent, At: base, DurationMs: 100, StepCount: 2,
			ModelProvider: "openai", ModelName: "gpt-4o-mini", Tags: `["prod"]`, AgentName: "harness-use",
			SubjectRef: "@a/x", SubjectVersion: "0.1.0", PayloadLevel: TracePayloadPreview,
			EvalGrade: "pass", EvalScore: 900, LabelCount: 1, EvalSplit: "eval",
			InputTokens: 10, OutputTokens: 5, CostUsdMicros: 1000},
		{ID: "TR-2", Status: TraceError, Kind: TraceKindAgent, At: base.Add(time.Minute), DurationMs: 300, StepCount: 4,
			ModelProvider: "openai", ModelName: "gpt-4o-mini", Tags: `["prod","hotel"]`, AgentName: "harness-use",
			SubjectRef: "@a/x", SubjectVersion: "0.1.0", PayloadLevel: TracePayloadDigest,
			RunLabels:   `{"failure":"tool_timeout"}`,
			InputTokens: 20, OutputTokens: 7, CostUsdMicros: 2000},
		{ID: "TR-3", Status: TraceOK, Kind: TraceKindHurRun, At: base.Add(2 * time.Minute), DurationMs: 200, StepCount: 1,
			SubjectRef: "@a/x", SubjectVersion: "0.2.0", PayloadLevel: TracePayloadFull},
	}
	st := TraceStatsOf(rows)
	if st.Total != 3 || st.ByStatus[TraceOK] != 2 || st.ByStatus[TraceError] != 1 {
		t.Fatalf("计数不对：%+v", st)
	}
	if st.Labeled != 1 || st.Unlabeled != 2 {
		t.Fatalf("标注覆盖率不对：labeled=%d unlabeled=%d", st.Labeled, st.Unlabeled)
	}
	if st.ScoreAvgMilli != 900 {
		t.Fatalf("平均分应当是 900（只有一个打了分），得到 %d", st.ScoreAvgMilli)
	}
	if st.Grades["pass"] != 1 {
		t.Fatalf("结论分布不对：%+v", st.Grades)
	}
	if st.Failures["tool_timeout"] != 1 {
		t.Fatalf("失败归类不对：%+v", st.Failures)
	}
	if st.BySubjectVer["@a/x@0.1.0"] != 2 || st.BySubjectVer["@a/x@0.2.0"] != 1 {
		t.Fatalf("按版本分组不对（评测要靠它比改版前后）：%+v", st.BySubjectVer)
	}
	if st.ByModel["openai/gpt-4o-mini"] != 2 {
		t.Fatalf("模型分布不对：%+v", st.ByModel)
	}
	if st.DurationMs.Min != 100 || st.DurationMs.Max != 300 || st.DurationMs.P50 != 200 {
		t.Fatalf("耗时分布不对：%+v", st.DurationMs)
	}
	if st.InputTokens != 30 || st.CostUsdMicros != 3000 {
		t.Fatalf("用量汇总不对：%+v", st)
	}
	if st.FirstAt != "2026-09-26T10:00:00Z" || st.LastAt != "2026-09-26T10:02:00Z" {
		t.Fatalf("时间范围不对：%s ~ %s", st.FirstAt, st.LastAt)
	}
}

func TestPercentilesNearestNeighbour(t *testing.T) {
	p := Percentiles([]int64{10, 20, 30, 40, 50})
	if p.Min != 10 || p.Max != 50 || p.P50 != 30 || p.Avg != 30 {
		t.Fatalf("分位数不对：%+v", p)
	}
	if empty := Percentiles(nil); empty != (TracePercentiles{}) {
		t.Fatalf("空样本应当是零值：%+v", empty)
	}
}
