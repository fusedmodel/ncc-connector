//! 远程执行任务（Remote Cloud Computer）的数据访问。
//!
//! 原实现：`ncc-registry/store/exec.go`。一次任务一行，**记录故意留得久** ——
//! 它是「这台机器替谁跑过什么」的账本（审计 + 排障），不像状态那样会被清理。
//!
//! 两个刻意的取舍：
//!
//! * `finish` 只认「还没结束」的行（`queued`/`running`）：后到的完成事件**不能**
//!   把 `canceled` 改回 `succeeded` —— 取消是用户意志（Go 那边是 `RowsAffected == 0`
//!   就回 `ErrRecordNotFound`，这里返回 `false`，语义一样）。
//! * `stale` 一把 UPDATE 收尾全部悬挂任务。Go 走的是 `ListExecRuns("", 200, 0)` 再逐条
//!   finish —— 一次只修 200 条，进程反复重启时会留下永远停在 `running` 的行。

use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::{now_go, parse_time};

/// 任务状态机：`queued → running → succeeded|failed|timeout|canceled`。
pub const EXEC_QUEUED: &str = "queued";
pub const EXEC_RUNNING: &str = "running";
pub const EXEC_SUCCEEDED: &str = "succeeded";
pub const EXEC_FAILED: &str = "failed";
pub const EXEC_TIMEOUT: &str = "timeout";
pub const EXEC_CANCELED: &str = "canceled";

const COLS: &str = "id, owner_id, requested_by, engine, kind, spec, image, package_id, \
     package_version, package_sha, work_dir, conn_id, log_path, reason, timeout_sec, status, \
     exit_code, log_bytes, log_truncated, error, created_at, started_at, finished_at";

/// 任务行（列名与既有库一一对应）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExecRunRow {
    pub id: String,
    pub owner_id: String,
    pub requested_by: String,
    pub engine: String,
    pub kind: String,
    pub spec: String,
    pub image: String,
    pub package_id: String,
    pub package_version: String,
    pub package_sha: String,
    pub work_dir: String,
    /// 非空 = 这次执行发生在某条**连接通道**上（会话层，见 `store::conn`）。
    pub conn_id: String,
    pub log_path: String,
    pub reason: String,
    pub timeout_sec: i64,
    pub status: String,
    pub exit_code: i64,
    pub log_bytes: i64,
    pub log_truncated: bool,
    pub error: String,
    pub created_at: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

impl ExecRunRow {
    /// 是否已结束（终态）。
    pub fn done(&self) -> bool {
        matches!(
            self.status.as_str(),
            EXEC_SUCCEEDED | EXEC_FAILED | EXEC_TIMEOUT | EXEC_CANCELED
        )
    }

    /// 耗时毫秒（没开始的一律 0；没结束的按「现在」算）。
    pub fn duration_ms(&self) -> i64 {
        let Some(start) = self.started_at.as_deref().and_then(parse_time) else {
            return 0;
        };
        let end = self
            .finished_at
            .as_deref()
            .and_then(parse_time)
            .unwrap_or_else(|| chrono::Local::now().fixed_offset());
        (end - start).num_milliseconds()
    }
}

/// 建任务的入参（`id` 留空则这里生成，与 Go 的 `store.NewID("ER")` 同形）。
///
/// 与 Go 的一个小差别：Go 的 `CreateExecRun` 不接受 id，于是**工作目录名**与
/// **任务 id** 是两个不同的随机串（handler 自己 `NewID` 一次建目录）。这里让两者
/// 共用一个 id —— 目录名是内部实现，不该是第二个身份。
#[derive(Debug, Clone, Default)]
pub struct ExecRunInput {
    pub id: String,
    pub owner_id: String,
    pub requested_by: String,
    pub engine: String,
    pub kind: String,
    pub spec: String,
    pub image: String,
    pub package_id: String,
    pub package_version: String,
    pub package_sha: String,
    pub work_dir: String,
    pub conn_id: String,
    pub log_path: String,
    pub reason: String,
    pub timeout_sec: i64,
}

