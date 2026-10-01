// Package store 数据访问层（GORM + glebarez/sqlite 纯 Go 驱动）。
//
// 单文件 SQLite：内网托管节点的部署形态通常是「一个二进制 + 一个数据目录」，
// 所以不引入外部数据库依赖（与平台侧默认驱动一致）。
package store

import (
	"crypto/rand"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
	"strings"
	"time"

	"github.com/glebarez/sqlite"
	"gorm.io/gorm"
	"gorm.io/gorm/clause"
	"gorm.io/gorm/logger"

	"github.com/fusedmodel/ncc-registry/model"
)

var reNonSlug = regexp.MustCompile(`[^a-z0-9]+`)

// Slugify 转成小写连字符 slug（中文等非 ASCII 字符会被丢掉，须调用方兜底）。
func Slugify(s string) string {
	out := reNonSlug.ReplaceAllString(strings.ToLower(strings.TrimSpace(s)), "-")
	out = strings.Trim(out, "-")
	if len(out) > 48 {
		out = out[:48]
	}
	if out == "" {
		return "x"
	}
	return out
}

var reSlugOK = regexp.MustCompile(`^[a-z0-9][a-z0-9._-]{1,63}$`)

// ValidSlug 校验规范 slug（小写字母数字与 - _ .，2..64 位，首字符是字母数字）。
func ValidSlug(s string) bool { return reSlugOK.MatchString(s) }

// RandHex 返回 n 字节随机数的 hex 串。
func RandHex(n int) string {
	buf := make([]byte, n)
	if _, err := rand.Read(buf); err != nil {
		panic(err)
	}
	return hex.EncodeToString(buf)
}

const b36chars = "0123456789abcdefghijklmnopqrstuvwxyz"

func base36(n int64) string {
	if n == 0 {
		return "0"
	}
	var sb []byte
	for n > 0 {
		sb = append([]byte{b36chars[n%36]}, sb...)
		n /= 36
	}
	return string(sb)
}

// NewID 生成带前缀的短 ID，如 A-<ts36>-<rand>。
func NewID(prefix string) string {
	return fmt.Sprintf("%s-%s-%s", prefix, base36(time.Now().UnixMilli()), RandHex(4))
}

// Store 数据访问句柄。
type Store struct {
	DB *gorm.DB
}

// Open 打开（必要时创建）SQLite 库并自动迁移。
func Open(path string) (*Store, error) {
	db, err := gorm.Open(sqlite.Open(path), &gorm.Config{
		Logger: logger.Default.LogMode(logger.Silent),
	})
	if err != nil {
		return nil, fmt.Errorf("打开 sqlite %s: %w", path, err)
	}
	// SQLite 是单写者：连接池 >1 会让「先查后写」的路径出现竞态。
	if sqlDB, err := db.DB(); err == nil {
		sqlDB.SetMaxOpenConns(1)
		_, _ = sqlDB.Exec("PRAGMA journal_mode=WAL;")
		_, _ = sqlDB.Exec("PRAGMA busy_timeout=5000;")
	}
	if err := db.AutoMigrate(
		&model.User{}, &model.Namespace{}, &model.NsMember{}, &model.ApiKey{},
		&model.Artifact{}, &model.HostedNode{}, &model.NodeLink{},
		// NCC Agent Share：点到点的 Agent 名片（包字节在 blob，token 只存 sha256）
		&model.AgentCard{},
		// 远程执行（云电脑）：一次任务一行，日志落在工作目录里
		&model.ExecRun{},
		// 连接通道（通信基础设施）：一段会话一行，工作目录是文件面的边界
		&model.Conn{},
		&model.ClusterWorker{}, &model.ArtifactAdvert{}, &model.ReplicaTarget{},
		&model.Grant{}, &model.AccessTicket{},
		// NCC Config：托管配置（条目 + 版本历史）
		&model.ConfigEntry{}, &model.ConfigRevision{},
		// 节点治理：机器管理凭据 / 审计 / 制品分享链接
		&model.AdminKey{}, &model.AuditLog{}, &model.ArtifactShare{},
		// NCC Trace：运行轨迹（文档不可变）+ 评测标注（只追加）
		&model.Trace{}, &model.TraceLabel{},
		// NCC State：知识库 / 记忆 / 检查点（三样状态住节点，包只声明要什么）
		&model.KbDoc{}, &model.KbRevision{}, &model.MemEntry{}, &model.Checkpoint{},
		// NCC Store：通用记录仓（集合 = 声明，记录 = 数据）。kb/mem/ckpt/trace 的**机械部分**
		// 会逐步收敛到这里；新内容类型（issue / log / …）声明一个集合即可，不改服务端。
		&model.Collection{}, &model.Record{}, &model.RecordRevision{}, &model.RecordIndex{},
		// NCC Index：平台推来的索引副本（内网本地检索用；幂等键 = 平台的索引 id）
		&model.IndexEntry{},
	); err != nil {
		return nil, fmt.Errorf("迁移表结构失败: %w", err)
	}
	return &Store{DB: db}, nil
}

/* ---------------- 用户 / 命名空间 ---------------- */

// CreateUser 建账号。
//
// **本节点的第一个用户自动成为管理员**：内网托管节点的部署形态是「谁先装谁是主人」，
// 不该再引入一套外部账号系统去决定谁管这台机器。之后想给机器一把管理凭据，用
// CreateAdminKey（管理员注册时会自动签发一份）。
func (s *Store) CreateUser(name, email, passHash string) (*model.User, error) {
	u := &model.User{ID: NewID("U"), Name: name, Email: strings.ToLower(email), PassHash: passHash, Plan: model.PlanFree}
	var existing int64
	if err := s.DB.Model(&model.User{}).Count(&existing).Error; err != nil {
		return nil, err
	}
	u.IsAdmin = existing == 0
	if err := s.DB.Create(u).Error; err != nil {
		return nil, err
	}
	return u, nil
}

