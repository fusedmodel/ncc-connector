package model

import (
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
	"time"
)

// ============================ 通用记录仓（NCC Store） ============================
//
// 为什么要有这一层（而不是给 issue / log 各写一套 handler）：
// 知识库 / 记忆 / 检查点 / 轨迹是四类内容，但它们的**机械部分**是同一件事 ——
// 归属命名空间、按 key 取、分页、标签、可见性、版本、上限、过期、软删。
// 四类内容各写一遍，就等于把同一段代码抄四遍；再加一类内容（issue / log / …）再抄一遍。
//
// 所以分工是：
//   · **集合（Collection）= 声明**：一类内容叫什么、可变不可变、字段有哪些、谁能看见、上限多少。
//     **服务端不认识业务字段** —— 它只读这份声明。
//   · **记录（Record）= 数据**：所有内容类型共享的机械部分放在这里。
//
// 于是新增一类内容**不需要改服务端**：声明一个集合即可。
//
// ⚠️ 三条红线（写在模型上，别在 handler 里破掉）：
//  1. **动态 ≠ 无模式**：没在 `Fields` 里声明的字段写不进来；没在 `Index` 里声明的字段不能当过滤条件。
//  2. **不可变就是不可变**：`Mutable=false` / `AppendOnly=true` 的集合**没有 PUT** ——
//     审计日志、轨迹、快照这类内容被改掉一次，它的价值就归零了。
//  3. **公开档逐个集合决定**：集合声明里没写 `public`，这个集合里的记录**永远**不会匿名可见，
//     哪怕记录自己写了 visibility=public。

// 集合名 / key 的语法：小写字母数字与 `-_.`，都以字母开头。
var collectionKindRe = regexp.MustCompile(`^[a-z][a-z0-9._-]{0,47}$`)

// 记录字段类型白名单。**故意很短**：类型只用来做写入校验，不是给人当 ORM 用的。
var fieldTypes = map[string]bool{
	"string": true, "text": true, "int": true, "bool": true, "string[]": true, "ref": true,
}

// 上限（与 kb / mem 同一量级，但通用仓只存**内联文本**）。
const (
	StoreMaxBytesDefault = 256 << 10 // 256KB
	StoreMaxBytesHard    = 1 << 20   // 1MB：再大就走制品或 ckpt 的 blob
	StoreMaxFields       = 32
	StoreMaxRecords      = 100000 // 单集合记录数（防"什么都往里塞"）
	StoreKeyMaxLen       = 96
	// StoreSearchCandidates 带关键词检索时**最多取回多少条候选**再排序。
	//
	// 为什么有上限：排序得在候选集上做（在"已经切好的一页"上排序等于没排 ——
	// 最相关的那条可能在第二页）。超出部分按更新时间截断，这是**明说的取舍**：
	// 要全库排序得引入真索引，而那是另一个决定（也会把数据库方言依赖引进来）。
	StoreSearchCandidates = 500
)

// Collection 一个集合（一类内容）的声明。
type Collection struct {
	ID          string `gorm:"primaryKey"`
	NamespaceID string `gorm:"index;not null"`
	Kind        string `gorm:"index;not null"` // 集合名：issue / log / note / …
	Title       string ``
	Summary     string ``
	// Reason：为什么要有这一类内容（人看的）。
	//
	// 包那边的 `state.stores[].reason` 是「**这个包**为什么需要它」，
	// 这里是「**这台节点**为什么提供它」—— 同一个词、两个主体，都该有人写下来。
	Reason     string    ``
	Mutable    bool      `` // 可变（改 = 新版本）；false = 不可变
	History    bool      `` // 可变时是否留历史
	AppendOnly bool      `` // 只追加：POST 可以，PUT 不行
	Visibility string    `` // public | private（默认 private）
	MaxBytes   int64     ``
	Fields     string    `` // JSON: {"title":"string","status":"enum:open|closed"}
	Index      string    `` // JSON: ["status","labels"] —— 允许作为过滤条件的字段
	DedupeBy   string    `` // 幂等键：空 | "checksum"（同 checksum 重复提交算 duplicate）
	DefaultTTL int64     `` // 天（0 = 不过期）
	Status     string    `` // active | archived（归档 = 没删，但默认不列出）
	CreatedBy  string    ``
	CreatedAt  time.Time ``
	UpdatedAt  time.Time ``
}

func (Collection) TableName() string { return "collections" }

