//! NCC Config：团队配置托管的数据访问（条目 + 版本历史）。
//!
//! 原实现：`ncc-registry/store/config.go`。与制品的差别在于：制品是「字节 + 声明」，
//! 配置只有**一份内容列 + 一串永不改写的历史**，所以这里没有 blob，只有 CRUD 与
//! `config_revisions` 的追加。
//!
//! 刻意的取舍：
//!
//! * **历史只增不改**：每次写内容（含回滚）都追加一行 revision 并把 `revision+1`
//!   写回条目；只改元数据（改名/标签/归档）走 `patch`，不动版本 —— 否则「历史」
//!   里会混进一堆没改过内容的噪音。
//! * **计数只算公开且 active**：`kind_counts` / `env_counts` / `count_public` 会被
//!   匿名接口用到，「本节点有几条安全类配置」不该从匿名侧漏出去。
//! * **内容由上层决定明文或密文**：本层只存字符串，`secret=true` 的加密在
//!   httpapi 用 `state.seal()` 做（见 `httpapi/configs.rs`）—— 数据层不该持有密钥。

use std::collections::HashMap;

use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::now_go;

use super::{exists, marshal_list};

/// 配置 + 命名空间 / 归属者摘要（列表一次 join 出，避免 N+1）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConfigRow {
    pub id: String,
    pub namespace_id: String,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub environment: String,
    pub format: String,
    pub summary: String,
    pub tags: String,
    pub visibility: String,
    pub status: String,
    pub secret: bool,
    pub revision: i64,
    pub content: String,
    pub checksum: String,
    pub size: i64,
    pub created_by: String,
    pub updated_by: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub ns_slug: Option<String>,
    pub ns_name: Option<String>,
    pub owner_id: Option<String>,
    pub owner_name: Option<String>,
}

impl ConfigRow {
    /// 规范引用 `@命名空间/slug`。
    pub fn ref_of(&self) -> String {
        format!("@{}/{}", self.ns_slug.clone().unwrap_or_default(), self.slug)
    }

    /// 公开且 active —— 这一条是「谁都能读」的唯一入口。
    pub fn is_public_active(&self) -> bool {
        self.visibility == "public" && self.status == "active"
    }
}

/// 一版历史（内容同样是明文或密文，取决于当初写入时 secret 的取值）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConfigRevision {
    pub id: String,
    pub config_id: String,
    pub revision: i64,
    pub content: String,
    pub checksum: String,
    pub size: i64,
    pub secret: bool,
    pub note: String,
    pub author_id: String,
    pub author_name: String,
    pub created_at: Option<String>,
}

/// 写入一条配置（`content` 由上层决定已加密还是明文）。
#[derive(Debug, Clone, Default)]
pub struct ConfigInput {
    pub namespace_id: String,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub environment: String,
    pub format: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub visibility: String,
    pub status: String,
    pub secret: bool,
    pub content: String,
    pub checksum: String,
    pub size: i64,
    pub note: String,
    pub author_id: String,
    pub author_name: String,
}

/// 只改元数据的补丁（不动内容，也不加版本）。
///
/// `secret`/`content` 成对出现：仅在「开关 secret」时更新内容（重新加密或解密回存）。
#[derive(Debug, Clone, Default)]
pub struct MetaPatch {
    pub name: String,
    pub kind: String,
    pub environment: String,
    pub format: String,
    pub summary: String,
    pub tags: String,
    pub visibility: String,
    pub status: String,
    pub updated_by: String,
    pub secret: Option<bool>,
    pub content: Option<String>,
}

/// 检索条件，字段与 `/api/configs` 的查询参数一一对应。
#[derive(Debug, Clone, Default)]
pub struct ListOpts {
    /// 限定这些命名空间（mine / bundle 用）。
    pub namespace_ids: Vec<String>,
    /// 全局视图：`(公开+active) ∪ 我的命名空间 ∪ 把 config 授权给我的人所在命名空间`。
    pub visible: bool,
    pub granted_owners: Vec<String>,
    pub ns_slug: String,
    pub kind: String,
    pub env: String,
    pub tag: String,
    pub q: String,
    pub secrets_only: bool,
    pub no_secrets: bool,
    pub statuses: Vec<String>,
    pub public_only: bool,
    pub page: i64,
    pub size: i64,
    /// 显式条数上限（bundle 用 200）；为 0 时回落到 size 的默认/上限规则。
    pub limit: i64,
}

