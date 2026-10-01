package store

import (
	"encoding/json"
	"errors"
	"strings"
	"time"

	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
)

/* ---------------- NCC Feedback：跨 Agent / 跨用户的反馈 ----------------
   存储层只做三件事：建一条、按条件列、改处置状态。
   **没有 Update**：反馈只追加，要补充就再建一条（回复）。这不是省事，
   是让"证据"这件事在数据层就成立 —— 换动词也绕不过去（与记录仓的不可变同一套规矩）。
*/

// FeedbackListOpts 列表过滤条件（全部可选）。
//
// 这里是**存储层**的过滤：可见性折成 ViewerID 传进来（与 StateScope 的思路一致 ——
// 存储层不做身份判断，但它也不会忘掉条件，更不会「什么都没给就返回全部」）。
type FeedbackListOpts struct {
	ViewerID string // 谁在看（空 = 匿名：只看得到 public）
	AllSeen  bool   // 管理员视角（看全部）

	AboutKind  string
	AboutRef   string
	OwnerID    string // 目标是我的（收件箱）
	AuthorID   string // 我发的
	Kind       string
	Status     string
	Visibility string // 显式要求某一档（relay 只要 public）
	Unresolved bool   // 只看还没处置的

	Page int
	Size int
}

// FeedbackSummary 聚合结果。
//
// 说清它**不是**什么：这不是排名分、不进任何匹配排序、不发给节点做权重
// （与「评分不对外显示」同一条红线）。它只是"这批反馈长什么样"的一句话。
type FeedbackSummary struct {
	Count        int64            `json:"count"`
	ByKind       map[string]int64 `json:"byKind"`
	ByStatus     map[string]int64 `json:"byStatus"`
	Scored       int64            `json:"scored"`
	ScoreAvg     float64          `json:"scoreAvg"`
	SelfCount    int64            `json:"selfCount"` // 自己给自己的（可能是自己跑完自己记一句）
	PublicCount  int64            `json:"publicCount"`
	PrivateCount int64            `json:"privateCount"`
	Agents       map[string]int64 `json:"agents"` // 哪些 Agent 在说话（跨 Agent 的分布）
	Tags         map[string]int64 `json:"tags"`
	OpenCount    int64            `json:"openCount"`
	FirstAt      *time.Time       `json:"firstAt"`
	LastAt       *time.Time       `json:"lastAt"`
}

// FeedbackRow 一条反馈 + 回复数（列表用；回复不展开成整棵树，避免一次拉太多）。
type FeedbackRow struct {
	model.Feedback
	Replies int64 `json:"replies"`
}

// CreateFeedback 建一条反馈（**没有 Update**，要改就再建一条）。
func (s *Store) CreateFeedback(f *model.Feedback) error {
	return s.DB.Create(f).Error
}

// GetFeedbackByID 按 id 取一条。
func (s *Store) GetFeedbackByID(id string) (*model.Feedback, error) {
	var f model.Feedback
	if err := s.DB.Where("id = ?", strings.TrimSpace(id)).First(&f).Error; err != nil {
		return nil, err
	}
	return &f, nil
}

// FindFeedbackByOrigin 按 (origin, originID) 找 —— relay 的幂等键。
//
// 第二次搬同一条：不算错误，算"已经有了"（与网关摘要的 digest 幂等同一套）。
func (s *Store) FindFeedbackByOrigin(origin, originID string) (*model.Feedback, error) {
	if strings.TrimSpace(origin) == "" || strings.TrimSpace(originID) == "" {
		return nil, gorm.ErrRecordNotFound
	}
	var f model.Feedback
	if err := s.DB.Where("origin = ? AND origin_id = ?", origin, originID).First(&f).Error; err != nil {
		return nil, err
	}
	return &f, nil
}

// fbNormPage 分页归一（1-based、默认 20、封顶 100 —— 与其它列表同一套规矩）。
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