func (s *Store) FindUserByEmail(email string) (*model.User, error) {
	var u model.User
	if err := s.DB.Where("email = ?", strings.ToLower(strings.TrimSpace(email))).First(&u).Error; err != nil {
		return nil, err
	}
	return &u, nil
}

func (s *Store) FindUserByID(id string) (*model.User, error) {
	var u model.User
	if err := s.DB.Where("id = ?", id).First(&u).Error; err != nil {
		return nil, err
	}
	return &u, nil
}

func (s *Store) TouchUserLogin(id string) error {
	now := time.Now()
	return s.DB.Model(&model.User{}).Where("id = ?", id).Update("last_login_at", now).Error
}

func (s *Store) UpdateUserName(id, name string) error {
	return s.DB.Model(&model.User{}).Where("id = ?", id).Update("name", name).Error
}

func (s *Store) UpdateUserPass(id, hash string) error {
	return s.DB.Model(&model.User{}).Where("id = ?", id).Update("pass_hash", hash).Error
}

// CreateAccountNamespace 建个人命名空间；slug 冲突时加随机后缀。
func (s *Store) CreateAccountNamespace(userID, name, slugSeed string) (*model.Namespace, error) {
	slug := Slugify(slugSeed)
	if slug == "x" && strings.TrimSpace(slugSeed) == "" {
		slug = Slugify(userID)
	}
	for i := 0; i < 5; i++ {
		if _, err := s.FindNamespaceBySlug(slug); errors.Is(err, gorm.ErrRecordNotFound) {
			break
		}
		slug = Slugify(slugSeed) + "-" + RandHex(2)
	}
	ns := &model.Namespace{ID: NewID("NS"), Slug: slug, Name: name, Type: "account", OwnerID: userID, Visibility: "public"}
	if err := s.DB.Create(ns).Error; err != nil {
		return nil, err
	}
	return ns, nil
}

// CreateOrgNamespace 建组织命名空间。
func (s *Store) CreateOrgNamespace(ownerID, slug, name string) (*model.Namespace, error) {
	ns := &model.Namespace{ID: NewID("NS"), Slug: Slugify(slug), Name: name, Type: "org", OwnerID: ownerID, Visibility: "public"}
	if err := s.DB.Create(ns).Error; err != nil {
		return nil, err
	}
	return ns, nil
}

func (s *Store) FindNamespaceBySlug(slug string) (*model.Namespace, error) {
	var ns model.Namespace
	if err := s.DB.Where("slug = ?", strings.TrimPrefix(strings.TrimSpace(slug), "@")).First(&ns).Error; err != nil {
		return nil, err
	}
	return &ns, nil
}

func (s *Store) FindNamespaceByID(id string) (*model.Namespace, error) {
	var ns model.Namespace
	if err := s.DB.Where("id = ?", id).First(&ns).Error; err != nil {
		return nil, err
	}
	return &ns, nil
}

// PersonalNamespace 取用户的个人命名空间。
func (s *Store) PersonalNamespace(userID string) (*model.Namespace, error) {
	var ns model.Namespace
	if err := s.DB.Where("owner_id = ? AND type = 'account'", userID).First(&ns).Error; err != nil {
		return nil, err
	}
	return &ns, nil
}

func (s *Store) NamespacesOfUser(userID string) ([]model.Namespace, error) {
	var out []model.Namespace
	err := s.DB.Where("owner_id = ? OR id IN (SELECT namespace_id FROM ns_members WHERE user_id = ?)", userID, userID).
		Order("type, slug").Find(&out).Error
	return out, err
}

func (s *Store) IsOwner(nsID, userID string) bool {
	var n int64
	_ = s.DB.Model(&model.Namespace{}).Where("id = ? AND owner_id = ?", nsID, userID).Count(&n).Error
	return n > 0
}

func (s *Store) IsMember(nsID, userID string) bool {
	var n int64
	_ = s.DB.Model(&model.NsMember{}).Where("namespace_id = ? AND user_id = ?", nsID, userID).Count(&n).Error
	return n > 0
}

func (s *Store) AddMember(nsID, userID, role string) error {
	return s.DB.Clauses(clause.OnConflict{DoNothing: true}).
		Create(&model.NsMember{NamespaceID: nsID, UserID: userID, Role: role}).Error
}

/* ---------------- API-Key ---------------- */

func HashSecret(secret string) string {
	sum := sha256.Sum256([]byte(secret))
	return hex.EncodeToString(sum[:])
}

func (s *Store) CreateApiKey(userID, label string, scopes []string) (*model.ApiKey, string, error) {
	prefix := RandHex(4)
	secret := "ncc_" + prefix + "_" + RandHex(20)
	sc := marshalList(scopes)
	k := &model.ApiKey{ID: NewID("K"), UserID: userID, Label: label, Prefix: prefix, SecretHash: HashSecret(secret), Scopes: sc}
	if err := s.DB.Create(k).Error; err != nil {
		return nil, "", err
	}
	return k, secret, nil
}

func (s *Store) FindKeyBySecret(secret string) (*model.ApiKey, error) {
	parts := strings.Split(secret, "_")
	if len(parts) < 3 {
		return nil, gorm.ErrRecordNotFound
	}
	var k model.ApiKey
	if err := s.DB.Where("prefix = ?", parts[1]).First(&k).Error; err != nil {
		return nil, err
	}
	if HashSecret(secret) != k.SecretHash {
		return nil, gorm.ErrRecordNotFound
	}
	return &k, nil
}

func (s *Store) ListApiKeys(userID string) ([]model.ApiKey, error) {
	var out []model.ApiKey
	err := s.DB.Where("user_id = ?", userID).Order("created_at desc").Find(&out).Error
	return out, err
}

func (s *Store) DeleteApiKey(id, userID string) error {
	return s.DB.Where("id = ? AND user_id = ?", id, userID).Delete(&model.ApiKey{}).Error
}

