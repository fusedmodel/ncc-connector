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

/* ---------------- NCC State：kb / mem / ckpt 的数据访问 ----------------
   三样状态共用一套「可见范围」写法（我的命名空间 ∪ 被授权者；管理员 all=1 才看全），
   但读写路径各按自己的形状来：
     kb    内容进库 + 每次写入留一版
     mem   (命名空间, subject, key) 唯一，写即更新，读时判过期
     ckpt  字节进 blob，元数据进库，不可变 + 血缘
*/

/* ============================ 可见范围 ============================ */

// StateScope 三样状态共用的可见范围。
//
// **fail-closed**：什么都没给就查不到东西（别把"没给条件"理解成"看全部"）。
type StateScope struct {
	NamespaceIDs  []string
	GrantedOwners []string
	All           bool
}

// publicCol 给"有公开档"的资源（kb / ckpt）：非空时把 `visibility = public` 并进
// 可见范围 —— 公开项**必须在列表里也出现**，否则会出现"按引用取得到、列表里看不到"
// 的怪现象。mem 传空串（记忆没有公开档，这一档它不该有）。
func (sc StateScope) apply(q *gorm.DB, nsCol, ownerCol, publicCol string) *gorm.DB {
	if sc.All {
		return q
	}
	conds := []string{}
	args := []any{}
	if len(sc.NamespaceIDs) > 0 {
		conds = append(conds, nsCol+" IN ?")
		args = append(args, sc.NamespaceIDs)
	}
	if len(sc.GrantedOwners) > 0 {
		conds = append(conds, ownerCol+" IN ?")
		args = append(args, sc.GrantedOwners)
	}
	if publicCol != "" {
		conds = append(conds, publicCol+" = ?")
		args = append(args, model.KbPublic)
	}
	if len(conds) == 0 {
		return q.Where("1 = 0")
	}
	return q.Where("("+strings.Join(conds, " OR ")+")", args...)
}

/* ============================ 知识库 kb ============================ */

// KbRow 文档 + 命名空间/归属者摘要。
type KbRow struct {
	model.KbDoc
	NsSlug    string `gorm:"column:ns_slug"`
	NsName    string `gorm:"column:ns_name"`
	OwnerID   string `gorm:"column:owner_id"`
	OwnerName string `gorm:"column:owner_name"`
}

// Ref 规范引用 @命名空间/slug。
func (r *KbRow) Ref() string { return "@" + r.NsSlug + "/" + r.Slug }

// KbInput 写入一篇文档。
type KbInput struct {
	NamespaceID string
	Slug        string
	Title       string
	Kind        string
	Format      string
	Summary     string
	Tags        []string
	Visibility  string
	Source      string
	Content     string
	Checksum    string
	Note        string
	AuthorID    string
	AuthorName  string
	// NewSlug 与 Slug 不同时表示改名（文档的引用会变，历史留在旧 id 上）。
	Rename bool
}

// KbListOpts 检索条件。
type KbListOpts struct {
	StateScope
	NsSlug      string
	Kind        string
	Tag         string
	Status      string
	TitleOnly   bool
	Q           string
	IncludeArch bool
	Page, Size  int
	Limit       int
	// Rank 为真时按命中打分排序（搜索用）；否则按更新时间倒序（列表用）。
	Rank bool
	// IncludePublic 把"公开"并进可见范围。
	//
	// KB 有公开档（`--public`）：公开是**语义层**的事，但列表也得能看见它 ——
	// 否则会出现"按引用取得到、列表里永远找不到"的怪现象。匿名调用者没有命名空间
	// 范围，这一档就是它唯一的可见面。
	IncludePublic bool
}

func (s *Store) kbBase() *gorm.DB {
	return s.DB.Table("kb_docs k").Joins("JOIN namespaces ns ON ns.id = k.namespace_id")
}

func (s *Store) kbQuery() *gorm.DB {
	return s.kbBase().
		Select(`k.*, ns.slug AS ns_slug, ns.name AS ns_name,
			ns.owner_id AS owner_id, u.name AS owner_name`).
		Joins("LEFT JOIN users u ON u.id = ns.owner_id")
}

