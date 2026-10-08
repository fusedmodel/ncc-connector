//! 通用记录仓（NCC Store）的数据访问：集合声明、记录、历史版本、索引行。
//!
//! 原实现：`ncc-registry/store/records.go`。这一层**只做机械部分** ——
//! 按 (集合, key) 找、分页、版本、过期、软删、幂等；它不认识 issue / log / kb
//! 这些词，那些属于「集合声明」（`crate::httpapi::records`）。
//!
//! 刻意的取舍：
//!
//! * **不可变判断放在数据层**：`RecordInput::immutable` 由调用方传入，而不是只在
//!   PUT 那条路由上判 —— 只在一条路由上判，换个动词（拿 POST 同一个 key）就能把
//!   `append_only` 集合刷成 rev 2。不变量得待在数据层，才不依赖「你从哪个门进来」。
//! * **内容没变就不动版本**：同 checksum 的重复提交算 `duplicate`（幂等重试不报错），
//!   且这一步排在不可变判断**之前** —— 只追加集合收到一模一样的一条不该报错，
//!   因为没有任何东西被改过。
//! * **历史只记元数据**：`record_revisions` 存的是「谁在什么时候写成了哪个摘要」，
//!   不存正文副本 —— 真要留全文就去打快照包，否则这张表迟早被撑爆。
//! * **过滤只走 `record_index` 显式表**：不用 JSON 查询函数（那是数据库方言依赖），
//!   而且没进声明的字段在表里根本没有行，「没声明就不能过滤」这条红线写进了数据模型。
//! * **时间列当 TEXT**：写用 `now_go()`、比较也走文本比较（同机同格式，字典序即时间序），
//!   不给 sqlx 映射 chrono 类型 —— 映射错了会变成「读写都能跑、老数据一读就炸」。

use std::collections::HashMap;
use std::sync::OnceLock;

use sqlx::SqlitePool;

use ncc_core::crypto::sha256_hex;
use ncc_core::ids::new_id;
use ncc_core::timeutil::now_go;

/// 把绑定值依次挂到查询上（写成宏是因为 sqlx 的 `Query` 类型带生命周期，函数签名很啰嗦）。
macro_rules! apply_binds {
    ($q:expr, $binds:expr) => {{
        let mut q = $q;
        for b in $binds.iter() {
            q = match b {
                Bind::S(s) => q.bind(s.clone()),
                Bind::I(i) => q.bind(*i),
            };
        }
        q
    }};
}

/// 上限（与 Go `model` 里的常量同值，改这里就要改那边）。
pub const MAX_BYTES_DEFAULT: i64 = 256 << 10;
pub const MAX_BYTES_HARD: i64 = 1 << 20;
pub const MAX_FIELDS: usize = 32;
pub const MAX_RECORDS: i64 = 100_000;
pub const KEY_MAX_LEN: usize = 96;
/// 带关键词检索时最多取回多少条候选再排序（见 `list_records` 的说明）。
pub const SEARCH_CANDIDATES: i64 = 500;

pub const STATUS_ACTIVE: &str = "active";
pub const STATUS_ARCHIVED: &str = "archived";

/// 数据层错误：调用方按变体翻成 4xx（**不静默覆盖**）。
#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    /// 这个集合没有「改」这条路（`mutable=false` 或 `append_only=true`）。
    #[error("immutable")]
    Immutable,
    /// 乐观并发失败：别人先改了。
    #[error("conflict")]
    Conflict,
    /// 记录数到顶了。
    #[error("too_many")]
    TooMany,
    #[error("db: {0}")]
    Db(#[from] sqlx::Error),
}

/// 集合名 / 记录 key 的语法：小写字母数字与 `-_.`，都以字母开头。
fn kind_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[a-z][a-z0-9._-]{0,47}$").expect("静态正则合法"))
}

pub fn valid_kind(k: &str) -> bool {
    kind_re().is_match(k)
}

/// 记录 key 是否合法（小写字母数字与 `-_.`，字母开头，≤96）。
pub fn valid_record_key(k: &str) -> bool {
    let k = k.trim();
    if k.is_empty() || k.len() > KEY_MAX_LEN {
        return false;
    }
    kind_re().is_match(k)
}

/* ---------------- 集合 ---------------- */

/// `collections` 一行。
///
/// 文本列在 DDL 里是可空的（GORM 的 `text` 没有 NOT NULL），所以都用 `Option` 兜 ——
/// 一行残缺的历史数据不该把整个列表接口打成 500；取值一律走下面那几个访问器。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Collection {
    pub id: String,
    pub namespace_id: String,
    pub kind: String,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub reason: Option<String>,
    pub mutable: Option<i64>,
    pub history: Option<i64>,
    pub append_only: Option<i64>,
    pub visibility: Option<String>,
    pub max_bytes: Option<i64>,
    pub fields: Option<String>,
    pub index: Option<String>,
    pub dedupe_by: Option<String>,
    pub default_ttl: Option<i64>,
    pub status: Option<String>,
    pub created_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl Collection {
    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or_default()
    }
    pub fn summary(&self) -> &str {
        self.summary.as_deref().unwrap_or_default()
    }
    pub fn reason(&self) -> &str {
        self.reason.as_deref().unwrap_or_default()
    }
    pub fn mutable(&self) -> bool {
        self.mutable.unwrap_or(0) != 0
    }
    pub fn history(&self) -> bool {
        self.history.unwrap_or(0) != 0
    }
    pub fn append_only(&self) -> bool {
        self.append_only.unwrap_or(0) != 0
    }
    pub fn visibility(&self) -> &str {
        match self.visibility.as_deref() {
            Some(v) if !v.is_empty() => v,
            _ => "private",
        }
    }
    pub fn max_bytes(&self) -> i64 {
        self.max_bytes.unwrap_or(MAX_BYTES_DEFAULT)
    }
    pub fn fields_raw(&self) -> &str {
        self.fields.as_deref().unwrap_or_default()
    }
    pub fn index_raw(&self) -> &str {
        self.index.as_deref().unwrap_or_default()
    }
    pub fn dedupe_by(&self) -> &str {
        self.dedupe_by.as_deref().unwrap_or_default()
    }
    pub fn default_ttl(&self) -> i64 {
        self.default_ttl.unwrap_or(0)
    }
    pub fn status(&self) -> &str {
        match self.status.as_deref() {
            Some(v) if !v.is_empty() => v,
            _ => STATUS_ACTIVE,
        }
    }
    pub fn created_by(&self) -> &str {
        self.created_by.as_deref().unwrap_or_default()
    }
    /// 不可变 = 没有「改」这条路（`mutable=false` 或 `append_only=true`）。
    pub fn immutable(&self) -> bool {
        !self.mutable() || self.append_only()
    }
}

