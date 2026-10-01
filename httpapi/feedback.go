package httpapi

import (
	"encoding/json"
	"strconv"
	"strings"

	"github.com/gin-gonic/gin"

	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

/* ---------------- NCC Feedback：跨 Agent / 跨用户的反馈 ----------------
   规矩（与状态/轨迹同一套，别在这里放宽）：

     · **只追加**：建了一条就改不了内容，要补充就再回一条（回复也是一条反馈）。
       唯一的写动作是 **处置状态**（open/ack/resolved/wontfix），而且只有**目标拥有者**能改 ——
       「处置」和「内容」是两件事，混在一起就会出现"把不好听的话删掉"。
     · **默认私有**：作者不说 public，就只有作者 + 目标拥有者看得到。
     · **拥有者由服务端解析**：客户端说 `ownerId` 没用（请求体里根本不收这一栏）。
     · **fail-closed**：匿名只看得到 public；什么都没给就什么都查不到。
     · **不搬内容**：只允许引用（traceRef / stateRefs）。
*/

// errFeedbackMissing 没有 / 不可见 —— 两种情况共用一句（不泄露"存在但你看不到"）。
var errFeedbackMissing = errWith(404, "not_found", "没有这条反馈（或者它不给你看）")

// fbWriteReq 写一条反馈的请求体。
//
// 刻意**没有** ownerID / authorID / status：那三样是服务端与目标拥有者的东西，
// 客户端说了不算。
type fbWriteReq struct {
	AboutKind  string   `json:"aboutKind"`
	AboutRef   string   `json:"aboutRef"`
	About      string   `json:"about"` // 简写别名（CLI 两种都可能发）
	Kind       string   `json:"kind"`
	Score      int      `json:"score"`
	Body       string   `json:"body"`
	Tags       []string `json:"tags"`
	Agent      string   `json:"agent"`
	Visibility string   `json:"visibility"`
	TraceRef   string   `json:"traceRef"`
	StateRefs  []string `json:"stateRefs"`
	Hops       []string `json:"hops"`
	ParentID   string   `json:"parentId"`
	// Origin / OriginID：只有 relay（搬运）才带 —— 让重复搬运是幂等的。
	Origin   string `json:"origin"`
	OriginID string `json:"originId"`
}

// fbKinds GET /api/feedback/kinds —— 词表 + 上限 + 三条红线（离线可读，不需要认证）。
func (s *Server) fbKinds(c *gin.Context) {
	about := make([]gin.H, 0, len(model.FbAboutKinds))
	for _, k := range model.FbAboutKinds {
		about = append(about, gin.H{
			"id": k, "zh": model.FbAboutLabel(k, "zh"), "en": model.FbAboutLabel(k, "en"),
			"descZh": model.FbAboutMeta[k][2], "descEn": model.FbAboutMeta[k][3],
		})
	}
	kinds := make([]gin.H, 0, len(model.FbKinds))
	for _, k := range model.FbKinds {
		kinds = append(kinds, gin.H{
			"id": k, "zh": model.FbKindLabel(k, "zh"), "en": model.FbKindLabel(k, "en"),
			"descZh": model.FbKindMeta[k][2], "descEn": model.FbKindMeta[k][3],
		})
	}
	statuses := make([]gin.H, 0, len(model.FbStatuses))
	for _, k := range model.FbStatuses {
		statuses = append(statuses, gin.H{
			"id": k, "zh": model.FbStatusLabel(k, "zh"), "en": model.FbStatusLabel(k, "en"),
			"descZh": model.FbStatusMeta[k][2], "descEn": model.FbStatusMeta[k][3],
		})
	}
	ok(c, 200, gin.H{
		"aboutKinds": about, "kinds": kinds, "statuses": statuses,
		"visibilities": []gin.H{
			{"id": model.FbPrivate, "zh": "私有", "en": "Private", "default": true,
				"descZh": "只有作者与目标拥有者看得到（默认）", "descEn": "Only the author and the target owner (default)"},
			{"id": model.FbPublic, "zh": "公开", "en": "Public",
				"descZh": "谁都能看；relay 上云只搬公开的那些", "descEn": "Anyone can read it; only public ones are relayed"},
		},
		"limits": gin.H{
			"maxBody": model.FbMaxBody, "maxTags": model.FbMaxTags,
			"maxHops": model.FbMaxHops, "maxStateRefs": model.FbMaxStateRefs,
			"scoreMin": model.FbScoreMin, "scoreMax": model.FbScoreMax,
		},
		// 红线与代码里那份是同一份（`model.FbRedLines`）—— 文档、README、这里是同一句话。
		"redLines": model.FbRedLines(),
	})
}

// fbWho 当前身份（登录姓名用于展示；**不是**授权依据）。
func (s *Server) fbWho(c *gin.Context) (id, handle, agent string) {
	a := authOf(c)
	if a == nil {
		return "", "", ""
	}
	name := a.Email
	if u, err := s.St.FindUserByID(a.UserID); err == nil && strings.TrimSpace(u.Name) != "" {
		name = u.Name
	}
	// 哪个 Agent 在说话：客户端用 `NCC_AGENT` / `--as-agent` 声明；服务端只记下来。
	return a.UserID, name, strings.TrimSpace(c.GetHeader("NCC-Agent"))
}

// fbVisible 这个身份能不能看这条反馈。
func (s *Server) fbVisible(c *gin.Context, f *model.Feedback) bool {
	if f.Visibility == model.FbPublic {
		return true
	}
	a := authOf(c)
	if a == nil {
		return false
	}
	if a.UserID == f.AuthorID || (f.OwnerID != "" && a.UserID == f.OwnerID) {
		return true
	}
	return s.ensureAdmin(c)
}

// fbResolveOwner 解析「被反馈的东西是谁的」。
//
// **解析不出来不算错误**：反馈是话，话先记下来才有意义（"还没归到具体东西上的话"
// 就是 topic 那一档）。但要说清楚 `resolved=false` 与原因 —— 因为私有反馈的可见范围
// 就建立在 owner 上：没解析出 owner 时，它实际上只有作者自己（与管理员）看得到。
func (s *Server) fbResolveOwner(kind, ref string) (ownerID string, resolved bool, note string) {
	ref = strings.TrimSpace(ref)
	if ref == "" {
		return "", false, "没说清是对哪个东西的反馈（只有你自己看得到）"
	}
	switch kind {
	case "artifact":
		row, err := s.findByRef(ref)
		if err != nil {
			return "", false, "对不上这台节点上的制品（" + ref + "）—— 私有反馈就只有你自己看得到"
		}
		if ns, err := s.St.FindNamespaceByID(row.NamespaceID); err == nil {
			return ns.OwnerID, true, ""
		}
		return "", false, "制品在，但找不到它的归属命名空间"
	case "node", "service":
		if row, err := s.St.FindNodeRow(ref, ""); err == nil {
			if kind == "service" && row.Kind != model.NodeService {
				return row.OwnerID, false, "这台节点上的 " + ref + " 不是 kind=service 的东西"
			}
			return row.OwnerID, true, ""
		}
		// `@命名空间/slug` 形态
		if strings.HasPrefix(ref, "@") && strings.Contains(ref, "/") {
			parts := strings.SplitN(strings.TrimPrefix(ref, "@"), "/", 2)
			if row, err := s.St.FindNodeByNsSlug(parts[0], parts[1], ""); err == nil {
				return row.OwnerID, true, ""
			}
		}
		return "", false, "对不上这台节点上的节点（" + ref + "）"
	case "run":
		row, err := s.St.GetTrace(ref)
		if err != nil {
			return "", false, "对不上这台节点上的轨迹（" + ref + "）"
		}
		if row.OwnerID != "" {
			return row.OwnerID, true, ""
		}
		return "", false, "轨迹在，但它没记归属（老数据）"
	case "profile":
		return "", false, "名片在 hub 上，节点这边没有这一层 —— 私有反馈只有你自己看得到"
	case "agent", "topic":
		return "", false, ""
	}
	return "", false, "不认识的 aboutKind（" + kind + "）"
}

// fbAppendHop 追加一跳（**只追加、去重、有上限**）。
func fbAppendHop(hops []string, hop string) []string {
	hop = strings.TrimSpace(hop)
	if hop == "" {
		return hops
	}
	for _, h := range hops {
		if h == hop {
			return hops
		}
	}
	if len(hops) >= model.FbMaxHops {
		return hops
	}
	return append(hops, hop)
}

// fbSelfHop 这台机器在链路上的名字。
func (s *Server) fbSelfHop() string {
	name := strings.TrimSpace(s.Cfg.NodeName)
	if name == "" {
		name = "node"
	}
	return "node:" + name
}

// fbJSON 一条反馈的对外形状。
//
// `mine` / `toMe` / `canResolve` 三个标记是**给客户端的**：谁在客户端把"这条是别人发给我的"
// 判错，就会出现"我把别人的反馈当成自己的处置了"这种事 —— 判定放在服务端，客户端只管显示。
func (s *Server) fbJSON(c *gin.Context, f *model.Feedback, replies int64) gin.H {
	out := gin.H{
		"id":         f.ID,
		"aboutKind":  f.AboutKind,
		"aboutRef":   f.AboutRef,
		"kind":       f.Kind,
		"score":      f.Score,
		"body":       f.Body,
		"tags":       store.ParseList(f.Tags),
		"author":     gin.H{"id": f.AuthorID, "handle": f.AuthorHandle},
		"agent":      f.AgentID,
		"parentId":   f.ParentID,
		"hops":       store.ParseList(f.Hops),
		"visibility": f.Visibility,
		"status":     f.Status,
		"traceRef":   f.TraceRef,
		"stateRefs":  store.ParseList(f.StateRefs),
		"origin":     f.Origin,
		"originId":   f.OriginID,
		"ownerId":    f.OwnerID,
		"self":       f.OwnerID != "" && f.OwnerID == f.AuthorID,
		"createdAt":  f.CreatedAt,
	}
	a := authOf(c)
	if a != nil {
		out["mine"] = a.UserID == f.AuthorID
		out["toMe"] = f.OwnerID != "" && a.UserID == f.OwnerID
		// ⚠️ canResolve 必须**与 patchFeedback 的规则一模一样**：
		// 一是「没有归属就没人能处置」，二是「拥有者或管理员」。
		// 客户端只拿它决定要不要显示按钮 —— 这里不一致，界面上就会出现按下去 403 的按钮。
		out["canResolve"] = f.OwnerID != "" && (a.UserID == f.OwnerID || s.ensureAdmin(c))
		out["canReply"] = true
	} else {
		out["mine"], out["toMe"], out["canResolve"], out["canReply"] = false, false, false, false
	}
	if replies >= 0 {
		out["replies"] = replies
	}
	return out
}

// fbWriteReqOf 从请求体（或父级）拼出待写入的记录内容。
func (s *Server) fbDecode(c *gin.Context) (*fbWriteReq, error) {
	var req fbWriteReq
	if err := c.ShouldBindJSON(&req); err != nil {
		return nil, errWith(400, "bad_request", "请求体不是合法 JSON")
	}
	if strings.TrimSpace(req.AboutRef) == "" {
		req.AboutRef = req.About
	}
	if strings.TrimSpace(req.AboutKind) == "" {
		req.AboutKind = "topic"
	}
	if strings.TrimSpace(req.Kind) == "" {
		req.Kind = "report"
	}
	if strings.TrimSpace(req.Visibility) == "" {
		req.Visibility = model.FbPrivate
	}
	return &req, nil
}

// createFeedback POST /api/feedback —— 说一句。
func (s *Server) createFeedback(c *gin.Context) {
	req, err := s.fbDecode(c)
	if err != nil {
		failErr(c, err)
		return
	}
	// 回复：`--reply-to`（body）或 `/api/feedback/:id/reply`（路由）都能走这里。
	parentID := strings.TrimSpace(c.Param("id"))
	if parentID == "" {
		parentID = strings.TrimSpace(req.ParentID)
	}
	if parentID != "" {
		parent, err := s.St.GetFeedbackByID(parentID)
		if err != nil {
			failErr(c, errFeedbackMissing)
			return
		}
		if !s.fbVisible(c, parent) {
			failErr(c, errFeedbackMissing)
			return
		}
		// 回复**继承**父的归属与可见性：一条私有对话不会因为有人回一句就变成公开的。
		req.AboutKind, req.AboutRef = parent.AboutKind, parent.AboutRef
		if parent.Visibility == model.FbPrivate {
			req.Visibility = model.FbPrivate
		}
		req.ParentID = parentID
	}

	if errs := model.FeedbackValidate(req.AboutKind, req.AboutRef, req.Kind, req.Score,
		req.Body, req.Visibility, req.Tags, req.Hops, req.StateRefs); len(errs) > 0 {
		fail(c, 400, "invalid_feedback", strings.Join(errs, "；"))
		return
	}

	id, handle, agent := s.fbWho(c)
	if strings.TrimSpace(req.Agent) != "" {
		agent = strings.TrimSpace(req.Agent)
	}
	ownerID, resolved, note := s.fbResolveOwner(req.AboutKind, req.AboutRef)

	tags, _ := json.Marshal(cleanList(req.Tags, model.FbMaxTags, 40))
	refs, _ := json.Marshal(cleanList(req.StateRefs, model.FbMaxStateRefs, model.FbMaxRef))
	hops := cleanList(req.Hops, model.FbMaxHops, model.FbMaxRef)
	// 搬运（relay）才带 origin：它同时是「哪一跳」与幂等键的一部分。
	if o := strings.TrimSpace(req.Origin); o != "" {
		hops = fbAppendHop(hops, "cli:"+o)
	}
	hops = fbAppendHop(hops, s.fbSelfHop())
	raw, _ := json.Marshal(hops)

	f := &model.Feedback{
		ID: store.NewID("FB"), OwnerID: ownerID,
		AboutKind: req.AboutKind, AboutRef: req.AboutRef,
		Kind: req.Kind, Score: req.Score, Body: strings.TrimSpace(req.Body),
		Tags: string(tags), AuthorID: id, AuthorHandle: handle, AgentID: agent,
		ParentID: req.ParentID, Hops: string(raw),
		Visibility: req.Visibility, Status: model.FbOpen,
		TraceRef: strings.TrimSpace(req.TraceRef), StateRefs: string(refs),
		Origin: strings.TrimSpace(req.Origin), OriginID: strings.TrimSpace(req.OriginID),
	}
	// relay 的幂等：同一台机器的同一条只落一次（重复搬运不算错误）。
	if f.Origin != "" && f.OriginID != "" {
		if old, err := s.St.FindFeedbackByOrigin(f.Origin, f.OriginID); err == nil {
			ok(c, 200, gin.H{
				"feedback": s.fbJSON(c, old, -1), "duplicated": true,
				"note": "这条已经从 " + f.Origin + " 搬过了，没有重复落库",
			})
			return
		}
	}
	if err := s.St.CreateFeedback(f); err != nil {
		fail(c, 500, "internal", "写入反馈失败："+err.Error())
		return
	}
	ok(c, 201, gin.H{
		"feedback": s.fbJSON(c, f, 0),
		"resolved": resolved,
		// 解析不出归属时说清楚 —— 因为那时私有反馈实际上只有作者看得到。
		"note": note,
	})
}

// listFeedback GET /api/feedback —— 看一批（可见范围折在查询里，fail-closed）。
func (s *Server) listFeedback(c *gin.Context) {
	o, err := s.fbListOpts(c, true)
	if err != nil {
		failErr(c, err)
		return
	}
	rows, total, err := s.St.ListFeedback(o)
	if err != nil {
		failErr(c, errWith(500, "internal", "读取反馈失败"))
		return
	}
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		out = append(out, s.fbJSON(c, &rows[i].Feedback, rows[i].Replies))
	}
	page, size := fbNormPage(o.Page, o.Size)
	ok(c, 200, gin.H{
		"feedback": out, "total": total, "page": page, "size": size,
		"aboutKind": o.AboutKind, "aboutRef": o.AboutRef,
	})
}

