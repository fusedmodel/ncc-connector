package store

import (
	"time"

	"gorm.io/gorm"

	"github.com/fusedmodel/ncc-registry/model"
)

// 远程执行任务（Remote Cloud Computer）：一次任务一行。
//
// 任务记录**故意留得久**：它是「这台机器替谁跑过什么」的账本（审计 + 排障），
// 不像状态那样会被清理。清理由运维按需做（删记录 + 删工作目录）。

type ExecRunInput struct {
	OwnerID        string
	RequestedBy    string
	Engine         string
	Kind           string
	Spec           string
	Image          string
	PackageID      string
	PackageVersion string
	PackageSHA     string
	WorkDir        string
	LogPath        string
	Reason         string
	TimeoutSec     int64
}

func (s *Store) CreateExecRun(in ExecRunInput) (*model.ExecRun, error) {
	r := &model.ExecRun{
		ID: NewID("ER"), OwnerID: in.OwnerID, RequestedBy: in.RequestedBy,
		Engine: in.Engine, Kind: in.Kind, Spec: in.Spec, Image: in.Image,
		PackageID: in.PackageID, PackageVersion: in.PackageVersion, PackageSHA: in.PackageSHA,
		WorkDir: in.WorkDir, LogPath: in.LogPath, Reason: in.Reason, TimeoutSec: in.TimeoutSec,
		Status: model.ExecQueued,
	}
	if err := s.DB.Create(r).Error; err != nil {
		return nil, err
	}
	return r, nil
}

func (s *Store) FindExecRun(id string) (*model.ExecRun, error) {
	var r model.ExecRun
	if err := s.DB.Where("id = ?", id).First(&r).Error; err != nil {
		return nil, err
	}
	return &r, nil
}

// ListExecRuns 列出任务。ownerID 为空 = 全部（管理员视角）。
func (s *Store) ListExecRuns(ownerID string, limit, offset int) ([]model.ExecRun, error) {
	if limit <= 0 || limit > 200 {
		limit = 50
	}
	q := s.DB.Model(&model.ExecRun{})
	if ownerID != "" {
		q = q.Where("owner_id = ?", ownerID)
	}
	var out []model.ExecRun
	err := q.Order("created_at DESC").Offset(offset).Limit(limit).Find(&out).Error
	return out, err
}

func (s *Store) CountExecRuns(ownerID string) (int64, error) {
	q := s.DB.Model(&model.ExecRun{})
	if ownerID != "" {
		q = q.Where("owner_id = ?", ownerID)
	}
	var n int64
	err := q.Count(&n).Error
	return n, err
}

// MarkExecRunning 置为运行中（记开始时刻）。
func (s *Store) MarkExecRunning(id string) error {
	now := time.Now()
	return s.DB.Model(&model.ExecRun{}).Where("id = ?", id).
		Updates(map[string]any{"status": model.ExecRunning, "started_at": now}).Error
}

// FinishExecRun 落终态。
func (s *Store) FinishExecRun(id, status string, exitCode int, logBytes int64, truncated bool, errMsg string) error {
	now := time.Now()
	fields := map[string]any{
		"status": status, "exit_code": exitCode,
		"log_bytes": logBytes, "log_truncated": truncated,
		"error": errMsg, "finished_at": now,
	}
	res := s.DB.Model(&model.ExecRun{}).Where("id = ? AND status IN ?", id, []string{model.ExecQueued, model.ExecRunning}).Updates(fields)
	if res.Error != nil {
		return res.Error
	}
	if res.RowsAffected == 0 {
		// 已经被取消了（或本来就终态）：不要把它从 canceled 改成 succeeded ——
		// 「取消」是用户意志，不该被后到的完成事件覆盖。
		return gorm.ErrRecordNotFound
	}
	return nil
}

// CancelExecRun 取消：把还在排队/运行的行置为 canceled，返回是否真的改到了。
//
// 进程的杀掉由调用方做（store 不该知道怎么 kill）。
func (s *Store) CancelExecRun(id, ownerID string) (bool, error) {
	q := s.DB.Model(&model.ExecRun{}).Where("id = ? AND status IN ?", id, []string{model.ExecQueued, model.ExecRunning})
	if ownerID != "" {
		q = q.Where("owner_id = ?", ownerID)
	}
	now := time.Now()
	res := q.Updates(map[string]any{"status": model.ExecCanceled, "finished_at": now, "error": "用户取消"})
	if res.Error != nil {
		return false, res.Error
	}
	return res.RowsAffected > 0, nil
}

// DeleteExecRun 删记录（工作目录与日志由调用方清）。
func (s *Store) DeleteExecRun(id, ownerID string) error {
	q := s.DB.Where("id = ?", id)
	if ownerID != "" {
		q = q.Where("owner_id = ?", ownerID)
	}
	return q.Delete(&model.ExecRun{}).Error
}
