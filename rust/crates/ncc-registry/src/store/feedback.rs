//! NCC Feedback：跨 Agent、跨用户的反馈。
//!
//! 为什么它既不是「评分」也不是「评论」：
//!
//! * 评分是**一个人对另一个人**的稳定看法（一人一条、可改）；
//! * 反馈是**一次使用之后说的一句话**：谁（人或 Agent）、对哪个东西、什么时候、
//!   是好话还是问题、有没有打分、从哪次运行来的。
//!
//! 于是它**只追加、不可改**：要补充就再回一条（回复也是一条反馈）。唯一的写动作是
//! **处置状态**（open/ack/resolved/wontfix），且只有目标的拥有者能改 ——
//! 「处置」与「内容」是两件事，混在一起就会出现「把不好听的话删掉」。
//!
//! 本模块把 Go 侧的 `model/feedback.go`（词表 / 校验 / 上限）与 `store/feedback.go`
//! （查询 / 聚合）合成一处：Rust 侧没有独立的 model 层，词汇表放 store 里与
//! `store::nodes` 的做法一致。

use std::collections::BTreeMap;

use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::now_go;

/* ---------------- 词表 ---------------- */

/// 反馈说的是**什么东西**。顺序即展示顺序。
pub const FB_ABOUT_KINDS: &[&str] = &[
    "artifact", "node", "service", "profile", "run", "agent", "topic",
];

/// 反馈的性质。
pub const FB_KINDS: &[&str] = &["report", "praise", "request", "correction", "rating"];

/// 处置状态。
pub const FB_STATUSES: &[&str] = &["open", "ack", "resolved", "wontfix"];

pub const FB_OPEN: &str = "open";
pub const FB_ACK: &str = "ack";

pub const FB_PRIVATE: &str = "private";
pub const FB_PUBLIC: &str = "public";

/// 上限（与 Go 逐字一致，改动会让两端对「什么算超限」的判定分叉）。
pub const FB_MAX_BODY: usize = 4000;
pub const FB_MAX_TAGS: usize = 8;
pub const FB_MAX_HOPS: usize = 8;
pub const FB_MAX_STATE_REFS: usize = 8;
pub const FB_MAX_REF: usize = 256;
pub const FB_SCORE_MIN: i64 = 0;
pub const FB_SCORE_MAX: i64 = 5;

/// `(id, 中文, 英文, 中文说明, 英文说明)`。
pub const FB_ABOUT_META: &[(&str, &str, &str, &str, &str)] = &[
    (
        "artifact",
        "制品",
        "Artifact",
        "对某个制品（skill/mcp/hur/镜像…）的反馈",
        "Feedback on an artifact",
    ),
    (
        "node",
        "节点",
        "Node",
        "对一台主机 / 服务的反馈（离线、慢、连不上…）",
        "Feedback on a machine or node",
    ),
    (
        "service",
        "服务",
        "Service",
        "对某个对外服务的反馈（会走到提供方那里）",
        "Feedback on a service offering",
    ),
    (
        "profile",
        "名片",
        "Profile",
        "对某个人的名片/作品的反馈",
        "Feedback about a person's profile",
    ),
    (
        "run",
        "一次运行",
        "Run",
        "一次运行之后的反馈（最常用，带 traceRef）",
        "Feedback after a run",
    ),
    (
        "agent",
        "Agent",
        "Agent",
        "对某个 Agent 的反馈",
        "Feedback on an agent",
    ),
    (
        "topic",
        "词条",
        "Topic",
        "还没归到具体东西上的话",
        "Not attached to anything specific yet",
    ),
];

pub const FB_KIND_META: &[(&str, &str, &str, &str, &str)] = &[
    (
        "report",
        "问题",
        "Report",
        "这里有毛病（能复现就写清怎么复现）",
        "Something is broken",
    ),
    (
        "praise",
        "表扬",
        "Praise",
        "这儿挺好用（比抱怨稀有，值钱）",
        "This worked well",
    ),
    (
        "request",
        "需求",
        "Request",
        "我希望它能……",
        "I wish it could…",
    ),
    (
        "correction",
        "纠正",
        "Correction",
        "上一条说法不对 / 文档与行为不一致",
        "A previous claim was wrong",
    ),
    (
        "rating",
        "打分",
        "Rating",
        "带分数的评价（1~5）",
        "A scored evaluation (1-5)",
    ),
];

