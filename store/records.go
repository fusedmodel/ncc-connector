package store

import (
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"sort"
	"strings"
	"time"

	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
)

// ============================ 通用记录仓的存取层 ============================
//
// 这一层只做**机械部分**：按 (集合, key) 找、分页、版本、过期、软删、幂等。
// 它不认识 issue / log / kb 这些词 —— 那是**集合声明**里的事（`model.Collection`）。
// 业务语义写在这里，就等于又回到了"加一类内容改一次服务端"。

var (
	// ErrNoCollection 集合不存在（节点上还没人声明过它）。
	ErrNoCollection = errors.New("no_collection")
	// ErrImmutable 这个集合不可变（或只追加），没有改这条路径。
	ErrImmutable = errors.New("immutable")
	// ErrConflict 乐观并发失败：别人先改了。
	ErrConflict = errors.New("conflict")
	// ErrTooMany 记录数到顶了。
	ErrTooMany = errors.New("too_many")
	// ErrNotBuiltin 这个名字不在内置集合表里（`EnsureBuiltinCollection` 只认内置的那几个）。
	ErrNotBuiltin = errors.New("not_builtin")
	// ErrBuiltinKind 内置集合名是保留的，用户不能声明同名的集合。
	ErrBuiltinKind = errors.New("builtin_kind")
)

/* ---------------- 集合 ---------------- */

// UpsertCollection 声明（或更新）一个集合。`kind` 在命名空间内唯一。
func (s *Store) UpsertCollection(c *model.Collection) (*model.Collection, bool, error) {
	old, err := s.GetCollection(c.NamespaceID, c.Kind)
	if err != nil && !errors.Is(err, gorm.ErrRecordNotFound) {
		return nil, false, err
	}
	if old != nil {
		// 只改"契约面"：字段、索引、可变性、上限、可见性、TTL。
		// **不改归属**（命名空间与 kind 是身份），也不动已有记录。
		old.Title = c.Title
		old.Summary = c.Summary
		old.Reason = c.Reason
		old.Mutable = c.Mutable
		old.History = c.History
		old.AppendOnly = c.AppendOnly
		old.Visibility = c.Visibility
		old.MaxBytes = c.MaxBytes
		old.Fields = c.Fields
		old.Index = c.Index
		old.DedupeBy = c.DedupeBy
		old.DefaultTTL = c.DefaultTTL
		old.UpdatedAt = time.Now()
		if err := s.DB.Save(old).Error; err != nil {
			return nil, false, err
		}
		return old, false, nil
	}
	if err := s.DB.Create(c).Error; err != nil {
		return nil, false, err
	}
	return c, true, nil
}

// GetCollection 按 (命名空间, kind) 取集合。
func (s *Store) GetCollection(nsID, kind string) (*model.Collection, error) {
	var c model.Collection
	err := s.DB.Where("namespace_id = ? AND kind = ?", nsID, strings.ToLower(strings.TrimSpace(kind))).
		First(&c).Error
	if err != nil {
		return nil, err
	}
	return &c, nil
}

// ListCollectionsOfNamespace 这个命名空间声明了哪些集合。
func (s *Store) ListCollectionsOfNamespace(nsID string) ([]model.Collection, error) {
	var out []model.Collection
	err := s.DB.Where("namespace_id = ? AND status <> ?", nsID, model.StoreStatusArchived).
		Order("kind asc").Find(&out).Error
	return out, err
}

// SetCollectionStatus 归档 / 恢复集合。
func (s *Store) SetCollectionStatus(id, status string) error {
	return s.DB.Model(&model.Collection{}).Where("id = ?", id).
		Updates(map[string]any{"status": status, "updated_at": time.Now()}).Error
}

// CountRecords 集合里有多少条（不含归档与过期）。
func (s *Store) CountRecords(collectionID string) (int64, error) {
	var n int64
	err := s.DB.Model(&model.Record{}).
		Where("collection_id = ? AND status = ?", collectionID, model.StoreStatusActive).
		Count(&n).Error
	return n, err
}

/* ---------------- 记录 ---------------- */

