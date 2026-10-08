//! 托管节点（HostedNode）与连接（NodeLink）。
//!
//! 「注册」与「心跳」是同一个动作（`upsert`）：对调用方来说「我在这儿」和
//! 「我还在这儿」本就是一件事，分成两个端点只会让每个客户端都要自己判断该调哪个。
//!
//! 在线与否**不落库**，由心跳时间 + TTL 现场判断 —— 心跳是最高频的写入，
//! 为它多加一次写会把节点写死在磁盘 I/O 上。

use sqlx::SqlitePool;

use ncc_core::ids::{new_id, slugify};
use ncc_core::timeutil::{now_go, parse_time, parse_time_or_epoch};
use serde::Deserialize;

use crate::config::Config;

use super::{exists, marshal_list, parse_list};

/// 节点类型。
pub const NODE_SERVICE: &str = "service";
pub const NODE_AGENT: &str = "agent";
pub const NODE_ASSIGNED: &str = "assigned";

pub fn valid_kind(k: &str) -> bool {
    matches!(k, NODE_SERVICE | NODE_AGENT | NODE_ASSIGNED)
}

/// 心跳 / 注册请求体（与 CLI 的参数一一对应）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct HeartbeatReq {
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default, rename = "capabilitiesVerified")]
    pub capabilities_verified: Vec<String>,
    #[serde(default)]
    pub visibility: String,
    /// 归属命名空间；留空用调用者的个人命名空间。
    #[serde(default, rename = "namespaceId")]
    pub namespace_id: String,
}

/// 节点 + 命名空间 + 归属者 + 我的连接状态（一次 join，避免 N+1）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NodeRow {
    pub id: String,
    pub namespace_id: String,
    pub slug: String,
    pub name: String,
    pub kind: String,
    pub region: String,
    pub url: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub agent: String,
    pub capabilities: String,
    pub offers_verified: String,
    pub visibility: String,
    pub last_seen: Option<String>,
    pub created_at: Option<String>,
    pub ns_slug: Option<String>,
    pub ns_name: Option<String>,
    pub owner_id: Option<String>,
    pub owner_name: Option<String>,
    pub owner_email: Option<String>,
    pub link_id: Option<String>,
    pub link_label: Option<String>,
}

impl NodeRow {
    pub fn online(&self, ttl: std::time::Duration) -> bool {
        match self.last_seen.as_deref() {
            Some(s) => {
                let seen = parse_time_or_epoch(s);
                let now = chrono::Utc::now().fixed_offset();
                now.signed_duration_since(seen).num_seconds() <= ttl.as_secs() as i64
            }
            None => false,
        }
    }
    pub fn caps(&self) -> Vec<String> {
        parse_list(&self.capabilities)
    }
    pub fn verified(&self) -> Vec<String> {
        parse_list(&self.offers_verified)
    }
    pub fn seen_at(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        self.last_seen.as_deref().and_then(parse_time)
    }
}

const SELECT_COLS: &str = "hosted_nodes.id, hosted_nodes.namespace_id, hosted_nodes.slug, hosted_nodes.name, \
     hosted_nodes.kind, hosted_nodes.region, hosted_nodes.url, hosted_nodes.os, hosted_nodes.arch, \
     hosted_nodes.version, hosted_nodes.agent, hosted_nodes.capabilities, hosted_nodes.offers_verified, \
     hosted_nodes.visibility, hosted_nodes.last_seen, hosted_nodes.created_at, \
     namespaces.slug AS ns_slug, namespaces.name AS ns_name, \
     users.id AS owner_id, users.name AS owner_name, users.email AS owner_email, \
     node_links.id AS link_id, node_links.label AS link_label";

fn base_query() -> String {
    format!(
        "SELECT {SELECT_COLS} FROM hosted_nodes \
         LEFT JOIN namespaces ON namespaces.id = hosted_nodes.namespace_id \
         LEFT JOIN users ON users.id = namespaces.owner_id \
         LEFT JOIN node_links ON node_links.node_id = hosted_nodes.id AND node_links.owner_id = ?"
    )
}

fn bind_owner<'a>(
    mut q: sqlx::query::QueryAs<'a, sqlx::Sqlite, NodeRow, sqlx::sqlite::SqliteArguments<'a>>,
    owner: &'a str,
) -> sqlx::query::QueryAs<'a, sqlx::Sqlite, NodeRow, sqlx::sqlite::SqliteArguments<'a>> {
    q = q.bind(owner);
    q
}