pub const FB_STATUS_META: &[(&str, &str, &str, &str, &str)] = &[
    (
        "open",
        "待处理",
        "Open",
        "还没人回应",
        "Nobody has responded yet",
    ),
    (
        "ack",
        "已确认",
        "Acknowledged",
        "看到了，会处理 / 会考虑",
        "Seen, will look at it",
    ),
    (
        "resolved",
        "已解决",
        "Resolved",
        "改完了（最好说清改在哪个版本）",
        "Fixed (say which version)",
    ),
    (
        "wontfix",
        "不处理",
        "Won't fix",
        "明确不做，说清为什么",
        "Deliberately not doing it, with a reason",
    ),
];

fn meta_of(
    catalog: &[(
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    )],
    k: &str,
) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    catalog
        .iter()
        .find(|m| m.0 == k)
        .map(|m| (m.1, m.2, m.3, m.4))
}

/// 取 `(中文, 英文, 中文说明, 英文说明)`；找不到回 `None`（调用方决定要不要原样展示 id）。
pub fn about_meta(k: &str) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    meta_of(FB_ABOUT_META, k)
}
pub fn kind_meta(k: &str) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    meta_of(FB_KIND_META, k)
}
pub fn status_meta(k: &str) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    meta_of(FB_STATUS_META, k)
}

pub fn valid_about_kind(k: &str) -> bool {
    FB_ABOUT_KINDS.contains(&k)
}
pub fn valid_kind(k: &str) -> bool {
    FB_KINDS.contains(&k)
}
pub fn valid_status(s: &str) -> bool {
    FB_STATUSES.contains(&s)
}
pub fn valid_visibility(v: &str) -> bool {
    v == FB_PRIVATE || v == FB_PUBLIC
}

/// 校验一条反馈的内容（空 = 通过）。**只管这份记录本身合不合法**；
/// 「能不能对那个东西说话、能不能看」是授权问题，在 HTTP 层判。
pub fn validate(
    about_kind: &str,
    about_ref: &str,
    kind: &str,
    score: i64,
    body: &str,
    visibility: &str,
    tags: &[String],
    hops: &[String],
    state_refs: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !valid_about_kind(about_kind) {
        out.push(format!(
            "aboutKind 必须是 {}，当前是 {}",
            FB_ABOUT_KINDS.join("|"),
            go_quote(about_kind)
        ));
    }
    if about_ref.trim().is_empty() {
        out.push(
            "缺 aboutRef（说的是哪个东西 —— `@命名空间/slug` / 节点 id / 运行 id）".to_string(),
        );
    }
    if about_ref.chars().count() > FB_MAX_REF {
        out.push(format!("aboutRef 太长（上限 {FB_MAX_REF}）"));
    }
    if !valid_kind(kind) {
        out.push(format!(
            "kind 必须是 {}，当前是 {}",
            FB_KINDS.join("|"),
            go_quote(kind)
        ));
    }
    if score < FB_SCORE_MIN || score > FB_SCORE_MAX {
        out.push(format!(
            "score 必须在 {FB_SCORE_MIN}~{FB_SCORE_MAX} 之间（0 = 不打分）"
        ));
    }
    if kind == "rating" && score == 0 {
        out.push(format!("kind=rating 就得给分（score 1~{FB_SCORE_MAX}）"));
    }
    if body.trim().is_empty() && score == 0 {
        out.push("body 与 score 不能都是空的（一条什么都没说的反馈没有意义）".to_string());
    }
    if body.len() > FB_MAX_BODY {
        out.push(format!(
            "body 太长（{} 字节，上限 {}）—— 长文请走 kb",
            body.len(),
            FB_MAX_BODY
        ));
    }
    if !valid_visibility(visibility) {
        out.push(format!(
            "visibility 必须是 private|public，当前是 {}",
            go_quote(visibility)
        ));
    }
    if tags.len() > FB_MAX_TAGS {
        out.push(format!("tags 太多（上限 {FB_MAX_TAGS}）"));
    }
    if hops.len() > FB_MAX_HOPS {
        out.push(format!(
            "hops 太长（上限 {FB_MAX_HOPS}）—— 链路是出处，不是日志"
        ));
    }
    if state_refs.len() > FB_MAX_STATE_REFS {
        out.push(format!("stateRefs 太多（上限 {FB_MAX_STATE_REFS}）"));
    }
    out
}

