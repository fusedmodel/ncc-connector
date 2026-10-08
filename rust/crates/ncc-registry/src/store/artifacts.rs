//! 制品（目录条目）的数据访问。
//!
//! 一条制品 = 一份**声明**（kind/name/version/summary/tags/manifest）+ 一份**字节**
//! （storage_url / blob_name / sha256 / size）。两者分开存是刻意的：字节可以在外部
//! （BYO 直链、对象存储、别的节点），声明必须在本节点 —— 这样目录与字节的可用性
//! 不再绑死在一起。

use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::now_go;

use super::{exists, marshal_list, parse_list};

/// 制品 + 其命名空间（一次 join，避免 N+1）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ArtifactRow {
    pub id: String,
    pub namespace_id: String,
    pub slug: String,
    pub kind: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub tags: String,
    pub visibility: String,
    pub status: String,
    pub manifest: String,
    pub storage_provider: String,
    pub storage_url: String,
    pub blob_name: String,
    pub sha256: String,
    pub size: i64,
    pub origin: String,
    pub origin_ref: String,
    pub created_by: String,
    pub downloads: i64,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub ns_slug: Option<String>,
    pub ns_name: Option<String>,
}

impl ArtifactRow {
    /// 规范引用 `@ns/slug`。
    pub fn ref_of(&self) -> String {
        format!(
            "@{}/{}",
            self.ns_slug.clone().unwrap_or_default(),
            self.slug
        )
    }

    /// 是不是别人分发过来的副本。
    pub fn is_replica(&self) -> bool {
        self.origin != "local"
    }

    /// 已发布的公开条目人人可见。
    pub fn is_public_published(&self) -> bool {
        self.status == "published" && self.visibility == "public"
    }
}

const SELECT_COLS: &str = "a.id, a.namespace_id, a.slug, a.kind, a.name, a.version, a.summary, \
     a.tags, a.visibility, a.status, a.manifest, a.storage_provider, a.storage_url, a.blob_name, \
     a.sha256, a.size, a.origin, a.origin_ref, a.created_by, a.downloads, a.created_at, a.updated_at, \
     n.slug AS ns_slug, n.name AS ns_name";

/// 列表筛选条件，字段与 `/api/registry` 的查询参数一一对应。
#[derive(Debug, Clone, Default)]
pub struct ListOpts {
    pub q: String,
    pub kind: String,
    pub tag: String,
    pub ns_slug: String,
    pub page: i64,
    pub size: i64,
    pub order_by: String,
    /// `mine=1` 时限定到这些命名空间。
    pub namespace_ids: Vec<String>,
    pub statuses: Vec<String>,
    /// 匿名 / 非成员视角：只看已发布的公开条目。
    pub public_only: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ListResult {
    pub rows: Vec<ArtifactRow>,
    pub total: i64,
}

/// 拼 where 子句与绑定值（列表与计数共用，避免两处条件写歪）。
fn where_clause(opts: &ListOpts) -> (String, Vec<String>) {
    let mut sql = String::from(" WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();

    if opts.public_only {
        sql.push_str(" AND a.visibility = 'public' AND a.status = 'published'");
    }
    if !opts.namespace_ids.is_empty() {
        let placeholders = vec!["?"; opts.namespace_ids.len()].join(",");
        sql.push_str(&format!(" AND a.namespace_id IN ({placeholders})"));
        binds.extend(opts.namespace_ids.iter().cloned());
    }
    if !opts.statuses.is_empty() {
        let placeholders = vec!["?"; opts.statuses.len()].join(",");
        sql.push_str(&format!(" AND a.status IN ({placeholders})"));
        binds.extend(opts.statuses.iter().cloned());
    }
    if !opts.kind.trim().is_empty() {
        sql.push_str(" AND a.kind = ?");
        binds.push(opts.kind.trim().to_string());
    }
    if !opts.ns_slug.trim().is_empty() {
        sql.push_str(" AND n.slug = ?");
        binds.push(opts.ns_slug.trim().trim_start_matches('@').to_string());
    }
    if !opts.tag.trim().is_empty() {
        // tags 存的是 JSON 数组文本；带引号匹配完整值，免得 "bar" 命中 "foobar"
        sql.push_str(" AND a.tags LIKE ?");
        binds.push(format!("%\"{}\"%", opts.tag.trim()));
    }
    if !opts.q.trim().is_empty() {
        sql.push_str(" AND (a.name LIKE ? OR a.slug LIKE ? OR a.summary LIKE ?)");
        let like = format!("%{}%", opts.q.trim());
        binds.push(like.clone());
        binds.push(like.clone());
        binds.push(like);
    }
    (sql, binds)
}

/// 列出制品（分页 + 总数）。
pub async fn list(pool: &SqlitePool, opts: &ListOpts) -> Result<ListResult, sqlx::Error> {
    let (where_sql, binds) = where_clause(opts);
    let page = opts.page.max(1);
    let size = opts.size.clamp(1, 200);

    let count_sql = format!(
        "SELECT COUNT(*) FROM artifacts a LEFT JOIN namespaces n ON n.id = a.namespace_id{where_sql}"
    );
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &binds {
        cq = cq.bind(b);
    }
    let total = cq.fetch_one(pool).await?;

    let order = match opts.order_by.trim() {
        "name" => "a.name ASC",
        "downloads" => "a.downloads DESC, a.updated_at DESC",
        "created" => "a.created_at DESC",
        _ => "a.updated_at DESC",
    };
    let sql = format!(
        "SELECT {SELECT_COLS} FROM artifacts a LEFT JOIN namespaces n ON n.id = a.namespace_id{where_sql} ORDER BY {order} LIMIT ? OFFSET ?"
    );
    let mut q = sqlx::query_as::<_, ArtifactRow>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    q = q.bind(size).bind((page - 1) * size);
    let rows = q.fetch_all(pool).await?;
    Ok(ListResult { rows, total })
}

pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<ArtifactRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM artifacts a LEFT JOIN namespaces n ON n.id = a.namespace_id WHERE a.id = ?"
    );
    sqlx::query_as::<_, ArtifactRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn by_ns_slug(
    pool: &SqlitePool,
    ns_slug: &str,
    slug: &str,
) -> Result<Option<ArtifactRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM artifacts a JOIN namespaces n ON n.id = a.namespace_id WHERE n.slug = ? AND a.slug = ?"
    );
    sqlx::query_as::<_, ArtifactRow>(&sql)
        .bind(ns_slug.trim().trim_start_matches('@'))
        .bind(slug)
        .fetch_optional(pool)
        .await
}

