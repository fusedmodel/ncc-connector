//! 数据库连接与建表。
//!
//! 内网节点与平台的默认形态都是「一个二进制 + 一个 SQLite 文件」，
//! 打开方式对齐 Go 侧 glebarez/sqlite 的默认行为：WAL、外键打开、忙等超时、
//! **单连接**（SQLite 是单写者，连接池 >1 会让「先查后写」的路径出现竞态）。
//!
//! `migrate` 做两件事，缺一不可：
//!
//! 1. **建表**：DDL 从 Go 服务（GORM AutoMigrate）建出的真实库上 dump，逐字对齐；
//! 2. **补列**：老库缺的列用 `ALTER TABLE ADD COLUMN` 补上。
//!
//! 第 2 条是「能开在旧数据库上」的关键：`CREATE TABLE IF NOT EXISTS` 对已存在的表
//! 什么都不做，只做第 1 步的话，一旦上游给某张表加过列，Rust 服务开在旧库上就会在
//! 第一条查询上报「no such column」——而这恰恰是这次重写最不能出的事。
//! Go 侧 GORM 的 AutoMigrate 也是这么做的，两边行为因此一致。

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

/// 打开（必要时创建）SQLite 库。
pub async fn open_sqlite(path: impl AsRef<Path>) -> Result<SqlitePool, String> {
    let path = path.as_ref();
    crate::env::ensure_parent(path).map_err(|e| format!("创建库文件目录失败: {e}"))?;

    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(opts)
        .await
        .map_err(|e| format!("打开数据库失败（{}）: {e}", path.display()))?;
    Ok(pool)
}

/// 建表 + 补列 + 建索引。DDL 里语句之间用 `;` 分隔。
///
/// 三个阶段必须按顺序：先建表 → 再补列 → 最后建索引。
/// 反过来会在老库上炸：索引引用的新列这时还没补上，`CREATE INDEX` 直接报
/// 「no such column」——而这条索引本来就是给新列建的。
pub async fn migrate(pool: &SqlitePool, ddl: &str) -> Result<(), String> {
    let statements = split_statements(ddl);

    // 阶段一：建表（对老库是空操作）
    for stmt in &statements {
        if is_create_table(stmt) {
            exec(pool, stmt).await?;
        }
    }
    // 阶段二：补列
    for stmt in &statements {
        if let Some((table, cols)) = parse_create_table(stmt) {
            add_missing_columns(pool, &table, &cols).await?;
        }
    }
    // 阶段三：建索引
    for stmt in &statements {
        if !is_create_table(stmt) {
            exec(pool, stmt).await?;
        }
    }
    Ok(())
}

fn is_create_table(stmt: &str) -> bool {
    stmt.trim().to_uppercase().starts_with("CREATE TABLE")
}

async fn exec(pool: &SqlitePool, stmt: &str) -> Result<(), String> {
    sqlx::query(stmt)
        .execute(pool)
        .await
        .map_err(|e| format!("执行失败（{stmt}）: {e}"))?;
    Ok(())
}

/// 按 `;` 切分 DDL。
///
/// SQLite 里没有存储过程 / 触发器体这种含分号的语句，简单切分在这里是安全的
/// （本项目自己的 schema 只有 CREATE TABLE / CREATE INDEX）。
fn split_statements(ddl: &str) -> Vec<String> {
    ddl.split(';')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// 从一条 `CREATE TABLE` 里抠出表名与「列定义」列表。
///
/// 表级约束（PRIMARY KEY / UNIQUE / FOREIGN KEY / CONSTRAINT / CHECK）不是列，跳过 ——
/// 拿去做 `ADD COLUMN` 会是非法 SQL。
fn parse_create_table(stmt: &str) -> Option<(String, Vec<(String, String)>)> {
    let upper = stmt.to_uppercase();
    if !upper.starts_with("CREATE TABLE") {
        return None;
    }
    let open = stmt.find('(')?;
    let close = stmt.rfind(')')?;
    if close <= open {
        return None;
    }
    let name = stmt[..open]
        .split_whitespace()
        .last()?
        .trim_matches(|c| c == '`' || c == '"' || c == '\'')
        .to_string();
    if name.is_empty() {
        return None;
    }

    let mut cols = Vec::new();
    for part in split_top_level(&stmt[open + 1..close]) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let pu = part.to_uppercase();
        if pu.starts_with("PRIMARY KEY")
            || pu.starts_with("UNIQUE")
            || pu.starts_with("FOREIGN KEY")
            || pu.starts_with("CONSTRAINT")
            || pu.starts_with("CHECK")
        {
            continue;
        }
        // 内联主键列不能靠 ALTER 补（SQLite 限制），而且老库里它一定已经在
        if pu.contains("PRIMARY KEY") {
            continue;
        }
        let col = match part.find('`') {
            Some(i) => match part[i + 1..].find('`') {
                Some(j) => part[i + 1..i + 1 + j].to_string(),
                None => continue,
            },
            None => match part.split_whitespace().next() {
                Some(w) => w.trim_matches('"').to_string(),
                None => continue,
            },
        };
        cols.push((col, part.to_string()));
    }
    Some((name, cols))
}