/// 三条不变量（`/api/feedback/kinds` 与文档共用同一份说法）。
pub fn red_lines() -> Vec<String> {
    vec![
        "反馈只追加、不可改：要补充就再回一条（回复也是一条反馈）—— 改了就不是证据了".to_string(),
        "默认私有，公开要作者显式说：relay 上云只搬公开的那些，私有的永不出机器".to_string(),
        "搬上来的人以令牌为准：原作者只作为转述写进 origin —— 证明来源，不证明内容真实".to_string(),
    ]
}

/// Go `%q` 风格的引号（错误文案里带引号，保持与 Go 一致）。
fn go_quote(s: &str) -> String {
    format!("{:?}", s)
}

/* ---------------- 记录 ---------------- */

/// 一条反馈（列名与既有库一一对应）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Feedback {
    pub id: String,
    pub owner_id: String,
    pub about_kind: String,
    pub about_ref: String,
    pub kind: String,
    pub score: i64,
    pub body: String,
    pub tags: String,
    pub author_id: String,
    pub author_handle: String,
    pub agent_id: String,
    pub parent_id: String,
    pub hops: String,
    pub visibility: String,
    pub status: String,
    pub trace_ref: String,
    pub state_refs: String,
    pub origin: String,
    pub origin_id: String,
    pub created_at: Option<String>,
}

/// 列表用：一条反馈 + 它的回复数（回复不展开成整棵树，避免一次拉太多）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeedbackRow {
    #[sqlx(flatten)]
    pub f: Feedback,
    pub replies: i64,
}

const FB_COLS: &str = "id, owner_id, about_kind, about_ref, kind, score, body, tags, author_id, \
     author_handle, agent_id, parent_id, hops, visibility, status, trace_ref, state_refs, origin, origin_id, created_at";

/// 列表过滤条件。可见性折成 `viewer_id` 传进来：存储层不做身份判断，但它也**不会
/// 忘掉条件**，更不会「什么都没给就返回全部」（fail-closed）。
#[derive(Debug, Clone, Default)]
pub struct FeedbackListOpts {
    /// 谁在看（空 = 匿名：只看得到 public）。
    pub viewer_id: String,
    /// 管理员视角（看全部）。
    pub all_seen: bool,
    pub about_kind: String,
    pub about_ref: String,
    /// 目标是我的（收件箱）。
    pub owner_id: String,
    /// 我发的。
    pub author_id: String,
    pub kind: String,
    pub status: String,
    pub visibility: String,
    /// 只看还没处置的。
    pub unresolved: bool,
    pub page: i64,
    pub size: i64,
}

/// 聚合结果。说清它**不是**什么：不是排名分、不进任何匹配排序、不发给节点做权重。
#[derive(Debug, Clone, Default)]
pub struct FeedbackSummary {
    pub count: i64,
    pub by_kind: BTreeMap<String, i64>,
    pub by_status: BTreeMap<String, i64>,
    pub scored: i64,
    pub score_avg: f64,
    pub self_count: i64,
    pub public_count: i64,
    pub private_count: i64,
    pub agents: BTreeMap<String, i64>,
    pub tags: BTreeMap<String, i64>,
    pub open_count: i64,
    pub first_at: Option<String>,
    pub last_at: Option<String>,
}

fn norm_page(page: i64, size: i64) -> (i64, i64) {
    let page = if page < 1 { 1 } else { page };
    let size = if size < 1 {
        20
    } else if size > 100 {
        100
    } else {
        size
    };
    (page, size)
}

