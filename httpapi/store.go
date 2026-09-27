package httpapi

import (
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/gin-gonic/gin"
	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

// ============================ 通用记录仓（NCC Store） ============================
//
// 一句话：**集合是声明，记录是数据，服务端不认识业务字段。**
//
// 为什么值得有这一层：知识库 / 记忆 / 检查点 / 轨迹是四类内容，机械部分却是同一件事
// （归属命名空间、按 key 取、分页、标签、可见性、版本、上限、过期、软删）——
// 各写一遍就是抄四遍，再加一类内容（issue / log / …）再抄一遍。
// 于是把"声明"与"数据"分开：新增一类内容**不改服务端**，声明一个集合即可。
//
// 三条红线（写在这里，别在实现里破掉）：
//  1. **CRUD ≠ 授权**：能写一个集合不等于能读别人的记录 —— 仍按命名空间归属 + 授权判。
//  2. **动态 ≠ 无模式**：没在 `fields` 声明的字段写不进来（400），没在 `index` 声明的字段不能当过滤条件。
//  3. **不可变就是不可变**：`mutable=false` / `append_only=true` 的集合**没有 PUT**。
//
// 与既有四类的关系：kb / mem / ckpt / trace **继续走各自的接口与语义**（对外不变），
// 它们的机械部分会逐步收敛到这一层。**不一起重写** —— 一次改四类状态的存储是拿数据冒险。

// storeKinds GET /api/store/kinds —— 词表与上限（CLI 取值来源；可离线读）。
func (s *Server) storeKinds(c *gin.Context) {
	ok(c, 200, gin.H{
		"collection": gin.H{
			"nameRule": "小写字母数字与 -_.，字母开头，≤48",
			"note":     "集合属于**命名空间**：同一个 kind 在不同命名空间里是两份声明、两份数据",
		},
		"fieldTypes": []string{"string", "text", "int", "bool", "string[]", "ref", "enum:a|b|c"},
		"fieldFlags": gin.H{"!": "必填", "?search": "该字段进搜索文本"},
		"visibility": model.StoreVisibility,
		"dedupeBy":   model.StoreDedupe,
		"maxBytes":   gin.H{"default": model.StoreMaxBytesDefault, "hard": model.StoreMaxBytesHard, "note": "通用仓只存**内联文本**；大对象走制品或检查点的 blob"},
		"maxFields":  model.StoreMaxFields,
		"maxRecords": model.StoreMaxRecords,
		"status":     []string{model.StoreStatusActive, model.StoreStatusArchived},
		// 五类内容**共用**的口径（一份常量，五处返回同一份；见 model.SharedInvariants）
		"invariants": model.SharedInvariants(),
		// 记录仓自己多几条：声明与索引的约束（其它四类没有"字段声明"这个概念）
		"invariantsExtra": []string{
			"没在 fields 里声明的字段写不进来（放进 meta 可以，但 meta 不可过滤）",
			"没在 index 里声明的字段不能当过滤条件",
			"mutable=false 或 append_only=true 的集合没有 PUT",
			"集合没声明 public 时，里面的记录永远不匿名可见（哪怕记录写了 public）",
		},
	})
}

// colSource 拼出集合的引用（`@命名空间/kind`）。
func colRefOf(nsSlug, kind string) string {
	s := strings.TrimPrefix(nsSlug, "@")
	return "@" + s + "/" + kind
}

func colJSON(s *Server, c *model.Collection, ns *model.Namespace, records int64) gin.H {
	nsSlug := ""
	if ns != nil {
		nsSlug = ns.Slug
	}
	out := gin.H{
		"id": c.ID, "ref": colRefOf(nsSlug, c.Kind), "kind": c.Kind,
		"namespace": gin.H{"id": c.NamespaceID, "slug": nsSlug},
		"title":     c.Title, "summary": c.Summary, "reason": c.Reason, // builtin：这几类是节点自己在用的内置内容（名字保留）
		"builtin": model.IsBuiltinKind(c.Kind), "mutable": c.Mutable, "history": c.History, "appendOnly": c.AppendOnly,
		"visibility": c.Visibility, "maxBytes": c.MaxBytes,
		"fields": model.DecodeFields(c.Fields), "index": model.DecodeStringList(c.Index),
		"dedupeBy": c.DedupeBy, "defaultTtlDays": c.DefaultTTL,
		"status": c.Status, "records": records,
		"createdBy": c.CreatedBy, "createdAt": c.CreatedAt, "updatedAt": c.UpdatedAt,
	}
	return out
}

func recJSON(r *model.Record, withBody bool) gin.H {
	out := gin.H{
		"id": r.ID, "key": r.Key, "revision": r.Revision,
		"checksum": r.Checksum, "size": r.Size,
		"tags":       model.DecodeStringList(r.Tags),
		"fields":     decodeMap(r.Data),
		"meta":       decodeMap(r.Meta),
		"visibility": r.Visibility, "status": r.Status, "source": r.Source,
		"lastNote":  r.LastNote,
		"expiresAt": r.ExpiresAt, "createdBy": r.CreatedBy, "updatedBy": r.UpdatedBy,
		"createdAt": r.CreatedAt, "updatedAt": r.UpdatedAt,
	}
	if withBody {
		out["body"] = r.Body
	}
	return out
}

func decodeMap(s string) map[string]any {
	if strings.TrimSpace(s) == "" {
		return map[string]any{}
	}
	var m map[string]any
	if err := json.Unmarshal([]byte(s), &m); err != nil {
		return map[string]any{}
	}
	return m
}

// readScopeFor 一个身份能读到哪些命名空间的记录（管理员 = 全部）。
func (s *Server) readScopeFor(c *gin.Context) (ids []string, all, anonymous bool) {
	a := authOf(c)
	if a == nil {
		return nil, false, true
	}
	if s.ensureAdmin(c) {
		return nil, true, false
	}
	if nss, err := s.St.NamespacesOfUser(a.UserID); err == nil {
		for i := range nss {
			ids = append(ids, nss[i].ID)
		}
	}
	if owners, err := s.St.GrantedOwners(a.UserID, model.GrantState); err == nil && len(owners) > 0 {
		// 被授权者：把"这些人的命名空间"也算进可读范围（与 kb 的 `?all=` 同一取舍）
		if granted, err := s.St.NamespaceIDsOfOwners(owners); err == nil {
			ids = append(ids, granted...)
		}
	}
	return ids, false, false
}

// resolveCollection 把 `:col`（+ `?namespace=`）解析成一个集合。
//
// ⚠️ 集合属于命名空间，所以**匿名或跨空间读必须显式给 namespace** ——
// 不给就只在"我自己能读的范围"里找，找不到就说清为什么（而不是猜一个）。
func (s *Server) resolveCollection(c *gin.Context) (*model.Collection, *model.Namespace, error) {
	kind := strings.ToLower(strings.TrimSpace(c.Param("col")))
	if !model.ValidCollectionKind(kind) {
		return nil, nil, errWith(400, "bad_collection", "集合名不合法（小写字母数字与 -_.，字母开头）")
	}
	want := strings.TrimSpace(c.Query("namespace"))
	a := authOf(c)

	if want != "" {
		want = strings.TrimPrefix(want, "@")
		ns, err := s.St.FindNamespaceBySlug(want)
		if err != nil {
			return nil, nil, errWith(404, "no_namespace", "没有命名空间 @"+want)
		}
		col, err := s.St.GetCollection(ns.ID, kind)
		if err != nil {
			return nil, nil, errWith(404, "no_collection", fmt.Sprintf("命名空间 @%s 还没声明集合「%s」（声明：POST /api/store，或 ncc store declare）", want, kind))
		}
		return col, ns, nil
	}

	if a == nil {
		return nil, nil, errWith(400, "need_namespace", "匿名读要指明命名空间：?namespace=@某人")
	}
	// 不给 namespace：在我自己的命名空间里找（个人空间优先）
	nss, err := s.St.NamespacesOfUser(a.UserID)
	if err != nil {
		return nil, nil, errWith(500, "internal", "服务内部错误")
	}
	ordered := make([]model.Namespace, 0, len(nss))
	for _, n := range nss {
		if n.Type == "account" {
			ordered = append(ordered, n)
		}
	}
	for _, n := range nss {
		if n.Type != "account" {
			ordered = append(ordered, n)
		}
	}
	for i := range ordered {
		if col, err := s.St.GetCollection(ordered[i].ID, kind); err == nil {
			return col, &ordered[i], nil
		}
	}
	return nil, nil, errWith(404, "no_collection", fmt.Sprintf("你的命名空间里没有集合「%s」（声明：POST /api/store）", kind))
}

/* ---------------- 集合：列 / 声明 / 改 / 归档 ---------------- */

// listStoreCollections GET /api/store?namespace=
func (s *Server) listStoreCollections(c *gin.Context) {
	ids, all, _ := s.readScopeFor(c)
	want := strings.TrimSpace(c.Query("namespace"))

	var rows []model.Collection
	q := s.St.DB.Model(&model.Collection{}).Where("status <> ?", model.StoreStatusArchived)
	if want != "" {
		ns, err := s.St.FindNamespaceBySlug(strings.TrimPrefix(want, "@"))
		if err != nil {
			fail(c, 404, "no_namespace", "没有命名空间 @"+strings.TrimPrefix(want, "@"))
			return
		}
		if !all && !containsStr(ids, ns.ID) {
			fail(c, 403, "forbidden", "你不在 @"+strings.TrimPrefix(want, "@")+" 里，看不到它的集合")
			return
		}
		q = q.Where("namespace_id = ?", ns.ID)
	} else if !all {
		if len(ids) == 0 {
			// 匿名/无归属：只列**公开集合**（还要靠记录级判定，这里给出集合面）
			q = q.Where("visibility = ?", "public")
		} else {
			q = q.Where("namespace_id IN ? OR visibility = ?", ids, "public")
		}
	}
	if err := q.Order("kind asc").Find(&rows).Error; err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		ns, _ := s.St.FindNamespaceByID(rows[i].NamespaceID)
		n, _ := s.St.CountRecords(rows[i].ID)
		out = append(out, colJSON(s, &rows[i], ns, n))
	}
	// 内置集合的**目录**（不管这台节点上有没有用过）：
	// `ncc store ls` 要能回答"这台节点上有什么内容"，而四类内置内容
	// 在没人用过之前是"还没有行"的 —— 那就成了一张骗人的清单。
	// 所以目录与实情分开给：`builtins` 是节点声明支持的几类，`collections` 是实际有的。
	pres := map[string]bool{}
	for i := range rows {
		pres[rows[i].Kind] = true
	}
	builtins := make([]gin.H, 0, len(model.BuiltinCollections))
	for _, b := range model.BuiltinCollections {
		builtins = append(builtins, gin.H{
			"kind": b.Kind, "title": b.Title, "summary": b.Summary, "reason": b.Reason,
			"fields": b.Fields, "index": b.Index,
			"shape": map[bool]string{true: "append-only", false: "mutable"}[b.AppendOnly],
			// 用过没有：没有就是"取用即声明"（第一次写它才出现）
			"present": pres[b.Kind],
		})
	}
	ok(c, 200, gin.H{"collections": out, "count": len(out), "builtins": builtins})
}

