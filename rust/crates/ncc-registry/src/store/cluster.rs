//! 集群（master / worker）的数据访问。
//!
//! 三张表各管一件事：
//!
//! * `cluster_workers` —— 向本 master 注册过的 worker（谁在线、提供什么）；
//! * `artifact_adverts` —— worker 上报的「我这儿有什么」，master 据此做目录聚合与能力路由；
//! * `replica_targets` —— master 侧「我把某份制品分发到了哪个 worker」的账。
//!
//! 为什么 `replica_targets` 不能由 adverts 反推：adverts 来自 worker 的**心跳**，天然滞后，
//! 下架回收不能等心跳 —— 分发成功那一刻就记账，回收以它为准（adverts 仅作兜底）。
//!
//! 副本（`origin='replica'`）落在自动建的镜像命名空间里，上游怎么变它就怎么变，
//! 所以可见性固定 public+published（与 Go 一致），本地没人「拥有」它。

use std::collections::HashMap;
use std::time::Duration;

use sqlx::SqlitePool;

use ncc_core::ids::{new_id, slugify};
use ncc_core::timeutil::{format_go, now_go, parse_time, parse_time_or_epoch};

use super::artifacts::ArtifactRow;
use super::namespaces::{self, Namespace};
use super::{marshal_list, parse_list};

/// 已发布的公开制品：worker 上报目录、master 聚合目录都以它为准。
///
/// 这里没有复用 `store::artifacts::SELECT_COLS`（那是私有常量），所以列清单是**抄**的一份。
/// 与 `store/admin.rs` 里那份同源，改列时三处要一起改。
const ART_COLS: &str = "a.id, a.namespace_id, a.slug, a.kind, a.name, a.version, a.summary, \
     a.tags, a.visibility, a.status, a.manifest, a.storage_provider, a.storage_url, a.blob_name, \
     a.sha256, a.size, a.origin, a.origin_ref, a.created_by, a.downloads, a.created_at, a.updated_at, \
     n.slug AS ns_slug, n.name AS ns_name";

fn trimmed_url(u: &str) -> String {
    u.trim_end_matches('/').to_string()
}

fn norm_time(v: Option<&str>) -> String {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => parse_time(s).map(format_go).unwrap_or_else(now_go),
        None => now_go(),
    }
}

/* ---------------- worker 注册表 ---------------- */

/// 注册 / 心跳的入参。
#[derive(Debug, Clone, Default)]
pub struct WorkerInput {
    pub id: String,
    pub name: String,
    pub url: String,
    pub version: String,
    pub region: String,
    pub capabilities: Vec<String>,
    pub artifacts: i64,
    pub nodes: i64,
    pub users: i64,
}

/// 一行 worker 记录。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClusterWorker {
    pub id: String,
    pub name: String,
    pub url: String,
    pub version: String,
    pub region: String,
    pub capabilities: String,
    pub artifacts: i64,
    pub nodes: i64,
    pub users: i64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
}

impl ClusterWorker {
    pub fn caps(&self) -> Vec<String> {
        parse_list(&self.capabilities)
    }

    pub fn online(&self, ttl: Duration) -> bool {
        match self.last_seen.as_deref() {
            Some(s) if !s.trim().is_empty() => {
                let seen = parse_time_or_epoch(s);
                chrono::Utc::now()
                    .fixed_offset()
                    .signed_duration_since(seen)
                    .num_seconds()
                    <= ttl.as_secs() as i64
            }
            _ => false,
        }
    }
}