fn push_where(o: &FeedbackListOpts, sql: &mut String, binds: &mut Vec<String>) {
    sql.push_str(" WHERE parent_id = ''");
    if !o.all_seen {
        if o.viewer_id.trim().is_empty() {
            sql.push_str(" AND visibility = ?");
            binds.push(FB_PUBLIC.to_string());
        } else {
            sql.push_str(" AND (visibility = ? OR author_id = ? OR owner_id = ?)");
            binds.push(FB_PUBLIC.to_string());
            binds.push(o.viewer_id.clone());
            binds.push(o.viewer_id.clone());
        }
    }
    let eq = |col: &str, v: &str, sql: &mut String, binds: &mut Vec<String>| {
        if !v.trim().is_empty() {
            sql.push_str(&format!(" AND {col} = ?"));
            binds.push(v.trim().to_string());
        }
    };
    eq("about_kind", &o.about_kind, sql, binds);
    eq("about_ref", &o.about_ref, sql, binds);
    eq("owner_id", &o.owner_id, sql, binds);
    eq("author_id", &o.author_id, sql, binds);
    eq("kind", &o.kind, sql, binds);
    eq("status", &o.status, sql, binds);
    eq("visibility", &o.visibility, sql, binds);
    if o.unresolved {
        sql.push_str(" AND status IN (?, ?)");
        binds.push(FB_OPEN.to_string());
        binds.push(FB_ACK.to_string());
    }
}