// declareStoreCollection POST /api/store —— 声明（或更新）一个集合。
//
// 谁可以声明：**该命名空间的成员**（与写 kb 同一权限）。集合声明是"这个空间里有一类这样的内容"，
// 不是节点级治理动作 —— 模式随命名空间走，两个团队各声明各的 `issue`，互不干扰。
func (s *Server) declareStoreCollection(c *gin.Context) {
	a := authOf(c)
	var body struct {
		Namespace  string   `json:"namespace"`
		Kind       string   `json:"kind"`
		Title      string   `json:"title"`
		Summary    string   `json:"summary"`
		Reason     string   `json:"reason"`
		Mutable    *bool    `json:"mutable"`
		History    bool     `json:"history"`
		AppendOnly bool     `json:"append_only"`
		Visibility string   `json:"visibility"`
		MaxBytes   int64    `json:"max_bytes"`
		Fields     []string `json:"fields"`
		Index      []string `json:"index"`
		DedupeBy   string   `json:"dedupe_by"`
		DefaultTTL int64    `json:"default_ttl_days"`
	}
	if err := c.ShouldBindJSON(&body); err != nil {
		fail(c, 400, "bad_request", "请求体不是合法 JSON")
		return
	}
	kind := strings.ToLower(strings.TrimSpace(body.Kind))
	if !model.ValidCollectionKind(kind) {
		fail(c, 400, "bad_collection", "集合名不合法（小写字母数字与 -_.，字母开头，≤48）")
		return
	}
	// 内置集合名是**保留**的：kb / mem / ckpt / trace 是节点自带的那几类内容。
	// 谁都能 declare 同名集合的话，`ncc store ls` 就开始说谎（同一个名字两种内容）。
	if model.IsBuiltinKind(kind) {
		fail(c, 400, "builtin_kind", fmt.Sprintf(
			"「%s」是节点的**内置集合名**（%s）—— 这几类内容由节点自己在用，名字保留（另起一个名字，比如 %s-mine）",
			kind, model.BuiltinKindList(), kind))
		return
	}
	ns, err := s.stateNamespace(a, body.Namespace)
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(ns.ID, a.UserID) {
		fail(c, 403, "forbidden", "只有命名空间的成员能声明集合")
		return
	}
	specs, err := model.ParseFields(body.Fields)
	if err != nil {
		fail(c, 400, "bad_field", err.Error())
		return
	}
	idx := make([]string, 0, len(body.Index))
	declared := map[string]model.FieldSpec{}
	for _, f := range specs {
		declared[f.Name] = f
	}
	for _, x := range body.Index {
		x = strings.TrimSpace(x)
		if _, ok := declared[x]; !ok {
			fail(c, 400, "bad_index", fmt.Sprintf("索引字段「%s」没在 fields 里声明 —— 没声明的字段不能当过滤条件", x))
			return
		}
		idx = append(idx, x)
	}
	vis := strings.TrimSpace(body.Visibility)
	if vis == "" {
		vis = "private"
	}
	if !model.ValidStoreVisibility(vis) {
		fail(c, 400, "bad_visibility", "visibility 只能是 public 或 private")
		return
	}
	maxBytes := body.MaxBytes
	if maxBytes <= 0 {
		maxBytes = model.StoreMaxBytesDefault
	}
	if maxBytes > model.StoreMaxBytesHard {
		fail(c, 400, "too_large", fmt.Sprintf("max_bytes 上限 %d（再大就走制品或检查点的 blob）", model.StoreMaxBytesHard))
		return
	}
	mutable := true
	if body.Mutable != nil {
		mutable = *body.Mutable
	}
	if body.AppendOnly {
		mutable = false // 只追加 = 不可改
	}
	if body.DedupeBy != "" && body.DedupeBy != "checksum" {
		fail(c, 400, "bad_dedupe", "dedupe_by 只支持 checksum（空 = 不去重）")
		return
	}
	if body.DefaultTTL < 0 || body.DefaultTTL > 3650 {
		fail(c, 400, "bad_ttl", "default_ttl_days 要在 0..3650（0 = 不过期）")
		return
	}

	col := &model.Collection{
		ID: store.NewID("C-"), NamespaceID: ns.ID, Kind: kind,
		Title: strings.TrimSpace(body.Title), Summary: strings.TrimSpace(body.Summary),
		Reason:  strings.TrimSpace(body.Reason),
		Mutable: mutable, History: body.History || mutable, AppendOnly: body.AppendOnly,
		Visibility: vis, MaxBytes: maxBytes,
		Fields: store.JSONList(body.Fields), Index: store.JSONList(idx),
		DedupeBy: body.DedupeBy, DefaultTTL: body.DefaultTTL,
		Status: model.StoreStatusActive, CreatedBy: a.UserID, CreatedAt: time.Now(), UpdatedAt: time.Now(),
	}
	if col.Title == "" {
		col.Title = kind
	}
	saved, created, err := s.St.UpsertCollection(col)
	if err != nil {
		failErr(c, err)
		return
	}
	n, _ := s.St.CountRecords(saved.ID)
	status := 200
	if created {
		status = 201
	}
	ok(c, status, gin.H{"collection": colJSON(s, saved, ns, n), "created": created})
}

