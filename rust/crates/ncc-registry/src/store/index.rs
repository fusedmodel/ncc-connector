//! 平台推来的索引副本（NCC Index）的数据访问。
//!
//! 原实现：`ncc-registry/store/index.go`。节点侧只是**副本**：
//!
//! * **幂等键 = 平台的索引 id（`source_id`）**：同一条重推是覆盖，不是新增 ——
//!   否则本地检索会看到同一条索引的十几份历史版本。覆盖时**不动** `hits`
//!   （本地召回次数是节点自己攒的，不该被一次重推抹掉）。
//! * **节点不算信誉权重**：评分只存在平台，这一层也没有评分列。
//! * 时间列按既有库的 GORM 文本格式存取（`ncc_core::timeutil`），不给 sqlx 映射
//!   chrono 类型 —— 映射错了只会得到「读写都能跑、老数据一读就炸」这种最难查的错。

use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::{format_go, now_go, parse_time};

use super::marshal_list;

/// 需求侧（与平台同一套词表）。
pub const KIND_NEED: &str = "need";

/// `index_entries` 一行。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IndexEntry {
    pub id: String,
    pub source_id: String,
    pub owner_key: String,
    pub owner: String,
    pub display_name: String,
    pub provider_kind: String,
    pub region: String,
    pub kind: String,
    pub channel: String,
    pub slug: String,
    #[sqlx(rename = "ref")]
    pub ref_: String,
    pub title: String,
    pub summary: String,
    pub description: String,
    pub category: String,
    pub tags: String,
    pub intents: String,
    pub languages: String,
    pub protocol: String,
    pub endpoint: String,
    pub visibility: String,
    pub status: String,
    pub hits: i64,
    pub pushed_at: Option<String>,
    pub received_at: Option<String>,
    pub updated_at: Option<String>,
}

impl IndexEntry {
    /// 这条索引是不是需求。
    pub fn kind_is_need(&self) -> bool {
        self.kind == KIND_NEED
    }

    /// 区域是否覆盖（口径与平台一致：空 / 全国 / 不限 = 覆盖任何区域）。
    pub fn region_match(&self, region: &str) -> bool {
        let region = region.trim();
        if region.is_empty() {
            return false;
        }
        let r = self.region.trim().to_lowercase();
        if ["", "全国", "不限", "any", "global", "all"].contains(&r.as_str()) {
            return true;
        }
        self.region.contains(region) || region.contains(&self.region)
    }
}

const COLS: &str = "id, source_id, owner_key, owner, display_name, provider_kind, region, kind, \
channel, slug, `ref`, title, summary, description, category, tags, intents, languages, protocol, \
endpoint, visibility, status, hits, pushed_at, received_at, updated_at";

/// 一份推来的索引（平台条目，已去掉平台自己的账）。
#[derive(Debug, Clone, Default)]
pub struct IndexInput {
    pub source_id: String,
    pub owner_key: String,
    pub owner: String,
    pub display_name: String,
    pub provider_kind: String,
    pub region: String,
    pub kind: String,
    pub channel: String,
    pub slug: String,
    pub ref_: String,
    pub title: String,
    pub summary: String,
    pub description: String,
    pub category: String,
    pub tags: Vec<String>,
    pub intents: Vec<String>,
    pub languages: Vec<String>,
    pub protocol: String,
    pub endpoint: String,
    pub visibility: String,
    pub status: String,
    /// 平台那条索引的更新时刻（解析不出来就是 `None`，不硬编一个假时间）。
    pub pushed_at: Option<String>,
}

