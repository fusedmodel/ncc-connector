package httpapi

import (
	"crypto/hmac"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"io"
	"strconv"
	"strings"
	"time"

	"github.com/gin-gonic/gin"
	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

/* ---------------- NCC State：知识库 / 记忆 / 检查点 ----------------
   三样状态的共同规矩（与制品/配置/轨迹保持一致，别在某一处放宽）：
     · **默认私有**：kb 默认 private，mem 没有公开档（记忆是私人/团队状态），
       ckpt 默认 private。跨人读要显式 grant（种类 `state`）。
     · **写只给命名空间成员**：被授权者只有读，永远不给写。
     · **fail-closed**：没有可见范围就什么都查不到（store 侧 `WHERE 1 = 0`）。
   各自的形状不同，所以是三个资源而不是一张表：
     kb    内容进库、可检索、改一次留一版
     mem   (命名空间, subject, key) 唯一，写即更新，读时判过期
     ckpt  字节进 blob、元数据进库、不可变、有血缘
*/

// StateScopeOf 把一个请求身份折成三样状态共用的可见范围。
//
// 与轨迹同一套：默认**只看我的 + 被授权给我的**；管理员要 `all=1` 才看全节点。
func (s *Server) stateScopeOf(c *gin.Context) store.StateScope {
	sc := store.StateScope{}
	a := authOf(c)
	if a == nil {
		return sc // fail-closed
	}
	if c.Query("all") == "1" && s.ensureAdmin(c) {
		sc.All = true
		return sc
	}
	if nss, err := s.St.NamespacesOfUser(a.UserID); err == nil {
		for i := range nss {
			sc.NamespaceIDs = append(sc.NamespaceIDs, nss[i].ID)
		}
	}
	if c.Query("mine") != "1" {
		if owners, err := s.St.GrantedOwners(a.UserID, model.GrantState); err == nil {
			sc.GrantedOwners = owners
		}
	}
	return sc
}

// stateNamespace 解析目标命名空间（默认：调用者的个人空间）。三样状态共用。
func (s *Server) stateNamespace(a *AuthInfo, want string) (*model.Namespace, error) {
	want = strings.TrimSpace(strings.TrimPrefix(want, "@"))
	nss, err := s.St.NamespacesOfUser(a.UserID)
	if err != nil {
		return nil, errWith(500, "internal", "服务内部错误")
	}
	if want == "" {
		for i := range nss {
			if nss[i].Type == "account" {
				return &nss[i], nil
			}
		}
		if len(nss) > 0 {
			return &nss[0], nil
		}
		return nil, errWith(400, "no_namespace", "你还没有命名空间（先注册或建一个组织空间）")
	}
	for i := range nss {
		if nss[i].Slug == want {
			return &nss[i], nil
		}
	}
	return nil, errWith(403, "forbidden", "你不是命名空间 @"+want+" 的成员，不能把状态写进去")
}

// markBuiltinUsed 把这个命名空间里的**内置内容**标成「用过了」（取用即声明）。
//
// 四类内置内容（kb / mem / ckpt / trace）的声明是代码里的常量
// （`model.BuiltinCollections`），但「这台节点上用过它」这件事得落在数据上 ——
// 否则 `ncc store ls` 只看得见用户自己声明的集合，
// 而它本该回答的「这台节点上有什么内容」就答不全。
//
// 失败**不影响主流程**：声明行只是清单与字段形状，写不进去不该让一次记忆写入失败。
func (s *Server) markBuiltinUsed(nsID, kind string) {
	if nsID == "" {
		return
	}
	_, _ = s.St.EnsureBuiltinCollection(nsID, kind)
}

// stateReadable 这个身份能不能读这个命名空间的状态（公开项另行判断）。
func (s *Server) stateReadable(c *gin.Context, nsID, ownerID string) bool {
	a := authOf(c)
	if a == nil {
		return false
	}
	if s.ensureAdmin(c) || s.canManage(nsID, a.UserID) {
		return true
	}
	return s.St.HasGrant(ownerID, a.UserID, model.GrantState, nsID) ||
		s.St.HasGrant(ownerID, a.UserID, model.GrantState, "")
}

// stateWritable 写权限：**只有命名空间成员**（被授权者只读）。
func (s *Server) stateWritable(nsID, userID string) bool {
	return s.canManage(nsID, userID)
}

/* ============================ 知识库 kb ============================ */

// kbRefOf 从路由参数拼出引用。
//
// 引用有两种形态（`KD-…` 单段、`@命名空间/slug` 两段），路由也注册了两套，
// 所以**必须把两段拼起来**再交给 store —— 只取 `:id` 会导致 `@ns/slug` 取不到。
func kbRefOf(c *gin.Context) string {
	if slug := c.Param("slug"); slug != "" {
		return c.Param("id") + "/" + slug
	}
	return c.Param("id")
}

func kbJSON(r *store.KbRow, withContent bool) gin.H {
	out := gin.H{
		"id": r.ID, "ref": r.Ref(), "slug": r.Slug, "title": r.Title,
		"kind": r.Kind, "kindLabel": model.KbKindLabel(r.Kind, "zh"), "format": r.Format,
		"summary": r.Summary, "tags": parseStringList(r.Tags),
		"visibility": r.Visibility, "status": r.Status, "revision": r.Revision,
		"size": r.Size, "checksum": r.Checksum, "source": r.Source,
		"namespace": gin.H{"slug": r.NsSlug, "name": r.NsName},
		"owner":     gin.H{"id": r.OwnerID, "name": r.OwnerName},
		"createdBy": r.CreatedBy, "updatedBy": r.UpdatedBy,
		"createdAt": r.CreatedAt.UTC().Format(time.RFC3339),
		"updatedAt": r.UpdatedAt.UTC().Format(time.RFC3339),
	}
	if withContent {
		out["content"] = r.Content
	}
	return out
}

// kbKinds GET /api/kb/kinds —— 词表与上限（CLI 取值来源）。
func (s *Server) kbKinds(c *gin.Context) {
	kinds := make([]gin.H, 0, len(model.KbKinds))
	for _, k := range model.KbKinds {
		meta := model.KbKindMeta[k]
		kinds = append(kinds, gin.H{"id": k, "zh": meta[0], "en": meta[1], "descZh": meta[2], "descEn": meta[3]})
	}
	ok(c, 200, gin.H{
		"resource": "kb", "kinds": kinds, "formats": model.KbFormats,
		"visibilities": []string{model.KbPrivate, model.KbPublic},
		"statuses":     []string{model.KbActive, model.KbArchived},
		"limits":       gin.H{"maxBytes": model.KbMaxBytes, "maxTags": model.KbMaxTags},
		// 五类内容**共用**的口径（一份常量，五处返回同一份；见 model.SharedInvariants）
		"invariants": model.SharedInvariants(),
		// 检索是**关键词**打分（标题/摘要/正文加权），不是向量检索 —— 说清楚。
		"search": "keyword (weighted title/summary/content); vector search is not implemented",
	})
}

// listKb GET /api/kb?…
//
// 读接口不挂作用域中间件：公开文档匿名可读（与配置同一取舍），
// 非公开的在 handler 里判 —— 「该不该给这个人看」与「这篇是不是公开」必须一起判。
func (s *Server) listKb(c *gin.Context) {
	page, _ := strconv.Atoi(c.DefaultQuery("page", "1"))
	size, _ := strconv.Atoi(c.DefaultQuery("size", "20"))
	if page < 1 {
		page = 1
	}
	if size <= 0 || size > 200 {
		size = 20
	}
	o := store.KbListOpts{
		StateScope:  s.stateScopeOf(c),
		NsSlug:      strings.TrimSpace(strings.TrimPrefix(c.Query("namespace"), "@")),
		Kind:        c.Query("kind"),
		Tag:         c.Query("tag"),
		Status:      c.Query("status"),
		Q:           strings.TrimSpace(c.Query("q")),
		IncludeArch: c.Query("archived") == "1",
		// 公开文档**在列表里也要出现**（否则"按引用取得到、列表里看不到"）。
		// 匿名调用者没有命名空间范围，这一档就是它唯一的可见面。
		IncludePublic: true,
		Rank:          c.Query("q") != "",
		Page:          page, Size: size,
	}
	if o.Kind != "" && !model.ValidKbKind(o.Kind) {
		fail(c, 400, "bad_kind", "未知文档类型: "+o.Kind+"（见 GET /api/kb/kinds）")
		return
	}
	rows, matched, err := s.St.ListKbDocs(o)
	if err != nil {
		failErr(c, err)
		return
	}
	// 行级过滤：公开项之外，还要这个身份真的能读（成员身份 / state 授权）。
	// store 侧已经按「我的 ∪ 被授权的 ∪ 公开的」取过一轮，这里是**第二道**（纵深防御）：
	// 可见性判错一次就等于泄漏，宁可多判一遍。
	list := make([]gin.H, 0, len(rows))
	for i := range rows {
		r := &rows[i]
		switch {
		case r.IsPublic() && r.Status == model.KbActive:
			list = append(list, kbJSON(r, false))
		case authOf(c) != nil && s.stateReadable(c, r.NamespaceID, r.OwnerID):
			list = append(list, kbJSON(r, false))
		}
	}
	ok(c, 200, gin.H{
		"docs": list,
		// total 是**过滤之后**能看的条数；matched 是查询命中的条数（两者差说明
		// 有些命中项不在你的可见范围里 —— 说清楚比给一个含糊的数好）。
		"total": len(list), "matched": matched, "page": page, "size": size,
		"scope": stateScopeLabel(c, o.StateScope.All), "ranked": o.Rank,
	})
}

func stateScopeLabel(c *gin.Context, all bool) string {
	if all {
		return "all"
	}
	if c.Query("mine") == "1" {
		return "mine"
	}
	if authOf(c) == nil {
		return "public"
	}
	return "visible"
}

// kbBundle GET /api/kb/bundle?namespace=&kind=&tag= —— Agent 把知识库拉到本地。
//
// 与配置的 bundle 同形：这是 Agent 落地的第一步（先有语料，才谈得上用）。
// 默认只给**读得到的**（公开 + 我的 + 被授权的），并带上 checksum 便于本地增量同步。
func (s *Server) kbBundle(c *gin.Context) {
	ns := strings.TrimSpace(strings.TrimPrefix(c.Query("namespace"), "@"))
	if ns == "" {
		fail(c, 400, "bad_request", "缺 namespace（要拉哪个命名空间的库，如 @team）")
		return
	}
	rows, _, err := s.St.ListKbDocs(store.KbListOpts{
		StateScope: s.stateScopeOf(c), NsSlug: ns,
		Kind: c.Query("kind"), Tag: c.Query("tag"),
		Status: model.KbActive, IncludePublic: true, Limit: 500,
	})
	if err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		r := &rows[i]
		if !r.IsPublic() && !s.stateReadable(c, r.NamespaceID, r.OwnerID) {
			continue
		}
		out = append(out, kbJSON(r, true))
	}
	ok(c, 200, gin.H{
		"namespace": ns, "docs": out, "count": len(out),
		"how": "每条带 checksum 与 format —— 本地按 slug 落盘，据 checksum 做增量同步",
	})
}