// archiveStoreCollection DELETE /api/store/:col —— 归档集合（**不删记录**）。
func (s *Server) archiveStoreCollection(c *gin.Context) {
	a := authOf(c)
	col, ns, err := s.resolveCollection(c)
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(ns.ID, a.UserID) {
		fail(c, 403, "forbidden", "只有命名空间的成员能归档集合")
		return
	}
	if err := s.St.SetCollectionStatus(col.ID, model.StoreStatusArchived); err != nil {
		failErr(c, err)
		return
	}
	n, _ := s.St.CountRecords(col.ID)
	ok(c, 200, gin.H{"archived": true, "collection": colRefOf(ns.Slug, col.Kind), "records": n,
		"note": "归档只是不再列出；记录还在（要真删就逐条 DELETE）"})
}

/* ---------------- 记录：列 / 写 / 读 / 改 / 删 / 历史 ---------------- */

// listStoreRecords GET /api/store/:col?q=&tag=&prefix=&status=&page=&size=&<字段>=v
func (s *Server) listStoreRecords(c *gin.Context) {
	col, ns, err := s.resolveCollection(c)
	if err != nil {
		failErr(c, err)
		return
	}
	ids, all, _ := s.readScopeFor(c)
	q := strings.TrimSpace(c.Query("q"))
	page, _ := strconv.Atoi(c.DefaultQuery("page", "1"))
	size, _ := strconv.Atoi(c.DefaultQuery("size", "50"))

	// 字段过滤：用 `f.<字段>=值` 前缀。
	//
	// 为什么不用裸名（`?status=open`）：**保留参数会跟同名字段撞车** —— 一个集合
	// 声明了 `status` 字段，而 `?status=` 又是记录状态（active/archived），两个意思一个 key。
	// 加了前缀就永远不会歧义，CLI 那边仍可以写成好看的 `--where status=open`。
	declared := map[string]model.FieldSpec{}
	for _, f := range model.DecodeFields(col.Fields) {
		declared[f.Name] = f
	}
	indexed := map[string]string{}
	reserved := map[string]bool{"q": true, "tag": true, "prefix": true, "state": true, "page": true, "size": true, "namespace": true, "archived": true, "expired": true, "all": true}
	var bad []string
	var unindexed []string
	for k := range c.Request.URL.Query() {
		if reserved[k] {
			continue
		}
		if !strings.HasPrefix(k, "f.") {
			bad = append(bad, k)
			continue
		}
		name := strings.TrimPrefix(k, "f.")
		if _, ok := declared[name]; !ok {
			bad = append(bad, name)
			continue
		}
		// 声明了字段不等于**能按它过滤**：只有 `index` 里列过的才有索引行。
		// 这里必须报错，不能默默返回空 —— "查不到"与"这个字段根本不能查"
		// 是两件事，混在一起会让人以为"真的没有这样的记录"。
		if !slices.Contains(model.DecodeStringList(col.Index), name) {
			unindexed = append(unindexed, name)
			continue
		}
		indexed[name] = c.Query(k)
	}
	if len(unindexed) > 0 {
		sort.Strings(unindexed)
		fail(c, 400, "bad_filter", fmt.Sprintf("「%s」是声明过的字段，但没在 index 里 —— 没声明的过滤条件就是一次全表扫，所以这里不接（能过滤的：%v）",
			strings.Join(unindexed, ", "), model.DecodeStringList(col.Index)))
		return
	}
	if len(bad) > 0 {
		sort.Strings(bad)
		fail(c, 400, "bad_filter", fmt.Sprintf("「%s」不是这个集合声明的字段（字段过滤要写成 `f.字段=值`；能过滤的：%v）",
			strings.Join(bad, ", "), model.DecodeStringList(col.Index)))
		return
	}

	rows, total, err := s.St.ListRecords(store.RecordListOpts{
		CollectionID: col.ID, NamespaceIDs: ids, Public: !all && len(ids) == 0,
		Q: q, Prefix: c.Query("prefix"), Tags: c.QueryArray("tag"),
		Status: c.Query("state"), All: all,
		IncludeArchived: c.Query("archived") == "1", IncludeExpired: c.Query("expired") == "1",
		Indexed: indexed, Page: page, Size: size, Now: time.Now(),
	})
	if err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		out = append(out, recJSON(&rows[i], false))
	}
	ok(c, 200, gin.H{
		"collection": colRefOf(ns.Slug, col.Kind), "records": out, "total": total,
		"page": page, "size": size,
		"note": "`?q=` 是**关键词匹配**（空格分隔的几个词都要出现，命中位置决定排序：key 3 / 标签与 ?search 字段 2 / 正文 1）—— 不是索引检索、也不是向量检索；要精确就用 `f.字段=值`。排序在最多 " + strconv.Itoa(model.StoreSearchCandidates) + " 条候选上做（超出按更新时间截断）。",
	})
}

