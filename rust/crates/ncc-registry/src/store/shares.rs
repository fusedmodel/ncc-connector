//! 制品分享链接的数据访问（原实现 `ncc-registry/store/share.go`）。
//!
//! 这一族做什么：把一条**已存在的制品**变成一段临时下载地址 ——
//! `<publicURL>/s/<token>` 给人看落地页，`/s/<token>/raw` 给 curl / Agent 取字节。
//!
//! 刻意的取舍：
//!
//! * **token 只存 sha256**（`store::hash_secret`）：库里泄漏了也换不回可用链接，
//!   代价是列表接口回不出可点的链接（只能给 `hint` 前缀）；反查一律现算哈希。
//! * **可用性在 Rust 侧判**（撤销 / 过期 / 用尽是三个独立条件）：时间列按既有库的
//!   GORM 文本格式存取（见 `ncc_core::timeutil`），不给 sqlx 映射 chrono 类型 ——
//!   映射错了只会得到「读写都能跑、老数据一读就炸」这种最难查的错。
//! * 与 Grant 表无关：分享是临时放行，撤销它不会改变制品本身的可见性。

use sqlx::SqlitePool;

use ncc_core::crypto::rand_hex;
use ncc_core::ids::new_id;
use ncc_core::timeutil::{format_go, now_go, parse_time};

use super::hash_secret;

/// token 随机字节数：16 字节 → 32 位 hex（与 Go 的 `RandHex(16)` 逐字一致）。
const TOKEN_BYTES: usize = 16;
/// 列表里「认人」用的 token 前缀长度（Go 取前 6 位）。
const HINT_LEN: usize = 6;
/// 一次最多列多少条（与 Go 的 `ListShares` 同值）。
const MAX_LIMIT: i64 = 200;
const DEFAULT_LIMIT: i64 = 50;

/// 生成分享 token（32 位 hex，放进 URL 路径，长到不可猜）。
pub fn new_token() -> String {
    rand_hex(TOKEN_BYTES)
}

/// 分享记录（`artifact_shares` 一行）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Share {
    pub id: String,
    pub artifact_id: String,
    pub namespace_id: String,
    pub token_hash: String,
    pub token_hint: String,
    pub label: String,
    pub created_by: String,
    pub max_uses: i64,
    pub used_count: i64,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl Share {
    /// 现在还能用吗（撤销 / 过期 / 用尽，缺一即失效）。
    pub fn usable(&self) -> bool {
        self.usable_at(chrono::Local::now().fixed_offset())
    }

    /// 指定时刻是否可用（「同一请求内多次判定」用，免得两次取时钟判断不一致）。
    pub fn usable_at(&self, now: chrono::DateTime<chrono::FixedOffset>) -> bool {
        if self.revoked_at.is_some() {
            return false;
        }
        if let Some(exp) = self.expires_at.as_deref().and_then(parse_time) {
            if now > exp {
                return false;
            }
        }
        if self.max_uses > 0 && self.used_count >= self.max_uses {
            return false;
        }
        true
    }

    /// 剩余次数：0 = 不限次（与 `max_uses` 的 0 同义，便于展示）。
    pub fn remaining_uses(&self) -> i64 {
        if self.max_uses <= 0 {
            return 0;
        }
        (self.max_uses - self.used_count).max(0)
    }
}

/// 分享 + 制品 + 命名空间 + 创建者（列表页一次 join 拿全）。
///
/// LEFT JOIN 出来的列在库里可能为 NULL（制品被删了），所以都是 `Option` ——
/// 不让一条残缺的历史数据把整个列表接口打成 500。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ShareRow {
    pub id: String,
    pub artifact_id: String,
    pub namespace_id: String,
    pub token_hash: String,
    pub token_hint: String,
    pub label: String,
    pub created_by: String,
    pub max_uses: i64,
    pub used_count: i64,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub artifact_slug: Option<String>,
    pub artifact_name: Option<String>,
    pub artifact_kind: Option<String>,
    pub artifact_sha: Option<String>,
    pub artifact_size: Option<i64>,
    pub artifact_status: Option<String>,
    pub ns_slug: Option<String>,
    pub ns_name: Option<String>,
    pub created_by_name: Option<String>,
}

