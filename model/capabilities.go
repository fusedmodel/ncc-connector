package model

import (
	"sort"
	"strings"
)

// 节点「提供能力」（hosted_nodes.capabilities）的受控词表。
//
// ⚠️ 别和 `GET /api/meta` 的 capabilities 混起来 —— 那是「这个节点实现了哪些**接口面**」
// （registry / nodes / config / …，用来决定命令面是否放行）；
// 这里说的是「这个节点能对外**提供**什么」（能不能跑 wasm、有没有外网出口、是否托管字节）。
// 两者是不同的轴，见 ncc-platform/prd/ncc-agent-infra.md §10。
//
// 三条规则：
//  1. **别名归一**：历史短名（mcp / api / wasm / llm …）照旧可用，读出来统一成规范 id；
//  2. **未知 id 原样保留**：不在词表里不等于不合法（词表是给检索用的，不是门禁）；
//  3. **检索是「全都要」**：`?can=run:wasm&can=egress:llm` = 两个都得具备。
//
// 本文件与 ncc-platform 的 `internal/model/capabilities.go` 保持同源同形
// （与 KindMeta 一样，两个 Go module 各持一份），改一处要同时改另一处。
type NodeOffer struct {
	ID     string `json:"id"`
	Zh     string `json:"zh"`
	En     string `json:"en"`
	Desc   string `json:"descZh"`
	DescEn string `json:"descEn"`
}

// NodeOffers 受控词表。id 用 `域:名` 形式，域只有四类（run / egress / serve / 裸名），
// 避免再和接口面能力撞名。`run:*` 与 hur-core 的引擎枚举（wasm/js/process/container/remote）
// 一一对应 —— 那边是执行引擎的白名单，这里是节点对外的声明，两者必须能对上。
var NodeOffers = []NodeOffer{
	{
		ID: "run:wasm", Zh: "跑 wasm 沙箱", En: "Run wasm sandbox",
		Desc:   "内置 wasm 引擎：能按限额执行 HUR 包的 wasm 入口（今天 ncc 默认具备）",
		DescEn: "Built-in wasm engine: runs a HUR package's wasm entry under per-package limits (ncc has this by default today)",
	},
	{
		ID: "run:js", Zh: "跑 JS 沙箱", En: "Run JS sandbox",
		Desc:   "JS 引擎：能执行声明了 js 引擎的包",
		DescEn: "A JS engine: can execute packages that declare the js engine",
	},
	{
		ID: "run:process", Zh: "跑本机进程", En: "Run host process",
		Desc:   "按包内声明直接起进程（隔离最弱，只该用在完全信任的包上）",
		DescEn: "Spawns a host process as declared (weakest isolation — only for fully trusted packages)",
	},
	{
		ID: "run:container", Zh: "跑容器", En: "Run container",
		Desc:   "用容器做隔离执行（本机需要有容器运行时）",
		DescEn: "Container-isolated execution (requires a container runtime on the host)",
	},
	{
		ID: "run:remote", Zh: "接受远程执行", En: "Accept remote execution",
		Desc:   "**别人可以把他的包发到这里跑**：网关型节点；配合远程 sandbox 环境登记、证明与限额",
		DescEn: "**Others may send their package here to run**: a gateway-style node — pairs with remote sandbox environment registration, attestation and limits",
	},
	{
		ID: "egress:llm", Zh: "出口可达 LLM API", En: "Egress to LLM APIs",
		Desc:   "本机网络能直接访问模型 API，可代没有出口的节点调用（两侧各自留痕）",
		DescEn: "This host can reach model APIs directly and can call them on behalf of a node without egress (both sides keep their own audit)",
	},
	{
		ID: "egress:internet", Zh: "通用外网出口", En: "General internet egress",
		Desc:   "能替别人的请求出网 —— 比 egress:llm 宽得多，默认应保持关闭",
		DescEn: "Can reach the internet on behalf of others — far broader than egress:llm, keep it off by default",
	},
	{
		ID: "serve:http", Zh: "提供 HTTP/API", En: "Serve HTTP/API",
		Desc:   "对外提供可被调用的端点（API / OpenAPI）",
		DescEn: "Exposes callable endpoints (API / OpenAPI)",
	},
	{
		ID: "serve:mcp", Zh: "提供 MCP server", En: "Serve MCP",
		Desc:   "对外用 MCP 协议提供工具",
		DescEn: "Exposes tools over MCP",
	},
	{
		ID: "artifact", Zh: "托管制品字节", En: "Host artifact bytes",
		Desc:   "本地目录里的制品字节可被按引用取走",
		DescEn: "Artifact bytes in this catalog can be fetched by reference",
	},
	{
		ID: "directory", Zh: "提供目录与寻址", En: "Provide directory",
		Desc:   "提供目录检索与 @命名空间/slug 寻址（hub / registry 节点）",
		DescEn: "Provides catalog search and @namespace/slug addressing (hub / registry nodes)",
	},
	{
		ID: "config", Zh: "托管团队配置", En: "Host team config",
		Desc:   "按环境托管并分发团队配置",
		DescEn: "Hosts and distributes team config per environment",
	},
	{
		ID: "share", Zh: "托管分享页", En: "Host share pages",
		Desc:   "托管点到点分享页的字节",
		DescEn: "Hosts point-to-point share page bytes",
	},
	{
		ID: "kb", Zh: "托管知识库", En: "Host knowledge bases",
		Desc:   "托管可检索的文档语料（`@命名空间/slug`，带版本历史）：Agent 落地前先把库拉下来",
		DescEn: "Hosts retrievable document corpora (with revision history) that an agent pulls before it runs",
	},
	{
		ID: "mem", Zh: "托管 Agent 记忆", En: "Host agent memory",
		Desc:   "托管的键值记忆（可 TTL、可追溯来源）：跨运行/跨机器记得住",
		DescEn: "Hosted key/value memory (TTL, source-traceable) so an agent remembers across runs and machines",
	},
	{
		ID: "ckpt", Zh: "托管检查点", En: "Host checkpoints",
		Desc:   "托管不可变快照（字节进 blob + 血缘）：交接与回滚的落点",
		DescEn: "Hosts immutable snapshots (bytes in blob storage, with lineage) for handoff and rollback",
	},
	{
		ID: "store", Zh: "通用记录仓", En: "Generic record store",
		Desc:   "**声明式集合 + 记录 CRUD**：新增一类内容（issue / log / note / …）只要声明集合，不用改服务端。集合属于命名空间；没声明的字段写不进来，没声明的字段不能当过滤条件，不可变的集合没有 PUT",
		DescEn: "**Declarative collections + record CRUD**: adding a new kind of content (issue / log / note / …) only needs a collection declaration — no server change. Collections belong to a namespace; undeclared fields cannot be written or filtered on, and immutable collections have no PUT",
	},
	{
		ID: "trace", Zh: "收运行轨迹", En: "Host run traces",
		Desc:   "**接受 Agent / HUR 的运行轨迹**：能力评估与后训练数据集的落点。轨迹默认私有，内容级别（摘要 / 预览 / 原文）由采集方在包里、命令里显式声明",
		DescEn: "**Accepts run traces from agents / HUR packages**: the landing spot for capability evaluation and post-training datasets. Traces are private by default; the content level (digest / preview / full) is declared explicitly by the collector",
	},
}

