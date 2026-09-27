package httpapi

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"strconv"
	"strings"
	"time"

	"github.com/gin-gonic/gin"
	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
	"github.com/fusedmodel/ncc-registry/store"
)

/* ---------------- NCC Trace：运行轨迹的采集与托管 ----------------
   给两件事用：
     · **能力评估**：同一个制品（HUR 包 / skill / agent 包）的不同版本跑下来，
       成功率、耗时、token/花费、人工结论分别是什么 —— 改版到底变好还是变差。
     · **后训练**：把真实轨迹导出成数据集（JSONL），带奖励/得分/切分标注。

   三条与「制品托管」不同的规矩（写在代码里，别在别处忘了）：
     1. **默认私有**：轨迹没有「公开」档。跑过的业务数据不该匿名可见。
     2. **内容级别由采集方声明**：`payload=digest`（默认）/ `preview` / `full`。
        服务端**只如实记录，不补全也不降级** —— 谁采集谁负责。
     3. **文档不可变、标注可变**：Doc 带采集方算的 digest（写入后不改），
        评测标注单独一张表只追加 —— 否则每打一次分就要重算摘要，幂等去重就废了。
*/

// TraceJSON 轨迹行的摘要视图（列表用；不含 Doc）。
func (s *Server) traceJSON(r *store.TraceRow) gin.H {
	out := gin.H{
		"id":         r.ID,
		"traceId":    r.TraceID,
		"kind":       r.Kind,
		"kindLabel":  model.TraceKindLabel(r.Kind, "zh"),
		"status":     r.Status,
		"at":         r.At.UTC().Format(time.RFC3339),
		"durationMs": r.DurationMs,
		"steps":      r.StepCount,
		"payload":    r.PayloadLevel,
		"digest":     r.Digest,
		"namespace":  gin.H{"slug": r.NsSlug, "name": r.NsName},
		"owner":      gin.H{"id": r.OwnerID, "name": r.OwnerName},
		"createdBy":  r.CreatedBy,
		"createdAt":  r.CreatedAt.UTC().Format(time.RFC3339),
	}
	if r.SubjectRef != "" || r.SubjectVersion != "" {
		out["subject"] = gin.H{
			"ref": r.SubjectRef, "kind": r.SubjectKind, "version": r.SubjectVersion,
			"digest": r.SubjectDigest, "engine": r.SubjectEngine, "policy": r.SubjectPolicy,
		}
	}
	if r.NodeName != "" || r.Host != "" || r.AgentName != "" {
		out["source"] = gin.H{
			"node": r.NodeName, "host": r.Host, "agent": r.AgentName, "user": r.UserName,
		}
	}
	if r.ModelName != "" || r.ModelProvider != "" {
		out["model"] = gin.H{"provider": r.ModelProvider, "name": r.ModelName, "calls": r.ModelCalls}
	}
	if r.InputTokens > 0 || r.OutputTokens > 0 || r.CostUsdMicros > 0 {
		out["usage"] = gin.H{
			"inputTokens": r.InputTokens, "outputTokens": r.OutputTokens,
			"costUsdMicros": r.CostUsdMicros,
			// 同时给一个人读的金额（微美元 → 美元，展示用，不参与摘要）。
			"costUsd": fmt.Sprintf("%.6f", float64(r.CostUsdMicros)/1e6),
		}
	}
	if tags := parseStringList(r.Tags); len(tags) > 0 {
		out["tags"] = tags
	}
	if labels := parseJSONAny(r.RunLabels); labels != nil {
		out["labels"] = labels
	}
	// 评测标注的投影：**与 run-time labels 分开报**，别让调用方以为是一个东西。
	ev := gin.H{"count": r.LabelCount}
	if r.EvalGrade != "" {
		ev["grade"] = r.EvalGrade
	}
	if r.EvalSplit != "" {
		ev["split"] = r.EvalSplit
	}
	if r.EvalReward != 0 {
		ev["rewardMilli"] = r.EvalReward
	}
	if r.EvalScore != 0 {
		ev["scoreMilli"] = r.EvalScore
	}
	out["evaluation"] = ev
	return out
}