func (o KbListOpts) apply(q *gorm.DB) *gorm.DB {
	publicCol := ""
	if o.IncludePublic {
		publicCol = "k.visibility"
	}
	q = o.StateScope.apply(q, "k.namespace_id", "ns.owner_id", publicCol)
	if o.NsSlug != "" {
		q = q.Where("ns.slug = ?", o.NsSlug)
	}
	if o.Kind != "" {
		q = q.Where("k.kind = ?", o.Kind)
	}
	if o.Tag != "" {
		q = q.Where("k.tags LIKE ?", `%"`+o.Tag+`"%`)
	}
	switch {
	case o.Status != "":
		q = q.Where("k.status = ?", o.Status)
	case !o.IncludeArch:
		q = q.Where("k.status = ?", model.KbActive)
	}
	if o.Q != "" {
		q = q.Where("(k.title LIKE ? OR k.summary LIKE ? OR k.search_text LIKE ?)",
			"%"+strings.ToLower(o.Q)+"%", "%"+strings.ToLower(o.Q)+"%", "%"+strings.ToLower(o.Q)+"%")
	}
	return q
}

// UpsertKbDoc 建/改一篇文档：改就是新版本（追加 KbRevision）。
func (s *Store) UpsertKbDoc(in KbInput) (*KbRow, bool, error) {
	var existing model.KbDoc
	created := false
	err := s.DB.Where("namespace_id = ? AND slug = ?", in.NamespaceID, in.Slug).First(&existing).Error
	switch {
	case errors.Is(err, gorm.ErrRecordNotFound):
		created = true
	case err != nil:
		return nil, false, err
	}
	tags, _ := json.Marshal(in.Tags)
	search := strings.ToLower(in.Title + "\n" + in.Summary + "\n" + in.Content)

	if created {
		rev := int64(1)
		row := &model.KbDoc{
			ID: NewID("KD"), NamespaceID: in.NamespaceID, Slug: in.Slug, Title: in.Title,
			Kind: in.Kind, Format: in.Format, Summary: in.Summary, Tags: string(tags),
			Visibility: in.Visibility, Status: model.KbActive, Revision: rev,
			Content: in.Content, Checksum: in.Checksum, Size: int64(len(in.Content)),
			Source: in.Source, SearchText: search, CreatedBy: in.AuthorID, UpdatedBy: in.AuthorID,
		}
		if err := s.DB.Create(row).Error; err != nil {
			return nil, false, err
		}
		if err := s.DB.Create(&model.KbRevision{
			ID: NewID("KR"), DocID: row.ID, Revision: rev, Title: in.Title,
			Content: in.Content, Checksum: in.Checksum, Size: int64(len(in.Content)),
			Note: kbFirstNote(in.Note), AuthorID: in.AuthorID, Author: in.AuthorName,
		}).Error; err != nil {
			return nil, false, err
		}
		got, err := s.GetKbDoc(row.ID)
		return got, true, err
	}

	rev := existing.Revision + 1
	upd := map[string]any{
		"title": in.Title, "kind": in.Kind, "format": in.Format, "summary": in.Summary,
		"tags": string(tags), "visibility": in.Visibility, "content": in.Content,
		"checksum": in.Checksum, "size": int64(len(in.Content)), "search_text": search,
		"revision": rev, "updated_by": in.AuthorID, "status": model.KbActive,
	}
	if in.Source != "" {
		upd["source"] = in.Source
	}
	if err := s.DB.Model(&model.KbDoc{}).Where("id = ?", existing.ID).Updates(upd).Error; err != nil {
		return nil, false, err
	}
	if err := s.DB.Create(&model.KbRevision{
		ID: NewID("KR"), DocID: existing.ID, Revision: rev, Title: in.Title,
		Content: in.Content, Checksum: in.Checksum, Size: int64(len(in.Content)),
		Note: in.Note, AuthorID: in.AuthorID, Author: in.AuthorName,
	}).Error; err != nil {
		return nil, false, err
	}
	got, err := s.GetKbDoc(existing.ID)
	return got, false, err
}

