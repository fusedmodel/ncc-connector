//! 命名空间（个人 account / 组织 org）与成员。

use sqlx::SqlitePool;

use ncc_core::ids::{new_id, slugify, valid_slug};
use ncc_core::timeutil::now_go;

use super::exists;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Namespace {
    pub id: String,
    pub slug: String,
    pub name: String,
    #[sqlx(rename = "type")]
    pub ns_type: String,
    pub owner_id: String,
    pub visibility: String,
    pub created_at: Option<String>,
}

const COLS: &str = "id, slug, name, type, owner_id, visibility, created_at";

/// 建个人命名空间：slug 冲突时加随机后缀（最多试 5 次）。
pub async fn create_account(
    pool: &SqlitePool,
    user_id: &str,
    name: &str,
    slug_seed: &str,
) -> Result<Namespace, sqlx::Error> {
    let mut slug = slugify(slug_seed);
    if slug == "x" && slug_seed.trim().is_empty() {
        slug = slugify(user_id);
    }
    for _ in 0..5 {
        if by_slug(pool, &slug).await?.is_none() {
            break;
        }
        slug = format!("{}-{}", slugify(slug_seed), ncc_core::crypto::rand_hex(2));
    }
    insert(pool, &slug, name, "account", user_id).await
}

/// 建组织命名空间。slug 非法时返回 None（由调用方给出 400），
/// 而不是硬造一个 slug —— 组织 slug 会进制品引用 `@slug/name`，不能让服务端随便改。
pub async fn create_org(
    pool: &SqlitePool,
    owner_id: &str,
    slug: &str,
    name: &str,
) -> Result<Option<Namespace>, sqlx::Error> {
    let slug = slugify(slug);
    if !valid_slug(&slug) {
        return Ok(None);
    }
    if by_slug(pool, &slug).await?.is_some() {
        return Ok(None);
    }
    insert(pool, &slug, name, "org", owner_id).await.map(Some)
}

async fn insert(
    pool: &SqlitePool,
    slug: &str,
    name: &str,
    ns_type: &str,
    owner_id: &str,
) -> Result<Namespace, sqlx::Error> {
    let ns = Namespace {
        id: new_id("NS"),
        slug: slug.to_string(),
        name: name.to_string(),
        ns_type: ns_type.to_string(),
        owner_id: owner_id.to_string(),
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

pub async fn by_slug(pool: &SqlitePool, slug: &str) -> Result<Option<Namespace>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM namespaces WHERE slug = ?");
    let slug = slug.trim().trim_start_matches('@');
    sqlx::query_as::<_, Namespace>(&sql)
        .bind(slug)
        .fetch_optional(pool)
        .await
}

pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<Namespace>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM namespaces WHERE id = ?");
    sqlx::query_as::<_, Namespace>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// 用户的个人命名空间。
pub async fn personal(pool: &SqlitePool, user_id: &str) -> Result<Option<Namespace>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM namespaces WHERE owner_id = ? AND type = 'account'");
    sqlx::query_as::<_, Namespace>(&sql)
        .bind(user_id)
        .fetch_optional(pool)
        .await
}

/// 用户能看到的全部命名空间：自己的 + 作为成员加入的。
pub async fn of_user(pool: &SqlitePool, user_id: &str) -> Result<Vec<Namespace>, sqlx::Error> {
    let sql = format!(
        "SELECT {COLS} FROM namespaces WHERE owner_id = ? OR id IN (SELECT namespace_id FROM ns_members WHERE user_id = ?) ORDER BY type, slug"
    );
    sqlx::query_as::<_, Namespace>(&sql)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(pool)
        .await
}

pub async fn is_owner(pool: &SqlitePool, ns_id: &str, user_id: &str) -> bool {
    exists(
        pool,
        "SELECT COUNT(*) FROM namespaces WHERE id = ? AND owner_id = ?",
        &[ns_id, user_id],
    )
    .await
    .unwrap_or(false)
}

pub async fn is_member(pool: &SqlitePool, ns_id: &str, user_id: &str) -> bool {
    exists(
        pool,
        "SELECT COUNT(*) FROM ns_members WHERE namespace_id = ? AND user_id = ?",
        &[ns_id, user_id],
    )
    .await
    .unwrap_or(false)
}

pub async fn add_member(
    pool: &SqlitePool,
    ns_id: &str,
    user_id: &str,
    role: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR IGNORE INTO ns_members (namespace_id, user_id, role) VALUES (?, ?, ?)")
        .bind(ns_id)
        .bind(user_id)
        .bind(role)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

    async fn pool() -> SqlitePool {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL)
            .await
            .unwrap();
        p
    }

    #[tokio::test]
    async fn 个人命名空间_slug_冲突自动加后缀() {
        let p = pool().await;
        let a = create_account(&p, "U-1", "张三", "zhangsan").await.unwrap();
        assert_eq!(a.slug, "zhangsan");
        let b = create_account(&p, "U-2", "张三二", "zhangsan")
            .await
            .unwrap();
        assert!(b.slug.starts_with("zhangsan-"));
        assert_ne!(a.slug, b.slug);
    }

    #[tokio::test]
    async fn 组织_slug_非法或重复返回_none() {
        let p = pool().await;
        assert!(create_org(&p, "U-1", "OK-Slug", "组织")
            .await
            .unwrap()
            .is_some());
        assert!(create_org(&p, "U-1", "ok-slug", "重名")
            .await
            .unwrap()
            .is_none());
    }
}
