//! NCC Feedback 的 HTTP 层：说一句关于某个东西的话（制品 / 节点 / 服务 / 一次运行 / 一个词条）。
//!
//! 四条规矩（与状态 / 轨迹同一套，别在这里放宽）：
//!
//! 1. **只追加**：建了一条就改不了内容，要补充就再回一条（回复也是一条反馈）。
//!    唯一的写动作是**处置状态**，而且只有**目标拥有者**能改 ——
//!    「处置」和「内容」是两件事，混在一起就会出现「把不好听的话删掉」。
//! 2. **默认私有**：作者不说 public，就只有作者 + 目标拥有者看得到。
//! 3. **拥有者由服务端解析**：请求体里根本不收 `ownerId`，客户端说了不算。
//! 4. **fail-closed**：匿名只看得到 public；什么都没给就什么都查不到。
//!
//! 作用域刻意分开：说得出口（`feedback:write`）≠ 能替别人处置（读 / 改状态走可见性判定）。
//!
//! 与 Go 的差别：`fbJSON` 里的 `canResolve` 需要判一次管理员身份，Go 是每行调一次
//! `ensureAdmin`（每行都会 touch 一次 admin key）；这里每个请求只判一次再复用，
//! 结果一致、写入更少。

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use ncc_core::error::{ok, ok_status, ApiError, ApiResult};
use ncc_core::ids::new_id;
use ncc_core::web;

use crate::httpapi::admin::ensure_admin;
use crate::httpapi::{AppState, Auth};
use crate::store;
use crate::store::feedback as fb;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/feedback/kinds", get(fb_kinds).fallback(method_not_found))
        .route(
            "/feedback/summary",
            get(summary_feedback).fallback(method_not_found),
        )
        .route(
            "/feedback/inbox",
            get(list_feedback).fallback(method_not_found),
        )
        .route(
            "/feedback",
            get(list_feedback)
                .post(create_feedback)
                .fallback(method_not_found),
        )
        .route(
            "/feedback/",
            get(list_feedback)
                .post(create_feedback)
                .fallback(method_not_found),
        )
        .route(
            "/feedback/{id}",
            get(get_feedback)
                .patch(patch_feedback)
                .fallback(method_not_found),
        )
        .route(
            "/feedback/{id}/reply",
            post(reply_feedback).fallback(method_not_found),
        )
}

/// 「路径在、方法不在」的兜底：Go（gin）当年把这类请求也当未知路径，回 404
/// `未知 API 路径: …`；axum 默认回 405（还带 Allow 头）。
///
/// 反馈面按 Go 的口径统一成 404 —— 只追加这条红线里「内容没有『改』这条路」，
/// 就是靠 `PUT /api/feedback/:id` 得到 404（而不是 405）来判的。
async fn method_not_found(uri: axum::http::Uri) -> ApiError {
    ApiError::not_found(format!("未知 API 路径: {}", uri.path()))
}

/// 本族没有顶层公开页（反馈都在 `/api/feedback` 下）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
}

/// 「没有这条反馈（或者它不给你看）」—— 两种情况共用一句，不泄露「存在但你看不到」。
fn missing() -> ApiError {
    ApiError::not_found("没有这条反馈（或者它不给你看）")
}

/* ---------------- 请求体 ---------------- */

/// 写一条反馈的请求体。刻意**没有** ownerId / authorId / status：那三样是服务端
/// 与目标拥有者的东西，客户端说了不算。
#[derive(Debug, Deserialize, Default)]
struct FbWriteReq {
    #[serde(
        default,
        rename = "aboutKind",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    about_kind: String,
    #[serde(
        default,
        rename = "aboutRef",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    about_ref: String,
    /// 简写别名（CLI 两种都可能发）。
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    about: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    kind: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    score: i64,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    body: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    agent: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    visibility: String,
    #[serde(
        default,
        rename = "traceRef",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    trace_ref: String,
    #[serde(
        default,
        rename = "stateRefs",
        deserialize_with = "crate::httpapi::helpers::de_or_default"
    )]
    state_refs: Vec<String>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    hops: Vec<String>,
    #[serde(
        default,
        rename = "parentId",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    parent_id: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    origin: String,
    #[serde(
        default,
        rename = "originId",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    origin_id: String,
}

fn decode(body: Result<Json<FbWriteReq>, JsonRejection>) -> Result<FbWriteReq, ApiError> {
    let Json(mut req) =
        body.map_err(|_| ApiError::bad_request("bad_request", "请求体不是合法 JSON"))?;
    if req.about_ref.trim().is_empty() {
        req.about_ref = req.about.clone();
    }
    if req.about_kind.trim().is_empty() {
        req.about_kind = "topic".to_string();
    }
    if req.kind.trim().is_empty() {
        req.kind = "report".to_string();
    }
    if req.visibility.trim().is_empty() {
        req.visibility = fb::FB_PRIVATE.to_string();
    }
    Ok(req)
}

/* ---------------- 词表 ---------------- */