pub async fn create(pool: &SqlitePool, in_: ExecRunInput) -> Result<ExecRunRow, sqlx::Error> {
    let row = ExecRunRow {
        id: if in_.id.is_empty() {
            new_id("ER")
        } else {
            in_.id
        },
        owner_id: in_.owner_id,
        requested_by: in_.requested_by,
        engine: in_.engine,
        kind: in_.kind,
        spec: in_.spec,
        image: in_.image,
        package_id: in_.package_id,
        package_version: in_.package_version,
        package_sha: in_.package_sha,
        work_dir: in_.work_dir,
        conn_id: in_.conn_id,
        log_path: in_.log_path,
        reason: in_.reason,
        timeout_sec: in_.timeout_sec,
        status: EXEC_QUEUED.to_string(),
        exit_code: 0,
        log_bytes: 0,
        log_truncated: false,
        error: String::new(),
        created_at: Some(now_go()),
        started_at: None,
        finished_at: None,
    };
    sqlx::query(
        "INSERT INTO exec_runs (id, owner_id, requested_by, engine, kind, spec, image, package_id,
         package_version, package_sha, work_dir, conn_id, log_path, reason, timeout_sec, status,
         exit_code, log_bytes, log_truncated, error, created_at, started_at, finished_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.id)
    .bind(&row.owner_id)
    .bind(&row.requested_by)
    .bind(&row.engine)
    .bind(&row.kind)
    .bind(&row.spec)
    .bind(&row.image)
    .bind(&row.package_id)
    .bind(&row.package_version)
    .bind(&row.package_sha)
    .bind(&row.work_dir)
    .bind(&row.conn_id)
    .bind(&row.log_path)
    .bind(&row.reason)
    .bind(row.timeout_sec)
    .bind(&row.status)
    .bind(row.exit_code)
    .bind(row.log_bytes)
    .bind(row.log_truncated)
    .bind(&row.error)
    .bind(&row.created_at)
    .bind(&row.started_at)
    .bind(&row.finished_at)
    .execute(pool)
    .await?;
    Ok(row)
}

pub async fn find(pool: &SqlitePool, id: &str) -> Result<Option<ExecRunRow>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM exec_runs WHERE id = ?");
    sqlx::query_as::<_, ExecRunRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// 列出任务。`owner_id` 为空 = 全部（管理员视角）。