func (s *Store) TouchApiKey(id string) error {
	now := time.Now()
	return s.DB.Model(&model.ApiKey{}).Where("id = ?", id).Update("last_used_at", now).Error
}

/* ---------------- 制品 ---------------- */

// ArtifactRow 制品 + 其命名空间（一次 join，避免 N+1）。
type ArtifactRow struct {
	model.Artifact
	NsSlug string `gorm:"column:ns_slug"`
	NsName string `gorm:"column:ns_name"`
}

// ListOpts 目录过滤条件。
type ListOpts struct {
	Q            string
	Kind         string
	Tag          string
	NsSlug       string
	NamespaceIDs []string
	Statuses     []string
	PublicOnly   bool
	Page         int
	Size         int
	OrderBy      string
}

// ListResult 分页结果。
type ListResult struct {
	Rows  []ArtifactRow
	Total int64
}

type CreateArtifactInput struct {
	NamespaceID string
	Kind        string
	Name        string
	Slug        string
	Version     string
	Summary     string
	Tags        []string
	Visibility  string
	Status      string
	Manifest    string
	Provider    string
	StorageURL  string
	BlobName    string
	SHA256      string
	Size        int64
	CreatedBy   string
	Origin      string // local（默认）| replica
	OriginRef   string // 副本对应的上游引用
	// NamespaceSlug 仅落副本时用：副本按 slug 对号入座，命名空间缺就建镜像空间。
	NamespaceSlug string
}

func (s *Store) CreateArtifact(in CreateArtifactInput) (*model.Artifact, error) {
	origin := in.Origin
	if origin == "" {
		origin = model.OriginLocal
	}
	a := &model.Artifact{
		ID: NewID("A"), NamespaceID: in.NamespaceID, Kind: in.Kind, Name: in.Name,
		Slug: in.Slug, Version: in.Version, Summary: in.Summary, Tags: marshalList(in.Tags),
		Visibility: in.Visibility, Status: in.Status, Manifest: in.Manifest,
		StorageProvider: in.Provider, StorageURL: in.StorageURL, BlobName: in.BlobName,
		SHA256: in.SHA256, Size: in.Size, CreatedBy: in.CreatedBy,
		Origin: origin, OriginRef: in.OriginRef,
	}
	if err := s.DB.Create(a).Error; err != nil {
		return nil, err
	}
	return a, nil
}

// UpsertReplica 落一份副本（幂等：同 namespace+slug 就整行盖写）。
// 副本本身不参与可见性判断（它跟着上游的 public/published 走），所以固定 public+published。
func (s *Store) UpsertReplica(nsID string, in CreateArtifactInput) (*ArtifactRow, error) {
	existing, err := s.FindArtifactByNsSlug(in.NamespaceSlug, in.Slug)
	if err != nil && !errors.Is(err, gorm.ErrRecordNotFound) {
		return nil, err
	}
	if existing != nil && existing.ID != "" {
		fields := map[string]any{
			"kind": in.Kind, "name": in.Name, "version": in.Version, "summary": in.Summary,
			"tags": marshalList(in.Tags), "manifest": in.Manifest, "storage_provider": in.Provider,
			"storage_url": in.StorageURL, "blob_name": in.BlobName, "sha256": in.SHA256,
			"size": in.Size, "visibility": "public", "status": "published",
			"origin": model.OriginReplica, "origin_ref": in.OriginRef,
		}
		if err := s.UpdateArtifactFields(existing.ID, fields); err != nil {
			return nil, err
		}
		return s.FindArtifactByID(existing.ID)
	}
	in.Origin = model.OriginReplica
	in.Visibility = "public"
	in.Status = "published"
	in.NamespaceID = nsID
	created, err := s.CreateArtifact(in)
	if err != nil {
		return nil, err
	}
	return s.FindArtifactByID(created.ID)
}

// EnsureMirrorNamespace 拿/建镜像命名空间（副本落在这里，本地无人可改）。
func (s *Store) EnsureMirrorNamespace(slug, name string) (*model.Namespace, error) {
	if ns, err := s.FindNamespaceBySlug(slug); err == nil {
		return ns, nil
	}
	ns := &model.Namespace{
		ID: NewID("NS"), Slug: Slugify(slug), Name: name,
		Type: "mirror", OwnerID: model.MirrorOwner, Visibility: "public",
	}
	if err := s.DB.Create(ns).Error; err != nil {
		// 并发下可能已被别人建好，回读一次
		if hit, e := s.FindNamespaceBySlug(slug); e == nil {
			return hit, nil
		}
		return nil, err
	}
	return ns, nil
}

// DeleteReplica 回收一份副本（只删副本，不碰本节点自己发布的条目）。
func (s *Store) DeleteReplica(ref string) (string, string, bool, error) {
	var rows []ArtifactRow
	err := s.artifactQuery().Where("artifacts.origin = ? AND artifacts.origin_ref = ?", model.OriginReplica, ref).Scan(&rows).Error
	if err != nil {
		return "", "", false, err
	}
	if len(rows) == 0 {
		return "", "", false, nil
	}
	row := rows[0]
	if err := s.DB.Where("id = ?", row.ID).Delete(&model.Artifact{}).Error; err != nil {
		return "", "", false, err
	}
	return row.ID, row.BlobName, true, nil
}

func (s *Store) artifactQuery() *gorm.DB {
	return s.DB.Table("artifacts").
		Select("artifacts.*, namespaces.slug AS ns_slug, namespaces.name AS ns_name").
		Joins("LEFT JOIN namespaces ON namespaces.id = artifacts.namespace_id")
}

func (s *Store) FindArtifactByID(id string) (*ArtifactRow, error) {
	var row ArtifactRow
	if err := s.artifactQuery().Where("artifacts.id = ?", id).Limit(1).Scan(&row).Error; err != nil {
		return nil, err
	}
	if row.ID == "" {
		return nil, gorm.ErrRecordNotFound
	}
	return &row, nil
}