// fbListOpts 把查询串折成过滤条件（列表与摘要共用，免得两边判得不一样）。
func (s *Server) fbListOpts(c *gin.Context, allowOwnerFilter bool) (store.FeedbackListOpts, error) {
	page, _ := strconv.Atoi(c.DefaultQuery("page", "1"))
	size, _ := strconv.Atoi(c.DefaultQuery("size", "20"))
	o := store.FeedbackListOpts{
		AboutKind:  strings.TrimSpace(c.Query("aboutKind")),
		AboutRef:   strings.TrimSpace(firstNonEmpty(c.Query("aboutRef"), c.Query("about"))),
		Kind:       strings.TrimSpace(c.Query("kind")),
		Status:     strings.TrimSpace(c.Query("status")),
		Visibility: strings.TrimSpace(c.Query("visibility")),
		Unresolved: c.Query("unresolved") == "1",
		Page:       page, Size: size,
	}
	if o.AboutKind != "" && !model.ValidFbAboutKind(o.AboutKind) {
		return o, errWith(400, "bad_about_kind", "aboutKind 必须是 "+strings.Join(model.FbAboutKinds, "|"))
	}
	if o.Visibility != "" && !model.ValidFbVisibility(o.Visibility) {
		return o, errWith(400, "bad_visibility", "visibility 必须是 private|public")
	}
	a := authOf(c)
	if a != nil {
		o.ViewerID = a.UserID
		o.AllSeen = c.Query("all") == "1" && s.ensureAdmin(c)
	}
	if allowOwnerFilter {
		switch strings.TrimSpace(c.Query("owner")) {
		case "me":
			if a == nil {
				return o, errWith(401, "unauthorized", "看「收件箱」要先登录")
			}
			o.OwnerID = a.UserID
		case "":
		default:
			o.OwnerID = strings.TrimSpace(c.Query("owner"))
		}
	}
	if c.Query("mine") == "1" {
		if a == nil {
			return o, errWith(401, "unauthorized", "看「我发过的」要先登录")
		}
		o.AuthorID = a.UserID
	}
	return o, nil
}

