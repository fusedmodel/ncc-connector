//! 授权（Grant）：把「私有制品 / 私有节点 / 配置 / P2P」的使用权授给某个人。
//!
//! 四种 kind 各自独立，不互相蕴含 —— `artifact` 授权不等于能看 `config`。
//! 一个授权动作只应对应一件事。

use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::now_go;

use super::exists;

pub const KIND_ARTIFACT: &str = "artifact";
pub const KIND_NODE: &str = "node";
pub const KIND_CONFIG: &str = "config";
pub const KIND_TRACE: &str = "trace";
pub const KIND_STATE: &str = "state";

pub fn valid_kind(k: &str) -> bool {
    // 与 Go 的 `model.GrantKinds` 逐项一致：`trace`（运行轨迹）与 `state`
    // （三样状态：知识库 / 记忆 / 检查点）是轨迹族与状态族读取私有内容时真正会查的种类，
    // 少一个就会出现「能授权却不能建」的怪事（`ncc grant set --kind trace` 被 400 拒）。
    matches!(
        k,
        KIND_ARTIFACT | KIND_NODE | KIND_CONFIG | KIND_TRACE | KIND_STATE
    )
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Grant {
    pub id: String,
    pub owner_id: String,
    pub grantee_user_id: String,
    pub kind: String,
    pub namespace_id: String,
    pub note: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

const COLS: &str =
    "id, owner_id, grantee_user_id, kind, namespace_id, note, created_at, updated_at";

/// 我发出的授权。
pub async fn list_owned(pool: &SqlitePool, owner_id: &str) -> Result<Vec<Grant>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM grants WHERE owner_id = ? ORDER BY created_at DESC");
    sqlx::query_as::<_, Grant>(&sql)
        .bind(owner_id)
        .fetch_all(pool)
        .await
}

/// 我收到的授权。
pub async fn list_received(pool: &SqlitePool, grantee_id: &str) -> Result<Vec<Grant>, sqlx::Error> {
    let sql =
        format!("SELECT {COLS} FROM grants WHERE grantee_user_id = ? ORDER BY created_at DESC");
    sqlx::query_as::<_, Grant>(&sql)
        .bind(grantee_id)
        .fetch_all(pool)
        .await
}

pub async fn create(
    pool: &SqlitePool,
    owner_id: &str,
    grantee_user_id: &str,
    kind: &str,
    namespace_id: &str,
    note: &str,
) -> Result<Grant, sqlx::Error> {
    let now = now_go();
    let g = Grant {
        id: new_id("G"),
        owner_id: owner_id.to_string(),
        grantee_user_id: grantee_user_id.to_string(),
        kind: kind.to_string(),
        namespace_id: namespace_id.to_string(),
        note: note.to_string(),
        created_at: Some(now.clone()),
        updated_at: Some(now),
    };
    sqlx::query(
        "INSERT INTO grants (id, owner_id, grantee_user_id, kind, namespace_id, note, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&g.id)
    .bind(&g.owner_id)
    .bind(&g.grantee_user_id)
    .bind(&g.kind)
    .bind(&g.namespace_id)
    .bind(&g.note)
    .bind(&g.created_at)
    .bind(&g.updated_at)
    .execute(pool)
    .await?;
    Ok(g)
}

pub async fn delete(pool: &SqlitePool, id: &str, owner_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM grants WHERE id = ? AND owner_id = ?")
        .bind(id)
        .bind(owner_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 是否已授权。`namespace_id` 为空 = 不限命名空间的全局授权。
pub async fn has(
    pool: &SqlitePool,
    owner_id: &str,
    grantee_id: &str,
    kind: &str,
    namespace_id: &str,
) -> bool {
    exists(
        pool,
        "SELECT COUNT(*) FROM grants WHERE owner_id = ? AND grantee_user_id = ? AND kind = ? AND (namespace_id = '' OR namespace_id = ?)",
        &[owner_id, grantee_id, kind, namespace_id],
    )
    .await
    .unwrap_or(false)
}

/// 拿到过某种授权的机主列表（让他们的私有节点出现在我的 discover 里）。
pub async fn granted_owners(pool: &SqlitePool, grantee_id: &str, kind: &str) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT DISTINCT owner_id FROM grants WHERE grantee_user_id = ? AND kind = ?",
    )
    .bind(grantee_id)
    .bind(kind)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 授权与判断() {
        let p = SqlitePool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL)
            .await
            .unwrap();
        let g = create(&p, "U-1", "U-2", KIND_ARTIFACT, "NS-1", "")
            .await
            .unwrap();
        assert!(has(&p, "U-1", "U-2", KIND_ARTIFACT, "NS-1").await);
        // 限定命名空间的授权不外溢到别的命名空间
        assert!(!has(&p, "U-1", "U-2", KIND_ARTIFACT, "NS-OTHER").await);
        // 不限命名空间（空串）的授权对任意命名空间都成立
        let global = create(&p, "U-1", "U-2", KIND_ARTIFACT, "", "")
            .await
            .unwrap();
        assert!(has(&p, "U-1", "U-2", KIND_ARTIFACT, "NS-OTHER").await);
        delete(&p, &global.id, "U-1").await.unwrap();
        // kind 之间不互相蕴含
        assert!(!has(&p, "U-1", "U-2", KIND_NODE, "NS-1").await);
        assert_eq!(
            granted_owners(&p, "U-2", KIND_ARTIFACT).await,
            vec!["U-1".to_string()]
        );
        delete(&p, &g.id, "U-2").await.unwrap();
        assert!(has(&p, "U-1", "U-2", KIND_ARTIFACT, "NS-1").await);
        delete(&p, &g.id, "U-1").await.unwrap();
        assert!(!has(&p, "U-1", "U-2", KIND_ARTIFACT, "NS-1").await);
    }
}
