//! NCC Index（节点侧）的 HTTP 层：接收平台推来的索引 + 在**内网本地**做匹配。
//!
//! 原实现：`ncc-registry/httpapi/index.go`。两条边界（照抄，别改成「更聪明」的实现）：
//!
//! 1. **只收平台推来的**：不提供「在节点上凭空造一条索引」的接口 —— 索引的权威在平台，
//!    节点侧开放自建就会立刻分叉出两个真相。所以 `POST /api/index` 的语义是
//!    **接收推送**（带 `id`），幂等：同一条重推是覆盖（幂等键 = 平台的索引 id）。
//! 2. **本地匹配不算信誉权重**：评分（star）只存在平台，是平台的内部权重；副本不假装
//!    自己知道全网信誉 —— 本地排序只有相关度，响应里也**没有评分字段**。
//!
//! 刻意的取舍：
//!
//! * 请求体走 `Bytes` + 手工 `serde_json`：与 Go 的 `ShouldBindJSON` 一致 ——
//!   非法 body 回 `400 bad_request`（而不是 axum `Json<T>` 默认的 422）。
//! * `pushedAt` 解析不出来就留空（不编一个假时间）；Go 那边是零值时间，
//!   落库/回显的形态不同，但「没有就是没有」的语义一致。
//! * 响应里的时间字段（`updatedAt` / `receivedAt` / `pushedAt`）按库里存的文本原样回，
//!   与其它已迁移族一样（不在这里做一次 RFC3339 转换）。
//! * 切词与打分**复制一份**而不是与平台共用：两个仓库各自发版，跨仓共享代码会让
//!   「节点升级」与「平台升级」绑在一起（内网节点常常比平台旧，这条自由度是刻意的）。

use axum::body::Bytes;
use axum::extract::State;
use axum::http::Uri;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::timeutil::parse_time_or_epoch;
use ncc_core::web;

use crate::httpapi::{AppState, Auth};
use crate::store;

/// 标题上限（超出就截断，不拒收：平台给的条目不该因为长一点就整条丢）。
const MAX_INDEX_TITLE: usize = 120;

/// 该族路由（相对 `/api`）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/index", get(list_index).post(push_index))
        .route("/index/", get(list_index).post(push_index))
        .route("/index/channels", get(index_channels))
        .route("/match", get(match_index))
}

/// 本族没有顶层公开页（索引读口本来就在 `/api` 下公开）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
}

