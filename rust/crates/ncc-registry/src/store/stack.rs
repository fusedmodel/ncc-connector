//! NCC State：知识库（kb）/ 记忆（mem）/ 检查点（ckpt）的数据访问。
//!
//! 原实现：`ncc-registry/store/state.go`。三样状态**住在节点上**，包只声明「要什么」，
//! 所以这一层同时是隔离的落点：可见范围（`StateScope`）是 **fail-closed** 的 ——
//! 什么都没给就查不到东西（拼 `1 = 0`），绝不把「没给条件」当成「看全部」。
//!
//! 三者的形状本来就不同，所以是三条独立路径而不是一张表：
//!
//! ```text
//! kb    内容进库 + 每次写入追加一版历史（revision 只增不改）
//! mem   (命名空间, subject, key) 唯一，写即更新；过期**读时**判定（不等清理任务）
//! ckpt  字节进 blob、元数据进库、不可变、有血缘（parent 链）
//! ```
//!
//! 刻意的取舍：
//!
//! * `expires_at` 是 TEXT 列，比较也按 TEXT 做：写入与比较都用 `now_go()`（本地偏移、
//!   GORM 同款格式），保证「自己写进去的行」比较结果正确。Go 侧绑定的是 `now.UTC()`，
//!   与库里 `+08:00` 形态的字符串逐字节比较会整段失效 —— 这个坑不复刻。
//! * 可见范围的拼法与配置那一族同形（`IN` + `OWNER` + `PUBLIC` 的 OR），
//!   但条件为空时**只回兜底常量**：配置带公开档，状态默认没有。
//! * 「取用即声明」只覆盖本族的三个 kind（kb/mem/ckpt）：通用记录仓（`store/state.rs`）
//!   迁完后应与此处合并成一份，现在先就地实现，避免跨文件改动。

use chrono::{Duration, Local};
use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::{format_go, now_go};

use super::{exists, marshal_list};

/* ============================ 可见范围 ============================ */

/// 三样状态共用的可见范围。
///
/// `all` 只有管理员显式要（`all=1`）时才为真；否则一律是
/// 「我的命名空间 ∪ 把 state 授权给我的人所在的命名空间（∪ 公开档）」。
#[derive(Debug, Clone, Default)]
pub struct StateScope {
    pub namespace_ids: Vec<String>,
    pub granted_owners: Vec<String>,
    pub all: bool,
}

/// 拼可见范围条件：`public_col` 非空时把 `visibility = public` 并进 OR。
///
/// 公开项**必须出现在列表里**，否则会出现「按引用取得到、列表里看不到」的怪现象。
/// mem 传空串（记忆没有公开档，这一档它不该有）。
fn scope_conditions(
    sc: &StateScope,
    ns_col: &str,
    owner_col: &str,
    public_col: &str,
) -> (String, Vec<String>) {
    if sc.all {
        return (String::new(), Vec::new());
    }
    let mut conds: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if !sc.namespace_ids.is_empty() {
        conds.push(format!(
            "{ns_col} IN ({})",
            placeholders(sc.namespace_ids.len())
        ));
        binds.extend(sc.namespace_ids.iter().cloned());
    }
    if !sc.granted_owners.is_empty() {
        conds.push(format!(
            "{owner_col} IN ({})",
            placeholders(sc.granted_owners.len())
        ));
        binds.extend(sc.granted_owners.iter().cloned());
    }
    if !public_col.is_empty() {
        conds.push(format!("{public_col} = 'public'"));
    }
    if conds.is_empty() {
        return ("1 = 0".to_string(), binds);
    }
    (format!("({})", conds.join(" OR ")), binds)
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// 逐条累加 where 片段，顺手记住绑定值的顺序（顺序错了 SQL 就静默查错）。
#[derive(Default)]
struct Where {
    sql: String,
    binds: Vec<String>,
}

impl Where {
    fn new() -> Self {
        Self {
            sql: String::from(" WHERE 1=1"),
            binds: Vec::new(),
        }
    }

    fn raw(&mut self, cond: &str, binds: Vec<String>) {
        self.sql.push_str(" AND ");
        self.sql.push_str(cond);
        self.binds.extend(binds);
    }

    /// 可见范围（放在最前，与绑定顺序一致）。
    fn scope(&mut self, sc: &StateScope, ns_col: &str, owner_col: &str, public_col: &str) {
        let (cond, binds) = scope_conditions(sc, ns_col, owner_col, public_col);
        if !cond.is_empty() {
            self.raw(&cond, binds);
        }
    }

    /// 等值过滤：值**原样**参与（是否 trim 由调用方决定，与 Go 的分工一致）。
    fn eq(&mut self, col: &str, val: &str) {
        if !val.is_empty() {
            self.raw(&format!("{col} = ?"), vec![val.to_string()]);
        }
    }

    fn like(&mut self, col: &str, pattern: String) {
        self.raw(&format!("{col} LIKE ?"), vec![pattern]);
    }
}

/// 把 where 片段里的绑定值灌进查询（顺序即占位符顺序）。
///
/// 分页用的 `LIMIT`/`OFFSET` **不在这里**：那些是整数，混进同一个 `Vec<String>`
/// 就得给绑定值做类型枚举，而 SQLite 对 LIMIT 的文本→整数转换不值得依赖。
macro_rules! bind_where {
    ($q:expr, $w:expr) => {{
        let mut q = $q;
        for b in &$w.binds {
            q = q.bind(b);
        }
        q
    }};
}

/* ============================ 知识库 kb ============================ */

/// 文档 + 命名空间 / 归属者摘要（一次 join 出，避免 N+1）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct KbRow {
    pub id: String,
    pub namespace_id: String,
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub format: String,
    pub summary: String,
    pub tags: String,
    pub visibility: String,
    pub status: String,
    pub revision: i64,
    pub content: String,
    pub checksum: String,
    pub size: i64,
    pub source: String,
    pub search_text: String,
    pub created_by: String,
    pub updated_by: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub ns_slug: Option<String>,
    pub ns_name: Option<String>,
    pub owner_id: Option<String>,
    pub owner_name: Option<String>,
}

impl KbRow {
    /// 规范引用 `@命名空间/slug`。
    pub fn ref_of(&self) -> String {
        format!(
            "@{}/{}",
            self.ns_slug.clone().unwrap_or_default(),
            self.slug
        )
    }

    pub fn is_public(&self) -> bool {
        self.visibility == "public"
    }
}

/// 一次内容快照。**只增不改**：每次写入追加一行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct KbRevision {
    pub id: String,
    pub doc_id: String,
    pub revision: i64,
    pub title: String,
    pub content: String,
    pub checksum: String,
    pub size: i64,
    pub note: String,
    pub author_id: String,
    pub author: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct KbInput {
    pub namespace_id: String,
    pub slug: String,
    pub title: String,
    pub kind: String,
    pub format: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub visibility: String,
    pub source: String,
    pub content: String,
    pub checksum: String,
    pub note: String,
    pub author_id: String,
    pub author_name: String,
}

#[derive(Debug, Clone, Default)]
pub struct KbListOpts {
    pub scope: StateScope,
    pub ns_slug: String,
    pub kind: String,
    pub tag: String,
    pub status: String,
    pub q: String,
    pub include_arch: bool,
    pub page: i64,
    pub size: i64,
    /// 显式条数上限（bundle 用 500）；为 0 时走 `size` 的分页规则。
    pub limit: i64,
    pub include_public: bool,
    /// 按命中打分排序（搜索用）；否则按更新时间倒序（列表用）。
    pub rank: bool,
}

const KB_COLS: &str = "k.*, ns.slug AS ns_slug, ns.name AS ns_name, \
     ns.owner_id AS owner_id, u.name AS owner_name";

fn kb_where(opts: &KbListOpts) -> Where {
    let mut w = Where::new();
    let public_col = if opts.include_public {
        "k.visibility"
    } else {
        ""
    };
    w.scope(&opts.scope, "k.namespace_id", "ns.owner_id", public_col);
    w.eq("ns.slug", &opts.ns_slug);
    w.eq("k.kind", &opts.kind);
    if !opts.tag.is_empty() {
        // tags 存 JSON 数组文本，带引号匹配完整值，免得 "bar" 命中 "foobar"。
        w.like("k.tags", format!("%\"{}\"%", opts.tag));
    }
    if !opts.status.is_empty() {
        w.eq("k.status", &opts.status);
    } else if !opts.include_arch {
        w.raw("k.status = ?", vec!["active".to_string()]);
    }
    if !opts.q.is_empty() {
        let like = format!("%{}%", opts.q.to_lowercase());
        w.raw(
            "(k.title LIKE ? OR k.summary LIKE ? OR k.search_text LIKE ?)",
            vec![like.clone(), like.clone(), like],
        );
    }
    w
}