// upsertStoreRecord POST /api/store/:col —— 建一条（同 key 已存在时按集合规则处理）。
func (s *Server) upsertStoreRecord(c *gin.Context) {
	s.writeStoreRecord(c, false)
}

// putStoreRecord PUT /api/store/:col/:key —— 改一条（不可变集合会拒）。
func (s *Server) putStoreRecord(c *gin.Context) {
	s.writeStoreRecord(c, true)
}

func (s *Server) writeStoreRecord(c *gin.Context, isPut bool) {
	a := authOf(c)
	col, ns, err := s.resolveCollection(c)
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(ns.ID, a.UserID) {
		fail(c, 403, "forbidden", "只有命名空间的成员能写这个集合")
		return
	}
	if isPut && (!col.Mutable || col.AppendOnly) {
		fail(c, 400, "immutable", fmt.Sprintf("集合「%s」是%s，没有改这条路径（要留新内容请用新的 key）",
			col.Kind, map[bool]string{true: "只追加", false: "不可变"}[col.AppendOnly]))
		return
	}

	var body struct {
		Key        string          `json:"key"`
		Body       string          `json:"body"`
		Fields     json.RawMessage `json:"fields"`
		Meta       json.RawMessage `json:"meta"`
		Tags       []string        `json:"tags"`
		Visibility string          `json:"visibility"`
		Status     string          `json:"status"`
		Source     string          `json:"source"`
		ExpiresAt  *time.Time      `json:"expires_at"`
		TTLDays    int64           `json:"ttl_days"`
		Revision   int64           `json:"revision"`
		Note       string          `json:"note"`
	}
	if err := c.ShouldBindJSON(&body); err != nil {
		fail(c, 400, "bad_request", "请求体不是合法 JSON")
		return
	}
	key := strings.ToLower(strings.TrimSpace(body.Key))
	if isPut {
		key = strings.ToLower(strings.TrimSpace(c.Param("key")))
	}
	if !model.ValidRecordKey(key) {
		fail(c, 400, "bad_key", "key 不合法（小写字母数字与 -_.，字母开头，≤96）")
		return
	}
	if len(body.Body) > int(col.MaxBytes) {
		fail(c, 400, "too_large", fmt.Sprintf("正文 %d 字节，超过这个集合的上限 %d", len(body.Body), col.MaxBytes))
		return
	}

	// ① 声明的字段：**没声明就写不进来**（要么加进声明，要么放进 meta）
	specs := model.DecodeFields(col.Fields)
	vals := map[string]any{}
	if len(body.Fields) > 0 {
		if err := json.Unmarshal(body.Fields, &vals); err != nil {
			fail(c, 400, "bad_field", "fields 必须是对象（{字段名: 值}）")
			return
		}
	}
	declared := map[string]bool{}
	search := make([]string, 0, 4)
	indexVals := map[string][]string{}
	for _, f := range specs {
		declared[f.Name] = true
		v, present := vals[f.Name]
		if !present || v == nil {
			if f.Require {
				fail(c, 400, "missing_field", fmt.Sprintf("字段「%s」是必填的", f.Name))
				return
			}
			continue
		}
		strs, err := validateFieldValue(f, v)
		if err != nil {
			fail(c, 400, "bad_field", err.Error())
			return
		}
		indexVals[f.Name] = strs
		if f.Search {
			search = append(search, strs...)
			if s, ok := v.(string); ok {
				search = append(search, s)
			}
		}
	}
	for k := range vals {
		if !declared[k] {
			fail(c, 400, "bad_field", fmt.Sprintf("字段「%s」没在集合声明里 —— 要么把它加进 fields，要么放进 meta（meta 不过滤）", k))
			return
		}
	}

	// ② 可见性：集合没声明 public 时，记录级 public 不作数（红线 3）
	vis := strings.TrimSpace(body.Visibility)
	if vis == "" {
		vis = "private"
	}
	if vis == "public" && col.Visibility != "public" {
		fail(c, 400, "bad_visibility", fmt.Sprintf("集合「%s」没声明 public，里面的记录不能公开（要公开就先改集合声明）", col.Kind))
		return
	}
	status := strings.TrimSpace(body.Status)
	if status == "" {
		status = model.StoreStatusActive
	}
	if status != model.StoreStatusActive && status != model.StoreStatusArchived {
		fail(c, 400, "bad_status", "status 只能是 active 或 archived")
		return
	}

	// ③ TTL
	exp := body.ExpiresAt
	if exp == nil && body.TTLDays > 0 {
		t := time.Now().AddDate(0, 0, int(body.TTLDays))
		exp = &t
	}
	if exp == nil && col.DefaultTTL > 0 {
		t := time.Now().AddDate(0, 0, int(col.DefaultTTL))
		exp = &t
	}

	dataJSON := "{}"
	if len(vals) > 0 {
		b, _ := json.Marshal(vals)
		dataJSON = string(b)
	}
	metaJSON := "{}"
	if len(body.Meta) > 0 {
		if !json.Valid(body.Meta) {
			fail(c, 400, "bad_meta", "meta 不是合法 JSON")
			return
		}
		metaJSON = string(body.Meta)
	}

	rec, created, duplicate, err := s.St.UpsertRecord(store.RecordInput{
		CollectionID: col.ID, NamespaceID: ns.ID, Key: key,
		Body: body.Body, Data: dataJSON, Meta: metaJSON, Tags: store.JSONList(body.Tags),
		Visibility: vis, Status: status, Source: body.Source,
		SearchText: strings.Join(append(search, body.Tags...), " "),
		ExpiresAt:  exp, UserID: a.UserID, ExpectRevision: body.Revision, Note: body.Note,
		// 不变量在数据层再判一次：上面那处是给"用错动词"的人一句**具体**的话
		// （只说一个或只追加），这里是兜底 —— 绕过路由也绕不过它。
		Immutable: !col.Mutable || col.AppendOnly,
	})
	if err != nil {
		switch {
		case errors.Is(err, store.ErrImmutable):
			fail(c, 400, "immutable", fmt.Sprintf("集合「%s」是%s，改不了（要留新内容请用新的 key）",
				col.Kind, map[bool]string{true: "只追加", false: "不可变"}[col.AppendOnly]))
		case errors.Is(err, store.ErrConflict):
			fail(c, 409, "conflict", "别人先改了（revision 对不上）—— 取最新一份再改，别覆盖别人的修改")
		case errors.Is(err, store.ErrTooMany):
			fail(c, 400, "too_many", fmt.Sprintf("这个集合记录数到顶了（%d）—— 一类内容攒到这个量，说明它该分集合或该走制品", model.StoreMaxRecords))
		default:
			failErr(c, err)
		}
		return
	}
	// ④ 索引：**只给声明过的可过滤字段建行**
	if err := s.St.SetIndex(rec.ID, col.ID, model.DecodeStringList(col.Index), indexVals); err != nil {
		failErr(c, err)
		return
	}
	st := 200
	if created {
		st = 201
	}
	ok(c, st, gin.H{"record": recJSON(rec, true), "created": created, "duplicate": duplicate,
		"collection": colRefOf(ns.Slug, col.Kind)})
}