// fbWhere 把过滤条件折成 WHERE。
//
// ⚠️ 每次调用都从**新的** chain 开始：GORM 的 chain 会在 Count / Find 之间累积状态
// （同一条 chain 先 Count 再 Find 会带上 count 的 SELECT），所以这里一律「一次一个」。
//
// 可见性 **fail-closed**：匿名只看 public；登录了就是「public ∪ 我发的 ∪ 发给我的」；
// 管理员才可能看全部（由调用方显式给 AllSeen）。
func (s *Store) fbWhere(o FeedbackListOpts) *gorm.DB {
	q := s.DB.Model(&model.Feedback{}).Where("parent_id = ''")
	if !o.AllSeen {
		if strings.TrimSpace(o.ViewerID) == "" {
			q = q.Where("visibility = ?", model.FbPublic)
		} else {
			q = q.Where("(visibility = ? OR author_id = ? OR owner_id = ?)",
				model.FbPublic, o.ViewerID, o.ViewerID)
		}
	}
	if k := strings.TrimSpace(o.AboutKind); k != "" {
		q = q.Where("about_kind = ?", k)
	}
	if r := strings.TrimSpace(o.AboutRef); r != "" {
		q = q.Where("about_ref = ?", r)
	}
	if v := strings.TrimSpace(o.OwnerID); v != "" {
		q = q.Where("owner_id = ?", v)
	}
	if v := strings.TrimSpace(o.AuthorID); v != "" {
		q = q.Where("author_id = ?", v)
	}
	if v := strings.TrimSpace(o.Kind); v != "" {
		q = q.Where("kind = ?", v)
	}
	if v := strings.TrimSpace(o.Status); v != "" {
		q = q.Where("status = ?", v)
	}
	if v := strings.TrimSpace(o.Visibility); v != "" {
		q = q.Where("visibility = ?", v)
	}
	if o.Unresolved {
		q = q.Where("status IN ?", []string{model.FbOpen, model.FbAck})
	}
	return q
}

// ListFeedback 列反馈（分页；**回复不列在主线里**，它们只在 get 时展开）。
func (s *Store) ListFeedback(o FeedbackListOpts) ([]FeedbackRow, int64, error) {
	page, size := fbNormPage(o.Page, o.Size)
	var total int64
	if err := s.fbWhere(o).Count(&total).Error; err != nil {
		return nil, 0, err
	}
	var rows []model.Feedback
	if err := s.fbWhere(o).Order("created_at DESC").
		Offset((page - 1) * size).Limit(size).Find(&rows).Error; err != nil {
		return nil, 0, err
	}
	out := make([]FeedbackRow, 0, len(rows))
	for i := range rows {
		var n int64
		_ = s.DB.Model(&model.Feedback{}).Where("parent_id = ?", rows[i].ID).Count(&n).Error
		out = append(out, FeedbackRow{Feedback: rows[i], Replies: n})
	}
	return out, total, nil
}

// ListFeedbackReplies 一条反馈下面的回复（按时间正序 —— 对话要按顺序读）。
func (s *Store) ListFeedbackReplies(parentID string) ([]model.Feedback, error) {
	var rows []model.Feedback
	err := s.DB.Where("parent_id = ?", parentID).Order("created_at ASC").Find(&rows).Error
	return rows, err
}

// SetFeedbackStatus 改**处置状态**（不是内容 —— 内容没有 "改" 这条路）。
//
// 谁有资格改由 HTTP 层判（只有目标拥有者 / 管理员）。
func (s *Store) SetFeedbackStatus(id, status string) error {
	res := s.DB.Model(&model.Feedback{}).Where("id = ?", strings.TrimSpace(id)).Update("status", status)
	if res.Error != nil {
		return res.Error
	}
	if res.RowsAffected == 0 {
		return errors.New("没有这条反馈")
	}
	return nil
}