/// 建一条反馈（**没有 Update**，要改就再建一条）。
pub async fn create(pool: &SqlitePool, f: &Feedback) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO feedback (id, owner_id, about_kind, about_ref, kind, score, body, tags, author_id, \
         author_handle, agent_id, parent_id, hops, visibility, status, trace_ref, state_refs, origin, origin_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(if f.id.trim().is_empty() { new_id("FB") } else { f.id.clone() })
    .bind(&f.owner_id)
    .bind(&f.about_kind)
    .bind(&f.about_ref)
    .bind(&f.kind)
    .bind(f.score)
    .bind(&f.body)
    .bind(&f.tags)
    .bind(&f.author_id)
    .bind(&f.author_handle)
    .bind(&f.agent_id)
    .bind(&f.parent_id)
    .bind(&f.hops)
    .bind(&f.visibility)
    .bind(&f.status)
    .bind(&f.trace_ref)
    .bind(&f.state_refs)
    .bind(&f.origin)
    .bind(&f.origin_id)
    .bind(f.created_at.clone().unwrap_or_else(now_go))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn by_id(pool: &SqlitePool, id: &str) -> Result<Option<Feedback>, sqlx::Error> {
    let sql = format!("SELECT {FB_COLS} FROM feedback WHERE id = ?");
    sqlx::query_as::<_, Feedback>(&sql)
        .bind(id.trim())
        .fetch_optional(pool)
        .await
}

/// 按 `(origin, origin_id)` 找 —— relay 的幂等键。第二次搬同一条不算错误，算「已经有了」。
pub async fn by_origin(
    pool: &SqlitePool,
    origin: &str,
    origin_id: &str,
) -> Result<Option<Feedback>, sqlx::Error> {
    if origin.trim().is_empty() || origin_id.trim().is_empty() {
        return Ok(None);
    }
    let sql = format!("SELECT {FB_COLS} FROM feedback WHERE origin = ? AND origin_id = ?");
    sqlx::query_as::<_, Feedback>(&sql)
        .bind(origin.trim())
        .bind(origin_id.trim())
        .fetch_optional(pool)
        .await
}

/// 列反馈（分页 + 总数）。回复不列在主线里，它们只在 get 时展开。
pub async fn list(
    pool: &SqlitePool,
    o: &FeedbackListOpts,
) -> Result<(Vec<FeedbackRow>, i64), sqlx::Error> {
    let (page, size) = norm_page(o.page, o.size);
    let (filter, binds) = {
        let mut sql = String::new();
        let mut binds = Vec::new();
        push_where(o, &mut sql, &mut binds);
        (sql, binds)
    };

    let count_sql = format!("SELECT COUNT(*) FROM feedback{filter}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &binds {
        cq = cq.bind(b);
    }
    let total = cq.fetch_one(pool).await?;

    let sql = format!(
        "SELECT feedback.*, (SELECT COUNT(*) FROM feedback r WHERE r.parent_id = feedback.id) AS replies \
         FROM feedback{filter} ORDER BY created_at DESC LIMIT ? OFFSET ?"
    );
    let mut q = sqlx::query_as::<_, FeedbackRow>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let rows = q.bind(size).bind((page - 1) * size).fetch_all(pool).await?;
    Ok((rows, total))
}

/// 一条反馈下面的回复（按时间正序 —— 对话要按顺序读）。
pub async fn list_replies(
    pool: &SqlitePool,
    parent_id: &str,
) -> Result<Vec<Feedback>, sqlx::Error> {
    let sql = format!("SELECT {FB_COLS} FROM feedback WHERE parent_id = ? ORDER BY created_at ASC");
    sqlx::query_as::<_, Feedback>(&sql)
        .bind(parent_id)
        .fetch_all(pool)
        .await
}

/// 改**处置状态**（不是内容 —— 内容没有「改」这条路）。返回是否命中一条。
pub async fn set_status(pool: &SqlitePool, id: &str, status: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("UPDATE feedback SET status = ? WHERE id = ?")
        .bind(status)
        .bind(id.trim())
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// 聚合一批反馈（每个聚合都从**新的** where 开始，免得条件被上一次的 chain 带跑偏）。
pub async fn summary(
    pool: &SqlitePool,
    o: &FeedbackListOpts,
) -> Result<FeedbackSummary, sqlx::Error> {
    let mut sum = FeedbackSummary::default();
    let (filter, binds) = {
        let mut sql = String::new();
        let mut binds = Vec::new();
        push_where(o, &mut sql, &mut binds);
        (sql, binds)
    };

    let pairs = |rows: Vec<(String, i64)>| -> BTreeMap<String, i64> { rows.into_iter().collect() };

    // 按 kind 分组（顺便得到总数）
    let sql = format!("SELECT kind, COUNT(*) FROM feedback{filter} GROUP BY kind");
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    for r in q.fetch_all(pool).await? {
        sum.count += r.1;
        sum.by_kind.insert(r.0, r.1);
    }

    let sql = format!("SELECT status, COUNT(*) FROM feedback{filter} GROUP BY status");
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    let status_rows = q.fetch_all(pool).await?;
    for (k, n) in &status_rows {
        if k == FB_OPEN {
            sum.open_count = *n;
        }
    }
    sum.by_status = pairs(status_rows);

    let sql = format!("SELECT visibility, COUNT(*) FROM feedback{filter} GROUP BY visibility");
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    for (k, n) in q.fetch_all(pool).await? {
        match k.as_str() {
            FB_PUBLIC => sum.public_count = n,
            FB_PRIVATE => sum.private_count = n,
            _ => {}
        }
    }

    // 带分的那部分：只算有分的（score=0 是「没打分」，不是「打了 0 分」）。
    let sql =
        format!("SELECT COUNT(*), COALESCE(SUM(score), 0) FROM feedback{filter} AND score > 0");
    let mut q = sqlx::query_as::<_, (i64, i64)>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    if let Some((n, s)) = q.fetch_optional(pool).await? {
        sum.scored = n;
        if n > 0 {
            sum.score_avg = s as f64 / n as f64;
        }
    }

    // 自己给自己记的 —— 列出来只是为了让读的人能自己把它扣掉。
    let sql = format!(
        "SELECT COUNT(*) FROM feedback{filter} AND owner_id != '' AND owner_id = author_id"
    );
    let mut q = sqlx::query_scalar::<_, i64>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    sum.self_count = q.fetch_one(pool).await?;

    let sql = format!("SELECT agent_id, COUNT(*) FROM feedback{filter} GROUP BY agent_id");
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    for (k, n) in q.fetch_all(pool).await? {
        let key = if k.is_empty() {
            "（人自己）".to_string()
        } else {
            k
        };
        sum.agents.insert(key, n);
    }

    // 标签分布：tags 是 JSON 文本，不做关联表，在内存里数。
    let sql = format!("SELECT tags FROM feedback{filter} AND tags != ''");
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    for raw in q.fetch_all(pool).await? {
        for t in super::parse_list(&raw) {
            *sum.tags.entry(t).or_insert(0) += 1;
        }
    }

    // 首尾时刻：用「取第一条/最后一条」而不是 MIN()/MAX() —— SQLite 把 datetime 当文本返回，
    // 直接扫进时间类型会炸（Go 侧踩过同一个坑）。
    let sql = format!("SELECT created_at FROM feedback{filter} ORDER BY created_at ASC LIMIT 1");
    let mut q = sqlx::query_scalar::<_, Option<String>>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    sum.first_at = q.fetch_optional(pool).await?.flatten();

    let sql = format!("SELECT created_at FROM feedback{filter} ORDER BY created_at DESC LIMIT 1");
    let mut q = sqlx::query_scalar::<_, Option<String>>(&sql);
    for b in &binds {
        q = q.bind(b);
    }
    sum.last_at = q.fetch_optional(pool).await?.flatten();

    Ok(sum)
}

/* ---------------- 归属解析（给 handler 用的只读查询） ---------------- */

/// 按运行 id / trace_id 找轨迹的归属者。
///
/// 返回 `None` = 没有这条轨迹；`Some("")` = 轨迹在但它没记归属（老数据）。
/// 直接查 `traces` 表而不是调 traces 族：那一族本批次还在迁移中，接口面未稳定。
pub async fn trace_owner(pool: &SqlitePool, ref_: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT COALESCE(ns.owner_id, '') FROM traces t LEFT JOIN namespaces ns ON ns.id = t.namespace_id \
         WHERE t.id = ? OR t.trace_id = ? LIMIT 1",
    )
    .bind(ref_.trim())
    .bind(ref_.trim())
    .fetch_optional(pool)
    .await
}

/// 按节点 id 或 `@命名空间/slug` 找托管节点，返回 `(owner_id, kind)`。
pub async fn node_owner_kind(
    pool: &SqlitePool,
    ref_: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let ref_ = ref_.trim();
    if let Some(body) = ref_.strip_prefix('@') {
        if let Some((ns, slug)) = body.split_once('/') {
            return sqlx::query_as::<_, (String, String)>(
                "SELECT COALESCE(ns.owner_id, ''), h.kind FROM hosted_nodes h \
                 JOIN namespaces ns ON ns.id = h.namespace_id WHERE ns.slug = ? AND h.slug = ? LIMIT 1",
            )
            .bind(ns)
            .bind(slug)
            .fetch_optional(pool)
            .await;
        }
    }
    sqlx::query_as::<_, (String, String)>(
        "SELECT COALESCE(ns.owner_id, ''), h.kind FROM hosted_nodes h \
         JOIN namespaces ns ON ns.id = h.namespace_id WHERE h.id = ? LIMIT 1",
    )
    .bind(ref_)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{namespaces, users};

    async fn pool(name: &str) -> (SqlitePool, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("ncc-fb-{}-{name}", std::process::id()));
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

    fn mk(id: &str, owner: &str, author: &str, vis: &str, kind: &str, status: &str) -> Feedback {
        Feedback {
            id: id.to_string(),
            owner_id: owner.to_string(),
            about_kind: "topic".to_string(),
            about_ref: "x".to_string(),
            kind: kind.to_string(),
            score: 0,
            body: format!("body-{id}"),
            tags: "[]".to_string(),
            author_id: author.to_string(),
            author_handle: "甲".to_string(),
            agent_id: String::new(),
            parent_id: String::new(),
            hops: "[]".to_string(),
            visibility: vis.to_string(),
            status: status.to_string(),
            trace_ref: String::new(),
            state_refs: "[]".to_string(),
            origin: String::new(),
            origin_id: String::new(),
            created_at: None,
        }
    }

    #[tokio::test]
    async fn 可见性_fail_closed() {
        let (p, _d) = pool("vis").await;
        create(
            &p,
            &mk("FB-1", "U-owner", "U-a", FB_PUBLIC, "report", FB_OPEN),
        )
        .await
        .unwrap();
        create(
            &p,
            &mk("FB-2", "U-owner", "U-a", FB_PRIVATE, "report", FB_OPEN),
        )
        .await
        .unwrap();
        create(&p, &mk("FB-3", "U-b", "U-b", FB_PRIVATE, "report", FB_OPEN))
            .await
            .unwrap();

        // 匿名：只有 public
        let anon = FeedbackListOpts::default();
        let (rows, total) = list(&p, &anon).await.unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].f.id, "FB-1");

        // 作者本人：public + 自己发的私有
        let author = FeedbackListOpts {
            viewer_id: "U-a".to_string(),
            ..Default::default()
        };
        let (_, total) = list(&p, &author).await.unwrap();
        assert_eq!(total, 2);

        // 目标拥有者：能看到发给自己的私有
        let owner = FeedbackListOpts {
            viewer_id: "U-owner".to_string(),
            ..Default::default()
        };
        let (_, total) = list(&p, &owner).await.unwrap();
        assert_eq!(total, 2);

        // 管理员（all_seen）：全部
        let admin = FeedbackListOpts {
            all_seen: true,
            ..Default::default()
        };
        let (_, total) = list(&p, &admin).await.unwrap();
        assert_eq!(total, 3);

        // 发件箱 / 收件箱过滤
        let mine = FeedbackListOpts {
            viewer_id: "U-a".to_string(),
            author_id: "U-a".to_string(),
            ..Default::default()
        };
        assert_eq!(list(&p, &mine).await.unwrap().1, 2);
        let inbox = FeedbackListOpts {
            viewer_id: "U-owner".to_string(),
            owner_id: "U-owner".to_string(),
            ..Default::default()
        };
        assert_eq!(list(&p, &inbox).await.unwrap().1, 2);
    }

    #[tokio::test]
    async fn 回复_不列主线_回复数计入() {
        let (p, _d) = pool("reply").await;
        create(&p, &mk("FB-1", "", "U-a", FB_PUBLIC, "report", FB_OPEN))
            .await
            .unwrap();
        let mut r = mk("FB-2", "", "U-b", FB_PUBLIC, "correction", FB_OPEN);
        r.parent_id = "FB-1".to_string();
        create(&p, &r).await.unwrap();

        let (rows, total) = list(&p, &FeedbackListOpts::default()).await.unwrap();
        assert_eq!(total, 1); // 回复不在主线
        assert_eq!(rows[0].replies, 1);

        let replies = list_replies(&p, "FB-1").await.unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, "FB-2");
    }

    #[tokio::test]
    async fn relay_幂等键_与_状态更新() {
        let (p, _d) = pool("relay").await;
        let mut f = mk("FB-1", "", "U-a", FB_PUBLIC, "report", FB_OPEN);
        f.origin = "node:office".to_string();
        f.origin_id = "FB-remote-1".to_string();
        create(&p, &f).await.unwrap();

        let hit = by_origin(&p, "node:office", "FB-remote-1").await.unwrap();
        assert_eq!(hit.unwrap().id, "FB-1");
        assert!(by_origin(&p, "node:office", "nope")
            .await
            .unwrap()
            .is_none());
        // 空 origin：一律当没有（免得本地反馈互相撞在一起）
        assert!(by_origin(&p, "", "").await.unwrap().is_none());

        assert!(set_status(&p, "FB-1", "resolved").await.unwrap());
        assert_eq!(by_id(&p, "FB-1").await.unwrap().unwrap().status, "resolved");
        // 不存在的 id：返回 false 而不是报错
        assert!(!set_status(&p, "FB-999", "ack").await.unwrap());
        assert!(by_id(&p, "FB-999").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 聚合_各项() {
        let (p, _d) = pool("summary").await;
        create(&p, &mk("FB-1", "U-o", "U-a", FB_PUBLIC, "report", FB_OPEN))
            .await
            .unwrap();
        create(&p, &mk("FB-2", "U-o", "U-a", FB_PRIVATE, "rating", FB_ACK))
            .await
            .unwrap();
        let mut f3 = mk("FB-3", "U-o", "U-o", FB_PUBLIC, "praise", FB_OPEN); // 自己给自己
        f3.score = 4;
        f3.tags = r#"["a","b"]"#.to_string();
        f3.agent_id = "AG-1".to_string();
        create(&p, &f3).await.unwrap();

        let all = FeedbackListOpts {
            all_seen: true,
            ..Default::default()
        };
        let s = summary(&p, &all).await.unwrap();
        assert_eq!(s.count, 3);
        assert_eq!(s.by_kind.get("report"), Some(&1));
        assert_eq!(s.by_status.get("open"), Some(&2));
        assert_eq!(s.open_count, 2);
        assert_eq!(s.public_count, 2);
        assert_eq!(s.private_count, 1);
        assert_eq!(s.scored, 1);
        assert_eq!(s.score_avg, 4.0);
        assert_eq!(s.self_count, 1);
        assert_eq!(s.agents.get("AG-1"), Some(&1));
        assert_eq!(s.agents.get("（人自己）"), Some(&2));
        assert_eq!(s.tags.get("a"), Some(&1));
        assert!(s.first_at.is_some());
        assert!(s.last_at.is_some());

        // 匿名视角的聚合只看 public
        let anon = summary(&p, &FeedbackListOpts::default()).await.unwrap();
        assert_eq!(anon.count, 2);
        assert_eq!(anon.private_count, 0);

        // unresolved：只算 open/ack
        let un = summary(
            &p,
            &FeedbackListOpts {
                all_seen: true,
                unresolved: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(un.count, 3); // open×2 + ack×1
    }

    #[tokio::test]
    async fn 校验_各类错() {
        let ok = validate(
            "topic",
            "x",
            "report",
            0,
            "有话说",
            "private",
            &[],
            &[],
            &[],
        );
        assert!(ok.is_empty());

        // 一次塞多个错：aboutKind / aboutRef / kind / score / visibility
        // （注意 score=9 时不会同时触发「body 与 score 不能都是空的」—— Go 的判定是 score==0）
        let errs = validate("不存在", "", "不存在", 9, "", "奇怪", &[], &[], &[]);
        assert_eq!(errs.len(), 5, "{errs:?}");
        assert!(errs[0].contains("aboutKind 必须是"));
        assert!(errs[1].contains("缺 aboutRef"));
        assert!(errs[2].contains("kind 必须是"));
        assert!(errs[3].contains("score 必须在"));
        assert!(errs[4].contains("visibility 必须是 private|public"));

        // 什么都没说：body 空且没分
        let errs = validate("topic", "x", "report", 0, "   ", "private", &[], &[], &[]);
        assert_eq!(
            errs,
            vec!["body 与 score 不能都是空的（一条什么都没说的反馈没有意义）".to_string()]
        );

        // rating 必须给分
        let errs = validate("topic", "x", "rating", 0, "话", "public", &[], &[], &[]);
        assert_eq!(errs, vec!["kind=rating 就得给分（score 1~5）".to_string()]);
        // 超上限
        let long_body = "字".repeat(FB_MAX_BODY + 1);
        let errs = validate(
            "topic",
            "x",
            "report",
            0,
            &long_body,
            "public",
            &[],
            &[],
            &[],
        );
        assert!(errs[0].contains("body 太长"));
        let tags: Vec<String> = (0..FB_MAX_TAGS + 1).map(|i| i.to_string()).collect();
        assert!(
            validate("topic", "x", "report", 0, "话", "public", &tags, &[], &[])
                .iter()
                .any(|e| e.contains("tags 太多"))
        );
    }

    #[tokio::test]
    async fn 归属解析_节点与轨迹() {
        let (p, _d) = pool("resolve").await;
        let u = users::create(&p, "甲", "jia@x.com", "h").await.unwrap();
        let ns = namespaces::create_account(&p, &u.id, "甲", "jia")
            .await
            .unwrap();
        crate::store::nodes::upsert(
            &p,
            &ns.id,
            &crate::store::nodes::HeartbeatReq {
                slug: "svc".to_string(),
                name: "svc".to_string(),
                kind: "service".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let node_id = crate::store::nodes::ids_of_namespaces(&p, &[ns.id.clone()])
            .await
            .unwrap()[0]
            .clone();

        let hit = node_owner_kind(&p, &node_id).await.unwrap().unwrap();
        assert_eq!(hit.0, u.id);
        assert_eq!(hit.1, "service");
        let hit = node_owner_kind(&p, "@jia/svc").await.unwrap().unwrap();
        assert_eq!(hit.0, u.id);
        assert!(node_owner_kind(&p, "ND-nope").await.unwrap().is_none());

        // traces 表存在但没数据 -> None
        assert!(trace_owner(&p, "t-1").await.unwrap().is_none());
    }
}