/// 我命名空间下的节点（`owner_id` 用于带出「我和它的连接」）。
pub async fn list_of_namespaces(
    pool: &SqlitePool,
    owner_id: &str,
    ns_ids: &[String],
) -> Result<Vec<NodeRow>, sqlx::Error> {
    if ns_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ns_ids.len()].join(",");
    let sql = format!(
        "{} WHERE hosted_nodes.namespace_id IN ({placeholders}) ORDER BY hosted_nodes.name",
        base_query()
    );
    let mut q = bind_owner(sqlx::query_as::<_, NodeRow>(&sql), owner_id);
    for id in ns_ids {
        q = q.bind(id);
    }
    q.fetch_all(pool).await
}

/// 我连接表里的节点。
pub async fn list_linked(pool: &SqlitePool, owner_id: &str) -> Result<Vec<NodeRow>, sqlx::Error> {
    let sql = format!(
        "{} WHERE node_links.id IS NOT NULL AND node_links.id <> '' ORDER BY node_links.label, hosted_nodes.name",
        base_query()
    );
    bind_owner(sqlx::query_as::<_, NodeRow>(&sql), owner_id)
        .fetch_all(pool)
        .await
}

/// 可发现的本实例节点（排除我自己的）。
///
/// `granted_owners`：拿到过 node 授权的人 —— 他们的私有节点也应当对我可见。
pub async fn list_public(
    pool: &SqlitePool,
    owner_id: &str,
    granted_owners: &[String],
    kind: &str,
    region: &str,
    q: &str,
    limit: i64,
) -> Result<Vec<NodeRow>, sqlx::Error> {
    let mut sql = format!(
        "{} WHERE hosted_nodes.namespace_id NOT IN (SELECT id FROM namespaces WHERE owner_id = ?)",
        base_query()
    );
    let mut binds: Vec<String> = vec![owner_id.to_string()];
    if granted_owners.is_empty() {
        sql.push_str(" AND hosted_nodes.visibility = 'public'");
    } else {
        let placeholders = vec!["?"; granted_owners.len()].join(",");
        sql.push_str(&format!(
            " AND (hosted_nodes.visibility = 'public' OR namespaces.owner_id IN ({placeholders}))"
        ));
        binds.extend(granted_owners.iter().cloned());
    }
    if !kind.trim().is_empty() {
        sql.push_str(" AND hosted_nodes.kind = ?");
        binds.push(kind.trim().to_string());
    }
    if !region.trim().is_empty() {
        sql.push_str(" AND hosted_nodes.region = ?");
        binds.push(region.trim().to_string());
    }
    if !q.trim().is_empty() {
        sql.push_str(" AND (hosted_nodes.name LIKE ? OR hosted_nodes.slug LIKE ?)");
        binds.push(format!("%{}%", q.trim()));
        binds.push(format!("%{}%", q.trim()));
    }
    sql.push_str(" ORDER BY hosted_nodes.last_seen DESC LIMIT ?");

    let mut query = bind_owner(sqlx::query_as::<_, NodeRow>(&sql), owner_id);
    for b in &binds {
        query = query.bind(b);
    }
    query.bind(limit.clamp(1, 200)).fetch_all(pool).await
}