const SELECT_COLS: &str = "c.id, c.namespace_id, c.slug, c.name, c.kind, c.environment, c.format, \
     c.summary, c.tags, c.visibility, c.status, c.secret, c.revision, c.content, c.checksum, c.size, \
     c.created_by, c.updated_by, c.created_at, c.updated_at, \
     ns.slug AS ns_slug, ns.name AS ns_name, ns.owner_id AS owner_id, u.name AS owner_name";

const REV_COLS: &str =
    "id, config_id, revision, content, checksum, size, secret, note, author_id, author_name, created_at";

/// 拼 where 子句与绑定值（列表与计数共用，避免两处条件写歪）。
fn where_clause(opts: &ListOpts) -> (String, Vec<String>) {
    let mut sql = String::from(" WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();

    if opts.visible {
        // 可见性是一条 OR 条件；空切片不能拼进 IN，所以按有没有值分别构造。
        let mut conds = vec!["(c.visibility = 'public' AND c.status = 'active')".to_string()];
        if !opts.namespace_ids.is_empty() {
            let ph = vec!["?"; opts.namespace_ids.len()].join(",");
            conds.push(format!("c.namespace_id IN ({ph})"));
            binds.extend(opts.namespace_ids.iter().cloned());
        }
        if !opts.granted_owners.is_empty() {
            let ph = vec!["?"; opts.granted_owners.len()].join(",");
            conds.push(format!("ns.owner_id IN ({ph})"));
            binds.extend(opts.granted_owners.iter().cloned());
        }
        sql.push_str(&format!(" AND ({})", conds.join(" OR ")));
    }
    if !opts.namespace_ids.is_empty() && !opts.visible {
        let ph = vec!["?"; opts.namespace_ids.len()].join(",");
        sql.push_str(&format!(" AND c.namespace_id IN ({ph})"));
        binds.extend(opts.namespace_ids.iter().cloned());
    }
    if !opts.ns_slug.trim().is_empty() {
        // 引用写作 @ns/slug，查询参数也可能带 @：统一去掉再比，免得 @team 查不到。
        sql.push_str(" AND ns.slug = ?");
        binds.push(opts.ns_slug.trim().trim_start_matches('@').to_string());
    }
    if !opts.kind.trim().is_empty() {
        sql.push_str(" AND c.kind = ?");
        binds.push(opts.kind.trim().to_string());
    }
    if !opts.env.trim().is_empty() {
        // 「any = 与环境无关」对任何环境查询都算命中，否则 prod 的 bundle 会漏掉通用配置。
        sql.push_str(" AND (c.environment = ? OR c.environment = 'any')");
        binds.push(opts.env.trim().to_string());
    }
    if !opts.tag.trim().is_empty() {
        // tags 存 JSON 数组文本，带引号匹配完整值，免得 "bar" 命中 "foobar"
        sql.push_str(" AND c.tags LIKE ?");
        binds.push(format!("%\"{}\"%", opts.tag.trim()));
    }
    if !opts.q.trim().is_empty() {
        sql.push_str(" AND (c.name LIKE ? OR c.slug LIKE ? OR c.summary LIKE ? OR c.tags LIKE ?)");
        let like = format!("%{}%", opts.q.trim());
        for _ in 0..4 {
            binds.push(like.clone());
        }
    }
    if opts.secrets_only {
        sql.push_str(" AND c.secret = 1");
    }
    if opts.no_secrets {
        sql.push_str(" AND c.secret = 0");
    }
    if !opts.statuses.is_empty() {
        let ph = vec!["?"; opts.statuses.len()].join(",");
        sql.push_str(&format!(" AND c.status IN ({ph})"));
        binds.extend(opts.statuses.iter().cloned());
    }
    if opts.public_only {
        sql.push_str(" AND c.visibility = 'public' AND c.status = 'active'");
    }
    (sql, binds)
}

/// 列出配置（分页 + 总数），按 `updated_at DESC`。
pub async fn list(pool: &SqlitePool, opts: &ListOpts) -> Result<(Vec<ConfigRow>, i64), sqlx::Error> {
    let (where_sql, binds) = where_clause(opts);

    let count_sql = format!(
        "SELECT COUNT(*) FROM config_entries c JOIN namespaces ns ON ns.id = c.namespace_id{where_sql}"
    );
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &binds {
        cq = cq.bind(b);
    }
    let total = cq.fetch_one(pool).await?;

    let mut limit = opts.limit;
    if limit <= 0 {
        limit = opts.size;
        if limit <= 0 {
            limit = 20;
        }
        if limit > 200 {
            limit = 200;
        }
    }
    let page = if opts.page < 1 { 1 } else { opts.page };

    let sql = format!(
        "SELECT {SELECT_COLS} FROM config_entries c JOIN namespaces ns ON ns.id = c.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id{where_sql} ORDER BY c.updated_at DESC LIMIT ? OFFSET ?"
    );
    let mut q = sqlx::query_as::<_, ConfigRow>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    q = q.bind(limit).bind((page - 1) * limit);
    let rows = q.fetch_all(pool).await?;
    Ok((rows, total))
}

pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<ConfigRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM config_entries c JOIN namespaces ns ON ns.id = c.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id WHERE c.id = ?"
    );
    sqlx::query_as::<_, ConfigRow>(&sql).bind(id).fetch_optional(pool).await
}