/// 按括号深度切分（列定义里可能带 `(...)`，例如 `numeric NOT NULL DEFAULT (0)`；
/// 也可能带字符串里的逗号）。
fn split_top_level(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut prev = '\0';
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '\'' if prev != '\\' => in_str = !in_str,
            '(' if !in_str => depth += 1,
            ')' if !in_str => depth -= 1,
            ',' if depth == 0 && !in_str => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        prev = c;
    }
    out.push(&s[start..]);
    out
}

/// 补上老库缺的列。
///
/// 补不上的（例如 `NOT NULL` 且无默认值、而表里已有数据）只记警告、不中断启动：
/// 让服务先起来并把问题说清楚，比因为一个补列失败就整体不可用要好。
async fn add_missing_columns(
    pool: &SqlitePool,
    table: &str,
    cols: &[(String, String)],
) -> Result<(), String> {
    let rows = sqlx::query(&format!("PRAGMA table_info(`{table}`)"))
        .fetch_all(pool)
        .await
        .map_err(|e| format!("读取表结构失败（{table}）: {e}"))?;
    if rows.is_empty() {
        return Ok(()); // DDL 里没有这张表（或没建成）
    }
    let existing: HashSet<String> = rows.iter().map(|r| r.get::<String, _>("name")).collect();

    for (col, def) in cols {
        if existing.contains(col) {
            continue;
        }
        if let Err(e) = sqlx::query(&format!("ALTER TABLE `{table}` ADD COLUMN {def}"))
            .execute(pool)
            .await
        {
            tracing::warn!("补列失败 {table}.{col}：{e}（老库里没有这一列，涉及它的查询会报错）");
        } else {
            tracing::info!("已补列 {table}.{col}（老库缺这一列）");
        }
    }
    Ok(())
}

/// 字节目录是否可用（启动自检：写不进去就不该假装服务正常）。
pub fn blob_dir_writable(dir: &Path) -> bool {
    crate::storage::LocalStorage::new(dir, "", "blobs").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时库：`sqlite::memory:` 在同一进程里会被多个测试共享，
    /// 建表/插入互相打架（表现为莫名其妙的 UNIQUE 冲突），所以测试用真文件。
    async fn test_pool(tag: &str) -> (SqlitePool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ncc-pool-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("t.db");
        (open_sqlite(&db).await.unwrap(), dir)
    }

    #[tokio::test]
    async fn 建表与读写() {
        let (pool, dir) = test_pool("crud").await;
        migrate(
            &pool,
            "CREATE TABLE IF NOT EXISTS t (id text PRIMARY KEY, n integer)",
        )
        .await
        .unwrap();
        sqlx::query("INSERT INTO t (id, n) VALUES (?, ?)")
            .bind("a")
            .bind(7)
            .execute(&pool)
            .await
            .unwrap();
        let n: i64 = sqlx::query_scalar("SELECT n FROM t WHERE id = ?")
            .bind("a")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 7);
        // 重复 migrate 不该报错（幂等）
        migrate(
            &pool,
            "CREATE TABLE IF NOT EXISTS t (id text PRIMARY KEY, n integer)",
        )
        .await
        .unwrap();
        drop(pool);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn 老库缺列会被补上且旧数据还在() {
        let (pool, dir) = test_pool("addcol").await;
        // 模拟「Go 老版本建的库」：只有两列
        sqlx::query("CREATE TABLE items (id text PRIMARY KEY, name text NOT NULL)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO items (id, name) VALUES ('a', '旧数据')")
            .execute(&pool)
            .await
            .unwrap();

        // 新版 DDL 多了两列（一列带默认值，一列允许 NULL）+ 一个新索引
        migrate(
            &pool,
            "CREATE TABLE IF NOT EXISTS `items` (`id` text, `name` text NOT NULL, \
             `scan` integer NOT NULL DEFAULT 0, `note` text DEFAULT null, PRIMARY KEY (`id`));\
             CREATE INDEX IF NOT EXISTS `idx_items_scan` ON `items`(`scan`);",
        )
        .await
        .unwrap();

        let cols: HashSet<String> = sqlx::query("PRAGMA table_info(`items`)")
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();
        assert!(cols.contains("scan"));
        assert!(cols.contains("note"));

        let (name, scan): (String, i64) =
            sqlx::query_as("SELECT name, scan FROM items WHERE id = 'a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(name, "旧数据");
        assert_eq!(scan, 0);
        drop(pool);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 解析建表语句() {
        let (table, cols) = parse_create_table(
            "CREATE TABLE IF NOT EXISTS `t` (`id` text, `a` integer NOT NULL DEFAULT 0, \
             `b` text DEFAULT null, PRIMARY KEY (`id`), UNIQUE (`a`))",
        )
        .unwrap();
        assert_eq!(table, "t");
        // `id` 是普通列（主键是表级约束），所以也在待补列里 —— 它一定已存在，
        // 补列时会被 PRAGMA 判断跳过；真正要过滤掉的是表级约束本身。
        let names: Vec<&str> = cols.iter().map(|(c, _)| c.as_str()).collect();
        assert_eq!(names, vec!["id", "a", "b"]);
        assert!(parse_create_table("CREATE INDEX i ON t(a)").is_none());
    }

    #[test]
    fn 括号与字符串里的逗号不切分() {
        let parts = split_top_level("a numeric NOT NULL DEFAULT (0), b text DEFAULT 'x,y'");
        assert_eq!(parts.len(), 2);
    }
}