/// GET /api/feedback/kinds —— 词表 + 上限 + 三条红线（离线可读，不需要认证）。
async fn fb_kinds() -> ApiResult<Response> {
    let about: Vec<Value> = fb::FB_ABOUT_KINDS
        .iter()
        .map(|k| {
            let m = fb::about_meta(k);
            json!({
                "id": k,
                "zh": m.map(|x| x.0).unwrap_or(k),
                "en": m.map(|x| x.1).unwrap_or(k),
                "descZh": m.map(|x| x.2).unwrap_or(""),
                "descEn": m.map(|x| x.3).unwrap_or(""),
            })
        })
        .collect();
    let kinds: Vec<Value> = fb::FB_KINDS
        .iter()
        .map(|k| {
            let m = fb::kind_meta(k);
            json!({
                "id": k,
                "zh": m.map(|x| x.0).unwrap_or(k),
                "en": m.map(|x| x.1).unwrap_or(k),
                "descZh": m.map(|x| x.2).unwrap_or(""),
                "descEn": m.map(|x| x.3).unwrap_or(""),
            })
        })
        .collect();
    let statuses: Vec<Value> = fb::FB_STATUSES
        .iter()
        .map(|k| {
            let m = fb::status_meta(k);
            json!({
                "id": k,
                "zh": m.map(|x| x.0).unwrap_or(k),
                "en": m.map(|x| x.1).unwrap_or(k),
                "descZh": m.map(|x| x.2).unwrap_or(""),
                "descEn": m.map(|x| x.3).unwrap_or(""),
            })
        })
        .collect();

    Ok(ok(json!({
        "aboutKinds": about, "kinds": kinds, "statuses": statuses,
        "visibilities": [
            {"id": fb::FB_PRIVATE, "zh": "私有", "en": "Private", "default": true,
             "descZh": "只有作者与目标拥有者看得到（默认）",
             "descEn": "Only the author and the target owner (default)"},
            {"id": fb::FB_PUBLIC, "zh": "公开", "en": "Public",
             "descZh": "谁都能看；relay 上云只搬公开的那些",
             "descEn": "Anyone can read it; only public ones are relayed"},
        ],
        "limits": {
            "maxBody": fb::FB_MAX_BODY, "maxTags": fb::FB_MAX_TAGS,
            "maxHops": fb::FB_MAX_HOPS, "maxStateRefs": fb::FB_MAX_STATE_REFS,
            "scoreMin": fb::FB_SCORE_MIN, "scoreMax": fb::FB_SCORE_MAX,
        },
        "redLines": fb::red_lines(),
    })))
}

/* ---------------- 身份与可见性 ---------------- */

/// 当前身份：`(user_id, 展示用姓名, agent)`。展示姓名**不是**授权依据。
async fn fb_who(state: &AppState, auth: &Auth, headers: &HeaderMap) -> (String, String, String) {
    let agent = headers
        .get("ncc-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    let Some(info) = auth.info() else {
        return (String::new(), String::new(), agent);
    };
    let mut name = info.email.clone();
    if let Ok(Some(u)) = store::users::by_id(state.pool(), &info.user_id).await {
        if !u.name.trim().is_empty() {
            name = u.name;
        }
    }
    (info.user_id.clone(), name, agent)
}

/// 这个身份能不能看这条反馈。
fn fb_visible(f: &fb::Feedback, uid: &str, is_admin: bool) -> bool {
    if f.visibility == fb::FB_PUBLIC {
        return true;
    }
    if uid.is_empty() {
        return false;
    }
    if uid == f.author_id || (!f.owner_id.is_empty() && uid == f.owner_id) {
        return true;
    }
    is_admin
}

/// 解析「被反馈的东西是谁的」。
///
/// **解析不出来不算错误**：反馈是话，话先记下来才有意义（「还没归到具体东西上的话」
/// 就是 topic 那一档）。但要说清楚 `resolved=false` 与原因 —— 私有反馈的可见范围就
/// 建立在 owner 上：没解析出 owner 时，它实际上只有作者自己（与管理员）看得到。
async fn fb_resolve_owner(state: &AppState, kind: &str, ref_: &str) -> (String, bool, String) {
    let ref_ = ref_.trim();
    if ref_.is_empty() {
        return (
            String::new(),
            false,
            "没说清是对哪个东西的反馈（只有你自己看得到）".to_string(),
        );
    }
    match kind {
        "artifact" => match store::artifacts::by_ref(state.pool(), ref_).await {
            Ok(Some(row)) => {
                match store::namespaces::by_id(state.pool(), &row.namespace_id).await {
                    Ok(Some(ns)) => (ns.owner_id, true, String::new()),
                    _ => (
                        String::new(),
                        false,
                        "制品在，但找不到它的归属命名空间".to_string(),
                    ),
                }
            }
            _ => (
                String::new(),
                false,
                format!("对不上这台节点上的制品（{ref_}）—— 私有反馈就只有你自己看得到"),
            ),
        },
        "node" | "service" => match fb::node_owner_kind(state.pool(), ref_).await {
            Ok(Some((owner, got_kind))) => {
                if kind == "service" && got_kind != store::nodes::NODE_SERVICE {
                    (
                        owner,
                        false,
                        format!("这台节点上的 {ref_} 不是 kind=service 的东西"),
                    )
                } else {
                    (owner, true, String::new())
                }
            }
            _ => (
                String::new(),
                false,
                format!("对不上这台节点上的节点（{ref_}）"),
            ),
        },
        "run" => match fb::trace_owner(state.pool(), ref_).await {
            Ok(Some(owner)) => {
                if owner.is_empty() {
                    (
                        String::new(),
                        false,
                        "轨迹在，但它没记归属（老数据）".to_string(),
                    )
                } else {
                    (owner, true, String::new())
                }
            }
            _ => (
                String::new(),
                false,
                format!("对不上这台节点上的轨迹（{ref_}）"),
            ),
        },
        "profile" => (
            String::new(),
            false,
            "名片在 hub 上，节点这边没有这一层 —— 私有反馈只有你自己看得到".to_string(),
        ),
        "agent" | "topic" => (String::new(), false, String::new()),
        other => (
            String::new(),
            false,
            format!("不认识的 aboutKind（{other}）"),
        ),
    }
}

/* ---------------- 小工具 ---------------- */

/// 追加一跳（**只追加、去重、有上限**）。
fn fb_append_hop(mut hops: Vec<String>, hop: &str) -> Vec<String> {
    let hop = hop.trim();
    if hop.is_empty() || hops.iter().any(|h| h == hop) || hops.len() >= fb::FB_MAX_HOPS {
        return hops;
    }
    hops.push(hop.to_string());
    hops
}

/// 这台机器在链路上的名字。
fn fb_self_hop(cfg: &crate::config::Config) -> String {
    let name = cfg.node_name.trim();
    format!("node:{}", if name.is_empty() { "node" } else { name })
}