/// 按引用取：`A-…` 形式的 id，或 `@ns/slug`（可带 `@version`）。
pub async fn by_ref(pool: &SqlitePool, r: &str) -> Result<Option<ArtifactRow>, sqlx::Error> {
    let r = r.trim();
    if let Some(body) = r.strip_prefix('@') {
        let mut body = body;
        if let Some(i) = body.rfind('@') {
            if i > 0 {
                body = &body[..i];
            }
        }
        return match body.split_once('/') {
            Some((ns, slug)) => by_ns_slug(pool, ns, slug).await,
            None => Ok(None),
        };
    }
    by_id(pool, r).await
}

pub async fn exists_ns_slug(
    pool: &SqlitePool,
    ns_id: &str,
    slug: &str,
) -> Result<bool, sqlx::Error> {
    exists(
        pool,
        "SELECT COUNT(*) FROM artifacts WHERE namespace_id = ? AND slug = ?",
        &[ns_id, slug],
    )
    .await
}

/// 新建制品的入参。
pub struct NewArtifact {
    pub namespace_id: String,
    pub slug: String,
    pub kind: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub visibility: String,
    pub status: String,
    pub manifest: String,
    pub storage_provider: String,
    pub storage_url: String,
    pub blob_name: String,
    pub sha256: String,
    pub size: i64,
    pub created_by: String,
}

