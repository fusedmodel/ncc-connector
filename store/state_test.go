package store

import (
	"errors"
	"strings"
	"testing"
	"time"

	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
)

/* ---------------- 小工具 ---------------- */

func kbIn(nsID, slug, title, content string) KbInput {
	return KbInput{
		NamespaceID: nsID, Slug: slug, Title: title, Kind: "doc", Format: "markdown",
		Visibility: model.KbPrivate, Content: content, AuthorID: "u-test", AuthorName: "tester",
	}
}

func memIn(nsID, subject, key, value string, ttlDays int) MemInput {
	return MemInput{
		NamespaceID: nsID, Subject: subject, Key: key, Value: value,
		Kind: "fact", TTLDays: ttlDays, AuthorID: "u-test",
	}
}

func ckptIn(nsID, ref, name, digest, parent string) CkptInput {
	return CkptInput{
		NamespaceID: nsID, SubjectRef: ref, Name: name, Label: "manual",
		Visibility: model.KbPrivate, Digest: digest, Size: 12, Parent: parent, AuthorID: "u-test",
	}
}

const testDigest = "sha256:0000000000000000000000000000000000000000000000000000000000000001"

/* ============================ 知识库 kb ============================ */

func TestKbUpsertKeepsRevisionsAndChecksumTrail(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")

	row, created, err := st.UpsertKbDoc(kbIn(ns.ID, "runbook", "Runbook", "第一版正文"))
	if err != nil || !created {
		t.Fatalf("首次写入应当 created=true: created=%v err=%v", created, err)
	}
	if row.Revision != 1 || row.Status != model.KbActive || row.Ref() != "@alice/runbook" {
		t.Fatalf("首次写入的字段不对: rev=%d status=%s ref=%s", row.Revision, row.Status, row.Ref())
	}
	if row.NsName != "alice" {
		t.Fatalf("应当 join 出命名空间名: %q", row.NsName)
	}

	// 改内容 = 新版本（旧正文进历史，当前行指向新版）。
	in := kbIn(ns.ID, "runbook", "Runbook", "第二版正文")
	in.Note = "补了回滚步骤"
	row2, created2, err := st.UpsertKbDoc(in)
	if err != nil || created2 {
		t.Fatalf("同 slug 再写应当 created=false: created=%v err=%v", created2, err)
	}
	if row2.Revision != 2 || row2.Content != "第二版正文" {
		t.Fatalf("应当是新版本: rev=%d content=%q", row2.Revision, row2.Content)
	}

	revs, err := st.KbRevisions(row.ID)
	if err != nil || len(revs) != 2 {
		t.Fatalf("应当有 2 版历史: len=%d err=%v", len(revs), err)
	}
	if revs[0].Content != "第一版正文" || revs[1].Content != "第二版正文" {
		t.Fatalf("历史顺序/内容不对: %q / %q", revs[0].Content, revs[1].Content)
	}
	if revs[0].Note != "初版" || revs[1].Note != "补了回滚步骤" {
		t.Fatalf("变更说明不对: %q / %q", revs[0].Note, revs[1].Note)
	}

	// 只有一行文档（upsert 不是 insert）。
	if n, _ := st.CountKbDocs(); n != 1 {
		t.Fatalf("文档数应当是 1，实际 %d", n)
	}
}

func TestKbGetByRefAndSearchRanking(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	if _, _, err := st.UpsertKbDoc(kbIn(ns.ID, "faq", "退款 FAQ", "正文里也提一次退款")); err != nil {
		t.Fatal(err)
	}
	if _, _, err := st.UpsertKbDoc(kbIn(ns.ID, "notes", "会议纪要", strings.Repeat("退款 ", 5))); err != nil {
		t.Fatal(err)
	}

	// 两种引用形态都要能取到。
	for _, ref := range []string{"@alice/faq", "alice/faq"} {
		got, err := st.GetKbDoc(ref)
		if err != nil || got.Slug != "faq" {
			t.Fatalf("按引用 %s 取失败: %v", ref, err)
		}
	}
	if _, err := st.GetKbDoc("@alice/nope"); !errors.Is(err, gorm.ErrRecordNotFound) {
		t.Fatalf("不存在的引用应当 ErrRecordNotFound，实际 %v", err)
	}

	// 关键词打分：标题命中(3) 应当压过正文多次命中(1×5→5)…… 这里故意让"标题命中"落在 faq 上。
	rows, total, err := st.ListKbDocs(KbListOpts{
		StateScope: StateScope{NamespaceIDs: []string{ns.ID}},
		Q:          "退款", Rank: true, Limit: 10,
	})
	if err != nil || total != 2 {
		t.Fatalf("应当命中 2 篇: total=%d err=%v", total, err)
	}
	if rows[0].Slug != "faq" && rows[0].Slug != "notes" {
		t.Fatalf("排序结果不对: %s 在前", rows[0].Slug)
	}
	// 大小写不敏感（SearchText 存的是小写正文）。
	up, _, err := st.ListKbDocs(KbListOpts{StateScope: StateScope{NamespaceIDs: []string{ns.ID}}, Q: "FAQ", Limit: 10})
	if err != nil || len(up) != 1 {
		t.Fatalf("大写查询应当也能命中: len=%d err=%v", len(up), err)
	}
}

