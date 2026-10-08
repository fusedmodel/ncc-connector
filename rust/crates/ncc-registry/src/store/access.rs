//! 接入票据（AccessTicket）的数据访问（原实现 `ncc-registry/store/store.go` 里
//! `AccessTicket` 那一段）。
//!
//! 这一族做什么：把「一个内网 registry 加进 Agent」做成一张可限次 / 可过期 / 可停用的
//! 票据 —— key 短到能念，secret 只在创建时回显一次，兑换出来的是一枚只能做票据授权
//! 之事的节点令牌。
//!
//! 刻意的取舍：
//!
//! * **secret 只存 sha256**（`store::hash_secret`）：库里泄漏也换不回可用票据；代价是
//!   列表接口回不出 secret（本来也只在创建那一刻回显），反查一律现算哈希。
//! * **可用性在 Rust 侧判**（停用 / 过期 / 用尽是三个独立条件，缺一即失效），时间列按
//!   既有库的 GORM 文本格式存取（见 `ncc_core::timeutil`），不给 sqlx 映射 chrono 类型 ——
//!   映射错了只会得到「读写都能跑、老数据一读就炸」这类最难查的错。
//! * 与 Grant 无关：票据换的是**一个节点身份**，不是别人的资源可见性。

use sqlx::SqlitePool;

use ncc_core::crypto::rand_hex;
use ncc_core::ids::new_id;
use ncc_core::timeutil::{format_go, now_go, parse_time};

use super::{hash_secret, marshal_list, parse_list};

/// 短 key 的随机字节数：3 字节 → 6 位 hex（与 Go 的 `RandHex(3)` 逐字一致），
/// 短到可以口头念出来。
const KEY_BYTES: usize = 3;
/// secret 随机字节数：16 字节 → 32 位 hex（与 Go 的 `RandHex(16)` 一致）。
const SECRET_BYTES: usize = 16;

/// 生成票据短 key（`NK-` 前缀 + 大写 hex）。前缀让它在日志里一眼可辨。
pub fn new_ticket_key() -> String {
    format!("NK-{}", rand_hex(KEY_BYTES).to_uppercase())
}

/// 生成 secret 明文（只返回给创建者一次）。
pub fn new_ticket_secret() -> String {
    rand_hex(SECRET_BYTES)
}

/// 票据记录（`access_tickets` 一行）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AccessTicket {
    pub id: String,
    pub key: String,
    pub secret_hash: String,
    pub label: String,
    pub scopes: String,
    pub namespace_id: String,
    pub created_by: String,
    /// 0 = 不限次。
    pub max_uses: i64,
    pub used_count: i64,
    pub expires_at: Option<String>,
    pub disabled: bool,
    pub last_used_at: Option<String>,
    pub created_at: Option<String>,
}

/// 列名写全（不用 `SELECT *`）：`key` 是 SQLite 的弱关键字，统一加反引号最稳。
const COLS: &str = "id, `key`, secret_hash, label, scopes, namespace_id, created_by, \
     max_uses, used_count, expires_at, disabled, last_used_at, created_at";

impl AccessTicket {
    /// 指定时刻是否可用（停用 / 过期 / 用尽，缺一即失效）。
    ///
    /// 收「当前时刻」而不是内部取时钟：一次请求里要判多次时，避免两次取时钟打架。
    pub fn usable_at(&self, now: chrono::DateTime<chrono::FixedOffset>) -> bool {
        if self.disabled {
            return false;
        }
        if let Some(exp) = self.expires() {
            if now > exp {
                return false;
            }
        }
        if self.max_uses > 0 && self.used_count >= self.max_uses {
            return false;
        }
        true
    }

    /// 现在可用吗。
    pub fn usable(&self) -> bool {
        self.usable_at(chrono::Local::now().fixed_offset())
    }

    /// 单据作用域（库里存的是 JSON 数组文本，脏值当空表）。
    pub fn scope_list(&self) -> Vec<String> {
        parse_list(&self.scopes)
    }

    pub fn expires(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        self.expires_at.as_deref().and_then(parse_time)
    }

    pub fn last_used(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        self.last_used_at.as_deref().and_then(parse_time)
    }

    pub fn created(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        self.created_at.as_deref().and_then(parse_time)
    }