func kbFirstNote(n string) string {
	if strings.TrimSpace(n) == "" {
		return "初版"
	}
	return n
}

// ListKbDocs 检索。`Rank` 时用**命中打分**排序（标题权重最高），否则按更新时间。
func (s *Store) ListKbDocs(o KbListOpts) ([]KbRow, int64, error) {
	var total int64
	if err := o.apply(s.kbBase()).Count(&total).Error; err != nil {
		return nil, 0, err
	}
	q := o.apply(s.kbQuery()).Order("k.updated_at DESC, k.id DESC")
	if o.Size > 0 {
		page := o.Page
		if page < 1 {
			page = 1
		}
		q = q.Offset((page - 1) * o.Size).Limit(o.Size)
	} else if o.Limit > 0 {
		q = q.Limit(o.Limit)
	}
	var rows []KbRow
	if err := q.Scan(&rows).Error; err != nil {
		return nil, 0, err
	}
	if o.Rank && o.Q != "" {
		rankKbRows(rows, o.Q)
	}
	return rows, total, nil
}

// rankKbRows 关键词打分：标题 3 分 / 摘要 2 分 / 正文 1 分，正文里的**命中次数**也计入。
//
// 这是**关键词检索**，不是向量检索 —— 数据量小的时候足够，前提是诚实地说清楚
// （见 PRD：换成向量/全文索引是下一步，不是"已经支持"）。
func rankKbRows(rows []KbRow, q string) {
	terms := strings.Fields(strings.ToLower(q))
	score := func(r *KbRow) int {
		t, sm, c := strings.ToLower(r.Title), strings.ToLower(r.Summary), strings.ToLower(r.Content)
		n := 0
		for _, term := range terms {
			n += 3 * strings.Count(t, term)
			n += 2 * strings.Count(sm, term)
			n += strings.Count(c, term)
		}
		return n
	}
	// 简单插入排序：结果集有分页上限，不值得引 sort。
	for i := 1; i < len(rows); i++ {
		for j := i; j > 0 && score(&rows[j]) > score(&rows[j-1]); j-- {
			rows[j], rows[j-1] = rows[j-1], rows[j]
		}
	}
}

// GetKbDoc 按行 id 或 `@ns/slug` / slug 取。
func (s *Store) GetKbDoc(idOrRef string) (*KbRow, error) {
	q := s.kbQuery()
	if strings.HasPrefix(idOrRef, "@") {
		ns, slug := splitRef(idOrRef)
		q = q.Where("ns.slug = ? AND k.slug = ?", ns, slug)
	} else if strings.Contains(idOrRef, "/") {
		ns, slug := splitRef(idOrRef)
		q = q.Where("ns.slug = ? AND k.slug = ?", ns, slug)
	} else {
		q = q.Where("k.id = ? OR k.slug = ?", idOrRef, idOrRef)
	}
	var row KbRow
	if err := q.Limit(1).Scan(&row).Error; err != nil {
		return nil, err
	}
	if row.ID == "" {
		return nil, gorm.ErrRecordNotFound
	}
	return &row, nil
}

// KbRevisions 版本历史（正序）。
func (s *Store) KbRevisions(docID string) ([]model.KbRevision, error) {
	var out []model.KbRevision
	err := s.DB.Where("doc_id = ?", docID).Order("revision ASC").Find(&out).Error
	return out, err
}

// KbSetStatus 归档/恢复。
func (s *Store) KbSetStatus(docID, status string) error {
	return s.DB.Model(&model.KbDoc{}).Where("id = ?", docID).Update("status", status).Error
}

// DeleteKbDoc 删除（连同历史）。KB 是数据不是审计，归属者有权删掉。
func (s *Store) DeleteKbDoc(docID string) error {
	if err := s.DB.Where("doc_id = ?", docID).Delete(&model.KbRevision{}).Error; err != nil {
		return err
	}
	return s.DB.Where("id = ?", docID).Delete(&model.KbDoc{}).Error
}