func (s *Store) FindArtifactByNsSlug(nsSlug, slug string) (*ArtifactRow, error) {
	var row ArtifactRow
	if err := s.artifactQuery().
		Where("namespaces.slug = ? AND artifacts.slug = ?", strings.TrimPrefix(nsSlug, "@"), slug).
		Limit(1).Scan(&row).Error; err != nil {
		return nil, err
	}
	if row.ID == "" {
		return nil, gorm.ErrRecordNotFound
	}
	return &row, nil
}

func (s *Store) ListArtifacts(o ListOpts) (ListResult, error) {
	q := s.artifactQuery()
	if o.PublicOnly {
		q = q.Where("artifacts.visibility = ?", "public")
	}
	if len(o.Statuses) > 0 {
		q = q.Where("artifacts.status IN ?", o.Statuses)
	}
	if len(o.NamespaceIDs) > 0 {
		q = q.Where("artifacts.namespace_id IN ?", o.NamespaceIDs)
	}
	if o.NsSlug != "" {
		q = q.Where("namespaces.slug = ?", strings.TrimPrefix(o.NsSlug, "@"))
	}
	if o.Kind != "" {
		q = q.Where("artifacts.kind = ?", o.Kind)
	}
	if o.Tag != "" {
		q = q.Where("artifacts.tags LIKE ?", "%\""+o.Tag+"\"%")
	}
	if o.Q != "" {
		like := "%" + o.Q + "%"
		q = q.Where("artifacts.name LIKE ? OR artifacts.slug LIKE ? OR artifacts.summary LIKE ?", like, like, like)
	}
	var total int64
	if err := q.Count(&total).Error; err != nil {
		return ListResult{}, err
	}
	size := o.Size
	if size <= 0 || size > 100 {
		size = 20
	}
	page := o.Page
	if page <= 0 {
		page = 1
	}
	// 排序字段必须带表名：join 之后 created_at/updated_at 在两张表里都有。
	order := "artifacts.updated_at DESC"
	if o.OrderBy == "downloads" {
		order = "artifacts.downloads DESC, artifacts.updated_at DESC"
	}
	var rows []ArtifactRow
	if err := q.Order(order).Offset((page - 1) * size).Limit(size).Scan(&rows).Error; err != nil {
		return ListResult{}, err
	}
	return ListResult{Rows: rows, Total: total}, nil
}

// ListAdvertisableArtifacts 供 worker 上报给 master 的目录：本节点持有的已发布公开条目。
func (s *Store) ListAdvertisableArtifacts(limit int) ([]ArtifactRow, error) {
	if limit <= 0 || limit > 5000 {
		limit = 2000
	}
	var rows []ArtifactRow
	err := s.artifactQuery().
		Where("artifacts.status = ? AND artifacts.visibility = ?", "published", "public").
		Order("artifacts.updated_at DESC").Limit(limit).Scan(&rows).Error
	return rows, err
}

// KindCounts 各类型的已发布条目数。
func (s *Store) KindCounts() (map[string]int64, error) {
	type row struct {
		Kind string
		N    int64
	}
	var rows []row
	err := s.DB.Model(&model.Artifact{}).
		Select("kind, count(*) AS n").
		Where("status = ? AND visibility = ?", "published", "public").
		Group("kind").Scan(&rows).Error
	out := map[string]int64{}
	for _, r := range rows {
		out[r.Kind] = r.N
	}
	return out, err
}

func (s *Store) CountArtifacts() (int64, error) {
	var n int64
	err := s.DB.Model(&model.Artifact{}).Where("status = ? AND visibility = ?", "published", "public").Count(&n).Error
	return n, err
}

func (s *Store) UpdateArtifactFields(id string, fields map[string]any) error {
	return s.DB.Model(&model.Artifact{}).Where("id = ?", id).Updates(fields).Error
}

func (s *Store) DeleteArtifact(id string) error {
	return s.DB.Where("id = ?", id).Delete(&model.Artifact{}).Error
}

func (s *Store) BumpDownloads(id string) error {
	return s.DB.Model(&model.Artifact{}).Where("id = ?", id).UpdateColumn("downloads", gorm.Expr("downloads + 1")).Error
}

/* ---------------- 托管节点 ---------------- */

type UpsertNodeInput struct {
	NamespaceID  string
	Name         string
	Slug         string
	Kind         string
	Region       string
	URL          string
	OS           string
	Arch         string
	Version      string
	Agent        string
	Capabilities []string
	// OffersVerified 本机**自证**的提供能力（不是运营者自选的，是由硬事实推导出来的）。
	OffersVerified []string
	Visibility     string
}

// UpsertHostedNode 注册与心跳合并：同 namespace+slug 则续租，否则新建。
func (s *Store) UpsertHostedNode(in UpsertNodeInput) (*model.HostedNode, bool, error) {
	kind := in.Kind
	if !model.ValidNodeKind(kind) {
		kind = model.NodeService
	}
	vis := in.Visibility
	if vis != "public" && vis != "private" {
		vis = "public"
	}
	var existing model.HostedNode
	err := s.DB.Where("namespace_id = ? AND slug = ?", in.NamespaceID, in.Slug).First(&existing).Error
	created := errors.Is(err, gorm.ErrRecordNotFound)
	if err != nil && !created {
		return nil, false, err
	}
	n := existing
	if created || n.ID == "" {
		n = model.HostedNode{ID: NewID("ND"), NamespaceID: in.NamespaceID, Slug: in.Slug}
		created = true
	}
	n.Name = in.Name
	n.Kind = kind
	n.Region = in.Region
	n.URL = in.URL
	n.OS = in.OS
	n.Arch = in.Arch
	n.Version = in.Version
	n.Agent = in.Agent
	n.Capabilities = marshalList(in.Capabilities)
	n.OffersVerified = marshalList(in.OffersVerified)
	n.Visibility = vis
	n.LastSeen = time.Now()

	if err := s.DB.Clauses(clause.OnConflict{
		Columns: []clause.Column{{Name: "namespace_id"}, {Name: "slug"}},
		DoUpdates: clause.AssignmentColumns([]string{
			"name", "kind", "region", "url", "os", "arch", "version", "agent",
			"capabilities", "offers_verified", "visibility", "last_seen", "updated_at",
		}),
	}).Create(&n).Error; err != nil {
		return nil, false, err
	}
	if created {
		var got model.HostedNode
		if err := s.DB.Where("namespace_id = ? AND slug = ?", in.NamespaceID, in.Slug).First(&got).Error; err != nil {
			return nil, false, err
		}
		if got.ID != n.ID {
			created = false // 并发下别人先插进去了
		}
		return &got, created, nil
	}
	return &n, false, nil
}

