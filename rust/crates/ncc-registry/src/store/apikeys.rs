//! API-Key：`ncc_<prefix>_<secret>`，库里只存 prefix 与整串的 sha256。

use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::now_go;

use super::{hash_secret, marshal_list, parse_list};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ApiKey {
    pub id: String,
    pub user_id: String,
    pub label: String,
    pub prefix: String,
    pub secret_hash: String,
    pub scopes: String,
    pub created_at: Option<String>,
    pub last_used_at: Option<String>,
}

const COLS: &str = "id, user_id, label, prefix, secret_hash, scopes, created_at, last_used_at";

/// 新建 Key，返回（记录，明文）。明文只在这一刻存在。
pub async fn create(
    pool: &SqlitePool,
    user_id: &str,
    label: &str,
    scopes: &[String],
) -> Result<(ApiKey, String), sqlx::Error> {
    let prefix = ncc_core::crypto::rand_prefix();
    let secret = format!("ncc_{prefix}_{}", ncc_core::crypto::rand_hex(20));
    let k = ApiKey {
        id: new_id("K"),
        user_id: user_id.to_string(),
        label: label.to_string(),
        prefix: prefix.clone(),
        secret_hash: hash_secret(&secret),
        scopes: marshal_list(scopes),
        created_at: Some(now_go()),
        last_used_at: None,
    };
    sqlx::query(
        "INSERT INTO api_keys (id, user_id, label, prefix, secret_hash, scopes, created_at, last_used_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&k.id)
    .bind(&k.user_id)
    .bind(&k.label)
    .bind(&k.prefix)
    .bind(&k.secret_hash)
    .bind(&k.scopes)
    .bind(&k.created_at)
    .bind(&k.last_used_at)
    .execute(pool)
    .await?;
    Ok((k, secret))
}

/// 按明文找 Key：先用 `ncc_` 之后的 prefix 段定位，再比对整串哈希。
///
/// 比对的是**整串**而不是随机段：这样即使 prefix 撞了也不会误认。
/// （与 Go 的 `strings.Split(secret, "_")` + `parts[1]` 行为一致。）
pub async fn find_by_secret(
    pool: &SqlitePool,
    secret: &str,
) -> Result<Option<ApiKey>, sqlx::Error> {
    let parts: Vec<&str> = secret.split('_').collect();
    if parts.len() < 3 {
        return Ok(None);
    }
    let sql = format!("SELECT {COLS} FROM api_keys WHERE prefix = ? LIMIT 1");
    let Some(k) = sqlx::query_as::<_, ApiKey>(&sql)
        .bind(parts[1])
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    if hash_secret(secret) != k.secret_hash {
        return Ok(None);
    }
    Ok(Some(k))
}

pub async fn list(pool: &SqlitePool, user_id: &str) -> Result<Vec<ApiKey>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM api_keys WHERE user_id = ? ORDER BY created_at DESC");
    sqlx::query_as::<_, ApiKey>(&sql)
        .bind(user_id)
        .fetch_all(pool)
        .await
}

pub async fn delete(pool: &SqlitePool, id: &str, user_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM api_keys WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn touch(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE id = ?")
        .bind(now_go())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Key 的作用域列表。
pub fn scopes_of(k: &ApiKey) -> Vec<String> {
    parse_list(&k.scopes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 明文可反查_且只存哈希() {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL)
            .await
            .unwrap();
        let (k, secret) = create(&p, "U-1", "ci", &["registry:read".to_string()])
            .await
            .unwrap();
        assert!(secret.starts_with("ncc_"));
        assert_ne!(k.secret_hash, secret);
        let found = find_by_secret(&p, &secret).await.unwrap().unwrap();
        assert_eq!(found.id, k.id);
        assert!(find_by_secret(&p, "ncc_xxxx_yyyy").await.unwrap().is_none());
        assert!(find_by_secret(&p, "not-a-key").await.unwrap().is_none());
    }
}