const COLLECTION_COLS: &str = "id, namespace_id, kind, title, summary, reason, mutable, history, \
append_only, visibility, max_bytes, fields, `index`, dedupe_by, default_ttl, status, created_by, \
created_at, updated_at";

/// 声明（或更新）一个集合要给的字段。
pub struct CollectionInput {
    pub namespace_id: String,
    pub kind: String,
    pub title: String,
    pub summary: String,
    pub reason: String,
    pub mutable: bool,
    pub history: bool,
    pub append_only: bool,
    pub visibility: String,
    pub max_bytes: i64,
    pub fields: String,
    pub index: String,
    pub dedupe_by: String,
    pub default_ttl: i64,
    pub created_by: String,
}

/// 声明（或更新）一个集合。`kind` 在命名空间内唯一，返回 (集合, 是否新建)。
///
/// 更新时只动「契约面」（字段、索引、可变性、上限、可见性、TTL）：
/// **不改归属**（命名空间与 kind 是身份），也不动已有记录。
pub async fn upsert_collection(
    pool: &SqlitePool,
    c: &CollectionInput,
) -> Result<(Collection, bool), sqlx::Error> {
    let kind = normal_kind(&c.kind);
    if let Some(old) = get_collection(pool, &c.namespace_id, &kind).await? {
        sqlx::query(
            "UPDATE collections SET title = ?, summary = ?, reason = ?, mutable = ?, history = ?, \
append_only = ?, visibility = ?, max_bytes = ?, fields = ?, `index` = ?, dedupe_by = ?, \
default_ttl = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&c.title)
        .bind(&c.summary)
        .bind(&c.reason)
        .bind(c.mutable as i64)
        .bind(c.history as i64)
        .bind(c.append_only as i64)
        .bind(&c.visibility)
        .bind(c.max_bytes)
        .bind(&c.fields)
        .bind(&c.index)
        .bind(&c.dedupe_by)
        .bind(c.default_ttl)
        .bind(now_go())
        .bind(&old.id)
        .execute(pool)
        .await?;
        let saved = get_collection_by_id(pool, &old.id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        return Ok((saved, false));
    }
    let now = now_go();
    let id = new_id("C-");
    sqlx::query(
        "INSERT INTO collections (id, namespace_id, kind, title, summary, reason, mutable, history, \
append_only, visibility, max_bytes, fields, `index`, dedupe_by, default_ttl, status, created_by, \
created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&c.namespace_id)
    .bind(&kind)
    .bind(&c.title)
    .bind(&c.summary)
    .bind(&c.reason)
    .bind(c.mutable as i64)
    .bind(c.history as i64)
    .bind(c.append_only as i64)
    .bind(&c.visibility)
    .bind(c.max_bytes)
    .bind(&c.fields)
    .bind(&c.index)
    .bind(&c.dedupe_by)
    .bind(c.default_ttl)
    .bind(STATUS_ACTIVE)
    .bind(&c.created_by)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    let saved = get_collection_by_id(pool, &id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    Ok((saved, true))
}

fn normal_kind(kind: &str) -> String {
    kind.trim().to_lowercase()
}

/// 按 (命名空间, kind) 取集合。
pub async fn get_collection(
    pool: &SqlitePool,
    ns_id: &str,
    kind: &str,
) -> Result<Option<Collection>, sqlx::Error> {
    let sql =
        format!("SELECT {COLLECTION_COLS} FROM collections WHERE namespace_id = ? AND kind = ?");
    sqlx::query_as::<_, Collection>(&sql)
        .bind(ns_id)
        .bind(normal_kind(kind))
        .fetch_optional(pool)
        .await
}

pub async fn get_collection_by_id(
    pool: &SqlitePool,
    id: &str,
) -> Result<Option<Collection>, sqlx::Error> {
    let sql = format!("SELECT {COLLECTION_COLS} FROM collections WHERE id = ?");
    sqlx::query_as::<_, Collection>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// 列集合的条件。
pub struct CollectionListOpts {
    /// 指定命名空间（`?namespace=`）；给了就只看它。
    pub namespace_id: Option<String>,
    /// 调用方**能读**的命名空间 id（空 = 匿名/无归属）。
    pub namespace_ids: Vec<String>,
    /// 管理员：不限命名空间。
    pub all: bool,
}

/// 列集合（归档的不列出）。
pub async fn list_collections(
    pool: &SqlitePool,
    o: &CollectionListOpts,
) -> Result<Vec<Collection>, sqlx::Error> {
    let mut wheres = vec!["status <> ?".to_string()];
    let mut binds: Vec<Bind> = vec![Bind::S(STATUS_ARCHIVED.to_string())];
    match &o.namespace_id {
        Some(id) => {
            wheres.push("namespace_id = ?".to_string());
            binds.push(Bind::S(id.clone()));
        }
        None if !o.all => {
            if o.namespace_ids.is_empty() {
                wheres.push("visibility = ?".to_string());
                binds.push(Bind::S("public".to_string()));
            } else {
                wheres.push(format!(
                    "(namespace_id IN ({}) OR visibility = ?)",
                    placeholders(o.namespace_ids.len())
                ));
                for id in &o.namespace_ids {
                    binds.push(Bind::S(id.clone()));
                }
                binds.push(Bind::S("public".to_string()));
            }
        }
        None => {}
    }
    let sql = format!(
        "SELECT {COLLECTION_COLS} FROM collections WHERE {} ORDER BY kind ASC",
        wheres.join(" AND ")
    );
    apply_binds!(sqlx::query_as::<_, Collection>(&sql), binds)
        .fetch_all(pool)
        .await
}

/// 归档 / 恢复集合（归档不删记录）。
pub async fn set_collection_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE collections SET status = ?, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(now_go())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 集合里有多少条（只算 `active`，与 Go 的 `CountRecords` 同口径）。
pub async fn count_records(pool: &SqlitePool, collection_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM records WHERE collection_id = ? AND status = ?")
        .bind(collection_id)
        .bind(STATUS_ACTIVE)
        .fetch_one(pool)
        .await
}

/// 通用记录仓的规模：集合数 + 记录数（`/api/meta` 的 `counts.collections/records` 用它）。
///
/// 与 Go 的 `StoreCounts` 同口径：集合只算未归档的（`status <> 'archived'`），
/// 记录只算 `active` 的 —— 归档的集合与记录不该继续占容量预估。
pub async fn store_counts(pool: &SqlitePool) -> Result<(i64, i64), sqlx::Error> {
    let collections: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM collections WHERE status <> ?")
        .bind(STATUS_ARCHIVED)
        .fetch_one(pool)
        .await?;
    let records: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM records WHERE status = ?")
        .bind(STATUS_ACTIVE)
        .fetch_one(pool)
        .await?;
    Ok((collections, records))
}

/// 这些人名下的命名空间 id（被授权者列表展开成「可读范围」）。
pub async fn namespace_ids_of_owners(
    pool: &SqlitePool,
    owner_ids: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    if owner_ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT id FROM namespaces WHERE owner_id IN ({})",
        placeholders(owner_ids.len())
    );
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for o in owner_ids {
        q = q.bind(o);
    }
    q.fetch_all(pool).await
}

/* ---------------- 记录 ---------------- */

/// `records` 一行。可空列同样用 `Option` 兜着，取值走访问器。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Record {
    pub id: String,
    pub collection_id: String,
    pub namespace_id: String,
    pub key: String,
    pub revision: Option<i64>,
    pub checksum: Option<String>,
    pub size: Option<i64>,
    pub body: Option<String>,
    pub data: Option<String>,
    pub meta: Option<String>,
    pub tags: Option<String>,
    pub visibility: Option<String>,
    pub status: Option<String>,
    pub source: Option<String>,
    pub search_text: Option<String>,
    pub last_note: Option<String>,
    pub expires_at: Option<String>,
    pub created_by: Option<String>,
    pub updated_by: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl Record {
    pub fn revision(&self) -> i64 {
        self.revision.unwrap_or(0)
    }
    pub fn checksum(&self) -> &str {
        self.checksum.as_deref().unwrap_or_default()
    }
    pub fn size(&self) -> i64 {
        self.size.unwrap_or(0)
    }
    pub fn body(&self) -> &str {
        self.body.as_deref().unwrap_or_default()
    }
    pub fn data_raw(&self) -> &str {
        self.data.as_deref().unwrap_or_default()
    }
    pub fn meta_raw(&self) -> &str {
        self.meta.as_deref().unwrap_or_default()
    }
    pub fn tags_raw(&self) -> &str {
        self.tags.as_deref().unwrap_or_default()
    }
    pub fn visibility(&self) -> &str {
        match self.visibility.as_deref() {
            Some(v) if !v.is_empty() => v,
            _ => "private",
        }
    }
    pub fn status(&self) -> &str {
        match self.status.as_deref() {
            Some(v) if !v.is_empty() => v,
            _ => STATUS_ACTIVE,
        }
    }
    pub fn source(&self) -> &str {
        self.source.as_deref().unwrap_or_default()
    }
    pub fn search_text(&self) -> &str {
        self.search_text.as_deref().unwrap_or_default()
    }
    pub fn last_note(&self) -> &str {
        self.last_note.as_deref().unwrap_or_default()
    }
    pub fn created_by(&self) -> &str {
        self.created_by.as_deref().unwrap_or_default()
    }
    pub fn updated_by(&self) -> &str {
        self.updated_by.as_deref().unwrap_or_default()
    }
    /// 读时判过期（过期即视为不存在；清理是另一个动作）。
    pub fn expired_at(&self, now: chrono::DateTime<chrono::FixedOffset>) -> bool {
        self.expires_at
            .as_deref()
            .and_then(ncc_core::timeutil::parse_time)
            .map(|t| t <= now)
            .unwrap_or(false)
    }
}

const RECORD_COLS: &str =
    "id, collection_id, namespace_id, `key`, revision, checksum, size, body, \
data, meta, tags, visibility, status, source, search_text, last_note, expires_at, created_by, \
updated_by, created_at, updated_at";

/// 写入一条记录要给的字段（**声明校验在 HTTP 层**，这里不认识字段语义）。
pub struct RecordInput {
    pub collection_id: String,
    pub namespace_id: String,
    pub key: String,
    pub body: String,
    pub data: String,
    pub meta: String,
    pub tags: String,
    pub visibility: String,
    pub status: String,
    pub source: String,
    pub search_text: String,
    pub expires_at: Option<String>,
    pub user_id: String,
    /// 非 0 时做乐观并发：库里的 revision 与它不等就冲突。
    pub expect_revision: i64,
    /// 写进历史的备注（跟着**它自己的版本**走）。
    pub note: String,
    /// 这个集合没有「改」这条路 —— 由调用方按集合声明传入。
    pub immutable: bool,
}

/// 建或改一条记录，返回 (记录, 是否新建, 是否幂等重复)。
pub async fn upsert_record(
    pool: &SqlitePool,
    input: &RecordInput,
) -> Result<(Record, bool, bool), RecordError> {
    let now = now_go();
    let sum = sha256_hex(input.body.as_bytes());

    let Some(cur) = get_record_by_key(pool, &input.collection_id, &input.key).await? else {
        let n = count_records(pool, &input.collection_id).await?;
        if n >= MAX_RECORDS {
            return Err(RecordError::TooMany);
        }
        let id = new_id("R-");
        let res = sqlx::query(
            "INSERT INTO records (id, collection_id, namespace_id, `key`, revision, checksum, size, \
body, data, meta, tags, visibility, status, source, search_text, last_note, expires_at, created_by, \
updated_by, created_at, updated_at) VALUES (?, ?, ?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&input.collection_id)
        .bind(&input.namespace_id)
        .bind(&input.key)
        .bind(&sum)
        .bind(input.body.len() as i64)
        .bind(&input.body)
        .bind(&input.data)
        .bind(&input.meta)
        .bind(&input.tags)
        .bind(&input.visibility)
        .bind(&input.status)
        .bind(&input.source)
        .bind(&input.search_text)
        .bind(&input.note)
        .bind(&input.expires_at)
        .bind(&input.user_id)
        .bind(&input.user_id)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await;
        if let Err(e) = res {
            // 并发插入同 key：唯一索引挡下，转成冲突（不是 500）。
            if is_unique_violation(&e) {
                return Err(RecordError::Conflict);
            }
            return Err(RecordError::Db(e));
        }
        let rec = get_record_by_id(pool, &id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)?;
        return Ok((rec, true, false));
    };

    if input.expect_revision != 0 && cur.revision() != input.expect_revision {
        return Err(RecordError::Conflict);
    }
    // 内容没变就不动版本：反复保存同一个值不该把历史刷满。
    // 这一步排在不可变判断之前 —— 只追加集合收到「一模一样的一条」算幂等重复。
    if cur.checksum() == sum {
        return Ok((cur, false, true));
    }
    if input.immutable {
        return Err(RecordError::Immutable);
    }

    // 历史（只记元数据）。备注跟着**它自己的版本**走：这一行描述正在被换掉的 rev N，
    // 它的备注就是当初写 rev N 时给的备注（当前版本那份存在 `records.last_note`）。
    sqlx::query(
        "INSERT INTO record_revisions (id, record_id, revision, checksum, size, changed_by, note, \
created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id("RV-"))
    .bind(&cur.id)
    .bind(cur.revision())
    .bind(cur.checksum())
    .bind(cur.size())
    .bind(&input.user_id)
    .bind(cur.last_note())
    .bind(&now)
    .execute(pool)
    .await?;

    sqlx::query(
        "UPDATE records SET revision = ?, checksum = ?, size = ?, body = ?, data = ?, meta = ?, \
tags = ?, visibility = ?, status = ?, source = ?, search_text = ?, expires_at = ?, last_note = ?, \
updated_by = ?, updated_at = ? WHERE id = ?",
    )
    .bind(cur.revision() + 1)
    .bind(&sum)
    .bind(input.body.len() as i64)
    .bind(&input.body)
    .bind(&input.data)
    .bind(&input.meta)
    .bind(&input.tags)
    .bind(&input.visibility)
    .bind(&input.status)
    .bind(&input.source)
    .bind(&input.search_text)
    .bind(&input.expires_at)
    .bind(&input.note)
    .bind(&input.user_id)
    .bind(&now)
    .bind(&cur.id)
    .execute(pool)
    .await?;

    let got = get_record_by_id(pool, &cur.id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    Ok((got, false, false))
}

pub async fn get_record_by_id(pool: &SqlitePool, id: &str) -> Result<Option<Record>, sqlx::Error> {
    let sql = format!("SELECT {RECORD_COLS} FROM records WHERE id = ?");
    sqlx::query_as::<_, Record>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn get_record_by_key(
    pool: &SqlitePool,
    collection_id: &str,
    key: &str,
) -> Result<Option<Record>, sqlx::Error> {
    let sql = format!("SELECT {RECORD_COLS} FROM records WHERE collection_id = ? AND `key` = ?");
    sqlx::query_as::<_, Record>(&sql)
        .bind(collection_id)
        .bind(key)
        .fetch_optional(pool)
        .await
}

/// 列记录的条件（**只认声明过的过滤维度**，字段校验在 HTTP 层）。
#[derive(Debug, Clone, Default)]
pub struct RecordListOpts {
    pub collection_id: String,
    /// 可读范围（命名空间 id）。
    pub namespace_ids: Vec<String>,
    /// 匿名：只看公开记录。
    pub public: bool,
    pub q: String,
    pub prefix: String,
    pub tags: Vec<String>,
    pub state: String,
    /// 管理员：不限命名空间。
    pub all: bool,
    pub include_archived: bool,
    pub include_expired: bool,
    /// 声明过的可过滤字段 → 值（走 `record_index` 表）。
    pub indexed: HashMap<String, String>,
    pub page: i64,
    pub size: i64,
    /// 判定过期用的「现在」（同一次请求里只取一次时钟）。
    pub now: String,
}

/// 列记录（分页），返回 (当前页, 总数)。
pub async fn list_records(
    pool: &SqlitePool,
    o: &RecordListOpts,
) -> Result<(Vec<Record>, i64), sqlx::Error> {
    let (where_sql, binds) = record_where(o);

    let count_sql = format!("SELECT COUNT(*) FROM records WHERE {where_sql}");
    let total: i64 = apply_binds!(sqlx::query_scalar::<_, i64>(&count_sql), binds.clone())
        .fetch_one(pool)
        .await?;

    let size = if o.size <= 0 || o.size > 200 {
        50
    } else {
        o.size
    };
    let page = if o.page <= 1 { 1 } else { o.page };

    if !o.q.is_empty() {
        // 带关键词时**先把候选集取回，排完序再切片**：在「已经切好的一页」上排序
        // 等于没排 —— 最相关的那条可能在第二页。候选上限是明说的取舍。
        let sql = format!(
            "SELECT {RECORD_COLS} FROM records WHERE {where_sql} ORDER BY updated_at DESC LIMIT ?"
        );
        let mut b = binds.clone();
        b.push(Bind::I(SEARCH_CANDIDATES));
        let mut cand: Vec<Record> = apply_binds!(sqlx::query_as::<_, Record>(&sql), b)
            .fetch_all(pool)
            .await?;
        rank_records(&mut cand, &o.q);
        let start = ((page - 1) * size).clamp(0, cand.len() as i64);
        let end = (start + size).clamp(0, cand.len() as i64);
        return Ok((cand[start as usize..end as usize].to_vec(), total));
    }

    let sql = format!(
        "SELECT {RECORD_COLS} FROM records WHERE {where_sql} ORDER BY updated_at DESC LIMIT ? OFFSET ?"
    );
    let mut b = binds.clone();
    b.push(Bind::I(size));
    b.push(Bind::I((page - 1) * size));
    let rows: Vec<Record> = apply_binds!(sqlx::query_as::<_, Record>(&sql), b)
        .fetch_all(pool)
        .await?;
    Ok((rows, total))
}

/// 拼出记录的 WHERE 子句与绑定值（count 与 select 共用，避免两处条件漂移）。
fn record_where(o: &RecordListOpts) -> (String, Vec<Bind>) {
    let mut wheres = vec!["collection_id = ?".to_string()];
    let mut binds = vec![Bind::S(o.collection_id.clone())];

    if !o.include_archived {
        wheres.push("status = ?".to_string());
        binds.push(Bind::S(STATUS_ACTIVE.to_string()));
    }
    if !o.include_expired {
        wheres.push("(expires_at IS NULL OR expires_at > ?)".to_string());
        binds.push(Bind::S(o.now.clone()));
    }
    for t in &o.tags {
        // tags 是 JSON 数组文本：用带引号的整体命中，避免前缀误命中别的标签。
        wheres.push("tags LIKE ?".to_string());
        binds.push(Bind::S(format!(
            "%{}%",
            serde_json::to_string(t).unwrap_or_default()
        )));
    }
    if !o.prefix.is_empty() {
        wheres.push("`key` LIKE ?".to_string());
        binds.push(Bind::S(format!("{}%", o.prefix)));
    }
    for (f, v) in &o.indexed {
        wheres.push(
            "id IN (SELECT record_id FROM record_index WHERE collection_id = ? AND field = ? AND value = ?)"
                .to_string(),
        );
        binds.push(Bind::S(o.collection_id.clone()));
        binds.push(Bind::S(f.clone()));
        binds.push(Bind::S(v.clone()));
    }
    if !o.q.is_empty() {
        for t in search_tokens(&o.q) {
            wheres.push(
                "(`key` LIKE ? OR tags LIKE ? OR search_text LIKE ? OR body LIKE ?)".to_string(),
            );
            let like = format!("%{t}%");
            binds.push(Bind::S(like.clone()));
            binds.push(Bind::S(like.clone()));
            binds.push(Bind::S(like.clone()));
            binds.push(Bind::S(like));
        }
    }
    if !o.state.is_empty() {
        wheres.push("status = ?".to_string());
        binds.push(Bind::S(o.state.clone()));
    }
    if !o.all {
        if o.public || o.namespace_ids.is_empty() {
            wheres.push("visibility = ?".to_string());
            binds.push(Bind::S("public".to_string()));
        } else {
            wheres.push(format!(
                "(visibility = ? OR namespace_id IN ({}))",
                placeholders(o.namespace_ids.len())
            ));
            binds.push(Bind::S("public".to_string()));
            for id in &o.namespace_ids {
                binds.push(Bind::S(id.clone()));
            }
        }
    }
    (wheres.join(" AND "), binds)
}

/// 把查询拆成关键词（空白与常见分隔符），去重、转小写、丢空。
///
/// 中英文混排都能用：中文没有词边界，所以按标点与空白切；一个中文词就是一个词。
pub fn search_tokens(q: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in q.split(|c: char| {
        matches!(
            c,
            ' ' | '\t' | '\n' | ',' | '，' | ';' | '；' | '、' | '|' | '/' | '　'
        )
    }) {
        let t = f.trim().to_lowercase();
        if t.is_empty() || out.iter().any(|x| *x == t) {
            continue;
        }
        out.push(t);
    }
    out
}

/// 按命中位置打分并**就地排序**（同分保持传入顺序 = 新的在前）。
///
/// 权重：key 3 · 标签与 `?search` 字段（search_text）2 · 正文 1，每个关键词各算一次。
pub fn rank_records(rows: &mut [Record], q: &str) {
    let tokens = search_tokens(q);
    if tokens.is_empty() {
        return;
    }
    let score: Vec<i64> = rows
        .iter()
        .map(|r| {
            let key = r.key.to_lowercase();
            let tags = r.tags_raw().to_lowercase();
            let search = r.search_text().to_lowercase();
            let body = r.body().to_lowercase();
            let mut n = 0;
            for t in &tokens {
                if key.contains(t.as_str()) {
                    n += 3;
                }
                if tags.contains(t.as_str()) || search.contains(t.as_str()) {
                    n += 2;
                }
                if body.contains(t.as_str()) {
                    n += 1;
                }
            }
            n
        })
        .collect();
    let mut idx: Vec<usize> = (0..rows.len()).collect();
    idx.sort_by(|a, b| score[*b].cmp(&score[*a]));
    let sorted: Vec<Record> = idx.into_iter().map(|i| rows[i].clone()).collect();
    rows.clone_from_slice(&sorted);
}

/// `record_revisions` 一行（**只有元数据**）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RecordRevision {
    pub id: String,
    pub record_id: String,
    pub revision: Option<i64>,
    pub checksum: Option<String>,
    pub size: Option<i64>,
    pub changed_by: Option<String>,
    pub note: Option<String>,
    pub created_at: Option<String>,
}

/// 历史（只记元数据），按版本倒序。
pub async fn record_revisions(
    pool: &SqlitePool,
    record_id: &str,
    limit: i64,
) -> Result<Vec<RecordRevision>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 200 { 50 } else { limit };
    sqlx::query_as::<_, RecordRevision>(
        "SELECT id, record_id, revision, checksum, size, changed_by, note, created_at \
FROM record_revisions WHERE record_id = ? ORDER BY revision DESC LIMIT ?",
    )
    .bind(record_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// 删除一条记录（历史与索引行一并删掉；归档请走状态位）。
pub async fn delete_record(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM record_revisions WHERE record_id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    drop_index(pool, id).await?;
    sqlx::query("DELETE FROM records WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 真删过期记录（读时已判过期，这里只是清垃圾）。`collection_id` 为空 = 不限集合。
pub async fn gc_records(
    pool: &SqlitePool,
    collection_id: &str,
    now: &str,
) -> Result<i64, sqlx::Error> {
    let mut sql =
        "SELECT id FROM records WHERE expires_at IS NOT NULL AND expires_at <= ?".to_string();
    if !collection_id.is_empty() {
        sql.push_str(" AND collection_id = ?");
    }
    let mut q = sqlx::query_scalar::<_, String>(&sql).bind(now);
    if !collection_id.is_empty() {
        q = q.bind(collection_id);
    }
    let ids: Vec<String> = q.fetch_all(pool).await?;
    if ids.is_empty() {
        return Ok(0);
    }
    let ph = placeholders(ids.len());
    for table in ["record_revisions", "record_index"] {
        let sql = format!("DELETE FROM {table} WHERE record_id IN ({ph})");
        let mut q = sqlx::query(&sql);
        for id in &ids {
            q = q.bind(id);
        }
        q.execute(pool).await?;
    }
    let sql = format!("DELETE FROM records WHERE id IN ({ph})");
    let mut q = sqlx::query(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    Ok(q.execute(pool).await?.rows_affected() as i64)
}

/// 改一条记录的状态（归档 / 恢复）。
pub async fn set_record_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    user_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE records SET status = ?, updated_by = ?, updated_at = ? WHERE id = ?")
        .bind(status)
        .bind(user_id)
        .bind(now_go())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/* ---------------- 索引行（只给声明过的字段） ---------------- */

/// 重建一条记录的索引行。
///
/// `indexed` 是集合声明里的可过滤字段；**只给这里出现的字段建行** ——
/// 没声明的字段在这张表里没有行，也就永远过滤不到。
pub async fn set_index(
    pool: &SqlitePool,
    record_id: &str,
    collection_id: &str,
    indexed: &[String],
    vals: &HashMap<String, Vec<String>>,
) -> Result<(), sqlx::Error> {
    drop_index(pool, record_id).await?;
    for f in indexed {
        let Some(list) = vals.get(f) else { continue };
        for v in list {
            if v.trim().is_empty() {
                continue;
            }
            sqlx::query(
                "INSERT INTO record_index (id, record_id, collection_id, field, value) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(new_id("IX-"))
            .bind(record_id)
            .bind(collection_id)
            .bind(f)
            .bind(v)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

pub async fn drop_index(pool: &SqlitePool, record_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM record_index WHERE record_id = ?")
        .bind(record_id)
        .execute(pool)
        .await?;
    Ok(())
}

/* ---------------- 工具 ---------------- */

/// 动态查询的绑定值（文本与整数混排，顺序必须与 SQL 里的 `?` 一致）。
#[derive(Debug, Clone)]
pub enum Bind {
    S(String),
    I(i64),
}

/// `?,?,?`。
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool(tag: &str) -> (SqlitePool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ncc-store-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL)
            .await
            .unwrap();
        (p, dir)
    }

    fn col_input(ns: &str, kind: &str, visibility: &str) -> CollectionInput {
        CollectionInput {
            namespace_id: ns.to_string(),
            kind: kind.to_string(),
            title: "Issue".to_string(),
            summary: String::new(),
            reason: String::new(),
            mutable: true,
            history: true,
            append_only: false,
            visibility: visibility.to_string(),
            max_bytes: MAX_BYTES_DEFAULT,
            fields: r#"["title:string!","status:enum:open|closed?search"]"#.to_string(),
            index: r#"["status"]"#.to_string(),
            dedupe_by: String::new(),
            default_ttl: 0,
            created_by: "U-1".to_string(),
        }
    }

    fn rec_input(col: &Collection, key: &str, body: &str) -> RecordInput {
        RecordInput {
            collection_id: col.id.clone(),
            namespace_id: col.namespace_id.clone(),
            key: key.to_string(),
            body: body.to_string(),
            data: r#"{"status":"open"}"#.to_string(),
            meta: "{}".to_string(),
            tags: r#"["bug"]"#.to_string(),
            visibility: "private".to_string(),
            status: STATUS_ACTIVE.to_string(),
            source: String::new(),
            search_text: "open".to_string(),
            expires_at: None,
            user_id: "U-1".to_string(),
            expect_revision: 0,
            note: "第一版".to_string(),
            immutable: col.immutable(),
        }
    }

    #[test]
    fn 名字与_key_规则() {
        assert!(valid_kind("issue"));
        assert!(valid_kind("kb-2.log"));
        assert!(!valid_kind("Issue")); // 大写不合法（调用方负责 lower）
        assert!(!valid_kind("2issue"));
        assert!(!valid_kind(&"a".repeat(49)));
        assert!(valid_record_key("note-1"));
        assert!(valid_record_key(" a ")); // 内部 trim
        assert!(!valid_record_key(""));
        assert!(!valid_record_key(&"a".repeat(97)));
    }

    #[tokio::test]
    async fn 集合_声明与更新_保留身份与原记录() {
        let (p, _d) = pool("col").await;
        let (c, created) = upsert_collection(&p, &col_input("NS-1", "Issue", "private"))
            .await
            .unwrap();
        assert!(created);
        assert!(c.id.starts_with("C-"));
        assert_eq!(c.kind, "issue"); // 入库时统一小写
        assert!(c.mutable() && c.history() && !c.immutable());

        let (r, is_new, dup) = upsert_record(&p, &rec_input(&c, "a-1", "正文"))
            .await
            .unwrap();
        assert!(is_new && !dup);
        assert_eq!(r.revision(), 1);

        // 再声明一次：同一个 id，只改契约面
        let mut again = col_input("NS-1", "ISSUE", "public");
        again.title = "Issue 2".to_string();
        again.append_only = true;
        let (c2, created) = upsert_collection(&p, &again).await.unwrap();
        assert!(!created);
        assert_eq!(c2.id, c.id);
        assert_eq!(c2.title(), "Issue 2");
        assert_eq!(c2.visibility(), "public");
        assert!(c2.immutable());
        // 记录没被动过
        assert_eq!(count_records(&p, &c.id).await.unwrap(), 1);
        assert!(get_collection(&p, "NS-1", "issue").await.unwrap().is_some());
        assert!(get_collection(&p, "NS-2", "issue").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 记录_幂等_版本与备注跟版本走() {
        let (p, _d) = pool("ver").await;
        let (c, _) = upsert_collection(&p, &col_input("NS-1", "issue", "private"))
            .await
            .unwrap();
        let (r, is_new, dup) = upsert_record(&p, &rec_input(&c, "a-1", "一"))
            .await
            .unwrap();
        assert!(is_new && !dup);
        assert_eq!(r.last_note(), "第一版");

        // 一模一样的内容：算幂等重复，版本不动
        let (r2, is_new, dup) = upsert_record(&p, &rec_input(&c, "a-1", "一"))
            .await
            .unwrap();
        assert!(!is_new && dup);
        assert_eq!(r2.revision(), 1);
        assert!(record_revisions(&p, &r.id, 50).await.unwrap().is_empty());

        // 改内容：版本 +1，旧版本进历史，备注是**当初写那一版时**的备注
        let mut inp = rec_input(&c, "a-1", "二");
        inp.note = "第二版".to_string();
        let (r3, is_new, dup) = upsert_record(&p, &inp).await.unwrap();
        assert!(!is_new && !dup);
        assert_eq!(r3.revision(), 2);
        assert_eq!(r3.last_note(), "第二版");
        assert_eq!(r3.size(), "二".len() as i64);
        let hist = record_revisions(&p, &r.id, 50).await.unwrap();
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].revision, Some(1));
        assert_eq!(hist[0].note.as_deref(), Some("第一版"));
        // 历史只记元数据：没有正文列
        assert_eq!(hist[0].checksum.as_deref(), Some(r.checksum()));
    }

    #[tokio::test]
    async fn 记录_不可变与并发冲突() {
        let (p, _d) = pool("imm").await;
        let mut cin = col_input("NS-1", "ckpt", "private");
        cin.mutable = false;
        cin.append_only = true;
        let (c, _) = upsert_collection(&p, &cin).await.unwrap();
        assert!(c.immutable());

        let (r, _, _) = upsert_record(&p, &rec_input(&c, "snap-1", "字节"))
            .await
            .unwrap();
        // 同内容重试：幂等重复，不报不可变
        let (_, _, dup) = upsert_record(&p, &rec_input(&c, "snap-1", "字节"))
            .await
            .unwrap();
        assert!(dup);
        // 换内容：不可变（不是静默覆盖）
        let err = upsert_record(&p, &rec_input(&c, "snap-1", "别的"))
            .await
            .unwrap_err();
        assert!(matches!(err, RecordError::Immutable));
        assert_eq!(
            get_record_by_id(&p, &r.id)
                .await
                .unwrap()
                .unwrap()
                .revision(),
            1
        );

        // 乐观并发：revision 对不上就是冲突
        let (c2, _) = upsert_collection(&p, &col_input("NS-1", "issue", "private"))
            .await
            .unwrap();
        upsert_record(&p, &rec_input(&c2, "a-1", "一"))
            .await
            .unwrap();
        let mut stale = rec_input(&c2, "a-1", "二");
        stale.expect_revision = 9;
        assert!(matches!(
            upsert_record(&p, &stale).await.unwrap_err(),
            RecordError::Conflict
        ));
        stale.expect_revision = 1;
        assert!(upsert_record(&p, &stale).await.is_ok());
    }

    #[tokio::test]
    async fn 索引行_只给声明的字段建() {
        let (p, _d) = pool("idx").await;
        let (c, _) = upsert_collection(&p, &col_input("NS-1", "issue", "private"))
            .await
            .unwrap();
        let (r, _, _) = upsert_record(&p, &rec_input(&c, "a-1", "正文"))
            .await
            .unwrap();
        let mut vals: HashMap<String, Vec<String>> = HashMap::new();
        vals.insert("status".to_string(), vec!["open".to_string()]);
        // 没在 index 里声明的字段，就算传了也不建行
        vals.insert("title".to_string(), vec!["标题".to_string()]);
        // 空值不建行
        vals.insert("tags".to_string(), vec!["  ".to_string()]);
        set_index(&p, &r.id, &c.id, &["status".to_string()], &vals)
            .await
            .unwrap();
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT field, value FROM record_index WHERE record_id = ?")
                .bind(&r.id)
                .fetch_all(&p)
                .await
                .unwrap();
        assert_eq!(rows, vec![("status".to_string(), "open".to_string())]);

        // 重建索引行：先删后插，不残留
        vals.insert("status".to_string(), vec!["closed".to_string()]);
        set_index(&p, &r.id, &c.id, &["status".to_string()], &vals)
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM record_index WHERE record_id = ?")
            .bind(&r.id)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn 列表_过滤_排序_可见性与过期() {
        let (p, _d) = pool("list").await;
        let (c, _) = upsert_collection(&p, &col_input("NS-1", "issue", "public"))
            .await
            .unwrap();

        // 三条：公开 / 私有 / 过期
        let mut a = rec_input(&c, "login-timeout", "登录 超时");
        a.visibility = "public".to_string();
        a.search_text = "open 登录".to_string();
        upsert_record(&p, &a).await.unwrap();

        let mut b = rec_input(&c, "other-1", "无关");
        b.visibility = "private".to_string();
        b.data = r#"{"status":"closed"}"#.to_string();
        let (b_rec, _, _) = upsert_record(&p, &b).await.unwrap();
        // 索引行只给声明过的字段建（data 里的 status 不会自己进索引表）
        set_index(
            &p,
            &b_rec.id,
            &c.id,
            &["status".to_string()],
            &HashMap::from([("status".to_string(), vec!["closed".to_string()])]),
        )
        .await
        .unwrap();

        let mut e = rec_input(&c, "gone-1", "过期的");
        e.visibility = "public".to_string();
        e.expires_at = Some("2020-01-01 00:00:00+08:00".to_string());
        upsert_record(&p, &e).await.unwrap();

        // 不同命名空间的记录不该混进来
        let (c2, _) = upsert_collection(&p, &col_input("NS-2", "issue", "public"))
            .await
            .unwrap();
        upsert_record(&p, &rec_input(&c2, "a-1", "别人的"))
            .await
            .unwrap();

        let now = now_go();
        // 内部视角（NS-1 可读）：归档与过期都不列；别的命名空间的记录不进来
        let mut o = RecordListOpts::default();
        o.collection_id = c.id.clone();
        o.now = now.clone();
        o.namespace_ids = vec!["NS-1".to_string()];
        let (rows, total) = list_records(&p, &o).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.collection_id == c.id));

        // 匿名（无范围）：只有公开且未过的
        let mut o = RecordListOpts::default();
        o.collection_id = c.id.clone();
        o.now = now.clone();
        o.public = true;
        let (rows, total) = list_records(&p, &o).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].key, "login-timeout");

        // 指定标签 / 前缀（有可读范围：不然就退化成「只看公开」）
        let mut o = RecordListOpts::default();
        o.collection_id = c.id.clone();
        o.now = now.clone();
        o.namespace_ids = vec!["NS-1".to_string()];
        o.tags = vec!["bug".to_string()];
        o.prefix = "other".to_string();
        let (rows, _) = list_records(&p, &o).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, "other-1");

        // 字段过滤走索引表（没建索引行的字段过滤不到 —— 红线写进了数据模型）
        let mut o = RecordListOpts::default();
        o.collection_id = c.id.clone();
        o.now = now.clone();
        o.namespace_ids = vec!["NS-1".to_string()];
        o.indexed.insert("status".to_string(), "closed".to_string());
        let (rows, total) = list_records(&p, &o).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].key, "other-1");

        // 关键词排序：key 命中 排在只有正文命中的前面
        let mut o = RecordListOpts::default();
        o.collection_id = c.id.clone();
        o.now = now.clone();
        o.namespace_ids = vec!["NS-1".to_string()];
        o.q = "登录".to_string();
        let (rows, total) = list_records(&p, &o).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].key, "login-timeout");

        // archived=1 / expired=1 才看得到
        let mut o = RecordListOpts::default();
        o.collection_id = c.id.clone();
        o.now = now.clone();
        o.namespace_ids = vec!["NS-1".to_string()];
        o.include_expired = true;
        o.include_archived = true;
        o.tags = vec!["bug".to_string()];
        let (rows, _) = list_records(&p, &o).await.unwrap();
        assert_eq!(rows.len(), 3);

        // 分页：size 超上限与页码低于 1 都被纠正
        let mut o = RecordListOpts::default();
        o.collection_id = c.id.clone();
        o.now = now.clone();
        o.namespace_ids = vec!["NS-1".to_string()];
        o.page = 0;
        o.size = 999;
        let (rows, _) = list_records(&p, &o).await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn 归档_硬删_与_gc() {
        let (p, _d) = pool("gc").await;
        let (c, _) = upsert_collection(&p, &col_input("NS-1", "issue", "private"))
            .await
            .unwrap();
        let (r1, _, _) = upsert_record(&p, &rec_input(&c, "a-1", "一"))
            .await
            .unwrap();
        let (r2, _, _) = upsert_record(&p, &rec_input(&c, "a-2", "二"))
            .await
            .unwrap();
        set_index(
            &p,
            &r2.id,
            &c.id,
            &["status".to_string()],
            &HashMap::from([("status".to_string(), vec!["open".to_string()])]),
        )
        .await
        .unwrap();

        // 软删（归档）：记录还在，但不算进计数
        set_record_status(&p, &r1.id, STATUS_ARCHIVED, "U-1")
            .await
            .unwrap();
        assert_eq!(count_records(&p, &c.id).await.unwrap(), 1);

        // 硬删：历史与索引行一并清掉
        let mut inp = rec_input(&c, "a-2", "二改");
        inp.note = "改".to_string();
        upsert_record(&p, &inp).await.unwrap();
        delete_record(&p, &r2.id).await.unwrap();
        assert!(get_record_by_id(&p, &r2.id).await.unwrap().is_none());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM record_index WHERE record_id = ?")
            .bind(&r2.id)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(n, 0);
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM record_revisions WHERE record_id = ?")
                .bind(&r2.id)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(n, 0);

        // gc：只清过期的那条
        let mut e = rec_input(&c, "old-1", "过期");
        e.expires_at = Some("2020-01-01 00:00:00+08:00".to_string());
        upsert_record(&p, &e).await.unwrap();
        let removed = gc_records(&p, &c.id, &now_go()).await.unwrap();
        assert_eq!(removed, 1);
        assert!(get_record_by_key(&p, &c.id, "old-1")
            .await
            .unwrap()
            .is_none());
        assert_eq!(gc_records(&p, "", &now_go()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 集合列表_范围与归档() {
        let (p, _d) = pool("collist").await;
        upsert_collection(&p, &col_input("NS-1", "issue", "private"))
            .await
            .unwrap();
        upsert_collection(&p, &col_input("NS-2", "log", "public"))
            .await
            .unwrap();
        let (archived, _) = upsert_collection(&p, &col_input("NS-2", "trace", "public"))
            .await
            .unwrap();
        set_collection_status(&p, &archived.id, STATUS_ARCHIVED)
            .await
            .unwrap();

        // 匿名：只有公开的
        let all = list_collections(
            &p,
            &CollectionListOpts {
                namespace_id: None,
                namespace_ids: vec![],
                all: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].kind, "log");

        // 有范围：自己的 + 公开的；归档的不列
        let mine = list_collections(
            &p,
            &CollectionListOpts {
                namespace_id: None,
                namespace_ids: vec!["NS-1".to_string()],
                all: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].kind, "issue");
        assert_eq!(mine[1].kind, "log");

        // 指定命名空间
        let one = list_collections(
            &p,
            &CollectionListOpts {
                namespace_id: Some("NS-1".to_string()),
                namespace_ids: vec![],
                all: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(one.len(), 1);

        // 管理员也一样看不到归档的集合
        let adm = list_collections(
            &p,
            &CollectionListOpts {
                namespace_id: None,
                namespace_ids: vec![],
                all: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(adm.len(), 2);

        assert!(namespace_ids_of_owners(&p, &[]).await.unwrap().is_empty());
        assert_eq!(
            namespace_ids_of_owners(&p, &["U-9".to_string()])
                .await
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn 分词_与_访问器兜底() {
        assert_eq!(search_tokens(" 登录, 超时 登录 "), vec!["登录", "超时"]);
        assert!(search_tokens("").is_empty());
        // 可空列全空也读得出来（脏数据不该炸列表）
        let c = Collection {
            id: "C-1".into(),
            namespace_id: "NS-1".into(),
            kind: "issue".into(),
            title: None,
            summary: None,
            reason: None,
            mutable: None,
            history: None,
            append_only: None,
            visibility: None,
            max_bytes: None,
            fields: None,
            index: None,
            dedupe_by: None,
            default_ttl: None,
            status: None,
            created_by: None,
            created_at: None,
            updated_at: None,
        };
        assert_eq!(c.title(), "");
        assert_eq!(c.visibility(), "private");
        assert_eq!(c.status(), STATUS_ACTIVE);
        assert_eq!(c.max_bytes(), MAX_BYTES_DEFAULT);
        assert!(c.immutable(), "mutable 缺省为 0 = 不可变");
    }
}
