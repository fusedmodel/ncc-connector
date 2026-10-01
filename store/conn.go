package store

import (
	"time"

	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
)

// 连接（通道）的会话状态。**记录留久**：它是「谁在这台机器上开过通道、跑过什么」的账本，
// 不像任务那样可以随手清。

type ConnInput struct {
	ID          string
	OwnerID     string
	RequestedBy string
	Name        string
	Note        string
	WorkDir     string
	Peer        string
	TTLSec      int64
	ExpiresAt   time.Time
}

func (s *Store) CreateConn(in ConnInput) (*model.Conn, error) {
	id := in.ID
	if id == "" {
		id = NewID("CN")
	}
	c := &model.Conn{
		ID: id, OwnerID: in.OwnerID, RequestedBy: in.RequestedBy,
		Name: in.Name, Note: in.Note, WorkDir: in.WorkDir, Peer: in.Peer,
		Status: model.ConnOpen, TTLSec: in.TTLSec, ExpiresAt: in.ExpiresAt,
	}
	if err := s.DB.Create(c).Error; err != nil {
		return nil, err
	}
	return c, nil
}

func (s *Store) FindConn(id string) (*model.Conn, error) {
	var c model.Conn
	if err := s.DB.Where("id = ?", id).First(&c).Error; err != nil {
		return nil, err
	}
	return &c, nil
}

// ListConns ownerID 为空 = 全部（管理员视角）。
func (s *Store) ListConns(ownerID string, limit, offset int) ([]model.Conn, error) {
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	q := s.DB.Model(&model.Conn{})
	if ownerID != "" {
		q = q.Where("owner_id = ?", ownerID)
	}
	var out []model.Conn
	err := q.Order("created_at DESC").Offset(offset).Limit(limit).Find(&out).Error
	return out, err
}

// TouchConn 记一次使用（每次 exec / push / pull 都刷一次，过期判定才有意义）。
func (s *Store) TouchConn(id string, execDelta, upDelta, downDelta, pullDelta int64) error {
	now := time.Now()
	fields := map[string]any{"last_used_at": now}
	if execDelta != 0 {
		fields["exec_count"] = gorm.Expr("exec_count + ?", execDelta)
	}
	if upDelta != 0 {
		fields["bytes_up"] = gorm.Expr("bytes_up + ?", upDelta)
	}
	if downDelta != 0 {
		fields["bytes_down"] = gorm.Expr("bytes_down + ?", downDelta)
	}
	if pullDelta != 0 {
		fields["pull_count"] = gorm.Expr("pull_count + ?", pullDelta)
	}
	return s.DB.Model(&model.Conn{}).Where("id = ?", id).Updates(fields).Error
}

// CloseConn 关闭通道。ownerID 为空 = 管理员（可关任何人的）。
//
// 条件里带 `status = open`：重复关闭不该把已经关掉的行再写一遍时间戳，
// 也不该把「已经关了」当失败报给用户（调用方会用 RowsAffected 判断是不是本来开着的）。
func (s *Store) CloseConn(id, ownerID string) (bool, error) {
	q := s.DB.Model(&model.Conn{}).Where("id = ? AND status = ?", id, model.ConnOpen)
	if ownerID != "" {
		q = q.Where("owner_id = ?", ownerID)
	}
	now := time.Now()
	res := q.Updates(map[string]any{"status": model.ConnClosed, "closed_at": now})
	if res.Error != nil {
		return false, res.Error
	}
	return res.RowsAffected > 0, nil
}

// CountExpiredConns 数一下「TTL 到了但状态还写着 open」的通道。
//
// **只数不改**：过期是**推导**出来的（`ExpiresAt` 到了即过期，见 `Conn.State`），
// 不是一次状态翻转。早期实现把它们改写成 closed，结果「过期」和「被人关掉」在接口上
// 分不出来（410 的文案、列表里的状态都一样）—— 而对用户这两件事意义完全不同：
// 一个是「等一等/重开一条」，一个是「别人把你的门关了」。
func (s *Store) CountExpiredConns(now time.Time) (int64, error) {
	var n int64
	err := s.DB.Model(&model.Conn{}).
		Where("status = ? AND expires_at < ?", model.ConnOpen, now).Count(&n).Error
	return n, err
}

func (s *Store) DeleteConn(id, ownerID string) error {
	q := s.DB.Where("id = ?", id)
	if ownerID != "" {
		q = q.Where("owner_id = ?", ownerID)
	}
	return q.Delete(&model.Conn{}).Error
}
