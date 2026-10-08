//! 连接（通道）的会话状态。
//!
//! 原实现：`ncc-registry/store/conn.go`。**记录留久**：它是「谁在这台机器上开过通道、
//! 跑过什么」的账本，不像任务那样可以随手清。
//!
//! 一个刻意的取舍（照搬 Go 的注释，因为它是个坑）：**过期不写状态**。
//! 「过期」是**推导**出来的（`ExpiresAt` 到了即过期，见 `ConnRow::state_at`），
//! 不是一次状态翻转 —— 早期实现把它们改写成 `closed`，结果「过期」和「被人关掉」
//! 在接口上分不出来（410 的文案、列表里的状态都一样），而对用户这两件事意义完全不同：
//! 一个是「等一等 / 重开一条」，一个是「别人把你的门关了」。

use chrono::{DateTime, FixedOffset, Local};

use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::{format_go, now_go, parse_time};

/// 只有 `open | closed` 两种落库状态；`expired` 是推导出来的。
pub const CONN_OPEN: &str = "open";
pub const CONN_CLOSED: &str = "closed";

const COLS: &str = "id, owner_id, requested_by, name, note, work_dir, peer, status, ttl_sec, \
     exec_count, bytes_up, bytes_down, pull_count, expires_at, last_used_at, created_at, closed_at";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ConnRow {
    pub id: String,
    pub owner_id: String,
    pub requested_by: String,
    pub name: String,
    pub note: String,
    /// 这条通道的工作目录（每条连接一个，文件推拉的边界也在这里）。
    pub work_dir: String,
    /// 建连时对方自报的身份（nodeId / product），便于审计里认人。
    pub peer: String,
    pub status: String,
    pub ttl_sec: i64,
    /// 计数：这条通道上发生过什么（列表里一眼看懂）。
    pub exec_count: i64,
    pub bytes_up: i64,
    pub bytes_down: i64,
    pub pull_count: i64,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: Option<String>,
    pub closed_at: Option<String>,
}

impl ConnRow {
    fn expires_dt(&self) -> Option<DateTime<FixedOffset>> {
        self.expires_at.as_deref().and_then(parse_time)
    }

    /// 通道现在可用吗（Go 的 `Conn.Open`）。
    ///
    /// `expires_at` 读不出来时按「已过期」处理：Go 里零值时间 `now.Before(zero)` 为假，
    /// 于是一条脏数据不会变成永久可用的通道。
    pub fn open_at(&self, now: DateTime<FixedOffset>) -> bool {
        let before = self.expires_dt().map(|e| now < e).unwrap_or(false);
        self.status == CONN_OPEN && before
    }

    /// 人读状态：`open | closed | expired`。
    ///
    /// 顺序要紧：`closed` 优先 —— 被关掉的通道即使 TTL 也过了，用户该看到的是
    /// 「有人把门关了」，而不是「等一等就好」。
    pub fn state_at(&self, now: DateTime<FixedOffset>) -> &'static str {
        if self.status == CONN_CLOSED {
            return CONN_CLOSED;
        }
        let before = self.expires_dt().map(|e| now < e).unwrap_or(false);
        if !before {
            return "expired";
        }
        CONN_OPEN
    }
}

/// 建连的入参（`ExpiresAt` 由调用方按 TTL 算好）。
#[derive(Debug, Clone, Default)]
pub struct ConnInput {
    pub id: String,
    pub owner_id: String,
    pub requested_by: String,
    pub name: String,
    pub note: String,
    pub work_dir: String,
    pub peer: String,
    pub ttl_sec: i64,
    /// 过期时刻（GORM 时间串）。
    pub expires_at: Option<String>,
}