pub async fn list(
    pool: &SqlitePool,
    owner_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<ExecRunRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 200 { 50 } else { limit };
    if owner_id.is_empty() {
        let sql = format!("SELECT {COLS} FROM exec_runs ORDER BY created_at DESC LIMIT ? OFFSET ?");
        sqlx::query_as::<_, ExecRunRow>(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await
    } else {
        let sql = format!(
            "SELECT {COLS} FROM exec_runs WHERE owner_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?"
        );
        sqlx::query_as::<_, ExecRunRow>(&sql)
            .bind(owner_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await
    }
}

pub async fn count(pool: &SqlitePool, owner_id: &str) -> Result<i64, sqlx::Error> {
    if owner_id.is_empty() {
        sqlx::query_scalar("SELECT COUNT(*) FROM exec_runs")
            .fetch_one(pool)
            .await
    } else {
        sqlx::query_scalar("SELECT COUNT(*) FROM exec_runs WHERE owner_id = ?")
            .bind(owner_id)
            .fetch_one(pool)
            .await
    }
}

/// 置为运行中（记开始时刻）。
pub async fn mark_running(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE exec_runs SET status = ?, started_at = ? WHERE id = ?")
        .bind(EXEC_RUNNING)
        .bind(now_go())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 落终态。返回是否真的改到了 —— `false` = 已经被取消（或本来就终态）。
pub async fn finish(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    exit_code: i64,
    log_bytes: i64,
    truncated: bool,
    error: &str,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE exec_runs SET status = ?, exit_code = ?, log_bytes = ?, log_truncated = ?,
         error = ?, finished_at = ? WHERE id = ? AND status IN (?, ?)",
    )
    .bind(status)
    .bind(exit_code)
    .bind(log_bytes)
    .bind(truncated)
    .bind(error)
    .bind(now_go())
    .bind(id)
    .bind(EXEC_QUEUED)
    .bind(EXEC_RUNNING)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// 取消：把还在排队/运行的行置为 canceled，返回是否真的改到了。
///
/// 进程的杀掉由调用方做（store 不该知道怎么 kill）。
pub async fn cancel(pool: &SqlitePool, id: &str, owner_id: &str) -> Result<bool, sqlx::Error> {
    let res = if owner_id.is_empty() {
        sqlx::query(
            "UPDATE exec_runs SET status = ?, finished_at = ?, error = ? WHERE id = ? AND status IN (?, ?)",
        )
        .bind(EXEC_CANCELED)
        .bind(now_go())
        .bind("用户取消")
        .bind(id)
        .bind(EXEC_QUEUED)
        .bind(EXEC_RUNNING)
        .execute(pool)
        .await?
    } else {
        sqlx::query(
            "UPDATE exec_runs SET status = ?, finished_at = ?, error = ? WHERE id = ? AND owner_id = ? AND status IN (?, ?)",
        )
        .bind(EXEC_CANCELED)
        .bind(now_go())
        .bind("用户取消")
        .bind(id)
        .bind(owner_id)
        .bind(EXEC_QUEUED)
        .bind(EXEC_RUNNING)
        .execute(pool)
        .await?
    };
    Ok(res.rows_affected() > 0)
}

/// 删记录（工作目录与日志由调用方清）。
pub async fn delete(pool: &SqlitePool, id: &str, owner_id: &str) -> Result<(), sqlx::Error> {
    if owner_id.is_empty() {
        sqlx::query("DELETE FROM exec_runs WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
    } else {
        sqlx::query("DELETE FROM exec_runs WHERE id = ? AND owner_id = ?")
            .bind(id)
            .bind(owner_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// 启动时收尾：把上一进程里中断的任务标成 failed。
///
/// 不这么做的话，重启后那些行会永远停在 running —— 调用方会一直等一个已经不存在
/// 的进程（`ncc sandbox run` 会等到超时才发现没人回话）。
///
/// 注：`main.rs` 属本批次不让改的文件，所以这个函数暂时只由测试调用；
/// 接线由整体收口的 agent 做（与 Go 的 `staleExecRuns` 同一位置）。
#[allow(dead_code)]
pub async fn stale(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE exec_runs SET status = ?, exit_code = -1, log_bytes = 0, log_truncated = 0,
         error = ?, finished_at = ? WHERE status IN (?, ?)",
    )
    .bind(EXEC_FAILED)
    .bind("节点重启：任务在上一进程里中断")
    .bind(now_go())
    .bind(EXEC_QUEUED)
    .bind(EXEC_RUNNING)
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时文件库：**不用 `sqlite::memory:`**（同一进程里多个测试会互相打架）。
    async fn pool(tag: &str) -> SqlitePool {
        let dir = std::env::temp_dir().join(format!("ncc-exec-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL)
            .await
            .unwrap();
        p
    }

    fn input(owner: &str, conn: &str) -> ExecRunInput {
        ExecRunInput {
            owner_id: owner.to_string(),
            requested_by: "a@x.com".to_string(),
            engine: "process".to_string(),
            kind: "cmd".to_string(),
            spec: "echo hi".to_string(),
            work_dir: "/w".to_string(),
            conn_id: conn.to_string(),
            log_path: "/w/output.log".to_string(),
            reason: "测试".to_string(),
            timeout_sec: 60,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn 建任务_查找_列出与计数() {
        let p = pool("crud").await;
        let a = create(&p, input("U-1", "")).await.unwrap();
        let b = create(&p, input("U-2", "CN-1")).await.unwrap();
        assert!(a.id.starts_with("ER-"));
        assert_eq!(a.status, EXEC_QUEUED);
        assert!(!a.done());
        assert_eq!(a.duration_ms(), 0);
        assert!(!a.log_truncated);

        assert_eq!(find(&p, &a.id).await.unwrap().unwrap().owner_id, "U-1");
        assert!(find(&p, "ER-nope").await.unwrap().is_none());

        assert_eq!(list(&p, "U-1", 50, 0).await.unwrap().len(), 1);
        assert_eq!(list(&p, "", 50, 0).await.unwrap().len(), 2);
        assert_eq!(count(&p, "U-2").await.unwrap(), 1);
        assert_eq!(count(&p, "").await.unwrap(), 2);
        // limit 越界回落 50（不报错）
        assert_eq!(list(&p, "", 999, 0).await.unwrap().len(), 2);
        // 连接通道上的任务能被 conn_id 捞回来
        assert_eq!(b.conn_id, "CN-1");
    }

    #[tokio::test]
    async fn 终态与取消_取消不能被完成事件覆盖() {
        let p = pool("finish").await;
        let r = create(&p, input("U-1", "")).await.unwrap();
        mark_running(&p, &r.id).await.unwrap();
        let running = find(&p, &r.id).await.unwrap().unwrap();
        assert_eq!(running.status, EXEC_RUNNING);
        assert!(running.started_at.is_some());

        assert!(cancel(&p, &r.id, "").await.unwrap());
        let canceled = find(&p, &r.id).await.unwrap().unwrap();
        assert_eq!(canceled.status, EXEC_CANCELED);
        assert_eq!(canceled.error, "用户取消");
        assert!(canceled.done());
        assert!(canceled.finished_at.is_some());

        // 后到的完成事件不许把 canceled 改回 succeeded
        assert!(!finish(&p, &r.id, EXEC_SUCCEEDED, 0, 3, false, "")
            .await
            .unwrap());
        assert_eq!(
            find(&p, &r.id).await.unwrap().unwrap().status,
            EXEC_CANCELED
        );
        // 重复取消也是 false（已经不是活跃状态）
        assert!(!cancel(&p, &r.id, "").await.unwrap());
    }

    #[tokio::test]
    async fn 取消按归属过滤_管理员视角可动全部() {
        let p = pool("cancel").await;
        let r = create(&p, input("U-1", "")).await.unwrap();
        assert!(!cancel(&p, &r.id, "U-2").await.unwrap());
        assert_eq!(find(&p, &r.id).await.unwrap().unwrap().status, EXEC_QUEUED);
        assert!(cancel(&p, &r.id, "U-1").await.unwrap());
        assert_eq!(
            find(&p, &r.id).await.unwrap().unwrap().status,
            EXEC_CANCELED
        );

        let r2 = create(&p, input("U-1", "")).await.unwrap();
        assert!(cancel(&p, &r2.id, "").await.unwrap());
    }

    #[tokio::test]
    async fn 收尾悬挂任务与删除记录() {
        let p = pool("stale").await;
        let r = create(&p, input("U-1", "")).await.unwrap();
        let r2 = create(&p, input("U-1", "")).await.unwrap();
        mark_running(&p, &r2.id).await.unwrap();
        let done = create(&p, input("U-1", "")).await.unwrap();
        finish(&p, &done.id, EXEC_SUCCEEDED, 0, 5, true, "")
            .await
            .unwrap();

        assert_eq!(stale(&p).await.unwrap(), 2);
        let a = find(&p, &r.id).await.unwrap().unwrap();
        assert_eq!(a.status, EXEC_FAILED);
        assert_eq!(a.exit_code, -1);
        assert_eq!(a.error, "节点重启：任务在上一进程里中断");
        // 已结束的不动
        let fin = find(&p, &done.id).await.unwrap().unwrap();
        assert_eq!(fin.status, EXEC_SUCCEEDED);
        assert!(fin.log_truncated);
        assert!(fin.duration_ms() >= 0);

        delete(&p, &a.id, "").await.unwrap();
        assert!(find(&p, &a.id).await.unwrap().is_none());
        // 归属不符删不掉
        delete(&p, &done.id, "U-9").await.unwrap();
        assert!(find(&p, &done.id).await.unwrap().is_some());
    }
}
