//! 节点治理面（admin）的数据访问：用户 / 节点 / 服务 / 审计 / admin key。
//!
//! 与「我的资产」那套的分界：这里**不带 ownerID 过滤** —— 管理员的视角本来就是全节点。
//! 每条写操作都必须由 HTTP 层先过 `require_admin`，别把这些函数暴露给普通路径的 caller。
//!
//! `AdminKey` 与接入票据（AccessTicket）形态刻意一致：短 key 可念、secret 只显示一次、
//! 库里只有 sha256。区别在语义：票据兑换出**受限节点令牌**，admin key 换来的是**治理权**。

use sqlx::SqlitePool;

use ncc_core::crypto::rand_hex;
use ncc_core::ids::new_id;
use ncc_core::timeutil::now_go;

use super::artifacts::ArtifactRow;
use super::hash_secret;
use super::nodes::NodeRow;

/* ---------------- 用户 ---------------- */

/// 用户 + 他名下的规模（管理台一眼看清「谁在用」）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AdminUserRow {
    pub id: String,
    pub email: String,
    pub name: String,
    pub pass_hash: String,
    pub plan: String,
    pub is_admin: bool,
    pub disabled: bool,
    pub disabled_at: Option<String>,
    pub admin_note: String,
    pub last_login_at: Option<String>,
    pub created_at: Option<String>,
    pub nodes: i64,
    pub artifacts: i64,
}

// 显式列出 users.* 之外的列：join/子查询混用时不写清楚，列名会互相覆盖。
const ADMIN_USER_SELECT: &str = "SELECT users.*, \
     (SELECT COUNT(*) FROM hosted_nodes WHERE namespace_id IN (SELECT id FROM namespaces WHERE owner_id = users.id)) AS nodes, \
     (SELECT COUNT(*) FROM artifacts WHERE namespace_id IN (SELECT id FROM namespaces WHERE owner_id = users.id)) AS artifacts \
     FROM users";