pub async fn create(pool: &SqlitePool, in_: ConnInput) -> Result<ConnRow, sqlx::Error> {
    let row = ConnRow {
        id: if in_.id.is_empty() {
            new_id("CN")
        } else {
            in_.id
        },
        owner_id: in_.owner_id,
        requested_by: in_.requested_by,
        name: in_.name,
        note: in_.note,
        work_dir: in_.work_dir,
        peer: in_.peer,
        status: CONN_OPEN.to_string(),
        ttl_sec: in_.ttl_sec,
        exec_count: 0,
        bytes_up: 0,
        bytes_down: 0,
        pull_count: 0,
        expires_at: in_.expires_at,
        last_used_at: None,
        created_at: Some(now_go()),
        closed_at: None,
    };
    sqlx::query(
        "INSERT INTO conns (id, owner_id, requested_by, name, note, work_dir, peer, status, ttl_sec,
         exec_count, bytes_up, bytes_down, pull_count, expires_at, last_used_at, created_at, closed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.id)
    .bind(&row.owner_id)
    .bind(&row.requested_by)
    .bind(&row.name)
    .bind(&row.note)
    .bind(&row.work_dir)
    .bind(&row.peer)
    .bind(&row.status)
    .bind(row.ttl_sec)
    .bind(row.exec_count)
    .bind(row.bytes_up)
    .bind(row.bytes_down)
    .bind(row.pull_count)
    .bind(&row.expires_at)
    .bind(&row.last_used_at)
    .bind(&row.created_at)
    .bind(&row.closed_at)
    .execute(pool)
    .await?;
    Ok(row)
}

pub async fn find(pool: &SqlitePool, id: &str) -> Result<Option<ConnRow>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM conns WHERE id = ?");
    sqlx::query_as::<_, ConnRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

/// 列通道。`owner_id` 为空 = 全部（管理员视角）。
pub async fn list(
    pool: &SqlitePool,
    owner_id: &str,
    limit: i64,
    offset: i64,
) -> Result<Vec<ConnRow>, sqlx::Error> {
    let limit = if limit <= 0 || limit > 200 { 50 } else { limit };
    if owner_id.is_empty() {
        let sql = format!("SELECT {COLS} FROM conns ORDER BY created_at DESC LIMIT ? OFFSET ?");
        sqlx::query_as::<_, ConnRow>(&sql)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await
    } else {
        let sql = format!(
            "SELECT {COLS} FROM conns WHERE owner_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?"
        );
        sqlx::query_as::<_, ConnRow>(&sql)
            .bind(owner_id)
            .bind(limit)
            .bind(offset)
            .fetch_all(pool)
            .await
    }
}

/// 记一次使用（每次 exec / push / pull 都刷一次，过期判定才有意义）。
///
/// 只给非零的计数加：`touch(…, 0, 0, 0, 0)` 是「只刷 last_used_at」，
/// 不该顺手把四个计数都写一遍（无谓的行写 + 并发下的额外争用）。
pub async fn touch(
    pool: &SqlitePool,
    id: &str,
    exec_delta: i64,
    up_delta: i64,
    down_delta: i64,
    pull_delta: i64,
) -> Result<(), sqlx::Error> {
    let mut sql = String::from("UPDATE conns SET last_used_at = ?");
    if exec_delta != 0 {
        sql.push_str(", exec_count = exec_count + ?");
    }
    if up_delta != 0 {
        sql.push_str(", bytes_up = bytes_up + ?");
    }
    if down_delta != 0 {
        sql.push_str(", bytes_down = bytes_down + ?");
    }
    if pull_delta != 0 {
        sql.push_str(", pull_count = pull_count + ?");
    }
    sql.push_str(" WHERE id = ?");

    let mut q = sqlx::query(&sql).bind(now_go());
    if exec_delta != 0 {
        q = q.bind(exec_delta);
    }
    if up_delta != 0 {
        q = q.bind(up_delta);
    }
    if down_delta != 0 {
        q = q.bind(down_delta);
    }
    if pull_delta != 0 {
        q = q.bind(pull_delta);
    }
    q.bind(id).execute(pool).await?;
    Ok(())
}

