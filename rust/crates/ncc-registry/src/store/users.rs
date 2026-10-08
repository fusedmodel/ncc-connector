//! 用户。第一个注册的账号自动成为**本节点管理员** —— 内网托管节点的部署形态是
//! 「谁先装谁是主人」，不该再引入一套外部账号系统去决定谁管这台机器。

use sqlx::SqlitePool;

use ncc_core::timeutil::now_go;

use super::exists;

/// 用户行（列名与既有库一一对应）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct User {
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
}

const COLS: &str = "id, email, name, pass_hash, plan, is_admin, disabled, disabled_at, admin_note, last_login_at, created_at";

/// 建账号；库为空时该账号成为管理员。
pub async fn create(pool: &SqlitePool, name: &str, email: &str, pass_hash: &str) -> Result<User, sqlx::Error> {
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(pool).await?;
    let u = User {
        id: ncc_core::ids::new_id("U"),
        email: email.trim().to_lowercase(),
        name: name.to_string(),
        pass_hash: pass_hash.to_string(),
        plan: "free".to_string(),
        is_admin: total == 0,
        disabled: false,
        disabled_at: None,
        admin_note: String::new(),
        last_login_at: None,
        created_at: Some(now_go()),
    };
    sqlx::query(
        "INSERT INTO users (id, email, name, pass_hash, plan, is_admin, disabled, disabled_at, admin_note, last_login_at, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&u.id)
    .bind(&u.email)
    .bind(&u.name)
    .bind(&u.pass_hash)
    .bind(&u.plan)
    .bind(u.is_admin)
    .bind(u.disabled)
    .bind(&u.disabled_at)
    .bind(&u.admin_note)
    .bind(&u.last_login_at)
    .bind(&u.created_at)
    .execute(pool)
    .await?;
    Ok(u)
}

pub async fn by_email(pool: &SqlitePool, email: &str) -> Result<Option<User>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM users WHERE email = ?");
    sqlx::query_as::<_, User>(&sql)
        .bind(email.trim().to_lowercase())
        .fetch_optional(pool)
        .await
}

pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<User>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM users WHERE id = ?");
    sqlx::query_as::<_, User>(&sql).bind(id).fetch_optional(pool).await
}

pub async fn touch_login(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET last_login_at = ? WHERE id = ?")
        .bind(now_go())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_name(pool: &SqlitePool, id: &str, name: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET name = ? WHERE id = ?")
        .bind(name)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_pass(pool: &SqlitePool, id: &str, hash: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET pass_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 统计用户数（`/api/meta`、健康检查、集群上报都用它）。
pub async fn count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM users").fetch_one(pool).await
}

/// 是否已存在该邮箱。
pub async fn email_taken(pool: &SqlitePool, email: &str) -> Result<bool, sqlx::Error> {
    exists(pool, "SELECT COUNT(*) FROM users WHERE email = ?", &[&email.trim().to_lowercase()]).await
}

/// 管理员名单（按创建时间正序）。
pub async fn list(pool: &SqlitePool, limit: i64, offset: i64) -> Result<Vec<User>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM users ORDER BY created_at ASC LIMIT ? OFFSET ?");
    sqlx::query_as::<_, User>(&sql).bind(limit).bind(offset).fetch_all(pool).await
}

/// 治理动作：禁用/启用、笔记。`disabled=true` 时记下时间 —— 节点治理要看得出
/// 「什么时候被谁停的」，只留一个布尔位事后无从追。
pub async fn set_disabled(pool: &SqlitePool, id: &str, disabled: bool) -> Result<(), sqlx::Error> {
    let at = if disabled { Some(now_go()) } else { None };
    sqlx::query("UPDATE users SET disabled = ?, disabled_at = ? WHERE id = ?")
        .bind(disabled)
        .bind(at)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_note(pool: &SqlitePool, id: &str, note: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE users SET admin_note = ? WHERE id = ?")
        .bind(note)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