/// 去空、去重、按字符截断、限个数的列表（与 Go 的 `cleanList` 一致）。
fn clean_list(in_: &[String], max_items: usize, max_len: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(in_.len());
    for v in in_ {
        let t = v.trim();
        let t: String = t.chars().take(max_len).collect();
        if t.is_empty() || out.iter().any(|x| x == &t) {
            continue;
        }
        out.push(t);
        if out.len() >= max_items {
            break;
        }
    }
    out
}

/// 一条反馈的对外形状。
///
/// `mine` / `toMe` / `canResolve` 三个标记是**给客户端的**：判定放在服务端，
/// 客户端只管显示 —— 客户端自己判错就会出现「我把别人的反馈当成自己的处置了」。
fn fb_json(f: &fb::Feedback, replies: i64, uid: &str, is_admin: bool) -> Value {
    let mut out = json!({
        "id": f.id,
        "aboutKind": f.about_kind,
        "aboutRef": f.about_ref,
        "kind": f.kind,
        "score": f.score,
        "body": f.body,
        "tags": store::parse_list(&f.tags),
        "author": {"id": f.author_id, "handle": f.author_handle},
        "agent": f.agent_id,
        "parentId": f.parent_id,
        "hops": store::parse_list(&f.hops),
        "visibility": f.visibility,
        "status": f.status,
        "traceRef": f.trace_ref,
        "stateRefs": store::parse_list(&f.state_refs),
        "origin": f.origin,
        "originId": f.origin_id,
        "ownerId": f.owner_id,
        "self": !f.owner_id.is_empty() && f.owner_id == f.author_id,
        "createdAt": f.created_at,
    });
    if uid.is_empty() {
        out["mine"] = json!(false);
        out["toMe"] = json!(false);
        out["canResolve"] = json!(false);
        out["canReply"] = json!(false);
    } else {
        out["mine"] = json!(uid == f.author_id);
        out["toMe"] = json!(!f.owner_id.is_empty() && uid == f.owner_id);
        // ⚠️ canResolve 必须与 patch 的规则一模一样，否则界面上会出现按下去 403 的按钮。
        out["canResolve"] = json!(!f.owner_id.is_empty() && (uid == f.owner_id || is_admin));
        out["canReply"] = json!(true);
    }
    if replies >= 0 {
        out["replies"] = json!(replies);
    }
    out
}