// getStoreRecord GET /api/store/:col/:key?revision=N
func (s *Server) getStoreRecord(c *gin.Context) {
	col, ns, err := s.resolveCollection(c)
	if err != nil {
		failErr(c, err)
		return
	}
	key := strings.ToLower(strings.TrimSpace(c.Param("key")))
	rec, err := s.St.GetRecordByKey(col.ID, key)
	if err != nil {
		fail(c, 404, "not_found", fmt.Sprintf("集合 %s 里没有「%s」", colRefOf(ns.Slug, col.Kind), key))
		return
	}
	// 可见性：公开记录匿名可读；其余要可读凭据
	if !s.recordReadable(c, col, rec) {
		fail(c, 403, "forbidden", "这条记录不是公开的（要读请带上能读这个命名空间的凭据）")
		return
	}
	if rev := strings.TrimSpace(c.Query("revision")); rev != "" {
		_ = rev // 历史版本**只记元数据**，取不到正文（见 model.RecordRevision 的说明）
	}
	out := recJSON(rec, true)
	out["expired"] = rec.Expired(time.Now())
	ok(c, 200, gin.H{"record": out, "collection": colRefOf(ns.Slug, col.Kind)})
}

// recordReadable 一条记录能不能被这个身份读。
func (s *Server) recordReadable(c *gin.Context, col *model.Collection, rec *model.Record) bool {
	if rec.Visibility == "public" && col.Visibility == "public" {
		return true
	}
	return rec.NamespaceID == col.NamespaceID && s.stateReadable(c, rec.NamespaceID, col.CreatedBy)
}