// RecordInput 写入一条记录要给的字段（**与集合声明无关**：声明校验在 handler 层）。
type RecordInput struct {
	CollectionID string
	NamespaceID  string
	Key          string
	Body         string
	Data         string
	Meta         string
	Tags         string
	Visibility   string
	Status       string
	Source       string
	SearchText   string
	ExpiresAt    *time.Time
	UserID       string
	// ExpectRevision 非 0 时做乐观并发：库里的 revision 与它不等就冲突。
	ExpectRevision int64
	Note           string // 写进历史的备注
	// Immutable：这个集合**没有改这条路**（`mutable=false` 或 `append_only=true`）。
	//
	// 为什么要把这个标志传进来，而不是只在 PUT 那个 handler 里判：
	// 一旦整条不变量只在某一条路由上判，**换个动词就能绕过去** ——
	// 拿 POST 同一个 key 就能把只追加集合刷新成 rev 2（真事，被 e2e 冒烟抓出来的）。
	// 不变量得待在数据层，才不依赖"你从哪个门进来"。
	Immutable bool
}

// UpsertRecord 建或改一条记录。
//
// 返回 (记录, 是否新建, 是否幂等重复, 错误)。不可变集合上第二次写会返回 `ErrImmutable` ——
// 由调用方翻译成 4xx（**不是**静默覆盖）。
func (s *Store) UpsertRecord(in RecordInput) (*model.Record, bool, bool, error) {
	now := time.Now()
	sum := sha256Hex(in.Body)

	cur, err := s.GetRecordByKey(in.CollectionID, in.Key)
	if err != nil && !errors.Is(err, gorm.ErrRecordNotFound) {
		return nil, false, false, err
	}

	if cur == nil {
		n, err := s.CountRecords(in.CollectionID)
		if err != nil {
			return nil, false, false, err
		}
		if n >= model.StoreMaxRecords {
			return nil, false, false, ErrTooMany
		}
		rec := &model.Record{
			ID: NewID("R-"), CollectionID: in.CollectionID, NamespaceID: in.NamespaceID,
			Key: in.Key, Revision: 1, Checksum: sum, Size: int64(len(in.Body)),
			Body: in.Body, Data: in.Data, Meta: in.Meta, Tags: in.Tags,
			Visibility: in.Visibility, Status: in.Status, Source: in.Source,
			SearchText: in.SearchText, ExpiresAt: in.ExpiresAt, LastNote: in.Note,
			CreatedBy: in.UserID, UpdatedBy: in.UserID, CreatedAt: now, UpdatedAt: now,
		}
		if err := s.DB.Create(rec).Error; err != nil {
			// 并发插入同 key：唯一索引挡下，转成冲突
			if isUniqueErr(err) {
				return nil, false, false, ErrConflict
			}
			return nil, false, false, err
		}
		return rec, true, false, nil
	}

	if in.ExpectRevision != 0 && cur.Revision != in.ExpectRevision {
		return nil, false, false, ErrConflict
	}
	// 内容没变就不动版本 —— 反复保存同一个值不该把历史刷满。
	//
	// 这一步排在不可变判断**之前**是有意的：只追加集合收到"一模一样的一条"
	// 应当算幂等重复（重试不报错），而不该报不可变 —— 没有任何东西被改过。
	if cur.Checksum == sum {
		return cur, false, true, nil
	}
	if in.Immutable {
		return nil, false, false, ErrImmutable
	}

	// 历史（只记元数据）。备注跟着**它自己的版本**走：这一行描述正在被换掉的
	// rev N，它的备注就是当初写 rev N 时给的备注（对当前版本则是 rec.LastNote）。
	rev := model.RecordRevision{
		ID: NewID("RV-"), RecordID: cur.ID, Revision: cur.Revision,
		Checksum: cur.Checksum, Size: cur.Size, ChangedBy: in.UserID,
		Note: cur.LastNote, CreatedAt: now,
	}
	if err := s.DB.Create(&rev).Error; err != nil {
		return nil, false, false, err
	}

	updates := map[string]any{
		"revision": cur.Revision + 1, "checksum": sum, "size": int64(len(in.Body)),
		"body": in.Body, "data": in.Data, "meta": in.Meta, "tags": in.Tags,
		"visibility": in.Visibility, "status": in.Status, "source": in.Source,
		"search_text": in.SearchText, "expires_at": in.ExpiresAt, "last_note": in.Note,
		"updated_by": in.UserID, "updated_at": now,
	}
	if err := s.DB.Model(&model.Record{}).Where("id = ?", cur.ID).Updates(updates).Error; err != nil {
		return nil, false, false, err
	}
	got, err := s.GetRecordByID(cur.ID)
	return got, false, false, err
}