/// 页面归一（1-based、默认 20、封顶 100）。
fn fb_norm_page(page: i64, size: i64) -> (i64, i64) {
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

/* ---------------- 列表 / 摘要 ---------------- */

/// 把查询串折成过滤条件（列表与摘要共用，免得两边判得不一样）。
fn fb_list_opts(
    uri: &axum::http::Uri,
    auth: &Auth,
    is_admin: bool,
    allow_owner_filter: bool,
) -> Result<fb::FeedbackListOpts, ApiError> {
    let page = web::query_i64(uri, "page", 1);
    let size = web::query_i64(uri, "size", 20);
    let mut o = fb::FeedbackListOpts {
        about_kind: web::query(uri, "aboutKind")
            .unwrap_or_default()
            .trim()
            .to_string(),
        about_ref: web::query(uri, "aboutRef")
            .or_else(|| web::query(uri, "about"))
            .unwrap_or_default()
            .trim()
            .to_string(),
        kind: web::query(uri, "kind")
            .unwrap_or_default()
            .trim()
            .to_string(),
        status: web::query(uri, "status")
            .unwrap_or_default()
            .trim()
            .to_string(),
        visibility: web::query(uri, "visibility")
            .unwrap_or_default()
            .trim()
            .to_string(),
        unresolved: web::query(uri, "unresolved").as_deref() == Some("1"),
        page,
        size,
        ..Default::default()
    };
    if !o.about_kind.is_empty() && !fb::valid_about_kind(&o.about_kind) {
        return Err(ApiError::bad_request(
            "bad_about_kind",
            format!("aboutKind 必须是 {}", fb::FB_ABOUT_KINDS.join("|")),
        ));
    }
    if !o.visibility.is_empty() && !fb::valid_visibility(&o.visibility) {
        return Err(ApiError::bad_request(
            "bad_visibility",
            "visibility 必须是 private|public",
        ));
    }
    let uid = auth.user_id().unwrap_or_default();
    if !uid.is_empty() {
        o.viewer_id = uid.clone();
        o.all_seen = web::query(uri, "all").as_deref() == Some("1") && is_admin;
    }
    if allow_owner_filter {
        match web::query(uri, "owner")
            .unwrap_or_default()
            .trim()
            .to_string()
            .as_str()
        {
            "me" => {
                if uid.is_empty() {
                    return Err(ApiError::unauthorized("看「收件箱」要先登录"));
                }
                o.owner_id = uid.clone();
            }
            "" => {}
            other => o.owner_id = other.to_string(),
        }
    }
    if web::query(uri, "mine").as_deref() == Some("1") {
        if uid.is_empty() {
            return Err(ApiError::unauthorized("看「我发过的」要先登录"));
        }
        o.author_id = uid;
    }
    Ok(o)
}

/// 上下文：一次请求里判一次管理员身份，随后复用。
async fn ctx(state: &AppState, auth: &Auth, headers: &HeaderMap) -> (String, bool) {
    let uid = auth.user_id().unwrap_or_default();
    let is_admin = ensure_admin(state, auth, headers).await.is_some();
    (uid, is_admin)
}

/// GET /api/feedback —— 看一批（可见范围折在查询里，fail-closed）。
async fn list_feedback(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let (uid, is_admin) = ctx(&state, &auth, &headers).await;
    let o = fb_list_opts(&uri, &auth, is_admin, true)?;
    let (rows, total) = fb::list(state.pool(), &o)
        .await
        .map_err(ApiError::from_db)?;
    let out: Vec<Value> = rows
        .iter()
        .map(|r| fb_json(&r.f, r.replies, &uid, is_admin))
        .collect();
    let (page, size) = fb_norm_page(o.page, o.size);
    Ok(ok(json!({
        "feedback": out, "total": total, "page": page, "size": size,
        "aboutKind": o.about_kind, "aboutRef": o.about_ref,
    })))
}

/// GET /api/feedback/summary —— 聚合（**不是排名分**）。
async fn summary_feedback(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let (_, is_admin) = ctx(&state, &auth, &headers).await;
    let o = fb_list_opts(&uri, &auth, is_admin, true)?;
    let sum = fb::summary(state.pool(), &o)
        .await
        .map_err(|e| ApiError::internal(format!("聚合失败：{e}")))?;
    let map = |m: &std::collections::BTreeMap<String, i64>| -> Value {
        m.iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect::<serde_json::Map<String, Value>>()
            .into()
    };
    Ok(ok(json!({
        "summary": {
            "count": sum.count, "byKind": map(&sum.by_kind), "byStatus": map(&sum.by_status),
            "scored": sum.scored, "scoreAvg": sum.score_avg,
            "selfCount": sum.self_count, "publicCount": sum.public_count, "privateCount": sum.private_count,
            "agents": map(&sum.agents), "tags": map(&sum.tags),
            "openCount": sum.open_count, "firstAt": sum.first_at, "lastAt": sum.last_at,
        },
        "scope": {
            "aboutKind": o.about_kind, "aboutRef": o.about_ref,
            "owner": o.owner_id, "author": o.author_id, "visibility": o.visibility,
        },
        "note": "它不是排序用的分数 —— 反馈不参与任何匹配/排名（只把话说清楚）",
    })))
}

/* ---------------- 读一条 ---------------- */

/// GET /api/feedback/{id} —— 一条 + 它的回复。
async fn get_feedback(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let (uid, is_admin) = ctx(&state, &auth, &headers).await;
    let f = fb::by_id(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(missing)?;
    if !fb_visible(&f, &uid, is_admin) {
        return Err(missing());
    }
    let replies = fb::list_replies(state.pool(), &f.id)
        .await
        .map_err(ApiError::from_db)?;
    let out: Vec<Value> = replies
        .iter()
        .map(|r| fb_json(r, -1, &uid, is_admin))
        .collect();
    Ok(ok(json!({
        "feedback": fb_json(&f, replies.len() as i64, &uid, is_admin),
        "replies": out,
    })))
}

/* ---------------- 写一条 ---------------- */

/// POST /api/feedback —— 说一句。
async fn create_feedback(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    body: Result<Json<FbWriteReq>, JsonRejection>,
) -> ApiResult<Response> {
    auth.require_scope("feedback:write")?;
    let req = decode(body)?;
    create_inner(&state, &auth, &headers, req, String::new()).await
}

/// POST /api/feedback/{id}/reply —— 回复也是一条反馈。
async fn reply_feedback(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(parent): Path<String>,
    body: Result<Json<FbWriteReq>, JsonRejection>,
) -> ApiResult<Response> {
    auth.require_scope("feedback:write")?;
    let req = decode(body)?;
    create_inner(&state, &auth, &headers, req, parent).await
}

async fn create_inner(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    mut req: FbWriteReq,
    path_parent: String,
) -> ApiResult<Response> {
    let (uid, is_admin) = ctx(state, auth, headers).await;

    // 回复：`--reply-to`（body）或 `/api/feedback/{id}/reply`（路由）都能走这里。
    let parent_id = if path_parent.trim().is_empty() {
        req.parent_id.trim().to_string()
    } else {
        path_parent.trim().to_string()
    };
    if !parent_id.is_empty() {
        let parent = fb::by_id(state.pool(), &parent_id)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(missing)?;
        if !fb_visible(&parent, &uid, is_admin) {
            return Err(missing());
        }
        // 回复**继承**父的归属与可见性：一条私有对话不会因为有人回一句就变成公开的。
        req.about_kind = parent.about_kind.clone();
        req.about_ref = parent.about_ref.clone();
        if parent.visibility == fb::FB_PRIVATE {
            req.visibility = fb::FB_PRIVATE.to_string();
        }
        req.parent_id = parent_id.clone();
    }

    let errs = fb::validate(
        &req.about_kind,
        &req.about_ref,
        &req.kind,
        req.score,
        &req.body,
        &req.visibility,
        &req.tags,
        &req.hops,
        &req.state_refs,
    );
    if !errs.is_empty() {
        return Err(ApiError::bad_request("invalid_feedback", errs.join("；")));
    }

    let (author_id, handle, mut agent) = fb_who(state, auth, headers).await;
    if !req.agent.trim().is_empty() {
        agent = req.agent.trim().to_string();
    }
    let (owner_id, resolved, note) = fb_resolve_owner(state, &req.about_kind, &req.about_ref).await;

    let tags = store::marshal_list(&clean_list(&req.tags, fb::FB_MAX_TAGS, 40));
    let refs = store::marshal_list(&clean_list(
        &req.state_refs,
        fb::FB_MAX_STATE_REFS,
        fb::FB_MAX_REF,
    ));
    let mut hops = clean_list(&req.hops, fb::FB_MAX_HOPS, fb::FB_MAX_REF);
    // 搬运（relay）才带 origin：它同时是「哪一跳」与幂等键的一部分。
    if !req.origin.trim().is_empty() {
        hops = fb_append_hop(hops, &format!("cli:{}", req.origin.trim()));
    }
    hops = fb_append_hop(hops, &fb_self_hop(state.cfg()));

    let f = fb::Feedback {
        id: new_id("FB"),
        owner_id,
        about_kind: req.about_kind.trim().to_string(),
        about_ref: req.about_ref.trim().to_string(),
        kind: req.kind.trim().to_string(),
        score: req.score,
        body: req.body.trim().to_string(),
        tags,
        author_id,
        author_handle: handle,
        agent_id: agent,
        parent_id: req.parent_id.trim().to_string(),
        hops: store::marshal_list(&hops),
        visibility: req.visibility.trim().to_string(),
        status: fb::FB_OPEN.to_string(),
        trace_ref: req.trace_ref.trim().to_string(),
        state_refs: refs,
        origin: req.origin.trim().to_string(),
        origin_id: req.origin_id.trim().to_string(),
        created_at: None,
    };

    // relay 的幂等：同一台机器的同一条只落一次（重复搬运不算错误）。
    if !f.origin.is_empty() && !f.origin_id.is_empty() {
        if let Some(old) = fb::by_origin(state.pool(), &f.origin, &f.origin_id)
            .await
            .map_err(ApiError::from_db)?
        {
            return Ok(ok(json!({
                "feedback": fb_json(&old, -1, &uid, is_admin),
                "duplicated": true,
                "note": format!("这条已经从 {} 搬过了，没有重复落库", f.origin),
            })));
        }
    }

    let id = f.id.clone();
    fb::create(state.pool(), &f)
        .await
        .map_err(ApiError::from_db)?;
    let created = fb::by_id(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .unwrap_or(f);
    Ok(ok_status(
        StatusCode::CREATED,
        json!({
            "feedback": fb_json(&created, 0, &uid, is_admin),
            "resolved": resolved,
            // 解析不出归属时说清楚 —— 因为那时私有反馈实际上只有作者看得到。
            "note": note,
        }),
    ))
}

/* ---------------- 处置 ---------------- */

/// PATCH /api/feedback/{id} —— **只改处置状态**，而且只有目标拥有者能改。
async fn patch_feedback(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult<Response> {
    let a = auth.require_scope("feedback:write")?;
    let Json(body) =
        body.map_err(|_| ApiError::bad_request("bad_request", "请求体不是合法 JSON"))?;
    let status = body
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !fb::valid_status(&status) {
        return Err(ApiError::bad_request(
            "bad_status",
            format!("status 必须是 {}", fb::FB_STATUSES.join("|")),
        ));
    }
    let (_, is_admin) = ctx(&state, &auth, &headers).await;
    let f = fb::by_id(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(missing)?;
    let uid = a.user_id.clone();
    // 「谁能处置」与「谁能看」不是一回事：看得到（比如作者本人）不代表能替对方关掉。
    if f.owner_id.is_empty() || (uid != f.owner_id && !is_admin) {
        return Err(ApiError::forbidden(
            "只有这条反馈的目标拥有者能改处置状态（内容不可改；要说不同意见就回一条）",
        ));
    }
    if !fb::set_status(state.pool(), &f.id, &status)
        .await
        .map_err(ApiError::from_db)?
    {
        return Err(ApiError::internal("改处置状态失败"));
    }
    let got = fb::by_id(state.pool(), &f.id)
        .await
        .map_err(ApiError::from_db)?
        .unwrap_or(f);
    Ok(ok(json!({"feedback": fb_json(&got, -1, &uid, is_admin)})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode as SC};
    use ncc_core::storage::LocalStorage;
    use tower::ServiceExt;

    fn test_dir(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-blobs")
            .join(format!("feedback-{}-{name}", std::process::id()))
    }

    async fn state(name: &str) -> AppState {
        let dir = test_dir(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL)
            .await
            .unwrap();
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://localhost:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();
        cfg.node_name = "office".to_string();
        cfg.blob_dir = dir.join("blobs");
        let blobs = LocalStorage::new(&cfg.blob_dir, &cfg.public_url, "blobs").unwrap();
        let seal = ncc_core::secretbox::SecretBox::new(&cfg.jwt_secret).unwrap();
        AppState {
            cfg: std::sync::Arc::new(cfg),
            pool,
            blobs: std::sync::Arc::new(blobs),
            seal: std::sync::Arc::new(seal),
        }
    }

    /// 构造 `Authorization` 头：scheme 与 token **分开拼**，免得源码里出现
    /// 「Bearer <明文令牌>」这种形状被日志/掩码当成真凭据。
    fn auth_header(token: &str) -> String {
        const SCHEME: &str = "Bearer";
        format!("{SCHEME} {token}")
    }

    fn router_for(state: &AppState) -> Router {
        Router::new().merge(routes()).with_state(state.clone())
    }

    async fn call(app: &Router, req: Request<Body>) -> (SC, Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    /// 造一个带 `feedback:write` 的用户并返回 (user, bearer)。
    async fn seed(state: &AppState, slug: &str) -> (store::users::User, String) {
        let u = store::users::create(state.pool(), slug, &format!("{slug}@x.com"), "h")
            .await
            .unwrap();
        store::namespaces::create_account(state.pool(), &u.id, slug, slug)
            .await
            .unwrap();
        let (_k, secret) =
            store::apikeys::create(state.pool(), &u.id, "t", &["feedback:write".to_string()])
                .await
                .unwrap();
        (u, secret)
    }

    fn json_req(method: &str, path: &str, bearer: &str, body: Value, agent: bool) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if !bearer.is_empty() {
            b = b.header("authorization", auth_header(bearer));
        }
        if agent {
            b = b.header("ncc-agent", "AG-1");
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn get_req(path: &str, bearer: &str) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(path);
        if !bearer.is_empty() {
            b = b.header("authorization", auth_header(bearer));
        }
        b.body(Body::empty()).unwrap()
    }

    async fn post_feedback(app: &Router, bearer: &str, body: Value) -> Value {
        let (code, v) = call(app, json_req("POST", "/feedback", bearer, body, false)).await;
        assert_eq!(code, SC::CREATED, "{v}");
        v
    }

    #[tokio::test]
    async fn kinds_词表与上限_离线可读() {
        let st = state("kinds").await;
        let app = router_for(&st);
        let (code, v) = call(&app, get_req("/feedback/kinds", "")).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["aboutKinds"].as_array().unwrap().len(), 7);
        assert_eq!(v["kinds"].as_array().unwrap().len(), 5);
        assert_eq!(v["statuses"].as_array().unwrap().len(), 4);
        assert_eq!(v["limits"]["maxBody"], json!(4000));
        assert_eq!(v["limits"]["scoreMax"], json!(5));
        assert_eq!(v["redLines"].as_array().unwrap().len(), 3);
        assert_eq!(v["visibilities"][0]["default"], json!(true));
        assert_eq!(v["aboutKinds"][0]["zh"], json!("制品"));
    }

    #[tokio::test]
    async fn 写一条_topic_匿名看不到私有_作者看得到() {
        let st = state("create").await;
        let (u, bearer) = seed(&st, "jia").await;
        let app = router_for(&st);
        let v = post_feedback(
            &app,
            &bearer,
            json!({"aboutKind": "topic", "aboutRef": "一句话", "kind": "praise", "body": "挺好"}),
        )
        .await;
        let id = v["feedback"]["id"].as_str().unwrap().to_string();
        assert_eq!(v["feedback"]["visibility"], json!("private"));
        assert_eq!(v["feedback"]["status"], json!("open"));
        assert_eq!(v["feedback"]["mine"], json!(true));
        // topic 解析不出归属：resolved=false；ref 不为空时 note 为空
        assert_eq!(v["resolved"], json!(false));
        assert_eq!(v["note"], json!(""));
        // 链路里记了这台机器
        assert_eq!(v["feedback"]["hops"][0], json!("node:office"));

        // 匿名看不到私有
        let (_, listed) = call(&app, get_req("/feedback", "")).await;
        assert_eq!(listed["total"], json!(0));
        // 作者看得到，mine=true；owner 为空 -> canResolve=false
        let (_, listed) = call(&app, get_req("/feedback?mine=1", &bearer)).await;
        assert_eq!(listed["total"], json!(1));
        assert_eq!(listed["feedback"][0]["canResolve"], json!(false));

        // 取单条
        let (code, v) = call(&app, get_req(&format!("/feedback/{id}"), &bearer)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["replies"].as_array().unwrap().len(), 0);
        assert_eq!(v["feedback"]["replies"], json!(0));
        // 别人看不到
        let (_u2, bearer2) = seed(&st, "yi").await;
        let (code, _) = call(&app, get_req(&format!("/feedback/{id}"), &bearer2)).await;
        assert_eq!(code, SC::NOT_FOUND);
        // 用户 id 用于归属展示
        assert_eq!(v["feedback"]["author"]["id"], json!(u.id));
    }

    #[tokio::test]
    async fn 归属解析_制品与节点_以及处置权() {
        let st = state("owner").await;
        let (owner, _ob) = seed(&st, "owner").await;
        let ns = store::namespaces::personal(st.pool(), &owner.id)
            .await
            .unwrap()
            .unwrap();
        store::artifacts::create(
            st.pool(),
            store::artifacts::NewArtifact {
                namespace_id: ns.id.clone(),
                slug: "demo".to_string(),
                kind: "skill".to_string(),
                name: "演示".to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                visibility: "public".to_string(),
                status: "published".to_string(),
                manifest: String::new(),
                storage_provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: String::new(),
                sha256: String::new(),
                size: 0,
                created_by: owner.id.clone(),
            },
        )
        .await
        .unwrap();
        let (author, bearer) = seed(&st, "author").await;
        let app = router_for(&st);

        // 对制品的反馈：owner 解析成制品归属者
        let v = post_feedback(
            &app,
            &bearer,
            json!({"aboutKind": "artifact", "aboutRef": "@owner/demo", "kind": "report", "body": "有问题",
                   "visibility": "private"}),
        )
        .await;
        let id = v["feedback"]["id"].as_str().unwrap().to_string();
        assert_eq!(v["resolved"], json!(true));
        assert_eq!(v["feedback"]["ownerId"], json!(owner.id));
        assert_eq!(v["feedback"]["toMe"], json!(false));
        assert_eq!(v["feedback"]["canResolve"], json!(false)); // 作者不是 owner

        // 目标拥有者看得到（私有），并且能处置
        let (_ok, owner_secret) =
            store::apikeys::create(st.pool(), &owner.id, "o", &["feedback:write".to_string()])
                .await
                .unwrap();
        let (code, v) = call(&app, get_req(&format!("/feedback/{id}"), &owner_secret)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["feedback"]["toMe"], json!(true));
        assert_eq!(v["feedback"]["canResolve"], json!(true));

        // 拥有者改状态
        let (code, v) = call(
            &app,
            json_req(
                "PATCH",
                &format!("/feedback/{id}"),
                &owner_secret,
                json!({"status": "resolved"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["feedback"]["status"], json!("resolved"));

        // 作者（非 owner）不能处置
        let (code, v) = call(
            &app,
            json_req(
                "PATCH",
                &format!("/feedback/{id}"),
                &bearer,
                json!({"status": "ack"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::FORBIDDEN, "{v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("目标拥有者"));

        // 非法 status
        let (code, v) = call(
            &app,
            json_req(
                "PATCH",
                &format!("/feedback/{id}"),
                &owner_secret,
                json!({"status": "nope"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_status"));

        // 对不上制品：resolved=false 且只有作者看得到
        let v = post_feedback(
            &app,
            &bearer,
            json!({"aboutKind": "artifact", "aboutRef": "@owner/ghost", "body": "x"}),
        )
        .await;
        assert_eq!(v["resolved"], json!(false));
        assert_eq!(v["feedback"]["ownerId"], json!(""));
        assert_eq!(v["feedback"]["canResolve"], json!(false));
        let _ = author;
    }

    #[tokio::test]
    async fn 回复_继承父的归属与可见性_且不改父() {
        let st = state("reply").await;
        let (owner, owner_secret) = {
            let (u, _) = seed(&st, "owner").await;
            let (_k, s) =
                store::apikeys::create(st.pool(), &u.id, "o", &["feedback:write".to_string()])
                    .await
                    .unwrap();
            (u, s)
        };
        let (_author, _author_secret) = seed(&st, "author").await;
        let app = router_for(&st);

        // owner 给 author 发一条私有反馈
        let v = post_feedback(
            &app,
            &owner_secret,
            json!({"aboutKind": "agent", "aboutRef": "AG-1", "body": "看看这个", "visibility": "private",
                   "ownerId": "想冒充", "status": "resolved"}),
        )
        .await;
        let id = v["feedback"]["id"].as_str().unwrap().to_string();
        // 请求体里的 ownerId/status 被忽略（agent 解析不出 owner，仍是空）
        assert_eq!(v["feedback"]["ownerId"], json!(""));
        assert_eq!(v["feedback"]["status"], json!("open"));
        assert_eq!(owner.id, owner.id);

        // 回复：默认 visible 会被父的 private 压回 private；aboutKind/aboutRef 继承
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                &format!("/feedback/{id}/reply"),
                &owner_secret,
                json!({"body": "收到", "aboutKind": "artifact", "aboutRef": "@x/y", "visibility": "public"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::CREATED, "{v}");
        assert_eq!(v["feedback"]["parentId"], json!(id));
        assert_eq!(v["feedback"]["visibility"], json!("private"));
        assert_eq!(v["feedback"]["aboutKind"], json!("agent"));
        assert_eq!(v["feedback"]["aboutRef"], json!("AG-1"));

        // 父的回复数 +1，且父内容不变
        let (_, v) = call(&app, get_req(&format!("/feedback/{id}"), &owner_secret)).await;
        assert_eq!(v["feedback"]["replies"], json!(1));
        assert_eq!(v["replies"].as_array().unwrap().len(), 1);
        assert_eq!(v["feedback"]["body"], json!("看看这个"));

        // 对不存在的父回复 -> 404
        let (code, _) = call(
            &app,
            json_req(
                "POST",
                "/feedback/FB-nope/reply",
                &owner_secret,
                json!({"body": "x"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::NOT_FOUND);
    }

    #[tokio::test]
    async fn 写权限_与_非法输入() {
        let st = state("write").await;
        let (u, _bearer) = seed(&st, "jia").await;
        // 只有 feedback:read 的 key：写得 403
        let (_k, read_only) =
            store::apikeys::create(st.pool(), &u.id, "r", &["feedback:read".to_string()])
                .await
                .unwrap();
        let app = router_for(&st);

        // 匿名写 -> 401
        let (code, _) = call(
            &app,
            json_req("POST", "/feedback", "", json!({"body": "hi"}), false),
        )
        .await;
        assert_eq!(code, SC::UNAUTHORIZED);
        // 只读 key 写 -> 403
        let (code, _) = call(
            &app,
            json_req(
                "POST",
                "/feedback",
                &read_only,
                json!({"body": "hi"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::FORBIDDEN);
        // 带 ncc-agent 头：记进 agent 字段
        let (_k2, w) =
            store::apikeys::create(st.pool(), &u.id, "w", &["feedback:write".to_string()])
                .await
                .unwrap();
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/feedback",
                &w,
                json!({"aboutKind": "topic", "aboutRef": "x", "body": "hi"}),
                true,
            ),
        )
        .await;
        assert_eq!(code, SC::CREATED, "{v}");
        assert_eq!(v["feedback"]["agent"], json!("AG-1"));

        // 非法 kind / 缺 aboutRef / rating 无分
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/feedback",
                &w,
                json!({"aboutKind": "topic", "aboutRef": "x", "kind": "bad", "body": "hi"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("invalid_feedback"));
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("kind 必须是"));

        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/feedback",
                &w,
                json!({"aboutKind": "topic", "body": "hi"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("缺 aboutRef"));

        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/feedback",
                &w,
                json!({"aboutKind": "topic", "aboutRef": "x", "kind": "rating"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("kind=rating"));

        // 非 JSON
        let req = Request::builder()
            .method("POST")
            .uri("/feedback")
            .header("content-type", "application/json")
            .header("authorization", auth_header(&w))
            .body(Body::from("不是 JSON"))
            .unwrap();
        let (code, v) = call(&app, req).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("请求体不是合法 JSON"));
    }

    #[tokio::test]
    async fn 列表_收件箱_摘要_与_可见性() {
        let st = state("list").await;
        let (owner, owner_secret) = {
            let (u, _) = seed(&st, "owner").await;
            let (_k, s) =
                store::apikeys::create(st.pool(), &u.id, "o", &["feedback:write".to_string()])
                    .await
                    .unwrap();
            (u, s)
        };
        let (author, author_secret) = seed(&st, "author").await;
        let app = router_for(&st);

        // 两条公开 + 一条私有（作者发给 owner）
        post_feedback(
            &app,
            &author_secret,
            json!({"aboutKind": "topic", "aboutRef": "a", "body": "公开1", "visibility": "public"}),
        )
        .await;
        let mut b = json!({"aboutKind": "topic", "aboutRef": "b", "body": "公开2", "visibility": "public", "kind": "rating", "score": 4});
        b["agent"] = json!("AG-2");
        post_feedback(&app, &author_secret, b).await;
        post_feedback(&app, &author_secret, json!({"aboutKind": "agent", "aboutRef": "AG-1", "body": "私有", "visibility": "private"})).await;
        let _ = owner;

        // 匿名只看 public
        let (_, v) = call(&app, get_req("/feedback", "")).await;
        assert_eq!(v["total"], json!(2));
        assert_eq!(v["page"], json!(1));
        assert_eq!(v["size"], json!(20));

        // 作者：public ∪ 自己发的
        let (_, v) = call(&app, get_req("/feedback", &author_secret)).await;
        assert_eq!(v["total"], json!(3));

        // 收件箱（owner=me）：发给我的（agent 解析不出归属，所以这里 total=0）
        let (_, v) = call(&app, get_req("/feedback/inbox?owner=me", &owner_secret)).await;
        assert_eq!(v["total"], json!(0));

        // 自己发的
        let (_, v) = call(&app, get_req("/feedback?mine=1", &author_secret)).await;
        assert_eq!(v["total"], json!(3));

        // 摘要（匿名）：只看 public
        let (code, v) = call(&app, get_req("/feedback/summary", "")).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["summary"]["count"], json!(2));
        assert_eq!(v["summary"]["publicCount"], json!(2));
        assert_eq!(v["summary"]["privateCount"], json!(0));
        assert_eq!(v["summary"]["scored"], json!(1));
        assert_eq!(v["summary"]["scoreAvg"], json!(4.0));
        assert_eq!(v["summary"]["byKind"]["report"], json!(1));
        assert_eq!(v["summary"]["agents"]["AG-2"], json!(1));
        assert!(v["note"].as_str().unwrap().contains("不是排序用的分数"));
        let _ = author;

        // 摘要（作者带 all=1 但不是管理员）：all 被忽略，仍是自己的可见范围
        let (_, v) = call(&app, get_req("/feedback/summary?all=1", &author_secret)).await;
        assert_eq!(v["summary"]["count"], json!(3));

        // 非法 aboutKind / visibility
        let (code, v) = call(&app, get_req("/feedback?aboutKind=nope", "")).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_about_kind"));
        let (code, v) = call(&app, get_req("/feedback?visibility=nope", "")).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_visibility"));

        // owner=me 未登录 -> 401
        let (code, v) = call(&app, get_req("/feedback?owner=me", "")).await;
        assert_eq!(code, SC::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"]["message"], json!("看「收件箱」要先登录"));
    }

    #[tokio::test]
    async fn 管理员能看到全部_且能处置() {
        let st = state("admin").await;
        // root 是第一个注册用户 = 管理员；管理员视角要走**会话**或 admin key，
        // 账号名下的普通 API-Key 不算管理员（Go 的 ensureAdmin 同样只认 Session）。
        let root = store::users::create(st.pool(), "管理员", "root@x.com", "h")
            .await
            .unwrap();
        let token = crate::jwt::sign_user(
            &st.cfg().jwt_secret,
            &root.id,
            &root.email,
            st.cfg().jwt_ttl,
        );
        // 另一个人（third）的东西被 author 私有反馈：owner=third，admin 才有「替别人处置」的资格
        let third = store::users::create(st.pool(), "第三方", "third@x.com", "h")
            .await
            .unwrap();
        let third_ns = store::namespaces::create_account(st.pool(), &third.id, "第三方", "third")
            .await
            .unwrap();
        store::artifacts::create(
            st.pool(),
            store::artifacts::NewArtifact {
                namespace_id: third_ns.id.clone(),
                slug: "demo".to_string(),
                kind: "skill".to_string(),
                name: "演示".to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                visibility: "public".to_string(),
                status: "published".to_string(),
                manifest: String::new(),
                storage_provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: String::new(),
                sha256: String::new(),
                size: 0,
                created_by: third.id.clone(),
            },
        )
        .await
        .unwrap();
        let (_author, author_secret) = seed(&st, "author").await;
        let app = router_for(&st);
        post_feedback(
            &app,
            &author_secret,
            json!({"aboutKind": "artifact", "aboutRef": "@third/demo", "body": "私有", "visibility": "private"}),
        )
        .await;

        // 作者不是 owner，看不到给别人东西的私有反馈被别人看到这件事：
        // 管理员 all=1 能看到全部，且 canResolve=true（owner 非空 + 是管理员）
        let (code, v) = call(&app, get_req("/feedback?all=1", &token)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["total"], json!(1));
        let id = v["feedback"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(v["feedback"][0]["ownerId"], json!(third.id));
        assert_eq!(v["feedback"][0]["canResolve"], json!(true));

        // 管理员能替别人处置
        let (code, v) = call(
            &app,
            json_req(
                "PATCH",
                &format!("/feedback/{id}"),
                &token,
                json!({"status": "wontfix"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["feedback"]["status"], json!("wontfix"));

        // 没有归属（owner 空）时**没人**能处置 —— 连管理员也不行（与 canResolve 同一规则）
        let topic = post_feedback(
            &app,
            &author_secret,
            json!({"aboutKind": "topic", "aboutRef": "一句话", "body": "没归属", "visibility": "public"}),
        )
        .await;
        let topic_id = topic["feedback"]["id"].as_str().unwrap().to_string();
        assert_eq!(topic["feedback"]["canResolve"], json!(false));
        let (code, v) = call(
            &app,
            json_req(
                "PATCH",
                &format!("/feedback/{topic_id}"),
                &token,
                json!({"status": "ack"}),
                false,
            ),
        )
        .await;
        assert_eq!(code, SC::FORBIDDEN, "{v}");
        let _ = root;
    }

    #[tokio::test]
    async fn relay_幂等_不重复落库() {
        let st = state("relay").await;
        let (_u, bearer) = seed(&st, "jia").await;
        let app = router_for(&st);
        let body = json!({
            "aboutKind": "topic", "aboutRef": "x", "body": "搬来的",
            "origin": "node:office", "originId": "FB-remote-1", "visibility": "public",
        });
        let v1 = post_feedback(&app, &bearer, body.clone()).await;
        let id1 = v1["feedback"]["id"].as_str().unwrap().to_string();
        // hops 里带了 cli:node:office 与 node:office
        assert!(v1["feedback"]["hops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h == "cli:node:office"));

        // 再搬一次：不算错误，返回已有那条
        let (code, v2) = call(&app, json_req("POST", "/feedback", &bearer, body, false)).await;
        assert_eq!(code, SC::OK, "{v2}");
        assert_eq!(v2["duplicated"], json!(true));
        assert_eq!(v2["feedback"]["id"], json!(id1));

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback")
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
    }
}