/// 注册与心跳合并：同 id 就续租（`first_seen` 不动），否则新建。
pub async fn upsert_worker(pool: &SqlitePool, w: &WorkerInput) -> Result<(), sqlx::Error> {
    if w.id.trim().is_empty() {
        return Ok(()); // 调用方已判必填，这里只做兜底
    }
    let now = now_go();
    sqlx::query(
        "INSERT INTO cluster_workers (id, name, url, version, region, capabilities, artifacts, nodes, users, first_seen, last_seen)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name, url = excluded.url, version = excluded.version,
            region = excluded.region, capabilities = excluded.capabilities,
            artifacts = excluded.artifacts, nodes = excluded.nodes, users = excluded.users,
            last_seen = excluded.last_seen",
    )
    .bind(w.id.trim())
    .bind(w.name.trim())
    .bind(trimmed_url(&w.url))
    .bind(w.version.trim())
    .bind(w.region.trim())
    .bind(marshal_list(&w.capabilities))
    .bind(w.artifacts)
    .bind(w.nodes)
    .bind(w.users)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_workers(pool: &SqlitePool) -> Result<Vec<ClusterWorker>, sqlx::Error> {
    sqlx::query_as::<_, ClusterWorker>(
        "SELECT id, name, url, version, region, capabilities, artifacts, nodes, users, first_seen, last_seen
         FROM cluster_workers ORDER BY name",
    )
    .fetch_all(pool)
    .await
}

/// 清掉长期没心跳的 worker，连带它的目录（否则目录里会挂着幽灵节点）。
pub async fn prune_workers(pool: &SqlitePool, older_than: Duration) -> Result<u64, sqlx::Error> {
    let cutoff = format_go(
        chrono::Local::now().fixed_offset()
            - chrono::Duration::from_std(older_than).unwrap_or_default(),
    );
    sqlx::query("DELETE FROM artifact_adverts WHERE worker_id IN (SELECT id FROM cluster_workers WHERE last_seen < ?)")
        .bind(&cutoff)
        .execute(pool)
        .await?;
    let res = sqlx::query("DELETE FROM cluster_workers WHERE last_seen < ?")
        .bind(&cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/* ---------------- 目录上报（adverts） ---------------- */

/// worker 上报的一条目录。
#[derive(Debug, Clone, Default)]
pub struct AdvertInput {
    pub ref_: String,
    pub namespace_slug: String,
    pub slug: String,
    pub kind: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub sha256: String,
    pub size: i64,
    pub downloads: i64,
    pub updated_at: Option<String>,
}

/// 目录行 + 来源 worker。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AdvertRow {
    pub id: String,
    pub worker_id: String,
    #[sqlx(rename = "ref")]
    pub ref_: String,
    pub namespace_slug: String,
    pub slug: String,
    pub kind: String,
    pub name: String,
    pub version: String,
    pub summary: String,
    pub tags: String,
    pub sha256: String,
    pub size: i64,
    pub downloads: i64,
    pub updated_at: Option<String>,
    pub seen_at: Option<String>,
    pub worker_name: Option<String>,
    pub worker_url: Option<String>,
}

impl AdvertRow {
    /// 上报这条目录的 worker 是否还在线（心跳时间 + TTL 现场算，与 `ClusterWorker::online` 同口径）。
    pub fn online(&self, ttl: Duration) -> bool {
        match self.seen_at.as_deref() {
            Some(s) if !s.trim().is_empty() => {
                let seen = parse_time_or_epoch(s);
                chrono::Utc::now()
                    .fixed_offset()
                    .signed_duration_since(seen)
                    .num_seconds()
                    <= ttl.as_secs() as i64
            }
            _ => false,
        }
    }
}

const ADVERT_COLS: &str = "artifact_adverts.id, artifact_adverts.worker_id, artifact_adverts.ref, \
     artifact_adverts.namespace_slug, artifact_adverts.slug, artifact_adverts.kind, artifact_adverts.name, \
     artifact_adverts.version, artifact_adverts.summary, artifact_adverts.tags, artifact_adverts.sha256, \
     artifact_adverts.size, artifact_adverts.downloads, artifact_adverts.updated_at, artifact_adverts.seen_at, \
     cluster_workers.name AS worker_name, cluster_workers.url AS worker_url";

/// 用本次上报的清单整体替换该 worker 的目录（发布 / 删除都能收敛）。
///
/// `ref` 去重（表上有 `(worker_id, ref)` 唯一索引）：同一批里重复上报同一条会撞唯一键，
/// 直接跳过更省事，也让调用方不必先自己清洗。
pub async fn replace_adverts(
    pool: &SqlitePool,
    worker_id: &str,
    rows: &[AdvertInput],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM artifact_adverts WHERE worker_id = ?")
        .bind(worker_id)
        .execute(&mut *tx)
        .await?;
    let now = now_go();
    let mut seen: Vec<&str> = Vec::new();
    for r in rows {
        if r.ref_.trim().is_empty() || seen.contains(&r.ref_.as_str()) {
            continue;
        }
        seen.push(r.ref_.as_str());
        sqlx::query(
            "INSERT INTO artifact_adverts (id, worker_id, ref, namespace_slug, slug, kind, name, version, summary, tags, sha256, size, downloads, updated_at, seen_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(new_id("AD"))
        .bind(worker_id)
        .bind(r.ref_.trim())
        .bind(r.namespace_slug.trim())
        .bind(r.slug.trim())
        .bind(r.kind.trim())
        .bind(r.name.trim())
        .bind(r.version.trim())
        .bind(r.summary.trim())
        .bind(marshal_list(&r.tags))
        .bind(r.sha256.trim())
        .bind(r.size)
        .bind(r.downloads)
        .bind(norm_time(r.updated_at.as_deref()))
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// 集群聚合目录（含来源 worker）。
pub async fn list_adverts(
    pool: &SqlitePool,
    q: &str,
    kind: &str,
    tag: &str,
    limit: i64,
) -> Result<Vec<AdvertRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 500 {
        200
    } else {
        limit
    };
    let mut sql = format!(
        "SELECT {ADVERT_COLS} FROM artifact_adverts \
         LEFT JOIN cluster_workers ON cluster_workers.id = artifact_adverts.worker_id WHERE 1=1"
    );
    let mut binds: Vec<String> = Vec::new();
    if !kind.trim().is_empty() {
        sql.push_str(" AND artifact_adverts.kind = ?");
        binds.push(kind.trim().to_string());
    }
    if !tag.trim().is_empty() {
        sql.push_str(" AND artifact_adverts.tags LIKE ?");
        binds.push(format!("%\"{}\"%", tag.trim()));
    }
    if !q.trim().is_empty() {
        sql.push_str(" AND (artifact_adverts.name LIKE ? OR artifact_adverts.slug LIKE ? OR artifact_adverts.summary LIKE ?)");
        let like = format!("%{}%", q.trim());
        binds.push(like.clone());
        binds.push(like.clone());
        binds.push(like);
    }
    sql.push_str(" ORDER BY artifact_adverts.updated_at DESC LIMIT ?");
    let mut query = sqlx::query_as::<_, AdvertRow>(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    query.bind(limit).fetch_all(pool).await
}

/// 按 `@ns/slug` 或 `@ns/slug@version` 找提供者（能力路由）。
pub async fn find_adverts_by_ref(
    pool: &SqlitePool,
    ref_: &str,
) -> Result<Vec<AdvertRow>, sqlx::Error> {
    let ref_ = ref_.trim();
    // 去掉版本后缀：`@ns/slug@1.0` 也要能命中 `@ns/slug@*` 的所有提供者。
    let slug = match ref_.rfind('@') {
        Some(i) if i > 0 => &ref_[..i],
        _ => ref_,
    };
    let sql = format!(
        "SELECT {ADVERT_COLS} FROM artifact_adverts \
         LEFT JOIN cluster_workers ON cluster_workers.id = artifact_adverts.worker_id \
         WHERE artifact_adverts.ref = ? OR artifact_adverts.ref LIKE ?"
    );
    sqlx::query_as::<_, AdvertRow>(&sql)
        .bind(ref_)
        .bind(format!("{slug}@%"))
        .fetch_all(pool)
        .await
}

/// 本节点持有的、可上报给 master 的目录：已发布的公开条目。
pub async fn list_advertisable_artifacts(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<ArtifactRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 5000 {
        2000
    } else {
        limit
    };
    let sql = format!(
        "SELECT {ART_COLS} FROM artifacts a LEFT JOIN namespaces n ON n.id = a.namespace_id \
         WHERE a.status = 'published' AND a.visibility = 'public' ORDER BY a.updated_at DESC LIMIT ?"
    );
    sqlx::query_as::<_, ArtifactRow>(&sql)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/* ---------------- 分发账（replica_targets） ---------------- */

/// master 侧「我把某份制品分发到了哪个 worker」。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReplicaTarget {
    pub id: String,
    #[sqlx(rename = "ref")]
    pub ref_: String,
    pub worker_id: String,
    pub worker_name: String,
    pub worker_url: String,
    pub sha256: String,
    pub size: i64,
    pub created_at: Option<String>,
}

pub async fn record_replica_target(
    pool: &SqlitePool,
    ref_: &str,
    worker_id: &str,
    name: &str,
    url: &str,
    sha: &str,
    size: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO replica_targets (id, ref, worker_id, worker_name, worker_url, sha256, size, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(ref, worker_id) DO UPDATE SET
            worker_name = excluded.worker_name, worker_url = excluded.worker_url,
            sha256 = excluded.sha256, size = excluded.size",
    )
    .bind(new_id("RT"))
    .bind(ref_.trim())
    .bind(worker_id.trim())
    .bind(name.trim())
    .bind(trimmed_url(url))
    .bind(sha.trim())
    .bind(size)
    .bind(now_go())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_replica_targets(
    pool: &SqlitePool,
    ref_: &str,
) -> Result<Vec<ReplicaTarget>, sqlx::Error> {
    sqlx::query_as::<_, ReplicaTarget>(
        "SELECT id, ref, worker_id, worker_name, worker_url, sha256, size, created_at \
         FROM replica_targets WHERE ref = ? ORDER BY created_at",
    )
    .bind(ref_.trim())
    .fetch_all(pool)
    .await
}

pub async fn delete_replica_targets(pool: &SqlitePool, ref_: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM replica_targets WHERE ref = ?")
        .bind(ref_.trim())
        .execute(pool)
        .await?;
    Ok(())
}

/// 一次拿全部分发记录（目录页给本地条目标注副本用，避免 N+1）。
pub async fn replica_targets_by_ref(
    pool: &SqlitePool,
) -> Result<HashMap<String, Vec<ReplicaTarget>>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ReplicaTarget>(
        "SELECT id, ref, worker_id, worker_name, worker_url, sha256, size, created_at \
         FROM replica_targets ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;
    let mut out: HashMap<String, Vec<ReplicaTarget>> = HashMap::new();
    for r in rows {
        out.entry(r.ref_.clone()).or_default().push(r);
    }
    Ok(out)
}

/* ---------------- worker 侧：落副本 / 回收副本 ---------------- */

/// 落一份副本的入参（来自 master 的 `/api/cluster/ingest`）。
pub struct NewReplica {
    pub namespace_slug: String,
    pub kind: String,
    pub name: String,
    pub slug: String,
    pub version: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub manifest: String,
    pub provider: String,
    pub storage_url: String,
    pub blob_name: String,
    pub sha256: String,
    pub size: i64,
    pub created_by: String,
    pub origin_ref: String,
}

/// 落副本：同 `namespace+slug` 整行盖写（幂等）。
pub async fn upsert_replica(
    pool: &SqlitePool,
    ns_id: &str,
    r: &NewReplica,
) -> Result<ArtifactRow, sqlx::Error> {
    let now = now_go();
    if let Some(existing) = super::artifacts::by_ns_slug(pool, &r.namespace_slug, &r.slug).await? {
        sqlx::query(
            "UPDATE artifacts SET kind = ?, name = ?, version = ?, summary = ?, tags = ?, manifest = ?, \
             storage_provider = ?, storage_url = ?, blob_name = ?, sha256 = ?, size = ?, \
             visibility = 'public', status = 'published', origin = 'replica', origin_ref = ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(r.kind.trim())
        .bind(r.name.trim())
        .bind(r.version.trim())
        .bind(r.summary.trim())
        .bind(marshal_list(&r.tags))
        .bind(&r.manifest)
        .bind(r.provider.trim())
        .bind(&r.storage_url)
        .bind(&r.blob_name)
        .bind(r.sha256.trim())
        .bind(r.size)
        .bind(r.origin_ref.trim())
        .bind(&now)
        .bind(&existing.id)
        .execute(pool)
        .await?;
        return super::artifacts::by_id(pool, &existing.id)
            .await?
            .ok_or(sqlx::Error::RowNotFound);
    }

    let id = new_id("A");
    sqlx::query(
        "INSERT INTO artifacts (id, namespace_id, slug, kind, name, version, summary, tags, visibility, status, \
         manifest, storage_provider, storage_url, blob_name, sha256, size, origin, origin_ref, created_by, downloads, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'public', 'published', ?, ?, ?, ?, ?, ?, 'replica', ?, ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(ns_id)
    .bind(r.slug.trim())
    .bind(r.kind.trim())
    .bind(r.name.trim())
    .bind(r.version.trim())
    .bind(r.summary.trim())
    .bind(marshal_list(&r.tags))
    .bind(&r.manifest)
    .bind(r.provider.trim())
    .bind(&r.storage_url)
    .bind(&r.blob_name)
    .bind(r.sha256.trim())
    .bind(r.size)
    .bind(r.origin_ref.trim())
    .bind(r.created_by.trim())
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    super::artifacts::by_id(pool, &id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

/// 拿 / 建镜像命名空间（副本落在这里，本地无人可改）。
pub async fn ensure_mirror_namespace(
    pool: &SqlitePool,
    slug: &str,
    name: &str,
) -> Result<Namespace, sqlx::Error> {
    let slug = slugify(slug);
    if let Some(ns) = namespaces::by_slug(pool, &slug).await? {
        return Ok(ns);
    }
    let ns = Namespace {
        id: new_id("NS"),
        slug,
        name: name.to_string(),
        ns_type: "mirror".to_string(),
        owner_id: "cluster".to_string(),
        visibility: "public".to_string(),
        created_at: Some(now_go()),
    };
    sqlx::query(
        "INSERT INTO namespaces (id, slug, name, type, owner_id, visibility, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&ns.id)
    .bind(&ns.slug)
    .bind(&ns.name)
    .bind(&ns.ns_type)
    .bind(&ns.owner_id)
    .bind(&ns.visibility)
    .bind(&ns.created_at)
    .execute(pool)
    .await?;
    Ok(ns)
}

/// 回收一份副本（只删副本，不碰本节点自己发布的条目）。返回 `(id, blob_name, removed)`。
pub async fn delete_replica(
    pool: &SqlitePool,
    ref_: &str,
) -> Result<(String, String, bool), sqlx::Error> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT id, blob_name FROM artifacts WHERE origin = 'replica' AND origin_ref = ? LIMIT 1",
    )
    .bind(ref_.trim())
    .fetch_optional(pool)
    .await?;
    let Some((id, blob)) = row else {
        return Ok((String::new(), String::new(), false));
    };
    sqlx::query("DELETE FROM artifacts WHERE id = ?")
        .bind(&id)
        .execute(pool)
        .await?;
    Ok((id, blob, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::artifacts::{self, NewArtifact};

    async fn pool(name: &str) -> (SqlitePool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ncc-cluster-{}-{name}", std::process::id()));
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

    fn worker(id: &str, name: &str) -> WorkerInput {
        WorkerInput {
            id: id.to_string(),
            name: name.to_string(),
            url: format!("http://{name}:9000/"),
            version: "ncc-registry/0.1.1".to_string(),
            region: "cn-east".to_string(),
            capabilities: vec!["registry".to_string()],
            artifacts: 1,
            nodes: 2,
            users: 3,
        }
    }

    #[tokio::test]
    async fn worker_注册_幂等_且_url_去尾斜杠() {
        let (p, _d) = pool("worker").await;
        upsert_worker(&p, &worker("W1", "office")).await.unwrap();
        // 第二次心跳：改名但不新建行
        let mut again = worker("W1", "office2");
        again.nodes = 9;
        upsert_worker(&p, &again).await.unwrap();

        let ws = list_workers(&p).await.unwrap();
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].name, "office2");
        assert_eq!(ws[0].url, "http://office2:9000"); // 尾斜杠被去掉
        assert_eq!(ws[0].nodes, 9);
        assert!(ws[0].online(Duration::from_secs(60)));
        assert!(ws[0].first_seen.is_some());
    }

    #[tokio::test]
    async fn adverts_整体替换_去重_且按_ref_查得到() {
        let (p, _d) = pool("adverts").await;
        upsert_worker(&p, &worker("W1", "office")).await.unwrap();
        let a = |ref_: &str, ver: &str| AdvertInput {
            ref_: ref_.to_string(),
            namespace_slug: "team".to_string(),
            slug: "demo".to_string(),
            kind: "skill".to_string(),
            name: "演示".to_string(),
            version: ver.to_string(),
            summary: String::new(),
            tags: vec!["x".to_string()],
            sha256: "s".to_string(),
            size: 1,
            downloads: 0,
            updated_at: Some("2026-09-07 01:16:25+08:00".to_string()),
        };
        // 同一批里有重复 ref，只落一条
        replace_adverts(
            &p,
            "W1",
            &[
                a("@team/demo@1.0.0", "1.0.0"),
                a("@team/demo@1.0.0", "1.0.0"),
            ],
        )
        .await
        .unwrap();
        let rows = list_adverts(&p, "", "", "", 100).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].worker_name.as_deref(), Some("office"));
        assert_eq!(rows[0].worker_url.as_deref(), Some("http://office:9000"));

        // 整体替换：换一份清单，旧的消失
        replace_adverts(&p, "W1", &[a("@team/demo@2.0.0", "2.0.0")])
            .await
            .unwrap();
        let rows = list_adverts(&p, "", "", "", 100).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].version, "2.0.0");

        // 按 ref 查：带版本与不带版本都能命中
        assert_eq!(
            find_adverts_by_ref(&p, "@team/demo@2.0.0")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            find_adverts_by_ref(&p, "@team/demo").await.unwrap().len(),
            1
        );
        assert_eq!(
            find_adverts_by_ref(&p, "@team/none").await.unwrap().len(),
            0
        );
        // 空 ref 一律跳过
        replace_adverts(&p, "W1", &[a("", "")]).await.unwrap();
        assert_eq!(list_adverts(&p, "", "", "", 100).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn adverts_过滤_与_tag_精确匹配() {
        let (p, _d) = pool("advfilter").await;
        upsert_worker(&p, &worker("W1", "office")).await.unwrap();
        let mk = |ref_: &str, slug: &str, tags: Vec<String>| AdvertInput {
            ref_: ref_.to_string(),
            namespace_slug: "team".to_string(),
            slug: slug.to_string(),
            kind: "mcp".to_string(),
            name: format!("包{slug}"),
            version: "1.0.0".to_string(),
            tags,
            ..Default::default()
        };
        replace_adverts(
            &p,
            "W1",
            &[
                mk("@team/alpha", "alpha", vec!["bar".to_string()]),
                mk("@team/beta", "beta", vec!["foobar".to_string()]),
            ],
        )
        .await
        .unwrap();
        // tag 精确匹配：bar 不该命中 foobar
        let hit = list_adverts(&p, "", "", "bar", 100).await.unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].slug, "alpha");
        // kind 过滤
        assert_eq!(
            list_adverts(&p, "", "skill", "", 100).await.unwrap().len(),
            0
        );
        assert_eq!(list_adverts(&p, "", "mcp", "", 100).await.unwrap().len(), 2);
        // q 过滤（名字/摘要）
        assert_eq!(
            list_adverts(&p, "包beta", "", "", 100).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn prune_清掉失联_worker_及其目录() {
        let (p, _d) = pool("prune").await;
        upsert_worker(&p, &worker("W1", "office")).await.unwrap();
        replace_adverts(
            &p,
            "W1",
            &[AdvertInput {
                ref_: "@team/demo@1.0.0".to_string(),
                kind: "skill".to_string(),
                ..Default::default()
            }],
        )
        .await
        .unwrap();
        // 把 last_seen 人为推到很久以前
        sqlx::query("UPDATE cluster_workers SET last_seen = ? WHERE id = 'W1'")
            .bind("2000-01-01 00:00:00+08:00")
            .execute(&p)
            .await
            .unwrap();
        let n = prune_workers(&p, Duration::from_secs(60)).await.unwrap();
        assert_eq!(n, 1);
        assert!(list_workers(&p).await.unwrap().is_empty());
        assert!(list_adverts(&p, "", "", "", 100).await.unwrap().is_empty());
        // 再清一次：没有可清的了
        assert_eq!(prune_workers(&p, Duration::from_secs(60)).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 副本落库_盖写_与回收() {
        let (p, _d) = pool("replica").await;
        let ns = ensure_mirror_namespace(&p, "Team", "Team").await.unwrap();
        assert_eq!(ns.ns_type, "mirror");
        assert_eq!(ns.owner_id, "cluster");
        // 第二次拿的是同一行（幂等）
        let ns2 = ensure_mirror_namespace(&p, "Team", "Team").await.unwrap();
        assert_eq!(ns.id, ns2.id);

        let r = NewReplica {
            namespace_slug: "team".to_string(),
            kind: "skill".to_string(),
            name: "演示".to_string(),
            slug: "demo".to_string(),
            version: "1.0.0".to_string(),
            summary: "一".to_string(),
            tags: vec!["t".to_string()],
            manifest: "{}".to_string(),
            provider: "local".to_string(),
            storage_url: "/blobs/x".to_string(),
            blob_name: "x".to_string(),
            sha256: "aaa".to_string(),
            size: 3,
            created_by: "cluster".to_string(),
            origin_ref: "@team/demo@1.0.0".to_string(),
        };
        let a1 = upsert_replica(&p, &ns.id, &r).await.unwrap();
        assert_eq!(a1.origin, "replica");
        assert_eq!(a1.status, "published");
        assert_eq!(a1.visibility, "public");

        // 盖写：同 ns+slug 换版本/摘要，不新建行
        let mut r2 = r;
        r2.version = "2.0.0".to_string();
        r2.summary = "二".to_string();
        let a2 = upsert_replica(&p, &ns.id, &r2).await.unwrap();
        assert_eq!(a2.id, a1.id);
        assert_eq!(a2.version, "2.0.0");
        assert_eq!(a2.summary, "二");

        // 回收：命中 origin_ref
        let (id, blob, removed) = delete_replica(&p, "@team/demo@1.0.0").await.unwrap();
        assert!(removed);
        assert_eq!(id, a1.id);
        assert_eq!(blob, "x");
        // 不存在的 ref：什么都没删
        let (_, _, removed) = delete_replica(&p, "@team/none@1.0.0").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn 回收不碰本节点自己的条目() {
        let (p, _d) = pool("replica-local").await;
        let u = super::super::users::create(&p, "甲", "a@x.com", "h")
            .await
            .unwrap();
        let ns = namespaces::create_account(&p, &u.id, "甲", "jia")
            .await
            .unwrap();
        let local = artifacts::create(
            &p,
            NewArtifact {
                namespace_id: ns.id.clone(),
                slug: "mine".to_string(),
                kind: "skill".to_string(),
                name: "我的".to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                visibility: "public".to_string(),
                status: "published".to_string(),
                manifest: String::new(),
                storage_provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: String::new(),
                sha256: String::new(),
                size: 0,
                created_by: u.id.clone(),
            },
        )
        .await
        .unwrap();
        // 本地条目的 origin_ref 是空串，回收任意 ref 都不该动它
        let (_, _, removed) = delete_replica(&p, "").await.unwrap();
        assert!(!removed);
        assert!(artifacts::by_id(&p, &local.id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn 上报目录只含已发布公开条目() {
        let (p, _d) = pool("advertisable").await;
        let u = super::super::users::create(&p, "乙", "b@x.com", "h")
            .await
            .unwrap();
        let ns = namespaces::create_account(&p, &u.id, "乙", "yi")
            .await
            .unwrap();
        let mk = |slug: &str, vis: &str, st: &str| NewArtifact {
            namespace_id: ns.id.clone(),
            slug: slug.to_string(),
            kind: "skill".to_string(),
            name: slug.to_string(),
            version: "1.0.0".to_string(),
            summary: String::new(),
            tags: vec![],
            visibility: vis.to_string(),
            status: st.to_string(),
            manifest: String::new(),
            storage_provider: "local".to_string(),
            storage_url: String::new(),
            blob_name: String::new(),
            sha256: String::new(),
            size: 0,
            created_by: u.id.clone(),
        };
        artifacts::create(&p, mk("pub", "public", "published"))
            .await
            .unwrap();
        artifacts::create(&p, mk("draft", "public", "draft"))
            .await
            .unwrap();
        artifacts::create(&p, mk("priv", "private", "published"))
            .await
            .unwrap();
        let rows = list_advertisable_artifacts(&p, 2000).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slug, "pub");
    }

    #[tokio::test]
    async fn 分发记账_幂等_可批量取() {
        let (p, _d) = pool("target").await;
        record_replica_target(
            &p,
            "@team/demo@1.0.0",
            "W1",
            "office",
            "http://office:9000/",
            "s1",
            10,
        )
        .await
        .unwrap();
        // 同一 (ref, worker)：盖写而不是新增
        record_replica_target(
            &p,
            "@team/demo@1.0.0",
            "W1",
            "office",
            "http://office:9000",
            "s2",
            20,
        )
        .await
        .unwrap();
        let rows = list_replica_targets(&p, "@team/demo@1.0.0").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sha256, "s2");
        assert_eq!(rows[0].size, 20);
        assert_eq!(rows[0].worker_url, "http://office:9000");

        let all = replica_targets_by_ref(&p).await.unwrap();
        assert_eq!(all.get("@team/demo@1.0.0").map(|v| v.len()), Some(1));

        delete_replica_targets(&p, "@team/demo@1.0.0")
            .await
            .unwrap();
        assert!(list_replica_targets(&p, "@team/demo@1.0.0")
            .await
            .unwrap()
            .is_empty());
    }
}