// GetRecordByID 按 id 取。
func (s *Store) GetRecordByID(id string) (*model.Record, error) {
	var r model.Record
	if err := s.DB.Where("id = ?", id).First(&r).Error; err != nil {
		return nil, err
	}
	return &r, nil
}

// GetRecordByKey 按 (集合, key) 取。
func (s *Store) GetRecordByKey(collectionID, key string) (*model.Record, error) {
	var r model.Record
	err := s.DB.Where("collection_id = ? AND key = ?", collectionID, key).First(&r).Error
	if err != nil {
		return nil, err
	}
	return &r, nil
}

// RecordListOpts 列表条件（**只认声明过的过滤维度**，见 handler 层的校验）。
type RecordListOpts struct {
	CollectionID    string
	NamespaceIDs    []string // 可读范围
	Public          bool     // 匿名：只看公开
	Q               string
	Prefix          string
	Tags            []string
	Status          string
	All             bool // 管理员：不限命名空间
	IncludeArchived bool
	IncludeExpired  bool
	Indexed         map[string]string // 声明过的字段 → 值（按需过滤）
	Page            int
	Size            int
	Now             time.Time
}

// ListRecords 列记录（分页）。
func (s *Store) ListRecords(o RecordListOpts) ([]model.Record, int64, error) {
	q := s.DB.Model(&model.Record{}).Where("collection_id = ?", o.CollectionID)
	if !o.IncludeArchived {
		q = q.Where("status = ?", model.StoreStatusActive)
	}
	if !o.IncludeExpired {
		q = q.Where("expires_at IS NULL OR expires_at > ?", o.Now)
	}
	if len(o.Tags) > 0 {
		for _, t := range o.Tags {
			// tags 是 JSON 数组文本：用 LIKE 命中 "tag" 这个整体（含引号，避免前缀误命中）
			q = q.Where("tags LIKE ?", fmt.Sprintf("%%%q%%", t))
		}
	}
	if o.Prefix != "" {
		q = q.Where("key LIKE ?", o.Prefix+"%")
	}
	// 声明的字段过滤：走索引表（没声明的字段在表里没有行，过滤不到）
	for f, v := range o.Indexed {
		q = q.Where("id IN (?)", s.DB.Model(&model.RecordIndex{}).
			Select("record_id").
			Where("collection_id = ? AND field = ? AND value = ?", o.CollectionID, f, v))
	}
	if o.Q != "" {
		// **关键词匹配**（空格分隔的每个词都要出现），命中的位置决定排序权重 ——
		// 与 kb 的「关键词加权」同一口径：key 3 / 标签与声明为 `?search` 的字段 2 / 正文 1。
		//
		// 为什么不是整句子串：人找东西时说「登录 超时」，两个词可能隔得很远；
		// 整句 LIKE 一个都找不到，然后使用者会以为「真的没有」。
		// 也不是索引检索、更不是向量检索 —— 接口文案里就这么写，不假装。
		for _, t := range SearchTokens(o.Q) {
			like := "%" + t + "%"
			q = q.Where("key LIKE ? OR tags LIKE ? OR search_text LIKE ? OR body LIKE ?", like, like, like, like)
		}
	}
	if o.Status != "" {
		q = q.Where("status = ?", o.Status)
	}
	if !o.All {
		if o.Public {
			q = q.Where("visibility = ?", "public")
		} else if len(o.NamespaceIDs) > 0 {
			q = q.Where("visibility = ? OR namespace_id IN ?", "public", o.NamespaceIDs)
		} else {
			q = q.Where("visibility = ?", "public")
		}
	}
	var total int64
	if err := q.Count(&total).Error; err != nil {
		return nil, 0, err
	}
	size := o.Size
	if size <= 0 || size > 200 {
		size = 50
	}
	page := o.Page
	if page <= 1 {
		page = 1
	}
	var out []model.Record
	if o.Q != "" {
		// 带关键词时**先把候选集取回，排完序再切片**：
		// 在「已切片的一页」上排序等于没排（最相关的那条可能在第二页）。
		// 代价：候选上限 `StoreSearchCandidates`（超出就按更新时间截断）——
		// 这一点是**明说的取舍**：要全库排序得引入真索引，那是另一个决定。
		cand := make([]model.Record, 0, 64)
		if err := q.Order("updated_at desc").Limit(model.StoreSearchCandidates).Find(&cand).Error; err != nil {
			return nil, 0, err
		}
		rankRecords(cand, o.Q)
		start := (page - 1) * size
		if start > len(cand) {
			start = len(cand)
		}
		end := start + size
		if end > len(cand) {
			end = len(cand)
		}
		return cand[start:end], total, nil
	}
	err := q.Order("updated_at desc").Offset((page - 1) * size).Limit(size).Find(&out).Error
	return out, total, err
}