/// 关闭通道。`owner_id` 为空 = 管理员（可关任何人的）。
///
/// 条件里带 `status = open`：重复关闭不该把已经关掉的行再写一遍时间戳，
/// 也不该把「已经关了」当失败报给用户（调用方用返回值判断是不是本来开着的）。
pub async fn close(pool: &SqlitePool, id: &str, owner_id: &str) -> Result<bool, sqlx::Error> {
    let res = if owner_id.is_empty() {
        sqlx::query("UPDATE conns SET status = ?, closed_at = ? WHERE id = ? AND status = ?")
            .bind(CONN_CLOSED)
            .bind(now_go())
            .bind(id)
            .bind(CONN_OPEN)
            .execute(pool)
            .await?
    } else {
        sqlx::query(
            "UPDATE conns SET status = ?, closed_at = ? WHERE id = ? AND owner_id = ? AND status = ?",
        )
        .bind(CONN_CLOSED)
        .bind(now_go())
        .bind(id)
        .bind(owner_id)
        .bind(CONN_OPEN)
        .execute(pool)
        .await?
    };
    Ok(res.rows_affected() > 0)
}

/// 数一下「TTL 到了但状态还写着 open」的通道。**只数不改**（见文件头）。
pub async fn count_expired(
    pool: &SqlitePool,
    now: DateTime<FixedOffset>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM conns WHERE status = ? AND expires_at < ?")
        .bind(CONN_OPEN)
        .bind(format_go(now))
        .fetch_one(pool)
        .await
}