/// 收下一份索引（幂等键 = `source_id`：重推覆盖）。
pub async fn upsert_index(
    pool: &SqlitePool,
    input: &IndexInput,
) -> Result<IndexEntry, sqlx::Error> {
    let tags = marshal_list(&input.tags);
    let intents = marshal_list(&input.intents);
    let languages = marshal_list(&input.languages);
    let visibility = if input.visibility.is_empty() {
        "public"
    } else {
        &input.visibility
    };
    let status = if input.status.is_empty() {
        "active"
    } else {
        &input.status
    };
    let pushed_at = input
        .pushed_at
        .as_deref()
        .and_then(parse_time)
        .map(format_go);

    if let Some(existing) = by_source_id(pool, &input.source_id).await? {
        sqlx::query(
            "UPDATE index_entries SET owner_key = ?, owner = ?, display_name = ?, provider_kind = ?, \
region = ?, kind = ?, channel = ?, slug = ?, `ref` = ?, title = ?, summary = ?, description = ?, \
category = ?, tags = ?, intents = ?, languages = ?, protocol = ?, endpoint = ?, visibility = ?, \
status = ?, pushed_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(&input.owner_key)
        .bind(&input.owner)
        .bind(&input.display_name)
        .bind(&input.provider_kind)
        .bind(&input.region)
        .bind(&input.kind)
        .bind(&input.channel)
        .bind(&input.slug)
        .bind(&input.ref_)
        .bind(&input.title)
        .bind(&input.summary)
        .bind(&input.description)
        .bind(&input.category)
        .bind(&tags)
        .bind(&intents)
        .bind(&languages)
        .bind(&input.protocol)
        .bind(&input.endpoint)
        .bind(visibility)
        .bind(status)
        .bind(&pushed_at)
        .bind(now_go())
        .bind(&existing.id)
        .execute(pool)
        .await?;
        return by_id(pool, &existing.id)
            .await?
            .ok_or(sqlx::Error::RowNotFound);
    }

    let now = now_go();
    let id = new_id("NI");
    sqlx::query(
        "INSERT INTO index_entries (id, source_id, owner_key, owner, display_name, provider_kind, \
region, kind, channel, slug, `ref`, title, summary, description, category, tags, intents, languages, \
protocol, endpoint, visibility, status, hits, pushed_at, received_at, updated_at) \
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&input.source_id)
    .bind(&input.owner_key)
    .bind(&input.owner)
    .bind(&input.display_name)
    .bind(&input.provider_kind)
    .bind(&input.region)
    .bind(&input.kind)
    .bind(&input.channel)
    .bind(&input.slug)
    .bind(&input.ref_)
    .bind(&input.title)
    .bind(&input.summary)
    .bind(&input.description)
    .bind(&input.category)
    .bind(&tags)
    .bind(&intents)
    .bind(&languages)
    .bind(&input.protocol)
    .bind(&input.endpoint)
    .bind(visibility)
    .bind(status)
    .bind(&pushed_at)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    by_id(pool, &id).await?.ok_or(sqlx::Error::RowNotFound)
}

pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<IndexEntry>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM index_entries WHERE id = ?");
    sqlx::query_as::<_, IndexEntry>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn by_source_id(
    pool: &SqlitePool,
    source_id: &str,
) -> Result<Option<IndexEntry>, sqlx::Error> {
    let sql = format!("SELECT {COLS} FROM index_entries WHERE source_id = ?");
    sqlx::query_as::<_, IndexEntry>(&sql)
        .bind(source_id)
        .fetch_optional(pool)
        .await
}

/// 本地检索条件。
#[derive(Debug, Clone, Default)]
pub struct IndexListOpts {
    /// 频道（前缀匹配：`food` 也算 `food/res` 命中）。
    pub channel: String,
    pub kind: String,
    /// supply | need。
    pub side: String,
    pub q: String,
    pub region: String,
    /// 只看 active + public（节点侧的读口都是公开的）。
    pub public_only: bool,
    pub limit: i64,
}

/// 列出本节点收到的索引（默认只看 active + public）。
pub async fn list_index(
    pool: &SqlitePool,
    o: &IndexListOpts,
) -> Result<Vec<IndexEntry>, sqlx::Error> {
    let mut wheres: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    if o.public_only {
        wheres.push("status = ? AND visibility = ?".to_string());
        binds.push("active".to_string());
        binds.push("public".to_string());
    }
    if !o.channel.is_empty() {
        wheres.push("(channel = ? OR channel LIKE ?)".to_string());
        binds.push(o.channel.clone());
        binds.push(format!("{}/%", o.channel));
    }
    if !o.kind.is_empty() {
        wheres.push("kind = ?".to_string());
        binds.push(o.kind.clone());
    }
    // 需求侧就是 `kind = need`；供给侧是「不是需求的都算」。
    if o.side == "need" {
        wheres.push("kind = ?".to_string());
        binds.push(KIND_NEED.to_string());
    } else if o.side == "supply" {
        wheres.push("kind <> ?".to_string());
        binds.push(KIND_NEED.to_string());
    }
    if !o.region.is_empty() {
        wheres.push(
            "(region LIKE ? OR region IN ('', '全国', '不限', 'any', 'global', 'all'))".to_string(),
        );
        binds.push(format!("%{}%", o.region));
    }
    if !o.q.is_empty() {
        wheres.push(
            "(title LIKE ? OR summary LIKE ? OR description LIKE ? OR channel LIKE ? OR tags LIKE ? OR intents LIKE ?)"
                .to_string(),
        );
        let like = format!("%{}%", o.q);
        for _ in 0..6 {
            binds.push(like.clone());
        }
    }
    let limit = if o.limit < 1 || o.limit > 200 {
        40
    } else {
        o.limit
    };
    let where_sql = if wheres.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", wheres.join(" AND "))
    };
    let sql =
        format!("SELECT {COLS} FROM index_entries{where_sql} ORDER BY updated_at DESC LIMIT ?");
    let mut q = sqlx::query_as::<_, IndexEntry>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    q.bind(limit).fetch_all(pool).await
}

/// 本节点的频道聚合（`ncc list channels --from <节点>`）。
pub async fn index_channels(pool: &SqlitePool) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT channel, COUNT(*) AS entries FROM index_entries \
WHERE status = ? AND visibility = ? GROUP BY channel ORDER BY entries DESC, channel ASC",
    )
    .bind("active")
    .bind("public")
    .fetch_all(pool)
    .await
}