/// 平台推来的一条索引（字段名与平台 `GET /api/index` 的条目一致，所以不需要转换表）。
///
/// 每个字段都容忍 JSON `null`（Go 的非指针字段对 `null` 就是留零值）——
/// 平台哪天多送一个 `null`，不该让整条推送变成 400。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PushReq {
    #[serde(deserialize_with = "de_str")]
    id: String,
    #[serde(rename = "ownerKey", deserialize_with = "de_str")]
    owner_key: String,
    #[serde(deserialize_with = "de_str")]
    owner: String,
    #[serde(rename = "displayName", deserialize_with = "de_str")]
    display_name: String,
    #[serde(deserialize_with = "de_str")]
    kind: String,
    #[serde(deserialize_with = "de_str")]
    channel: String,
    #[serde(deserialize_with = "de_str")]
    slug: String,
    #[serde(rename = "ref", deserialize_with = "de_str")]
    ref_: String,
    #[serde(deserialize_with = "de_str")]
    title: String,
    #[serde(deserialize_with = "de_str")]
    summary: String,
    #[serde(deserialize_with = "de_str")]
    description: String,
    #[serde(deserialize_with = "de_str")]
    category: String,
    #[serde(deserialize_with = "de_str")]
    region: String,
    #[serde(deserialize_with = "de_str_list")]
    tags: Vec<String>,
    #[serde(deserialize_with = "de_str_list")]
    intents: Vec<String>,
    #[serde(deserialize_with = "de_str_list")]
    languages: Vec<String>,
    #[serde(deserialize_with = "de_str")]
    protocol: String,
    #[serde(deserialize_with = "de_str")]
    endpoint: String,
    #[serde(deserialize_with = "de_str")]
    visibility: String,
    #[serde(deserialize_with = "de_str")]
    status: String,
    #[serde(rename = "pushedAt", deserialize_with = "de_str")]
    pushed_at: String,
    provider: Provider,
    source: SourceRef,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Provider {
    #[serde(deserialize_with = "de_str")]
    handle: String,
    #[serde(rename = "displayName", deserialize_with = "de_str")]
    display_name: String,
    #[serde(rename = "providerKind", deserialize_with = "de_str")]
    provider_kind: String,
    #[serde(deserialize_with = "de_str")]
    region: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SourceRef {
    #[serde(rename = "ref", deserialize_with = "de_str")]
    ref_: String,
}

/// `null` 当空串（Go 的非指针字符串就是这样）。
fn de_str<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// `null` 当空列表。
fn de_str_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

/// POST /api/index —— 接收一条推来的索引（需 `index:write`；同一条重推是覆盖）。
async fn push_index(State(state): State<AppState>, auth: Auth, body: Bytes) -> ApiResult<Response> {
    auth.require_scope("index:write")?;
    let req: PushReq = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("bad_request", "请求体格式错误"))?;
    if req.id.trim().is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺少 id：索引的权威在平台，节点只收平台推来的条目（重推按 id 覆盖）",
        ));
    }
    let channel = normalize_channel(&req.channel);
    if channel.is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺少 channel（频道，如 booking/hotel）",
        ));
    }
    let title = req.title.trim();
    if title.is_empty() {
        return Err(ApiError::bad_request("bad_request", "缺少 title"));
    }
    let kind = {
        let k = req.kind.trim();
        if k.is_empty() {
            "service".to_string()
        } else {
            k.to_string()
        }
    };
    // 登记人身份：优先用平台给的名片信息，没有就用 owner 兜底。
    let mut owner = req.provider.handle.trim().to_string();
    let mut display = req.provider.display_name.trim().to_string();
    if owner.is_empty() {
        owner = req.owner.trim().to_string();
    }
    if display.is_empty() {
        display = req.display_name.trim().to_string();
    }
    let mut ref_ = req.ref_.trim().to_string();
    if ref_.is_empty() {
        ref_ = req.source.ref_.trim().to_string();
    }
    let region = first_non_empty(&[req.region.trim(), req.provider.region.trim()]);

    let input = store::index::IndexInput {
        source_id: req.id.trim().to_string(),
        owner_key: req.owner_key.trim().to_string(),
        owner: truncate_runes(&owner, 80),
        display_name: truncate_runes(&display, 80),
        provider_kind: req.provider.provider_kind.trim().to_string(),
        region: truncate_runes(&region, 60),
        kind,
        channel,
        slug: req.slug.trim().to_string(),
        ref_: truncate_runes(&ref_, 120),
        title: truncate_runes(title, MAX_INDEX_TITLE),
        summary: truncate_runes(&req.summary, 300),
        description: truncate_runes(&req.description, 4000),
        category: req.category.trim().to_string(),
        tags: clean_list(&req.tags, 12, 24),
        intents: clean_list(&req.intents, 20, 40),
        languages: clean_list(&req.languages, 6, 12),
        protocol: truncate_runes(&req.protocol, 20),
        endpoint: truncate_runes(&req.endpoint, 400),
        visibility: or_default(&req.visibility, "public"),
        status: or_default(&req.status, "active"),
        pushed_at: if req.pushed_at.trim().is_empty() {
            None
        } else {
            Some(req.pushed_at.clone())
        },
    };
    let v = store::index::upsert_index(state.pool(), &input)
        .await
        .map_err(|e| {
            tracing::error!("接收索引失败: {e}");
            ApiError::internal("接收索引失败")
        })?;
    Ok(super::helpers::ok_json(json!({
        "ok": true,
        "id": v.id,
        "sourceId": v.source_id,
        "channel": v.channel,
        // 与 Go 同形：`updated` 表示「库里已有一行有效的 updatedAt」——
        // 新建与覆盖都写了 updated_at，所以这里恒为 true（保留这个字段是为了不改响应契约）。
        "updated": v.updated_at.is_some(),
    })))
}

/// GET /api/index?channel=&kind=&side=&q=&region=&limit=
async fn list_index(State(state): State<AppState>, uri: Uri) -> ApiResult<Response> {
    let rows = store::index::list_index(
        state.pool(),
        &store::index::IndexListOpts {
            channel: normalize_channel(&q(&uri, "channel")),
            kind: q(&uri, "kind").trim().to_string(),
            side: q(&uri, "side").trim().to_string(),
            q: q(&uri, "q").trim().to_string(),
            region: q(&uri, "region").trim().to_string(),
            public_only: true,
            limit: int_param(&uri, "limit", 40),
        },
    )
    .await
    .map_err(|e| {
        tracing::error!("读取索引失败: {e}");
        ApiError::internal("读取索引失败")
    })?;
    let list: Vec<Value> = rows.iter().map(index_json).collect();
    Ok(super::helpers::ok_json(json!({
        "index": list,
        "total": list.len(),
        "source": "node",
        "kind": "node",
    })))
}