/// 删通道记录（Go 里这个方法也**没有调用方** —— 通道是账本，正常路径只关不删；
/// 留着是为了与 `Store` 的接口对齐，运维脚本可能直接用它）。
#[allow(dead_code)]
pub async fn delete(pool: &SqlitePool, id: &str, owner_id: &str) -> Result<(), sqlx::Error> {
    if owner_id.is_empty() {
        sqlx::query("DELETE FROM conns WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
    } else {
        sqlx::query("DELETE FROM conns WHERE id = ? AND owner_id = ?")
            .bind(id)
            .bind(owner_id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

/// 启动时数一下已经过期的通道（只打日志）。
///
/// 状态是推导的，所以这里**没有需要修复的数据** —— 只是重启后给运维一个告警：
/// 「你这台机器上还挂着 N 条 TTL 已过的通道」。
///
/// 注：`main.rs` 属本批次不让改的文件，所以这个函数暂时只由测试调用；
/// 接线由整体收口的 agent 做（与 Go 的 `staleConns` 同一位置）。
#[allow(dead_code)]
pub async fn count_expired_now(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    count_expired(pool, Local::now().fixed_offset()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    async fn pool(tag: &str) -> SqlitePool {
        let dir = std::env::temp_dir().join(format!("ncc-conn-{}-{tag}", std::process::id()));
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

    fn input(owner: &str, id: &str, ttl: i64) -> ConnInput {
        ConnInput {
            id: id.to_string(),
            owner_id: owner.to_string(),
            requested_by: "a@x.com".to_string(),
            name: "演示通道".to_string(),
            note: "n".to_string(),
            work_dir: format!("/w/{id}"),
            peer: "node/ND-1".to_string(),
            ttl_sec: ttl,
            expires_at: Some(format_go(
                Local::now().fixed_offset() + Duration::seconds(ttl),
            )),
        }
    }

    #[tokio::test]
    async fn 建通道_查找_列出与状态推导() {
        let p = pool("crud").await;
        let c = create(&p, input("U-1", "CN-1", 3600)).await.unwrap();
        assert_eq!(c.status, CONN_OPEN);
        assert_eq!(c.ttl_sec, 3600);
        assert_eq!(c.exec_count, 0);
        let now = Local::now().fixed_offset();
        assert!(c.open_at(now));
        assert_eq!(c.state_at(now), "open");

        // 空 id 自动生成（与 Go 的 store.NewID("CN") 同形）
        let auto = create(&p, input("U-1", "", 60)).await.unwrap();
        assert!(auto.id.starts_with("CN-"));

        assert_eq!(find(&p, "CN-1").await.unwrap().unwrap().name, "演示通道");
        assert!(find(&p, "CN-nope").await.unwrap().is_none());
        assert_eq!(list(&p, "U-1", 50, 0).await.unwrap().len(), 2);
        assert_eq!(list(&p, "U-2", 50, 0).await.unwrap().len(), 0);
        assert_eq!(list(&p, "", 50, 0).await.unwrap().len(), 2);

        // 过期是推导的：库里状态还是 open，但 state 已经是 expired
        let expired = create(&p, input("U-1", "CN-OLD", -10)).await.unwrap();
        assert!(!expired.open_at(now));
        assert_eq!(expired.state_at(now), "expired");
        assert_eq!(find(&p, "CN-OLD").await.unwrap().unwrap().status, CONN_OPEN);
        assert_eq!(count_expired(&p, now).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn 关闭幂等且不能覆盖过期语义() {
        let p = pool("close").await;
        create(&p, input("U-1", "CN-1", 3600)).await.unwrap();
        assert!(close(&p, "CN-1", "").await.unwrap());
        let now = Local::now().fixed_offset();
        let c = find(&p, "CN-1").await.unwrap().unwrap();
        assert_eq!(c.status, CONN_CLOSED);
        assert!(c.closed_at.is_some());
        assert!(!c.open_at(now));
        assert_eq!(c.state_at(now), "closed");
        // 重复关闭：不算改到（时间戳不重写）
        assert!(!close(&p, "CN-1", "").await.unwrap());

        // 归属不符关不掉
        create(&p, input("U-1", "CN-2", 3600)).await.unwrap();
        assert!(!close(&p, "CN-2", "U-9").await.unwrap());
        assert!(close(&p, "CN-2", "U-1").await.unwrap());

        // 关掉 + 过期：状态仍说 closed（谁关的比 TTL 更该被看到）
        create(&p, input("U-1", "CN-3", -10)).await.unwrap();
        close(&p, "CN-3", "").await.unwrap();
        assert_eq!(
            find(&p, "CN-3").await.unwrap().unwrap().state_at(now),
            "closed"
        );
    }

    #[tokio::test]
    async fn 使用计数_只加非零项() {
        let p = pool("touch").await;
        create(&p, input("U-1", "CN-1", 3600)).await.unwrap();
        touch(&p, "CN-1", 0, 0, 0, 0).await.unwrap();
        let c = find(&p, "CN-1").await.unwrap().unwrap();
        assert!(c.last_used_at.is_some());
        assert_eq!(
            (c.exec_count, c.bytes_up, c.bytes_down, c.pull_count),
            (0, 0, 0, 0)
        );

        touch(&p, "CN-1", 1, 100, 0, 0).await.unwrap();
        touch(&p, "CN-1", 2, 0, 50, 3).await.unwrap();
        let c = find(&p, "CN-1").await.unwrap().unwrap();
        assert_eq!(c.exec_count, 3);
        assert_eq!(c.bytes_up, 100);
        assert_eq!(c.bytes_down, 50);
        assert_eq!(c.pull_count, 3);
    }

    #[tokio::test]
    async fn 清理过期通道与删除_归属过滤() {
        let p = pool("del").await;
        create(&p, input("U-1", "CN-1", -1)).await.unwrap();
        create(&p, input("U-1", "CN-2", 3600)).await.unwrap();
        assert_eq!(count_expired_now(&p).await.unwrap(), 1);

        delete(&p, "CN-2", "U-9").await.unwrap();
        assert!(find(&p, "CN-2").await.unwrap().is_some());
        delete(&p, "CN-2", "U-1").await.unwrap();
        assert!(find(&p, "CN-2").await.unwrap().is_none());
    }
}
