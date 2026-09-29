package store

import (
	"time"

	"github.com/fusedmodel/ncc-registry/model"
)

/* ---------------- NCC Index（节点侧）：接收平台推来的索引 + 本地检索 ---------------- */

// IndexInput 一份推来的索引（平台条目，去掉平台自己的账）。
type IndexInput struct {
	SourceID     string
	OwnerKey     string
	Owner        string
	DisplayName  string
	ProviderKind string
	Region       string
	Kind         string
	Channel      string
	Slug         string
	Ref          string
	Title        string
	Summary      string
	Description  string
	Category     string
	Tags         []string
	Intents      []string
	Languages    []string
	Protocol     string
	Endpoint     string
	Visibility   string
	Status       string
	PushedAt     time.Time
}

// UpsertIndex 收下一份索引。
//
// 幂等键 = SourceID（平台那条索引的 id）：重推是**覆盖**，不是新增 ——
// 否则内网检索会看到同一条索引的十几份历史版本。
func (s *Store) UpsertIndex(in IndexInput) (*model.IndexEntry, error) {
	tags := marshalList(in.Tags)
	intents := marshalList(in.Intents)
	langs := marshalList(in.Languages)
	if in.Visibility == "" {
		in.Visibility = "public"
	}
	if in.Status == "" {
		in.Status = "active"
	}
	fields := map[string]any{
		"owner_key": in.OwnerKey, "owner": in.Owner, "display_name": in.DisplayName,
		"provider_kind": in.ProviderKind, "region": in.Region,
		"kind": in.Kind, "channel": in.Channel, "slug": in.Slug, "ref": in.Ref,
		"title": in.Title, "summary": in.Summary, "description": in.Description,
		"category": in.Category, "tags": tags, "intents": intents, "languages": langs,
		"protocol": in.Protocol, "endpoint": in.Endpoint,
		"visibility": in.Visibility, "status": in.Status,
		"pushed_at": in.PushedAt, "updated_at": time.Now(),
	}
	var existing model.IndexEntry
	err := s.DB.Where("source_id = ?", in.SourceID).First(&existing).Error
	switch {
	case err == nil:
		if e := s.DB.Model(&model.IndexEntry{}).Where("id = ?", existing.ID).
			Updates(fields).Error; e != nil {
			return nil, e
		}
		var out model.IndexEntry
		if e := s.DB.First(&out, "id = ?", existing.ID).Error; e != nil {
			return nil, e
		}
		return &out, nil
	default:
		v := &model.IndexEntry{
			ID: NewID("NI"), SourceID: in.SourceID, OwnerKey: in.OwnerKey,
			Owner: in.Owner, DisplayName: in.DisplayName, ProviderKind: in.ProviderKind,
			Region: in.Region, Kind: in.Kind, Channel: in.Channel, Slug: in.Slug,
			Ref: in.Ref, Title: in.Title, Summary: in.Summary, Description: in.Description,
			Category: in.Category, Tags: tags, Intents: intents, Languages: langs,
			Protocol: in.Protocol, Endpoint: in.Endpoint,
			Visibility: in.Visibility, Status: in.Status, PushedAt: in.PushedAt,
		}
		if e := s.DB.Create(v).Error; e != nil {
			return nil, e
		}
		return v, nil
	}
}

// IndexListOpts 本地检索条件。
type IndexListOpts struct {
	Channel    string // 频道（前缀匹配）
	Kind       string
	Side       string // supply | need
	Q          string
	Region     string
	PublicOnly bool
	Limit      int
}

// ListIndex 列出本节点收到的索引（默认只看 active + public）。
func (s *Store) ListIndex(o IndexListOpts) ([]model.IndexEntry, error) {
	q := s.DB.Model(&model.IndexEntry{})
	if o.PublicOnly {
		q = q.Where("status = ? AND visibility = ?", "active", "public")
	}
	if o.Channel != "" {
		q = q.Where("(channel = ? OR channel LIKE ?)", o.Channel, o.Channel+"/%")
	}
	if o.Kind != "" {
		q = q.Where("kind = ?", o.Kind)
	}
	if o.Side == "need" {
		q = q.Where("kind = ?", model.IndexKindNeed)
	} else if o.Side == "supply" {
		q = q.Where("kind <> ?", model.IndexKindNeed)
	}
	if o.Region != "" {
		q = q.Where("(region LIKE ? OR region IN ?)", "%"+o.Region+"%",
			[]string{"", "全国", "不限", "any", "global", "all"})
	}
	if o.Q != "" {
		like := "%" + o.Q + "%"
		q = q.Where("(title LIKE ? OR summary LIKE ? OR description LIKE ? OR channel LIKE ? OR tags LIKE ? OR intents LIKE ?)",
			like, like, like, like, like, like)
	}
	limit := o.Limit
	if limit < 1 || limit > 200 {
		limit = 40
	}
	var out []model.IndexEntry
	if err := q.Order("updated_at DESC").Limit(limit).Find(&out).Error; err != nil {
		return nil, err
	}
	return out, nil
}

// IndexChannels 本节点的频道聚合（`ncc list channels --from <节点>`）。
type IndexChannelCount struct {
	Channel string `gorm:"column:channel"`
	Entries int64  `gorm:"column:entries"`
}

func (s *Store) IndexChannels() ([]IndexChannelCount, error) {
	var out []IndexChannelCount
	err := s.DB.Model(&model.IndexEntry{}).
		Select("channel, COUNT(*) AS entries").
		Where("status = ? AND visibility = ?", "active", "public").
		Group("channel").Order("entries DESC, channel").Scan(&out).Error
	return out, err
}

// CountIndex 本节点收到的索引数（meta 的 counts 里报出来）。
func (s *Store) CountIndex() (int64, error) {
	var c int64
	err := s.DB.Model(&model.IndexEntry{}).Count(&c).Error
	return c, err
}

// BumpIndexHits 记一次本地召回。
func (s *Store) BumpIndexHits(ids []string) {
	if len(ids) == 0 {
		return
	}
	_ = s.DB.Exec("UPDATE index_entries SET hits = hits + 1 WHERE id IN ?", ids).Error
}