pub async fn create(pool: &SqlitePool, a: NewArtifact) -> Result<ArtifactRow, sqlx::Error> {
    let id = new_id("A");
    let now = now_go();
    sqlx::query(
        "INSERT INTO artifacts (id, namespace_id, slug, kind, name, version, summary, tags, visibility, status, manifest, storage_provider, storage_url, blob_name, sha256, size, origin, origin_ref, created_by, downloads, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'local', '', ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(&a.namespace_id)
    .bind(&a.slug)
    .bind(&a.kind)
    .bind(&a.name)
    .bind(&a.version)
    .bind(&a.summary)
    .bind(marshal_list(&a.tags))
    .bind(&a.visibility)
    .bind(&a.status)
    .bind(&a.manifest)
    .bind(&a.storage_provider)
    .bind(&a.storage_url)
    .bind(&a.blob_name)
    .bind(&a.sha256)
    .bind(a.size)
    .bind(&a.created_by)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    by_id(pool, &id).await?.ok_or(sqlx::Error::RowNotFound)
}

/// 局部更新：只改传进来的字段（`None` = 不动）。
#[derive(Debug, Clone, Default)]
pub struct Patch {
    pub name: Option<String>,
    pub version: Option<String>,
    pub summary: Option<String>,
    pub tags: Option<Vec<String>>,
    pub status: Option<String>,
    pub visibility: Option<String>,
    pub manifest: Option<String>,
    pub storage_url: Option<String>,
    pub blob_name: Option<String>,
    pub sha256: Option<String>,
    pub size: Option<i64>,
}

pub async fn patch(pool: &SqlitePool, id: &str, p: &Patch) -> Result<(), sqlx::Error> {
    let mut sets: Vec<&str> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    let mut ints: Vec<i64> = Vec::new();

    macro_rules! set_text {
        ($field:expr, $col:expr) => {
            if let Some(v) = $field.as_ref() {
                sets.push(concat!($col, " = ?"));
                binds.push(v.clone());
            }
        };
    }
    set_text!(p.name, "name");
    set_text!(p.version, "version");
    set_text!(p.summary, "summary");
    set_text!(p.status, "status");
    set_text!(p.visibility, "visibility");
    set_text!(p.manifest, "manifest");
    set_text!(p.storage_url, "storage_url");
    set_text!(p.blob_name, "blob_name");
    set_text!(p.sha256, "sha256");
    if let Some(tags) = p.tags.as_ref() {
        sets.push("tags = ?");
        binds.push(marshal_list(tags));
    }
    if let Some(size) = p.size {
        sets.push("size = ?");
        ints.push(size);
    }
    if sets.is_empty() {
        return Ok(());
    }
    sets.push("updated_at = ?");
    let sql = format!("UPDATE artifacts SET {} WHERE id = ?", sets.join(", "));
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    for i in &ints {
        q = q.bind(i);
    }
    q = q.bind(now_go()).bind(id);
    q.execute(pool).await?;
    Ok(())
}

pub async fn bump_downloads(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE artifacts SET downloads = downloads + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM artifacts WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 按 kind 统计（`/api/registry/kinds`）。
pub async fn kind_counts(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT kind, COUNT(*) FROM artifacts GROUP BY kind")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

pub async fn count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
        .fetch_one(pool)
        .await
}

/// 制品的标签列表（解析 JSON 文本）。
pub fn tags_of(a: &ArtifactRow) -> Vec<String> {
    parse_list(&a.tags)
}

/// 被托管的字节目录里，这个 URL 对应的对象名（不是本节点的对象返回空串）。
pub fn blob_name_from_url(public_url: &str, u: &str) -> String {
    let prefix = format!("{}/blobs/", public_url.trim_end_matches('/'));
    if let Some(rest) = u.strip_prefix(&prefix) {
        return rest.to_string();
    }
    match u.find("/blobs/") {
        Some(i) => u[i + "/blobs/".len()..].to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL)
            .await
            .unwrap();
        p
    }

    fn new_art(ns_id: &str, slug: &str, status: &str, vis: &str) -> NewArtifact {
        NewArtifact {
            namespace_id: ns_id.to_string(),
            slug: slug.to_string(),
            kind: "skill".to_string(),
            name: format!("名字 {slug}"),
            version: "1.0.0".to_string(),
            summary: String::new(),
            tags: vec!["a".to_string()],
            visibility: vis.to_string(),
            status: status.to_string(),
            manifest: String::new(),
            storage_provider: "local".to_string(),
            storage_url: "http://x/blobs/y".to_string(),
            blob_name: "y".to_string(),
            sha256: "abc".to_string(),
            size: 3,
            created_by: "U-1".to_string(),
        }
    }

    #[tokio::test]
    async fn 建制品与按引用查找() {
        let p = pool().await;
        let ns = crate::store::namespaces::create_account(&p, "U-1", "张三", "zhangsan")
            .await
            .unwrap();
        let a = create(&p, new_art(&ns.id, "demo", "published", "public"))
            .await
            .unwrap();
        assert_eq!(a.ref_of(), "@zhangsan/demo");
        assert!(!a.is_replica());
        assert!(a.is_public_published());
        assert_eq!(tags_of(&a), vec!["a"]);
        assert_eq!(
            by_ref(&p, "@zhangsan/demo").await.unwrap().unwrap().id,
            a.id
        );
        assert_eq!(
            by_ref(&p, "@zhangsan/demo@1.0.0")
                .await
                .unwrap()
                .unwrap()
                .id,
            a.id
        );
        assert_eq!(by_ref(&p, &a.id).await.unwrap().unwrap().id, a.id);
        assert!(by_ref(&p, "@nobody/x").await.unwrap().is_none());
        assert_eq!(count(&p).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn 列表可见性与筛选() {
        let p = pool().await;
        let ns = crate::store::namespaces::create_account(&p, "U-1", "张三", "zs")
            .await
            .unwrap();
        create(&p, new_art(&ns.id, "pub", "published", "public"))
            .await
            .unwrap();
        create(&p, new_art(&ns.id, "draft", "draft", "public"))
            .await
            .unwrap();
        create(&p, new_art(&ns.id, "priv", "published", "private"))
            .await
            .unwrap();

        let public = list(
            &p,
            &ListOpts {
                public_only: true,
                page: 1,
                size: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(public.total, 1);
        assert_eq!(public.rows[0].slug, "pub");

        let mine = list(
            &p,
            &ListOpts {
                namespace_ids: vec![ns.id.clone()],
                page: 1,
                size: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(mine.total, 3);

        let by_tag = list(
            &p,
            &ListOpts {
                tag: "a".to_string(),
                page: 1,
                size: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_tag.total, 3);

        let by_q = list(
            &p,
            &ListOpts {
                q: "名字 pub".to_string(),
                page: 1,
                size: 20,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(by_q.total, 1);
    }

    #[test]
    fn 字节对象名解析() {
        assert_eq!(
            blob_name_from_url(
                "http://localhost:8282",
                "http://localhost:8282/blobs/a/b.hur"
            ),
            "a/b.hur"
        );
        assert_eq!(
            blob_name_from_url("http://x", "https://cdn.example/e.hur"),
            ""
        );
    }
}