// NodeRow 节点 + 命名空间 + 归属者 + 我的连接状态（一次 join）。
type NodeRow struct {
	model.HostedNode
	NsSlug      string `gorm:"column:ns_slug"`
	NsName      string `gorm:"column:ns_name"`
	OwnerID     string `gorm:"column:owner_id"`
	OwnerName   string `gorm:"column:owner_name"`
	OwnerEmail  string `gorm:"column:owner_email"`
	OwnerRegion string `gorm:"column:owner_region"`
	LinkID      string `gorm:"column:link_id"`
	LinkLabel   string `gorm:"column:link_label"`
}

func (s *Store) nodeQuery(ownerID string) *gorm.DB {
	return s.DB.Table("hosted_nodes").
		Select(`hosted_nodes.*, namespaces.slug AS ns_slug, namespaces.name AS ns_name,
			users.id AS owner_id, users.name AS owner_name, users.email AS owner_email,
			hosted_nodes.region AS owner_region,
			node_links.id AS link_id, node_links.label AS link_label`).
		Joins("LEFT JOIN namespaces ON namespaces.id = hosted_nodes.namespace_id").
		Joins("LEFT JOIN users ON users.id = namespaces.owner_id").
		Joins("LEFT JOIN node_links ON node_links.node_id = hosted_nodes.id AND node_links.owner_id = ?", ownerID)
}

// ListNodesOfNamespaces 我命名空间下的节点。
func (s *Store) ListNodesOfNamespaces(ownerID string, nsIDs []string) ([]NodeRow, error) {
	var rows []NodeRow
	q := s.nodeQuery(ownerID).Where("hosted_nodes.namespace_id IN ?", nsIDs).
		Order("hosted_nodes.name")
	err := q.Scan(&rows).Error
	return rows, err
}

// ListLinkedNodes 我连接表里的节点。
func (s *Store) ListLinkedNodes(ownerID string) ([]NodeRow, error) {
	var rows []NodeRow
	err := s.nodeQuery(ownerID).
		Where("node_links.id IS NOT NULL AND node_links.id <> ''").
		Order("node_links.label, hosted_nodes.name").Scan(&rows).Error
	return rows, err
}

// ListPublicNodes 本实例上可被发现的节点（排除我自己的）。
// grantedOwners：拿到过 node 授权的人 —— 他们的私有节点也应当对我可见。
func (s *Store) ListPublicNodes(ownerID string, grantedOwners []string, kind, region, q string, offers []model.OfferQuery, limit int) ([]NodeRow, error) {
	query := s.nodeQuery(ownerID).
		Where("hosted_nodes.namespace_id NOT IN (SELECT id FROM namespaces WHERE owner_id = ?)", ownerID)
	if len(grantedOwners) > 0 {
		query = query.Where("(hosted_nodes.visibility = ? OR namespaces.owner_id IN ?)", "public", grantedOwners)
	} else {
		query = query.Where("hosted_nodes.visibility = ?", "public")
	}
	if kind != "" {
		query = query.Where("hosted_nodes.kind = ?", kind)
	}
	if region != "" {
		query = query.Where("hosted_nodes.region = ?", region)
	}
	if q != "" {
		like := "%" + q + "%"
		query = query.Where("hosted_nodes.name LIKE ? OR hosted_nodes.slug LIKE ?", like, like)
	}
	query = whereNodeOffers(query, offers)
	if limit <= 0 || limit > 200 {
		limit = 100
	}
	var rows []NodeRow
	err := query.Order("hosted_nodes.last_seen DESC").Limit(limit).Scan(&rows).Error
	return rows, err
}

// whereNodeOffers 给节点查询加上「声明/自证里含这些能力」的条件。
//
// 每条能力内部是「规范 id 或某个历史别名」的或，能力之间是且。`@verified` 的只查自证列；
// 不带限定符的查**两列** —— 自证比声明强，一个只在自证列表里的能力也应该被 `can=X` 命中。
//
// capabilities / offers_verified 都是 JSON 数组字符串（["mcp","api"]），
// 所以用带引号的 LIKE：`%"mcp"%` 不会误命中 `"mcp-server"`。库里可能还存着老节点上报的
// 短名，因此候选集里同时包含规范 id 与别名（model.OfferAlternatives）。
func whereNodeOffers(query *gorm.DB, offers []model.OfferQuery) *gorm.DB {
	for _, o := range offers {
		alts := model.OfferAlternatives(o.ID)
		if len(alts) == 0 {
			continue
		}
		cols := []string{"hosted_nodes.capabilities", "hosted_nodes.offers_verified"}
		if o.VerifiedOnly {
			cols = []string{"hosted_nodes.offers_verified"}
		}
		ors := make([]string, 0, len(alts)*len(cols))
		args := make([]any, 0, len(alts)*len(cols))
		for _, c := range cols {
			for _, a := range alts {
				ors = append(ors, c+" LIKE ?")
				args = append(args, "%\""+a+"\"%")
			}
		}
		query = query.Where("("+strings.Join(ors, " OR ")+")", args...)
	}
	return query
}