// SearchTokens 把查询拆成关键词（空格/逗号/常见分隔符），去重、转小写、丢空。
//
// 中英文混排都能用：中文没有词边界，所以按标点与空白切；一个中文词就是一个词。
func SearchTokens(q string) []string {
	fields := strings.FieldsFunc(q, func(r rune) bool {
		switch r {
		case ' ', '\t', '\n', ',', '，', ';', '；', '、', '|', '/', '　':
			return true
		}
		return false
	})
	seen := map[string]bool{}
	out := make([]string, 0, len(fields))
	for _, f := range fields {
		t := strings.ToLower(strings.TrimSpace(f))
		if t == "" || seen[t] {
			continue
		}
		seen[t] = true
		out = append(out, t)
	}
	return out
}

// rankRecords 按命中位置给候选打分并**就地排序**（同分保持传入顺序 = 新的在前）。
//
// 权重：key 3 · 标签与 `?search` 字段（search_text）2 · 正文 1，每个关键词各算一次。
// 与 kb 的 title 3 / summary 2 / content 1 是同一套思路（不是同一张表，所以不是同一组字段）。
func rankRecords(rows []model.Record, q string) {
	tokens := SearchTokens(q)
	if len(tokens) == 0 {
		return
	}
	score := make([]int, len(rows))
	for i := range rows {
		key := strings.ToLower(rows[i].Key)
		tags := strings.ToLower(rows[i].Tags)
		search := strings.ToLower(rows[i].SearchText)
		body := strings.ToLower(rows[i].Body)
		n := 0
		for _, t := range tokens {
			if strings.Contains(key, t) {
				n += 3
			}
			if strings.Contains(tags, t) || strings.Contains(search, t) {
				n += 2
			}
			if strings.Contains(body, t) {
				n++
			}
		}
		score[i] = n
	}
	// 稳定排序：同分时保持传入顺序（updated_at desc）
	idx := make([]int, len(rows))
	for i := range idx {
		idx[i] = i
	}
	sort.SliceStable(idx, func(a, b int) bool { return score[idx[a]] > score[idx[b]] })
	out := make([]model.Record, len(rows))
	for i, k := range idx {
		out[i] = rows[k]
	}
	copy(rows, out)
}

// RecordRevisions 历史（只记元数据）。
func (s *Store) RecordRevisions(recordID string, limit int) ([]model.RecordRevision, error) {
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	var out []model.RecordRevision
	err := s.DB.Where("record_id = ?", recordID).Order("revision desc").Limit(limit).Find(&out).Error
	return out, err
}

// DeleteRecord 删除一条记录（历史与索引一并删掉；归档请走状态位）。
func (s *Store) DeleteRecord(id string) error {
	if err := s.DB.Where("record_id = ?", id).Delete(&model.RecordRevision{}).Error; err != nil {
		return err
	}
	if err := s.DropIndex(id); err != nil {
		return err
	}
	return s.DB.Where("id = ?", id).Delete(&model.Record{}).Error
}