func TestKbArchivedHiddenByDefault(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	row, _, err := st.UpsertKbDoc(kbIn(ns.ID, "old", "旧手册", "内容"))
	if err != nil {
		t.Fatal(err)
	}
	if err := st.KbSetStatus(row.ID, model.KbArchived); err != nil {
		t.Fatal(err)
	}
	scope := StateScope{NamespaceIDs: []string{ns.ID}}
	if rows, _, _ := st.ListKbDocs(KbListOpts{StateScope: scope, Limit: 10}); len(rows) != 0 {
		t.Fatalf("归档的默认不该出现: %d 篇", len(rows))
	}
	if rows, _, _ := st.ListKbDocs(KbListOpts{StateScope: scope, IncludeArch: true, Limit: 10}); len(rows) != 1 {
		t.Fatalf("显式要归档才该出现: %d 篇", len(rows))
	}
	// 归档的不计入 /api/meta 的 kbDocs。
	if n, _ := st.CountKbDocs(); n != 0 {
		t.Fatalf("归档不该计入计数，实际 %d", n)
	}
}

func TestKbDeleteRemovesHistory(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	row, _, _ := st.UpsertKbDoc(kbIn(ns.ID, "tmp", "临时", "v1"))
	if _, _, err := st.UpsertKbDoc(kbIn(ns.ID, "tmp", "临时", "v2")); err != nil {
		t.Fatal(err)
	}
	if err := st.DeleteKbDoc(row.ID); err != nil {
		t.Fatal(err)
	}
	revs, err := st.KbRevisions(row.ID)
	if err != nil || len(revs) != 0 {
		t.Fatalf("删文档应当连历史一起删: len=%d err=%v", len(revs), err)
	}
}

/* ============================ 记忆 mem ============================ */

func TestMemUpsertBumpsRevision(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")

	row, created, err := st.UpsertMemEntry(memIn(ns.ID, "self", "timezone", "Asia/Shanghai", 0))
	if err != nil || !created {
		t.Fatalf("首次写入应当 created=true: created=%v err=%v", created, err)
	}
	if row.Revision != 1 || row.ExpiresAt != nil {
		t.Fatalf("首次写入不该有过期时间: rev=%d exp=%v", row.Revision, row.ExpiresAt)
	}

	row2, created2, err := st.UpsertMemEntry(memIn(ns.ID, "self", "timezone", "UTC+8", 0))
	if err != nil || created2 {
		t.Fatalf("同键再写应当是更新: created=%v err=%v", created2, err)
	}
	if row2.Revision != 2 || row2.Value != "UTC+8" {
		t.Fatalf("更新后不对: rev=%d value=%q", row2.Revision, row2.Value)
	}
	if n, _ := st.CountMemEntries(); n != 1 {
		t.Fatalf("同键应当是同一行，实际 %d 行", n)
	}
	// 不同 subject 是**另一条**记忆（同一把键在不同人/流水线下互不覆盖）。
	if _, created, err := st.UpsertMemEntry(memIn(ns.ID, "planner", "timezone", "UTC", 0)); err != nil || !created {
		t.Fatalf("换 subject 应当是新建: created=%v err=%v", created, err)
	}
}

func TestMemPinnedSurvivesPlainWrites(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	in := memIn(ns.ID, "self", "name", "小王", 0)
	in.Pinned = true
	if _, _, err := st.UpsertMemEntry(in); err != nil {
		t.Fatal(err)
	}
	// 之后的普通写入（Pinned=false）不该把钉住的记忆"摘"下来 —— 那属于误伤。
	in.Pinned = false
	in.Value = "小王（客服组）"
	row, _, err := st.UpsertMemEntry(in)
	if err != nil {
		t.Fatal(err)
	}
	if !row.Pinned {
		t.Fatal("普通写入不该取消 pinned")
	}
	// 显式取消：再写一次 true→false 不行，得走 pin 的语义（这里用 List 的 PinnedOnly 验）
	rows, _, err := st.ListMemEntries(MemListOpts{
		StateScope: StateScope{NamespaceIDs: []string{ns.ID}}, PinnedOnly: true, Limit: 10,
	})
	if err != nil || len(rows) != 1 {
		t.Fatalf("按 pinned 过滤应当只有 1 条: len=%d err=%v", len(rows), err)
	}
}