/// 建/改一篇文档：改就是新版本（追加 `kb_revisions`）。
pub async fn upsert_kb_doc(pool: &SqlitePool, in_: &KbInput) -> Result<(KbRow, bool), sqlx::Error> {
    let tags = tags_json(&in_.tags);
    let size = in_.content.len() as i64;
    // 自己存一份小写正文：SQLite 的 LIKE 大小写不敏感但细节多，不如显式存下来。
    let search = format!("{}\n{}\n{}", in_.title, in_.summary, in_.content).to_lowercase();

    let existing: Option<(String, i64)> =
        sqlx::query_as("SELECT id, revision FROM kb_docs WHERE namespace_id = ? AND slug = ?")
            .bind(&in_.namespace_id)
            .bind(&in_.slug)
            .fetch_optional(pool)
            .await?;

    let now = now_go();
    match existing {
        None => {
            let doc_id = new_id("KD");
            let mut tx = pool.begin().await?;
            sqlx::query(
                "INSERT INTO kb_docs (id, namespace_id, slug, title, kind, format, summary, tags, \
                 visibility, status, revision, content, checksum, size, source, search_text, \
                 created_by, updated_by, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', 1, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&doc_id)
            .bind(&in_.namespace_id)
            .bind(&in_.slug)
            .bind(&in_.title)
            .bind(&in_.kind)
            .bind(&in_.format)
            .bind(&in_.summary)
            .bind(&tags)
            .bind(&in_.visibility)
            .bind(&in_.content)
            .bind(&in_.checksum)
            .bind(size)
            .bind(&in_.source)
            .bind(&search)
            .bind(&in_.author_id)
            .bind(&in_.author_id)
            .bind(&now)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO kb_revisions (id, doc_id, revision, title, content, checksum, size, note, \
                 author_id, author, created_at) VALUES (?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(new_id("KR"))
            .bind(&doc_id)
            .bind(&in_.title)
            .bind(&in_.content)
            .bind(&in_.checksum)
            .bind(size)
            .bind(first_note(&in_.note))
            .bind(&in_.author_id)
            .bind(&in_.author_name)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            let row = by_required(pool, &doc_id).await?;
            Ok((row, true))
        }
        Some((doc_id, revision)) => {
            let next = revision + 1;
            let mut tx = pool.begin().await?;
            // 写入即「恢复 active」：改一篇归档文档不会让它悄悄留在归档里。
            let mut sql = String::from(
                "UPDATE kb_docs SET title = ?, kind = ?, format = ?, summary = ?, tags = ?, \
                 visibility = ?, content = ?, checksum = ?, size = ?, search_text = ?, revision = ?, \
                 status = 'active', updated_by = ?, updated_at = ?",
            );
            if !in_.source.is_empty() {
                sql.push_str(", source = ?");
            }
            sql.push_str(" WHERE id = ?");
            let mut q = sqlx::query(&sql)
                .bind(&in_.title)
                .bind(&in_.kind)
                .bind(&in_.format)
                .bind(&in_.summary)
                .bind(&tags)
                .bind(&in_.visibility)
                .bind(&in_.content)
                .bind(&in_.checksum)
                .bind(size)
                .bind(&search)
                .bind(next)
                .bind(&in_.author_id)
                .bind(&now);
            if !in_.source.is_empty() {
                q = q.bind(&in_.source);
            }
            q.bind(&doc_id).execute(&mut *tx).await?;
            sqlx::query(
                "INSERT INTO kb_revisions (id, doc_id, revision, title, content, checksum, size, note, \
                 author_id, author, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(new_id("KR"))
            .bind(&doc_id)
            .bind(next)
            .bind(&in_.title)
            .bind(&in_.content)
            .bind(&in_.checksum)
            .bind(size)
            .bind(&in_.note)
            .bind(&in_.author_id)
            .bind(&in_.author_name)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            let row = by_required(pool, &doc_id).await?;
            Ok((row, false))
        }
    }
}

/// 首版的变更说明：空说明补「初版」，让历史里每一行都能读懂。
fn first_note(note: &str) -> String {
    if note.trim().is_empty() {
        "初版".to_string()
    } else {
        note.to_string()
    }
}

fn tags_json(tags: &[String]) -> String {
    if tags.is_empty() {
        "[]".to_string()
    } else {
        marshal_list(tags)
    }
}

async fn by_required(pool: &SqlitePool, id: &str) -> Result<KbRow, sqlx::Error> {
    get_kb_doc(pool, id).await?.ok_or(sqlx::Error::RowNotFound)
}

/// 检索（分页 + 总数）。`rank` 时按命中打分重排**当前页**（与 Go 一致）。
pub async fn list_kb_docs(
    pool: &SqlitePool,
    opts: &KbListOpts,
) -> Result<(Vec<KbRow>, i64), sqlx::Error> {
    let w = kb_where(opts);

    let count_sql = format!(
        "SELECT COUNT(*) FROM kb_docs k JOIN namespaces ns ON ns.id = k.namespace_id{}",
        w.sql
    );
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &w.binds {
        cq = cq.bind(b);
    }
    let total = cq.fetch_one(pool).await?;

    let mut sql = format!(
        "SELECT {KB_COLS} FROM kb_docs k JOIN namespaces ns ON ns.id = k.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id{} ORDER BY k.updated_at DESC, k.id DESC",
        w.sql
    );
    // 分页优先于 limit（bundle 走 limit，列表走分页）—— 与 Go 的分支顺序一致。
    let page = if opts.page < 1 { 1 } else { opts.page };
    let mut rows = if opts.size > 0 {
        sql.push_str(" LIMIT ? OFFSET ?");
        bind_where!(sqlx::query_as::<_, KbRow>(&sql), w)
            .bind(opts.size)
            .bind((page - 1) * opts.size)
            .fetch_all(pool)
            .await?
    } else if opts.limit > 0 {
        sql.push_str(" LIMIT ?");
        bind_where!(sqlx::query_as::<_, KbRow>(&sql), w)
            .bind(opts.limit)
            .fetch_all(pool)
            .await?
    } else {
        bind_where!(sqlx::query_as::<_, KbRow>(&sql), w)
            .fetch_all(pool)
            .await?
    };
    if opts.rank && !opts.q.is_empty() {
        rank_kb_rows(&mut rows, &opts.q);
    }
    Ok((rows, total))
}

/// 关键词打分：标题 3 分 / 摘要 2 分 / 正文 1 分，正文里的**命中次数**也计入。
///
/// 这是**关键词检索**，不是向量检索 —— 数据量小的时候够用，前提是说清楚。
fn rank_kb_rows(rows: &mut [KbRow], q: &str) {
    let terms: Vec<String> = q
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    let score = |r: &KbRow| -> i64 {
        let (t, sm, c) = (
            r.title.to_lowercase(),
            r.summary.to_lowercase(),
            r.content.to_lowercase(),
        );
        let mut n = 0i64;
        for term in &terms {
            n += 3 * count_occurrences(&t, term);
            n += 2 * count_occurrences(&sm, term);
            n += count_occurrences(&c, term);
        }
        n
    };
    // 简单插入排序：结果集有分页上限，不值得引 sort。
    for i in 1..rows.len() {
        let mut j = i;
        while j > 0 && score(&rows[j]) > score(&rows[j - 1]) {
            rows.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn count_occurrences(hay: &str, needle: &str) -> i64 {
    if needle.is_empty() {
        return 0;
    }
    hay.match_indices(needle).count() as i64
}

/// 按行 id 或 `@ns/slug` / `ns/slug` 取一篇。
pub async fn get_kb_doc(pool: &SqlitePool, id_or_ref: &str) -> Result<Option<KbRow>, sqlx::Error> {
    let base = format!(
        "SELECT {KB_COLS} FROM kb_docs k JOIN namespaces ns ON ns.id = k.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id"
    );
    if id_or_ref.starts_with('@') || id_or_ref.contains('/') {
        let (ns, slug) = split_ref(id_or_ref);
        let sql = format!("{base} WHERE ns.slug = ? AND k.slug = ? LIMIT 1");
        return sqlx::query_as::<_, KbRow>(&sql)
            .bind(ns)
            .bind(slug)
            .fetch_optional(pool)
            .await;
    }
    let sql = format!("{base} WHERE (k.id = ? OR k.slug = ?) LIMIT 1");
    sqlx::query_as::<_, KbRow>(&sql)
        .bind(id_or_ref)
        .bind(id_or_ref)
        .fetch_optional(pool)
        .await
}

/// 把 `@ns/slug` 或 `ns/slug` 拆开。
pub fn split_ref(ref_: &str) -> (String, String) {
    let r = ref_.trim().trim_start_matches('@');
    match r.split_once('/') {
        Some((ns, slug)) => (ns.to_string(), slug.to_string()),
        None => (r.to_string(), String::new()),
    }
}

/// 版本历史（正序：老版本在前）。
pub async fn kb_revisions(pool: &SqlitePool, doc_id: &str) -> Result<Vec<KbRevision>, sqlx::Error> {
    sqlx::query_as::<_, KbRevision>(
        "SELECT id, doc_id, revision, title, content, checksum, size, note, author_id, author, created_at \
         FROM kb_revisions WHERE doc_id = ? ORDER BY revision ASC",
    )
    .bind(doc_id)
    .fetch_all(pool)
    .await
}

/// 只改元数据的补丁（改名 / 归档 / 恢复 / 切可见性）。
///
/// 与 Go 一致：**不重算 `search_text`，也不加版本** —— 版本只跟「内容变了」绑定，
/// 否则历史里会混进一堆没改过内容的噪音。
#[derive(Debug, Clone, Default)]
pub struct KbPatch {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub tags: Option<Vec<String>>,
    pub visibility: Option<String>,
    pub status: Option<String>,
    pub updated_by: String,
}

pub async fn patch_kb_doc(pool: &SqlitePool, id: &str, p: &KbPatch) -> Result<(), sqlx::Error> {
    let mut sql = String::from("UPDATE kb_docs SET updated_by = ?, updated_at = ?");
    let mut binds: Vec<String> = vec![p.updated_by.clone(), now_go()];
    if let Some(v) = &p.title {
        sql.push_str(", title = ?");
        binds.push(v.clone());
    }
    if let Some(v) = &p.summary {
        sql.push_str(", summary = ?");
        binds.push(v.clone());
    }
    if let Some(v) = &p.tags {
        sql.push_str(", tags = ?");
        binds.push(tags_json(v));
    }
    if let Some(v) = &p.visibility {
        sql.push_str(", visibility = ?");
        binds.push(v.clone());
    }
    if let Some(v) = &p.status {
        sql.push_str(", status = ?");
        binds.push(v.clone());
    }
    sql.push_str(" WHERE id = ?");
    binds.push(id.to_string());

    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    q.execute(pool).await?;
    Ok(())
}

/// 删除（连同历史）。KB 是数据不是审计，归属者有权删掉。
pub async fn delete_kb_doc(pool: &SqlitePool, doc_id: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM kb_revisions WHERE doc_id = ?")
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM kb_docs WHERE id = ?")
        .bind(doc_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// 计数（`/api/meta` 与集群上报用）。
pub async fn count_kb_docs(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM kb_docs WHERE status = 'active'")
        .fetch_one(pool)
        .await
}

/* ============================ 记忆 mem ============================ */

/// 记忆 + 命名空间摘要。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MemRow {
    pub id: String,
    pub namespace_id: String,
    pub subject: String,
    pub key: String,
    pub value: String,
    pub kind: String,
    pub tags: String,
    pub source: String,
    pub confidence: i64,
    pub pinned: bool,
    pub revision: i64,
    pub expires_at: Option<String>,
    pub created_by: String,
    pub updated_by: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub ns_slug: Option<String>,
    pub ns_name: Option<String>,
    pub owner_id: Option<String>,
}

impl MemRow {
    /// 在给定时刻是否已过期。时间为空 = 永不过期。
    pub fn expired_at(&self, now: chrono::DateTime<chrono::FixedOffset>) -> bool {
        match self
            .expires_at
            .as_deref()
            .and_then(ncc_core::timeutil::parse_time)
        {
            Some(t) => t <= now,
            None => false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct MemInput {
    pub namespace_id: String,
    pub subject: String,
    pub key: String,
    pub value: String,
    pub kind: String,
    pub tags: Vec<String>,
    pub source: String,
    pub confidence: i64,
    pub pinned: bool,
    pub ttl_days: i64,
    pub author_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct MemListOpts {
    pub scope: StateScope,
    pub subject: String,
    pub prefix: String,
    pub kind: String,
    pub tag: String,
    pub source: String,
    pub include_expired: bool,
    pub pinned_only: bool,
    pub limit: i64,
    /// 判过期用的「现在」（为空 = 服务端当前时间）。
    pub now: Option<String>,
}

const MEM_COLS: &str = "m.*, ns.slug AS ns_slug, ns.name AS ns_name, ns.owner_id AS owner_id";

fn mem_where(opts: &MemListOpts) -> Where {
    let mut w = Where::new();
    // 记忆**没有公开档**：这里传空串，公开那一档它不该有。
    w.scope(&opts.scope, "m.namespace_id", "ns.owner_id", "");
    w.eq("m.subject", &opts.subject);
    if !opts.prefix.is_empty() {
        w.like("m.key", format!("{}%", opts.prefix));
    }
    w.eq("m.kind", &opts.kind);
    if !opts.tag.is_empty() {
        w.like("m.tags", format!("%\"{}\"%", opts.tag));
    }
    w.eq("m.source", &opts.source);
    if opts.pinned_only {
        w.raw("m.pinned = ?", vec!["1".to_string()]);
    }
    if !opts.include_expired {
        // 读时判过期：`expires_at IS NULL` = 永不过期，否则必须还没到点。
        let now = opts.now.clone().unwrap_or_else(now_go);
        w.raw("(m.expires_at IS NULL OR m.expires_at > ?)", vec![now]);
    }
    w
}

/// 写一条记忆（同 `(命名空间, subject, key)` 即更新，`revision+1`）。
pub async fn upsert_mem_entry(
    pool: &SqlitePool,
    in_: &MemInput,
) -> Result<(MemRow, bool), sqlx::Error> {
    let tags = tags_json(&in_.tags);
    let exp = if in_.ttl_days > 0 {
        Some(expires_after_days(in_.ttl_days))
    } else {
        None
    };
    let existing: Option<(String, i64, bool)> = sqlx::query_as(
        "SELECT id, revision, pinned FROM mem_entries WHERE namespace_id = ? AND subject = ? AND key = ?",
    )
    .bind(&in_.namespace_id)
    .bind(&in_.subject)
    .bind(&in_.key)
    .fetch_optional(pool)
    .await?;

    let now = now_go();
    match existing {
        None => {
            let id = new_id("ME");
            sqlx::query(
                "INSERT INTO mem_entries (id, namespace_id, subject, key, value, kind, tags, source, \
                 confidence, pinned, revision, expires_at, created_by, updated_by, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&in_.namespace_id)
            .bind(&in_.subject)
            .bind(&in_.key)
            .bind(&in_.value)
            .bind(&in_.kind)
            .bind(&tags)
            .bind(&in_.source)
            .bind(in_.confidence)
            .bind(in_.pinned)
            .bind(&exp)
            .bind(&in_.author_id)
            .bind(&in_.author_id)
            .bind(&now)
            .bind(&now)
            .execute(pool)
            .await?;
            let row = by_required_mem(pool, &id).await?;
            Ok((row, true))
        }
        Some((id, revision, pinned)) => {
            let mut sql = String::from(
                "UPDATE mem_entries SET value = ?, kind = ?, tags = ?, source = ?, confidence = ?, \
                 revision = ?, expires_at = ?, updated_by = ?, updated_at = ?",
            );
            // Pinned 只能被显式设真或保持原样（不因为一次普通写入被意外取消）。
            let set_pin = in_.pinned || !pinned;
            if set_pin {
                sql.push_str(", pinned = ?");
            }
            sql.push_str(" WHERE id = ?");
            let mut q = sqlx::query(&sql)
                .bind(&in_.value)
                .bind(&in_.kind)
                .bind(&tags)
                .bind(&in_.source)
                .bind(in_.confidence)
                .bind(revision + 1)
                .bind(&exp)
                .bind(&in_.author_id)
                .bind(&now);
            if set_pin {
                q = q.bind(in_.pinned);
            }
            q.bind(&id).execute(pool).await?;
            let row = by_required_mem(pool, &id).await?;
            Ok((row, false))
        }
    }
}

/// TTL：写入时从**现在**重新计时；`ttl_days = 0` 显式清空过期时间。
fn expires_after_days(days: i64) -> String {
    format_go(Local::now().fixed_offset() + Duration::days(days))
}

async fn by_required_mem(pool: &SqlitePool, id: &str) -> Result<MemRow, sqlx::Error> {
    get_mem_entry(pool, id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

/// 列记忆（置顶在前，再按更新时间倒序）。
pub async fn list_mem_entries(
    pool: &SqlitePool,
    opts: &MemListOpts,
) -> Result<(Vec<MemRow>, i64), sqlx::Error> {
    let w = mem_where(opts);

    let count_sql = format!(
        "SELECT COUNT(*) FROM mem_entries m JOIN namespaces ns ON ns.id = m.namespace_id{}",
        w.sql
    );
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &w.binds {
        cq = cq.bind(b);
    }
    let total = cq.fetch_one(pool).await?;

    let mut sql = format!(
        "SELECT {MEM_COLS} FROM mem_entries m JOIN namespaces ns ON ns.id = m.namespace_id{} \
         ORDER BY m.pinned DESC, m.updated_at DESC, m.id DESC",
        w.sql
    );
    let rows = if opts.limit > 0 {
        sql.push_str(" LIMIT ?");
        bind_where!(sqlx::query_as::<_, MemRow>(&sql), w)
            .bind(opts.limit)
            .fetch_all(pool)
            .await?
    } else {
        bind_where!(sqlx::query_as::<_, MemRow>(&sql), w)
            .fetch_all(pool)
            .await?
    };
    Ok((rows, total))
}

/// 按 id 或 key 取一条（**不判过期**：调用方要的是「这一行在不在」）。
pub async fn get_mem_entry(
    pool: &SqlitePool,
    id_or_key: &str,
) -> Result<Option<MemRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {MEM_COLS} FROM mem_entries m JOIN namespaces ns ON ns.id = m.namespace_id \
         WHERE (m.id = ? OR m.key = ?) LIMIT 1"
    );
    sqlx::query_as::<_, MemRow>(&sql)
        .bind(id_or_key)
        .bind(id_or_key)
        .fetch_optional(pool)
        .await
}

/// 按 `(命名空间, subject, key)` 精确取 —— Agent 读记忆的主路径（过期即视为不存在）。
pub async fn get_mem_by_key(
    pool: &SqlitePool,
    ns_id: &str,
    subject: &str,
    key: &str,
    now: Option<&str>,
) -> Result<Option<MemRow>, sqlx::Error> {
    let mut sql = format!(
        "SELECT {MEM_COLS} FROM mem_entries m JOIN namespaces ns ON ns.id = m.namespace_id \
         WHERE m.namespace_id = ? AND m.subject = ? AND m.key = ?"
    );
    if now.is_some() {
        sql.push_str(" AND (m.expires_at IS NULL OR m.expires_at > ?)");
    }
    sql.push_str(" LIMIT 1");
    let mut q = sqlx::query_as::<_, MemRow>(&sql)
        .bind(ns_id)
        .bind(subject)
        .bind(key);
    if let Some(n) = now {
        q = q.bind(n);
    }
    q.fetch_optional(pool).await
}

pub async fn delete_mem_entry(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM mem_entries WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 真正删掉过期条目（读时已经判过过期，这里只是清垃圾）。
pub async fn gc_mem_entries(
    pool: &SqlitePool,
    ns_id: &str,
    now: Option<&str>,
) -> Result<i64, sqlx::Error> {
    let now = now.map(|s| s.to_string()).unwrap_or_else(now_go);
    let res = if ns_id.is_empty() {
        sqlx::query("DELETE FROM mem_entries WHERE expires_at IS NOT NULL AND expires_at <= ?")
            .bind(&now)
            .execute(pool)
            .await?
    } else {
        sqlx::query(
            "DELETE FROM mem_entries WHERE expires_at IS NOT NULL AND expires_at <= ? AND namespace_id = ?",
        )
        .bind(&now)
        .bind(ns_id)
        .execute(pool)
        .await?
    };
    Ok(res.rows_affected() as i64)
}

/// 计数（**含过期**：它问的是「库里有多少」）。
pub async fn count_mem_entries(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM mem_entries")
        .fetch_one(pool)
        .await
}

/* ============================ 检查点 ckpt ============================ */

/// 检查点 + 命名空间摘要。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CkptRow {
    pub id: String,
    pub namespace_id: String,
    pub subject_ref: String,
    pub subject_version: String,
    pub name: String,
    pub label: String,
    pub step: i64,
    pub summary: String,
    pub tags: String,
    pub visibility: String,
    pub status: String,
    pub parent: String,
    pub object_key: String,
    pub digest: String,
    pub size: i64,
    pub media_type: String,
    pub meta: String,
    pub created_by: String,
    pub created_at: Option<String>,
    pub ns_slug: Option<String>,
    pub ns_name: Option<String>,
    pub owner_id: Option<String>,
}

impl CkptRow {
    pub fn is_public(&self) -> bool {
        self.visibility == "public"
    }
}

#[derive(Debug, Clone, Default)]
pub struct CkptInput {
    pub namespace_id: String,
    pub subject_ref: String,
    pub subject_version: String,
    pub name: String,
    pub label: String,
    pub step: i64,
    pub summary: String,
    pub tags: Vec<String>,
    pub visibility: String,
    pub parent: String,
    pub digest: String,
    pub size: i64,
    pub media_type: String,
    pub meta: String,
    pub author_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct CkptListOpts {
    pub scope: StateScope,
    pub subject_ref: String,
    pub label: String,
    pub tag: String,
    pub status: String,
    pub name: String,
    pub limit: i64,
    pub include_public: bool,
}

const CKPT_COLS: &str = "c.*, ns.slug AS ns_slug, ns.name AS ns_name, ns.owner_id AS owner_id";

fn ckpt_where(opts: &CkptListOpts) -> Where {
    let mut w = Where::new();
    let public_col = if opts.include_public {
        "c.visibility"
    } else {
        ""
    };
    w.scope(&opts.scope, "c.namespace_id", "ns.owner_id", public_col);
    w.eq("c.subject_ref", &opts.subject_ref);
    w.eq("c.label", &opts.label);
    if !opts.tag.is_empty() {
        w.like("c.tags", format!("%\"{}\"%", opts.tag));
    }
    if !opts.name.is_empty() {
        w.like("c.name", format!("%{}%", opts.name));
    }
    // 没显式指定状态时只列 active：pruned 的元数据留下是为了「查得到它曾经存在」，
    // 不是为了让它们继续出现在默认列表里。
    if !opts.status.is_empty() {
        w.eq("c.status", &opts.status);
    } else {
        w.raw("c.status = ?", vec!["active".to_string()]);
    }
    w
}

/// 建一个检查点（元数据；字节另走 `set_checkpoint_object`）。**不可变：没有 update**。
pub async fn create_checkpoint(pool: &SqlitePool, in_: &CkptInput) -> Result<CkptRow, sqlx::Error> {
    let meta = if in_.meta.trim().is_empty() {
        "{}".to_string()
    } else {
        in_.meta.clone()
    };
    let media_type = if in_.media_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        in_.media_type.clone()
    };
    let id = new_id("CK");
    let now = now_go();
    sqlx::query(
        "INSERT INTO checkpoints (id, namespace_id, subject_ref, subject_version, name, label, step, \
         summary, tags, visibility, status, parent, digest, size, media_type, meta, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&in_.namespace_id)
    .bind(&in_.subject_ref)
    .bind(&in_.subject_version)
    .bind(&in_.name)
    .bind(&in_.label)
    .bind(in_.step)
    .bind(&in_.summary)
    .bind(tags_json(&in_.tags))
    .bind(&in_.visibility)
    .bind(&in_.parent)
    .bind(&in_.digest)
    .bind(in_.size)
    .bind(&media_type)
    .bind(&meta)
    .bind(&in_.author_id)
    .bind(&now)
    .execute(pool)
    .await?;
    by_required_ckpt(pool, &id).await
}

async fn by_required_ckpt(pool: &SqlitePool, id: &str) -> Result<CkptRow, sqlx::Error> {
    get_checkpoint(pool, id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

/// 字节落盘后回填对象名（服务端**核对过摘要**才调它）。
pub async fn set_checkpoint_object(
    pool: &SqlitePool,
    id: &str,
    object_key: &str,
    size: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE checkpoints SET object_key = ?, size = ? WHERE id = ?")
        .bind(object_key)
        .bind(size)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 列表（新的在前）。
pub async fn list_checkpoints(
    pool: &SqlitePool,
    opts: &CkptListOpts,
) -> Result<(Vec<CkptRow>, i64), sqlx::Error> {
    let w = ckpt_where(opts);

    let count_sql = format!(
        "SELECT COUNT(*) FROM checkpoints c JOIN namespaces ns ON ns.id = c.namespace_id{}",
        w.sql
    );
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &w.binds {
        cq = cq.bind(b);
    }
    let total = cq.fetch_one(pool).await?;

    let mut sql = format!(
        "SELECT {CKPT_COLS} FROM checkpoints c JOIN namespaces ns ON ns.id = c.namespace_id{} \
         ORDER BY c.created_at DESC, c.id DESC",
        w.sql
    );
    let rows = if opts.limit > 0 {
        sql.push_str(" LIMIT ?");
        bind_where!(sqlx::query_as::<_, CkptRow>(&sql), w)
            .bind(opts.limit)
            .fetch_all(pool)
            .await?
    } else {
        bind_where!(sqlx::query_as::<_, CkptRow>(&sql), w)
            .fetch_all(pool)
            .await?
    };
    Ok((rows, total))
}

/// 按 id 或名称取（同名取最新那个）。
pub async fn get_checkpoint(
    pool: &SqlitePool,
    id_or_name: &str,
) -> Result<Option<CkptRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {CKPT_COLS} FROM checkpoints c JOIN namespaces ns ON ns.id = c.namespace_id \
         WHERE (c.id = ? OR c.name = ?) ORDER BY c.created_at DESC LIMIT 1"
    );
    sqlx::query_as::<_, CkptRow>(&sql)
        .bind(id_or_name)
        .bind(id_or_name)
        .fetch_optional(pool)
        .await
}

/// 从某个点沿 parent 回溯（含自身，最新在前）。
pub async fn checkpoint_lineage(pool: &SqlitePool, id: &str) -> Result<Vec<CkptRow>, sqlx::Error> {
    let mut out: Vec<CkptRow> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut cur = id.to_string();
    // 上限防环：数据是人写的，别信它一定无环。
    for _ in 0..64 {
        if cur.is_empty() || seen.iter().any(|s| s == &cur) {
            break;
        }
        seen.push(cur.clone());
        let Some(row) = get_checkpoint(pool, &cur).await? else {
            break;
        };
        cur = row.parent.clone();
        out.push(row);
    }
    Ok(out)
}

/// 每个 subject 只留最新 `keep` 个（其余标 pruned 并返回要删的对象名）。
///
/// **标 pruned + 删字节、元数据留下**：这样「这里曾经有个点、后来被清理了」仍然可查，
/// 删干净会让历史出现无法解释的空洞。
///
/// 与 Go 的差异（有意的）：这里多一个 `ns_id` 过滤，只清理**目标命名空间内**的点。
/// Go 只按 `subject_ref` 过滤，而调用方校验的是「我自己的命名空间」——
/// 等于谁都能按 ref 把别人的点标成 pruned。跨命名空间写入必须拦住，所以这里收口。
///
/// `keep <= 0` 由**上层**拒掉（Go 侧 store 也报错，但 handler 先校验一遍）；
/// 这里回空列表而不是造一个错误类型出来，免得数据层去承担 HTTP 语义。
pub async fn prune_checkpoints(
    pool: &SqlitePool,
    ns_id: &str,
    subject_ref: &str,
    keep: i64,
) -> Result<Vec<String>, sqlx::Error> {
    if keep <= 0 {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT id, object_key FROM checkpoints WHERE status = 'active' AND namespace_id = ?",
    );
    if !subject_ref.is_empty() {
        sql.push_str(" AND subject_ref = ?");
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC");
    let mut q = sqlx::query_as::<_, (String, String)>(&sql).bind(ns_id);
    if !subject_ref.is_empty() {
        q = q.bind(subject_ref);
    }
    let rows = q.fetch_all(pool).await?;

    let mut doomed: Vec<String> = Vec::new();
    for (i, (id, object_key)) in rows.into_iter().enumerate() {
        if (i as i64) < keep {
            continue;
        }
        sqlx::query("UPDATE checkpoints SET status = 'pruned' WHERE id = ?")
            .bind(&id)
            .execute(pool)
            .await?;
        if !object_key.is_empty() {
            doomed.push(object_key);
        }
    }
    Ok(doomed)
}

/// 删一个（元数据也删；要「留个记录」请用 prune）。返回它的对象名。
pub async fn delete_checkpoint(pool: &SqlitePool, id: &str) -> Result<Option<String>, sqlx::Error> {
    let Some(row) = get_checkpoint(pool, id).await? else {
        return Ok(None);
    };
    sqlx::query("DELETE FROM checkpoints WHERE id = ?")
        .bind(&row.id)
        .execute(pool)
        .await?;
    Ok(Some(row.object_key))
}

pub async fn count_checkpoints(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM checkpoints WHERE status = 'active'")
        .fetch_one(pool)
        .await
}

/* ============================ 状态授权与计数 ============================ */

/// 是否拿到 `state` 授权。
///
/// 与 Go 的 `HasGrant(owner, grantee, "state", ns)` 逐字对齐：`ns_id` **为空串 =
/// 不限命名空间**（任意一条 state 授权都算），而不是「只匹配全局授权」——
/// 后者会让「别人授权给我的这条记忆」在列表里看得见、按引用却读不到。
/// 自己给自己、空 id 一律 false：授权是「别人给我的」，不该自证。
pub async fn has_state_grant(
    pool: &SqlitePool,
    owner_id: &str,
    grantee_id: &str,
    ns_id: &str,
) -> bool {
    if owner_id.is_empty() || grantee_id.is_empty() || owner_id == grantee_id {
        return false;
    }
    if ns_id.is_empty() {
        return exists(
            pool,
            "SELECT COUNT(*) FROM grants WHERE owner_id = ? AND grantee_user_id = ? AND kind = 'state'",
            &[owner_id, grantee_id],
        )
        .await
        .unwrap_or(false);
    }
    exists(
        pool,
        "SELECT COUNT(*) FROM grants WHERE owner_id = ? AND grantee_user_id = ? AND kind = 'state' \
         AND (namespace_id = '' OR namespace_id = ?)",
        &[owner_id, grantee_id, ns_id],
    )
    .await
    .unwrap_or(false)
}

/// 三样状态的计数（`/api/meta` 用）。
pub async fn state_counts(pool: &SqlitePool) -> Result<(i64, i64, i64), sqlx::Error> {
    let kb = count_kb_docs(pool).await?;
    let mem = count_mem_entries(pool).await?;
    let ckpt = count_checkpoints(pool).await?;
    Ok((kb, mem, ckpt))
}

/* ---------------- 「取用即声明」：内置集合声明行 ---------------- */

/// 内置集合的声明（只列本族三个 kind；`trace` 归轨迹那一族）。
struct BuiltinDecl {
    title: &'static str,
    summary: &'static str,
    reason: &'static str,
    fields: &'static [&'static str],
    index: &'static [&'static str],
    append_only: bool,
}

fn builtin_decl(kind: &str) -> Option<BuiltinDecl> {
    match kind {
        "kb" => Some(BuiltinDecl {
            title: "知识库",
            summary: "托管的语料：按 slug 取，可导出快照包",
            reason: "语料是 Agent 的长期上下文 —— 它按改名进版本，所以用 slug 当键（而不是 id）",
            fields: &[
                "slug:string!",
                "title:string!",
                "format:string",
                "kind:enum:doc|faq|notes|spec|transcript",
                "tags:string[]",
                "body:text?search",
            ],
            index: &["slug", "kind", "tags"],
            append_only: false,
        }),
        "mem" => Some(BuiltinDecl {
            title: "记忆",
            summary: "键值 + TTL + 来源：跨运行、跨机器记得住",
            reason: "记忆是「小结论」：按 (subject, key) 覆盖自己，读时判过期",
            fields: &[
                "subject:string!",
                "key:string!",
                "kind:enum:fact|preference|episode|summary|pointer",
                "source:string",
                "confidence:int",
                "pinned:bool",
                "value:text?search",
            ],
            index: &["subject", "kind"],
            append_only: false,
        }),
        "ckpt" => Some(BuiltinDecl {
            title: "检查点",
            summary: "不可变快照 + 血缘：交接与回滚的落点",
            reason: "检查点的价值是「拿回来的是原来那份」—— 所以只追加，字节进 blob、元数据进这里",
            fields: &[
                "name:string!",
                "label:enum:episode|step|run|release|handoff|manual",
                "digest:string!",
                "size:int",
                "subject_ref:string",
                "parent:string",
                "note:text?search",
            ],
            index: &["label", "subject_ref", "parent"],
            append_only: true,
        }),
        _ => None,
    }
}

/// 保证命名空间里有这个内置集合的声明行（幂等）——「取用即声明」：
/// 一台节点上从没写过记忆时 `ncc store ls` 里没有 `mem`，第一次写入之后它才出现，
/// 并且从此带上字段与红线。
///
/// 已存在时只把「契约面」（字段 / 索引 / 可变性 / 可见性 / 上限）按常量重新对齐，
/// 归属（命名空间 + kind）与已有记录都不动。
pub async fn ensure_builtin_collection(
    pool: &SqlitePool,
    ns_id: &str,
    kind: &str,
) -> Result<(), sqlx::Error> {
    let Some(decl) = builtin_decl(kind) else {
        return Ok(());
    };
    if ns_id.is_empty() {
        return Ok(());
    }
    let fields: Vec<String> = decl.fields.iter().map(|s| s.to_string()).collect();
    let index: Vec<String> = decl.index.iter().map(|s| s.to_string()).collect();
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM collections WHERE namespace_id = ? AND kind = ?")
            .bind(ns_id)
            .bind(kind)
            .fetch_optional(pool)
            .await?;

    let now = now_go();
    match existing {
        Some(id) => {
            sqlx::query(
                "UPDATE collections SET title = ?, summary = ?, reason = ?, mutable = ?, history = ?, \
                 append_only = ?, visibility = 'private', max_bytes = ?, fields = ?, `index` = ?, \
                 dedupe_by = '', default_ttl = 0, updated_at = ? WHERE id = ?",
            )
            .bind(decl.title)
            .bind(decl.summary)
            .bind(decl.reason)
            .bind(!decl.append_only)
            .bind(!decl.append_only)
            .bind(decl.append_only)
            .bind(256_i64 * 1024)
            .bind(marshal_list(&fields))
            .bind(marshal_list(&index))
            .bind(&now)
            .bind(&id)
            .execute(pool)
            .await?;
        }
        None => {
            sqlx::query(
                "INSERT INTO collections (id, namespace_id, kind, title, summary, reason, mutable, history, \
                 append_only, visibility, max_bytes, fields, `index`, dedupe_by, default_ttl, status, \
                 created_by, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'private', ?, ?, ?, '', 0, 'active', '', ?, ?)",
            )
            .bind(new_id("C"))
            .bind(ns_id)
            .bind(kind)
            .bind(decl.title)
            .bind(decl.summary)
            .bind(decl.reason)
            .bind(!decl.append_only)
            .bind(!decl.append_only)
            .bind(decl.append_only)
            .bind(256_i64 * 1024)
            .bind(marshal_list(&fields))
            .bind(marshal_list(&index))
            .bind(&now)
            .bind(&now)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    /// 每个测试一个**临时文件库**：`sqlite::memory:` 配连接池会退化成同名文件，
    /// 同进程的多个测试互相打架（表现为莫名其妙的 UNIQUE 冲突）。
    async fn pool(tag: &str) -> SqlitePool {
        let dir =
            std::env::temp_dir().join(format!("ncc-stack-store-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL)
            .await
            .unwrap();
        p
    }

    /// 建一个命名空间 + 机主用户（列表要 join users 取 owner_name）。
    async fn seed_ns(p: &SqlitePool, ns_id: &str, slug: &str, owner_id: &str, ns_type: &str) {
        sqlx::query(
            "INSERT INTO users (id, email, name, pass_hash, plan, is_admin, disabled) VALUES (?, ?, ?, '', 'free', 0, 0)",
        )
        .bind(owner_id)
        .bind(format!("{}@example.com", owner_id.to_lowercase()))
        .bind(format!("用户{owner_id}"))
        .execute(p)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO namespaces (id, slug, name, type, owner_id, visibility, created_at) VALUES (?, ?, ?, ?, ?, 'public', ?)",
        )
        .bind(ns_id)
        .bind(slug)
        .bind(format!("空间{slug}"))
        .bind(ns_type)
        .bind(owner_id)
        .bind(now_go())
        .execute(p)
        .await
        .unwrap();
    }

    fn scope_all() -> StateScope {
        StateScope {
            all: true,
            ..Default::default()
        }
    }

    fn kb_input(ns_id: &str, slug: &str, content: &str) -> KbInput {
        KbInput {
            namespace_id: ns_id.to_string(),
            slug: slug.to_string(),
            title: slug.to_string(),
            kind: "doc".to_string(),
            format: "markdown".to_string(),
            summary: String::new(),
            tags: vec!["prod".to_string()],
            visibility: "private".to_string(),
            source: String::new(),
            content: content.to_string(),
            checksum: format!(
                "sha256:{}",
                ncc_core::crypto::sha256_hex(content.as_bytes())
            ),
            note: String::new(),
            author_id: "U-1".to_string(),
            author_name: "u1@example.com".to_string(),
        }
    }

    #[tokio::test]
    async fn kb_建改留历史并按引用取出() {
        let p = pool("kb").await;
        seed_ns(&p, "NS-1", "team", "U-1", "org").await;
        let (row, created) = upsert_kb_doc(&p, &kb_input("NS-1", "handbook", "第一版"))
            .await
            .unwrap();
        assert!(created);
        assert_eq!(row.revision, 1);
        assert_eq!(row.ref_of(), "@team/handbook");
        assert_eq!(row.owner_name.as_deref(), Some("用户U-1"));
        assert_eq!(row.size, "第一版".len() as i64);

        let mut next = kb_input("NS-1", "handbook", "第二版");
        next.note = "改内容".to_string();
        let (row2, created2) = upsert_kb_doc(&p, &next).await.unwrap();
        assert!(!created2);
        assert_eq!(row2.revision, 2);
        assert_eq!(row2.content, "第二版");

        let revs = kb_revisions(&p, &row.id).await.unwrap();
        assert_eq!(revs.len(), 2);
        assert_eq!(revs[0].note, "初版");
        assert_eq!(revs[1].note, "改内容");
        // 历史永不改写：老正文还在
        assert_eq!(revs[0].content, "第一版");

        // 引用两种形态都能取
        assert!(get_kb_doc(&p, &row.id).await.unwrap().is_some());
        assert!(get_kb_doc(&p, "@team/handbook").await.unwrap().is_some());
        assert!(get_kb_doc(&p, "team/handbook").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn kb_归档默认不列_补丁不加版本() {
        let p = pool("kb-arch").await;
        seed_ns(&p, "NS-1", "team", "U-1", "org").await;
        let (row, _) = upsert_kb_doc(&p, &kb_input("NS-1", "handbook", "内容"))
            .await
            .unwrap();

        let patch = KbPatch {
            title: Some("手册".to_string()),
            status: Some("archived".to_string()),
            updated_by: "U-1".to_string(),
            ..Default::default()
        };
        patch_kb_doc(&p, &row.id, &patch).await.unwrap();

        let active = list_kb_docs(
            &p,
            &KbListOpts {
                scope: scope_all(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(active.1, 0);

        let with_arch = list_kb_docs(
            &p,
            &KbListOpts {
                scope: scope_all(),
                include_arch: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(with_arch.1, 1);
        assert_eq!(with_arch.0[0].title, "手册");
        // 补丁不动版本
        assert_eq!(with_arch.0[0].revision, 1);
    }

    #[tokio::test]
    async fn kb_检索打分与可见范围() {
        let p = pool("kb-rank").await;
        seed_ns(&p, "NS-1", "team", "U-1", "org").await;
        seed_ns(&p, "NS-2", "other", "U-2", "org").await;
        let mut a = kb_input("NS-1", "alpha", "部署手册");
        a.title = "部署".to_string();
        a.summary = "部署说明".to_string();
        upsert_kb_doc(&p, &a).await.unwrap();
        upsert_kb_doc(&p, &kb_input("NS-2", "beta", "别人的东西"))
            .await
            .unwrap();

        // fail-closed：什么都没给就查不到东西
        let none = list_kb_docs(&p, &KbListOpts::default()).await.unwrap();
        assert_eq!(none.1, 0);

        let mine = list_kb_docs(
            &p,
            &KbListOpts {
                scope: StateScope {
                    namespace_ids: vec!["NS-1".to_string()],
                    ..Default::default()
                },
                q: "部署".to_string(),
                rank: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(mine.1, 1);
        assert_eq!(mine.0[0].slug, "alpha");

        // 被授权者能看到别人的（按 owner 维度）
        let granted = list_kb_docs(
            &p,
            &KbListOpts {
                scope: StateScope {
                    granted_owners: vec!["U-2".to_string()],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(granted.1, 1);
        assert_eq!(granted.0[0].slug, "beta");

        // 公开档并进可见范围（匿名的唯一可见面）
        sqlx::query("UPDATE kb_docs SET visibility = 'public' WHERE slug = 'beta'")
            .execute(&p)
            .await
            .unwrap();
        let public = list_kb_docs(
            &p,
            &KbListOpts {
                include_public: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(public.1, 1);
        assert_eq!(public.0[0].slug, "beta");
    }

    #[tokio::test]
    async fn kb_分页与上限() {
        let p = pool("kb-page").await;
        seed_ns(&p, "NS-1", "team", "U-1", "org").await;
        for i in 0..5 {
            upsert_kb_doc(&p, &kb_input("NS-1", &format!("doc-{i}"), "x"))
                .await
                .unwrap();
        }
        let page1 = list_kb_docs(
            &p,
            &KbListOpts {
                scope: scope_all(),
                page: 1,
                size: 2,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(page1.0.len(), 2);
        assert_eq!(page1.1, 5);
        let limited = list_kb_docs(
            &p,
            &KbListOpts {
                scope: scope_all(),
                limit: 3,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(limited.0.len(), 3);
    }

    #[tokio::test]
    async fn kb_标签过滤与删除连历史() {
        let p = pool("kb-del").await;
        seed_ns(&p, "NS-1", "team", "U-1", "org").await;
        let (row, _) = upsert_kb_doc(&p, &kb_input("NS-1", "handbook", "内容"))
            .await
            .unwrap();
        let hit = list_kb_docs(
            &p,
            &KbListOpts {
                scope: scope_all(),
                tag: "prod".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(hit.1, 1);
        // "pro" 不该命中 "prod"（带引号匹配完整值）
        let miss = list_kb_docs(
            &p,
            &KbListOpts {
                scope: scope_all(),
                tag: "pro".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(miss.1, 0);

        delete_kb_doc(&p, &row.id).await.unwrap();
        assert!(kb_revisions(&p, &row.id).await.unwrap().is_empty());
        assert!(get_kb_doc(&p, &row.id).await.unwrap().is_none());
    }

    fn mem_input(ns_id: &str, key: &str, value: &str) -> MemInput {
        MemInput {
            namespace_id: ns_id.to_string(),
            subject: "self".to_string(),
            key: key.to_string(),
            value: value.to_string(),
            kind: "fact".to_string(),
            tags: vec![],
            source: "trace-1".to_string(),
            confidence: 800,
            pinned: false,
            ttl_days: 0,
            author_id: "U-1".to_string(),
        }
    }

    #[tokio::test]
    async fn mem_同键即更新且版本递增() {
        let p = pool("mem").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        let (row, created) = upsert_mem_entry(&p, &mem_input("NS-1", "skill", "rust"))
            .await
            .unwrap();
        assert!(created);
        assert_eq!(row.revision, 1);
        assert!(!row.pinned);

        let (row2, created2) = upsert_mem_entry(&p, &mem_input("NS-1", "skill", "go"))
            .await
            .unwrap();
        assert!(!created2);
        assert_eq!(row2.id, row.id);
        assert_eq!(row2.revision, 2);
        assert_eq!(row2.value, "go");
    }

    #[tokio::test]
    async fn mem_置顶不会被普通写入取消() {
        let p = pool("mem-pin").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        let mut inp = mem_input("NS-1", "k", "v");
        inp.pinned = true;
        let (row, _) = upsert_mem_entry(&p, &inp).await.unwrap();
        assert!(row.pinned);

        // 这次不带 pinned：原值保持
        let (row2, _) = upsert_mem_entry(&p, &mem_input("NS-1", "k", "v2"))
            .await
            .unwrap();
        assert!(row2.pinned);
        assert_eq!(row2.revision, 2);
    }

    #[tokio::test]
    async fn mem_过期读时判定_gc_真删() {
        let p = pool("mem-ttl").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        let mut inp = mem_input("NS-1", "tmp", "x");
        inp.ttl_days = 1;
        upsert_mem_entry(&p, &inp).await.unwrap();
        upsert_mem_entry(&p, &mem_input("NS-1", "keep", "y"))
            .await
            .unwrap();

        // 「现在」推到 3 天后：过期那条视为不存在
        let future =
            ncc_core::timeutil::format_go(chrono::Local::now().fixed_offset() + Duration::days(3));
        let listed = list_mem_entries(
            &p,
            &MemListOpts {
                scope: scope_all(),
                now: Some(future.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(listed.0.len(), 1);
        assert_eq!(listed.0[0].key, "keep");
        // include_expired 时它还在
        let all = list_mem_entries(
            &p,
            &MemListOpts {
                scope: scope_all(),
                include_expired: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(all.1, 2);

        // 主路径：按 (ns, subject, key) 取，过期即取不到
        assert!(get_mem_by_key(&p, "NS-1", "self", "tmp", Some(&future))
            .await
            .unwrap()
            .is_none());
        assert!(get_mem_by_key(&p, "NS-1", "self", "keep", Some(&future))
            .await
            .unwrap()
            .is_some());
        // 不传 now = 不判过期（只看这一行在不在）
        assert!(get_mem_by_key(&p, "NS-1", "self", "tmp", None)
            .await
            .unwrap()
            .is_some());

        let removed = gc_mem_entries(&p, "NS-1", Some(&future)).await.unwrap();
        assert_eq!(removed, 1);
        assert_eq!(count_mem_entries(&p).await.unwrap(), 1);
        // gc 只清指定命名空间
        assert_eq!(gc_mem_entries(&p, "", Some(&future)).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn mem_前缀与来源过滤() {
        let p = pool("mem-filter").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        upsert_mem_entry(&p, &mem_input("NS-1", "user.name", "a"))
            .await
            .unwrap();
        upsert_mem_entry(&p, &mem_input("NS-1", "user.lang", "b"))
            .await
            .unwrap();
        upsert_mem_entry(&p, &mem_input("NS-1", "other", "c"))
            .await
            .unwrap();

        let listed = list_mem_entries(
            &p,
            &MemListOpts {
                scope: scope_all(),
                prefix: "user.".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(listed.1, 2);

        let by_source = list_mem_entries(
            &p,
            &MemListOpts {
                scope: scope_all(),
                source: "trace-1".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_source.1, 3);

        // 置顶过滤
        let mut pin = mem_input("NS-1", "important", "!");
        pin.pinned = true;
        upsert_mem_entry(&p, &pin).await.unwrap();
        let pinned = list_mem_entries(
            &p,
            &MemListOpts {
                scope: scope_all(),
                pinned_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(pinned.1, 1);
        assert_eq!(pinned.0[0].key, "important");

        // 删除一条
        let row = get_mem_entry(&p, "other").await.unwrap().unwrap();
        delete_mem_entry(&p, &row.id).await.unwrap();
        assert!(get_mem_entry(&p, "other").await.unwrap().is_none());
    }

    fn ckpt_input(ns_id: &str, name: &str, parent: &str) -> CkptInput {
        CkptInput {
            namespace_id: ns_id.to_string(),
            subject_ref: "@team/agent".to_string(),
            subject_version: "1.0.0".to_string(),
            name: name.to_string(),
            label: "manual".to_string(),
            step: 1,
            summary: String::new(),
            tags: vec!["a".to_string()],
            visibility: "private".to_string(),
            parent: parent.to_string(),
            digest: String::new(),
            size: 0,
            media_type: String::new(),
            meta: String::new(),
            author_id: "U-1".to_string(),
        }
    }

    #[tokio::test]
    async fn ckpt_建点_回填字节_血缘回溯() {
        let p = pool("ckpt").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        let a = create_checkpoint(&p, &ckpt_input("NS-1", "起点", ""))
            .await
            .unwrap();
        assert_eq!(a.media_type, "application/octet-stream");
        assert_eq!(a.meta, "{}");
        assert_eq!(a.subject_ref, "@team/agent");

        let b = create_checkpoint(&p, &ckpt_input("NS-1", "第二步", &a.id))
            .await
            .unwrap();
        set_checkpoint_object(&p, &b.id, "ckpt-b-abc123", 12)
            .await
            .unwrap();
        let b2 = get_checkpoint(&p, &b.id).await.unwrap().unwrap();
        assert_eq!(b2.object_key, "ckpt-b-abc123");
        assert_eq!(b2.size, 12);

        let lineage = checkpoint_lineage(&p, &b.id).await.unwrap();
        assert_eq!(lineage.len(), 2);
        assert_eq!(lineage[0].id, b.id);
        assert_eq!(lineage[1].id, a.id);

        // 按名称取
        assert!(get_checkpoint(&p, "起点").await.unwrap().is_some());
        // 血缘里的点都是同一命名空间的（跨空间由上层拦）
        assert_eq!(b.subject_version, "1.0.0");
    }

    #[tokio::test]
    async fn ckpt_血缘上限防环() {
        let p = pool("ckpt-cycle").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        let a = create_checkpoint(&p, &ckpt_input("NS-1", "a", ""))
            .await
            .unwrap();
        let b = create_checkpoint(&p, &ckpt_input("NS-1", "b", &a.id))
            .await
            .unwrap();
        // 人为造环：a 的 parent 指向 b
        sqlx::query("UPDATE checkpoints SET parent = ? WHERE id = ?")
            .bind(&b.id)
            .bind(&a.id)
            .execute(&p)
            .await
            .unwrap();
        let lineage = checkpoint_lineage(&p, &a.id).await.unwrap();
        // 环被 seen 截住，不会转 64 圈
        assert_eq!(lineage.len(), 2);
        // 起点不存在时回溯回空表（不是报错）
        assert!(checkpoint_lineage(&p, "CK-不存在")
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn ckpt_prune_只留最新并交出对象名() {
        let p = pool("ckpt-prune").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        for i in 0..4 {
            let c = create_checkpoint(&p, &ckpt_input("NS-1", &format!("p{i}"), ""))
                .await
                .unwrap();
            set_checkpoint_object(&p, &c.id, &format!("obj-{i}"), 1)
                .await
                .unwrap();
        }
        // keep <= 0 由上层拒掉，数据层回空
        assert!(prune_checkpoints(&p, "NS-1", "@team/agent", 0)
            .await
            .unwrap()
            .is_empty());

        let doomed = prune_checkpoints(&p, "NS-1", "@team/agent", 2)
            .await
            .unwrap();
        assert_eq!(doomed.len(), 2);
        assert_eq!(count_checkpoints(&p).await.unwrap(), 2);
        // 标 pruned 而不是删干净：元数据留下，历史才没有空洞
        let all = list_checkpoints(
            &p,
            &CkptListOpts {
                scope: scope_all(),
                status: "pruned".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(all.1, 2);
        // 别的 subject 不受影响
        let mut other = ckpt_input("NS-1", "other", "");
        other.subject_ref = "@team/other".to_string();
        create_checkpoint(&p, &other).await.unwrap();
        assert_eq!(count_checkpoints(&p).await.unwrap(), 3);

        // 别的命名空间里**同 ref** 的点不该被动到（跨空间写入必须拦住）
        seed_ns(&p, "NS-2", "other-space", "U-2", "account").await;
        for i in 0..3 {
            let c = create_checkpoint(&p, &ckpt_input("NS-2", &format!("o{i}"), ""))
                .await
                .unwrap();
            set_checkpoint_object(&p, &c.id, &format!("other-{i}"), 1)
                .await
                .unwrap();
        }
        let doomed2 = prune_checkpoints(&p, "NS-2", "@team/agent", 1)
            .await
            .unwrap();
        assert_eq!(doomed2.len(), 2);
    }

    #[tokio::test]
    async fn ckpt_删除交出对象名_按引用过滤() {
        let p = pool("ckpt-del").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        let c = create_checkpoint(&p, &ckpt_input("NS-1", "x", ""))
            .await
            .unwrap();
        set_checkpoint_object(&p, &c.id, "obj-x", 3).await.unwrap();

        let listed = list_checkpoints(
            &p,
            &CkptListOpts {
                scope: scope_all(),
                subject_ref: "@team/agent".to_string(),
                label: "manual".to_string(),
                tag: "a".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(listed.1, 1);
        // 名称模糊匹配
        let by_name = list_checkpoints(
            &p,
            &CkptListOpts {
                scope: scope_all(),
                name: "x".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_name.1, 1);

        let key = delete_checkpoint(&p, &c.id).await.unwrap();
        assert_eq!(key.as_deref(), Some("obj-x"));
        assert!(get_checkpoint(&p, &c.id).await.unwrap().is_none());
        assert_eq!(count_checkpoints(&p).await.unwrap(), 0);
        // 删不存在的：回 None（元数据早就没了）
        assert!(delete_checkpoint(&p, "CK-无").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn state_授权判定与计数() {
        let p = pool("grant").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        seed_ns(&p, "NS-2", "other", "U-2", "account").await;
        upsert_kb_doc(&p, &kb_input("NS-1", "d", "c"))
            .await
            .unwrap();
        upsert_mem_entry(&p, &mem_input("NS-1", "k", "v"))
            .await
            .unwrap();
        create_checkpoint(&p, &ckpt_input("NS-1", "x", ""))
            .await
            .unwrap();

        assert_eq!(state_counts(&p).await.unwrap(), (1, 1, 1));

        // 自己给自己不算授权
        assert!(!has_state_grant(&p, "U-1", "U-1", "NS-1").await);
        assert!(!has_state_grant(&p, "", "U-9", "NS-1").await);
        sqlx::query(
            "INSERT INTO grants (id, owner_id, grantee_user_id, kind, namespace_id, note, created_at) \
             VALUES ('G-1', 'U-2', 'U-9', 'state', 'NS-2', '', ?)",
        )
        .bind(now_go())
        .execute(&p)
        .await
        .unwrap();
        assert!(has_state_grant(&p, "U-2", "U-9", "NS-2").await);
        // 限定命名空间的授权不外溢
        assert!(!has_state_grant(&p, "U-2", "U-9", "NS-1").await);
        // ns_id 空串 = 不限命名空间（Go 的 HasGrant 语义）
        assert!(has_state_grant(&p, "U-2", "U-9", "").await);
        // kind 之间不互相蕴含
        sqlx::query("UPDATE grants SET kind = 'config' WHERE id = 'G-1'")
            .execute(&p)
            .await
            .unwrap();
        assert!(!has_state_grant(&p, "U-2", "U-9", "").await);
    }

    #[tokio::test]
    async fn 取用即声明_内置集合幂等() {
        let p = pool("builtin").await;
        seed_ns(&p, "NS-1", "team", "U-1", "account").await;
        ensure_builtin_collection(&p, "NS-1", "mem").await.unwrap();
        ensure_builtin_collection(&p, "NS-1", "mem").await.unwrap();
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM collections WHERE namespace_id = 'NS-1'")
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(n, 1);
        let (kind, fields, status): (String, String, String) = sqlx::query_as(
            "SELECT kind, fields, status FROM collections WHERE namespace_id = 'NS-1'",
        )
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(kind, "mem");
        assert!(fields.contains("subject:string!"));
        assert_eq!(status, "active");

        // 不是本族的内置 kind：什么都不做（trace 归轨迹那一族）
        ensure_builtin_collection(&p, "NS-1", "trace")
            .await
            .unwrap();
        ensure_builtin_collection(&p, "NS-1", "不是内置")
            .await
            .unwrap();
        let n2: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM collections WHERE namespace_id = 'NS-1'")
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(n2, 1);
    }

    #[test]
    fn 拆分引用() {
        assert_eq!(split_ref("@team/x"), ("team".to_string(), "x".to_string()));
        assert_eq!(split_ref(" a/b/c "), ("a".to_string(), "b/c".to_string()));
        assert_eq!(split_ref("solo"), ("solo".to_string(), String::new()));
    }

    #[test]
    fn 打分按标题权重最高() {
        let mk = |title: &str, summary: &str, content: &str| KbRow {
            id: String::new(),
            namespace_id: String::new(),
            slug: String::new(),
            title: title.to_string(),
            kind: String::new(),
            format: String::new(),
            summary: summary.to_string(),
            tags: String::new(),
            visibility: String::new(),
            status: String::new(),
            revision: 0,
            content: content.to_string(),
            checksum: String::new(),
            size: 0,
            source: String::new(),
            search_text: String::new(),
            created_by: String::new(),
            updated_by: String::new(),
            created_at: None,
            updated_at: None,
            ns_slug: None,
            ns_name: None,
            owner_id: None,
            owner_name: None,
        };
        // 标题命中（3 分）要压过正文命中两次（2 分）
        let mut rows = vec![mk("x", "y", "部署 部署"), mk("部署", "", "")];
        rank_kb_rows(&mut rows, "部署");
        assert_eq!(rows[0].title, "部署");
        // 摘要命中（2 分）压过正文命中一次（1 分）
        let mut rows = vec![mk("a", "", "部署"), mk("b", "部署", "")];
        rank_kb_rows(&mut rows, "部署");
        assert_eq!(rows[0].title, "b");
        // 不命中的排最后
        let mut rows = vec![mk("a", "", "无关"), mk("b", "", "部署")];
        rank_kb_rows(&mut rows, "部署");
        assert_eq!(rows[0].title, "b");
    }
}
