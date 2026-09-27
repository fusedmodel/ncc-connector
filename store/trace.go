package store

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
)

/* ---------------- NCC Trace：运行轨迹的数据访问 ----------------
   与制品/配置共用「引用 = @命名空间/slug」的写法，但轨迹的主键是**客户端生成的
   traceId**（幂等键），行 id（TR-…）只是本节点的地址。

   两条不变量：
     1. **Doc 不可变**：写入后不再改（它带采集方算的 digest）。评测标注单独一张表。
     2. **内容级别如实存**：`payload` 由采集方声明，服务端不做"补全"也不做"降级"。
*/

// ErrTraceConflict 同一 traceId 但摘要不同：说明采集方改了内容却没换 id。
// 这不是"重复上报"，是"两条不同的轨迹撞了 id" —— 必须拒，否则数据集会静默丢数据。
var ErrTraceConflict = errors.New("trace_conflict")

// TraceRow 轨迹 + 命名空间/归属者摘要（列表一次 join 出，避免 N+1）。
type TraceRow struct {
	model.Trace
	NsSlug    string `gorm:"column:ns_slug"`
	NsName    string `gorm:"column:ns_name"`
	OwnerID   string `gorm:"column:owner_id"`
	OwnerName string `gorm:"column:owner_name"`
}

// Ref 规范引用（本节点内的地址就是 TR-… 行 id）。
func (r *TraceRow) Ref() string { return r.ID }

// TraceInsert 写入一条轨迹。
type TraceInsert struct {
	NamespaceID string
	CreatedBy   string
	Doc         *model.TraceDoc
	// Raw 落库的 JSON（采集方原文；解析失败时由调用方处理）。空则重新序列化 Doc。
	Raw []byte
}

// TraceListOpts 轨迹检索条件。
type TraceListOpts struct {
	// 可见性：轨迹**没有「公开」这一档**（跑过的业务数据不该匿名可见）。
	// NamespaceIDs = 我的命名空间；GrantedOwners = 把 trace 授权给我的人。
	NamespaceIDs  []string
	GrantedOwners []string
	All           bool // 管理员视角：不限命名空间

	Ref     string // 制品引用 @ns/slug（精确）
	Kind    string
	Status  string
	Agent   string
	Node    string
	Model   string
	Payload string
	Tag     string
	Grade   string
	Split   string
	Q       string // 关键词：traceId / 引用 / 节点 / Agent / 备注

	Since time.Time
	Until time.Time

	OnlyLabeled   bool
	OnlyUnlabeled bool
	FailuresOnly  bool

	NewestFirst bool // 默认 true（at desc）
	Page        int
	Size        int
	Limit       int
}

// traceBase 轨迹检索的基查询（不带 Select —— GORM 的 Count 会拿 Selects 拼 count()）。
func (s *Store) traceBase() *gorm.DB {
	return s.DB.Table("traces t").Joins("JOIN namespaces ns ON ns.id = t.namespace_id")
}

func (s *Store) traceQuery() *gorm.DB {
	return s.traceBase().
		Select(`t.*, ns.slug AS ns_slug, ns.name AS ns_name,
			ns.owner_id AS owner_id, u.name AS owner_name`).
		Joins("LEFT JOIN users u ON u.id = ns.owner_id")
}

func (o TraceListOpts) apply(q *gorm.DB) *gorm.DB {
	if !o.All {
		conds := []string{}
		args := []any{}
		if len(o.NamespaceIDs) > 0 {
			conds = append(conds, "t.namespace_id IN ?")
			args = append(args, o.NamespaceIDs)
		}
		if len(o.GrantedOwners) > 0 {
			conds = append(conds, "ns.owner_id IN ?")
			args = append(args, o.GrantedOwners)
		}
		if len(conds) == 0 {
			// 既没有命名空间也没被授权：查不到任何东西（fail-closed）。
			q = q.Where("1 = 0")
		} else {
			q = q.Where("("+strings.Join(conds, " OR ")+")", args...)
		}
	}
	if o.Ref != "" {
		q = q.Where("t.subject_ref = ?", o.Ref)
	}
	if o.Kind != "" {
		q = q.Where("t.kind = ?", o.Kind)
	}
	if o.Status != "" {
		q = q.Where("t.status = ?", o.Status)
	}
	if o.Agent != "" {
		q = q.Where("t.agent_name = ?", o.Agent)
	}
	if o.Node != "" {
		q = q.Where("t.node_name = ?", o.Node)
	}
	if o.Model != "" {
		// 模型用 name 或 provider/name 两种写法都能命中。
		q = q.Where("(t.model_name = ? OR t.model_provider || '/' || t.model_name = ?)", o.Model, o.Model)
	}
	if o.Payload != "" {
		q = q.Where("t.payload_level = ?", o.Payload)
	}
	if o.Tag != "" {
		q = q.Where("t.tags LIKE ?", `%"`+o.Tag+`"%`)
	}
	if o.Grade != "" {
		q = q.Where("t.eval_grade = ?", o.Grade)
	}
	if o.Split != "" {
		q = q.Where("t.eval_split = ?", o.Split)
	}
	if !o.Since.IsZero() {
		q = q.Where("t.at >= ?", o.Since.UTC())
	}
	if !o.Until.IsZero() {
		q = q.Where("t.at <= ?", o.Until.UTC())
	}
	if o.FailuresOnly {
		q = q.Where("t.status <> ?", model.TraceOK)
	}
	if o.OnlyLabeled {
		q = q.Where("t.label_count > 0")
	}
	if o.OnlyUnlabeled {
		q = q.Where("t.label_count = 0")
	}
	if o.Q != "" {
		like := "%" + o.Q + "%"
		q = q.Where("(t.trace_id LIKE ? OR t.subject_ref LIKE ? OR t.node_name LIKE ? OR t.agent_name LIKE ? OR t.doc LIKE ?)",
			like, like, like, like, like)
	}
	return q
}