/// GET /api/index/channels
async fn index_channels(State(state): State<AppState>) -> ApiResult<Response> {
    let rows = store::index::index_channels(state.pool())
        .await
        .map_err(|e| {
            tracing::error!("读取频道失败: {e}");
            ApiError::internal("读取频道失败")
        })?;
    let list: Vec<Value> = rows
        .iter()
        .map(|(channel, entries)| json!({"channel": channel, "entries": entries}))
        .collect();
    Ok(super::helpers::ok_json(json!({
        "channels": list,
        "total": list.len(),
        "source": "node",
        "kind": "node",
    })))
}

/// GET /api/match?intent=&channel=&region=&side=&limit=
///
/// 本地匹配：只在本节点收到的索引里找。排序 = 相关度（**没有信誉权重**）。
async fn match_index(State(state): State<AppState>, uri: Uri) -> ApiResult<Response> {
    let intent = truncate_runes(&q(&uri, "intent"), 200);
    let channel = normalize_channel(&q(&uri, "channel"));
    let region = truncate_runes(&q(&uri, "region"), 60);
    let side = or_default(&q(&uri, "side"), "supply");
    if side != "supply" && side != "need" {
        return Err(ApiError::bad_request(
            "bad_request",
            "side 只能是 supply 或 need",
        ));
    }
    if intent.trim().is_empty() && channel.is_empty() && region.is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "请给出 intent（一句需求）或 channel / region 之一",
        ));
    }
    let mut limit = int_param(&uri, "limit", 10);
    if limit < 1 {
        limit = 10;
    }
    if limit > 50 {
        limit = 50;
    }

    let rows = store::index::list_index(
        state.pool(),
        &store::index::IndexListOpts {
            channel: channel.clone(),
            side: side.clone(),
            region: region.clone(),
            public_only: true,
            limit: 500,
            ..Default::default()
        },
    )
    .await
    .map_err(|e| {
        tracing::error!("读取索引失败: {e}");
        ApiError::internal("读取索引失败")
    })?;

    let tokens = tokenize_intent(&intent);
    let mut hits: Vec<(i64, Vec<String>, &store::index::IndexEntry)> = Vec::new();
    for row in &rows {
        let (score, reasons) = index_local_score(row, &tokens, &channel, &region);
        if score == 0 {
            continue;
        }
        hits.push((score, reasons, row));
    }
    // 同分时**新的在前**（SQL 已是 updated_at DESC，稳定排序保住这个顺序）。
    hits.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            let ta = parse_time_or_epoch(a.2.updated_at.as_deref().unwrap_or_default());
            let tb = parse_time_or_epoch(b.2.updated_at.as_deref().unwrap_or_default());
            tb.cmp(&ta)
        })
    });
    let hits: Vec<_> = hits.into_iter().take(limit as usize).collect();

    let ids: Vec<String> = hits.iter().map(|h| h.2.id.clone()).collect();
    let results: Vec<Value> = hits
        .iter()
        .map(|(score, reasons, row)| {
            json!({"score": score, "reasons": reasons, "index": index_json(row)})
        })
        .collect();
    store::index::bump_index_hits(state.pool(), &ids)
        .await
        .map_err(|e| {
            tracing::error!("记录召回失败: {e}");
            ApiError::internal("记录召回失败")
        })?;

    Ok(super::helpers::ok_json(json!({
        "query": {"intent": intent, "channel": channel, "side": side, "region": region},
        "results": results,
        "count": results.len(),
        "candidates": rows.len(),
        "source": "node",
        "kind": "node",
        // 说清这里少了什么：本地副本没有平台的信誉权重
        "note": "本地索引：排序只有相关度（信誉权重在平台侧，节点不持有评分）",
    })))
}

/* ---------------- 打分（节点侧：只有相关度） ---------------- */