// traceLabelJSON 一条标注。
func traceLabelJSON(l *model.TraceLabel) gin.H {
	out := gin.H{
		"id": l.ID, "key": l.Key, "value": l.Value,
		"by": l.By, "note": l.Note,
		"at": l.CreatedAt.UTC().Format(time.RFC3339),
	}
	if l.HasNum {
		out["valueNum"] = l.ValueNum
	}
	return out
}

/* ---------------- 检索条件 ---------------- */

// traceOpts 从查询串构造检索条件，并**按身份收窄可见范围**。
//
// 轨迹的可见性只有三条路：我的命名空间 / 把 trace 授权给我的人 / 管理员。
// 没有「公开」这一档 —— 这不是漏了，是有意的（见文件头注释）。
func (s *Server) traceOpts(c *gin.Context) store.TraceListOpts {
	o := store.TraceListOpts{NewestFirst: true}
	o.Ref = strings.TrimSpace(c.Query("ref"))
	o.Kind = strings.TrimSpace(c.Query("kind"))
	o.Status = strings.TrimSpace(c.Query("status"))
	o.Agent = strings.TrimSpace(c.Query("agent"))
	o.Node = strings.TrimSpace(c.Query("node"))
	o.Model = strings.TrimSpace(c.Query("model"))
	o.Payload = strings.TrimSpace(c.Query("payload"))
	o.Tag = strings.TrimSpace(c.Query("tag"))
	o.Grade = strings.TrimSpace(c.Query("grade"))
	o.Split = strings.TrimSpace(c.Query("split"))
	o.Q = strings.TrimSpace(c.Query("q"))
	o.Since = parseTraceTime(c.Query("since"))
	o.Until = parseTraceTime(c.Query("until"))
	o.OnlyLabeled = c.Query("labeled") == "1"
	o.OnlyUnlabeled = c.Query("labeled") == "0"
	o.FailuresOnly = c.Query("status") == "" && c.Query("failures") == "1"
	if order := strings.TrimSpace(c.Query("order")); order == "asc" {
		o.NewestFirst = false
	}
	return o
}

// scopeTraceVisibility 把身份折成检索的可见范围（写回 opts）。
//
// 默认**只给「我的 + 被授权给我的」**：管理员也要显式 `all=1` 才能看全节点。
// 轨迹是业务行为数据，"因为我是管理员所以一进来就看到所有人的轨迹"是错的默认
// —— 想看全量得先说出来。
func (s *Server) scopeTraceVisibility(c *gin.Context, o *store.TraceListOpts) {
	a := authOf(c)
	if a == nil {
		// 到不了这里（路由挂了 requireScope），但留一条 fail-closed 的兜底。
		o.NamespaceIDs = []string{"-"}
		return
	}
	if c.Query("all") == "1" && s.ensureAdmin(c) {
		o.All = true
		return
	}
	// mine=1：只要自己的命名空间（不含被授权的）。
	if nss, err := s.St.NamespacesOfUser(a.UserID); err == nil {
		for i := range nss {
			o.NamespaceIDs = append(o.NamespaceIDs, nss[i].ID)
		}
	}
	if c.Query("mine") != "1" {
		if owners, err := s.St.GrantedOwners(a.UserID, model.GrantTrace); err == nil {
			o.GrantedOwners = owners
		}
	}
}

// canReadTrace 命中的这一条能不能给这个身份看。
func (s *Server) canReadTrace(c *gin.Context, row *store.TraceRow) bool {
	a := authOf(c)
	if a == nil {
		return false
	}
	if s.ensureAdmin(c) {
		return true
	}
	if s.canManage(row.NamespaceID, a.UserID) {
		return true
	}
	return s.St.HasGrant(row.OwnerID, a.UserID, model.GrantTrace, row.NamespaceID) ||
		s.St.HasGrant(row.OwnerID, a.UserID, model.GrantTrace, "")
}

