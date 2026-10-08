//! 数据访问层（SQLite）。
//!
//! 与 Go 侧一样只依赖一个 SQLite 文件：「一个二进制 + 一个数据目录」是内网托管
//! 节点的部署形态。表结构与既有库逐字一致（见 `crate::schema`），所以新版服务
//! 可以直接开在旧库上。
//!
//! 拆分方式与 Go 的 `Store` 单结构体不同：这里按领域拆成若干模块，每个模块
//! 只拿 `&SqlitePool` 做事。好处是没有一个懂所有表的上帝对象，代价是跨领域
//! 的联表查询要显式指定归属（都放在 `artifacts.rs` 的 `list` 里了）。

pub mod access;
pub mod admin;
pub mod agentcards;
pub mod apikeys;
pub mod artifacts;
pub mod cluster;
pub mod configs;
pub mod conn;
pub mod exec;
pub mod feedback;
pub mod grants;
pub mod index;
pub mod namespaces;
pub mod nodes;
pub mod shares;
pub mod stack;
pub mod state;
pub mod traces;
pub mod users;

use sqlx::SqlitePool;

/// token / secret 的存储指纹：本仓规矩是**只存 sha256**，
/// 库里泄漏了也换不回可用凭据（列表因此也回不出可点链接）。
pub fn hash_secret(secret: &str) -> String {
    ncc_core::crypto::sha256_hex(secret.as_bytes())
}

/// 解析库里存的 JSON 字符串数组（`["a","b"]`）。
///
/// 老数据里可能是空串或者不是合法 JSON —— 那种一律当空列表，
/// 不该让一个字段的历史脏值把整个列表接口打成 500。
pub fn parse_list(s: &str) -> Vec<String> {
    let s = s.trim();
    if s.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Vec<String>>(s) {
        Ok(v) => v,
        Err(_) => Vec::new(),
    }
}

/// 序列化成库里存的 JSON 数组形态。
pub fn marshal_list(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

/// 按类型统计已发布制品数（`/api/registry/kinds` 用）。
pub async fn kind_counts(
    pool: &SqlitePool,
) -> Result<std::collections::HashMap<String, i64>, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT kind, COUNT(*) FROM artifacts GROUP BY kind")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().collect())
}

/// 表里是否已有满足条件的行（`SELECT COUNT(*) > 0`）。
pub async fn exists(pool: &SqlitePool, sql: &str, binds: &[&str]) -> Result<bool, sqlx::Error> {
    let mut q = sqlx::query_scalar::<_, i64>(sql);
    for b in binds {
        q = q.bind(*b);
    }
    Ok(q.fetch_one(pool).await? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 列表解析容错() {
        assert_eq!(parse_list(r#"["a","b"]"#), vec!["a", "b"]);
        assert!(parse_list("").is_empty());
        assert!(parse_list("不是 JSON").is_empty());
        assert_eq!(marshal_list(&["a".to_string()]), r#"["a"]"#);
    }
}