// CountKbDocs 计数（/api/meta 用）。
func (s *Store) CountKbDocs() (int64, error) {
	var n int64
	err := s.DB.Model(&model.KbDoc{}).Where("status = ?", model.KbActive).Count(&n).Error
	return n, err
}

/* ============================ 记忆 mem ============================ */

// MemRow 记忆 + 命名空间摘要。
type MemRow struct {
	model.MemEntry
	NsSlug  string `gorm:"column:ns_slug"`
	NsName  string `gorm:"column:ns_name"`
	OwnerID string `gorm:"column:owner_id"`
}

// MemInput 写一条记忆（同 (命名空间, subject, key) 即更新）。
type MemInput struct {
	NamespaceID string
	Subject     string
	Key         string
	Value       string
	Kind        string
	Tags        []string
	Source      string
	Confidence  int64
	Pinned      bool
	TTLDays     int
	AuthorID    string
}

// MemListOpts 记忆检索。
type MemListOpts struct {
	StateScope
	Subject        string
	Prefix         string
	Kind           string
	Tag            string
	Source         string
	IncludeExpired bool
	PinnedOnly     bool
	Limit          int
	Now            time.Time
}

func (s *Store) memBase() *gorm.DB {
	return s.DB.Table("mem_entries m").Joins("JOIN namespaces ns ON ns.id = m.namespace_id")
}

func (s *Store) memQuery() *gorm.DB {
	return s.memBase().
		Select(`m.*, ns.slug AS ns_slug, ns.name AS ns_name, ns.owner_id AS owner_id`)
}

func (o MemListOpts) apply(q *gorm.DB) *gorm.DB {
	q = o.StateScope.apply(q, "m.namespace_id", "ns.owner_id", "")
	if o.Subject != "" {
		q = q.Where("m.subject = ?", o.Subject)
	}
	if o.Prefix != "" {
		q = q.Where("m.key LIKE ?", o.Prefix+"%")
	}
	if o.Kind != "" {
		q = q.Where("m.kind = ?", o.Kind)
	}
	if o.Tag != "" {
		q = q.Where("m.tags LIKE ?", `%"`+o.Tag+`"%`)
	}
	if o.Source != "" {
		q = q.Where("m.source = ?", o.Source)
	}
	if o.PinnedOnly {
		q = q.Where("m.pinned = ?", true)
	}
	if !o.IncludeExpired {
		now := o.Now
		if now.IsZero() {
			now = time.Now()
		}
		// 读时判过期：`expires_at IS NULL` 表示永不过期，否则必须还没到点。
		q = q.Where("(m.expires_at IS NULL OR m.expires_at > ?)", now.UTC())
	}
	return q
}

// UpsertMemEntry 写一条记忆（同键即更新，Revision+1）。
func (s *Store) UpsertMemEntry(in MemInput) (*MemRow, bool, error) {
	tags, _ := json.Marshal(in.Tags)
	var exp *time.Time
	if in.TTLDays > 0 {
		t := time.Now().Add(time.Duration(in.TTLDays) * 24 * time.Hour).UTC()
		exp = &t
	}
	var existing model.MemEntry
	err := s.DB.Where("namespace_id = ? AND subject = ? AND key = ?", in.NamespaceID, in.Subject, in.Key).
		First(&existing).Error
	switch {
	case errors.Is(err, gorm.ErrRecordNotFound):
		row := &model.MemEntry{
			ID: NewID("ME"), NamespaceID: in.NamespaceID, Subject: in.Subject, Key: in.Key,
			Value: in.Value, Kind: in.Kind, Tags: string(tags), Source: in.Source,
			Confidence: in.Confidence, Pinned: in.Pinned, Revision: 1, ExpiresAt: exp,
			CreatedBy: in.AuthorID, UpdatedBy: in.AuthorID,
		}
		if err := s.DB.Create(row).Error; err != nil {
			return nil, false, err
		}
		got, err := s.GetMemEntry(row.ID)
		return got, true, err
	case err != nil:
		return nil, false, err
	}
	upd := map[string]any{
		"value": in.Value, "kind": in.Kind, "tags": string(tags), "source": in.Source,
		"confidence": in.Confidence, "revision": existing.Revision + 1,
		"updated_by": in.AuthorID,
	}
	// TTL：0 = 不过期（显式清空），>0 = 从**现在**重新计时。
	if in.TTLDays > 0 {
		upd["expires_at"] = exp
	} else {
		upd["expires_at"] = nil
	}
	// Pinned 只能被显式设为 true 或保持原样（不因为一次普通写入被意外取消）。
	if in.Pinned || !existing.Pinned {
		upd["pinned"] = in.Pinned
	}
	if err := s.DB.Model(&model.MemEntry{}).Where("id = ?", existing.ID).Updates(upd).Error; err != nil {
		return nil, false, err
	}
	got, err := s.GetMemEntry(existing.ID)
	return got, false, err
}