// kbReq 写入请求（创建与更新共用；按 (namespace, slug) upsert）。
type kbReq struct {
	Namespace  string   `json:"namespace"`
	Slug       string   `json:"slug"`
	Title      string   `json:"title"`
	Kind       string   `json:"kind"`
	Format     string   `json:"format"`
	Summary    string   `json:"summary"`
	Tags       []string `json:"tags"`
	Visibility string   `json:"visibility"`
	Source     string   `json:"source"`
	Content    string   `json:"content"`
	Note       string   `json:"note"`
}

func (s *Server) kbBody(c *gin.Context) (*kbReq, *model.Namespace, bool) {
	a := authOf(c)
	var req kbReq
	if err := c.ShouldBindJSON(&req); err != nil {
		fail(c, 400, "bad_json", "请求体不是合法 JSON: "+err.Error())
		return nil, nil, false
	}
	if req.Kind == "" {
		req.Kind = "doc"
	}
	if req.Format == "" {
		req.Format = "markdown"
	}
	if req.Visibility == "" {
		req.Visibility = model.KbPrivate
	}
	if strings.TrimSpace(req.Slug) == "" {
		// slug 缺省从 title 生成（与制品/配置同一套清洗）。
		req.Slug = store.Slugify(req.Title)
	}
	if !store.ValidSlug(req.Slug) {
		fail(c, 400, "bad_slug", "slug 不合法: "+strconv.Quote(req.Slug)+"（小写字母数字与 - _ .，2..64 位）")
		return nil, nil, false
	}
	if errs := model.KbValidate(req.Title, req.Kind, req.Format, req.Visibility, req.Content, req.Tags); len(errs) > 0 {
		fail(c, 400, "kb_invalid", strings.Join(errs, "; "))
		return nil, nil, false
	}
	ns, err := s.stateNamespace(a, req.Namespace)
	if err != nil {
		failErr(c, err)
		return nil, nil, false
	}
	return &req, ns, true
}

