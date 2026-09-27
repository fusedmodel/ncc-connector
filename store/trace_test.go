package store

import (
	"errors"
	"path/filepath"
	"testing"
	"time"

	"github.com/fusedmodel/ncc-registry/model"
)

// openTestStore 开一个临时库（每个测试一个文件，互不干扰）。
func openTestStore(t *testing.T) *Store {
	t.Helper()
	st, err := Open(filepath.Join(t.TempDir(), "trace-test.db"))
	if err != nil {
		t.Fatalf("开库失败: %v", err)
	}
	return st
}

// mkNs 直接建用户 + 命名空间（测试不走注册流程，省事且不依赖业务规则）。
func mkNs(t *testing.T, st *Store, slug string) (*model.User, *model.Namespace) {
	t.Helper()
	u := &model.User{ID: NewID("U"), Name: slug, Email: slug + "@corp.com"}
	if err := st.DB.Create(u).Error; err != nil {
		t.Fatalf("建用户失败: %v", err)
	}
	ns := &model.Namespace{ID: NewID("NS"), Slug: slug, Name: slug, Type: "account", OwnerID: u.ID}
	if err := st.DB.Create(ns).Error; err != nil {
		t.Fatalf("建命名空间失败: %v", err)
	}
	return u, ns
}

func traceDoc(id, status, ref, version string) *model.TraceDoc {
	d := &model.TraceDoc{
		Spec:       model.TraceSpec,
		ID:         id,
		Kind:       model.TraceKindAgent,
		At:         "2026-09-26T10:00:00Z",
		DurationMs: 100,
		Status:     status,
		Source:     model.TraceSource{Node: "n1", Agent: "harness-use"},
		Subject:    model.TraceSubject{Ref: ref, Version: version},
		Model:      model.TraceModel{Provider: "openai", Name: "gpt-4o-mini", Calls: 1},
		Usage:      model.TraceUsage{InputTokens: 10, OutputTokens: 5, CostUsdMicros: 700},
		Steps: []model.TraceStep{
			{I: 0, Type: model.TraceStepLLM, Name: "plan", Ms: 80, Status: model.TraceOK, InDigest: "sha256:a", OutDigest: "sha256:b"},
		},
		Labels:  map[string]any{"task": "book-hotel"},
		Tags:    []string{"prod"},
		Payload: model.TracePayloadDigest,
	}
	model.NormalizeTrace(d)
	return d
}

func TestInsertTraceIsIdempotentAndConflictsOnChangedContent(t *testing.T) {
	st := openTestStore(t)
	u, ns := mkNs(t, st, "alice")
	d := traceDoc("TRC-1", model.TraceOK, "@alice/skill", "0.1.0")

	row, created, err := st.InsertTrace(TraceInsert{NamespaceID: ns.ID, CreatedBy: u.ID, Doc: d})
	if err != nil || !created {
		t.Fatalf("首次写入应当 created=true: created=%v err=%v", created, err)
	}
	if row.PayloadLevel != model.TracePayloadDigest || row.StepCount != 1 {
		t.Fatalf("投影列不对: %+v", row)
	}

	// 重传：网络重试 / 离线补报是常态，必须当"已收下"而不是错误。
	again, created2, err := st.InsertTrace(TraceInsert{NamespaceID: ns.ID, CreatedBy: u.ID, Doc: d})
	if err != nil || created2 {
		t.Fatalf("同 id 同摘要应当 created=false: created=%v err=%v", created2, err)
	}
	if again.ID != row.ID {
		t.Fatalf("重传应当指向同一条记录: %s vs %s", again.ID, row.ID)
	}

	// 同 id 但内容变了：**必须拒** —— 静默覆盖会让数据集少掉一条且没人发现。
	d2 := traceDoc("TRC-1", model.TraceError, "@alice/skill", "0.1.0")
	_, _, err = st.InsertTrace(TraceInsert{NamespaceID: ns.ID, CreatedBy: u.ID, Doc: d2})
	if !errors.Is(err, ErrTraceConflict) {
		t.Fatalf("同 id 不同摘要应当报 ErrTraceConflict，实际 %v", err)
	}
}