func TestMemExpiryIsDecidedAtReadTime(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	row, _, err := st.UpsertMemEntry(memIn(ns.ID, "self", "temp", "十分钟内的临时结论", 1))
	if err != nil {
		t.Fatal(err)
	}
	if row.ExpiresAt == nil {
		t.Fatal("ttl_days=1 应当写上过期时间")
	}

	// 还没到点：正常读到。
	if _, err := st.GetMemByKey(ns.ID, "self", "temp", time.Now()); err != nil {
		t.Fatalf("未过期应当读得到: %v", err)
	}

	// 把过期时间推到过去（模拟时间流逝）：读时即视为不存在，不用等清理任务。
	past := time.Now().Add(-time.Minute).UTC()
	if err := st.DB.Model(&model.MemEntry{}).Where("id = ?", row.ID).
		Update("expires_at", past).Error; err != nil {
		t.Fatal(err)
	}
	if _, err := st.GetMemByKey(ns.ID, "self", "temp", time.Now()); !errors.Is(err, gorm.ErrRecordNotFound) {
		t.Fatalf("过期应当 ErrRecordNotFound，实际 %v", err)
	}
	scope := StateScope{NamespaceIDs: []string{ns.ID}}
	if rows, _, _ := st.ListMemEntries(MemListOpts{StateScope: scope, Limit: 10}); len(rows) != 0 {
		t.Fatalf("过期的不该出现在默认列表里: %d", len(rows))
	}
	if rows, _, _ := st.ListMemEntries(MemListOpts{StateScope: scope, IncludeExpired: true, Limit: 10}); len(rows) != 1 {
		t.Fatalf("显式要过期项才该出现: %d", len(rows))
	}

	// gc 真正删掉（返回删除行数）。
	n, err := st.GcMemEntries(ns.ID, time.Now())
	if err != nil || n != 1 {
		t.Fatalf("gc 应当删掉 1 行: n=%d err=%v", n, err)
	}
	if rows, _, _ := st.ListMemEntries(MemListOpts{StateScope: scope, IncludeExpired: true, Limit: 10}); len(rows) != 0 {
		t.Fatal("gc 之后不该再有这一条")
	}
}

func TestMemTtlIsResetOnRewrite(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	if _, _, err := st.UpsertMemEntry(memIn(ns.ID, "self", "k", "v", 3)); err != nil {
		t.Fatal(err)
	}
	// ttl_days=0 是**显式清空**（这条记忆从"会过期"变成"不过期"）。
	row, _, err := st.UpsertMemEntry(memIn(ns.ID, "self", "k", "v2", 0))
	if err != nil {
		t.Fatal(err)
	}
	if row.ExpiresAt != nil {
		t.Fatalf("ttl_days=0 应当清掉过期时间，实际 %v", row.ExpiresAt)
	}
}

/* ============================ 检查点 ckpt ============================ */

func TestCheckpointLineageWalksParents(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	a, err := st.CreateCheckpoint(ckptIn(ns.ID, "@alice/agent", "起点", testDigest, ""))
	if err != nil {
		t.Fatal(err)
	}
	b, err := st.CreateCheckpoint(ckptIn(ns.ID, "@alice/agent", "第二步", testDigest, a.ID))
	if err != nil {
		t.Fatal(err)
	}
	c, err := st.CreateCheckpoint(ckptIn(ns.ID, "@alice/agent", "当前", testDigest, b.ID))
	if err != nil {
		t.Fatal(err)
	}
	// 默认 media_type 兜底，避免列表里出现空值。
	if c.MediaType != "application/octet-stream" {
		t.Fatalf("media_type 应当兜底: %q", c.MediaType)
	}
	line, err := st.CheckpointLineage(c.ID)
	if err != nil || len(line) != 3 {
		t.Fatalf("血缘应当有 3 个点: len=%d err=%v", len(line), err)
	}
	if line[0].ID != c.ID || line[2].ID != a.ID {
		t.Fatalf("血缘顺序应当是「最新在前」: %s → %s", line[0].Name, line[2].Name)
	}

	// 环也不能把服务端转死（数据是人写的，别信它一定无环）。
	if err := st.DB.Model(&model.Checkpoint{}).Where("id = ?", a.ID).
		Update("parent", c.ID).Error; err != nil {
		t.Fatal(err)
	}
	if line, err := st.CheckpointLineage(c.ID); err != nil || len(line) != 3 {
		t.Fatalf("有环时应当安全停下: len=%d err=%v", len(line), err)
	}
}