/// 注册与心跳合并：同 namespace+slug 则续租，否则新建。
pub async fn upsert(
    pool: &SqlitePool,
    ns_id: &str,
    in_req: &HeartbeatReq,
) -> Result<(NodeRow, bool), sqlx::Error> {
    let kind = if valid_kind(&in_req.kind) {
        in_req.kind.clone()
    } else {
        NODE_SERVICE.to_string()
    };
    let visibility = if in_req.visibility == "private" {
        "private"
    } else {
        "public"
    };
    let slug = {
        let s = slugify(&in_req.slug);
        if s == "x" || in_req.slug.trim().is_empty() {
            let from_name = slugify(&in_req.name);
            if from_name == "x" {
                format!("node-{}", ncc_core::crypto::rand_hex(3))
            } else {
                from_name
            }
        } else {
            s
        }
    };

    let existing: Option<String> = sqlx::query_scalar(
        "SELECT id FROM hosted_nodes WHERE namespace_id = ? AND slug = ? LIMIT 1",
    )
    .bind(ns_id)
    .bind(&slug)
    .fetch_optional(pool)
    .await?;

    let now = now_go();
    let created = existing.is_none();
    let id = existing.clone().unwrap_or_else(|| new_id("ND"));

    if created {
        sqlx::query(
            "INSERT INTO hosted_nodes (id, namespace_id, slug, name, kind, region, url, os, arch, version, agent, capabilities, offers_verified, visibility, last_seen, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(ns_id)
        .bind(&slug)
        .bind(in_req.name.trim())
        .bind(&kind)
        .bind(in_req.region.trim())
        .bind(in_req.url.trim())
        .bind(in_req.os.trim())
        .bind(in_req.arch.trim())
        .bind(in_req.version.trim())
        .bind(in_req.agent.trim())
        .bind(marshal_list(&in_req.capabilities))
        .bind(marshal_list(&in_req.capabilities_verified))
        .bind(visibility)
        .bind(&now)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            "UPDATE hosted_nodes SET name = ?, kind = ?, region = ?, url = ?, os = ?, arch = ?, version = ?, agent = ?, capabilities = ?, offers_verified = ?, visibility = ?, last_seen = ?, updated_at = ? WHERE id = ?",
        )
        .bind(in_req.name.trim())
        .bind(&kind)
        .bind(in_req.region.trim())
        .bind(in_req.url.trim())
        .bind(in_req.os.trim())
        .bind(in_req.arch.trim())
        .bind(in_req.version.trim())
        .bind(in_req.agent.trim())
        .bind(marshal_list(&in_req.capabilities))
        .bind(marshal_list(&in_req.capabilities_verified))
        .bind(visibility)
        .bind(&now)
        .bind(&now)
        .bind(&id)
        .execute(pool)
        .await?;
    }

    let row = by_id(pool, "", &id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)?;
    Ok((row, created))
}