// upsertKb POST /api/kb —— 建或改（改 = 新版本）。
func (s *Server) upsertKb(c *gin.Context) {
	req, ns, okc := s.kbBody(c)
	if !okc {
		return
	}
	a := authOf(c)
	s.markBuiltinUsed(ns.ID, "kb")
	sum := sha256.Sum256([]byte(req.Content))
	row, created, err := s.St.UpsertKbDoc(store.KbInput{
		NamespaceID: ns.ID, Slug: req.Slug, Title: req.Title, Kind: req.Kind,
		Format: req.Format, Summary: req.Summary, Tags: req.Tags, Visibility: req.Visibility,
		Source: req.Source, Content: req.Content,
		Checksum: "sha256:" + hex.EncodeToString(sum[:]),
		Note:     req.Note, AuthorID: a.UserID, AuthorName: a.Email,
	})
	if err != nil {
		failErr(c, err)
		return
	}
	status := 201
	if !created {
		status = 200
	}
	ok(c, status, gin.H{
		"doc": kbJSON(row, false), "created": created,
		"ref": row.Ref(), "revision": row.Revision,
	})
}

// getKb GET /api/kb/:id（或 /:id/:slug）—— 取一篇（含内容）。
func (s *Server) getKb(c *gin.Context) {
	ref := kbRefOf(c)
	row, err := s.St.GetKbDoc(ref)
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这篇文档: "+ref)
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !row.IsPublic() && !s.stateReadable(c, row.NamespaceID, row.OwnerID) {
		fail(c, 403, "forbidden", "这篇文档不在你可见的范围内（默认私有；跨人查看要 state 授权）")
		return
	}
	out := kbJSON(row, true)
	// 取历史版本：`?revision=N`（默认最新）。
	if rev := c.Query("revision"); rev != "" && rev != "latest" {
		n, err := strconv.ParseInt(rev, 10, 64)
		if err != nil || n <= 0 {
			fail(c, 400, "bad_revision", "revision 要是正整数")
			return
		}
		revs, err := s.St.KbRevisions(row.ID)
		if err != nil {
			failErr(c, err)
			return
		}
		for i := range revs {
			if revs[i].Revision == n {
				out["content"] = revs[i].Content
				out["checksum"] = revs[i].Checksum
				out["title"] = revs[i].Title
				out["requestedRevision"] = n
				ok(c, 200, out)
				return
			}
		}
		fail(c, 404, "not_found", "没有第 "+rev+" 版")
		return
	}
	ok(c, 200, out)
}

