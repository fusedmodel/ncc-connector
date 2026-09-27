package store

import (
	"time"

	"github.com/fusedmodel/ncc-registry/model"
)

// EnsureBuiltinCollection 保证这个命名空间里有那个**内置集合**的声明行，并返回它。
//
// 为什么需要"保证"这一步：内置集合的声明是代码里的常量（`model.BuiltinCollections`），
// 而记录要挂在一行 `collections` 上。所以任何一个内置内容的读写入口，
// 都先调用这里 —— 声明不存在就按常量建出来（幂等）。
//
// 这也是"取用即声明"：一台节点上一个记忆都没写过时，`ncc store ls` 里没有 `mem`；
// 第一次 `ncc mem set` 之后它才出现，并且**从此带上字段与红线**。
func (s *Store) EnsureBuiltinCollection(nsID, kind string) (*model.Collection, error) {
	decl, ok := model.BuiltinDeclOf(kind)
	if !ok {
		return nil, ErrNotBuiltin
	}
	if c, err := s.GetCollection(nsID, decl.Kind); err == nil {
		return c, nil
	}
	col := &model.Collection{
		ID: NewID("C-"), NamespaceID: nsID, Kind: decl.Kind,
		Title: decl.Title, Summary: decl.Summary, Reason: decl.Reason,
		Mutable: !decl.AppendOnly, History: !decl.AppendOnly, AppendOnly: decl.AppendOnly,
		Visibility: decl.Visibility, MaxBytes: model.StoreMaxBytesDefault,
		Fields: JSONList(decl.Fields), Index: JSONList(decl.Index),
		DedupeBy: "", DefaultTTL: 0,
		Status: model.StoreStatusActive, CreatedBy: "", CreatedAt: time.Now(), UpdatedAt: time.Now(),
	}
	if col.Visibility == "" {
		col.Visibility = "private"
	}
	saved, _, err := s.UpsertCollection(col)
	return saved, err
}