/// 本地打分：只算相关度，**没有信誉权重**。
fn index_local_score(
    e: &store::index::IndexEntry,
    tokens: &[String],
    channel: &str,
    region: &str,
) -> (i64, Vec<String>) {
    let mut relevance: i64 = 0;
    // textSignal：「真的读懂了你在说什么」的那部分。频道 / 区域只用来缩小范围，
    // 不能单独把人召回（与平台同一套口径）。
    let mut text_signal: i64 = 0;
    let mut reasons: Vec<String> = Vec::new();

    if !channel.is_empty() {
        if e.channel == channel {
            relevance += 30;
            reasons.push(format!("频道命中：{}", e.channel));
        } else if e.channel.starts_with(&format!("{channel}/")) {
            relevance += 18;
            reasons.push(format!("频道命中：{}", e.channel));
        }
    }
    if !tokens.is_empty() {
        let tags = store::parse_list(&e.tags);
        let intents = store::parse_list(&e.intents);
        let mut hit_tags: Vec<String> = Vec::new();
        for tk in tokens {
            for h in tags.iter().chain(intents.iter()) {
                if h.to_lowercase() == tk.to_lowercase() {
                    hit_tags.push(h.clone());
                    break;
                }
            }
        }
        let n = hit_tags.len() as i64 * 8;
        if n > 32 {
            relevance += 32;
            text_signal += 32;
        } else {
            relevance += n;
            text_signal += n;
        }
        if !hit_tags.is_empty() {
            hit_tags.truncate(5);
            reasons.push(format!("标签 / 关键词命中：{}", hit_tags.join("、")));
        }

        let title = e.title.to_lowercase();
        let ch = e.channel.to_lowercase();
        let body = format!("{} {} {}", e.summary, e.description, intents.join(" ")).to_lowercase();
        let mut weight: i64 = 0;
        let mut hits: Vec<String> = Vec::new();
        for tk in tokens {
            let w = if title.contains(tk.as_str()) {
                3
            } else if ch.contains(tk.as_str()) {
                2
            } else if body.contains(tk.as_str()) {
                1
            } else {
                0
            };
            if w > 0 {
                weight += w;
                hits.push(tk.clone());
            }
        }
        if weight > 40 {
            weight = 40;
        }
        relevance += weight;
        text_signal += weight;
        if !hits.is_empty() {
            // 长的优先、短的若被长的包含就不重复列（可读性处理，与平台一致）。
            hits.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()));
            let mut keep: Vec<String> = Vec::new();
            for tk in &hits {
                if !keep.iter().any(|k| k.contains(tk.as_str())) {
                    keep.push(tk.clone());
                }
            }
            keep.truncate(4);
            reasons.push(format!("关键词命中：{}", keep.join("、")));
        }
    }
    if !region.is_empty() && e.region_match(region) {
        relevance += 10;
        reasons.push(format!("区域覆盖：{}", e.region));
    }
    // 说了话却一个字都没命中：不匹配（频道 / 区域不能单独召回）
    if !tokens.is_empty() && text_signal == 0 {
        return (0, Vec::new());
    }
    if relevance == 0 {
        return (0, Vec::new());
    }
    if !e.endpoint.is_empty() {
        relevance += 4;
        reasons.push("公开可直接接取".to_string());
    }
    if let Some(t) = e
        .updated_at
        .as_deref()
        .and_then(ncc_core::timeutil::parse_time)
    {
        let now = chrono::Local::now().fixed_offset();
        if (now - t).num_hours() < 30 * 24 {
            relevance += 4;
        }
    }
    (relevance, reasons)
}

/* ---------------- 视图与小工具 ---------------- */

/// 节点侧的索引视图。**没有评分字段**（节点不持有评分）。
fn index_json(e: &store::index::IndexEntry) -> Value {
    let mut out = json!({
        "id": e.id,
        "sourceId": e.source_id,
        "kind": e.kind,
        "channel": e.channel,
        "slug": e.slug,
        "ref": e.ref_,
        "title": e.title,
        "summary": e.summary,
        "description": e.description,
        "category": e.category,
        "region": e.region,
        "tags": store::parse_list(&e.tags),
        "intents": store::parse_list(&e.intents),
        "languages": store::parse_list(&e.languages),
        "protocol": e.protocol,
        "endpoint": e.endpoint,
        "visibility": e.visibility,
        "status": e.status,
        "hits": e.hits,
        "updatedAt": e.updated_at,
        "receivedAt": e.received_at,
        "pushedAt": e.pushed_at,
        "provider": {
            "handle": e.owner,
            "displayName": e.display_name,
            "providerKind": e.provider_kind,
            "region": e.region,
        },
    });
    if !e.endpoint.is_empty() {
        out["howTo"] = json!({"endpoint": e.endpoint, "protocol": e.protocol, "open": true});
    } else if !e.ref_.is_empty() {
        out["howTo"] = json!({"step": format!("引用原件：{}", e.ref_)});
    }
    out
}