// GcRecords 真正删掉过期的记录（读时已判过期，这只是清垃圾）。
func (s *Store) GcRecords(collectionID string, now time.Time) (int64, error) {
	q := s.DB.Where("expires_at IS NOT NULL AND expires_at <= ?", now)
	if collectionID != "" {
		q = q.Where("collection_id = ?", collectionID)
	}
	var ids []string
	if err := q.Model(&model.Record{}).Pluck("id", &ids).Error; err != nil {
		return 0, err
	}
	if len(ids) == 0 {
		return 0, nil
	}
	if err := s.DB.Where("record_id IN ?", ids).Delete(&model.RecordRevision{}).Error; err != nil {
		return 0, err
	}
	if err := s.DB.Where("record_id IN ?", ids).Delete(&model.RecordIndex{}).Error; err != nil {
		return 0, err
	}
	res := s.DB.Where("id IN ?", ids).Delete(&model.Record{})
	return res.RowsAffected, res.Error
}

// NamespaceIDsOfOwners 这些人名下的命名空间 id（被授权者列表用）。
//
// 单条读走 `stateReadable` → `HasGrant`；**列表**需要的是"范围"，
// 所以这里把 owner 的命名空间展开成一组 id。
func (s *Store) NamespaceIDsOfOwners(ownerIDs []string) ([]string, error) {
	if len(ownerIDs) == 0 {
		return nil, nil
	}
	var ids []string
	err := s.DB.Model(&model.Namespace{}).Where("owner_id IN ?", ownerIDs).Pluck("id", &ids).Error
	return ids, err
}

// SetRecordStatus 改一条记录的状态（归档 / 恢复）。
func (s *Store) SetRecordStatus(id, status, userID string) error {
	return s.DB.Model(&model.Record{}).Where("id = ?", id).
		Updates(map[string]any{"status": status, "updated_by": userID, "updated_at": time.Now()}).Error
}

/* ---------------- 索引（只给声明过的字段） ---------------- */

// SetIndex 重建一条记录的索引行。
//
// `indexed` 是集合声明里的可过滤字段名；`vals` 是这条记录的字段值。
// **只给 indexed 里出现的字段建行** —— 没声明的字段写不进这张表，也就永远过滤不到。
func (s *Store) SetIndex(recordID, collectionID string, indexed []string, vals map[string][]string) error {
	if err := s.DB.Where("record_id = ?", recordID).Delete(&model.RecordIndex{}).Error; err != nil {
		return err
	}
	for _, f := range indexed {
		for _, v := range vals[f] {
			if strings.TrimSpace(v) == "" {
				continue
			}
			row := model.RecordIndex{
				ID: NewID("IX-"), RecordID: recordID, CollectionID: collectionID,
				Field: f, Value: v,
			}
			if err := s.DB.Create(&row).Error; err != nil {
				return err
			}
		}
	}
	return nil
}

// DropIndex 删掉一条记录的索引行（删除记录时用）。
func (s *Store) DropIndex(recordID string) error {
	return s.DB.Where("record_id = ?", recordID).Delete(&model.RecordIndex{}).Error
}

// StoreCounts 集合数与记录数（fsck 与容量规划看）。
func (s *Store) StoreCounts() (int64, int64, error) {
	var cols, recs int64
	if err := s.DB.Model(&model.Collection{}).Where("status <> ?", model.StoreStatusArchived).Count(&cols).Error; err != nil {
		return 0, 0, err
	}
	if err := s.DB.Model(&model.Record{}).Where("status = ?", model.StoreStatusActive).Count(&recs).Error; err != nil {
		return cols, 0, err
	}
	return cols, recs, nil
}

/* ---------------- 小工具 ---------------- */

func sha256Hex(s string) string {
	sum := sha256.Sum256([]byte(s))
	return "sha256:" + hex.EncodeToString(sum[:])
}

func isUniqueErr(err error) bool {
	if err == nil {
		return false
	}
	msg := strings.ToLower(err.Error())
	return strings.Contains(msg, "unique") || strings.Contains(msg, "constraint")
}

// JSONList 把字符串切片编码成存进库的 JSON（空 = `[]`，不是空串）。
func JSONList(v []string) string {
	if len(v) == 0 {
		return "[]"
	}
	b, err := json.Marshal(v)
	if err != nil {
		return "[]"
	}
	return string(b)
}