impl ShareRow {
    /// 取出「分享本身」的那部分（列表与单条视图共用同一段 JSON 骨架）。
    pub fn as_share(&self) -> Share {
        Share {
            id: self.id.clone(),
            artifact_id: self.artifact_id.clone(),
            namespace_id: self.namespace_id.clone(),
            token_hash: self.token_hash.clone(),
            token_hint: self.token_hint.clone(),
            label: self.label.clone(),
            created_by: self.created_by.clone(),
            max_uses: self.max_uses,
            used_count: self.used_count,
            expires_at: self.expires_at.clone(),
            revoked_at: self.revoked_at.clone(),
            last_used_at: self.last_used_at.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }
}

/// 显式列出 `artifact_shares.*`：join 之后 `id` / `created_at` 在几张表里都有，
/// 不写清楚会互相覆盖（这正是 Go 侧注释里踩过的坑）。
const SHARE_COLS: &str =
    "artifact_shares.id, artifact_shares.artifact_id, artifact_shares.namespace_id, \
     artifact_shares.token_hash, artifact_shares.token_hint, artifact_shares.label, \
     artifact_shares.created_by, artifact_shares.max_uses, artifact_shares.used_count, \
     artifact_shares.expires_at, artifact_shares.revoked_at, artifact_shares.last_used_at, \
     artifact_shares.created_at, artifact_shares.updated_at";

const JOIN_COLS: &str = "artifacts.slug AS artifact_slug, artifacts.name AS artifact_name, \
     artifacts.kind AS artifact_kind, artifacts.sha256 AS artifact_sha, \
     artifacts.size AS artifact_size, artifacts.status AS artifact_status, \
     namespaces.slug AS ns_slug, namespaces.name AS ns_name, \
     users.name AS created_by_name";

fn row_query(created_by: &str) -> String {
    let mut sql = format!(
        "SELECT {SHARE_COLS}, {JOIN_COLS} FROM artifact_shares \
         LEFT JOIN artifacts ON artifacts.id = artifact_shares.artifact_id \
         LEFT JOIN namespaces ON namespaces.id = artifact_shares.namespace_id \
         LEFT JOIN users ON users.id = artifact_shares.created_by"
    );
    if !created_by.is_empty() {
        sql.push_str(" WHERE artifact_shares.created_by = ?");
    }
    sql
}

/// 建一条分享链接，返回记录与**明文 token**（明文只在这一刻存在）。
pub async fn create(
    pool: &SqlitePool,
    artifact_id: &str,
    ns_id: &str,
    created_by: &str,
    label: &str,
    max_uses: i64,
    expires_at: Option<String>,
) -> Result<(Share, String), sqlx::Error> {
    let token = new_token();
    let hint: String = token.chars().take(HINT_LEN).collect();
    let id = new_id("SH");
    let now = now_go();
    sqlx::query(
        "INSERT INTO artifact_shares (id, artifact_id, namespace_id, token_hash, token_hint, label, created_by, max_uses, used_count, expires_at, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(&id)
    .bind(artifact_id)
    .bind(ns_id)
    .bind(hash_secret(&token))
    .bind(&hint)
    .bind(label.trim())
    .bind(created_by)
    .bind(max_uses)
    .bind(&expires_at)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    let sh = by_id(pool, &id).await?.ok_or(sqlx::Error::RowNotFound)?;
    Ok((sh, token))
}

/// token 是明文，库里存哈希 —— 查的时候现算（这就是「token 不可反查」的实现）。
pub async fn by_token(pool: &SqlitePool, token: &str) -> Result<Option<Share>, sqlx::Error> {
    let hash = hash_secret(token.trim());
    sqlx::query_as::<_, Share>("SELECT * FROM artifact_shares WHERE token_hash = ?")
        .bind(hash)
        .fetch_optional(pool)
        .await
}

pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<Share>, sqlx::Error> {
    sqlx::query_as::<_, Share>("SELECT * FROM artifact_shares WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// 分享列表。`created_by` 为空 = 全部（管理员视角）。
pub async fn list(
    pool: &SqlitePool,
    created_by: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<ShareRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > MAX_LIMIT {
        DEFAULT_LIMIT
    } else {
        limit
    };
    let mut sql = row_query(created_by);
    sql.push_str(" ORDER BY artifact_shares.created_at DESC LIMIT ? OFFSET ?");
    let mut q = sqlx::query_as::<_, ShareRow>(&sql);
    if !created_by.is_empty() {
        q = q.bind(created_by);
    }
    q.bind(limit).bind(offset.max(0)).fetch_all(pool).await
}

pub async fn count(pool: &SqlitePool, created_by: &str) -> Result<i64, sqlx::Error> {
    if created_by.is_empty() {
        sqlx::query_scalar("SELECT COUNT(*) FROM artifact_shares")
            .fetch_one(pool)
            .await
    } else {
        sqlx::query_scalar("SELECT COUNT(*) FROM artifact_shares WHERE created_by = ?")
            .bind(created_by)
            .fetch_one(pool)
            .await
    }
}

/// 未撤销 / 未过期 / 次数未用尽的数量（管理台概览用）。
///
/// 与 Go 一样把时间比较交给 SQL：两端都写 GORM 文本格式，字典序与时间序一致。
pub async fn count_active(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    let now = now_go();
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM artifact_shares WHERE revoked_at IS NULL \
         AND (expires_at IS NULL OR expires_at > ?) \
         AND (max_uses = 0 OR used_count < max_uses)",
    )
    .bind(now)
    .fetch_one(pool)
    .await
}

/// 撤销。`created_by` 非空时限定为「只能撤自己发的」；管理员传空串撤任意。
///
/// 返回是否真的改到了行：调用方靠它区分「撤成功」与「这条本来就不存在」。
pub async fn revoke(pool: &SqlitePool, id: &str, created_by: &str) -> Result<bool, sqlx::Error> {
    let now = now_go();
    let res = if created_by.is_empty() {
        sqlx::query("UPDATE artifact_shares SET revoked_at = ?, updated_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&now)
            .bind(id)
            .execute(pool)
            .await?
    } else {
        sqlx::query(
            "UPDATE artifact_shares SET revoked_at = ?, updated_at = ? WHERE id = ? AND created_by = ?",
        )
        .bind(&now)
        .bind(&now)
        .bind(id)
        .bind(created_by)
        .execute(pool)
        .await?
    };
    Ok(res.rows_affected() > 0)
}

/// 记一次使用（用尽后自动失效，见 `Share::usable`）。
pub async fn mark_used(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    let now = now_go();
    sqlx::query(
        "UPDATE artifact_shares SET used_count = used_count + 1, last_used_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// 用「当前时间 + 天数」算过期时刻（等价 Go 的 `time.Now().AddDate(0,0,days)`）。
pub fn expires_in_days(days: i64) -> String {
    let t = chrono::Local::now().fixed_offset() + chrono::Duration::days(days);
    format_go(t)
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

    /// 建一个用户 + 他的命名空间 + 一条私有制品，返回 (用户 id, 命名空间 id, 制品 id)。
    async fn seed_artifact(p: &SqlitePool) -> (String, String, String) {
        let u = crate::store::users::create(p, "张三", "zhangsan@x.com", "h")
            .await
            .unwrap();
        let ns = crate::store::namespaces::create_account(p, &u.id, "张三", "zhangsan")
            .await
            .unwrap();
        let a = crate::store::artifacts::create(
            p,
            crate::store::artifacts::NewArtifact {
                namespace_id: ns.id.clone(),
                slug: "demo".to_string(),
                kind: "skill".to_string(),
                name: "演示".to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                visibility: "private".to_string(),
                status: "published".to_string(),
                manifest: String::new(),
                storage_provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: "b".to_string(),
                sha256: "sha".to_string(),
                size: 3,
                created_by: u.id.clone(),
            },
        )
        .await
        .unwrap();
        (u.id, ns.id, a.id)
    }

    #[tokio::test]
    async fn 建分享_明文只回一次_库里只有哈希() {
        let p = pool().await;
        let (uid, nsid, aid) = seed_artifact(&p).await;
        let (sh, token) = create(&p, &aid, &nsid, &uid, " 给小李 ", 3, None)
            .await
            .unwrap();
        assert_eq!(token.len(), 32);
        assert_eq!(sh.token_hint, token[..6]);
        assert_eq!(sh.label, "给小李"); // 入库前 trim
        assert_eq!(sh.max_uses, 3);
        assert_eq!(sh.used_count, 0);
        assert!(sh.usable());
        assert_eq!(sh.remaining_uses(), 3);

        // 库里没有明文 token，只有 sha256
        let raw: String = sqlx::query_scalar("SELECT token_hash FROM artifact_shares WHERE id = ?")
            .bind(&sh.id)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(raw, hash_secret(&token));
        assert_ne!(raw, token);
        let hits: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM artifact_shares WHERE token_hash = ?")
                .bind(&token)
                .fetch_one(&p)
                .await
                .unwrap();
        assert_eq!(hits, 0, "拿明文 token 当哈希查必须查不到");

        // 反查要现算哈希；大小写不同就是另一个 token
        let found = by_token(&p, &token).await.unwrap().unwrap();
        assert_eq!(found.id, sh.id);
        assert!(by_token(&p, &token.to_uppercase()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 过期_用尽_撤销_三种失效() {
        let p = pool().await;
        let (uid, nsid, aid) = seed_artifact(&p).await;

        // 过期：昨天到期
        let (sh, _t) = create(&p, &aid, &nsid, &uid, "", 0, Some(expires_in_days(-1)))
            .await
            .unwrap();
        assert!(!sh.usable());

        // 用尽：限 1 次，用掉 1 次
        let (sh2, token2) = create(&p, &aid, &nsid, &uid, "", 1, None).await.unwrap();
        assert!(sh2.usable());
        mark_used(&p, &sh2.id).await.unwrap();
        let sh2 = by_token(&p, &token2).await.unwrap().unwrap();
        assert_eq!(sh2.used_count, 1);
        assert!(!sh2.usable());
        assert_eq!(sh2.remaining_uses(), 0);
        assert!(sh2.last_used_at.is_some());

        // 不限次（max_uses = 0）用多少次都可用
        let (sh3, token3) = create(&p, &aid, &nsid, &uid, "", 0, None).await.unwrap();
        mark_used(&p, &sh3.id).await.unwrap();
        mark_used(&p, &sh3.id).await.unwrap();
        let sh3 = by_token(&p, &token3).await.unwrap().unwrap();
        assert!(sh3.usable());
        assert_eq!(sh3.remaining_uses(), 0);

        // 撤销
        assert!(revoke(&p, &sh.id, &uid).await.unwrap());
        let sh = by_id(&p, &sh.id).await.unwrap().unwrap();
        assert!(sh.revoked_at.is_some());
        assert!(!sh.usable());
        // 别人的分享撤不动；管理员传空串可撤任意
        assert!(!revoke(&p, &sh2.id, "U-9").await.unwrap());
        assert!(revoke(&p, &sh2.id, "").await.unwrap());
        // 不存在
        assert!(!revoke(&p, "SH-nope", "").await.unwrap());
    }

    #[tokio::test]
    async fn 列表与计数_按创建者过滤() {
        let p = pool().await;
        let (uid, nsid, aid) = seed_artifact(&p).await;
        create(&p, &aid, &nsid, &uid, "一", 0, None).await.unwrap();
        create(&p, &aid, &nsid, &uid, "二", 0, None).await.unwrap();
        let (sh3, _t3) = create(&p, &aid, &nsid, "U-2", "三", 5, None).await.unwrap();

        assert_eq!(count(&p, &uid).await.unwrap(), 2);
        assert_eq!(count(&p, "").await.unwrap(), 3);

        let mine = list(&p, &uid, 0, 0).await.unwrap();
        assert_eq!(mine.len(), 2);
        assert_eq!(mine[0].created_by, uid);
        // join 带出制品与命名空间
        assert_eq!(mine[0].artifact_slug.as_deref(), Some("demo"));
        assert_eq!(mine[0].ns_slug.as_deref(), Some("zhangsan"));
        assert_eq!(mine[0].created_by_name.as_deref(), Some("张三"));
        assert_eq!(mine[0].as_share().id, mine[0].id);

        let all = list(&p, "", 200, 0).await.unwrap();
        assert_eq!(all.len(), 3);

        // 全部可用 → 活跃计数 = 3；撤一条 → 2
        assert_eq!(count_active(&p).await.unwrap(), 3);
        revoke(&p, &sh3.id, "U-2").await.unwrap();
        assert_eq!(count_active(&p).await.unwrap(), 2);

        // 分页
        assert_eq!(list(&p, "", 1, 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn 过期时刻落在未来() {
        let now = chrono::Local::now().fixed_offset();
        assert!(parse_time(&expires_in_days(7)).unwrap() > now);
    }
}