// patchKb PATCH /api/kb/:id —— 改名 / 归档 / 恢复（内容更新走 POST upsert）。
func (s *Server) patchKb(c *gin.Context) {
	a := authOf(c)
	row, err := s.St.GetKbDoc(kbRefOf(c))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这篇文档")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(row.NamespaceID, a.UserID) {
		fail(c, 403, "forbidden", "只有归属命名空间的成员能改这篇文档")
		return
	}
	var req struct {
		Title      *string   `json:"title"`
		Summary    *string   `json:"summary"`
		Tags       *[]string `json:"tags"`
		Visibility *string   `json:"visibility"`
		Status     *string   `json:"status"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		fail(c, 400, "bad_json", "请求体不是合法 JSON: "+err.Error())
		return
	}
	upd := map[string]any{"updated_by": a.UserID}
	if req.Title != nil {
		upd["title"] = *req.Title
	}
	if req.Summary != nil {
		upd["summary"] = *req.Summary
	}
	if req.Tags != nil {
		b, _ := json.Marshal(*req.Tags)
		upd["tags"] = string(b)
	}
	if req.Visibility != nil {
		if !model.ValidKbVisibility(*req.Visibility) {
			fail(c, 400, "bad_visibility", "visibility 只能是 private|public")
			return
		}
		upd["visibility"] = *req.Visibility
	}
	if req.Status != nil {
		if !model.ValidKbStatus(*req.Status) {
			fail(c, 400, "bad_status", "status 只能是 active|archived")
			return
		}
		upd["status"] = *req.Status
	}
	if err := s.St.DB.Model(&model.KbDoc{}).Where("id = ?", row.ID).Updates(upd).Error; err != nil {
		failErr(c, err)
		return
	}
	got, _ := s.St.GetKbDoc(row.ID)
	ok(c, 200, gin.H{"doc": kbJSON(got, false)})
}

// deleteKb DELETE /api/kb/:id
func (s *Server) deleteKb(c *gin.Context) {
	a := authOf(c)
	row, err := s.St.GetKbDoc(kbRefOf(c))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这篇文档")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(row.NamespaceID, a.UserID) {
		fail(c, 403, "forbidden", "只有归属命名空间的成员能删这篇文档")
		return
	}
	if err := s.St.DeleteKbDoc(row.ID); err != nil {
		failErr(c, err)
		return
	}
	ok(c, 200, gin.H{"ok": true, "id": row.ID, "ref": row.Ref()})
}

// kbRevisions GET /api/kb/:id/revisions
func (s *Server) kbRevisions(c *gin.Context) {
	row, err := s.St.GetKbDoc(kbRefOf(c))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这篇文档")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !row.IsPublic() && !s.stateReadable(c, row.NamespaceID, row.OwnerID) {
		fail(c, 403, "forbidden", "这篇文档不在你可见的范围内")
		return
	}
	revs, err := s.St.KbRevisions(row.ID)
	if err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(revs))
	for i := range revs {
		r := &revs[i]
		// **不回正文**：历史列表只给"谁在什么时候改了什么"，
		// 要旧版正文就用 `?revision=N` 明确去取（避免一次拉回几十版全文）。
		out = append(out, gin.H{
			"revision": r.Revision, "title": r.Title, "size": r.Size, "checksum": r.Checksum,
			"note": r.Note, "author": r.Author, "authorId": r.AuthorID,
			"at": r.CreatedAt.UTC().Format(time.RFC3339),
		})
	}
	ok(c, 200, gin.H{"ref": row.Ref(), "revisions": out, "revision": row.Revision})
}

/* ============================ 记忆 mem ============================ */

func memJSON(r *store.MemRow, now time.Time) gin.H {
	out := gin.H{
		"id": r.ID, "subject": r.Subject, "key": r.Key, "value": r.Value,
		"kind": r.Kind, "kindLabel": model.MemKindLabel(r.Kind, "zh"),
		"tags": parseStringList(r.Tags), "source": r.Source,
		"confidence": r.Confidence, "pinned": r.Pinned, "revision": r.Revision,
		"namespace": gin.H{"slug": r.NsSlug, "name": r.NsName},
		"createdBy": r.CreatedBy, "updatedBy": r.UpdatedBy,
		"createdAt": r.CreatedAt.UTC().Format(time.RFC3339),
		"updatedAt": r.UpdatedAt.UTC().Format(time.RFC3339),
	}
	if r.ExpiresAt != nil {
		out["expiresAt"] = r.ExpiresAt.UTC().Format(time.RFC3339)
		out["expired"] = r.Expired(now)
	}
	return out
}

// memKinds GET /api/mem/kinds
func (s *Server) memKinds(c *gin.Context) {
	kinds := make([]gin.H, 0, len(model.MemKinds))
	for _, k := range model.MemKinds {
		meta := model.MemKindMeta[k]
		kinds = append(kinds, gin.H{"id": k, "zh": meta[0], "en": meta[1], "descZh": meta[2], "descEn": meta[3]})
	}
	ok(c, 200, gin.H{
		"resource": "mem", "kinds": kinds,
		"limits": gin.H{"maxValueBytes": model.MemMaxValueBytes, "maxKeyLen": model.MemMaxKeyLen, "maxSubjectLen": model.MemMaxSubjectLen},
		// 五类内容**共用**的口径（一份常量，五处返回同一份）
		"invariants": model.SharedInvariants(),
		"semantics": gin.H{
			"upsert":     "(namespace, subject, key) 唯一：同键再写就是更新（Revision+1）",
			"expiry":     "ttl_days>0 时写 expiresAt；**读时判定**过期（过期即视为不存在），另有 gc 真正清理",
			"visibility": "记忆没有公开档：只有命名空间成员与拿到 state 授权的人能读",
		},
	})
}

// listMem GET /api/mem?…
func (s *Server) listMem(c *gin.Context) {
	limit, _ := strconv.Atoi(c.DefaultQuery("limit", "200"))
	if limit <= 0 || limit > 1000 {
		limit = 200
	}
	o := store.MemListOpts{
		StateScope:     s.stateScopeOf(c),
		Subject:        c.Query("subject"),
		Prefix:         c.Query("prefix"),
		Kind:           c.Query("kind"),
		Tag:            c.Query("tag"),
		Source:         c.Query("source"),
		IncludeExpired: c.Query("expired") == "1",
		PinnedOnly:     c.Query("pinned") == "1",
		Limit:          limit,
	}
	rows, total, err := s.St.ListMemEntries(o)
	if err != nil {
		failErr(c, err)
		return
	}
	now := time.Now()
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		out = append(out, memJSON(&rows[i], now))
	}
	ok(c, 200, gin.H{
		"memories": out, "total": total, "scope": stateScopeLabel(c, o.StateScope.All),
	})
}

// lookupMem GET /api/mem/lookup?namespace=&subject=&key= —— Agent 读一条记忆的主路径。
func (s *Server) lookupMem(c *gin.Context) {
	a := authOf(c)
	ns, err := s.stateNamespace(a, c.Query("namespace"))
	if err != nil {
		failErr(c, err)
		return
	}
	subject := c.DefaultQuery("subject", "self")
	key := c.Query("key")
	if key == "" {
		fail(c, 400, "bad_request", "缺 key")
		return
	}
	row, err := s.St.GetMemByKey(ns.ID, subject, key, time.Now())
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这条记忆（或已过期）: "+subject+"/"+key)
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	ok(c, 200, gin.H{"memory": memJSON(row, time.Now())})
}

// memPutReq 写记忆（upsert）。
type memPutReq struct {
	Namespace  string   `json:"namespace"`
	Subject    string   `json:"subject"`
	Key        string   `json:"key"`
	Value      string   `json:"value"`
	Kind       string   `json:"kind"`
	Tags       []string `json:"tags"`
	Source     string   `json:"source"`
	Confidence int64    `json:"confidence"`
	Pinned     bool     `json:"pinned"`
	TTLDays    int      `json:"ttl_days"`
}

// putMem PUT /api/mem —— 写一条记忆（同键即更新）。
func (s *Server) putMem(c *gin.Context) {
	a := authOf(c)
	var req memPutReq
	if err := c.ShouldBindJSON(&req); err != nil {
		fail(c, 400, "bad_json", "请求体不是合法 JSON: "+err.Error())
		return
	}
	if req.Subject == "" {
		req.Subject = "self"
	}
	if req.Kind == "" {
		req.Kind = "fact"
	}
	if errs := model.MemValidate(req.Subject, req.Key, req.Kind, req.Value, req.Confidence, req.TTLDays); len(errs) > 0 {
		fail(c, 400, "mem_invalid", strings.Join(errs, "; "))
		return
	}
	ns, err := s.stateNamespace(a, req.Namespace)
	if err != nil {
		failErr(c, err)
		return
	}
	s.markBuiltinUsed(ns.ID, "mem")
	row, created, err := s.St.UpsertMemEntry(store.MemInput{
		NamespaceID: ns.ID, Subject: req.Subject, Key: req.Key, Value: req.Value,
		Kind: req.Kind, Tags: req.Tags, Source: req.Source, Confidence: req.Confidence,
		Pinned: req.Pinned, TTLDays: req.TTLDays, AuthorID: a.UserID,
	})
	if err != nil {
		failErr(c, err)
		return
	}
	status := 201
	if !created {
		status = 200
	}
	ok(c, status, gin.H{
		"memory": memJSON(row, time.Now()), "created": created, "revision": row.Revision,
	})
}

// getMem GET /api/mem/:id
func (s *Server) getMem(c *gin.Context) {
	row, err := s.St.GetMemEntry(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这条记忆")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateReadable(c, row.NamespaceID, row.OwnerID) {
		fail(c, 403, "forbidden", "这条记忆不在你可见的范围内")
		return
	}
	ok(c, 200, gin.H{"memory": memJSON(row, time.Now())})
}

// deleteMem DELETE /api/mem/:id
func (s *Server) deleteMem(c *gin.Context) {
	a := authOf(c)
	row, err := s.St.GetMemEntry(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这条记忆")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(row.NamespaceID, a.UserID) {
		fail(c, 403, "forbidden", "只有归属命名空间的成员能删这条记忆")
		return
	}
	if err := s.St.DeleteMemEntry(row.ID); err != nil {
		failErr(c, err)
		return
	}
	ok(c, 200, gin.H{"ok": true, "id": row.ID, "key": row.Key})
}

// gcMem POST /api/mem/gc —— 真正删掉过期条目（读时已判过期，这里只是清垃圾）。
func (s *Server) gcMem(c *gin.Context) {
	a := authOf(c)
	ns, err := s.stateNamespace(a, c.Query("namespace"))
	if err != nil {
		failErr(c, err)
		return
	}
	n, err := s.St.GcMemEntries(ns.ID, time.Now())
	if err != nil {
		failErr(c, err)
		return
	}
	ok(c, 200, gin.H{"ok": true, "removed": n, "namespace": ns.Slug})
}

/* ============================ 检查点 ckpt ============================ */

func ckptJSON(s *Server, r *store.CkptRow, withURL bool) gin.H {
	out := gin.H{
		"id": r.ID, "name": r.Name, "label": r.Label,
		"labelText": model.CkptLabelText(r.Label, "zh"),
		"step":      r.Step, "summary": r.Summary, "tags": parseStringList(r.Tags),
		"visibility": r.Visibility, "status": r.Status,
		"subjectRef": r.SubjectRef, "subjectVersion": r.SubjectVersion,
		"parent": r.Parent, "digest": r.Digest, "size": r.Size, "mediaType": r.MediaType,
		"meta":      parseJSONAny(r.Meta),
		"namespace": gin.H{"slug": r.NsSlug, "name": r.NsName},
		"createdBy": r.CreatedBy,
		"createdAt": r.CreatedAt.UTC().Format(time.RFC3339),
	}
	if withURL && r.ObjectKey != "" {
		out["bytesUrl"] = s.ckptBytesURL(r.ID, 10*time.Minute)
		out["bytesTtlSec"] = 600
	}
	return out
}

// ckptKinds GET /api/ckpt/kinds
func (s *Server) ckptKinds(c *gin.Context) {
	labels := make([]gin.H, 0, len(model.CkptLabels))
	for _, l := range model.CkptLabels {
		meta := model.CkptLabelMeta[l]
		labels = append(labels, gin.H{"id": l, "zh": meta[0], "en": meta[1], "descZh": meta[2], "descEn": meta[3]})
	}
	ok(c, 200, gin.H{
		"resource": "ckpt", "labels": labels,
		"limits": gin.H{"maxBytes": model.CkptMaxBytes, "signedUrlTtlSec": 600, "uploadMaxBytes": 256 << 20},
		// 五类内容**共用**的口径（一份常量，五处返回同一份）
		"invariants": model.SharedInvariants(),
		"semantics": gin.H{
			"immutable": "检查点不可改：要改就再打一个点（打点记录的是「当时是什么样」）",
			"lineage":   "parent 指向上一个点，可一路回溯",
		},
	})
}

// listCkpt GET /api/ckpt?…
func (s *Server) listCkpt(c *gin.Context) {
	limit, _ := strconv.Atoi(c.DefaultQuery("limit", "50"))
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	o := store.CkptListOpts{
		StateScope: s.stateScopeOf(c),
		SubjectRef: c.Query("ref"), Label: c.Query("label"), Tag: c.Query("tag"),
		Name: c.Query("q"), Status: c.Query("status"), IncludePublic: true, Limit: limit,
	}
	rows, total, err := s.St.ListCheckpoints(o)
	if err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		out = append(out, ckptJSON(s, &rows[i], false))
	}
	ok(c, 200, gin.H{
		"checkpoints": out, "total": total, "scope": stateScopeLabel(c, o.StateScope.All),
	})
}

// ckptCreateReq 创建检查点（元数据）。
type ckptCreateReq struct {
	Namespace      string   `json:"namespace"`
	SubjectRef     string   `json:"subject_ref"`
	SubjectVersion string   `json:"subject_version"`
	Name           string   `json:"name"`
	Label          string   `json:"label"`
	Step           int64    `json:"step"`
	Summary        string   `json:"summary"`
	Tags           []string `json:"tags"`
	Visibility     string   `json:"visibility"`
	Parent         string   `json:"parent"`
	Digest         string   `json:"digest"`
	Size           int64    `json:"size"`
	MediaType      string   `json:"media_type"`
	Meta           any      `json:"meta"`
}

// createCkpt POST /api/ckpt —— 先建元数据，再 PUT 字节（两步：避免把大对象塞进 JSON）。
func (s *Server) createCkpt(c *gin.Context) {
	a := authOf(c)
	var req ckptCreateReq
	if err := c.ShouldBindJSON(&req); err != nil {
		fail(c, 400, "bad_json", "请求体不是合法 JSON: "+err.Error())
		return
	}
	if req.Label == "" {
		req.Label = "manual"
	}
	if req.Visibility == "" {
		req.Visibility = model.KbPrivate
	}
	if errs := model.CkptValidate(req.Label, req.Name, req.SubjectRef, req.Digest, req.Size, req.Visibility); len(errs) > 0 {
		fail(c, 400, "ckpt_invalid", strings.Join(errs, "; "))
		return
	}
	// parent 必须存在且**属于同一个命名空间**（别让血缘跨到别人的点上）。
	if req.Parent != "" {
		p, err := s.St.GetCheckpoint(req.Parent)
		if err != nil {
			fail(c, 400, "bad_parent", "parent 不存在: "+req.Parent)
			return
		}
		if p.NamespaceID != "" && !s.stateReadable(c, p.NamespaceID, p.OwnerID) {
			fail(c, 403, "forbidden", "parent 不在你可见的范围内")
			return
		}
	}
	ns, err := s.stateNamespace(a, req.Namespace)
	if err != nil {
		failErr(c, err)
		return
	}
	meta := "{}"
	if req.Meta != nil {
		if b, err := json.Marshal(req.Meta); err == nil {
			meta = string(b)
		}
	}
	s.markBuiltinUsed(ns.ID, "ckpt")
	row, err := s.St.CreateCheckpoint(store.CkptInput{
		NamespaceID: ns.ID, SubjectRef: req.SubjectRef, SubjectVersion: req.SubjectVersion,
		Name: req.Name, Label: req.Label, Step: req.Step, Summary: req.Summary,
		Tags: req.Tags, Visibility: req.Visibility, Parent: req.Parent,
		Digest: req.Digest, Size: req.Size, MediaType: req.MediaType, Meta: meta,
		AuthorID: a.UserID,
	})
	if err != nil {
		failErr(c, err)
		return
	}
	ok(c, 201, gin.H{
		"checkpoint": ckptJSON(s, row, false),
		"next":       "PUT /api/ckpt/" + row.ID + "/blob （raw body，服务端会用 sha256 核对 digest）",
	})
}

// putCkptBlob PUT /api/ckpt/:id/blob —— 上传字节（服务端核对摘要后才落盘）。
func (s *Server) putCkptBlob(c *gin.Context) {
	a := authOf(c)
	row, err := s.St.GetCheckpoint(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这个检查点")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(row.NamespaceID, a.UserID) {
		fail(c, 403, "forbidden", "只有归属命名空间的成员能上传字节")
		return
	}
	if row.ObjectKey != "" {
		fail(c, 409, "already_uploaded", "这个检查点已经有字节了（检查点不可变：要改就再打一个点）")
		return
	}
	body, err := io.ReadAll(io.LimitReader(c.Request.Body, 256<<20+1))
	if err != nil || len(body) == 0 {
		fail(c, 400, "bad_request", "请求体为空或读取失败")
		return
	}
	if len(body) > 256<<20 {
		fail(c, 413, "payload_too_large", "请求体超过 256MB")
		return
	}
	sum := sha256.Sum256(body)
	got := "sha256:" + hex.EncodeToString(sum[:])
	if got != row.Digest {
		fail(c, 400, "digest_mismatch", "字节摘要与创建时声明的不一致（声明 "+row.Digest+"，实际 "+got+"）")
		return
	}
	objectKey := "ckpt-" + row.ID + "-" + store.RandHex(6)
	if _, err := s.Blob.Put(objectKey, body); err != nil {
		fail(c, 500, "internal", "写入字节失败")
		return
	}
	if err := s.St.SetCheckpointObject(row.ID, objectKey, int64(len(body))); err != nil {
		failErr(c, err)
		return
	}
	got2, _ := s.St.GetCheckpoint(row.ID)
	ok(c, 200, gin.H{"ok": true, "checkpoint": ckptJSON(s, got2, true)})
}

// getCkpt GET /api/ckpt/:id
func (s *Server) getCkpt(c *gin.Context) {
	row, err := s.St.GetCheckpoint(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这个检查点")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if row.Visibility != model.KbPublic && !s.stateReadable(c, row.NamespaceID, row.OwnerID) {
		fail(c, 403, "forbidden", "这个检查点不在你可见的范围内")
		return
	}
	ok(c, 200, gin.H{"checkpoint": ckptJSON(s, row, true)})
}

// ckptLineage GET /api/ckpt/:id/lineage
func (s *Server) ckptLineage(c *gin.Context) {
	row, err := s.St.GetCheckpoint(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这个检查点")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if row.Visibility != model.KbPublic && !s.stateReadable(c, row.NamespaceID, row.OwnerID) {
		fail(c, 403, "forbidden", "这个检查点不在你可见的范围内")
		return
	}
	rows, err := s.St.CheckpointLineage(row.ID)
	if err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		out = append(out, ckptJSON(s, &rows[i], false))
	}
	ok(c, 200, gin.H{"lineage": out, "count": len(out)})
}

// ckptBytes GET /api/ckpt/:id/bytes —— 字节流。
//
// 与制品同规矩：要么带签名参数（服务端签发的短时地址），要么带能读它的凭据。
func (s *Server) ckptBytes(c *gin.Context) {
	row, err := s.St.GetCheckpoint(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这个检查点")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.validCkptSig(row.ID, c.Query("exp"), c.Query("sig")) &&
		(row.Visibility != model.KbPublic || !s.stateReadable(c, row.NamespaceID, row.OwnerID)) {
		fail(c, 403, "forbidden", "需要签名地址（bytesUrl）或可读凭据")
		return
	}
	if row.ObjectKey == "" {
		fail(c, 409, "no_bytes", "这个检查点只有元数据，没有字节（创建后还没上传）")
		return
	}
	rc, _, err := s.Blob.Open(row.ObjectKey)
	if err != nil {
		fail(c, 404, "not_found", "字节已不在（可能被清理）")
		return
	}
	defer rc.Close()
	c.Header("Content-Type", row.MediaType)
	c.Header("X-NCC-Digest", row.Digest)
	c.Status(200)
	_, _ = io.Copy(c.Writer, rc)
}

// deleteCkpt DELETE /api/ckpt/:id —— 元数据与字节一起删。
func (s *Server) deleteCkpt(c *gin.Context) {
	a := authOf(c)
	row, err := s.St.GetCheckpoint(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) {
		fail(c, 404, "not_found", "没有这个检查点")
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.stateWritable(row.NamespaceID, a.UserID) {
		fail(c, 403, "forbidden", "只有归属命名空间的成员能删这个检查点")
		return
	}
	key, err := s.St.DeleteCheckpoint(row.ID)
	if err != nil {
		failErr(c, err)
		return
	}
	if key != "" {
		_ = s.Blob.Delete(key)
	}
	ok(c, 200, gin.H{"ok": true, "id": row.ID, "name": row.Name, "bytes": key != ""})
}

// pruneCkpt POST /api/ckpt/prune?ref=&keep=N —— 每个 subject 只留最新 N 个。
//
// **标 pruned + 删字节，元数据留下**：这样"这里曾经有个点、后来被清理了"仍然可查 ——
// 删干净会让历史出现无法解释的空洞。
func (s *Server) pruneCkpt(c *gin.Context) {
	a := authOf(c)
	keep, _ := strconv.Atoi(c.DefaultQuery("keep", "5"))
	if keep <= 0 {
		fail(c, 400, "bad_keep", "keep 必须大于 0（要全删请逐个 DELETE）")
		return
	}
	ref := c.Query("ref")
	ns, err := s.stateNamespace(a, c.Query("namespace"))
	if err != nil {
		failErr(c, err)
		return
	}
	if ref == "" {
		fail(c, 400, "bad_request", "缺 ref（要清理哪个制品的检查点，如 @you/agent）")
		return
	}
	if !s.stateWritable(ns.ID, a.UserID) {
		fail(c, 403, "forbidden", "只有归属命名空间的成员能清理检查点")
		return
	}
	doomed, err := s.St.PruneCheckpoints(ref, keep)
	if err != nil {
		failErr(c, err)
		return
	}
	removed := 0
	for _, k := range doomed {
		if err := s.Blob.Delete(k); err == nil {
			removed++
		}
	}
	ok(c, 200, gin.H{"ok": true, "ref": ref, "keep": keep, "pruned": len(doomed), "bytesRemoved": removed})
}

/* ---------------- 检查点字节的签名地址 ---------------- */

func (s *Server) ckptBytesURL(id string, ttl time.Duration) string {
	exp := time.Now().Add(ttl).Unix()
	sig := hmacSHA256(s.Cfg.JWTSecret, "ckpt:"+id+"|"+strconv.FormatInt(exp, 10))
	return s.Cfg.PublicURL + "/api/ckpt/" + id + "/bytes?exp=" + strconv.FormatInt(exp, 10) + "&sig=" + sig
}

// validCkptSig 校验签名地址。域前缀 `ckpt:` 让制品与检查点的签名**不能互相顶替**。
func (s *Server) validCkptSig(id, exp, sig string) bool {
	n, err := strconv.ParseInt(exp, 10, 64)
	if err != nil || n < time.Now().Unix() || sig == "" {
		return false
	}
	expect := hmacSHA256(s.Cfg.JWTSecret, "ckpt:"+id+"|"+exp)
	return hmac.Equal([]byte(sig), []byte(expect))
}