/* ---------------- 词表 ---------------- */

// traceKinds GET /api/traces/kinds —— 词表与上限（CLI 取值来源，与 /api/registry/kinds 同形）。
func (s *Server) traceKinds(c *gin.Context) {
	steps := make([]gin.H, 0, len(model.TraceStepTypes))
	for _, t := range model.TraceStepTypes {
		steps = append(steps, gin.H{"id": t})
	}
	labels := make([]gin.H, 0, len(model.TraceLabelKeys))
	for _, k := range model.TraceLabelKeys {
		meta := model.TraceLabelMeta[k]
		labels = append(labels, gin.H{"id": k, "zh": meta[0], "en": meta[1], "descZh": meta[2], "descEn": meta[3], "key": k, "keyEn": meta[1]})
	}
	kinds := make([]gin.H, 0, len(model.TraceKinds))
	for _, k := range model.TraceKinds {
		kinds = append(kinds, gin.H{"id": k, "zh": model.TraceKindLabel(k, "zh"), "en": model.TraceKindLabel(k, "en")})
	}
	ok(c, 200, gin.H{
		"spec":      model.TraceSpec,
		"kinds":     kinds,
		"statuses":  model.TraceStatuses,
		"payloads":  model.TracePayloadLevels,
		"stepTypes": steps,
		"labelKeys": labels,
		"grades":    model.TraceGrades,
		"splits":    model.TraceSplits,
		"limits": gin.H{
			"maxBytes":    model.TraceMaxBytes,
			"maxSteps":    model.TraceMaxSteps,
			"batchMax":    model.TraceBatchMax,
			"previewMax":  model.TracePreviewMax,
			"maxTags":     model.TraceMaxTags,
			"exportLimit": 20000,
		},
		// 五类内容**共用**的口径（一份常量，五处返回同一份）
		"invariants": model.SharedInvariants(),
	})
}

/* ---------------- 上报 ---------------- */

// traceIngestReq 一次上报：批量（pipeline 推送）或单条（Agent 跑完就报）。
type traceIngestReq struct {
	Traces    []json.RawMessage `json:"traces"`
	Namespace string            `json:"namespace"`
	// 单条形态：直接就是一份 trace 文档（带 spec 字段）。
	Spec string `json:"spec"`
}

// traceReject 一条被拒的轨迹：**逐条给出原因**，不让一条坏数据废掉整批。
type traceReject struct {
	Index  int                `json:"index"`
	ID     string             `json:"id,omitempty"`
	Code   string             `json:"code"`
	Msg    string             `json:"msg,omitempty"`
	Issues []model.TraceIssue `json:"issues,omitempty"`
}