pub async fn by_id(
    pool: &SqlitePool,
    owner_id: &str,
    id: &str,
) -> Result<Option<NodeRow>, sqlx::Error> {
    let sql = format!("{} WHERE hosted_nodes.id = ?", base_query());
    bind_owner(sqlx::query_as::<_, NodeRow>(&sql), owner_id)
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn delete(pool: &SqlitePool, id: &str, ns_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM hosted_nodes WHERE id = ? AND namespace_id = ?")
        .bind(id)
        .bind(ns_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 我命名空间下的全部节点 id（判定「这个节点是不是我的」用）。
pub async fn ids_of_namespaces(
    pool: &SqlitePool,
    ns_ids: &[String],
) -> Result<Vec<String>, sqlx::Error> {
    if ns_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = vec!["?"; ns_ids.len()].join(",");
    let sql = format!("SELECT id FROM hosted_nodes WHERE namespace_id IN ({placeholders})");
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for id in ns_ids {
        q = q.bind(id);
    }
    q.fetch_all(pool).await
}

pub async fn count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM hosted_nodes")
        .fetch_one(pool)
        .await
}

/* ---------------- 连接（NodeLink） ---------------- */

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NodeLink {
    pub id: String,
    pub owner_id: String,
    pub node_id: String,
    pub target_user_id: String,
    pub label: String,
    pub note: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// 建连接（同一对 owner/node 只留一条）。
pub async fn link(
    pool: &SqlitePool,
    owner_id: &str,
    node_id: &str,
    target_user_id: &str,
    label: &str,
    note: &str,
) -> Result<NodeLink, sqlx::Error> {
    let now = now_go();
    let id = new_id("L");
    sqlx::query(
        "INSERT INTO node_links (id, owner_id, node_id, target_user_id, label, note, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(owner_id, node_id) DO UPDATE SET label = excluded.label, note = excluded.note, updated_at = excluded.updated_at",
    )
    .bind(&id)
    .bind(owner_id)
    .bind(node_id)
    .bind(target_user_id)
    .bind(label)
    .bind(note)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    find_link(pool, owner_id, node_id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

pub async fn find_link(
    pool: &SqlitePool,
    owner_id: &str,
    node_id: &str,
) -> Result<Option<NodeLink>, sqlx::Error> {
    sqlx::query_as::<_, NodeLink>(
        "SELECT id, owner_id, node_id, target_user_id, label, note, created_at, updated_at FROM node_links WHERE owner_id = ? AND node_id = ?",
    )
    .bind(owner_id)
    .bind(node_id)
    .fetch_optional(pool)
    .await
}

/// 更新连接（只有主人能动）。
pub async fn patch_link(
    pool: &SqlitePool,
    id: &str,
    owner_id: &str,
    label: Option<&str>,
    note: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let mut sets: Vec<&str> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if let Some(v) = label {
        sets.push("label = ?");
        binds.push(v.to_string());
    }
    if let Some(v) = note {
        sets.push("note = ?");
        binds.push(v.to_string());
    }
    if sets.is_empty() {
        return Ok(false);
    }
    sets.push("updated_at = ?");
    let sql = format!(
        "UPDATE node_links SET {} WHERE id = ? AND owner_id = ?",
        sets.join(", ")
    );
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let res = q
        .bind(now_go())
        .bind(id)
        .bind(owner_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn delete_link(pool: &SqlitePool, id: &str, owner_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM node_links WHERE id = ? AND owner_id = ?")
        .bind(id)
        .bind(owner_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 节点归属者（建连接时要记下对方 owner）。
pub async fn owner_of(pool: &SqlitePool, node_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT namespaces.owner_id FROM hosted_nodes JOIN namespaces ON namespaces.id = hosted_nodes.namespace_id WHERE hosted_nodes.id = ?",
    )
    .bind(node_id)
    .fetch_optional(pool)
    .await
}

/// 我（或我的组织）名下的节点（`/api/namespaces/living`）。
pub async fn list_of_user(
    pool: &SqlitePool,
    user_id: &str,
    _ttl: std::time::Duration,
) -> Result<Vec<NodeRow>, sqlx::Error> {
    let ns_ids = crate::store::namespaces::of_user(pool, user_id)
        .await?
        .into_iter()
        .map(|n| n.id)
        .collect::<Vec<_>>();
    list_of_namespaces(pool, user_id, &ns_ids).await
}

/// 按 kind 统计在线/离线节点数（`/api/meta` 与 kinds 端点用）。
pub async fn kind_counts(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT kind, COUNT(*) FROM hosted_nodes GROUP BY kind")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// 出现过的区域列表。
pub async fn regions(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT region FROM hosted_nodes WHERE region <> '' ORDER BY region",
    )
    .fetch_all(pool)
    .await
}

/// 是否有这个节点（心跳里判断归属用）。
pub async fn exists_node(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    exists(
        pool,
        "SELECT COUNT(*) FROM hosted_nodes WHERE id = ?",
        &[id],
    )
    .await
}

/// 配置里的节点 TTL（判定在线）。
pub fn ttl_of(cfg: &Config) -> std::time::Duration {
    cfg.node_ttl
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

    #[tokio::test]
    async fn 注册与心跳合并() {
        let p = pool().await;
        let ns = crate::store::namespaces::create_account(&p, "U-1", "张三", "zs")
            .await
            .unwrap();
        let req = HeartbeatReq {
            name: "张三的 Mac".to_string(),
            slug: "my-mac".to_string(),
            capabilities: vec!["serve:mcp".to_string()],
            ..Default::default()
        };
        let (n1, created) = upsert(&p, &ns.id, &req).await.unwrap();
        assert!(created);
        assert_eq!(n1.slug, "my-mac");
        assert_eq!(n1.caps(), vec!["serve:mcp"]);
        assert!(n1.online(std::time::Duration::from_secs(60)));

        let (n2, created2) = upsert(&p, &ns.id, &req).await.unwrap();
        assert!(!created2);
        assert_eq!(n1.id, n2.id);
        assert_eq!(count(&p).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn 连接与解绑() {
        let p = pool().await;
        let ns1 = crate::store::namespaces::create_account(&p, "U-1", "甲", "jia")
            .await
            .unwrap();
        let ns2 = crate::store::namespaces::create_account(&p, "U-2", "乙", "yi")
            .await
            .unwrap();
        let (node, _) = upsert(
            &p,
            &ns2.id,
            &HeartbeatReq {
                name: "乙的机器".to_string(),
                slug: "yi-node".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(owner_of(&p, &node.id).await.unwrap().unwrap(), "U-2");
        let l1 = link(&p, "U-1", &node.id, "U-2", "同事", "").await.unwrap();
        assert_eq!(l1.label, "同事");
        // 重复连接是更新而不是新增
        let again = link(&p, "U-1", &node.id, "U-2", "老同事", "")
            .await
            .unwrap();
        assert_eq!(again.id, l1.id);
        assert_eq!(again.label, "老同事");
        assert_eq!(list_linked(&p, "U-1").await.unwrap().len(), 1);
        assert!(patch_link(&p, &l1.id, "U-1", Some("好友"), None)
            .await
            .unwrap());
        assert!(!patch_link(&p, &l1.id, "U-9", Some("越权"), None)
            .await
            .unwrap());
        delete_link(&p, &l1.id, "U-1").await.unwrap();
        assert!(list_linked(&p, "U-1").await.unwrap().is_empty());
        let _ = ns1;
    }
}