// ListMemEntries 列记忆。
func (s *Store) ListMemEntries(o MemListOpts) ([]MemRow, int64, error) {
	var total int64
	if err := o.apply(s.memBase()).Count(&total).Error; err != nil {
		return nil, 0, err
	}
	q := o.apply(s.memQuery()).
		Order("m.pinned DESC, m.updated_at DESC, m.id DESC")
	if o.Limit > 0 {
		q = q.Limit(o.Limit)
	}
	var rows []MemRow
	if err := q.Scan(&rows).Error; err != nil {
		return nil, 0, err
	}
	return rows, total, nil
}

// GetMemEntry 按 id 或 (subject,key) 取一条。
func (s *Store) GetMemEntry(idOrKey string) (*MemRow, error) {
	var row MemRow
	err := s.memQuery().Where("m.id = ? OR m.key = ?", idOrKey, idOrKey).Limit(1).Scan(&row).Error
	if err != nil {
		return nil, err
	}
	if row.ID == "" {
		return nil, gorm.ErrRecordNotFound
	}
	return &row, nil
}

// GetMemByKey 按 (命名空间, subject, key) 精确取（Agent 读记忆的主路径）。
func (s *Store) GetMemByKey(nsID, subject, key string, now time.Time) (*MemRow, error) {
	var row MemRow
	q := s.memQuery().Where("m.namespace_id = ? AND m.subject = ? AND m.key = ?", nsID, subject, key)
	if !now.IsZero() {
		q = q.Where("(m.expires_at IS NULL OR m.expires_at > ?)", now.UTC())
	}
	if err := q.Limit(1).Scan(&row).Error; err != nil {
		return nil, err
	}
	if row.ID == "" {
		return nil, gorm.ErrRecordNotFound
	}
	return &row, nil
}

// DeleteMemEntry 删除一条记忆。
func (s *Store) DeleteMemEntry(id string) error {
	return s.DB.Where("id = ?", id).Delete(&model.MemEntry{}).Error
}

// GcMemEntries 真正删掉过期条目（读时已经判过过期，这里只是清垃圾）。
func (s *Store) GcMemEntries(nsID string, now time.Time) (int64, error) {
	q := s.DB.Where("expires_at IS NOT NULL AND expires_at <= ?", now.UTC())
	if nsID != "" {
		q = q.Where("namespace_id = ?", nsID)
	}
	res := q.Delete(&model.MemEntry{})
	return res.RowsAffected, res.Error
}

// CountMemEntries 计数（含过期：它问的是"库里有多少"）。
func (s *Store) CountMemEntries() (int64, error) {
	var n int64
	err := s.DB.Model(&model.MemEntry{}).Count(&n).Error
	return n, err
}

/* ============================ 检查点 ckpt ============================ */

// CkptRow 检查点 + 命名空间摘要。
type CkptRow struct {
	model.Checkpoint
	NsSlug  string `gorm:"column:ns_slug"`
	NsName  string `gorm:"column:ns_name"`
	OwnerID string `gorm:"column:owner_id"`
}