// ingestTraces POST /api/traces —— 采集入口。
//
// 幂等：同 (命名空间, traceId) 重复上报是**正常现象**（网络重试、离线补报），
// 摘要一致就当已收下（duplicates++），摘要不同才拒（trace_conflict）。
func (s *Server) ingestTraces(c *gin.Context) {
	a := authOf(c)
	if a == nil {
		fail(c, 401, "unauthorized", "未认证或凭据无效")
		return
	}
	var req traceIngestReq
	if err := c.ShouldBindJSON(&req); err != nil {
		fail(c, 400, "bad_json", "请求体不是合法 JSON: "+err.Error())
		return
	}
	docs := req.Traces
	if len(docs) == 0 && req.Spec != "" {
		// 单条形态：整个 body 就是一份文档。重新序列化一次，保持一条路径。
		b, err := json.Marshal(req)
		if err == nil {
			docs = []json.RawMessage{b}
		}
	}
	if len(docs) == 0 {
		fail(c, 400, "empty", "没有要上报的轨迹（body 用 {\"traces\":[…]} 或直接给一份文档）")
		return
	}
	if len(docs) > model.TraceBatchMax {
		fail(c, 413, "batch_too_large", fmt.Sprintf("一次最多上报 %d 条，收到 %d 条", model.TraceBatchMax, len(docs)))
		return
	}

	// 落到哪个命名空间：默认个人空间。**不隐式跨命名空间**（轨迹属于采集它的那个空间）。
	ns, err := s.traceNamespace(a, req.Namespace)
	if err != nil {
		failErr(c, err)
		return
	}
	s.markBuiltinUsed(ns.ID, "trace")

	type accepted struct {
		Row   *model.Trace `json:"-"`
		Trace *model.TraceDoc
	}
	var (
		acc  []accepted
		rej  []traceReject
		dups int
	)
	for i, raw := range docs {
		var d model.TraceDoc
		if err := json.Unmarshal(raw, &d); err != nil {
			rej = append(rej, traceReject{Index: i, Code: "bad_json", Msg: "不是一份轨迹文档: " + err.Error()})
			continue
		}
		model.NormalizeTrace(&d)
		if issues := model.ValidateTrace(&d); model.TraceHasError(issues) {
			rej = append(rej, traceReject{Index: i, ID: d.ID, Code: "trace_invalid", Issues: issues})
			continue
		}
		row, created, err := s.St.InsertTrace(store.TraceInsert{
			NamespaceID: ns.ID, CreatedBy: a.UserID, Doc: &d, Raw: raw,
		})
		switch {
		case err == nil && created:
			acc = append(acc, accepted{Row: row, Trace: &d})
		case err == nil:
			dups++
		case errors.Is(err, store.ErrTraceConflict):
			rej = append(rej, traceReject{Index: i, ID: d.ID, Code: "trace_conflict", Msg: err.Error()})
		default:
			rej = append(rej, traceReject{Index: i, ID: d.ID, Code: "store_error", Msg: err.Error()})
		}
	}

	out := gin.H{
		"accepted":   len(acc),
		"duplicates": dups,
		"rejected":   len(rej),
		"namespace":  gin.H{"slug": ns.Slug, "name": ns.Name},
		"refs":       []gin.H{},
	}
	if len(rej) > 0 {
		out["rejects"] = rej
	}
	refs := make([]gin.H, 0, len(acc))
	for _, x := range acc {
		refs = append(refs, gin.H{
			"id": x.Row.ID, "traceId": x.Row.TraceID, "digest": x.Row.Digest,
			"kind": x.Row.Kind, "status": x.Row.Status, "payload": x.Row.PayloadLevel,
		})
	}
	out["refs"] = refs

	// 全被拒 → 400（让 pipeline 当场失败，而不是静默"成功 0 条"）。
	if len(acc) == 0 && dups == 0 {
		code := "trace_invalid"
		if len(rej) == 1 {
			code = rej[0].Code
		}
		fail(c, 400, code, "全部被拒：见 rejects")
		return
	}
	ok(c, 201, out)
}

// traceNamespace 解析目标命名空间（默认：调用者的个人空间）。
func (s *Server) traceNamespace(a *AuthInfo, want string) (*model.Namespace, error) {
	want = strings.TrimSpace(strings.TrimPrefix(want, "@"))
	nss, err := s.St.NamespacesOfUser(a.UserID)
	if err != nil {
		return nil, errWith(500, "internal", "服务内部错误")
	}
	if want == "" {
		// 默认落到 `type=account` 的个人空间（与 register 建空间时的默认一致）。
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
	return nil, errWith(403, "forbidden", "你不是命名空间 @"+want+" 的成员，不能把轨迹写进去")
}

/* ---------------- 列表 / 详情 / 删除 ---------------- */

func (s *Server) listTraces(c *gin.Context) {
	o := s.traceOpts(c)
	s.scopeTraceVisibility(c, &o)
	page, _ := strconv.Atoi(c.DefaultQuery("page", "1"))
	size, _ := strconv.Atoi(c.DefaultQuery("size", "20"))
	if page < 1 {
		page = 1
	}
	if size <= 0 || size > 200 {
		size = 20
	}
	o.Page, o.Size = page, size
	rows, total, err := s.St.ListTraces(o)
	if err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(rows))
	for i := range rows {
		out = append(out, s.traceJSON(&rows[i]))
	}
	ok(c, 200, gin.H{
		"traces": out, "total": total, "page": page, "size": size,
		// 一句话说清"我看到的是哪一部分"，免得把"我的轨迹"当成"全部轨迹"。
		"scope": traceScopeLabel(c, o.All),
	})
}

