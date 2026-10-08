//! Agent 名片的数据访问（原实现 `ncc-registry/store/agentcard.go`）。
//!
//! 名片（NCC Agent Share）= 把「我设计好的 Agent」点到点交给指定的人：
//! 一份 `.hur` 字节 + 可选一台节点，链接是 `/a/<token>`。
//!
//! 与分享（ArtifactShare）同一套「临时放行」的规矩，只有一处差别：分享指向一条
//! **已存在的制品**，名片自己带着字节（作者还没发布也能先把 Agent 交出去）。
//!
//! 刻意的取舍：
//!
//! * **token 只存 sha256**（`token_hash` + `token_hint`），明文只在创建那一刻返回 ——
//!   所以列表接口回不出可点的链接，也**没有**「按 token 反查明文」这种可能。
//! * **访问口令另算**：token 是 32 位随机串（不怕字典），口令是人写的 3-16 位短串，
//!   必须加盐（`pass_salt` + `hash_pass`），否则库一泄漏就是彩虹表秒破。
//! * 撤销是**标记 + 保留记录**（字节由调用方删）：作者在 `ncc agent ls` 里要看得到
//!   自己发过什么、哪张已作废。

use sqlx::SqlitePool;

use ncc_core::crypto::{rand_hex, sha256_hex};
use ncc_core::ids::new_id;
use ncc_core::timeutil::{now_go, parse_time};

use super::hash_secret;

/// 名片 token 的随机字节数：16 字节 → 32 位 hex（与 Go 的 `RandHex(16)` 一致）。
const TOKEN_BYTES: usize = 16;
/// 列表里「认人」用的 token 前缀长度。
const HINT_LEN: usize = 6;
const MAX_LIMIT: i64 = 200;
const DEFAULT_LIMIT: i64 = 50;

/// 生成名片 token（32 位 hex，放进 URL 路径；库里只存 sha256）。
pub fn new_token() -> String {
    rand_hex(TOKEN_BYTES)
}

/// 访问口令的**盐化**哈希（`sha256(salt + ":" + pw)`，与 Go 的 `HashCardPass` 逐字节一致）。
pub fn hash_pass(salt: &str, pw: &str) -> String {
    sha256_hex(format!("{salt}:{pw}").as_bytes())
}

/// 生成口令盐（8 字节 hex）。
pub fn new_salt() -> String {
    rand_hex(8)
}

/// 名片记录（`agent_cards` 一行）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AgentCard {
    pub id: String,
    pub token_hash: String,
    pub token_hint: String,
    pub owner_id: String,
    pub name: String,
    pub note: String,
    pub blob_name: String,
    pub size: i64,
    pub sha256: String,
    pub manifest: String,
    pub agent_id: String,
    pub agent_version: String,
    pub agent_kind: String,
    pub agent_profile: String,
    pub node_ref: String,
    pub node_id: String,
    pub node_kind: String,
    pub node_label: String,
    pub pass_hash: String,
    pub pass_salt: String,
    pub max_uses: i64,
    pub uses: i64,
    pub expires_at: Option<String>,
    pub revoked_at: Option<String>,
    pub views: i64,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl AgentCard {
    pub fn revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn expired(&self) -> bool {
        self.expired_at(chrono::Local::now().fixed_offset())
    }

    pub fn expired_at(&self, now: chrono::DateTime<chrono::FixedOffset>) -> bool {
        match self.expires_at.as_deref().and_then(parse_time) {
            Some(exp) => now > exp,
            None => false,
        }
    }

    pub fn exhausted(&self) -> bool {
        self.max_uses > 0 && self.uses >= self.max_uses
    }

    /// 为什么不能收：`""` = 可用，否则是三种分开说的原因
    /// （对接受方都是「用不了」，但原因决定了他下一步做什么）。
    pub fn state(&self) -> &'static str {
        if self.revoked() {
            return "revoked";
        }
        if self.expired() {
            return "expired";
        }
        if self.exhausted() {
            return "exhausted";
        }
        ""
    }

    /// 读（读名片 / 取字节）要不要拦。
    ///
    /// ⚠️ **名额用完不算拦**：名额是在 accept 那一刻扣的，扣完就不让取字节，
    /// 等于把已经拿到名额的人关在门外（平台侧实测踩过这个坑）。用完只对 accept 生效。
    pub fn readable(&self) -> bool {
        !self.revoked() && !self.expired()
    }

    /// 包内 `hur.json` 的解析结果（坏 JSON 一律回 `null`，不让名片整个读不出来）。
    pub fn manifest_value(&self) -> serde_json::Value {
        if self.manifest.trim().is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_str(&self.manifest).unwrap_or(serde_json::Value::Null)
    }

    /// 过期时刻的 RFC3339(UTC, 秒精度) 形态（响应里与 Go 的 `UTC().Format(RFC3339)` 一致）。
    pub fn expires_at_utc(&self) -> Option<String> {
        self.expires_at.as_deref().and_then(parse_time).map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
    }

    /// 有效期展示用（本地时区，`2006-01-02 15:04`）。
    pub fn expires_at_local_text(&self) -> Option<String> {
        self.expires_at
            .as_deref()
            .and_then(parse_time)
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
    }
}