    /// 是不是这把 secret（只比对 sha256，明文从不落库）。
    pub fn secret_matches(&self, secret: &str) -> bool {
        hash_secret(secret.trim()) == self.secret_hash
    }
}

/// 建票据。`secret` 是明文，只用来算哈希。
pub async fn create(
    pool: &SqlitePool,
    key: &str,
    secret: &str,
    label: &str,
    scopes: &[String],
    ns_id: &str,
    created_by: &str,
    max_uses: i64,
    expires_at: Option<chrono::DateTime<chrono::FixedOffset>>,
) -> Result<AccessTicket, sqlx::Error> {
    let id = new_id("TK");
    let expires = expires_at.map(format_go);
    sqlx::query(
        "INSERT INTO access_tickets (id, `key`, secret_hash, label, scopes, namespace_id, created_by, \
         max_uses, used_count, expires_at, disabled, last_used_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, NULL, ?)",
    )
    .bind(&id)
    .bind(key)
    .bind(hash_secret(secret))
    .bind(label)
    .bind(marshal_list(scopes))
    .bind(ns_id)
    .bind(created_by)
    .bind(max_uses.max(0))
    .bind(expires)
    .bind(false)
    .bind(now_go())
    .execute(pool)
    .await?;
    by_id(pool, &id).await?.ok_or(sqlx::Error::RowNotFound)
}

pub async fn by_key(pool: &SqlitePool, key: &str) -> Result<Option<AccessTicket>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM access_tickets WHERE `key` = ?");
    sqlx::query_as::<_, AccessTicket>(&sql)
        .bind(key.trim())
        .fetch_optional(pool)
        .await
}

pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<AccessTicket>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM access_tickets WHERE id = ?");
    sqlx::query_as::<_, AccessTicket>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// 我签发的票据（新的在前）。
pub async fn list_by_creator(
    pool: &SqlitePool,
    created_by: &str,
) -> Result<Vec<AccessTicket>, sqlx::Error> {
    let sql =
        format!("SELECT {COLS} FROM access_tickets WHERE created_by = ? ORDER BY created_at desc");
    sqlx::query_as::<_, AccessTicket>(&sql)
        .bind(created_by)
        .fetch_all(pool)
        .await
}

/// 删票据。带 `created_by` 是有意的：只能删自己签发的（删别人的返回 0 行、不报错，
/// 与 Go 一致 —— 删除是幂等动作，不该因为「不是你的」而暴露票据归属）。
pub async fn delete(pool: &SqlitePool, id: &str, created_by: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM access_tickets WHERE id = ? AND created_by = ?")
        .bind(id)
        .bind(created_by)
        .execute(pool)
        .await?;
    Ok(())
}