/// 本节点收到的索引数（meta 的 counts 里报出来）。
pub async fn count_index(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM index_entries")
        .fetch_one(pool)
        .await
}

/// 记一次本地召回。
pub async fn bump_index_hits(pool: &SqlitePool, ids: &[String]) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    let ph = vec!["?"; ids.len()].join(",");
    let sql = format!("UPDATE index_entries SET hits = hits + 1 WHERE id IN ({ph})");
    let mut q = sqlx::query(&sql);
    for id in ids {
        q = q.bind(id);
    }
    q.execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool(tag: &str) -> (SqlitePool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ncc-index-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
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

    fn input(source: &str, kind: &str, title: &str, channel: &str) -> IndexInput {
        IndexInput {
            source_id: source.to_string(),
            owner_key: "U-1".to_string(),
            owner: "@alice".to_string(),
            display_name: "Alice".to_string(),
            provider_kind: "user".to_string(),
            region: "华东".to_string(),
            kind: kind.to_string(),
            channel: channel.to_string(),
            slug: "demo".to_string(),
            ref_: "@alice/demo".to_string(),
            title: title.to_string(),
            summary: "摘要".to_string(),
            description: "描述".to_string(),
            category: "life".to_string(),
            tags: vec!["酒店".to_string()],
            intents: vec!["订房".to_string()],
            languages: vec!["zh".to_string()],
            protocol: "https".to_string(),
            endpoint: "https://example.com".to_string(),
            visibility: String::new(), // 空 → public
            status: String::new(),     // 空 → active
            pushed_at: Some("2026-01-01T10:00:00Z".to_string()),
        }
    }

    #[tokio::test]
    async fn 幂等键_是平台的索引_id() {
        let (p, _d) = pool("idem").await;
        let first = upsert_index(&p, &input("IX-1", "service", "酒店预订", "food/hotel"))
            .await
            .unwrap();
        assert!(first.id.starts_with("NI-"));
        assert_eq!(first.visibility, "public");
        assert_eq!(first.status, "active");
        assert_eq!(first.hits, 0);
        assert!(first.pushed_at.is_some());

        // 重推同一条：覆盖而不是新增
        let mut again = input("IX-1", "service", "酒店预订 v2", "food/hotel");
        again.region = "华南".to_string();
        let second = upsert_index(&p, &again).await.unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.title, "酒店预订 v2");
        assert_eq!(second.region, "华南");
        assert_eq!(count_index(&p).await.unwrap(), 1);

        // 覆盖不该抹掉本地召回次数
        bump_index_hits(&p, &[second.id.clone()]).await.unwrap();
        bump_index_hits(&p, &[second.id.clone()]).await.unwrap();
        let third = upsert_index(&p, &again).await.unwrap();
        assert_eq!(third.hits, 2);

        // 平台解析不出的 pushed_at：留空而不是编一个时间
        let mut bad = input("IX-2", "service", "没时间", "food");
        bad.pushed_at = Some("不是时间".to_string());
        assert!(upsert_index(&p, &bad).await.unwrap().pushed_at.is_none());
        let mut none = input("IX-3", "service", "没给时间", "food");
        none.pushed_at = None;
        assert!(upsert_index(&p, &none).await.unwrap().pushed_at.is_none());

        assert!(by_source_id(&p, "IX-1").await.unwrap().is_some());
        assert!(by_id(&p, "NI-nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 列表_过滤与默认值() {
        let (p, _d) = pool("list").await;
        upsert_index(&p, &input("IX-1", "service", "酒店预订", "food/hotel"))
            .await
            .unwrap();
        let mut need = input("IX-2", "need", "找人做官网", "dev/web");
        need.region = "全国".to_string();
        upsert_index(&p, &need).await.unwrap();
        let mut hidden = input("IX-3", "service", "私有的", "dev/web");
        hidden.visibility = "private".to_string();
        upsert_index(&p, &hidden).await.unwrap();

        // 公开读口：私有的不出现
        let all = list_index(
            &p,
            &IndexListOpts {
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(all.len(), 2);

        // 频道前缀匹配：food 命中 food/hotel
        let food = list_index(
            &p,
            &IndexListOpts {
                channel: "food".to_string(),
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(food.len(), 1);
        assert_eq!(food[0].channel, "food/hotel");

        // side：need 只要需求，supply 是「不是需求的都算」
        let needs = list_index(
            &p,
            &IndexListOpts {
                side: "need".to_string(),
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(needs.len(), 1);
        assert!(needs[0].kind_is_need());
        let supply = list_index(
            &p,
            &IndexListOpts {
                side: "supply".to_string(),
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(supply.len(), 1);
        assert_eq!(supply[0].source_id, "IX-1");

        // 区域：全国 = 覆盖任何区域；华东 命中「华东」
        let any = list_index(
            &p,
            &IndexListOpts {
                region: "华南".to_string(),
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(any.len(), 1);
        assert_eq!(any[0].source_id, "IX-2");
        let east = list_index(
            &p,
            &IndexListOpts {
                region: "华东".to_string(),
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // 「全国」= 覆盖任何区域：所以它也在结果里，排在更具体的「华东」之后
        assert_eq!(east.len(), 2);
        assert!(east.iter().any(|e| e.source_id == "IX-1"));

        // 关键词：标题 / 标签 / 意向都算命中面（两条都带「酒店」标签）
        let q = list_index(
            &p,
            &IndexListOpts {
                q: "酒店".to_string(),
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(q.len(), 2);
        // 只命中标题的那条
        let q = list_index(
            &p,
            &IndexListOpts {
                q: "官网".to_string(),
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].source_id, "IX-2");

        // limit 越界回落到 40
        let capped = list_index(
            &p,
            &IndexListOpts {
                limit: 999,
                public_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(capped.len(), 2);
    }

    #[tokio::test]
    async fn 频道聚合与区域判断() {
        let (p, _d) = pool("chan").await;
        upsert_index(&p, &input("IX-1", "service", "A", "food/hotel"))
            .await
            .unwrap();
        upsert_index(&p, &input("IX-2", "service", "B", "food/hotel"))
            .await
            .unwrap();
        upsert_index(&p, &input("IX-3", "service", "C", "dev/web"))
            .await
            .unwrap();
        let mut hidden = input("IX-4", "service", "D", "dev/web");
        hidden.status = "archived".to_string();
        upsert_index(&p, &hidden).await.unwrap();

        let chans = index_channels(&p).await.unwrap();
        assert_eq!(
            chans,
            vec![("food/hotel".to_string(), 2), ("dev/web".to_string(), 1)]
        );

        let e = by_source_id(&p, "IX-1").await.unwrap().unwrap();
        assert!(e.region_match("华东"));
        assert!(e.region_match("华")); // 部分包含也算覆盖
        assert!(!e.region_match(""));
        let mut any = input("IX-9", "service", "E", "x");
        any.region = "全国".to_string();
        let any = upsert_index(&p, &any).await.unwrap();
        assert!(any.region_match("随便哪儿"));
    }
}