// CkptInput 创建一个检查点（元数据；字节另走 PutCheckpointBytes）。
type CkptInput struct {
	NamespaceID    string
	SubjectRef     string
	SubjectVersion string
	Name           string
	Label          string
	Step           int64
	Summary        string
	Tags           []string
	Visibility     string
	Parent         string
	Digest         string
	Size           int64
	MediaType      string
	Meta           string
	AuthorID       string
}

// CkptListOpts 检索条件。
type CkptListOpts struct {
	StateScope
	SubjectRef string
	Label      string
	Tag        string
	Status     string
	Name       string
	Limit      int
	// IncludePublic 见 KbListOpts：检查点也有公开档（公开 = 谁拿得到节点就能取）。
	IncludePublic bool
}

func (s *Store) ckptBase() *gorm.DB {
	return s.DB.Table("checkpoints c").Joins("JOIN namespaces ns ON ns.id = c.namespace_id")
}

func (s *Store) ckptQuery() *gorm.DB {
	return s.ckptBase().
		Select(`c.*, ns.slug AS ns_slug, ns.name AS ns_name, ns.owner_id AS owner_id`)
}

func (o CkptListOpts) apply(q *gorm.DB) *gorm.DB {
	publicCol := ""
	if o.IncludePublic {
		publicCol = "c.visibility"
	}
	q = o.StateScope.apply(q, "c.namespace_id", "ns.owner_id", publicCol)
	if o.SubjectRef != "" {
		q = q.Where("c.subject_ref = ?", o.SubjectRef)
	}
	if o.Label != "" {
		q = q.Where("c.label = ?", o.Label)
	}
	if o.Tag != "" {
		q = q.Where("c.tags LIKE ?", `%"`+o.Tag+`"%`)
	}
	if o.Name != "" {
		q = q.Where("c.name LIKE ?", "%"+o.Name+"%")
	}
	switch {
	case o.Status != "":
		q = q.Where("c.status = ?", o.Status)
	default:
		q = q.Where("c.status = ?", model.CkptActive)
	}
	return q
}

// CreateCheckpoint 创建（不可变：没有 update）。
func (s *Store) CreateCheckpoint(in CkptInput) (*CkptRow, error) {
	tags, _ := json.Marshal(in.Tags)
	meta := in.Meta
	if strings.TrimSpace(meta) == "" {
		meta = "{}"
	}
	mt := in.MediaType
	if mt == "" {
		mt = "application/octet-stream"
	}
	row := &model.Checkpoint{
		ID: NewID("CK"), NamespaceID: in.NamespaceID, SubjectRef: in.SubjectRef,
		SubjectVersion: in.SubjectVersion, Name: in.Name, Label: in.Label, Step: in.Step,
		Summary: in.Summary, Tags: string(tags), Visibility: in.Visibility,
		Status: model.CkptActive, Parent: in.Parent, Digest: in.Digest, Size: in.Size,
		MediaType: mt, Meta: meta, CreatedBy: in.AuthorID,
	}
	if err := s.DB.Create(row).Error; err != nil {
		return nil, err
	}
	got, err := s.GetCheckpoint(row.ID)
	return got, err
}

// SetCheckpointObject 字节落盘后回填对象名（服务端核对过摘要才调它）。
func (s *Store) SetCheckpointObject(id, objectKey string, size int64) error {
	return s.DB.Model(&model.Checkpoint{}).Where("id = ?", id).
		Updates(map[string]any{"object_key": objectKey, "size": size}).Error
}

// ListCheckpoints 列表（新的在前）。
func (s *Store) ListCheckpoints(o CkptListOpts) ([]CkptRow, int64, error) {
	var total int64
	if err := o.apply(s.ckptBase()).Count(&total).Error; err != nil {
		return nil, 0, err
	}
	q := o.apply(s.ckptQuery()).Order("c.created_at DESC, c.id DESC")
	if o.Limit > 0 {
		q = q.Limit(o.Limit)
	}
	var rows []CkptRow
	if err := q.Scan(&rows).Error; err != nil {
		return nil, 0, err
	}
	return rows, total, nil
}