func (o TraceListOpts) order() string {
	if o.NewestFirst {
		return "t.at DESC, t.id DESC"
	}
	return "t.at ASC, t.id ASC"
}

/* ---------------- 写入 ---------------- */

// InsertTrace 落一条轨迹（按 (命名空间, traceId) 幂等）。
//
// 三种结果：
//   - 新的 → created=true；
//   - 同 id 同摘要 → created=false（**重传是正常的**：网络重试、离线补报）；
//   - 同 id 不同摘要 → ErrTraceConflict（内容变了却复用 id，必须让采集方换 id）。
func (s *Store) InsertTrace(in TraceInsert) (*model.Trace, bool, error) {
	d := in.Doc
	if d == nil {
		return nil, false, errors.New("trace doc is nil")
	}
	at, err := time.Parse(time.RFC3339, d.At)
	if err != nil {
		return nil, false, fmt.Errorf("at 不是 RFC3339: %w", err)
	}
	raw := in.Raw
	if len(raw) == 0 {
		b, err := json.Marshal(d)
		if err != nil {
			return nil, false, err
		}
		raw = b
	}

	var existing model.Trace
	err = s.DB.Where("namespace_id = ? AND trace_id = ?", in.NamespaceID, d.ID).First(&existing).Error
	switch {
	case err == nil:
		if existing.Digest == d.Digest {
			return &existing, false, nil
		}
		return nil, false, fmt.Errorf("%w: traceId %s 已存在且内容不同（旧摘要 %s，新摘要 %s）—— 换一个 id",
			ErrTraceConflict, d.ID, existing.Digest, d.Digest)
	case !errors.Is(err, gorm.ErrRecordNotFound):
		return nil, false, err
	}

	tags, _ := json.Marshal(d.Tags)
	runLabels, _ := json.Marshal(d.Labels)
	row := &model.Trace{
		ID:             NewID("TR"),
		NamespaceID:    in.NamespaceID,
		TraceID:        d.ID,
		Kind:           d.Kind,
		Status:         d.Status,
		At:             at.UTC(),
		DurationMs:     d.DurationMs,
		SubjectRef:     d.Subject.Ref,
		SubjectKind:    d.Subject.Kind,
		SubjectVersion: d.Subject.Version,
		SubjectDigest:  d.Subject.Digest,
		SubjectEngine:  d.Subject.Engine,
		SubjectPolicy:  d.Subject.Policy,
		NodeName:       d.Source.Node,
		Host:           d.Source.Host,
		AgentName:      d.Source.Agent,
		UserName:       d.Source.User,
		ModelProvider:  d.Model.Provider,
		ModelName:      d.Model.Name,
		ModelCalls:     d.Model.Calls,
		InputTokens:    d.Usage.InputTokens,
		OutputTokens:   d.Usage.OutputTokens,
		CostUsdMicros:  d.Usage.CostUsdMicros,
		StepCount:      len(d.Steps),
		PayloadLevel:   d.Payload,
		Tags:           string(tags),
		RunLabels:      string(runLabels),
		Digest:         d.Digest,
		Doc:            string(raw),
		CreatedBy:      in.CreatedBy,
	}
	if err := s.DB.Create(row).Error; err != nil {
		return nil, false, err
	}
	return row, true, nil
}

/* ---------------- 读取 ---------------- */

// ListTraces 列表（不带 Doc，轻量）。
func (s *Store) ListTraces(o TraceListOpts) ([]TraceRow, int64, error) {
	var total int64
	if err := o.apply(s.traceBase()).Count(&total).Error; err != nil {
		return nil, 0, err
	}
	q := o.apply(s.traceQuery()).Order(o.order())
	if o.Size > 0 {
		page := o.Page
		if page < 1 {
			page = 1
		}
		q = q.Offset((page - 1) * o.Size).Limit(o.Size)
	} else if o.Limit > 0 {
		q = q.Limit(o.Limit)
	}
	var rows []TraceRow
	if err := q.Scan(&rows).Error; err != nil {
		return nil, 0, err
	}
	return rows, total, nil
}