// getFeedback GET /api/feedback/:id —— 一条 + 它的回复。
func (s *Server) getFeedback(c *gin.Context) {
	f, err := s.St.GetFeedbackByID(c.Param("id"))
	if err != nil {
		failErr(c, errFeedbackMissing)
		return
	}
	if !s.fbVisible(c, f) {
		failErr(c, errFeedbackMissing)
		return
	}
	replies, err := s.St.ListFeedbackReplies(f.ID)
	if err != nil {
		failErr(c, errWith(500, "internal", "读取回复失败"))
		return
	}
	out := make([]gin.H, 0, len(replies))
	for i := range replies {
		out = append(out, s.fbJSON(c, &replies[i], -1))
	}
	ok(c, 200, gin.H{
		"feedback": s.fbJSON(c, f, int64(len(replies))),
		"replies":  out,
	})
}

// patchFeedback PATCH /api/feedback/:id —— **只改处置状态**，而且只有目标拥有者能改。
func (s *Server) patchFeedback(c *gin.Context) {
	var req struct {
		Status string `json:"status"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		fail(c, 400, "bad_request", "请求体不是合法 JSON")
		return
	}
	if !model.ValidFbStatus(req.Status) {
		fail(c, 400, "bad_status", "status 必须是 "+strings.Join(model.FbStatuses, "|"))
		return
	}
	f, err := s.St.GetFeedbackByID(c.Param("id"))
	if err != nil {
		failErr(c, errFeedbackMissing)
		return
	}
	a := authOf(c)
	if a == nil {
		fail(c, 401, "unauthorized", "先登录")
		return
	}
	// 「谁能处置」与「谁能看」不是一回事：看得到（比如作者本人）不代表能替对方关掉。
	if f.OwnerID == "" || (a.UserID != f.OwnerID && !s.ensureAdmin(c)) {
		fail(c, 403, "forbidden",
			"只有这条反馈的目标拥有者能改处置状态（内容不可改；要说不同意见就回一条）")
		return
	}
	if err := s.St.SetFeedbackStatus(f.ID, req.Status); err != nil {
		failErr(c, errWith(500, "internal", "改处置状态失败"))
		return
	}
	got, _ := s.St.GetFeedbackByID(f.ID)
	ok(c, 200, gin.H{"feedback": s.fbJSON(c, got, -1)})
}

// summaryFeedback GET /api/feedback/summary —— 聚合（**不是排名分**）。
func (s *Server) summaryFeedback(c *gin.Context) {
	o, err := s.fbListOpts(c, true)
	if err != nil {
		failErr(c, err)
		return
	}
	sum, err := s.St.FeedbackSummaryOf(o)
	if err != nil {
		fail(c, 500, "internal", "聚合失败："+err.Error())
		return
	}
	ok(c, 200, gin.H{
		"summary": sum,
		"scope": gin.H{
			"aboutKind": o.AboutKind, "aboutRef": o.AboutRef,
			"owner": o.OwnerID, "author": o.AuthorID,
			"visibility": o.Visibility,
		},
		"note": "它不是排序用的分数 —— 反馈不参与任何匹配/排名（只把话说清楚）",
	})
}

/* ---------------- 小工具 ---------------- */

// fbNormPage 与 store 那边同一套规则（1-based、默认 20、封顶 100）。
func fbNormPage(page, size int) (int, int) {
	if page < 1 {
		page = 1
	}
	if size < 1 {
		size = 20
	}
	if size > 100 {
		size = 100
	}
	return page, size
}