func (s *Store) FindNodeRow(id string, ownerID string) (*NodeRow, error) {
	var rows []NodeRow
	if err := s.nodeQuery(ownerID).Where("hosted_nodes.id = ?", id).Limit(1).Scan(&rows).Error; err != nil {
		return nil, err
	}
	if len(rows) == 0 {
		return nil, gorm.ErrRecordNotFound
	}
	return &rows[0], nil
}

func (s *Store) FindNodeByNsSlug(nsSlug, slug, ownerID string) (*NodeRow, error) {
	var rows []NodeRow
	err := s.nodeQuery(ownerID).
		Where("namespaces.slug = ? AND hosted_nodes.slug = ?", strings.TrimPrefix(nsSlug, "@"), slug).
		Limit(1).Scan(&rows).Error
	if err != nil {
		return nil, err
	}
	if len(rows) == 0 {
		return nil, gorm.ErrRecordNotFound
	}
	return &rows[0], nil
}

func (s *Store) DeleteHostedNode(id, namespaceID string) error {
	return s.DB.Where("id = ? AND namespace_id = ?", id, namespaceID).Delete(&model.HostedNode{}).Error
}

func (s *Store) CountNodes() (int64, error) {
	var n int64
	err := s.DB.Model(&model.HostedNode{}).Count(&n).Error
	return n, err
}

func (s *Store) CountUsers() (int64, error) {
	var n int64
	err := s.DB.Model(&model.User{}).Count(&n).Error
	return n, err
}

// RegionCounts 各区域在线/离线节点数（区域覆盖视图）。
type RegionCount struct {
	Region string
	Total  int64
	Online int64
}

func (s *Store) RegionCounts(ttl time.Duration) ([]RegionCount, error) {
	type row struct {
		Region string
		Total  int64
		Online int64
	}
	cutoff := time.Now().Add(-ttl)
	var rows []row
	err := s.DB.Model(&model.HostedNode{}).
		Select("region, count(*) AS total, sum(CASE WHEN last_seen >= ? THEN 1 ELSE 0 END) AS online", cutoff).
		Group("region").Order("total DESC").Scan(&rows).Error
	out := make([]RegionCount, 0, len(rows))
	for _, r := range rows {
		region := r.Region
		if region == "" {
			region = "未声明"
		}
		out = append(out, RegionCount{Region: region, Total: r.Total, Online: r.Online})
	}
	return out, err
}

/* ---------------- 节点连接 ---------------- */

func (s *Store) LinkNode(ownerID, nodeID, targetUserID, label, note string) (*model.NodeLink, error) {
	l := &model.NodeLink{ID: NewID("LK"), OwnerID: ownerID, NodeID: nodeID, TargetUserID: targetUserID, Label: label, Note: note}
	if err := s.DB.Create(l).Error; err != nil {
		return nil, err
	}
	return l, nil
}

func (s *Store) FindLink(ownerID, nodeID string) (*model.NodeLink, error) {
	var l model.NodeLink
	if err := s.DB.Where("owner_id = ? AND node_id = ?", ownerID, nodeID).First(&l).Error; err != nil {
		return nil, err
	}
	return &l, nil
}

func (s *Store) FindLinkByID(id, ownerID string) (*model.NodeLink, error) {
	var l model.NodeLink
	if err := s.DB.Where("id = ? AND owner_id = ?", id, ownerID).First(&l).Error; err != nil {
		return nil, err
	}
	return &l, nil
}

func (s *Store) UpdateLink(id, ownerID, label, note string) error {
	return s.DB.Model(&model.NodeLink{}).Where("id = ? AND owner_id = ?", id, ownerID).
		Updates(map[string]any{"label": label, "note": note}).Error
}

func (s *Store) UnlinkNode(id, ownerID string) error {
	return s.DB.Where("id = ? AND owner_id = ?", id, ownerID).Delete(&model.NodeLink{}).Error
}

/* ---------------- 集群（master 侧） ---------------- */

type WorkerInput struct {
	ID           string
	Name         string
	URL          string
	Version      string
	Region       string
	Capabilities []string
	Artifacts    int64
	Nodes        int64
	Users        int64
}

func (s *Store) UpsertWorker(in WorkerInput) error {
	if in.ID == "" {
		return errors.New("worker id 不能为空")
	}
	w := model.ClusterWorker{
		ID: in.ID, Name: in.Name, URL: strings.TrimRight(in.URL, "/"), Version: in.Version,
		Region: in.Region, Capabilities: marshalList(in.Capabilities),
		Artifacts: in.Artifacts, Nodes: in.Nodes, Users: in.Users,
		LastSeen: time.Now(), FirstSeen: time.Now(),
	}
	return s.DB.Clauses(clause.OnConflict{
		Columns: []clause.Column{{Name: "id"}},
		DoUpdates: clause.AssignmentColumns([]string{
			"name", "url", "version", "region", "capabilities", "artifacts", "nodes", "users", "last_seen",
		}),
	}).Create(&w).Error
}

func (s *Store) ListWorkers() ([]model.ClusterWorker, error) {
	var out []model.ClusterWorker
	err := s.DB.Order("name").Find(&out).Error
	return out, err
}

// PruneWorkers 清掉长期没心跳的 worker（连带它的目录advert）。
func (s *Store) PruneWorkers(olderThan time.Duration) (int64, error) {
	cutoff := time.Now().Add(-olderThan)
	var stale []model.ClusterWorker
	if err := s.DB.Where("last_seen < ?", cutoff).Find(&stale).Error; err != nil {
		return 0, err
	}
	if len(stale) == 0 {
		return 0, nil
	}
	ids := make([]string, 0, len(stale))
	for _, w := range stale {
		ids = append(ids, w.ID)
	}
	if err := s.DB.Where("worker_id IN ?", ids).Delete(&model.ArtifactAdvert{}).Error; err != nil {
		return 0, err
	}
	res := s.DB.Where("id IN ?", ids).Delete(&model.ClusterWorker{})
	return res.RowsAffected, res.Error
}