/// 建名片时的入参（清单与 sha256 都由**服务端从字节里读/算**，不信客户端报的）。
pub struct Input {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub note: String,
    pub blob_name: String,
    pub size: i64,
    pub sha256: String,
    pub manifest: String,
    pub agent_id: String,
    pub agent_version: String,
    pub agent_kind: String,
    pub agent_profile: String,
    pub node_ref: String,
    pub node_id: String,
    pub node_kind: String,
    pub node_label: String,
    pub pass_hash: String,
    pub pass_salt: String,
    pub max_uses: i64,
    pub expires_at: Option<String>,
}

/// 建一张名片，返回记录与**明文 token**（明文只在这一刻存在）。
pub async fn create(pool: &SqlitePool, in_: Input) -> Result<(AgentCard, String), sqlx::Error> {
    let token = new_token();
    let hint: String = token.chars().take(HINT_LEN).collect();
    let id = if in_.id.trim().is_empty() {
        new_id("AC")
    } else {
        in_.id.clone()
    };
    let now = now_go();
    sqlx::query(
        "INSERT INTO agent_cards (id, token_hash, token_hint, owner_id, name, note, blob_name, size, sha256, manifest, agent_id, agent_version, agent_kind, agent_profile, node_ref, node_id, node_kind, node_label, pass_hash, pass_salt, max_uses, uses, expires_at, views, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(hash_secret(&token))
    .bind(&hint)
    .bind(&in_.owner_id)
    .bind(in_.name.trim())
    .bind(in_.note.trim())
    .bind(&in_.blob_name)
    .bind(in_.size)
    .bind(&in_.sha256)
    .bind(&in_.manifest)
    .bind(&in_.agent_id)
    .bind(&in_.agent_version)
    .bind(&in_.agent_kind)
    .bind(&in_.agent_profile)
    .bind(&in_.node_ref)
    .bind(&in_.node_id)
    .bind(&in_.node_kind)
    .bind(&in_.node_label)
    .bind(&in_.pass_hash)
    .bind(&in_.pass_salt)
    .bind(in_.max_uses)
    .bind(&in_.expires_at)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    let card = by_id(pool, &id).await?.ok_or(sqlx::Error::RowNotFound)?;
    Ok((card, token))
}

/// token 是明文，库里存哈希 —— 查的时候现算。
pub async fn by_token(pool: &SqlitePool, token: &str) -> Result<Option<AgentCard>, sqlx::Error> {
    let hash = hash_secret(token.trim());
    sqlx::query_as::<_, AgentCard>("SELECT * FROM agent_cards WHERE token_hash = ?")
        .bind(hash)
        .fetch_optional(pool)
        .await
}

/// 按 id（`AC-…`）取 —— 作者自己管理时用得到，不必再握着 token。
pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<AgentCard>, sqlx::Error> {
    sqlx::query_as::<_, AgentCard>("SELECT * FROM agent_cards WHERE id = ?")
        .bind(id.trim())
        .fetch_optional(pool)
        .await
}

/// 只列**某个人的**名片（含已撤销 / 过期 / 用尽 —— 作者要看得到自己发过什么）。
/// 与分享一样，刻意没有「全站名片」这种查询。
pub async fn list(
    pool: &SqlitePool,
    owner_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<AgentCard>, sqlx::Error> {
    let limit = if limit <= 0 || limit > MAX_LIMIT { DEFAULT_LIMIT } else { limit };
    sqlx::query_as::<_, AgentCard>(
        "SELECT * FROM agent_cards WHERE owner_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(owner_id)
    .bind(limit)
    .bind(offset.max(0))
    .fetch_all(pool)
    .await
}

/// 撤销：**标记 + 保留记录**（字节由调用方删）。返回是否真的改到了行。
pub async fn revoke(pool: &SqlitePool, id: &str, owner_id: &str) -> Result<bool, sqlx::Error> {
    let now = now_go();
    let res = sqlx::query(
        "UPDATE agent_cards SET revoked_at = ?, updated_at = ? WHERE id = ? AND owner_id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(id)
    .bind(owner_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// 扣一次名额，返回扣完后的用量（`None` = 名额刚好在并发里被别人用掉了）。
///
/// 用一条**带条件的 UPDATE** 完成「检查 + 扣减」：只有 `max_uses = 0`（不限次）
/// 或还没用完的那一行会被更新，`rows_affected = 0` 就说明抢光了 —— 这时候不能当成功。
pub async fn consume(pool: &SqlitePool, id: &str) -> Result<Option<i64>, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE agent_cards SET uses = uses + 1 WHERE id = ? AND (max_uses = 0 OR uses < max_uses)",
    )
    .bind(id)
    .execute(pool)
    .await?;
    if res.rows_affected() == 0 {
        return Ok(None);
    }
    let uses: Option<i64> = sqlx::query_scalar("SELECT uses FROM agent_cards WHERE id = ?")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(uses)
}

pub async fn bump_views(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE agent_cards SET views = views + 1 WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL).await.unwrap();
        p
    }

    fn input(owner: &str, name: &str) -> Input {
        Input {
            id: String::new(),
            owner_id: owner.to_string(),
            name: name.to_string(),
            note: "  备注  ".to_string(),
            blob_name: "agent-cards/AC-x.hur".to_string(),
            size: 12,
            sha256: "deadbeef".to_string(),
            manifest: r#"{"spec":"hur/1","id":"demo","version":"1.0.0"}"#.to_string(),
            agent_id: "demo".to_string(),
            agent_version: "1.0.0".to_string(),
            agent_kind: "agent".to_string(),
            agent_profile: "default".to_string(),
            node_ref: String::new(),
            node_id: String::new(),
            node_kind: String::new(),
            node_label: String::new(),
            pass_hash: String::new(),
            pass_salt: String::new(),
            max_uses: 0,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn 建名片_明文只回一次_库里只有哈希() {
        let p = pool().await;
        let (card, token) = create(&p, input("U-1", "  演示 Agent  ")).await.unwrap();
        assert_eq!(token.len(), 32);
        assert!(card.id.starts_with("AC-"));
        assert_eq!(card.token_hint, token[..6]);
        assert_eq!(card.name, "演示 Agent"); // 入库前 trim
        assert_eq!(card.note, "备注");
        assert_eq!(card.uses, 0);
        assert_eq!(card.views, 0);
        assert!(card.readable());
        assert_eq!(card.state(), "");
        assert_eq!(card.manifest_value()["id"], "demo");
        assert_eq!(card.expires_at, None);
        assert_eq!(card.expires_at_utc(), None);

        let stored: String = sqlx::query_scalar("SELECT token_hash FROM agent_cards WHERE id = ?")
            .bind(&card.id)
            .fetch_one(&p)
            .await
            .unwrap();
        assert_eq!(stored, hash_secret(&token));
        assert_ne!(stored, token);
        assert!(by_token(&p, &token).await.unwrap().is_some());
        assert!(by_token(&p, "wrong").await.unwrap().is_none());
        assert!(by_id(&p, &card.id).await.unwrap().is_some());
        assert!(by_id(&p, "AC-nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 口令盐化哈希_同口令不同盐结果不同() {
        let salt = new_salt();
        assert_eq!(salt.len(), 16);
        assert_eq!(hash_pass(&salt, "abc"), hash_pass(&salt, "abc"));
        assert_ne!(hash_pass(&salt, "abc"), hash_pass(&salt, "abd"));
        assert_ne!(hash_pass(&salt, "abc"), hash_pass("other-salt", "abc"));
        // 与 Go 的 HashCardPass 一致：sha256(salt + ":" + pw)
        assert_eq!(
            hash_pass("s", "p"),
            ncc_core::crypto::sha256_hex(b"s:p")
        );
    }

    #[tokio::test]
    async fn 次数_扣减与并发抢光() {
        let p = pool().await;
        let (card, _t) = create(&p, input("U-1", "限次名片")).await.unwrap();
        // 不限次：想扣几次扣几次（max_uses = 0）
        assert_eq!(consume(&p, &card.id).await.unwrap(), Some(1));
        assert_eq!(consume(&p, &card.id).await.unwrap(), Some(2));

        // 限 2 次：第 3 次扣不动
        let mut inp = input("U-1", "限次名片2");
        inp.max_uses = 2;
        let (c2, _t2) = create(&p, inp).await.unwrap();
        assert_eq!(consume(&p, &c2.id).await.unwrap(), Some(1));
        assert_eq!(consume(&p, &c2.id).await.unwrap(), Some(2));
        assert_eq!(consume(&p, &c2.id).await.unwrap(), None, "名额用尽要扣不动");

        let fresh = by_id(&p, &c2.id).await.unwrap().unwrap();
        assert!(fresh.exhausted());
        assert_eq!(fresh.state(), "exhausted");
        // ⚠️ 用尽不等于不可读：已拿到名额的人还得能取字节
        assert!(fresh.readable());
        // 不存在 / 已扣完的 id
        assert_eq!(consume(&p, "AC-nope").await.unwrap(), None);
    }

    #[tokio::test]
    async fn 过期与撤销_读被拦但状态分开说() {
        let p = pool().await;
        let mut inp = input("U-1", "过期名片");
        inp.expires_at = Some(ncc_core::timeutil::format_go(
            chrono::Local::now().fixed_offset() - chrono::Duration::hours(1),
        ));
        let (c1, _t) = create(&p, inp).await.unwrap();
        assert!(c1.expired());
        assert_eq!(c1.state(), "expired");
        assert!(!c1.readable());
        assert!(!c1.exhausted());
        assert!(c1.expires_at_utc().is_some());
        assert!(c1.expires_at_local_text().is_some());

        let (c2, _t2) = create(&p, input("U-1", "撤销名片")).await.unwrap();
        assert!(revoke(&p, &c2.id, "U-1").await.unwrap());
        let c2 = by_id(&p, &c2.id).await.unwrap().unwrap();
        assert!(c2.revoked());
        assert_eq!(c2.state(), "revoked");
        assert!(!c2.readable());
        // 记录仍在（作者要看得到自己发过什么）
        assert!(by_id(&p, &c2.id).await.unwrap().is_some());
        // 别人的名片撤不动；不存在的也撤不动
        assert!(!revoke(&p, &c2.id, "U-9").await.unwrap());
        assert!(!revoke(&p, "AC-nope", "U-1").await.unwrap());
    }

    #[tokio::test]
    async fn 列表_只列自己的_含已失效() {
        let p = pool().await;
        let (c1, _t) = create(&p, input("U-1", "一")).await.unwrap();
        let (c2, _t2) = create(&p, input("U-1", "二")).await.unwrap();
        let (_c3, _t3) = create(&p, input("U-2", "别人的")).await.unwrap();
        revoke(&p, &c1.id, "U-1").await.unwrap();
        bump_views(&p, &c2.id).await.unwrap();

        let mine = list(&p, "U-1", 0, 0).await.unwrap();
        assert_eq!(mine.len(), 2, "自己的两张都在（含已撤销）");
        assert!(mine.iter().any(|c| c.revoked()));
        let listed = list(&p, "U-1", 1, 0).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(list(&p, "U-1", 0, 5).await.unwrap().len(), 0);
        let c2 = by_id(&p, &c2.id).await.unwrap().unwrap();
        assert_eq!(c2.views, 1);
    }

    #[tokio::test]
    async fn 重复创建_同一份字节两张名片_互不影响() {
        let p = pool().await;
        let (a, ta) = create(&p, input("U-1", "同名")).await.unwrap();
        let (b, tb) = create(&p, input("U-1", "同名")).await.unwrap();
        assert_ne!(a.id, b.id);
        assert_ne!(ta, tb);
        assert_ne!(a.token_hash, b.token_hash);
        // 撤销一张不影响另一张
        assert!(revoke(&p, &a.id, "U-1").await.unwrap());
        assert!(by_id(&p, &a.id).await.unwrap().unwrap().revoked());
        assert!(!by_id(&p, &b.id).await.unwrap().unwrap().revoked());
        assert_eq!(by_token(&p, &tb).await.unwrap().unwrap().id, b.id);
    }
}