/// 频道规范化（与平台同一套规则：小写、去空段、下划线/点/空格统一成段分隔符，
/// 段内连字符折叠 —— `Food____RES` 要与 `food-res` 是同一个频道）。
fn normalize_channel(s: &str) -> String {
    let mut s = s.trim().to_lowercase();
    for c in ['_', ' ', '.'] {
        s = s.replace(c, "-");
    }
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.split('/')
        .map(|p| p.trim_matches('-'))
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

fn q(uri: &Uri, key: &str) -> String {
    web::query(uri, key).unwrap_or_default()
}

/// 空值兜底（Go 的 `orDefault`）。
fn or_default(v: &str, def: &str) -> String {
    let v = v.trim();
    if v.is_empty() {
        def.to_string()
    } else {
        v.to_string()
    }
}

fn first_non_empty(vals: &[&str]) -> String {
    for v in vals {
        if !v.trim().is_empty() {
            return v.trim().to_string();
        }
    }
    String::new()
}

/// 去重、截断的字符串列表上限（转成字符串形态，便于直接进 `IndexInput`）。
fn clean_list(in_: &[String], max_items: usize, max_len: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for v in in_ {
        let v = truncate_runes(v, max_len);
        if v.is_empty() || out.contains(&v) {
            continue;
        }
        out.push(v);
        if out.len() >= max_items {
            break;
        }
    }
    out
}

/// 按**字符**（不是字节）截断，避免把中文截成半个字。
fn truncate_runes(s: &str, n: usize) -> String {
    let t = s.trim();
    t.chars().take(n).collect()
}

/// 查询串里的整数：非数字**整体**回落默认值（Go 的 `intParam` 口径）。
fn int_param(uri: &Uri, key: &str, def: i64) -> i64 {
    let v = q(uri, key);
    let v = v.trim();
    if v.is_empty() {
        return def;
    }
    if !v.chars().all(|c| c.is_ascii_digit()) {
        return def;
    }
    v.parse::<i64>().unwrap_or(def)
}

/// 与平台同口径的意图切词：空白与标点切分，中文补 2-4 字滑窗。
fn tokenize_intent(s: &str) -> Vec<String> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return Vec::new();
    }
    let seps = |c: char| {
        matches!(
            c,
            ' ' | '\t'
                | '\n'
                | ','
                | '.'
                | ';'
                | ':'
                | '!'
                | '?'
                | '/'
                | '\\'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '"'
                | '\''
                | '“'
                | '”'
                | '，'
                | '。'
                | '、'
                | '；'
                | '：'
                | '！'
                | '？'
                | '（'
                | '）'
                | '「'
                | '」'
                | '-'
                | '_'
                | '+'
                | '@'
                | '#'
                | '*'
                | '|'
                | '~'
                | '='
                | '<'
                | '>'
        )
    };
    let mut out: Vec<String> = Vec::new();
    for f in s.split(seps).filter(|f| !f.is_empty()) {
        if f.is_ascii() {
            add_token(&mut out, f);
            continue;
        }
        let runes: Vec<char> = f.chars().collect();
        if runes.len() <= 8 {
            add_token(&mut out, f);
        }
        for size in (2..=4).rev() {
            if size > runes.len() {
                continue;
            }
            for i in 0..=(runes.len() - size) {
                let t: String = runes[i..i + size].iter().collect();
                add_token(&mut out, &t);
            }
        }
    }
    out.truncate(24);
    out
}

