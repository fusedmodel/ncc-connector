package model

import (
	"strings"
	"testing"
	"time"
)

/* ---------------- 知识库 kb ---------------- */

func TestKbValidateCoversVocabularyAndCaps(t *testing.T) {
	ok := KbValidate("退款流程", "doc", "markdown", KbPrivate, "正文", []string{"support"})
	if len(ok) != 0 {
		t.Fatalf("正常输入不该报错: %v", ok)
	}
	// 词表外的值要被点出来，而且**要说清有哪些合法值**（报错是给人改代码用的）。
	for _, c := range []struct {
		name string
		errs []string
		need string
	}{
		{"kind 词表外", KbValidate("t", "nope", "markdown", KbPrivate, "x", nil), "kind 必须是"},
		{"format 词表外", KbValidate("t", "doc", "nope", KbPrivate, "x", nil), "format 必须是"},
		{"visibility 词表外", KbValidate("t", "doc", "markdown", "secret", "x", nil), "visibility 必须是"},
		{"缺标题", KbValidate("  ", "doc", "markdown", KbPrivate, "x", nil), "缺 title"},
	} {
		if len(c.errs) == 0 {
			t.Fatalf("%s 应当报错", c.name)
		}
		if !strings.Contains(strings.Join(c.errs, "; "), c.need) {
			t.Fatalf("%s 报错里应当提到 %q，实际 %v", c.name, c.need, c.errs)
		}
	}
	// 上限：1 MB（KB 是语料不是大文件）—— 边界值本身要**通过**。
	if errs := KbValidate("t", "doc", "markdown", KbPrivate, strings.Repeat("a", KbMaxBytes), nil); len(errs) != 0 {
		t.Fatalf("正好到上限不该报错: %v", errs)
	}
	if errs := KbValidate("t", "doc", "markdown", KbPrivate, strings.Repeat("a", KbMaxBytes+1), nil); len(errs) == 0 {
		t.Fatal("超过上限应当报错")
	}
	// 标签数上限。
	many := make([]string, KbMaxTags+1)
	if errs := KbValidate("t", "doc", "markdown", KbPrivate, "x", many); len(errs) == 0 {
		t.Fatal("标签超上限应当报错")
	}
}

/* ---------------- 记忆 mem ---------------- */

func TestMemValidateCoversVocabularyAndCaps(t *testing.T) {
	if errs := MemValidate("self", "timezone", "fact", "Asia/Shanghai", 800, 30); len(errs) != 0 {
		t.Fatalf("正常输入不该报错: %v", errs)
	}
	cases := []struct {
		name string
		errs []string
		want string
	}{
		{"缺 subject", MemValidate("", "k", "fact", "v", 0, 0), "缺 subject"},
		{"subject 太长", MemValidate(strings.Repeat("s", MemMaxSubjectLen+1), "k", "fact", "v", 0, 0), "subject 太长"},
		{"缺 key", MemValidate("self", "", "fact", "v", 0, 0), "缺 key"},
		{"key 太长", MemValidate("self", strings.Repeat("k", MemMaxKeyLen+1), "fact", "v", 0, 0), "key 太长"},
		{"kind 词表外", MemValidate("self", "k", "nope", "v", 0, 0), "kind 必须是"},
		{"value 太大", MemValidate("self", "k", "fact", strings.Repeat("v", MemMaxValueBytes+1), 0, 0), "value 太大"},
		{"置信度为负", MemValidate("self", "k", "fact", "v", -1, 0), "confidence"},
		{"置信度超千", MemValidate("self", "k", "fact", "v", 1001, 0), "confidence"},
		{"TTL 为负", MemValidate("self", "k", "fact", "v", 0, -1), "ttl_days"},
		{"TTL 超十年", MemValidate("self", "k", "fact", "v", 0, 3651), "ttl_days"},
	}
	for _, c := range cases {
		if len(c.errs) == 0 {
			t.Fatalf("%s 应当报错", c.name)
		}
		if !strings.Contains(strings.Join(c.errs, "; "), c.want) {
			t.Fatalf("%s 的报错里应当提到 %q，实际 %v", c.name, c.want, c.errs)
		}
	}
	// value 正好到上限：通过（边界不能一边倒地拒绝）。
	if errs := MemValidate("self", "k", "fact", strings.Repeat("v", MemMaxValueBytes), 1000, 3650); len(errs) != 0 {
		t.Fatalf("边界值不该报错: %v", errs)
	}
}

func TestMemExpiredIsHalfOpenAtTheDeadline(t *testing.T) {
	now := time.Date(2026, 9, 26, 12, 0, 0, 0, time.UTC)
	if (&MemEntry{}).Expired(now) {
		t.Fatal("没有 expiresAt = 永不过期")
	}
	// 到点就算过期（免得"刚好那一秒还能读到"的边界争议）。
	at := now
	if !(&MemEntry{ExpiresAt: &at}).Expired(now) {
		t.Fatal("正好到点应当算过期")
	}
	later := now.Add(time.Second)
	if (&MemEntry{ExpiresAt: &later}).Expired(now) {
		t.Fatal("还没到点不该算过期")
	}
}

