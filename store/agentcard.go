package store

import (
	"crypto/sha256"
	"encoding/hex"
	"strings"
	"time"

	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
)

// Agent 名片（NCC Agent Share）：与分享链接同一套「临时放行」的规矩 ——
// token 只存 sha256、可限次 / 限时 / 撤销，且**不进 Grant 那张表**。
//
// 与分享（ArtifactShare）的差别只有一处：分享指向一条**已存在的制品**，
// 名片自己带着字节（作者还没发布也能先把 Agent 交出去）。

// NewCardToken 生成名片 token（32 位 hex，放进 URL 路径；库里只存 sha256）。
func NewCardToken() string { return RandHex(16) }

// HashCardPass 访问口令的**盐化**哈希。
//
// 与 token（`HashSecret`，不加盐）分开：token 是 32 位随机串，不怕字典；
// 口令是人写的 3-16 位短串，必须加盐。
func HashCardPass(salt, pw string) string {
	h := sha256.Sum256([]byte(salt + ":" + pw))
	return hex.EncodeToString(h[:])
}

type AgentCardInput struct {
	ID           string
	OwnerID      string
	Name         string
	Note         string
	BlobName     string
	Size         int64
	SHA256       string
	Manifest     string
	AgentID      string
	AgentVersion string
	AgentKind    string
	AgentProfile string
	NodeRef      string
	NodeID       string
	NodeKind     string
	NodeLabel    string
	PassHash     string
	PassSalt     string
	MaxUses      int64
	ExpiresAt    *time.Time
}

// CreateAgentCard 建一张名片，返回记录与**明文 token**（明文只在这一刻存在）。
func (s *Store) CreateAgentCard(in AgentCardInput) (*model.AgentCard, string, error) {
	token := NewCardToken()
	hint := token
	if len(hint) > 6 {
		hint = hint[:6]
	}
	id := in.ID
	if id == "" {
		id = NewID("AC")
	}
	c := &model.AgentCard{
		ID: id, TokenHash: HashSecret(token), TokenHint: hint, OwnerID: in.OwnerID,
		Name: strings.TrimSpace(in.Name), Note: strings.TrimSpace(in.Note),
		BlobName: in.BlobName, Size: in.Size, SHA256: in.SHA256, Manifest: in.Manifest,
		AgentID: in.AgentID, AgentVersion: in.AgentVersion,
		AgentKind: in.AgentKind, AgentProfile: in.AgentProfile,
		NodeRef: in.NodeRef, NodeID: in.NodeID, NodeKind: in.NodeKind, NodeLabel: in.NodeLabel,
		PassHash: in.PassHash, PassSalt: in.PassSalt,
		MaxUses: in.MaxUses, ExpiresAt: in.ExpiresAt,
	}
	if err := s.DB.Create(c).Error; err != nil {
		return nil, "", err
	}
	return c, token, nil
}

// FindAgentCardByToken token 是明文，库里存哈希 —— 查的时候现算。
func (s *Store) FindAgentCardByToken(token string) (*model.AgentCard, error) {
	var c model.AgentCard
	if err := s.DB.Where("token_hash = ?", HashSecret(strings.TrimSpace(token))).First(&c).Error; err != nil {
		return nil, err
	}
	return &c, nil
}

// FindAgentCardByID 按 id（AC-…）取 —— 作者自己管理时用得到，不必再握着 token。
func (s *Store) FindAgentCardByID(id string) (*model.AgentCard, error) {
	var c model.AgentCard
	if err := s.DB.Where("id = ?", strings.TrimSpace(id)).First(&c).Error; err != nil {
		return nil, err
	}
	return &c, nil
}

// ListAgentCards 只列**某个人的**名片（含已撤销/过期/用尽 —— 作者要看得到自己发过什么）。
// 与分享一样，刻意没有「全站名片」这种查询。
func (s *Store) ListAgentCards(ownerID string, limit, offset int) ([]model.AgentCard, error) {
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	var out []model.AgentCard
	err := s.DB.Model(&model.AgentCard{}).
		Where("owner_id = ?", ownerID).
		Order("created_at DESC").Offset(offset).Limit(limit).Find(&out).Error
	return out, err
}

// RevokeAgentCard 撤销：**标记 + 保留记录**（字节由调用方删）。
//
// 不删行：作者在 `ncc agent ls` 里要看得到自己发过什么、哪张已作废，
// 对方点开也该得到「作者撤销了」而不是含糊的 404。
func (s *Store) RevokeAgentCard(id, ownerID string) error {
	res := s.DB.Model(&model.AgentCard{}).
		Where("id = ? AND owner_id = ?", id, ownerID).
		Update("revoked_at", time.Now())
	if res.Error != nil {
		return res.Error
	}
	if res.RowsAffected == 0 {
		return gorm.ErrRecordNotFound
	}
	return nil
}

// ConsumeAgentCard 扣一次名额，返回扣完后的用量。
//
// 用一条**带条件的 UPDATE** 完成「检查 + 扣减」：只有 `max_uses = 0`（不限次）
// 或还没用完的那一行会被更新，`RowsAffected = 0` 就说明名额刚好在并发里被别人
// 用掉了 —— 这时候不能当成功。
func (s *Store) ConsumeAgentCard(id string) (int64, error) {
	res := s.DB.Exec(
		"UPDATE agent_cards SET uses = uses + 1 WHERE id = ? AND (max_uses = 0 OR uses < max_uses)", id)
	if res.Error != nil {
		return 0, res.Error
	}
	if res.RowsAffected == 0 {
		return 0, gorm.ErrRecordNotFound
	}
	var c model.AgentCard
	if err := s.DB.Select("uses").First(&c, "id = ?", id).Error; err != nil {
		return 0, err
	}
	return c.Uses, nil
}

func (s *Store) BumpAgentCardViews(id string) error {
	return s.DB.Exec("UPDATE agent_cards SET views = views + 1 WHERE id = ?", id).Error
}