type AdvertInput struct {
	Ref           string
	NamespaceSlug string
	Slug          string
	Kind          string
	Name          string
	Version       string
	Summary       string
	Tags          []string
	SHA256        string
	Size          int64
	Downloads     int64
	UpdatedAt     time.Time
}

// ReplaceAdverts 用本次上报的清单整体替换该 worker 的目录（发布/删除都能收敛）。
func (s *Store) ReplaceAdverts(workerID string, rows []AdvertInput) error {
	return s.DB.Transaction(func(tx *gorm.DB) error {
		if err := tx.Where("worker_id = ?", workerID).Delete(&model.ArtifactAdvert{}).Error; err != nil {
			return err
		}
		if len(rows) == 0 {
			return nil
		}
		seen := map[string]bool{}
		now := time.Now()
		out := make([]model.ArtifactAdvert, 0, len(rows))
		for _, r := range rows {
			if r.Ref == "" || seen[r.Ref] {
				continue
			}
			seen[r.Ref] = true
			upd := r.UpdatedAt
			if upd.IsZero() {
				upd = now
			}
			out = append(out, model.ArtifactAdvert{
				ID: NewID("AD"), WorkerID: workerID, Ref: r.Ref, NamespaceSlug: r.NamespaceSlug,
				Slug: r.Slug, Kind: r.Kind, Name: r.Name, Version: r.Version, Summary: r.Summary,
				Tags: marshalList(r.Tags), SHA256: r.SHA256, Size: r.Size, Downloads: r.Downloads,
				UpdatedAt: upd, SeenAt: now,
			})
		}
		return tx.Create(&out).Error
	})
}

// ListAdverts 集群聚合目录（含来源 worker）。
type AdvertRow struct {
	model.ArtifactAdvert
	WorkerName string `gorm:"column:worker_name"`
	WorkerURL  string `gorm:"column:worker_url"`
}

func (s *Store) ListAdverts(q, kind, tag string, limit int) ([]AdvertRow, error) {
	query := s.DB.Table("artifact_adverts").
		Select("artifact_adverts.*, cluster_workers.name AS worker_name, cluster_workers.url AS worker_url").
		Joins("LEFT JOIN cluster_workers ON cluster_workers.id = artifact_adverts.worker_id")
	if kind != "" {
		query = query.Where("artifact_adverts.kind = ?", kind)
	}
	if tag != "" {
		query = query.Where("artifact_adverts.tags LIKE ?", "%\""+tag+"\"%")
	}
	if q != "" {
		like := "%" + q + "%"
		query = query.Where("artifact_adverts.name LIKE ? OR artifact_adverts.slug LIKE ? OR artifact_adverts.summary LIKE ?", like, like, like)
	}
	if limit <= 0 || limit > 500 {
		limit = 200
	}
	var rows []AdvertRow
	err := query.Order("artifact_adverts.updated_at DESC").Limit(limit).Scan(&rows).Error
	return rows, err
}

// FindAdvertsByRef 按 @ns/slug 或 @ns/slug@version 找提供者（能力路由）。
func (s *Store) FindAdvertsByRef(ref string) ([]AdvertRow, error) {
	ref = strings.TrimSpace(ref)
	slug := ref
	if i := strings.LastIndex(ref, "@"); i > 0 {
		slug = ref[:i] // 去掉版本
	}
	var rows []AdvertRow
	err := s.DB.Table("artifact_adverts").
		Select("artifact_adverts.*, cluster_workers.name AS worker_name, cluster_workers.url AS worker_url").
		Joins("LEFT JOIN cluster_workers ON cluster_workers.id = artifact_adverts.worker_id").
		Where("artifact_adverts.ref = ? OR artifact_adverts.ref LIKE ?", ref, slug+"@%").
		Scan(&rows).Error
	return rows, err
}

/* ---------------- 授权（Grant） ---------------- */

type GrantInput struct {
	OwnerID       string
	GranteeUserID string
	Kind          string
	NamespaceID   string
	Note          string
}

// UpsertGrant 同一（授权人, 被授权人, 类型, 命名空间）只有一条（幂等）。
func (s *Store) UpsertGrant(in GrantInput) (*model.Grant, error) {
	g := model.Grant{
		ID: NewID("G"), OwnerID: in.OwnerID, GranteeUserID: in.GranteeUserID,
		Kind: in.Kind, NamespaceID: in.NamespaceID, Note: in.Note,
	}
	err := s.DB.Clauses(clause.OnConflict{
		Columns:   []clause.Column{{Name: "owner_id"}, {Name: "grantee_user_id"}, {Name: "kind"}, {Name: "namespace_id"}},
		DoUpdates: clause.AssignmentColumns([]string{"note", "updated_at"}),
	}).Create(&g).Error
	if err != nil {
		return nil, err
	}
	var got model.Grant
	if err := s.DB.Where("owner_id = ? AND grantee_user_id = ? AND kind = ? AND namespace_id = ?",
		in.OwnerID, in.GranteeUserID, in.Kind, in.NamespaceID).First(&got).Error; err != nil {
		return nil, err
	}
	return &got, nil
}

func (s *Store) ListGrants(ownerID, granteeID string) ([]model.Grant, error) {
	var out []model.Grant
	q := s.DB.Model(&model.Grant{})
	if ownerID != "" {
		q = q.Where("owner_id = ?", ownerID)
	}
	if granteeID != "" {
		q = q.Where("grantee_user_id = ?", granteeID)
	}
	err := q.Order("created_at desc").Find(&out).Error
	return out, err
}

func (s *Store) FindGrant(id string) (*model.Grant, error) {
	var g model.Grant
	if err := s.DB.Where("id = ?", id).First(&g).Error; err != nil {
		return nil, err
	}
	return &g, nil
}