/// 按「命名空间 + slug」取（外部引用的写法：`@team/network`）。
pub async fn by_ns_slug(
    pool: &SqlitePool,
    ns_slug: &str,
    slug: &str,
) -> Result<Option<ConfigRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM config_entries c JOIN namespaces ns ON ns.id = c.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id WHERE ns.slug = ? AND c.slug = ?"
    );
    sqlx::query_as::<_, ConfigRow>(&sql)
        .bind(ns_slug.trim().trim_start_matches('@'))
        .bind(slug)
        .fetch_optional(pool)
        .await
}

/// 按引用取：`C-…` id 或 `@ns/slug`。
pub async fn by_ref(pool: &SqlitePool, r: &str) -> Result<Option<ConfigRow>, sqlx::Error> {
    let r = r.trim();
    if r.is_empty() {
        return Ok(None);
    }
    if let Some(body) = r.strip_prefix('@') {
        return match body.find('/') {
            Some(i) if i > 0 => by_ns_slug(pool, &body[..i], &body[i + 1..]).await,
            _ => Ok(None),
        };
    }
    by_id(pool, r).await
}

/// 同一命名空间下 slug 是否已占用。
pub async fn slug_exists(pool: &SqlitePool, ns_id: &str, slug: &str) -> Result<bool, sqlx::Error> {
    exists(
        pool,
        "SELECT COUNT(*) FROM config_entries WHERE namespace_id = ? AND slug = ?",
        &[ns_id, slug],
    )
    .await
}

pub async fn count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM config_entries")
        .fetch_one(pool)
        .await
}

/// 命名空间下的配置数（配额检查用）。
pub async fn count_in_namespace(pool: &SqlitePool, ns_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM config_entries WHERE namespace_id = ?")
        .bind(ns_id)
        .fetch_one(pool)
        .await
}

fn tags_json(tags: &[String]) -> String {
    if tags.is_empty() {
        "[]".to_string()
    } else {
        marshal_list(tags)
    }
}

fn first_note(note: &str) -> String {
    if note.is_empty() {
        "（未写变更说明）".to_string()
    } else {
        note.to_string()
    }
}