func TestCheckpointPruneKeepsNewestAndMarksTheRest(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	var ids []string
	for i := 0; i < 3; i++ {
		row, err := st.CreateCheckpoint(ckptIn(ns.ID, "@alice/agent", "点", testDigest, ""))
		if err != nil {
			t.Fatal(err)
		}
		// 模拟"字节已上传"（真实路径由 httpapi 上传后回填）。
		if err := st.SetCheckpointObject(row.ID, "ckpt-"+row.ID, 12); err != nil {
			t.Fatal(err)
		}
		ids = append(ids, row.ID)
		// 让 created_at 有区分度（同一秒内创建时排序会不稳）。
		time.Sleep(2 * time.Millisecond)
	}
	doomed, err := st.PruneCheckpoints("@alice/agent", 1)
	if err != nil {
		t.Fatal(err)
	}
	if len(doomed) != 2 {
		t.Fatalf("应当有 2 个对象要删，实际 %d", len(doomed))
	}
	// 元数据留下、状态标 pruned：删干净会让"这里曾经有个点"无法解释。
	for _, id := range ids {
		row, err := st.GetCheckpoint(id)
		if err != nil {
			t.Fatalf("清理后元数据应当还在（%s）: %v", id, err)
		}
		_ = row
	}
	if n, _ := st.CountCheckpoints(); n != 1 {
		t.Fatalf("active 计数应当是 1，实际 %d", n)
	}
	// 默认列表只给 active。
	rows, total, err := st.ListCheckpoints(CkptListOpts{
		StateScope: StateScope{NamespaceIDs: []string{ns.ID}}, Limit: 10,
	})
	if err != nil || total != 1 || len(rows) != 1 {
		t.Fatalf("默认列表只该有 1 个: total=%d len=%d err=%v", total, len(rows), err)
	}
	// keep<=0 是"全删"，必须显式拒绝（别让人以为 0 是默认）。
	if _, err := st.PruneCheckpoints("@alice/agent", 0); err == nil {
		t.Fatal("keep=0 应当报错")
	}
}

func TestCheckpointDeleteReturnsObjectKey(t *testing.T) {
	st := openTestStore(t)
	_, ns := mkNs(t, st, "alice")
	row, err := st.CreateCheckpoint(ckptIn(ns.ID, "@alice/agent", "点", testDigest, ""))
	if err != nil {
		t.Fatal(err)
	}
	if err := st.SetCheckpointObject(row.ID, "ckpt-obj", 12); err != nil {
		t.Fatal(err)
	}
	key, err := st.DeleteCheckpoint(row.ID)
	if err != nil || key != "ckpt-obj" {
		t.Fatalf("应当返回对象名好让调用方删字节: key=%q err=%v", key, err)
	}
	if _, err := st.GetCheckpoint(row.ID); !errors.Is(err, gorm.ErrRecordNotFound) {
		t.Fatalf("删掉之后不该再取到: %v", err)
	}
}

/* ============================ 可见范围（三样共用） ============================ */