func traceScopeLabel(c *gin.Context, all bool) string {
	if all {
		return "all"
	}
	if c.Query("mine") == "1" {
		return "mine"
	}
	return "visible"
}

func (s *Server) getTrace(c *gin.Context) {
	row, err := s.St.GetTrace(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) || (err == nil && row.ID == "") {
		fail(c, 404, "not_found", "没有这条轨迹: "+c.Param("id"))
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.canReadTrace(c, row) {
		fail(c, 403, "forbidden", "这条轨迹不在你可见的范围内（轨迹默认私有；跨人查看需要 trace 授权）")
		return
	}
	var doc any
	if err := json.Unmarshal([]byte(row.Doc), &doc); err != nil {
		doc = json.RawMessage(row.Doc)
	}
	labels, _ := s.St.ListTraceLabels(row.ID)
	ls := make([]gin.H, 0, len(labels))
	for i := range labels {
		ls = append(ls, traceLabelJSON(&labels[i]))
	}
	out := s.traceJSON(row)
	out["trace"] = doc
	// 标注历史挂在 evaluation 里（gin.H 是 map[string]any，取出来改再放回去）。
	if ev, okc := out["evaluation"].(gin.H); okc {
		ev["labels"] = ls
		out["evaluation"] = ev
	}
	ok(c, 200, out)
}

func (s *Server) deleteTrace(c *gin.Context) {
	a := authOf(c)
	row, err := s.St.GetTrace(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) || (err == nil && row.ID == "") {
		fail(c, 404, "not_found", "没有这条轨迹: "+c.Param("id"))
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.ensureAdmin(c) && (a == nil || !s.canManage(row.NamespaceID, a.UserID)) {
		fail(c, 403, "forbidden", "只有归属命名空间的管理者能删除这条轨迹")
		return
	}
	if err := s.St.DeleteTrace(row.ID); err != nil {
		failErr(c, err)
		return
	}
	ok(c, 200, gin.H{"ok": true, "id": row.ID, "traceId": row.TraceID})
}

/* ---------------- 评测标注 ---------------- */

// traceLabelReq 追加标注：`key` + `value`（或 `reward` / `score` 数值）。
type traceLabelReq struct {
	Key    string `json:"key"`
	Value  string `json:"value"`
	Reward *int64 `json:"reward"` // 整数奖励（1 / 0 / -1）
	Score  *int64 `json:"score"`  // 0..1000 的千分位得分
	Note   string `json:"note"`
	By     string `json:"by"`
}

func (s *Server) addTraceLabel(c *gin.Context) {
	a := authOf(c)
	row, err := s.St.GetTrace(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) || (err == nil && row.ID == "") {
		fail(c, 404, "not_found", "没有这条轨迹: "+c.Param("id"))
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.canReadTrace(c, row) {
		fail(c, 403, "forbidden", "看不到这条轨迹，就不能标注它")
		return
	}
	var req traceLabelReq
	if err := c.ShouldBindJSON(&req); err != nil {
		fail(c, 400, "bad_json", "请求体不是合法 JSON: "+err.Error())
		return
	}
	in := store.TraceLabelInput{TraceID: row.ID, Note: req.Note, By: req.By}
	if in.By == "" {
		in.By = a.UserID
	}
	switch {
	case req.Reward != nil:
		in.Key, in.Value, in.ValueNum, in.HasNum = "reward", strconv.FormatInt(*req.Reward, 10), *req.Reward*1000, true
	case req.Score != nil:
		if *req.Score < -1000 || *req.Score > 1000 {
			fail(c, 400, "bad_score", "score 要在 -1..1 之间（千分位：-1000..1000）")
			return
		}
		in.Key, in.Value, in.ValueNum, in.HasNum = "score", strconv.FormatInt(*req.Score, 10), *req.Score, true
	default:
		k := strings.TrimSpace(req.Key)
		if k == "" {
			fail(c, 400, "bad_key", "缺 key（常用键见 GET /api/traces/kinds）")
			return
		}
		if len(k) > 64 {
			fail(c, 400, "bad_key", "key 太长（≤64）")
			return
		}
		if k == "grade" && req.Value != "" && !contains(model.TraceGrades, req.Value) {
			// 不拦，但提醒 —— 词表是给管线对齐用的。
			logf("trace label grade=%q 不在建议词表 %v 里（trace %s）", req.Value, model.TraceGrades, row.TraceID)
		}
		in.Key, in.Value = k, req.Value
	}
	l, err := s.St.AddTraceLabel(in)
	if err != nil {
		failErr(c, err)
		return
	}
	ok(c, 201, gin.H{"ok": true, "label": traceLabelJSON(l), "traceId": row.TraceID, "id": row.ID})
}

func (s *Server) listTraceLabels(c *gin.Context) {
	row, err := s.St.GetTrace(c.Param("id"))
	if errors.Is(err, gorm.ErrRecordNotFound) || (err == nil && row.ID == "") {
		fail(c, 404, "not_found", "没有这条轨迹: "+c.Param("id"))
		return
	}
	if err != nil {
		failErr(c, err)
		return
	}
	if !s.canReadTrace(c, row) {
		fail(c, 403, "forbidden", "这条轨迹不在你可见的范围内")
		return
	}
	labels, err := s.St.ListTraceLabels(row.ID)
	if err != nil {
		failErr(c, err)
		return
	}
	out := make([]gin.H, 0, len(labels))
	for i := range labels {
		out = append(out, traceLabelJSON(&labels[i]))
	}
	ok(c, 200, gin.H{"id": row.ID, "traceId": row.TraceID, "labels": out, "count": len(out)})
}

/* ---------------- 聚合（能力评估） ---------------- */

func (s *Server) traceStats(c *gin.Context) {
	o := s.traceOpts(c)
	s.scopeTraceVisibility(c, &o)
	rows, truncated, err := s.St.ScanTracesForStats(o, 200000)
	if err != nil {
		failErr(c, err)
		return
	}
	st := model.TraceStatsOf(rows)
	ok(c, 200, gin.H{
		"stats":     st,
		"sampled":   len(rows),
		"truncated": truncated,
		"scope":     traceScopeLabel(c, o.All),
		// 评估口径写在响应里：拿到这份 JSON 的人不用猜"成功率怎么算的"。
		"how": gin.H{
			"successRate":      "byStatus.ok / total",
			"scoreAvgMilli":    "只对**打过分的**轨迹求平均（不拿未标注的稀释分母）",
			"bySubjectVersion": "同一个 ref 的不同 version 分组 —— 评测改版前后的落点",
			"labeled":          "label_count > 0 的条数（标注覆盖率）",
		},
	})
}

/* ---------------- 数据集导出 ---------------- */

// exportTraces GET /api/traces/export —— 导出成数据集（JSONL）。
//
// 后训练管线要的就是这个：一行一条轨迹，原文（若有）与标注都在里面。
// 同时回**数据集摘要**（条数 + sha256 + 查询条件），这样「这份训练集是怎么来的」
// 是可复现、可核对的 —— 数据集本身就是一次实验的输入，得有指纹。
func (s *Server) exportTraces(c *gin.Context) {
	o := s.traceOpts(c)
	s.scopeTraceVisibility(c, &o)
	limit := 1000
	if v := strings.TrimSpace(c.Query("limit")); v != "" {
		if n, err := strconv.Atoi(v); err == nil && n > 0 {
			limit = n
		}
	}
	if limit > 20000 {
		limit = 20000
	}
	rows, truncated, err := s.St.ExportTraces(o, limit)
	if err != nil {
		failErr(c, err)
		return
	}

	type line struct {
		Trace any            `json:"trace"`
		Eval  []gin.H        `json:"evaluation,omitempty"`
		Ref   map[string]any `json:"ref,omitempty"`
	}
	var (
		blob strings.Builder
		docs []gin.H
		n    int
	)
	for i := range rows {
		r := &rows[i]
		var doc any
		if err := json.Unmarshal([]byte(r.Doc), &doc); err != nil {
			// 库里的 Doc 坏了：**如实报出来**，别静默跳过（那是数据丢失）。
			fail(c, 500, "corrupt_doc", "轨迹 "+r.ID+" 的文档解析失败："+err.Error())
			return
		}
		labels, _ := s.St.ListTraceLabels(r.ID)
		ls := make([]gin.H, 0, len(labels))
		for j := range labels {
			ls = append(ls, traceLabelJSON(&labels[j]))
		}
		row := gin.H{
			"spec":  "ncc-trace-dataset/v1",
			"trace": doc,
			"meta":  s.traceJSON(r),
		}
		if len(ls) > 0 {
			row["evaluation"] = ls
		}
		b, err := json.Marshal(row)
		if err != nil {
			fail(c, 500, "internal", "序列化失败："+err.Error())
			return
		}
		blob.Write(b)
		blob.WriteByte('\n')
		docs = append(docs, row)
		n++
	}
	sum := sha256.Sum256([]byte(blob.String()))
	digest := "sha256:" + hex.EncodeToString(sum[:])
	manifest := gin.H{
		"spec": "ncc-trace-dataset/v1", "count": n, "truncated": truncated,
		"limit": limit, "digest": digest, "generatedAt": timeNow(),
		"query": gin.H{
			"ref": o.Ref, "kind": o.Kind, "status": o.Status, "agent": o.Agent,
			"node": o.Node, "model": o.Model, "payload": o.Payload, "tag": o.Tag,
			"grade": o.Grade, "split": o.Split, "q": o.Q, "since": c.Query("since"), "until": c.Query("until"),
		},
		"scope": traceScopeLabel(c, o.All),
	}
	if c.Query("format") == "json" || c.Query("manifest") == "1" {
		ok(c, 200, gin.H{"manifest": manifest, "rows": docs})
		return
	}
	// 默认 JSONL：训练管线直接吃。
	c.Header("Content-Type", "application/x-ndjson; charset=utf-8")
	c.Header("X-NCC-Trace-Count", strconv.Itoa(n))
	c.Header("X-NCC-Dataset-Digest", digest)
	if truncated {
		c.Header("X-NCC-Truncated", "1")
		c.Header("Warning", `199 ncc-registry "结果被 limit 截断：用 since/size 收窄或分批导出"`)
	}
	c.String(200, blob.String())
}

/* ---------------- 小工具 ---------------- */

// parseTraceTime 接受 RFC3339 / `2026-09-26` / unix 秒（手写时间轴的三种常见写法）。
func parseTraceTime(v string) time.Time {
	v = strings.TrimSpace(v)
	if v == "" {
		return time.Time{}
	}
	for _, layout := range []string{time.RFC3339, "2006-01-02T15:04:05", "2006-01-02 15:04:05", "2006-01-02"} {
		if t, err := time.Parse(layout, v); err == nil {
			return t
		}
	}
	if n, err := strconv.ParseInt(v, 10, 64); err == nil && n > 0 {
		return time.Unix(n, 0)
	}
	return time.Time{}
}

func parseStringList(s string) []string {
	if strings.TrimSpace(s) == "" || s == "[]" {
		return nil
	}
	var out []string
	if err := json.Unmarshal([]byte(s), &out); err != nil {
		return nil
	}
	return out
}

func contains(list []string, v string) bool {
	for _, x := range list {
		if x == v {
			return true
		}
	}
	return false
}