// deleteStoreRecord DELETE /api/store/:col/:key —— **归档**（软删；要真删用 ?hard=1）。
func (s *Server) deleteStoreRecord(c *gin.Context) {
	a := authOf(c)
	col, ns, err := s.resolveCollection(c)
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(ns.ID, a.UserID) {
		fail(c, 403, "forbidden", "只有命名空间的成员能删这个集合里的记录")
		return
	}
	key := strings.ToLower(strings.TrimSpace(c.Param("key")))
	rec, err := s.St.GetRecordByKey(col.ID, key)
	if err != nil {
		fail(c, 404, "not_found", "没有这条记录")
		return
	}
	if c.Query("hard") == "1" {
		if err := s.St.DeleteRecord(rec.ID); err != nil {
			failErr(c, err)
			return
		}
		ok(c, 200, gin.H{"deleted": true, "hard": true, "key": key})
		return
	}
	if err := s.St.SetRecordStatus(rec.ID, model.StoreStatusArchived, a.UserID); err != nil {
		failErr(c, err)
		return
	}
	ok(c, 200, gin.H{"archived": true, "hard": false, "key": key,
		"note": "归档只是不再列出（?archived=1 还能看）；真删要 ?hard=1"})
}

// storeRecordHistory GET /api/store/:col/:key/history
func (s *Server) storeRecordHistory(c *gin.Context) {
	col, ns, err := s.resolveCollection(c)
	if err != nil {
		failErr(c, err)
		return
	}
	key := strings.ToLower(strings.TrimSpace(c.Param("key")))
	rec, err := s.St.GetRecordByKey(col.ID, key)
	if err != nil {
		fail(c, 404, "not_found", "没有这条记录")
		return
	}
	rows, err := s.St.RecordRevisions(rec.ID, 50)
	if err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		out = append(out, gin.H{
			"revision": rows[i].Revision, "checksum": rows[i].Checksum, "size": rows[i].Size,
			"changedBy": rows[i].ChangedBy, "note": rows[i].Note, "at": rows[i].CreatedAt,
		})
	}
	ok(c, 200, gin.H{"collection": colRefOf(ns.Slug, col.Kind), "key": key,
		"currentRevision": rec.Revision, "currentNote": rec.LastNote, "history": out,
		"note": "历史只记元数据（谁在什么时候写成了哪个摘要），不存正文副本；`note` 是**写进那个版本时**给的备注"})
}