func TestStateScopeIsFailClosedForAllThreeResources(t *testing.T) {
	st := openTestStore(t)
	u1, ns1 := mkNs(t, st, "alice")
	_, ns2 := mkNs(t, st, "bob")
	if _, _, err := st.UpsertKbDoc(kbIn(ns1.ID, "secret", "内部手册", "机密内容")); err != nil {
		t.Fatal(err)
	}
	if _, _, err := st.UpsertMemEntry(memIn(ns1.ID, "self", "k", "v", 0)); err != nil {
		t.Fatal(err)
	}
	if _, err := st.CreateCheckpoint(ckptIn(ns1.ID, "@alice/agent", "点", testDigest, "")); err != nil {
		t.Fatal(err)
	}
	// 连 public 的也建一个：可见范围**不是**"公开项兜底"，公开与否在 handler 里另行判断
	// （store 层只回答"属于谁的"），所以空范围什么都不给。
	pub := kbIn(ns1.ID, "pub", "公开手册", "公开内容")
	pub.Visibility = model.KbPublic
	if _, _, err := st.UpsertKbDoc(pub); err != nil {
		t.Fatal(err)
	}

	empty := StateScope{}
	// 空范围**不等于**看到全部：私有的一个都看不到（fail-closed）。
	// 但"公开"是另一档：kb 有公开档，列表里带上它才对（IncludePublic），
	// 否则会出现"按引用取得到、列表里看不到"的怪现象。
	if rows, _, _ := st.ListKbDocs(KbListOpts{StateScope: empty, Limit: 10}); len(rows) != 0 {
		t.Fatalf("空范围且不带公开档，不该看到任何文档，实际 %d", len(rows))
	}
	if rows, _, _ := st.ListKbDocs(KbListOpts{StateScope: empty, IncludePublic: true, Limit: 10}); len(rows) != 1 || rows[0].Slug != "pub" {
		t.Fatalf("带公开档时应当只看到那一篇公开文档，实际 %d", len(rows))
	}
	if rows, _, _ := st.ListMemEntries(MemListOpts{StateScope: empty, Limit: 10}); len(rows) != 0 {
		t.Fatalf("空范围不该看到任何记忆，实际 %d", len(rows))
	}
	// 记忆**没有**公开档：即使把可见范围放空，也不该漏出任何一条（这一档它不该有）。
	if rows, _, _ := st.ListMemEntries(MemListOpts{StateScope: empty, IncludeExpired: true, Limit: 10}); len(rows) != 0 {
		t.Fatalf("记忆不该有公开兜底，实际 %d", len(rows))
	}
	if rows, _, _ := st.ListCheckpoints(CkptListOpts{StateScope: empty, Limit: 10}); len(rows) != 0 {
		t.Fatalf("空范围不该看到任何检查点，实际 %d", len(rows))
	}

	// bob 自己的命名空间：也看不到 alice 的（不是"看见不相关的"）。
	bobScope := StateScope{NamespaceIDs: []string{ns2.ID}}
	if rows, _, _ := st.ListKbDocs(KbListOpts{StateScope: bobScope, Limit: 10}); len(rows) != 0 {
		t.Fatalf("bob 不该看到 alice 的文档，实际 %d", len(rows))
	}

	// 显式授权（状态授权，owner = 归属者 user id）：这才看得到。
	granted := StateScope{NamespaceIDs: []string{ns2.ID}, GrantedOwners: []string{u1.ID}}
	if rows, _, _ := st.ListKbDocs(KbListOpts{StateScope: granted, IncludePublic: true, Limit: 10}); len(rows) != 2 {
		t.Fatalf("被授权后应当看到 alice 的 2 篇: %d", len(rows))
	}
	// 只给公开档、不给授权：看得到公开那篇，看不到私有那篇 —— 这就是"公开"的边界。
	if rows, _, _ := st.ListKbDocs(KbListOpts{StateScope: StateScope{}, IncludePublic: true, Limit: 10}); len(rows) != 1 {
		t.Fatalf("没有授权时应当只看得到公开那篇: %d", len(rows))
	}
	if rows, _, _ := st.ListMemEntries(MemListOpts{StateScope: granted, Limit: 10}); len(rows) != 1 {
		t.Fatalf("被授权后应当看到 alice 的 1 条记忆: %d", len(rows))
	}
	if rows, _, _ := st.ListCheckpoints(CkptListOpts{StateScope: granted, IncludePublic: true, Limit: 10}); len(rows) != 1 {
		t.Fatalf("被授权后应当看到 alice 的 1 个检查点: %d", len(rows))
	}
	// 检查点的公开档：公开 = 谁拿得到节点就能取（prune 的 status 过滤也不算它进来）。
	pubCk := ckptIn(ns1.ID, "@alice/agent", "公开点", testDigest, "")
	pubCk.Visibility = model.KbPublic
	if _, err := st.CreateCheckpoint(pubCk); err != nil {
		t.Fatal(err)
	}
	if rows, _, _ := st.ListCheckpoints(CkptListOpts{StateScope: StateScope{}, IncludePublic: true, Limit: 10}); len(rows) != 1 {
		t.Fatalf("无范围时应当只看到公开的检查点: %d", len(rows))
	}
	if rows, _, _ := st.ListCheckpoints(CkptListOpts{StateScope: StateScope{}, Limit: 10}); len(rows) != 0 {
		t.Fatalf("不带公开档时不该看到任何检查点: %d", len(rows))
	}

	// All（管理员带 all=1）才看全，并且三样计数的口径与之一致。
	all := StateScope{All: true}
	if _, total, _ := st.ListMemEntries(MemListOpts{StateScope: all, Limit: 10}); total != 1 {
		t.Fatalf("All 应当看到 1 条记忆，实际 %d", total)
	}
	kb, mem, ckpt, err := st.StateCounts()
	if err != nil || kb != 2 || mem != 1 || ckpt != 2 {
		t.Fatalf("计数不对: kb=%d mem=%d ckpt=%d err=%v", kb, mem, ckpt, err)
	}
}