// GetCheckpoint 按 id 或名称取。
func (s *Store) GetCheckpoint(idOrName string) (*CkptRow, error) {
	var row CkptRow
	err := s.ckptQuery().Where("c.id = ? OR c.name = ?", idOrName, idOrName).
		Order("c.created_at DESC").Limit(1).Scan(&row).Error
	if err != nil {
		return nil, err
	}
	if row.ID == "" {
		return nil, gorm.ErrRecordNotFound
	}
	return &row, nil
}

// CheckpointLineage 从某个点沿 parent 回溯（含自身，最新在前）。
func (s *Store) CheckpointLineage(id string) ([]CkptRow, error) {
	var out []CkptRow
	seen := map[string]bool{}
	cur := id
	for i := 0; i < 64 && cur != ""; i++ { // 上限防环（数据是人写的，别信它一定无环）
		if seen[cur] {
			break
		}
		seen[cur] = true
		row, err := s.GetCheckpoint(cur)
		if err != nil {
			break
		}
		out = append(out, *row)
		cur = row.Parent
	}
	return out, nil
}

// PruneCheckpoints 每个 subject 只留最新 keep 个（其余标 pruned 并返回要删的对象名）。
//
// `keep <= 0` 无效（那是"全删"，请用 DeleteCheckpoint 明说）。
func (s *Store) PruneCheckpoints(subjectRef string, keep int) ([]string, error) {
	if keep <= 0 {
		return nil, errors.New("keep 必须大于 0（要全删请逐个 DeleteCheckpoint）")
	}
	q := s.DB.Model(&model.Checkpoint{}).Where("status = ?", model.CkptActive)
	if subjectRef != "" {
		q = q.Where("subject_ref = ?", subjectRef)
	}
	var rows []model.Checkpoint
	if err := q.Order("created_at DESC, id DESC").Find(&rows).Error; err != nil {
		return nil, err
	}
	var doomed []string
	for i, r := range rows {
		if i < keep {
			continue
		}
		if err := s.DB.Model(&model.Checkpoint{}).Where("id = ?", r.ID).
			Update("status", model.CkptPruned).Error; err != nil {
			return doomed, err
		}
		if r.ObjectKey != "" {
			doomed = append(doomed, r.ObjectKey)
		}
	}
	return doomed, nil
}

// DeleteCheckpoint 删一个（元数据也删；要"留个记录"请用 Prune）。
func (s *Store) DeleteCheckpoint(id string) (string, error) {
	row, err := s.GetCheckpoint(id)
	if err != nil {
		return "", err
	}
	if err := s.DB.Where("id = ?", row.ID).Delete(&model.Checkpoint{}).Error; err != nil {
		return "", err
	}
	return row.ObjectKey, nil
}

// CountCheckpoints 计数。
func (s *Store) CountCheckpoints() (int64, error) {
	var n int64
	err := s.DB.Model(&model.Checkpoint{}).Where("status = ?", model.CkptActive).Count(&n).Error
	return n, err
}

/* ---------------- 小工具 ---------------- */

// splitRef 把 `@ns/slug` 或 `ns/slug` 拆开。
func splitRef(ref string) (string, string) {
	r := strings.TrimPrefix(strings.TrimSpace(ref), "@")
	parts := strings.SplitN(r, "/", 2)
	if len(parts) != 2 {
		return r, ""
	}
	return parts[0], parts[1]
}

// StateCounts 三样状态的计数（/api/meta 用）。
func (s *Store) StateCounts() (kb, mem, ckpt int64, err error) {
	if kb, err = s.CountKbDocs(); err != nil {
		return
	}
	if mem, err = s.CountMemEntries(); err != nil {
		return
	}
	ckpt, err = s.CountCheckpoints()
	return
}

// fmtRef 拼规范引用（供 httpapi 复用）。
func fmtRef(ns, slug string) string { return fmt.Sprintf("@%s/%s", ns, slug) }