// offerAliases 历史短名 → 规范 id。
// 节点上报时一直可以写 `mcp,api`（文档与示例都这么写），归一后照旧能被 `--can serve:mcp` 搜到。
var offerAliases = map[string]string{
	"mcp":         "serve:mcp",
	"api":         "serve:http",
	"http":        "serve:http",
	"openapi":     "serve:http",
	"wasm":        "run:wasm",
	"js":          "run:js",
	"process":     "run:process",
	"container":   "run:container",
	"remote":      "run:remote",
	"llm":         "egress:llm",
	"internet":    "egress:internet",
	"traces":      "trace",
	"telemetry":   "trace",
	"knowledge":   "kb",
	"kb":          "kb",
	"memory":      "mem",
	"checkpoint":  "ckpt",
	"checkpoints": "ckpt",
}

var offerIndex = func() map[string]bool {
	m := make(map[string]bool, len(NodeOffers))
	for _, o := range NodeOffers {
		m[o.ID] = true
	}
	return m
}()

// OfferAlias 一条别名映射（别名 → 规范 id），给 CLI / 网页展示「哪些老写法还认」。
type OfferAlias struct {
	From string `json:"from"`
	To   string `json:"to"`
}

// OfferAliases 别名表的稳定快照（按 From 排序）。
func OfferAliases() []OfferAlias {
	out := make([]OfferAlias, 0, len(offerAliases))
	for from, to := range offerAliases {
		out = append(out, OfferAlias{From: from, To: to})
	}
	sort.Slice(out, func(i, j int) bool { return out[i].From < out[j].From })
	return out
}

// OfferQuery 一条能力检索条件。
type OfferQuery struct {
	ID           string `json:"id"`
	VerifiedOnly bool   `json:"verifiedOnly"`
}

// ParseOfferQuery 解析一条 `?can=` 取值。
//
// 语法：
//   - `<id>`           → 按「**声明或自证**」匹配（自证比声明强，所以自证也算）
//   - `<id>@verified`  → 只要**自证**具备该能力的节点（本机事实推出来的，不是自己说的）
//
// 只有后缀正好是 `verified` 才算限定符；其余情况整串当 id（自定义 id 里带 @ 也照旧可用）。
func ParseOfferQuery(s string) OfferQuery {
	s = strings.TrimSpace(s)
	if rest, ok := strings.CutSuffix(s, "@verified"); ok {
		return OfferQuery{ID: NormalizeOffer(rest), VerifiedOnly: true}
	}
	return OfferQuery{ID: NormalizeOffer(s)}
}

// NormalizeOffer 把一条声明归一成规范 id。空串返回空串；未知 id 原样返回（只做小写去空格）。
func NormalizeOffer(s string) string {
	s = strings.ToLower(strings.TrimSpace(s))
	if s == "" {
		return ""
	}
	if a, ok := offerAliases[s]; ok {
		return a
	}
	return s
}

// NormalizeOffers 归一 + 去重（保持原顺序）。用于展示与比较。
func NormalizeOffers(in []string) []string {
	out := make([]string, 0, len(in))
	seen := map[string]bool{}
	for _, raw := range in {
		v := NormalizeOffer(raw)
		if v == "" || seen[v] {
			continue
		}
		seen[v] = true
		out = append(out, v)
	}
	return out
}

// KnownOffer 是否在受控词表里（归一后判断）。
func KnownOffer(id string) bool {
	return offerIndex[NormalizeOffer(id)]
}

// OfferAlternatives 返回「规范 id + 所有指向它的历史别名」。
//
// 库里可能还存着老节点上报的 `mcp`，不能因为读的时候归一了，写的时候就搜不到：
// 检索会对每个候选都做一次匹配。顺序不重要，但必须包含规范 id 本身。
func OfferAlternatives(id string) []string {
	id = NormalizeOffer(id)
	if id == "" {
		return nil
	}
	out := []string{id}
	for alias, canonical := range offerAliases {
		if canonical == id {
			out = append(out, alias)
		}
	}
	return out
}