/// 加一个词：短于 2 个字符、重复的都丢掉。
fn add_token(out: &mut Vec<String>, t: &str) {
    let t = t.trim();
    if t.chars().count() < 2 || out.iter().any(|x| x == t) {
        return;
    }
    out.push(t.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ncc_core::scope::AuthInfo;
    use ncc_core::secretbox::SecretBox;
    use ncc_core::storage::LocalStorage;
    use sqlx::SqlitePool;

    async fn test_state(tag: &str) -> (AppState, std::path::PathBuf) {
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://localhost:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();

        let dir = std::env::temp_dir().join(format!("ncc-index-http-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool: SqlitePool = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL)
            .await
            .unwrap();

        let blobs =
            Arc::new(LocalStorage::new(&dir.join("blobs"), &cfg.public_url, "blobs").unwrap());
        let seal = Arc::new(SecretBox::new(&cfg.jwt_secret).unwrap());
        (
            AppState {
                cfg: Arc::new(cfg),
                pool,
                blobs,
                seal,
            },
            dir,
        )
    }

    fn key_auth(uid: &str, scopes: &[&str]) -> Auth {
        Auth(Some(AuthInfo {
            user_id: uid.to_string(),
            email: format!("{}@example.com", uid.to_lowercase()),
            kind: "key".to_string(),
            session: false,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }))
    }

    fn json_of(r: ApiResult<Response>) -> (axum::http::StatusCode, Value) {
        match r {
            Ok(resp) => (resp.status(), Value::Null),
            Err(e) => (
                e.status,
                json!({"error": {"code": e.code, "message": e.message}}),
            ),
        }
    }

    async fn body_of(r: ApiResult<Response>) -> (axum::http::StatusCode, Value) {
        match r {
            Ok(resp) => {
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (status, serde_json::from_slice(&bytes).unwrap())
            }
            Err(e) => (
                e.status,
                json!({"error": {"code": e.code, "message": e.message}}),
            ),
        }
    }

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    fn push_body(id: &str, channel: &str, title: &str) -> Bytes {
        Bytes::from(
            serde_json::to_vec(&json!({
                "id": id,
                "ownerKey": "U-9",
                "owner": "old-name",
                "displayName": "旧名字",
                "kind": "service",
                "channel": channel,
                "slug": "demo",
                "title": title,
                "summary": "摘要",
                "description": "描述",
                "category": "life",
                "region": "华东",
                "tags": ["酒店", "订房"],
                "intents": ["订房"],
                "languages": ["zh"],
                "protocol": "https",
                "endpoint": "https://example.com",
                "pushedAt": "2026-01-01T10:00:00Z",
                "provider": {"handle": "@alice", "displayName": "Alice", "providerKind": "user", "region": "华东"},
                "source": {"ref": "@alice/demo"},
            }))
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn 推送_幂等与校验() {
        let (st, _d) = test_state("push").await;

        // 没凭据 → 401；有凭据但缺作用域 → 403
        let anon = push_index(
            State(st.clone()),
            Auth(None),
            push_body("IX-1", "food/hotel", "酒店预订"),
        )
        .await;
        assert_eq!(json_of(anon).0, axum::http::StatusCode::UNAUTHORIZED);
        let noscope = push_index(
            State(st.clone()),
            key_auth("U-1", &["index:read"]),
            push_body("IX-1", "food/hotel", "酒店预订"),
        )
        .await;
        let (status, v) = json_of(noscope);
        assert_eq!(status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], json!("forbidden"));

        // 正常推送
        let (status, v) = body_of(
            push_index(
                State(st.clone()),
                key_auth("U-1", &["index:write"]),
                push_body("IX-1", "Food____RES", "酒店预订"),
            )
            .await,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["sourceId"], json!("IX-1"));
        assert_eq!(v["channel"], json!("food-res")); // 频道规范化
        assert_eq!(v["updated"], json!(true));

        // 重推同一条：覆盖而不是新增
        let (_, v2) = body_of(
            push_index(
                State(st.clone()),
                key_auth("U-1", &["index:write"]),
                push_body("IX-1", "food/res", "酒店预订 v2"),
            )
            .await,
        )
        .await;
        assert_eq!(v2["id"], v["id"]);
        assert_eq!(store::index::count_index(st.pool()).await.unwrap(), 1);
        let row = store::index::by_source_id(st.pool(), "IX-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.title, "酒店预订 v2");
        // 名片优先于 owner 兜底
        assert_eq!(row.owner, "@alice");
        assert_eq!(row.display_name, "Alice");
        assert_eq!(row.ref_, "@alice/demo");
        assert!(row.pushed_at.is_some());

        // 校验：缺 id / 缺 channel / 缺 title / 非法 body
        for (body, expect) in [
            (
                Bytes::from_static(br#"{"channel":"x","title":"t"}"#),
                "缺少 id",
            ),
            (
                Bytes::from_static(br#"{"id":"IX-2","title":"t"}"#),
                "缺少 channel",
            ),
            (
                Bytes::from_static(br#"{"id":"IX-2","channel":"x"}"#),
                "缺少 title",
            ),
            (Bytes::from_static(b"not json"), "请求体格式错误"),
        ] {
            let (status, v) = json_of(
                push_index(State(st.clone()), key_auth("U-1", &["index:write"]), body).await,
            );
            assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
            assert_eq!(v["error"]["code"], json!("bad_request"));
            assert!(
                v["error"]["message"].as_str().unwrap().contains(expect),
                "期望包含 {expect}，实际 {}",
                v["error"]["message"]
            );
        }

        // 没给名片时用 owner 兜底；kind 缺省 service
        let (_, v) = body_of(
            push_index(
                State(st.clone()),
                key_auth("U-1", &["index:write"]),
                Bytes::from_static(br#"{"id":"IX-3","channel":"dev","title":"t","owner":"bob"}"#),
            )
            .await,
        )
        .await;
        assert_eq!(v["channel"], json!("dev"));
        let row = store::index::by_source_id(st.pool(), "IX-3")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.owner, "bob");
        assert_eq!(row.kind, "service");
        assert!(row.pushed_at.is_none());
    }

    #[tokio::test]
    async fn 列表_频道_与匹配打分() {
        let (st, _d) = test_state("match").await;
        for (id, channel, title, region) in [
            ("IX-1", "food/hotel", "酒店预订平台", "华东"),
            ("IX-2", "dev/web", "找人做官网", "全国"),
            ("IX-3", "dev/web", "另一个人", "华南"),
        ] {
            let mut body: Value = serde_json::from_slice(&push_body(id, channel, title)).unwrap();
            body["region"] = json!(region);
            // 只有第一条真的跟「订房」有关：标签 / 意向 / 标题都不沾边的那些不该被召回
            body["intents"] = json!(if id == "IX-1" {
                vec!["订房", "住宿"]
            } else {
                vec![]
            });
            body["tags"] = json!(if id == "IX-1" {
                vec!["酒店", "订房"]
            } else {
                vec![]
            });
            push_index(
                State(st.clone()),
                key_auth("U-1", &["index:write"]),
                Bytes::from(serde_json::to_vec(&body).unwrap()),
            )
            .await
            .unwrap();
        }

        // 列表：公开读口，不需要凭据
        let (status, v) = body_of(list_index(State(st.clone()), uri("/api/index")).await).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["total"], json!(3));
        assert_eq!(v["source"], json!("node"));
        assert_eq!(v["index"].as_array().unwrap().len(), 3);
        // 节点视图里没有评分字段
        assert!(v["index"][0].get("score").is_none());
        assert!(v["index"][0]["provider"]["handle"].is_string());

        // 列表过滤：频道前缀 + kind
        let (_, v) = body_of(
            list_index(
                State(st.clone()),
                uri("/api/index?channel=food&kind=service"),
            )
            .await,
        )
        .await;
        assert_eq!(v["total"], json!(1));
        assert_eq!(v["index"][0]["channel"], json!("food/hotel"));

        // 频道聚合
        let (_, v) = body_of(index_channels(State(st.clone())).await).await;
        assert_eq!(v["total"], json!(2));
        assert_eq!(
            v["channels"][0],
            json!({"channel": "dev/web", "entries": 2})
        );
        assert_eq!(
            v["channels"][1],
            json!({"channel": "food/hotel", "entries": 1})
        );

        // 匹配：intent 命中标签 / 标题 → 有分数与理由
        let (status, v) = body_of(
            match_index(
                State(st.clone()),
                uri("/api/match?intent=%E8%AE%A2%E6%88%BF&limit=3"),
            )
            .await,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(v["count"], json!(1));
        assert_eq!(v["candidates"], json!(3)); // side=supply 只看供给侧（这里三条都是 service）
        assert!(v["results"][0]["score"].as_i64().unwrap() > 0);
        assert!(v["results"][0].get("index").unwrap().get("score").is_none());
        assert!(v["results"][0]["reasons"].as_array().unwrap().len() >= 1);
        // 召回次数被记下来
        let row = store::index::by_source_id(st.pool(), "IX-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.hits, 1);

        // 说了话却一个字都没命中：不召回（频道 / 区域不能单独把人召回）
        let (_, v) = body_of(
            match_index(
                State(st.clone()),
                uri("/api/match?intent=%E6%AF%AB%E6%97%A0%E5%85%B3%E7%B3%BB%E7%9A%84%E8%AF%8D"),
            )
            .await,
        )
        .await;
        assert_eq!(v["count"], json!(0));

        // 只有频道 / 区域、没有 intent：仍能按频道召回
        let (_, v) =
            body_of(match_index(State(st.clone()), uri("/api/match?channel=dev/web")).await).await;
        assert_eq!(v["count"], json!(2));

        // region 与 side 校验
        let (status, v) =
            json_of(match_index(State(st.clone()), uri("/api/match?side=both")).await);
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], json!("side 只能是 supply 或 need"));
        let (status, v) = json_of(match_index(State(st.clone()), uri("/api/match")).await);
        assert_eq!(status, axum::http::StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("请给出 intent"));

        // limit 越界被夹到 1..50
        let (_, v) = body_of(
            match_index(
                State(st.clone()),
                uri("/api/match?channel=dev/web&limit=999"),
            )
            .await,
        )
        .await;
        assert_eq!(v["count"], json!(2));
    }

    #[test]
    fn 频道规范化_截断_切词与整数参数() {
        assert_eq!(normalize_channel(" Food____RES "), "food-res");
        assert_eq!(normalize_channel("food/_hotel/"), "food/hotel");
        assert_eq!(normalize_channel("///"), "");
        assert_eq!(normalize_channel("A.B C"), "a-b-c");

        assert_eq!(truncate_runes("  你好世界  ", 2), "你好");
        assert_eq!(truncate_runes("abc", 10), "abc");

        let toks = tokenize_intent("订房 Book 一下");
        assert!(toks.contains(&"book".to_string()));
        assert!(toks.contains(&"订房".to_string()));
        assert!(!toks.contains(&"一".to_string()), "单字不进词表");

        assert_eq!(int_param(&uri("/x?limit=7"), "limit", 40), 7);
        assert_eq!(int_param(&uri("/x?limit=abc"), "limit", 40), 40);
        assert_eq!(int_param(&uri("/x?limit=7x"), "limit", 40), 40);
        assert_eq!(int_param(&uri("/x"), "limit", 40), 40);

        assert_eq!(or_default("  ", "public"), "public");
        assert_eq!(or_default(" private ", "public"), "private");
        assert_eq!(
            clean_list(&[" a ".to_string(), "a".to_string()], 12, 24),
            vec!["a"]
        );
    }

    #[tokio::test]
    async fn 打分_只有相关度_没有信誉() {
        let e = store::index::IndexEntry {
            id: "NI-1".into(),
            source_id: "IX-1".into(),
            owner_key: "U-1".into(),
            owner: "@alice".into(),
            display_name: "Alice".into(),
            provider_kind: "user".into(),
            region: "全国".into(),
            kind: "service".into(),
            channel: "food/hotel".into(),
            slug: "demo".into(),
            ref_: "@alice/demo".into(),
            title: "订房平台".into(),
            summary: "摘要".into(),
            description: "描述".into(),
            category: "life".into(),
            tags: r#"["订房"]"#.to_string(),
            intents: "[]".to_string(),
            languages: "[]".to_string(),
            protocol: "https".into(),
            endpoint: "https://example.com".into(),
            visibility: "public".into(),
            status: "active".into(),
            hits: 0,
            pushed_at: None,
            received_at: None,
            updated_at: Some(ncc_core::timeutil::now_go()),
        };

        // 频道精确命中 30 + 标签命中 8 + 标题命中 3 + 区域 10 + 有 endpoint 4 + 新鲜 4
        let (score, reasons) =
            index_local_score(&e, &tokenize_intent("订房"), "food/hotel", "上海");
        assert_eq!(score, 30 + 8 + 3 + 10 + 4 + 4);
        assert!(reasons.iter().any(|r| r.starts_with("频道命中：")));
        assert!(reasons.iter().any(|r| r.starts_with("标签 / 关键词命中：")));
        assert!(reasons.iter().any(|r| r.starts_with("关键词命中：")));
        assert!(reasons.iter().any(|r| r == "区域覆盖：全国"));
        assert!(reasons.iter().any(|r| r == "公开可直接接取"));

        // 子频道只给 18 分
        let (sub, _) = index_local_score(&e, &tokenize_intent("订房"), "food", "");
        assert_eq!(sub, 18 + 8 + 3 + 4 + 4);

        // 有词但一个字都没命中 → 0（频道 / 区域不能单独召回）
        let (none, _) =
            index_local_score(&e, &tokenize_intent("毫无关系的词"), "food/hotel", "上海");
        assert_eq!(none, 0);

        // 没有词：频道/区域可以单独召回
        let (by_region, reasons) = index_local_score(&e, &[], "", "上海");
        assert_eq!(by_region, 10 + 4 + 4);
        assert_eq!(reasons[0], "区域覆盖：全国");

        // 什么都没有 → 0
        assert_eq!(index_local_score(&e, &[], "", "").0, 0);
    }
}