/// 用户列表（管理员视角，含被禁用的）。
pub async fn list_users(
    pool: &SqlitePool,
    q: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AdminUserRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 200 { 50 } else { limit };
    let mut sql = ADMIN_USER_SELECT.to_string();
    let mut binds: Vec<String> = Vec::new();
    if !q.trim().is_empty() {
        sql.push_str(" WHERE users.email LIKE ? OR users.name LIKE ? OR users.id LIKE ?");
        let like = format!("%{}%", q.trim());
        binds.push(like.clone());
        binds.push(like.clone());
        binds.push(like);
    }
    sql.push_str(" ORDER BY users.created_at DESC LIMIT ? OFFSET ?");
    let mut query = sqlx::query_as::<_, AdminUserRow>(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    query.bind(limit).bind(offset.max(0)).fetch_all(pool).await
}

/// 与 `list_users` 同口径的总数（分开写：Count 不能带 Select 子查询）。
pub async fn count_users_filtered(pool: &SqlitePool, q: &str) -> Result<i64, sqlx::Error> {
    if q.trim().is_empty() {
        return sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(pool)
            .await;
    }
    let like = format!("%{}%", q.trim());
    sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE email LIKE ? OR name LIKE ? OR id LIKE ?")
        .bind(&like)
        .bind(&like)
        .bind(&like)
        .fetch_one(pool)
        .await
}

/// 禁用 / 启用 + 备注。禁用立即生效：auth 每次都回查库，旧令牌不会继续通行。
pub async fn set_user_disabled(
    pool: &SqlitePool,
    id: &str,
    disabled: bool,
    note: Option<&str>,
) -> Result<(), sqlx::Error> {
    let at = if disabled { Some(now_go()) } else { None };
    sqlx::query("UPDATE users SET disabled = ?, disabled_at = ? WHERE id = ?")
        .bind(disabled)
        .bind(at)
        .bind(id)
        .execute(pool)
        .await?;
    if let Some(n) = note.map(str::trim).filter(|s| !s.is_empty()) {
        sqlx::query("UPDATE users SET admin_note = ? WHERE id = ?")
            .bind(n)
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// 本节点管理员数量（用于「最后一个管理员不能被禁用」这类判断）。
pub async fn count_admins(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE is_admin = ? AND disabled = ?")
        .bind(true)
        .bind(false)
        .fetch_one(pool)
        .await
}

/* ---------------- 节点（管理面看全部） ---------------- */

const NODE_COLS: &str = "hosted_nodes.id, hosted_nodes.namespace_id, hosted_nodes.slug, hosted_nodes.name, \
     hosted_nodes.kind, hosted_nodes.region, hosted_nodes.url, hosted_nodes.os, hosted_nodes.arch, \
     hosted_nodes.version, hosted_nodes.agent, hosted_nodes.capabilities, hosted_nodes.offers_verified, \
     hosted_nodes.visibility, hosted_nodes.last_seen, hosted_nodes.created_at, \
     namespaces.slug AS ns_slug, namespaces.name AS ns_name, \
     users.id AS owner_id, users.name AS owner_name, users.email AS owner_email, \
     NULL AS link_id, NULL AS link_label";

fn node_from() -> &'static str {
    "FROM hosted_nodes \
     LEFT JOIN namespaces ON namespaces.id = hosted_nodes.namespace_id \
     LEFT JOIN users ON users.id = namespaces.owner_id"
}

fn node_filter(kind: &str, region: &str, q: &str) -> (String, Vec<String>) {
    let mut sql = String::from(" WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();
    if !kind.trim().is_empty() {
        sql.push_str(" AND hosted_nodes.kind = ?");
        binds.push(kind.trim().to_string());
    }
    if !region.trim().is_empty() {
        sql.push_str(" AND hosted_nodes.region = ?");
        binds.push(region.trim().to_string());
    }
    if !q.trim().is_empty() {
        sql.push_str(
            " AND (hosted_nodes.name LIKE ? OR hosted_nodes.slug LIKE ? OR users.email LIKE ?)",
        );
        let like = format!("%{}%", q.trim());
        binds.push(like.clone());
        binds.push(like.clone());
        binds.push(like);
    }
    (sql, binds)
}

/// 全节点列表：管理台没有「我的 / 别人的」之分，私有与离线也照列。
pub async fn list_all_nodes(
    pool: &SqlitePool,
    kind: &str,
    region: &str,
    q: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<NodeRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 500 {
        100
    } else {
        limit
    };
    let (filter, binds) = node_filter(kind, region, q);
    let sql = format!(
        "SELECT {NODE_COLS} {} {filter} ORDER BY hosted_nodes.last_seen DESC LIMIT ? OFFSET ?",
        node_from()
    );
    let mut query = sqlx::query_as::<_, NodeRow>(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    query.bind(limit).bind(offset.max(0)).fetch_all(pool).await
}

/// 与 `list_all_nodes` 同口径的总数。
pub async fn count_all_nodes(
    pool: &SqlitePool,
    kind: &str,
    region: &str,
    q: &str,
) -> Result<i64, sqlx::Error> {
    let (filter, binds) = node_filter(kind, region, q);
    let sql = format!("SELECT COUNT(*) {} {filter}", node_from());
    let mut query = sqlx::query_scalar::<_, i64>(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    query.fetch_one(pool).await
}

/// 按 `@命名空间/slug` 找一个托管节点（管理台列表里两处都用了这个引用）。
pub async fn find_node_by_ns_slug(
    pool: &SqlitePool,
    ns_slug: &str,
    slug: &str,
) -> Result<Option<NodeRow>, sqlx::Error> {
    let sql = format!(
        "SELECT {NODE_COLS} {} WHERE namespaces.slug = ? AND hosted_nodes.slug = ? LIMIT 1",
        node_from()
    );
    sqlx::query_as::<_, NodeRow>(&sql)
        .bind(ns_slug.trim().trim_start_matches('@'))
        .bind(slug.trim())
        .fetch_optional(pool)
        .await
}

/// 摘除一个节点（不论归属），并清掉指向它的连接记录 ——
/// 否则别人的连接表里会留下一条指向空气的条目。
pub async fn delete_hosted_node_as_admin(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM hosted_nodes WHERE id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM node_links WHERE node_id = ?")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/* ---------------- 服务（两类来源） ---------------- */

/// 制品侧的列清单（与 `store/cluster.rs` 那份同源，改列时两处要一起改）。
const ART_COLS: &str = "a.id, a.namespace_id, a.slug, a.kind, a.name, a.version, a.summary, \
     a.tags, a.visibility, a.status, a.manifest, a.storage_provider, a.storage_url, a.blob_name, \
     a.sha256, a.size, a.origin, a.origin_ref, a.created_by, a.downloads, a.created_at, a.updated_at, \
     n.slug AS ns_slug, n.name AS ns_name";

/// 「服务」在制品侧的取值：`kind=api` 的条目就是对外可调用的服务接口。
pub const SERVICE_ARTIFACT_KINDS: &[&str] = &["api"];

fn service_kinds(kind: &str) -> Vec<String> {
    if kind.trim().is_empty() {
        SERVICE_ARTIFACT_KINDS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![kind.trim().to_string()]
    }
}

fn service_where(kind: &str, q: &str) -> (String, Vec<String>) {
    let kinds = service_kinds(kind);
    let placeholders = vec!["?"; kinds.len()].join(",");
    let mut sql = format!(" WHERE a.kind IN ({placeholders})");
    let mut binds: Vec<String> = kinds;
    if !q.trim().is_empty() {
        sql.push_str(" AND (a.name LIKE ? OR a.slug LIKE ? OR a.summary LIKE ?)");
        let like = format!("%{}%", q.trim());
        binds.push(like.clone());
        binds.push(like.clone());
        binds.push(like);
    }
    (sql, binds)
}

/// 制品侧的服务：`kind=api` 的条目。
pub async fn list_service_artifacts(
    pool: &SqlitePool,
    kind: &str,
    q: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<ArtifactRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 200 { 50 } else { limit };
    let (filter, binds) = service_where(kind, q);
    let sql = format!(
        "SELECT {ART_COLS} FROM artifacts a LEFT JOIN namespaces n ON n.id = a.namespace_id \
         {filter} ORDER BY a.updated_at DESC LIMIT ? OFFSET ?"
    );
    let mut query = sqlx::query_as::<_, ArtifactRow>(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    query.bind(limit).bind(offset.max(0)).fetch_all(pool).await
}

/// 与 `list_service_artifacts` 同口径的总数（计数不需要 join 命名空间）。
pub async fn count_service_artifacts(
    pool: &SqlitePool,
    kind: &str,
    q: &str,
) -> Result<i64, sqlx::Error> {
    let kinds = service_kinds(kind);
    let placeholders = vec!["?"; kinds.len()].join(",");
    let mut sql = format!("SELECT COUNT(*) FROM artifacts a WHERE a.kind IN ({placeholders})");
    let mut binds = kinds;
    if !q.trim().is_empty() {
        sql.push_str(" AND (a.name LIKE ? OR a.slug LIKE ? OR a.summary LIKE ?)");
        let like = format!("%{}%", q.trim());
        binds.push(like.clone());
        binds.push(like.clone());
        binds.push(like);
    }
    let mut query = sqlx::query_scalar::<_, i64>(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    query.fetch_one(pool).await
}

/// 下架（归档）而不删字节：管理员处理「有问题的服务」时，先让它从目录里消失，
/// 字节是否清理交给条目所属者决定。
pub async fn archive_artifact(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE artifacts SET status = 'archived' WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 概览用：各类型节点数（service / agent / assigned）。
pub async fn node_kind_counts(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT kind, COUNT(*) FROM hosted_nodes GROUP BY kind")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// 概览用：已发布公开的制品数（Go 的 `CountArtifacts`）。
pub async fn count_published_public_artifacts(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM artifacts WHERE status = 'published' AND visibility = 'public'",
    )
    .fetch_one(pool)
    .await
}

/* ---------------- 审计 ---------------- */

/// 一条管理动作审计。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AuditLog {
    pub id: String,
    pub actor_kind: String,
    pub actor_id: String,
    pub actor_name: String,
    pub action: String,
    pub target: String,
    pub target_name: String,
    pub summary: String,
    pub detail: String,
    pub ip: String,
    pub created_at: Option<String>,
}

/// 待落库的审计内容（id 与 created_at 由本层生成）。
#[derive(Debug, Clone, Default)]
pub struct NewAudit {
    pub actor_kind: String,
    pub actor_id: String,
    pub actor_name: String,
    pub action: String,
    pub target: String,
    pub target_name: String,
    pub summary: String,
    pub detail: String,
    pub ip: String,
}

pub async fn append_audit(pool: &SqlitePool, a: &NewAudit) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_logs (id, actor_kind, actor_id, actor_name, action, target, target_name, summary, detail, ip, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id("L"))
    .bind(a.actor_kind.trim())
    .bind(a.actor_id.trim())
    .bind(a.actor_name.trim())
    .bind(a.action.trim())
    .bind(&a.target)
    .bind(&a.target_name)
    .bind(&a.summary)
    .bind(if a.detail.trim().is_empty() { "{}" } else { a.detail.as_str() })
    .bind(a.ip.trim())
    .bind(now_go())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_audit(
    pool: &SqlitePool,
    action: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AuditLog>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 500 {
        100
    } else {
        limit
    };
    const COLS: &str = "id, actor_kind, actor_id, actor_name, action, target, target_name, summary, detail, ip, created_at";
    if action.trim().is_empty() {
        let sql =
            format!("SELECT {COLS} FROM audit_logs ORDER BY created_at DESC LIMIT ? OFFSET ?");
        sqlx::query_as::<_, AuditLog>(&sql)
            .bind(limit)
            .bind(offset.max(0))
            .fetch_all(pool)
            .await
    } else {
        let sql = format!(
            "SELECT {COLS} FROM audit_logs WHERE action = ? ORDER BY created_at DESC LIMIT ? OFFSET ?"
        );
        sqlx::query_as::<_, AuditLog>(&sql)
            .bind(action.trim())
            .bind(limit)
            .bind(offset.max(0))
            .fetch_all(pool)
            .await
    }
}

pub async fn count_audit(pool: &SqlitePool, action: &str) -> Result<i64, sqlx::Error> {
    if action.trim().is_empty() {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs")
            .fetch_one(pool)
            .await
    } else {
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = ?")
            .bind(action.trim())
            .fetch_one(pool)
            .await
    }
}

/* ---------------- admin key / secret ---------------- */

/// 节点级机器管理凭据。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AdminKey {
    pub id: String,
    pub label: String,
    pub key: String,
    pub secret_hash: String,
    pub created_by: String,
    pub created_at: Option<String>,
    pub last_used_at: Option<String>,
    pub revoked_at: Option<String>,
}

impl AdminKey {
    /// 是否可用（未被轮换 / 撤销）。
    pub fn active(&self) -> bool {
        self.revoked_at.is_none()
    }
}

/// 生成短 admin key（可念、可抄）。
pub fn new_admin_key() -> String {
    format!("AK-{}", rand_hex(3).to_uppercase())
}

/// 生成 admin secret（只显示一次）。
pub fn new_admin_secret() -> String {
    rand_hex(20)
}

/// 签发一份机器管理凭据（返回明文 secret，仅此一次）。
pub async fn create_admin_key(
    pool: &SqlitePool,
    label: &str,
    created_by: &str,
) -> Result<(AdminKey, String), sqlx::Error> {
    let key = new_admin_key();
    let secret = new_admin_secret();
    let k = AdminKey {
        id: new_id("AKX"),
        label: label.trim().to_string(),
        key,
        secret_hash: hash_secret(&secret),
        created_by: created_by.trim().to_string(),
        created_at: Some(now_go()),
        last_used_at: None,
        revoked_at: None,
    };
    sqlx::query(
        "INSERT INTO admin_keys (id, label, key, secret_hash, created_by, created_at, last_used_at, revoked_at)
         VALUES (?, ?, ?, ?, ?, ?, NULL, NULL)",
    )
    .bind(&k.id)
    .bind(&k.label)
    .bind(&k.key)
    .bind(&k.secret_hash)
    .bind(&k.created_by)
    .bind(&k.created_at)
    .execute(pool)
    .await?;
    Ok((k, secret))
}

pub async fn find_admin_key(pool: &SqlitePool, key: &str) -> Result<Option<AdminKey>, sqlx::Error> {
    sqlx::query_as::<_, AdminKey>(
        "SELECT id, label, key, secret_hash, created_by, created_at, last_used_at, revoked_at \
         FROM admin_keys WHERE key = ?",
    )
    .bind(key.trim())
    .fetch_optional(pool)
    .await
}

/// 全部凭据（含已撤销，便于说明「为什么那份 secret 不能用了」）。
pub async fn list_admin_keys(pool: &SqlitePool) -> Result<Vec<AdminKey>, sqlx::Error> {
    sqlx::query_as::<_, AdminKey>(
        "SELECT id, label, key, secret_hash, created_by, created_at, last_used_at, revoked_at \
         FROM admin_keys ORDER BY created_at DESC",
    )
    .fetch_all(pool)
    .await
}

/// 本节点是否已有可用的机器管理凭据（`/api/meta` 的 `auth.adminKey` 用它）。
pub async fn has_active_admin_key(pool: &SqlitePool) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM admin_keys WHERE revoked_at IS NULL")
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

pub async fn touch_admin_key(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE admin_keys SET last_used_at = ? WHERE id = ?")
        .bind(now_go())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 撤销除 `keep_id` 之外的全部机器凭据（轮换用；`keep_id` 为空即全撤）。
pub async fn revoke_admin_keys(pool: &SqlitePool, keep_id: &str) -> Result<u64, sqlx::Error> {
    let now = now_go();
    let res = if keep_id.trim().is_empty() {
        sqlx::query("UPDATE admin_keys SET revoked_at = ? WHERE revoked_at IS NULL")
            .bind(&now)
            .execute(pool)
            .await?
    } else {
        sqlx::query("UPDATE admin_keys SET revoked_at = ? WHERE revoked_at IS NULL AND id <> ?")
            .bind(&now)
            .bind(keep_id.trim())
            .execute(pool)
            .await?
    };
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{artifacts, namespaces, nodes, users};

    async fn pool(name: &str) -> (SqlitePool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ncc-admin-{}-{name}", std::process::id()));
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

    async fn seed_user(pool: &SqlitePool, name: &str, slug: &str) -> users::User {
        let u = users::create(pool, name, &format!("{slug}@x.com"), "h")
            .await
            .unwrap();
        namespaces::create_account(pool, &u.id, name, slug)
            .await
            .unwrap();
        u
    }

    async fn seed_node(pool: &SqlitePool, ns_id: &str, slug: &str, kind: &str) {
        nodes::upsert(
            pool,
            ns_id,
            &nodes::HeartbeatReq {
                slug: slug.to_string(),
                name: slug.to_string(),
                kind: kind.to_string(),
                region: "cn-east".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    async fn seed_artifact(
        pool: &SqlitePool,
        ns_id: &str,
        uid: &str,
        slug: &str,
        kind: &str,
        status: &str,
        vis: &str,
    ) -> artifacts::ArtifactRow {
        artifacts::create(
            pool,
            artifacts::NewArtifact {
                namespace_id: ns_id.to_string(),
                slug: slug.to_string(),
                kind: kind.to_string(),
                name: slug.to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                visibility: vis.to_string(),
                status: status.to_string(),
                manifest: String::new(),
                storage_provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: String::new(),
                sha256: String::new(),
                size: 0,
                created_by: uid.to_string(),
            },
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn 用户列表带名下规模_且可按_q_过滤() {
        let (p, _d) = pool("users").await;
        let u1 = seed_user(&p, "甲", "jia").await;
        let ns1 = namespaces::personal(&p, &u1.id).await.unwrap().unwrap();
        seed_artifact(&p, &ns1.id, &u1.id, "a", "skill", "published", "public").await;
        seed_node(&p, &ns1.id, "node-a", "service").await;
        let u2 = seed_user(&p, "乙", "yi").await;

        let rows = list_users(&p, "", 50, 0).await.unwrap();
        assert_eq!(rows.len(), 2);
        let jia = rows.iter().find(|r| r.id == u1.id).unwrap();
        assert_eq!(jia.nodes, 1);
        assert_eq!(jia.artifacts, 1);
        // 第一个注册用户自动成为管理员，后来者不是
        assert!(jia.is_admin);
        assert!(!rows.iter().find(|r| r.id == u2.id).unwrap().is_admin);
        assert_eq!(count_users_filtered(&p, "").await.unwrap(), 2);
        assert_eq!(count_users_filtered(&p, "yi@").await.unwrap(), 1);
        assert_eq!(count_users_filtered(&p, "不存在").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 启停用户_与管理员计数() {
        let (p, _d) = pool("disable").await;
        let u1 = seed_user(&p, "甲", "jia").await;
        let u2 = seed_user(&p, "乙", "yi").await;
        assert_eq!(count_admins(&p).await.unwrap(), 1); // 第一个用户是管理员

        // 禁用非管理员 + 备注
        set_user_disabled(&p, &u2.id, true, Some("违规"))
            .await
            .unwrap();
        let row = list_users(&p, "", 50, 0)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == u2.id)
            .unwrap();
        assert!(row.disabled);
        assert!(row.disabled_at.is_some());
        assert_eq!(row.admin_note, "违规");

        // 重新启用会清掉 disabled_at；备注留空时不动 admin_note
        set_user_disabled(&p, &u2.id, false, None).await.unwrap();
        let row = list_users(&p, "", 50, 0)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == u2.id)
            .unwrap();
        assert!(!row.disabled);
        assert!(row.disabled_at.is_none());
        assert_eq!(row.admin_note, "违规");

        // 禁用管理员 -> 计数归零
        set_user_disabled(&p, &u1.id, true, None).await.unwrap();
        assert_eq!(count_admins(&p).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 节点列表_过滤_与摘除清连接() {
        let (p, _d) = pool("nodes").await;
        let u1 = seed_user(&p, "甲", "jia").await;
        let ns1 = namespaces::personal(&p, &u1.id).await.unwrap().unwrap();
        seed_node(&p, &ns1.id, "svc", "service").await;
        seed_node(&p, &ns1.id, "ag", "agent").await;
        let u2 = seed_user(&p, "乙", "yi").await;
        let ns2 = namespaces::personal(&p, &u2.id).await.unwrap().unwrap();
        seed_node(&p, &ns2.id, "svc2", "service").await;

        // 全量：3 条（跨 owner，管理面不该有「我的/别人的」之分）
        let rows = list_all_nodes(&p, "", "", "", 100, 0).await.unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(count_all_nodes(&p, "", "", "").await.unwrap(), 3);
        // kind 过滤
        assert_eq!(
            list_all_nodes(&p, "service", "", "", 100, 0)
                .await
                .unwrap()
                .len(),
            2
        );
        // q 按 owner 邮箱过滤
        assert_eq!(
            list_all_nodes(&p, "", "", "yi@", 100, 0)
                .await
                .unwrap()
                .len(),
            1
        );
        // region 过滤
        assert_eq!(
            list_all_nodes(&p, "", "cn-east", "", 100, 0)
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            list_all_nodes(&p, "", "us-west", "", 100, 0)
                .await
                .unwrap()
                .len(),
            0
        );

        // 用 @ns/slug 形态找节点
        let hit = find_node_by_ns_slug(&p, "@jia", "svc").await.unwrap();
        assert!(hit.is_some());
        assert!(find_node_by_ns_slug(&p, "jia", "nope")
            .await
            .unwrap()
            .is_none());

        // 摘除：节点没了，别人指向它的连接也一起清掉
        let node_id = hit.unwrap().id;
        let owner_of_node = nodes::owner_of(&p, &node_id).await.unwrap().unwrap();
        nodes::link(&p, &u2.id, &node_id, &owner_of_node, "l", "")
            .await
            .unwrap();
        assert!(nodes::find_link(&p, &u2.id, &node_id)
            .await
            .unwrap()
            .is_some());
        delete_hosted_node_as_admin(&p, &node_id).await.unwrap();
        assert!(nodes::owner_of(&p, &node_id).await.unwrap().is_none());
        // 指向它的连接被一起清掉（否则别人的连接表里会留下一条指向空气的条目）
        assert!(nodes::find_link(&p, &u2.id, &node_id)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn 服务条目_默认只看_kind_api_可归档() {
        let (p, _d) = pool("services").await;
        let u = seed_user(&p, "甲", "jia").await;
        let ns = namespaces::personal(&p, &u.id).await.unwrap().unwrap();
        seed_artifact(&p, &ns.id, &u.id, "api1", "api", "published", "public").await;
        seed_artifact(&p, &ns.id, &u.id, "skill1", "skill", "published", "public").await;
        let a = seed_artifact(&p, &ns.id, &u.id, "api2", "api", "published", "public").await;

        let rows = list_service_artifacts(&p, "", "", 50, 0).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(count_service_artifacts(&p, "", "").await.unwrap(), 2);
        // 显式指定 kind 才换口径
        assert_eq!(
            list_service_artifacts(&p, "skill", "", 50, 0)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list_service_artifacts(&p, "", "api1", 50, 0)
                .await
                .unwrap()
                .len(),
            1
        );

        archive_artifact(&p, &a.id).await.unwrap();
        let row = artifacts::by_id(&p, &a.id).await.unwrap().unwrap();
        assert_eq!(row.status, "archived");
    }

    #[tokio::test]
    async fn 概览计数() {
        let (p, _d) = pool("overview").await;
        let u = seed_user(&p, "甲", "jia").await;
        let ns = namespaces::personal(&p, &u.id).await.unwrap().unwrap();
        seed_node(&p, &ns.id, "svc", "service").await;
        seed_node(&p, &ns.id, "ag", "agent").await;
        seed_artifact(&p, &ns.id, &u.id, "pub", "skill", "published", "public").await;
        seed_artifact(&p, &ns.id, &u.id, "draft", "skill", "draft", "public").await;

        let kinds = node_kind_counts(&p).await.unwrap();
        assert_eq!(kinds.get("service"), Some(&1));
        assert_eq!(kinds.get("agent"), Some(&1));
        // 只有 published+public 才计入
        assert_eq!(count_published_public_artifacts(&p).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn 审计_写入_列表_计数_与动作过滤() {
        let (p, _d) = pool("audit").await;
        let mk = |action: &str| NewAudit {
            actor_kind: "admin_key".to_string(),
            actor_id: "AK-1".to_string(),
            actor_name: "运维".to_string(),
            action: action.to_string(),
            target: "U-1".to_string(),
            target_name: "甲".to_string(),
            summary: "测试".to_string(),
            detail: String::new(), // 空 detail 落成 {}
            ip: "127.0.0.1".to_string(),
        };
        append_audit(&p, &mk("user.disable")).await.unwrap();
        append_audit(&p, &mk("user.enable")).await.unwrap();

        let all = list_audit(&p, "", 100, 0).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].detail, "{}");
        assert!(all[0].created_at.is_some());
        assert_eq!(count_audit(&p, "").await.unwrap(), 2);
        assert_eq!(count_audit(&p, "user.disable").await.unwrap(), 1);
        assert_eq!(
            list_audit(&p, "user.disable", 100, 0).await.unwrap().len(),
            1
        );
        assert_eq!(
            list_audit(&p, "user.enable", 100, 0).await.unwrap()[0].action,
            "user.enable"
        );
    }

    #[tokio::test]
    async fn admin_key_签发_校验_轮换() {
        let (p, _d) = pool("keys").await;
        let (k1, s1) = create_admin_key(&p, "首次", "U-1").await.unwrap();
        assert!(k1.key.starts_with("AK-"));
        assert_eq!(k1.secret_hash, hash_secret(&s1));
        assert!(k1.active());

        let found = find_admin_key(&p, &k1.key).await.unwrap().unwrap();
        assert_eq!(found.id, k1.id);
        assert_eq!(found.secret_hash, hash_secret(&s1));
        assert!(find_admin_key(&p, "AK-NOPE").await.unwrap().is_none());

        touch_admin_key(&p, &k1.id).await.unwrap();
        let touched = find_admin_key(&p, &k1.key).await.unwrap().unwrap();
        assert!(touched.last_used_at.is_some());

        // 轮换：签发新的，撤掉除它之外的全部
        let (k2, _s2) = create_admin_key(&p, "轮换", "U-1").await.unwrap();
        let revoked = revoke_admin_keys(&p, &k2.id).await.unwrap();
        assert_eq!(revoked, 1);
        let old = find_admin_key(&p, &k1.key).await.unwrap().unwrap();
        assert!(!old.active()); // 旧 secret 立即失效
        let new = find_admin_key(&p, &k2.key).await.unwrap().unwrap();
        assert!(new.active());
        // 列表含已撤销的
        assert_eq!(list_admin_keys(&p).await.unwrap().len(), 2);

        // 全撤
        assert_eq!(revoke_admin_keys(&p, "").await.unwrap(), 1);
        assert!(!find_admin_key(&p, &k2.key).await.unwrap().unwrap().active());
    }
}