/// 记一次兑换（次数 +1、记最后一次使用时间）。
pub async fn mark_used(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE access_tickets SET used_count = used_count + 1, last_used_at = ? WHERE id = ?",
    )
    .bind(now_go())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    /// 每个测试一个**临时文件库**（不用 `sqlite::memory:` + 连接池：sqlx 会把它落成
    /// 同名文件，同进程的多个测试互相打架）。
    async fn pool(name: &str) -> (SqlitePool, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("ncc-accessstore-{}-{name}", std::process::id()));
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

    fn scopes(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn 票据Key形态与唯一性() {
        let a = new_ticket_key();
        let b = new_ticket_key();
        assert!(a.starts_with("NK-"), "{a}");
        assert_eq!(a.len(), 9, "{a}");
        assert!(a[3..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
        assert_ne!(a, b);
        assert_eq!(new_ticket_secret().len(), 32);
    }

    #[tokio::test]
    async fn 创建查找与secret只存哈希() {
        let (p, dir) = pool("create").await;
        let t = create(
            &p,
            "NK-ABC123",
            "s3cret",
            "内网节点",
            &scopes(&["nodes:write", "registry:read"]),
            "NS-1",
            "U-1",
            3,
            None,
        )
        .await
        .unwrap();
        assert!(t.id.starts_with("TK"));
        assert_eq!(t.max_uses, 3);
        assert_eq!(t.used_count, 0);
        assert!(!t.disabled);
        assert!(t.expires_at.is_none());
        // 库里存的是哈希，明文进不去
        assert_ne!(t.secret_hash, "s3cret");
        assert_eq!(t.secret_hash, hash_secret("s3cret"));
        assert!(t.secret_matches("s3cret"));
        assert!(
            t.secret_matches(" s3cret "),
            "比对前要 trim（Go 侧同样 trim）"
        );
        assert!(!t.secret_matches("别的"));
        assert_eq!(t.scope_list(), scopes(&["nodes:write", "registry:read"]));

        let got = by_key(&p, " NK-ABC123 ").await.unwrap().unwrap();
        assert_eq!(got.id, t.id);
        assert!(by_key(&p, "NK-NOPE").await.unwrap().is_none());
        assert!(by_id(&p, &t.id).await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn 可用性_停用过期用尽() {
        let (p, dir) = pool("usable").await;
        let now = chrono::Local::now().fixed_offset();

        // 正常
        let t = create(&p, "NK-1", "s", "", &scopes(&[]), "NS-1", "U-1", 0, None)
            .await
            .unwrap();
        assert!(t.usable_at(now));

        // 过期（票据带过去时间）
        let past = create(
            &p,
            "NK-2",
            "s",
            "",
            &scopes(&[]),
            "NS-1",
            "U-1",
            0,
            Some(now - Duration::seconds(5)),
        )
        .await
        .unwrap();
        assert!(!past.usable_at(now));
        assert!(past.expires().is_some());

        // 未到期：还能用
        let future = create(
            &p,
            "NK-3",
            "s",
            "",
            &scopes(&[]),
            "NS-1",
            "U-1",
            0,
            Some(now + Duration::hours(1)),
        )
        .await
        .unwrap();
        assert!(future.usable_at(now));

        // 用尽：1 次用完即失效；不限次（max_uses=0）不受影响
        let limited = create(&p, "NK-4", "s", "", &scopes(&[]), "NS-1", "U-1", 1, None)
            .await
            .unwrap();
        mark_used(&p, &limited.id).await.unwrap();
        let after = by_id(&p, &limited.id).await.unwrap().unwrap();
        assert_eq!(after.used_count, 1);
        assert!(after.last_used().is_some());
        assert!(!after.usable_at(now));

        mark_used(&p, &t.id).await.unwrap();
        let unlimited = by_id(&p, &t.id).await.unwrap().unwrap();
        assert!(unlimited.usable_at(now), "max_uses=0 是不限次");

        // 停用
        sqlx::query("UPDATE access_tickets SET disabled = true WHERE id = ?")
            .bind(&t.id)
            .execute(&p)
            .await
            .unwrap();
        let off = by_id(&p, &t.id).await.unwrap().unwrap();
        assert!(!off.usable_at(now));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn 列表与删除只限自己签发的() {
        let (p, dir) = pool("list").await;
        let mine = create(
            &p,
            "NK-MINE1",
            "s",
            "我的",
            &scopes(&[]),
            "NS-1",
            "U-1",
            0,
            None,
        )
        .await
        .unwrap();
        let other = create(
            &p,
            "NK-OTHR1",
            "s",
            "别人的",
            &scopes(&[]),
            "NS-1",
            "U-2",
            0,
            None,
        )
        .await
        .unwrap();

        let list = list_by_creator(&p, "U-1").await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, mine.id);

        // 删别人的：不回错，但也删不掉
        delete(&p, &other.id, "U-1").await.unwrap();
        assert!(by_id(&p, &other.id).await.unwrap().is_some());
        // 删自己的：真删
        delete(&p, &mine.id, "U-1").await.unwrap();
        assert!(by_id(&p, &mine.id).await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn 脏作用域与空时间字段不当成错误() {
        let (p, dir) = pool("dirty").await;
        sqlx::query(
            "INSERT INTO access_tickets (id, `key`, secret_hash, label, scopes, namespace_id, created_by, \
             max_uses, used_count, disabled, created_at) VALUES ('TK-X', 'NK-X', 'h', '', '不是 JSON', '', 'U-1', 0, 0, 0, ?)",
        )
        .bind(now_go())
        .execute(&p)
        .await
        .unwrap();
        let t = by_key(&p, "NK-X").await.unwrap().unwrap();
        assert!(t.scope_list().is_empty());
        assert!(t.expires().is_none());
        assert!(t.last_used().is_none());
        assert!(t.created().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