/// 建一条配置，并写入第 1 个版本。返回新条目 id。
pub async fn create(pool: &SqlitePool, in_: &ConfigInput) -> Result<String, sqlx::Error> {
    let id = new_id("C");
    let now = now_go();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO config_entries \
         (id, namespace_id, slug, name, kind, environment, format, summary, tags, visibility, status, \
          secret, revision, content, checksum, size, created_by, updated_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&in_.namespace_id)
    .bind(&in_.slug)
    .bind(&in_.name)
    .bind(&in_.kind)
    .bind(&in_.environment)
    .bind(&in_.format)
    .bind(&in_.summary)
    .bind(tags_json(&in_.tags))
    .bind(&in_.visibility)
    .bind(&in_.status)
    .bind(in_.secret)
    .bind(1_i64)
    .bind(&in_.content)
    .bind(&in_.checksum)
    .bind(in_.size)
    .bind(&in_.author_id)
    .bind(&in_.author_id)
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO config_revisions \
         (id, config_id, revision, content, checksum, size, secret, note, author_id, author_name, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id("CR"))
    .bind(&id)
    .bind(1_i64)
    .bind(&in_.content)
    .bind(&in_.checksum)
    .bind(in_.size)
    .bind(in_.secret)
    .bind(first_note(&in_.note))
    .bind(&in_.author_id)
    .bind(&in_.author_name)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(id)
}