/* ---------------- 检查点 ckpt ---------------- */

func TestCkptValidateRequiresDigestAndSizeTogether(t *testing.T) {
	full := "sha256:" + strings.Repeat("a", 64)
	if errs := CkptValidate("run", "snap1", "@alice/agent", full, 1024, KbPrivate); len(errs) != 0 {
		t.Fatalf("摘要+大小都给时不该报错: %v", errs)
	}
	// 元数据点（先建元数据、字节稍后传）：两个都空是合法的。
	if errs := CkptValidate("manual", "probe", "", "", 0, KbPrivate); len(errs) != 0 {
		t.Fatalf("元数据点不该报错: %v", errs)
	}
	// 只给一个 = 打架，必须拒（否则服务端不知道该信谁）。
	if errs := CkptValidate("run", "x", "", "", 1024, KbPrivate); len(errs) == 0 {
		t.Fatal("只给 size 应当报错")
	}
	if errs := CkptValidate("run", "x", "", full, 0, KbPrivate); len(errs) == 0 {
		t.Fatal("只给 digest 应当报错")
	}
	// 摘要格式：不是 sha256:<64 hex> 一律拒（上传时要用它核对字节）。
	for _, bad := range []string{"md5:abc", "sha256:short", strings.Repeat("a", 71)} {
		if errs := CkptValidate("run", "x", "", bad, 8, KbPrivate); len(errs) == 0 {
			t.Fatalf("摘要 %q 应当被拒", bad)
		}
	}
	// 上限：模型权重那种大家伙该走对象存储，而不是"Agent 的检查点"。
	if errs := CkptValidate("run", "x", "", full, CkptMaxBytes+1, KbPrivate); len(errs) == 0 {
		t.Fatal("超过单点上限应当报错")
	}
	if errs := CkptValidate("run", "x", "", full, CkptMaxBytes, KbPrivate); len(errs) != 0 {
		t.Fatalf("正好到上限不该报错: %v", errs)
	}
	// 其它字段。
	if errs := CkptValidate("nope", "x", "", full, 8, KbPrivate); len(errs) == 0 {
		t.Fatal("label 词表外应当报错")
	}
	if errs := CkptValidate("run", "  ", "", full, 8, KbPrivate); len(errs) == 0 {
		t.Fatal("缺 name 应当报错")
	}
	if errs := CkptValidate("run", "x", "alice/agent", full, 8, KbPrivate); len(errs) == 0 {
		t.Fatal("subject_ref 必须是 @命名空间/slug，不能是裸 alice/agent")
	}
	if errs := CkptValidate("run", "x", "@alice/agent", full, 8, "secret"); len(errs) == 0 {
		t.Fatal("visibility 词表外应当报错")
	}
}

/* ---------------- 三样状态的共同词汇 ---------------- */

func TestVocabulariesAreStableAndLabelled(t *testing.T) {
	if strings.Join(StateResources, ",") != "kb,mem,ckpt" {
		t.Fatalf("资源顺序是对外契约，不该变: %v", StateResources)
	}
	for _, r := range StateResources {
		if StateOfKind(r) != r {
			t.Fatalf("%s 应当映射回自己", r)
		}
		if StateResourceLabel(r, "zh") == "" || StateResourceLabel(r, "en") == "" {
			t.Fatalf("%s 缺中/英标签", r)
		}
	}
	if StateOfKind("knowledge") != "" {
		t.Fatal("只有 kb/mem/ckpt 三个短名（别名在能力词表那一层做）")
	}
	// 每个词表成员都要有说明与标签 —— 否则 CLI 的 kinds 会打出空行。
	for _, k := range KbKinds {
		if !ValidKbKind(k) || KbKindLabel(k, "zh") == k && KbKindMeta[k][0] == "" {
			t.Fatalf("kb kind %s 缺标签", k)
		}
	}
	for _, k := range MemKinds {
		if !ValidMemKind(k) || MemKindMeta[k][0] == "" {
			t.Fatalf("mem kind %s 缺标签", k)
		}
	}
	for _, l := range CkptLabels {
		if !ValidCkptLabel(l) || CkptLabelMeta[l][0] == "" {
			t.Fatalf("ckpt label %s 缺标签", l)
		}
	}
	// 未知值不该 panic，原样返回（界面上显示原文比显示空白好）。
	if MemKindLabel("unknown", "zh") != "unknown" || CkptLabelText("unknown", "en") != "unknown" {
		t.Fatal("未知值应当原样返回")
	}
	// 中英标签必须不同（同值说明只有一个语言写了两遍）。
	if KbKindLabel("doc", "zh") == KbKindLabel("doc", "en") {
		t.Fatal("kb kind 的中英标签不该相同")
	}
}