func TestTraceVisibilityIsFailClosed(t *testing.T) {
	st := openTestStore(t)
	u1, ns1 := mkNs(t, st, "alice")
	_, ns2 := mkNs(t, st, "bob")
	d1 := traceDoc("TRC-a", model.TraceOK, "@alice/skill", "0.1.0")
	d2 := traceDoc("TRC-b", model.TraceOK, "@bob/other", "0.2.0")
	if _, _, err := st.InsertTrace(TraceInsert{NamespaceID: ns1.ID, CreatedBy: u1.ID, Doc: d1}); err != nil {
		t.Fatal(err)
	}
	if _, _, err := st.InsertTrace(TraceInsert{NamespaceID: ns2.ID, CreatedBy: u1.ID, Doc: d2}); err != nil {
		t.Fatal(err)
	}

	// 只看自己的命名空间 → 只有自己那条。
	rows, total, err := st.ListTraces(TraceListOpts{NamespaceIDs: []string{ns1.ID}})
	if err != nil || total != 1 || len(rows) != 1 {
		t.Fatalf("按命名空间过滤不对: total=%d rows=%d err=%v", total, len(rows), err)
	}
	// 什么都没给 → 查不到（fail-closed，别把"没给条件"当成"看全部"）。
	if _, total, _ := st.ListTraces(TraceListOpts{}); total != 0 {
		t.Fatalf("没有任何可见范围时应当查不到，实际 %d 条", total)
	}
	// 被授权者的命名空间可见
	if _, total, _ := st.ListTraces(TraceListOpts{GrantedOwners: []string{ns2.OwnerID}}); total != 1 {
		t.Fatalf("被授权的命名空间应当可见，实际 %d 条", total)
	}
	// 管理员视角：全部
	if _, total, _ := st.ListTraces(TraceListOpts{All: true}); total != 2 {
		t.Fatalf("All 应当看到 2 条，实际 %d", total)
	}
}

func TestTraceFiltersAndExportTruncation(t *testing.T) {
	st := openTestStore(t)
	u, ns := mkNs(t, st, "alice")
	for i, d := range []*model.TraceDoc{
		traceDoc("TRC-1", model.TraceOK, "@alice/skill", "0.1.0"),
		traceDoc("TRC-2", model.TraceError, "@alice/skill", "0.1.0"),
		traceDoc("TRC-3", model.TraceOK, "@alice/skill", "0.2.0"),
	} {
		// at 错开，便于验证时间过滤与排序。
		d.At = time.Date(2026, 9, 26, 10, i, 0, 0, time.UTC).Format(time.RFC3339)
		model.NormalizeTrace(d)
		if _, _, err := st.InsertTrace(TraceInsert{NamespaceID: ns.ID, CreatedBy: u.ID, Doc: d}); err != nil {
			t.Fatal(err)
		}
	}
	base := TraceListOpts{NamespaceIDs: []string{ns.ID}}
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: base.NamespaceIDs, Ref: "@alice/skill", Status: model.TraceError}); total != 1 {
		t.Fatal("按 ref+status 过滤不对")
	}
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: base.NamespaceIDs, Status: model.TraceOK}); total != 2 {
		t.Fatal("按 status 过滤不对")
	}
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: base.NamespaceIDs, Tag: "prod"}); total != 3 {
		t.Fatal("按 tag 过滤不对")
	}
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: base.NamespaceIDs, Model: "openai/gpt-4o-mini"}); total != 3 {
		t.Fatal("按 provider/name 过滤不对")
	}
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: base.NamespaceIDs, Since: time.Date(2026, 9, 26, 10, 1, 0, 0, time.UTC)}); total != 2 {
		t.Fatal("按时间过滤不对")
	}
	// 标注过滤：一条都没标注
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: base.NamespaceIDs, OnlyUnlabeled: true}); total != 3 {
		t.Fatal("未标注过滤不对")
	}
	// 失败优先看：FailuresOnly 包含 error 与 cancelled，不含 ok
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: base.NamespaceIDs, FailuresOnly: true}); total != 1 {
		t.Fatal("只看失败不对")
	}

	// 导出：limit 截断必须**如实报**（悄悄少给几行，训练集就少一截）。
	rows, truncated, err := st.ExportTraces(base, 2)
	if err != nil || len(rows) != 2 || !truncated {
		t.Fatalf("导出截断标志不对: rows=%d truncated=%v err=%v", len(rows), truncated, err)
	}
	if rows[0].Doc == "" {
		t.Fatal("导出必须带 Doc（否则数据集没有内容）")
	}
	rows, truncated, _ = st.ExportTraces(base, 10)
	if len(rows) != 3 || truncated {
		t.Fatalf("不截断时不该报 truncated: rows=%d truncated=%v", len(rows), truncated)
	}
}