/// 写入新内容：版本号 +1 并追加历史（历史永不改写）。返回新版本号。
pub async fn update(pool: &SqlitePool, id: &str, in_: &ConfigInput) -> Result<i64, sqlx::Error> {
    let now = now_go();
    let mut tx = pool.begin().await?;
    let cur: i64 = sqlx::query_scalar("SELECT revision FROM config_entries WHERE id = ?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let next = cur + 1;

    sqlx::query(
        "UPDATE config_entries SET name = ?, kind = ?, environment = ?, format = ?, summary = ?, tags = ?, \
         visibility = ?, status = ?, secret = ?, revision = ?, content = ?, checksum = ?, size = ?, \
         updated_by = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&in_.name)
    .bind(&in_.kind)
    .bind(&in_.environment)
    .bind(&in_.format)
    .bind(&in_.summary)
    .bind(tags_json(&in_.tags))
    .bind(&in_.visibility)
    .bind(&in_.status)
    .bind(in_.secret)
    .bind(next)
    .bind(&in_.content)
    .bind(&in_.checksum)
    .bind(in_.size)
    .bind(&in_.author_id)
    .bind(&now)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO config_revisions \
         (id, config_id, revision, content, checksum, size, secret, note, author_id, author_name, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id("CR"))
    .bind(id)
    .bind(next)
    .bind(&in_.content)
    .bind(&in_.checksum)
    .bind(in_.size)
    .bind(in_.secret)
    .bind(first_note(&in_.note))
    .bind(&in_.author_id)
    .bind(&in_.author_name)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(next)
}

/// 只改元数据（改名 / 改标签 / 归档 / 切换可见性），**不动内容也不加版本** ——
/// 把「配置内容变了」与「只是改了备注」区分开，版本历史才有意义。
pub async fn patch(pool: &SqlitePool, id: &str, p: &MetaPatch) -> Result<(), sqlx::Error> {
    let mut sql = String::from(
        "UPDATE config_entries SET name = ?, kind = ?, environment = ?, format = ?, summary = ?, tags = ?, \
         visibility = ?, status = ?, updated_by = ?, updated_at = ?",
    );
    if p.secret.is_some() {
        sql.push_str(", secret = ?, content = ?");
    }
    sql.push_str(" WHERE id = ?");

    let mut q = sqlx::query(&sql)
        .bind(&p.name)
        .bind(&p.kind)
        .bind(&p.environment)
        .bind(&p.format)
        .bind(&p.summary)
        .bind(&p.tags)
        .bind(&p.visibility)
        .bind(&p.status)
        .bind(&p.updated_by)
        .bind(now_go());
    if let Some(s) = p.secret {
        q = q.bind(s).bind(p.content.clone().unwrap_or_default());
    }
    q.bind(id).execute(pool).await?;
    Ok(())
}

/// 删除配置，连同它的历史一起删。
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM config_revisions WHERE config_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM config_entries WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// 版本历史（新→旧，最多 200 条）。
pub async fn list_revisions(
    pool: &SqlitePool,
    config_id: &str,
) -> Result<Vec<ConfigRevision>, sqlx::Error> {
    let sql = format!(
        "SELECT {REV_COLS} FROM config_revisions WHERE config_id = ? ORDER BY revision DESC LIMIT 200"
    );
    sqlx::query_as::<_, ConfigRevision>(&sql)
        .bind(config_id)
        .fetch_all(pool)
        .await
}

pub async fn find_revision(
    pool: &SqlitePool,
    config_id: &str,
    revision: i64,
) -> Result<Option<ConfigRevision>, sqlx::Error> {
    let sql = format!("SELECT {REV_COLS} FROM config_revisions WHERE config_id = ? AND revision = ?");
    sqlx::query_as::<_, ConfigRevision>(&sql)
        .bind(config_id)
        .bind(revision)
        .fetch_optional(pool)
        .await
}

/// 各类配置的数量（**只统计公开且 active 的**）。
pub async fn kind_counts(pool: &SqlitePool) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, COUNT(*) FROM config_entries WHERE visibility = 'public' AND status = 'active' GROUP BY kind",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// 各环境的配置数量（同样只算公开且 active 的）。
pub async fn env_counts(pool: &SqlitePool) -> Result<HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT environment, COUNT(*) FROM config_entries WHERE visibility = 'public' AND status = 'active' GROUP BY environment",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// 公开且 active 的配置数（匿名控制台用）。
pub async fn count_public(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM config_entries WHERE visibility = 'public' AND status = 'active'",
    )
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn pool() -> SqlitePool {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL).await.unwrap();
        p
    }

    /// 建一个命名空间 + 机主用户（列表要 join users 取 owner_name）。
    async fn seed_ns(p: &SqlitePool, ns_id: &str, slug: &str, owner_id: &str) {
        sqlx::query(
            "INSERT INTO users (id, email, name, pass_hash, plan, is_admin, disabled) VALUES (?, ?, ?, '', 'free', 0, 0)",
        )
        .bind(owner_id)
        .bind(format!("{owner_id}@example.com"))
        .bind(format!("用户{owner_id}"))
        .execute(p)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO namespaces (id, slug, name, type, owner_id, visibility, created_at) VALUES (?, ?, ?, 'account', ?, 'public', ?)",
        )
        .bind(ns_id)
        .bind(slug)
        .bind(format!("空间{slug}"))
        .bind(owner_id)
        .bind(now_go())
        .execute(p)
        .await
        .unwrap();
    }

    fn input(ns_id: &str, slug: &str) -> ConfigInput {
        ConfigInput {
            namespace_id: ns_id.to_string(),
            slug: slug.to_string(),
            name: slug.to_string(),
            kind: "network".to_string(),
            environment: "any".to_string(),
            format: "yaml".to_string(),
            summary: String::new(),
            tags: vec!["prod".to_string()],
            visibility: "private".to_string(),
            status: "active".to_string(),
            secret: false,
            content: "a: 1".to_string(),
            checksum: ncc_core::crypto::sha256_hex(b"a: 1"),
            size: 4,
            note: String::new(),
            author_id: "U-1".to_string(),
            author_name: "u1@example.com".to_string(),
        }
    }

    #[tokio::test]
    async fn 创建写入首版并可按下划线引用取出() {
        let p = pool().await;
        seed_ns(&p, "NS-1", "team", "U-1").await;
        let id = create(&p, &input("NS-1", "network")).await.unwrap();
        let row = by_id(&p, &id).await.unwrap().unwrap();
        assert_eq!(row.revision, 1);
        assert_eq!(row.ref_of(), "@team/network");
        assert_eq!(row.owner_name.as_deref(), Some("用户U-1"));
        assert!(slug_exists(&p, "NS-1", "network").await.unwrap());
        let via_ref = by_ref(&p, "@team/network").await.unwrap().unwrap();
        assert_eq!(via_ref.id, id);
        // 单段 id 形态也能取到
        assert!(by_ref(&p, &id).await.unwrap().is_some());
        let revs = list_revisions(&p, &id).await.unwrap();
        assert_eq!(revs.len(), 1);
        assert_eq!(revs[0].note, "（未写变更说明）");
    }

    #[tokio::test]
    async fn 更新追加版本_历史永不改写() {
        let p = pool().await;
        seed_ns(&p, "NS-1", "team", "U-1").await;
        let id = create(&p, &input("NS-1", "network")).await.unwrap();
        let mut next = input("NS-1", "network");
        next.content = "a: 2".to_string();
        next.checksum = ncc_core::crypto::sha256_hex(b"a: 2");
        next.size = 4;
        next.note = "改端口".to_string();
        let rev = update(&p, &id, &next).await.unwrap();
        assert_eq!(rev, 2);
        let row = by_id(&p, &id).await.unwrap().unwrap();
        assert_eq!(row.revision, 2);
        assert_eq!(row.content, "a: 2");
        let revs = list_revisions(&p, &id).await.unwrap();
        assert_eq!(revs.len(), 2);
        // 旧版本内容仍在
        assert_eq!(revs[1].content, "a: 1");
        assert_eq!(revs[1].note, "（未写变更说明）");
        assert_eq!(find_revision(&p, &id, 1).await.unwrap().unwrap().content, "a: 1");
    }

    #[tokio::test]
    async fn 元数据补丁不加版本() {
        let p = pool().await;
        seed_ns(&p, "NS-1", "team", "U-1").await;
        let id = create(&p, &input("NS-1", "network")).await.unwrap();
        let patch_in = MetaPatch {
            name: "网络配置".to_string(),
            kind: "network".to_string(),
            environment: "prod".to_string(),
            format: "yaml".to_string(),
            summary: "备注".to_string(),
            tags: r#"["a","b"]"#.to_string(),
            visibility: "private".to_string(),
            status: "archived".to_string(),
            updated_by: "U-1".to_string(),
            secret: None,
            content: None,
        };
        patch(&p, &id, &patch_in).await.unwrap();
        let row = by_id(&p, &id).await.unwrap().unwrap();
        assert_eq!(row.name, "网络配置");
        assert_eq!(row.status, "archived");
        assert_eq!(row.revision, 1); // 没动内容就没加版本
        assert_eq!(list_revisions(&p, &id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn 删除连同历史一起删() {
        let p = pool().await;
        seed_ns(&p, "NS-1", "team", "U-1").await;
        let id = create(&p, &input("NS-1", "network")).await.unwrap();
        delete(&p, &id).await.unwrap();
        assert!(by_id(&p, &id).await.unwrap().is_none());
        assert!(list_revisions(&p, &id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn 计数只算公开且_active() {
        let p = pool().await;
        seed_ns(&p, "NS-1", "team", "U-1").await;
        let _private = create(&p, &input("NS-1", "priv")).await.unwrap();
        let mut pub_in = input("NS-1", "pub");
        pub_in.visibility = "public".to_string();
        let pub_id = create(&p, &pub_in).await.unwrap();
        let mut arch = input("NS-1", "old");
        arch.visibility = "public".to_string();
        arch.status = "archived".to_string();
        create(&p, &arch).await.unwrap();

        assert_eq!(count(&p).await.unwrap(), 3);
        assert_eq!(count_in_namespace(&p, "NS-1").await.unwrap(), 3);
        assert_eq!(count_public(&p).await.unwrap(), 1);
        assert_eq!(kind_counts(&p).await.unwrap().get("network"), Some(&1));
        assert_eq!(env_counts(&p).await.unwrap().get("any"), Some(&1));
        assert_eq!(by_id(&p, &pub_id).await.unwrap().unwrap().revision, 1);
    }

    #[tokio::test]
    async fn 可见性过滤_公开_only_与环境_any_兜底() {
        let p = pool().await;
        seed_ns(&p, "NS-1", "team", "U-1").await;
        let mut pub_prod = input("NS-1", "pub-prod");
        pub_prod.visibility = "public".to_string();
        pub_prod.environment = "prod".to_string();
        create(&p, &pub_prod).await.unwrap();
        let mut pub_any = input("NS-1", "pub-any");
        pub_any.visibility = "public".to_string();
        create(&p, &pub_any).await.unwrap();
        create(&p, &input("NS-1", "priv-any")).await.unwrap();

        let public = list(
            &p,
            &ListOpts {
                public_only: true,
                env: "prod".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // prod 查询同时命中 prod 与 any，且不含私有
        assert_eq!(public.1, 2);
        assert!(public.0.iter().all(|r| r.visibility == "public"));

        let visible = list(
            &p,
            &ListOpts {
                visible: true,
                namespace_ids: vec!["NS-1".to_string()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(visible.1, 3);
    }
}
