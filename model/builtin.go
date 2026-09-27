package model

import "strings"

// 内置集合：节点自己用的那几类内容（kb / mem / ckpt / trace）。
//
// 为什么把它们**也写成声明**，而不是散在各处的建表代码里：
// 一台节点上有哪些内容、每类内容长什么样，本来就该有一个地方说得清 ——
// `ncc store ls` 就是那个地方。四类内容就是节点的"内置内容"，
// 声明形状与用户自己声明的集合**完全一样**（同一套字段语法、同一套红线）。
//
// 词表不是另抄一遍：`enum` 直接用 `KbKinds` / `MemKinds` / `CkptLabels` /
// `TraceStatuses` 那些**已经在用的切片**拼出来 —— 否则声明与校验会各自演化，
// 然后两边对不上时没人知道该信哪份。
//
// ⚠️ 内置集合名是**保留**的：用户不能 declare 同名集合（`IsBuiltinKind` 拦），
// 否则谁都能把 `kb` 的语义接管过去，而 `ncc store ls` 就开始说谎。
type BuiltinDecl struct {
	Kind       string
	Title      string
	Summary    string
	Reason     string
	Fields     []string
	Index      []string
	AppendOnly bool
	Visibility string // 一律 private：这四类内容从来不是"公开档"的
}

// enum 声明：`kind:enum:a|b|c`。
func enumField(name string, vals []string) string {
	return name + ":enum:" + strings.Join(vals, "|")
}

// BuiltinCollections 内置集合声明（顺序即展示顺序）。
var BuiltinCollections = []BuiltinDecl{
	{
		Kind: "kb", Title: "知识库", Summary: "托管的语料：按 slug 取，可导出快照包",
		Reason: "语料是 Agent 的长期上下文 —— 它按改名进版本，所以用 slug 当键（而不是 id）",
		Fields: []string{
			"slug:string!",
			"title:string!",
			"format:string",
			enumField("kind", KbKinds),
			"tags:string[]",
			"body:text?search",
		},
		Index: []string{"slug", "kind", "tags"},
	},
	{
		Kind: "mem", Title: "记忆", Summary: "键值 + TTL + 来源：跨运行、跨机器记得住",
		Reason: "记忆是「小结论」：按 (subject, key) 覆盖自己，读时判过期",
		Fields: []string{
			"subject:string!",
			"key:string!",
			enumField("kind", MemKinds),
			"source:string",
			"confidence:int",
			"pinned:bool",
			"value:text?search",
		},
		Index: []string{"subject", "kind"},
	},
	{
		Kind: "ckpt", Title: "检查点", Summary: "不可变快照 + 血缘：交接与回滚的落点",
		Reason: "检查点的价值是「拿回来的是原来那份」—— 所以只追加，字节进 blob、元数据进这里",
		Fields: []string{
			"name:string!",
			enumField("label", CkptLabels),
			"digest:string!",
			"size:int",
			"subject_ref:string",
			"parent:string",
			"note:text?search",
		},
		Index:      []string{"label", "subject_ref", "parent"},
		AppendOnly: true,
	},
	{
		Kind: "trace", Title: "运行轨迹", Summary: "一次运行的真实流水 + 评测：默认私有、显式采集",
		Reason: "轨迹只追加：它记的是「当时发生了什么」，改一个字就不再是证据",
		Fields: []string{
			enumField("kind", TraceKinds),
			enumField("status", TraceStatuses),
			"label:string",
			"model:string",
			"tool:string",
			"digest:string!",
			"steps:int",
			"note:text?search",
		},
		Index:      []string{"kind", "status", "label", "model"},
		AppendOnly: true,
	},
}

// BuiltinKindList 内置集合名（给人看的列表）。
func BuiltinKindList() string {
	out := make([]string, 0, len(BuiltinCollections))
	for _, b := range BuiltinCollections {
		out = append(out, b.Kind)
	}
	return strings.Join(out, " / ")
}

// IsBuiltinKind 这个名字是不是节点自用的内置集合。
func IsBuiltinKind(kind string) bool {
	k := strings.ToLower(strings.TrimSpace(kind))
	for _, b := range BuiltinCollections {
		if b.Kind == k {
			return true
		}
	}
	return false
}

// BuiltinDeclOf 取一个内置声明。
func BuiltinDeclOf(kind string) (BuiltinDecl, bool) {
	k := strings.ToLower(strings.TrimSpace(kind))
	for _, b := range BuiltinCollections {
		if b.Kind == k {
			return b, true
		}
	}
	return BuiltinDecl{}, false
}