// FeedbackSummaryOf 聚合一批反馈。
//
// 每个聚合都从**新的** chain 开始（理由见 fbWhere）；只取需要的列，不把正文拉回来。
func (s *Store) FeedbackSummaryOf(o FeedbackListOpts) (*FeedbackSummary, error) {
	sum := &FeedbackSummary{
		ByKind:   map[string]int64{},
		ByStatus: map[string]int64{},
		Agents:   map[string]int64{},
		Tags:     map[string]int64{},
	}
	type kv struct {
		K string
		N int64
	}
	var rows []kv
	if err := s.fbWhere(o).Select("kind AS k, COUNT(*) AS n").Group("kind").Scan(&rows).Error; err != nil {
		return nil, err
	}
	for _, r := range rows {
		sum.ByKind[r.K] = r.N
		sum.Count += r.N
	}
	rows = nil
	if err := s.fbWhere(o).Select("status AS k, COUNT(*) AS n").Group("status").Scan(&rows).Error; err != nil {
		return nil, err
	}
	for _, r := range rows {
		sum.ByStatus[r.K] = r.N
		if r.K == model.FbOpen {
			sum.OpenCount = r.N
		}
	}
	rows = nil
	if err := s.fbWhere(o).Select("visibility AS k, COUNT(*) AS n").Group("visibility").Scan(&rows).Error; err != nil {
		return nil, err
	}
	for _, r := range rows {
		switch r.K {
		case model.FbPublic:
			sum.PublicCount = r.N
		case model.FbPrivate:
			sum.PrivateCount = r.N
		}
	}
	// 带分的那部分：只算有分的（score=0 是"没打分"，不是"打了 0 分"）。
	var scored struct {
		N int64
		S int64
	}
	if err := s.fbWhere(o).Where("score > 0").
		Select("COUNT(*) AS n, COALESCE(SUM(score),0) AS s").Scan(&scored).Error; err != nil {
		return nil, err
	}
	sum.Scored = scored.N
	if scored.N > 0 {
		sum.ScoreAvg = float64(scored.S) / float64(scored.N)
	}
	// 自己给自己记的 —— 列出来只是为了让读的人能自己把它扣掉。
	if err := s.fbWhere(o).Where("owner_id != '' AND owner_id = author_id").
		Count(&sum.SelfCount).Error; err != nil {
		return nil, err
	}
	rows = nil
	if err := s.fbWhere(o).Select("agent_id AS k, COUNT(*) AS n").Group("agent_id").Scan(&rows).Error; err != nil {
		return nil, err
	}
	for _, r := range rows {
		key := r.K
		if key == "" {
			key = "（人自己）"
		}
		sum.Agents[key] = r.N
	}
	// 标签分布：tags 是 JSON 文本，不做关联表（见 model 注释），在内存里数。
	var tagRows []string
	if err := s.fbWhere(o).Where("tags != ''").Pluck("tags", &tagRows).Error; err != nil {
		return nil, err
	}
	for _, raw := range tagRows {
		var tags []string
		if json.Unmarshal([]byte(raw), &tags) != nil {
			continue
		}
		for _, t := range tags {
			sum.Tags[t]++
		}
	}
	// 首尾时刻：用「取第一条/最后一条」而不是 MIN()/MAX() ——
	// SQLite 把 datetime 当文本返回，直接 Scan 进 time.Time 会报
	// `unsupported Scan, storing driver.Value type string`（踩过）。
	var firstRow, lastRow model.Feedback
	if err := s.fbWhere(o).Order("created_at ASC").Limit(1).Find(&firstRow).Error; err == nil && firstRow.ID != "" {
		t := firstRow.CreatedAt
		sum.FirstAt = &t
	}
	if err := s.fbWhere(o).Order("created_at DESC").Limit(1).Find(&lastRow).Error; err == nil && lastRow.ID != "" {
		t := lastRow.CreatedAt
		sum.LastAt = &t
	}
	return sum, nil
}