// gcStoreRecords POST /api/store/gc —— 真删过期记录。
func (s *Server) gcStoreRecords(c *gin.Context) {
	a := authOf(c)
	ns, err := s.stateNamespace(a, c.Query("namespace"))
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(ns.ID, a.UserID) {
		fail(c, 403, "forbidden", "只有命名空间的成员能清理它的记录")
		return
	}
	kind := strings.ToLower(strings.TrimSpace(c.Query("collection")))
	colID := ""
	if kind != "" {
		col, err := s.St.GetCollection(ns.ID, kind)
		if err != nil {
			fail(c, 404, "no_collection", "这个命名空间里没有集合「"+kind+"」")
			return
		}
		colID = col.ID
	}
	n, err := s.St.GcRecords(colID, time.Now())
	if err != nil {
		failErr(c, err)
		return
	}
	ok(c, 200, gin.H{"removed": n, "namespace": ns.Slug, "collection": kind})
}

func containsStr(list []string, v string) bool {
	for _, x := range list {
		if x == v {
			return true
		}
	}
	return false
}

// validateFieldValue 按声明校验一个字段值，返回它的**字符串形态**（索引用）。
func validateFieldValue(f model.FieldSpec, v any) ([]string, error) {
	str := func() (string, error) {
		s, ok := v.(string)
		if !ok {
			return "", fmt.Errorf("字段「%s」要字符串", f.Name)
		}
		return s, nil
	}
	switch f.Type {
	case "string", "text", "ref":
		s, err := str()
		if err != nil {
			return nil, err
		}
		// `ref`：引用。**不强行要求 `@ns/slug` 两段** —— "指派给 @alice 这个人" 也是引用，
		// 要求写两段会逼出一堆没意义的填法。只要求它是 `@` 开头的非空引用，
		// 并把 `@ns/` 这种半个引用挡下来。
		if f.Type == "ref" && s != "" {
			if !strings.HasPrefix(s, "@") {
				return nil, fmt.Errorf("字段「%s」要写成引用（@某人 或 @命名空间/slug），收到 %q", f.Name, s)
			}
			if rest, ok := strings.CutPrefix(s, "@"); ok && (rest == "" || strings.HasSuffix(rest, "/") || strings.Contains(rest, "//")) {
				return nil, fmt.Errorf("字段「%s」的引用不完整（收到 %q）", f.Name, s)
			}
		}
		return []string{s}, nil
	case "enum":
		s, err := str()
		if err != nil {
			return nil, err
		}
		for _, x := range f.Enum {
			if x == s {
				return []string{s}, nil
			}
		}
		return nil, fmt.Errorf("字段「%s」只能是 %s（收到 %q）", f.Name, strings.Join(f.Enum, " / "), s)
	case "int":
		switch n := v.(type) {
		case float64:
			return []string{strconv.FormatInt(int64(n), 10)}, nil
		default:
			return nil, fmt.Errorf("字段「%s」要整数", f.Name)
		}
	case "bool":
		b, ok := v.(bool)
		if !ok {
			return nil, fmt.Errorf("字段「%s」要布尔", f.Name)
		}
		return []string{strconv.FormatBool(b)}, nil
	case "string[]":
		arr, ok := v.([]any)
		if !ok {
			return nil, fmt.Errorf("字段「%s」要字符串数组", f.Name)
		}
		out := make([]string, 0, len(arr))
		for _, x := range arr {
			s, ok := x.(string)
			if !ok {
				return nil, fmt.Errorf("字段「%s」要字符串数组（里面有非字符串）", f.Name)
			}
			out = append(out, s)
		}
		return out, nil
	}
	return nil, fmt.Errorf("字段「%s」的类型「%s」不支持", f.Name, f.Type)
}

var _ = gorm.ErrRecordNotFound