// Record 一条记录：所有内容类型共享的机械部分。
//
// `Body` 是**内联文本**：通用仓不做二进制大对象 —— 那是制品（对象存储）与检查点（blob）
// 的地盘。这里要的是"能被模型读、能被 diff、能被校验"的小内容。
type Record struct {
	ID           string `gorm:"primaryKey"`
	CollectionID string `gorm:"index;not null"`
	NamespaceID  string `gorm:"index;not null"`
	Key          string `gorm:"index;not null"`
	Revision     int64  ``
	Checksum     string ``
	Size         int64  ``
	Body         string ``
	// Data 按**集合声明校验过**的字段值（JSON 对象）。
	//
	// 与 `Meta` 分开存是有意的：Data 是"按契约填的表"，Meta 是逃生口。
	// 混在一起以后就分不清"这个字段是设计里的"还是"随手塞的"，
	// 而"没声明的字段不能当过滤条件"这条红线也就无从谈起。
	Data       string ``
	Meta       string `` // JSON：自由结构（**不参与过滤**，是逃生口不是索引）
	Tags       string `` // JSON 数组
	Visibility string ``
	Status     string `` // active | archived
	Source     string ``
	SearchText string `` // 由已声明的字段拼出来的可搜索文本（`?q=` 用）
	// LastNote：写进**当前版本**的那次写入给的备注。
	//
	// 为什么记录里也要存一份：历史表里只有"被换掉的那些版本"的行，
	// 当前版本没有行 —— 不存这一份，建记录时写的备注就永远看不到了
	// （"我写的备注去哪了？"）。于是约定：**每个版本的备注跟着它自己的版本走**。
	LastNote  string     ``
	ExpiresAt *time.Time ``
	CreatedBy string     ``
	UpdatedBy string     ``
	CreatedAt time.Time  ``
	UpdatedAt time.Time  ``
}

func (Record) TableName() string { return "records" }

// RecordRevision 可变集合的历史版本（**只记元数据，不记正文**）。
//
// 为什么不存正文：那是另一个"会被撑爆的表"。真要留全文，用快照包（`nur kb export --as-package`
// 那条路）把它固化成一个可签名的制品，比在库里存 N 份副本更诚实。
type RecordRevision struct {
	ID        string `gorm:"primaryKey"`
	RecordID  string `gorm:"index;not null"`
	Revision  int64  ``
	Checksum  string ``
	Size      int64  ``
	ChangedBy string ``
	// Note：**写进这个版本时**给的备注（而不是"后来把它换掉时"的备注）。
	//
	// 这两种读法都能自圆其说，所以必须挑一个说清楚 —— 挑"跟着版本走"，
	// 因为这样每个版本都有备注、一个都不丢（包括最初建的那一版）。
	Note      string    ``
	CreatedAt time.Time ``
}

func (RecordRevision) TableName() string { return "record_revisions" }

// RecordIndex 索引条目：**只给声明过的字段建**。
//
// 为什么不用 JSON 查询函数：那是一处数据库方言依赖，而且 Sqlite / MySQL / Postgres 的写法各不同。
// 一张显式的索引表与方言无关，还把"没声明的字段不索引"这条红线**写进了数据模型** ——
// 表里没有那一行，就永远搜不到它。
type RecordIndex struct {
	ID           string `gorm:"primaryKey"`
	RecordID     string `gorm:"index;not null"`
	CollectionID string `gorm:"index;not null"`
	Field        string `gorm:"index;not null"`
	Value        string `gorm:"index;not null"`
}

func (RecordIndex) TableName() string { return "record_index" }

// ValidCollectionKind 集合名是否合法。
func ValidCollectionKind(k string) bool { return collectionKindRe.MatchString(k) }

// ValidRecordKey 记录 key 是否合法（`[a-z0-9._-]`，不以下划线/点开头）。
func ValidRecordKey(k string) bool {
	k = strings.TrimSpace(k)
	if k == "" || len(k) > StoreKeyMaxLen {
		return false
	}
	if !collectionKindRe.MatchString(k) {
		return false
	}
	return true
}

// FieldSpec 一个声明字段：`string` / `text` / `int` / `bool` / `string[]` / `ref` / `enum:a|b`。
type FieldSpec struct {
	// ⚠️ 这几个 tag 不是装饰：没有它们，Go 的字段名会原样变成 JSON 键
	// （`Name` / `Type` / `Enum`…），而这份响应的其它键全是 camelCase ——
	// 于是客户端得靠"猜大小写"来读声明。**接口是契约，别让语言的字段名漏出去。**
	Name    string   `json:"name"`
	Type    string   `json:"type"`
	Enum    []string `json:"enum,omitempty"`
	Search  bool     `json:"search,omitempty"`  // 这个字段的正文进 SearchText（`?q=` 能搜到）
	Require bool     `json:"require,omitempty"` // 写入必填
}