func TestTraceLabelsAreAppendOnlyAndProjectToTheRow(t *testing.T) {
	st := openTestStore(t)
	u, ns := mkNs(t, st, "alice")
	d := traceDoc("TRC-1", model.TraceOK, "@alice/skill", "0.1.0")
	row, _, err := st.InsertTrace(TraceInsert{NamespaceID: ns.ID, CreatedBy: u.ID, Doc: d})
	if err != nil {
		t.Fatal(err)
	}
	add := func(in TraceLabelInput) {
		in.TraceID = row.ID
		if _, err := st.AddTraceLabel(in); err != nil {
			t.Fatal(err)
		}
	}
	add(TraceLabelInput{Key: "grade", Value: "pass", By: u.ID})
	add(TraceLabelInput{Key: "reward", Value: "1", ValueNum: 1000, HasNum: true, By: u.ID})
	add(TraceLabelInput{Key: "score", Value: "850", ValueNum: 850, HasNum: true, By: u.ID})
	add(TraceLabelInput{Key: "split", Value: "eval", By: u.ID})

	got, err := st.GetTrace(row.ID)
	if err != nil {
		t.Fatal(err)
	}
	if got.EvalGrade != "pass" || got.EvalReward != 1000 || got.EvalScore != 850 || got.EvalSplit != "eval" {
		t.Fatalf("标注投影不对: %+v", got)
	}
	if got.LabelCount != 4 {
		t.Fatalf("标注计数不对: %d", got.LabelCount)
	}
	// 文档本身**不变**（标注不改写被判断的事实）。
	if got.Digest != d.Digest {
		t.Fatalf("标注不该改文档摘要: %s vs %s", got.Digest, d.Digest)
	}
	// 历史留得住（只追加）。
	ls, err := st.ListTraceLabels(row.ID)
	if err != nil || len(ls) != 4 {
		t.Fatalf("标注历史不对: %d 条 err=%v", len(ls), err)
	}
	// 标注后能按 grade/split 过滤，且不再算"未标注"。
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: []string{ns.ID}, Grade: "pass"}); total != 1 {
		t.Fatal("按 grade 过滤不对")
	}
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: []string{ns.ID}, Split: "eval"}); total != 1 {
		t.Fatal("按 split 过滤不对")
	}
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: []string{ns.ID}, OnlyLabeled: true}); total != 1 {
		t.Fatal("只看已标注不对")
	}
	if _, total, _ := st.ListTraces(TraceListOpts{NamespaceIDs: []string{ns.ID}, OnlyUnlabeled: true}); total != 0 {
		t.Fatal("标注后不该再算未标注")
	}

	// 删除会把标注一起删（否则留下孤儿行）。
	if err := st.DeleteTrace(row.ID); err != nil {
		t.Fatal(err)
	}
	if ls, _ := st.ListTraceLabels(row.ID); len(ls) != 0 {
		t.Fatalf("删除轨迹后标注应当一并清掉，剩 %d 条", len(ls))
	}
}

func TestTraceStatsReadsTheProjectionOnly(t *testing.T) {
	st := openTestStore(t)
	u, ns := mkNs(t, st, "alice")
	for i, s := range []string{model.TraceOK, model.TraceOK, model.TraceError} {
		d := traceDoc("TRC-"+string(rune('a'+i)), s, "@alice/skill", "0.1.0")
		d.At = time.Date(2026, 9, 26, 10, i, 0, 0, time.UTC).Format(time.RFC3339)
		model.NormalizeTrace(d)
		if _, _, err := st.InsertTrace(TraceInsert{NamespaceID: ns.ID, CreatedBy: u.ID, Doc: d}); err != nil {
			t.Fatal(err)
		}
	}
	rows, truncated, err := st.ScanTracesForStats(TraceListOpts{NamespaceIDs: []string{ns.ID}}, 100)
	if err != nil || truncated || len(rows) != 3 {
		t.Fatalf("取样不对: %d 条 truncated=%v err=%v", len(rows), truncated, err)
	}
	st2 := model.TraceStatsOf(rows)
	if st2.Total != 3 || st2.ByStatus[model.TraceOK] != 2 || st2.ByStatus[model.TraceError] != 1 {
		t.Fatalf("聚合不对: %+v", st2.ByStatus)
	}
	// 上限保护：cap 比实际条数小 → 如实报 truncated。
	if _, truncated, _ := st.ScanTracesForStats(TraceListOpts{NamespaceIDs: []string{ns.ID}}, 2); !truncated {
		t.Fatal("超过取样上限时应当报 truncated")
	}
	n, err := st.CountTraces()
	if err != nil || n != 3 {
		t.Fatalf("计数不对: %d err=%v", n, err)
	}
}