func (s *Store) DeleteGrant(id, ownerID string) error {
	return s.DB.Where("id = ? AND owner_id = ?", id, ownerID).Delete(&model.Grant{}).Error
}

// HasGrant 判断 owner 是否已把 kind 权限授给 grantee（nsID 为空 = 不限命名空间）。
func (s *Store) HasGrant(ownerID, granteeID, kind, nsID string) bool {
	if ownerID == "" || granteeID == "" || ownerID == granteeID {
		return false
	}
	var n int64
	q := s.DB.Model(&model.Grant{}).Where(
		"owner_id = ? AND grantee_user_id = ? AND kind = ?", ownerID, granteeID, kind)
	if nsID != "" {
		q = q.Where("namespace_id = '' OR namespace_id = ?", nsID)
	}
	_ = q.Count(&n).Error
	return n > 0
}

// GrantedOwners 我（grantee）被哪些人授过某类权限 —— 用于「可发现」时把私有节点算进来。
func (s *Store) GrantedOwners(granteeID, kind string) ([]string, error) {
	var owners []string
	err := s.DB.Model(&model.Grant{}).
		Where("grantee_user_id = ? AND kind = ?", granteeID, kind).
		Distinct().Pluck("owner_id", &owners).Error
	return owners, err
}

/* ---------------- 接入票据（AccessTicket） ---------------- */

// NewTicketKey 生成短 key（可念、可手打）。
func NewTicketKey() string { return "NK-" + strings.ToUpper(RandHex(3)) }

// NewTicketSecret 生成 secret 明文（只返回给创建者一次）。
func NewTicketSecret() string { return RandHex(16) }

func (s *Store) CreateAccessTicket(key, secret, label string, scopes []string, nsID, createdBy string, maxUses int64, expiresAt *time.Time) (*model.AccessTicket, error) {
	t := &model.AccessTicket{
		ID: NewID("TK"), Key: key, SecretHash: HashSecret(secret), Label: label,
		Scopes: marshalList(scopes), NamespaceID: nsID, CreatedBy: createdBy,
		MaxUses: maxUses, ExpiresAt: expiresAt,
	}
	if err := s.DB.Create(t).Error; err != nil {
		return nil, err
	}
	return t, nil
}

func (s *Store) FindTicketByKey(key string) (*model.AccessTicket, error) {
	var t model.AccessTicket
	if err := s.DB.Where("key = ?", strings.TrimSpace(key)).First(&t).Error; err != nil {
		return nil, err
	}
	return &t, nil
}

func (s *Store) FindTicketByID(id string) (*model.AccessTicket, error) {
	var t model.AccessTicket
	if err := s.DB.Where("id = ?", id).First(&t).Error; err != nil {
		return nil, err
	}
	return &t, nil
}

func (s *Store) ListTickets(createdBy string) ([]model.AccessTicket, error) {
	var out []model.AccessTicket
	err := s.DB.Where("created_by = ?", createdBy).Order("created_at desc").Find(&out).Error
	return out, err
}

func (s *Store) DeleteTicket(id, createdBy string) error {
	return s.DB.Where("id = ? AND created_by = ?", id, createdBy).Delete(&model.AccessTicket{}).Error
}

func (s *Store) MarkTicketUsed(id string) error {
	now := time.Now()
	return s.DB.Model(&model.AccessTicket{}).Where("id = ?", id).
		Updates(map[string]any{
			"used_count":   gorm.Expr("used_count + 1"),
			"last_used_at": now,
		}).Error
}

/* ---------------- 分发目标（ReplicaTarget） ---------------- */

// RecordReplicaTarget 记下「这份制品已分发到该 worker」（幂等）。
func (s *Store) RecordReplicaTarget(ref, workerID, name, url, sha string, size int64) error {
	t := model.ReplicaTarget{
		ID: NewID("RT"), Ref: ref, WorkerID: workerID, WorkerName: name,
		WorkerURL: strings.TrimRight(url, "/"), SHA256: sha, Size: size,
	}
	return s.DB.Clauses(clause.OnConflict{
		Columns:   []clause.Column{{Name: "ref"}, {Name: "worker_id"}},
		DoUpdates: clause.AssignmentColumns([]string{"worker_name", "worker_url", "sha256", "size"}),
	}).Create(&t).Error
}

func (s *Store) ListReplicaTargets(ref string) ([]model.ReplicaTarget, error) {
	var out []model.ReplicaTarget
	err := s.DB.Where("ref = ?", ref).Order("created_at").Find(&out).Error
	return out, err
}

func (s *Store) DeleteReplicaTargets(ref string) error {
	return s.DB.Where("ref = ?", ref).Delete(&model.ReplicaTarget{}).Error
}

// ReplicaTargetsByRef 一次拿全部分发记录（目录页给本地条目标注副本用，避免 N+1）。
func (s *Store) ReplicaTargetsByRef() (map[string][]model.ReplicaTarget, error) {
	var rows []model.ReplicaTarget
	if err := s.DB.Order("created_at").Find(&rows).Error; err != nil {
		return nil, err
	}
	out := make(map[string][]model.ReplicaTarget, len(rows))
	for _, r := range rows {
		out[r.Ref] = append(out[r.Ref], r)
	}
	return out, nil
}

/* ---------------- 工具 ---------------- */

func marshalList(v []string) string {
	if v == nil {
		v = []string{}
	}
	b, err := json.Marshal(v)
	if err != nil {
		return "[]"
	}
	return string(b)
}

// ParseList 还原 JSON 字符串数组；非法值返回空切片（响应里永远是数组）。
func ParseList(s string) []string {
	out := []string{}
	if strings.TrimSpace(s) == "" {
		return out
	}
	if err := json.Unmarshal([]byte(s), &out); err != nil {
		return []string{}
	}
	return out
}