// ParseFieldSpec 解析一条字段声明。
//
// 语法故意简单到"看一眼就懂"：
//
//	"title:string"            必填？不，默认可选
//	"status:enum:open|closed"
//	"labels:string[]"
//	"body:text?search"
//	"owner:ref!"
//
// 尾部 `!` = 必填，`?search` = 进搜索文本。
func ParseFieldSpec(raw string) (FieldSpec, error) {
	s := strings.TrimSpace(raw)
	if s == "" {
		return FieldSpec{}, fmt.Errorf("空字段声明")
	}
	name := s
	typ := "string"
	if i := strings.Index(s, ":"); i > 0 {
		name, typ = s[:i], s[i+1:]
	}
	name = strings.TrimSpace(name)
	if !ValidCollectionKind(name) {
		return FieldSpec{}, fmt.Errorf("字段名「%s」不合法（小写字母数字与 -_.，字母开头）", name)
	}
	fs := FieldSpec{Name: name}
	// 尾修饰
	for {
		switch {
		case strings.HasSuffix(typ, "!"):
			fs.Require = true
			typ = strings.TrimSuffix(typ, "!")
		case strings.HasSuffix(typ, "?search"):
			fs.Search = true
			typ = strings.TrimSuffix(typ, "?search")
		default:
			goto done
		}
	}
done:
	typ = strings.TrimSpace(typ)
	if strings.HasPrefix(typ, "enum:") {
		vals := strings.Split(strings.TrimPrefix(typ, "enum:"), "|")
		for i := range vals {
			vals[i] = strings.TrimSpace(vals[i])
			if vals[i] == "" {
				return FieldSpec{}, fmt.Errorf("字段「%s」的 enum 里有空值", name)
			}
		}
		fs.Type = "enum"
		fs.Enum = vals
		return fs, nil
	}
	if !fieldTypes[typ] {
		return FieldSpec{}, fmt.Errorf("字段「%s」的类型「%s」不在白名单（string/text/int/bool/string[]/ref/enum:a|b）", name, typ)
	}
	fs.Type = typ
	return fs, nil
}

// ParseFields 解析集合的字段声明数组。
func ParseFields(raw []string) ([]FieldSpec, error) {
	if len(raw) > StoreMaxFields {
		return nil, fmt.Errorf("字段最多 %d 个（收到 %d）—— 一个集合不是一张宽表", StoreMaxFields, len(raw))
	}
	out := make([]FieldSpec, 0, len(raw))
	seen := map[string]bool{}
	for _, r := range raw {
		fs, err := ParseFieldSpec(r)
		if err != nil {
			return nil, err
		}
		if seen[fs.Name] {
			return nil, fmt.Errorf("字段「%s」重复声明", fs.Name)
		}
		seen[fs.Name] = true
		out = append(out, fs)
	}
	return out, nil
}

// DecodeFields 把存起来的 JSON 还原成字段声明。
func DecodeFields(s string) []FieldSpec {
	if strings.TrimSpace(s) == "" {
		return nil
	}
	var raw []string
	if err := json.Unmarshal([]byte(s), &raw); err != nil {
		return nil
	}
	out, err := ParseFields(raw)
	if err != nil {
		return nil
	}
	return out
}

// DecodeStringList 把存起来的 JSON 数组还原（失败返回空，不抛 —— 展示路径不该因为脏数据炸）。
func DecodeStringList(s string) []string {
	if strings.TrimSpace(s) == "" {
		return []string{}
	}
	var out []string
	if err := json.Unmarshal([]byte(s), &out); err != nil {
		return []string{}
	}
	return out
}

// 记录与集合的状态取值。
const (
	StoreStatusActive   = "active"
	StoreStatusArchived = "archived"
)

// StoreVisibility 集合声明里允许的可见性。
var StoreVisibility = []string{"public", "private"}

// ValidStoreVisibility 可见性是否合法。
func ValidStoreVisibility(v string) bool {
	for _, x := range StoreVisibility {
		if x == v {
			return true
		}
	}
	return false
}

// StoreDedupe 幂等键取值（只支持 checksum：同内容重复提交算 duplicate）。
var StoreDedupe = []string{"", "checksum"}

// Expired 读时判过期（与记忆同一口径：过期即视为不存在，清理是另一个动作）。
func (r *Record) Expired(now time.Time) bool {
	return r.ExpiresAt != nil && !r.ExpiresAt.After(now)
}