// GetTrace 取一条（含 Doc）。id 可以是 TR-… 行 id，也可以是 traceId。
func (s *Store) GetTrace(id string) (*TraceRow, error) {
	var row TraceRow
	err := s.traceQuery().Where("t.id = ? OR t.trace_id = ?", id, id).Limit(1).Scan(&row).Error
	if err != nil {
		return nil, err
	}
	if row.ID == "" {
		return nil, gorm.ErrRecordNotFound
	}
	return &row, nil
}

// ExportTraces 取一批**含 Doc** 的轨迹（数据集导出用）。
//
// 返回 `truncated=true` 表示命中条数超过 limit：**如实告诉调用方被截断了** ——
// 悄悄少给几行，训练集就少了一截，而且没人会发现。
func (s *Store) ExportTraces(o TraceListOpts, limit int) ([]TraceRow, bool, error) {
	if limit <= 0 {
		limit = 5000
	}
	q := o.apply(s.traceQuery()).Order(o.order()).Limit(limit + 1)
	var rows []TraceRow
	if err := q.Scan(&rows).Error; err != nil {
		return nil, false, err
	}
	if len(rows) > limit {
		return rows[:limit], true, nil
	}
	return rows, false, nil
}

// ScanTracesForStats 取投影列（不含 Doc）用于聚合。上限防止一条 stats 查询把内存吃光。
func (s *Store) ScanTracesForStats(o TraceListOpts, cap int) ([]model.Trace, bool, error) {
	if cap <= 0 {
		cap = 200000
	}
	q := o.apply(s.traceBase()).
		Select(`t.id, t.trace_id, t.kind, t.status, t.at, t.duration_ms, t.subject_ref,
			t.subject_version, t.model_provider, t.model_name, t.step_count, t.payload_level,
			t.tags, t.agent_name, t.input_tokens, t.output_tokens, t.cost_usd_micros,
			t.eval_grade, t.eval_reward, t.eval_score, t.eval_split, t.label_count`).
		Order("t.at ASC").Limit(cap + 1)
	var rows []model.Trace
	if err := q.Scan(&rows).Error; err != nil {
		return nil, false, err
	}
	if len(rows) > cap {
		return rows[:cap], true, nil
	}
	return rows, false, nil
}

// CountTraces 全量条数（/api/meta 用）。
func (s *Store) CountTraces() (int64, error) {
	var n int64
	err := s.DB.Model(&model.Trace{}).Count(&n).Error
	return n, err
}

/* ---------------- 评测标注 ---------------- */

// TraceLabelInput 追加一条标注。
type TraceLabelInput struct {
	TraceID  string // 行 id（TR-…）
	Key      string
	Value    string
	ValueNum int64
	HasNum   bool
	By       string
	Note     string
}

// AddTraceLabel 追加一条标注，并把最新值投影回 traces 行（供检索与聚合）。
func (s *Store) AddTraceLabel(in TraceLabelInput) (*model.TraceLabel, error) {
	l := &model.TraceLabel{
		ID: NewID("TL"), TraceID: in.TraceID, Key: in.Key,
		Value: in.Value, ValueNum: in.ValueNum, HasNum: in.HasNum,
		By: in.By, Note: in.Note,
	}
	if err := s.DB.Create(l).Error; err != nil {
		return nil, err
	}
	// 计数器与投影：失败也不该让标注本身丢（标注已经落库了），所以只把错误带出去。
	upd := map[string]any{}
	switch in.Key {
	case "grade":
		upd["eval_grade"] = in.Value
	case "reward":
		upd["eval_reward"] = in.ValueNum
	case "score":
		upd["eval_score"] = in.ValueNum
	case "split":
		upd["eval_split"] = in.Value
	}
	q := s.DB.Model(&model.Trace{}).Where("id = ?", in.TraceID)
	if len(upd) > 0 {
		q = q.Updates(upd)
	}
	if err := q.Error; err != nil {
		return l, err
	}
	if err := s.DB.Model(&model.Trace{}).Where("id = ?", in.TraceID).
		UpdateColumn("label_count", gorm.Expr("label_count + 1")).Error; err != nil {
		return l, err
	}
	return l, nil
}

// ListTraceLabels 一条轨迹的标注历史（按时间正序）。
func (s *Store) ListTraceLabels(traceID string) ([]model.TraceLabel, error) {
	var out []model.TraceLabel
	err := s.DB.Where("trace_id = ?", traceID).Order("created_at ASC").Find(&out).Error
	return out, err
}

// DeleteTrace 删一条轨迹（连同它的标注）。**删就是删**：轨迹不是审计记录，
// 归属者有权把自己采集的数据删掉（审计面记的是"谁做了什么"，与轨迹数据无关）。
func (s *Store) DeleteTrace(id string) error {
	if err := s.DB.Where("trace_id = ?", id).Delete(&model.TraceLabel{}).Error; err != nil {
		return err
	}
	return s.DB.Where("id = ?", id).Delete(&model.Trace{}).Error
}

/* ---------------- 聚合（能力评估） ---------------- */

// 聚合本身在 model 层（纯函数、好测）：`model.TraceStatsOf(rows)`。
// 这里只负责把投影列扫出来 —— 不扫 Doc，几万条轨迹的 stats 也不会把内存吃光。
