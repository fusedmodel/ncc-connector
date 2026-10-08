//! NCC Trace 的 HTTP 层：词表 / 上报 / 列表 / 详情 / 删除 / 标注 / 聚合 / 导出。
//!
//! 原实现：`ncc-registry/httpapi/traces.go`（路由表在 `httpapi/server.go`）。
//!
//! 与「制品托管」不同的三条规矩（写在代码里，别在别处忘了）：
//!
//! 1. **默认私有**：轨迹没有「公开」档 —— 跑过的业务数据不该匿名可见，
//!    所以可见性只有「我的命名空间 / 把 trace 授权给我的人 / 管理员」三条路；
//! 2. **三个作用域分开**：采集 `trace:write`、看 `trace:read`、**下判断 `trace:label`**。
//!    采集是 Agent 的日常动作，下判断是一次评测行为，不该顺着同一把凭据自动拿到；
//! 3. **文档不可变、标注只追加**：这是「评测结论可追溯」的地基。
//!
//! 管理员的默认也是**收窄**的：`all=1` 才会放开命名空间限制 ——
//! 「因为我是管理员所以一进来就看到所有人的轨迹」是错的默认，想看全量得先说出来。

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, FixedOffset, Local, SecondsFormat, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::scope::AuthInfo;
use ncc_core::web;

use crate::httpapi::shares::ensure_admin;
use crate::httpapi::{helpers, AppState, Auth};
use crate::store;
use crate::store::traces as tr;

/// 该族路由（相对 `/api`）。
///
/// 轨迹**没有顶层公开页**（Go 侧所有 trace 路由都挂在 `/api/traces` 下 ——
/// 「默认私有」就意味着没有可匿名打开的链接），所以这里没有 `public_routes()`。
pub fn routes() -> Router<AppState> {
    Router::new()
        // 词表：**不挂作用域**（与 Go 一致）—— 它是取值来源，看词表不算看轨迹。
        .route("/traces/kinds", get(kinds))
        .route("/traces/stats", get(stats))
        .route("/traces/export", get(export))
        .route("/traces", get(list).post(ingest))
        // Go 的路由表同时挂了 `""` 与 `"/"`（gin 的 Group 语义），照抄。
        .route("/traces/", get(list).post(ingest))
        .route("/traces/{id}", get(get_one).delete(delete_one))
        .route("/traces/{id}/labels", get(list_labels).post(add_label))
}

/* ---------------- 鉴权与可见性 ---------------- */

/// 作用域门禁，错误码与文案与 Go 的 `requireScope` 一致
/// （401 `unauthorized` / 403 `scope_required`）。
fn require_scope<'a>(auth: &'a Auth, scope: &str) -> ApiResult<&'a AuthInfo> {
    let Some(a) = auth.info() else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "未认证或凭据无效（先 ncc login 或带 API-Key）",
        ));
    };
    if !auth.allow(scope) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "scope_required",
            format!("当前凭据缺少作用域 {scope}"),
        ));
    }
    Ok(a)
}

/// 命中的这一条能不能给这个身份看：我的命名空间 / 被授权 / 管理员。
async fn can_read_trace(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    row: &tr::TraceRow,
) -> bool {
    let Some(a) = auth.info() else {
        return false;
    };
    if ensure_admin(state, auth, headers).await.is_some() {
        return true;
    }
    if helpers::can_manage(state, &row.namespace_id, &a.user_id).await {
        return true;
    }
    store::grants::has(
        state.pool(),
        &row.owner_id,
        &a.user_id,
        tr::KIND_TRACE,
        &row.namespace_id,
    )
    .await
        || store::grants::has(state.pool(), &row.owner_id, &a.user_id, tr::KIND_TRACE, "").await
}

/// 把身份折成检索的可见范围（写回 opts）。
///
/// 默认**只给「我的 + 被授权给我的」**：管理员也要显式 `all=1` 才能看全节点。
async fn scope_visibility(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    uri: &Uri,
    o: &mut tr::TraceListOpts,
) {
    let Some(a) = auth.info() else {
        // 到不了这里（路由挂了作用域门禁），但留一条 fail-closed 的兜底。
        o.namespace_ids = vec!["-".to_string()];
        return;
    };
    if web::query(uri, "all").as_deref() == Some("1")
        && ensure_admin(state, auth, headers).await.is_some()
    {
        o.all = true;
        return;
    }
    if let Ok(nss) = store::namespaces::of_user(state.pool(), &a.user_id).await {
        o.namespace_ids = nss.iter().map(|n| n.id.clone()).collect();
    }
    // `mine=1`：只要自己的命名空间（不含被授权的）。
    if web::query(uri, "mine").as_deref() != Some("1") {
        o.granted_owners =
            store::grants::granted_owners(state.pool(), &a.user_id, tr::KIND_TRACE).await;
    }
}

/// 一句话说清「我看到的是哪一部分」，免得把「我的轨迹」当成「全部轨迹」。
fn scope_label(uri: &Uri, all: bool) -> &'static str {
    if all {
        return "all";
    }
    if web::query(uri, "mine").as_deref() == Some("1") {
        return "mine";
    }
    "visible"
}

/* ---------------- 视图 ---------------- */

/// 轨迹行的摘要视图（列表用；不含 Doc）。
fn trace_json(r: &tr::TraceRow) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), json!(r.id));
    out.insert("traceId".into(), json!(r.trace_id));
    out.insert("kind".into(), json!(r.kind));
    out.insert(
        "kindLabel".into(),
        json!(tr::trace_kind_label(&r.kind, "zh")),
    );
    out.insert("status".into(), json!(r.status));
    out.insert("at".into(), json!(time_or(r.at.as_str())));
    out.insert("durationMs".into(), json!(r.duration_ms));
    out.insert("steps".into(), json!(r.step_count));
    out.insert("payload".into(), json!(r.payload_level));
    out.insert("digest".into(), json!(r.digest));
    out.insert(
        "namespace".into(),
        json!({"slug": r.ns_slug, "name": r.ns_name}),
    );
    out.insert(
        "owner".into(),
        json!({"id": r.owner_id, "name": r.owner_name.clone().unwrap_or_default()}),
    );
    out.insert("createdBy".into(), json!(r.created_by));
    out.insert(
        "createdAt".into(),
        json!(opt_time_or(r.created_at.as_deref())),
    );
    if !r.subject_ref.is_empty() || !r.subject_version.is_empty() {
        out.insert(
            "subject".into(),
            json!({
                "ref": r.subject_ref, "kind": r.subject_kind, "version": r.subject_version,
                "digest": r.subject_digest, "engine": r.subject_engine, "policy": r.subject_policy,
            }),
        );
    }
    if !r.node_name.is_empty() || !r.host.is_empty() || !r.agent_name.is_empty() {
        out.insert(
            "source".into(),
            json!({"node": r.node_name, "host": r.host, "agent": r.agent_name, "user": r.user_name}),
        );
    }
    if !r.model_provider.is_empty() || !r.model_name.is_empty() {
        out.insert(
            "model".into(),
            json!({"provider": r.model_provider, "name": r.model_name, "calls": r.model_calls}),
        );
    }
    if r.input_tokens > 0 || r.output_tokens > 0 || r.cost_usd_micros > 0 {
        out.insert(
            "usage".into(),
            json!({
                "inputTokens": r.input_tokens, "outputTokens": r.output_tokens,
                "costUsdMicros": r.cost_usd_micros,
                // 同时给一个人读的金额（微美元 → 美元，展示用，不参与摘要）。
                "costUsd": format!("{:.6}", r.cost_usd_micros as f64 / 1e6),
            }),
        );
    }
    if let Some(tags) = parse_string_list(&r.tags) {
        if !tags.is_empty() {
            out.insert("tags".into(), json!(tags));
        }
    }
    let labels = web::parse_json_any(&r.run_labels);
    if !labels.is_null() {
        out.insert("labels".into(), labels);
    }
    // 评测标注的投影：**与 run-time labels 分开报**，别让调用方以为是一个东西。
    let mut ev = Map::new();
    ev.insert("count".into(), json!(r.label_count));
    if !r.eval_grade.is_empty() {
        ev.insert("grade".into(), json!(r.eval_grade));
    }
    if !r.eval_split.is_empty() {
        ev.insert("split".into(), json!(r.eval_split));
    }
    if r.eval_reward != 0 {
        ev.insert("rewardMilli".into(), json!(r.eval_reward));
    }
    if r.eval_score != 0 {
        ev.insert("scoreMilli".into(), json!(r.eval_score));
    }
    out.insert("evaluation".into(), Value::Object(ev));
    Value::Object(out)
}

/// 一条标注。
fn trace_label_json(l: &tr::TraceLabel) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), json!(l.id));
    out.insert("key".into(), json!(l.key));
    out.insert("value".into(), json!(l.value));
    out.insert("by".into(), json!(l.by));
    out.insert("note".into(), json!(l.note));
    out.insert("at".into(), json!(opt_time_or(l.created_at.as_deref())));
    if l.has_num {
        out.insert("valueNum".into(), json!(l.value_num));
    }
    Value::Object(out)
}

/// 时间列（TEXT）→ UTC RFC3339；解析不了就原样回（别让一条脏数据把接口打成 500）。
fn time_or(s: &str) -> String {
    tr::rfc3339_utc(s).unwrap_or_else(|| s.to_string())
}

/// 可空时间列：`None` 按 Go 的零值时间给出。
fn opt_time_or(s: Option<&str>) -> String {
    s.and_then(tr::rfc3339_utc)
        .unwrap_or_else(|| "0001-01-01T00:00:00Z".to_string())
}

/// 库里的 JSON 字符串数组：脏值当空（Go 的 `parseStringList`）。
fn parse_string_list(s: &str) -> Option<Vec<String>> {
    let t = s.trim();
    if t.is_empty() || t == "[]" {
        return None;
    }
    serde_json::from_str::<Vec<String>>(t).ok()
}

/* ---------------- 检索条件 ---------------- */

/// 从查询串构造检索条件（可见性由 `scope_visibility` 另行折进去）。
fn trace_opts(uri: &Uri) -> tr::TraceListOpts {
    let s = |k: &str| web::query(uri, k).unwrap_or_default().trim().to_string();
    let raw = |k: &str| web::query(uri, k).unwrap_or_default();
    let mut o = tr::TraceListOpts::new();
    o.ref_ = s("ref");
    o.kind = s("kind");
    o.status = s("status");
    o.agent = s("agent");
    o.node = s("node");
    o.model = s("model");
    o.payload = s("payload");
    o.tag = s("tag");
    o.grade = s("grade");
    o.split = s("split");
    o.q = s("q");
    o.since = parse_trace_time(&raw("since"));
    o.until = parse_trace_time(&raw("until"));
    o.only_labeled = raw("labeled") == "1";
    o.only_unlabeled = raw("labeled") == "0";
    o.failures_only = raw("status").is_empty() && raw("failures") == "1";
    if s("order") == "asc" {
        o.newest_first = false;
    }
    o
}

/// 手写时间轴的三种常见写法：RFC3339 / `2026-09-26` / unix 秒。
fn parse_trace_time(v: &str) -> Option<DateTime<FixedOffset>> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    if let Some(t) = ncc_core::timeutil::parse_time(v) {
        return Some(t);
    }
    if let Ok(n) = v.parse::<i64>() {
        if n > 0 {
            return Utc.timestamp_opt(n, 0).single().map(|t| t.fixed_offset());
        }
    }
    None
}

/* ---------------- 词表 ---------------- */

/// GET /api/traces/kinds —— 词表与上限（CLI 取值来源，与 `/api/registry/kinds` 同形）。
async fn kinds() -> ApiResult<Response> {
    let steps: Vec<Value> = tr::TRACE_STEP_TYPES
        .iter()
        .map(|t| json!({"id": t}))
        .collect();
    let labels: Vec<Value> = tr::TRACE_LABEL_KEYS
        .iter()
        .map(|k| {
            let meta = tr::trace_label_meta(k).unwrap_or(["", "", "", ""]);
            json!({
                "id": k, "zh": meta[0], "en": meta[1], "descZh": meta[2], "descEn": meta[3],
                "key": k, "keyEn": meta[1],
            })
        })
        .collect();
    let kinds: Vec<Value> = tr::TRACE_KINDS
        .iter()
        .map(|k| {
            json!({
                "id": k,
                "zh": tr::trace_kind_label(k, "zh"),
                "en": tr::trace_kind_label(k, "en"),
            })
        })
        .collect();
    Ok(helpers::ok_json(json!({
        "spec": tr::TRACE_SPEC,
        "kinds": kinds,
        "statuses": tr::TRACE_STATUSES,
        "payloads": tr::TRACE_PAYLOAD_LEVELS,
        "stepTypes": steps,
        "labelKeys": labels,
        "grades": tr::TRACE_GRADES,
        "splits": tr::TRACE_SPLITS,
        "limits": {
            "maxBytes": tr::TRACE_MAX_BYTES,
            "maxSteps": tr::TRACE_MAX_STEPS,
            "batchMax": tr::TRACE_BATCH_MAX,
            "previewMax": tr::TRACE_PREVIEW_MAX,
            "maxTags": tr::TRACE_MAX_TAGS,
            "exportLimit": 20000,
        },
        // 五类内容**共用**的口径（一份常量，五处返回同一份）
        "invariants": tr::shared_invariants(),
    })))
}

/* ---------------- 上报 ---------------- */

/// 一条被拒的轨迹：**逐条给出原因**，不让一条坏数据废掉整批。
#[derive(Debug, serde::Serialize)]
struct TraceReject {
    index: usize,
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    code: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    msg: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    issues: Vec<tr::TraceIssue>,
}

impl TraceReject {
    fn new(index: usize, id: &str, code: &str, msg: &str, issues: Vec<tr::TraceIssue>) -> Self {
        Self {
            index,
            id: id.to_string(),
            code: code.to_string(),
            msg: msg.to_string(),
            issues,
        }
    }
}

/// POST /api/traces —— 采集入口。
///
/// 幂等：同 (命名空间, traceId) 重复上报是**正常现象**（网络重试、离线补报），
/// 摘要一致就当已收下（duplicates++），摘要不同才拒（trace_conflict）。
async fn ingest(State(state): State<AppState>, auth: Auth, body: Bytes) -> ApiResult<Response> {
    let a = require_scope(&auth, "trace:write")?;
    let req: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request("bad_json", format!("请求体不是合法 JSON: {e}")))?;

    // 两种形态：批量 `{"traces":[…]}` 与单条（整个 body 就是一份文档，带 spec）。
    let mut docs: Vec<Value> = Vec::new();
    let mut namespace = String::new();
    if let Some(m) = req.as_object() {
        namespace = m
            .get("namespace")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match m.get("traces") {
            Some(Value::Array(arr)) => docs = arr.clone(),
            None | Some(Value::Null) => {
                // 单条形态：整个 body 就是一份文档。
                let spec = m.get("spec").and_then(|v| v.as_str()).unwrap_or("");
                if !spec.is_empty() {
                    docs.push(req.clone());
                }
            }
            Some(_) => {
                return Err(ApiError::bad_request(
                    "bad_json",
                    "请求体里的 traces 必须是数组",
                ))
            }
        }
    }
    if docs.is_empty() {
        return Err(ApiError::bad_request(
            "empty",
            "没有要上报的轨迹（body 用 {\"traces\":[…]} 或直接给一份文档）",
        ));
    }
    if docs.len() > tr::TRACE_BATCH_MAX {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "batch_too_large",
            format!(
                "一次最多上报 {} 条，收到 {} 条",
                tr::TRACE_BATCH_MAX,
                docs.len()
            ),
        ));
    }

    // 落到哪个命名空间：默认个人空间。**不隐式跨命名空间**（轨迹属于采集它的那个空间）。
    let ns = trace_namespace(&state, a, &namespace).await?;
    // 取用即声明：内置集合的声明行建出来（失败不影响采集本身）。
    let _ = tr::mark_builtin_used(state.pool(), &ns.id, "trace").await;

    let mut acc: Vec<tr::TraceRow> = Vec::new();
    let mut rej: Vec<TraceReject> = Vec::new();
    let mut dups = 0i64;
    for (i, raw) in docs.iter().enumerate() {
        let parsed = match tr::parse_trace_doc(raw.clone()) {
            Ok(p) => p,
            Err(e) => {
                rej.push(TraceReject::new(i, "", "bad_json", &e.message(), vec![]));
                continue;
            }
        };
        let issues = tr::validate_trace(&parsed.value);
        if tr::trace_has_error(&issues) {
            rej.push(TraceReject::new(i, &parsed.id, "trace_invalid", "", issues));
            continue;
        }
        match tr::insert_trace(state.pool(), &ns.id, &a.user_id, &parsed).await {
            Ok(tr::TraceInsertOutcome::Created(row)) => acc.push(row),
            Ok(tr::TraceInsertOutcome::Duplicate(_)) => dups += 1,
            Err(tr::TraceWriteError::Conflict(m)) => rej.push(TraceReject::new(
                i,
                &parsed.id,
                "trace_conflict",
                &m,
                vec![],
            )),
            Err(e) => rej.push(TraceReject::new(
                i,
                &parsed.id,
                "store_error",
                &e.message(),
                vec![],
            )),
        }
    }

    let refs: Vec<Value> = acc
        .iter()
        .map(|x| {
            json!({
                "id": x.id, "traceId": x.trace_id, "digest": x.digest,
                "kind": x.kind, "status": x.status, "payload": x.payload_level,
            })
        })
        .collect();
    let mut out = Map::new();
    out.insert("accepted".into(), json!(acc.len()));
    out.insert("duplicates".into(), json!(dups));
    out.insert("rejected".into(), json!(rej.len()));
    out.insert(
        "namespace".into(),
        json!({"slug": ns.slug, "name": ns.name}),
    );
    if !rej.is_empty() {
        out.insert(
            "rejects".into(),
            serde_json::to_value(&rej).unwrap_or(Value::Array(vec![])),
        );
    }
    out.insert("refs".into(), json!(refs));

    // 全被拒 → 400（让 pipeline 当场失败，而不是静默「成功 0 条」）。
    if acc.is_empty() && dups == 0 {
        let code = if rej.len() == 1 {
            rej[0].code.clone()
        } else {
            "trace_invalid".to_string()
        };
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            &code,
            "全部被拒：见 rejects",
        ));
    }
    Ok(helpers::ok_status(StatusCode::CREATED, Value::Object(out)))
}

/// 解析目标命名空间（默认：调用者的个人空间）。
async fn trace_namespace(
    state: &AppState,
    a: &AuthInfo,
    want: &str,
) -> ApiResult<store::namespaces::Namespace> {
    let want = want.strip_prefix('@').unwrap_or(want).trim().to_string();
    let nss = store::namespaces::of_user(state.pool(), &a.user_id)
        .await
        .map_err(|_| ApiError::internal("服务内部错误"))?;
    if want.is_empty() {
        // 默认落到 `type=account` 的个人空间（与注册建空间时的默认一致）。
        if let Some(n) = nss.iter().find(|n| n.ns_type == "account") {
            return Ok(n.clone());
        }
        if let Some(n) = nss.first() {
            return Ok(n.clone());
        }
        return Err(ApiError::bad_request(
            "no_namespace",
            "你还没有命名空间（先注册或建一个组织空间）",
        ));
    }
    if let Some(n) = nss.iter().find(|n| n.slug == want) {
        return Ok(n.clone());
    }
    Err(ApiError::forbidden(format!(
        "你不是命名空间 @{want} 的成员，不能把轨迹写进去"
    )))
}

/* ---------------- 列表 / 详情 / 删除 ---------------- */

/// GET /api/traces —— 列表。
async fn list(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    require_scope(&auth, "trace:read")?;
    let mut o = trace_opts(&uri);
    scope_visibility(&state, &auth, &headers, &uri, &mut o).await;
    let page = web::query_i64(&uri, "page", 1).max(1);
    let mut size = web::query_i64(&uri, "size", 20);
    if size <= 0 || size > 200 {
        size = 20;
    }
    o.page = page;
    o.size = size;
    let (rows, total) = store::traces::list_traces(state.pool(), &o)
        .await
        .map_err(ApiError::from_db)?;
    let out: Vec<Value> = rows.iter().map(trace_json).collect();
    Ok(helpers::ok_json(json!({
        "traces": out, "total": total, "page": page, "size": size,
        "scope": scope_label(&uri, o.all),
    })))
}

/// 取一条（不存在时 404 的文案与 Go 一致）。
async fn find_trace(state: &AppState, id: &str) -> ApiResult<tr::TraceRow> {
    store::traces::get_trace(state.pool(), id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found(format!("没有这条轨迹: {id}")))
}

/// GET /api/traces/{id} —— 详情（含文档与标注历史）。
async fn get_one(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    require_scope(&auth, "trace:read")?;
    let row = find_trace(&state, &id).await?;
    if !can_read_trace(&state, &auth, &headers, &row).await {
        return Err(ApiError::forbidden(
            "这条轨迹不在你可见的范围内（轨迹默认私有；跨人查看需要 trace 授权）",
        ));
    }
    // 库里的文档坏了也要如实回（而不是 500）：轨迹是证据，坏了比不显示更容易被发现。
    let doc =
        serde_json::from_str::<Value>(&row.doc).unwrap_or_else(|_| Value::String(row.doc.clone()));
    let labels = store::traces::list_trace_labels(state.pool(), &row.id)
        .await
        .unwrap_or_default();
    let ls: Vec<Value> = labels.iter().map(trace_label_json).collect();
    let mut out = trace_json(&row);
    if let Some(obj) = out.as_object_mut() {
        obj.insert("trace".into(), doc);
        // 标注历史挂在 evaluation 里。
        if let Some(ev) = obj.get_mut("evaluation").and_then(|v| v.as_object_mut()) {
            ev.insert("labels".into(), json!(ls));
        }
    }
    Ok(helpers::ok_json(out))
}

/// DELETE /api/traces/{id}
async fn delete_one(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = require_scope(&auth, "trace:write")?;
    let row = find_trace(&state, &id).await?;
    if ensure_admin(&state, &auth, &headers).await.is_none()
        && !helpers::can_manage(&state, &row.namespace_id, &a.user_id).await
    {
        return Err(ApiError::forbidden(
            "只有归属命名空间的管理者能删除这条轨迹",
        ));
    }
    store::traces::delete_trace(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_json(
        json!({"ok": true, "id": row.id, "traceId": row.trace_id}),
    ))
}

/* ---------------- 评测标注 ---------------- */

/// 追加标注的请求体：`key` + `value`（或 `reward` / `score` 数值）。
///
/// 全部用 `Option`：Go 的 `json.Unmarshal` 把 `null` 当「没给」，而不是类型错误。
#[derive(Debug, Default, Deserialize)]
struct TraceLabelReq {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    value: Option<String>,
    /// 整数奖励（1 / 0 / -1）。
    #[serde(default)]
    reward: Option<i64>,
    /// 0..1000 的千分位得分（C- 侧已经换算好）。
    #[serde(default)]
    score: Option<i64>,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    by: Option<String>,
}

/// POST /api/traces/{id}/labels —— 下判断（`trace:label`，**不是** `trace:write`）。
async fn add_label(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let a = require_scope(&auth, "trace:label")?;
    let row = find_trace(&state, &id).await?;
    if !can_read_trace(&state, &auth, &headers, &row).await {
        return Err(ApiError::forbidden("看不到这条轨迹，就不能标注它"));
    }
    let body: Value = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request("bad_json", format!("请求体不是合法 JSON: {e}")))?;
    let req: TraceLabelReq = TraceLabelReq::deserialize(&body)
        .map_err(|e| ApiError::bad_request("bad_json", format!("请求体不是合法 JSON: {e}")))?;

    let mut in_ = tr::TraceLabelInput {
        trace_id: row.id.clone(),
        key: String::new(),
        value: String::new(),
        value_num: 0,
        has_num: false,
        by: req.by.unwrap_or_default(),
        note: req.note.unwrap_or_default(),
    };
    if in_.by.is_empty() {
        in_.by = a.user_id.clone();
    }
    if let Some(r) = req.reward {
        in_.key = "reward".to_string();
        in_.value = r.to_string();
        in_.value_num = r * 1000;
        in_.has_num = true;
    } else if let Some(s) = req.score {
        if !(-1000..=1000).contains(&s) {
            return Err(ApiError::bad_request(
                "bad_score",
                "score 要在 -1..1 之间（千分位：-1000..1000）",
            ));
        }
        in_.key = "score".to_string();
        in_.value = s.to_string();
        in_.value_num = s;
        in_.has_num = true;
    } else {
        let k = req.key.unwrap_or_default().trim().to_string();
        if k.is_empty() {
            return Err(ApiError::bad_request(
                "bad_key",
                "缺 key（常用键见 GET /api/traces/kinds）",
            ));
        }
        // 与 Go 一样按**字节**算长度（键一般是 ASCII）。
        if k.len() > 64 {
            return Err(ApiError::bad_request("bad_key", "key 太长（≤64）"));
        }
        let value = req.value.unwrap_or_default();
        if k == "grade" && !value.is_empty() && !tr::TRACE_GRADES.contains(&value.as_str()) {
            // 不拦，但提醒 —— 词表是给管线对齐用的。
            tracing::warn!(
                "trace label grade={value:?} 不在建议词表 {:?} 里（trace {}）",
                tr::TRACE_GRADES,
                row.trace_id
            );
        }
        in_.key = k;
        in_.value = value;
    }
    let l = store::traces::add_trace_label(state.pool(), in_)
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({"ok": true, "label": trace_label_json(&l), "traceId": row.trace_id, "id": row.id}),
    ))
}

/// GET /api/traces/{id}/labels —— 标注历史。
async fn list_labels(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    require_scope(&auth, "trace:read")?;
    let row = find_trace(&state, &id).await?;
    if !can_read_trace(&state, &auth, &headers, &row).await {
        return Err(ApiError::forbidden("这条轨迹不在你可见的范围内"));
    }
    let labels = store::traces::list_trace_labels(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?;
    let out: Vec<Value> = labels.iter().map(trace_label_json).collect();
    Ok(helpers::ok_json(json!({
        "id": row.id, "traceId": row.trace_id, "labels": out, "count": out.len(),
    })))
}

/* ---------------- 聚合（能力评估） ---------------- */

/// GET /api/traces/stats
async fn stats(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    require_scope(&auth, "trace:read")?;
    let mut o = trace_opts(&uri);
    scope_visibility(&state, &auth, &headers, &uri, &mut o).await;
    let (rows, truncated) = store::traces::scan_traces_for_stats(state.pool(), &o, 200000)
        .await
        .map_err(ApiError::from_db)?;
    let st = store::traces::trace_stats_of(&rows);
    Ok(helpers::ok_json(json!({
        "stats": st,
        "sampled": rows.len(),
        "truncated": truncated,
        "scope": scope_label(&uri, o.all),
        // 评估口径写在响应里：拿到这份 JSON 的人不用猜「成功率怎么算的」。
        "how": {
            "successRate": "byStatus.ok / total",
            "scoreAvgMilli": "只对**打过分的**轨迹求平均（不拿未标注的稀释分母）",
            "bySubjectVersion": "同一个 ref 的不同 version 分组 —— 评测改版前后的落点",
            "labeled": "label_count > 0 的条数（标注覆盖率）",
        },
    })))
}

/* ---------------- 数据集导出 ---------------- */

/// GET /api/traces/export —— 导出成数据集（JSONL）。
///
/// 后训练管线要的就是这个：一行一条轨迹，原文（若有）与标注都在里面。
/// 同时回**数据集摘要**（条数 + sha256 + 查询条件），这样「这份训练集是怎么来的」
/// 是可复现、可核对的 —— 数据集本身就是一次实验的输入，得有指纹。
async fn export(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    require_scope(&auth, "trace:read")?;
    let mut o = trace_opts(&uri);
    scope_visibility(&state, &auth, &headers, &uri, &mut o).await;
    let mut limit = web::query_i64(&uri, "limit", 1000);
    if limit <= 0 {
        limit = 1000;
    }
    if limit > 20000 {
        limit = 20000;
    }
    let (rows, truncated) = store::traces::export_traces(state.pool(), &o, limit)
        .await
        .map_err(ApiError::from_db)?;

    let mut blob = String::new();
    let mut docs: Vec<Value> = Vec::new();
    for r in &rows {
        // 库里的 Doc 坏了：**如实报出来**，别静默跳过（那是数据丢失）。
        let doc = serde_json::from_str::<Value>(&r.doc).map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "corrupt_doc",
                format!("轨迹 {} 的文档解析失败：{}", r.id, e),
            )
        })?;
        let labels = store::traces::list_trace_labels(state.pool(), &r.id)
            .await
            .unwrap_or_default();
        let ls: Vec<Value> = labels.iter().map(trace_label_json).collect();
        let mut row = Map::new();
        row.insert("spec".into(), json!("ncc-trace-dataset/v1"));
        row.insert("trace".into(), doc);
        row.insert("meta".into(), trace_json(r));
        if !ls.is_empty() {
            row.insert("evaluation".into(), json!(ls));
        }
        let row = Value::Object(row);
        let line = serde_json::to_string(&row)
            .map_err(|e| ApiError::internal(format!("序列化失败：{e}")))?;
        blob.push_str(&line);
        blob.push('\n');
        docs.push(row);
    }
    let digest = format!("sha256:{}", ncc_core::crypto::sha256_hex(blob.as_bytes()));
    let generated_at = Local::now()
        .fixed_offset()
        .to_rfc3339_opts(SecondsFormat::Secs, false);
    let manifest = json!({
        "spec": "ncc-trace-dataset/v1",
        "count": docs.len(),
        "truncated": truncated,
        "limit": limit,
        "digest": digest,
        "generatedAt": generated_at,
        "query": {
            "ref": o.ref_, "kind": o.kind, "status": o.status, "agent": o.agent,
            "node": o.node, "model": o.model, "payload": o.payload, "tag": o.tag,
            "grade": o.grade, "split": o.split, "q": o.q,
            "since": web::query(&uri, "since").unwrap_or_default(),
            "until": web::query(&uri, "until").unwrap_or_default(),
        },
        "scope": scope_label(&uri, o.all),
    });
    if web::query(&uri, "format").as_deref() == Some("json")
        || web::query(&uri, "manifest").as_deref() == Some("1")
    {
        return Ok(helpers::ok_json(
            json!({"manifest": manifest, "rows": docs}),
        ));
    }

    // 默认 JSONL：训练管线直接吃。
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-ndjson; charset=utf-8")
        .header("X-NCC-Trace-Count", docs.len().to_string())
        .header("X-NCC-Dataset-Digest", digest);
    if truncated {
        builder = builder.header("X-NCC-Truncated", "1");
        // Go 把中文原样写进 Warning 头（HTTP 头只允许可见 ASCII，那是非法值）。
        // 这里按仓库既有约定：值能变成合法头才发 —— `X-NCC-Truncated` 仍然给出，
        // 客户端（CLI）也只读那一个。
        if let Ok(v) = HeaderValue::from_str(
            r#"199 ncc-registry "结果被 limit 截断：用 since/size 收窄或分批导出""#,
        ) {
            builder = builder.header("Warning", v);
        }
    }
    builder
        .body(Body::from(blob))
        .map_err(|_| ApiError::internal("服务内部错误"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_dir(name: &str) -> std::path::PathBuf {
        // 落在工作区的 target/ 里（已 gitignore），别污染 crate 源码树
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-blobs")
            .join(format!("traces-{}-{name}", std::process::id()))
    }

    async fn state(name: &str) -> AppState {
        let dir =
            std::env::temp_dir().join(format!("ncc-http-traces-{}-{name}", std::process::id()));
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
        cfg.blob_dir = test_dir(name);
        let blobs =
            ncc_core::storage::LocalStorage::new(&cfg.blob_dir, &cfg.public_url, "blobs").unwrap();
        let seal = ncc_core::secretbox::SecretBox::new(&cfg.jwt_secret).unwrap();
        AppState {
            cfg: std::sync::Arc::new(cfg),
            pool,
            blobs: std::sync::Arc::new(blobs),
            seal: std::sync::Arc::new(seal),
        }
    }

    fn app(state: &AppState) -> Router {
        Router::new().merge(routes()).with_state(state.clone())
    }

    /// 建用户 + 个人命名空间 + 一把带指定作用域的 API-Key，返回 (用户, 命名空间, bearer)。
    async fn seed(
        state: &AppState,
        slug: &str,
        scopes: &[&str],
    ) -> (store::users::User, store::namespaces::Namespace, String) {
        let u = store::users::create(state.pool(), slug, &format!("{slug}@x.com"), "h")
            .await
            .unwrap();
        let ns = store::namespaces::create_account(state.pool(), &u.id, slug, slug)
            .await
            .unwrap();
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        let (_k, secret) = store::apikeys::create(state.pool(), &u.id, "t", &scopes)
            .await
            .unwrap();
        (u, ns, secret)
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    async fn call_raw(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
            .await
            .unwrap();
        (status, headers, String::from_utf8_lossy(&bytes).to_string())
    }

    fn json_req(method: &str, path: &str, bearer: &str, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if !bearer.is_empty() {
            b = b.header("authorization", format!("Bearer {bearer}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn get_req(path: &str, bearer: &str) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(path);
        if !bearer.is_empty() {
            b = b.header("authorization", format!("Bearer {bearer}"));
        }
        b.body(Body::empty()).unwrap()
    }

    /// 一份合规轨迹文档（`at` 与 id 由调用方给）。
    fn doc(id: &str, status: &str, at: &str) -> Value {
        let mut d = json!({
            "spec": tr::TRACE_SPEC,
            "id": id,
            "kind": "agent",
            "at": at,
            "durationMs": 120,
            "status": status,
            "source": { "node": "n1", "agent": "harness-use" },
            "subject": { "ref": "@alice/skill", "version": "0.1.0" },
            "model": { "provider": "openai", "name": "gpt-4o-mini", "calls": 1 },
            "usage": { "inputTokens": 10, "outputTokens": 5, "costUsdMicros": 700 },
            "steps": [
                { "i": 0, "type": "llm", "name": "plan", "ms": 80, "status": "ok",
                  "inDigest": "sha256:a", "outDigest": "sha256:b" }
            ],
            "labels": { "task": "book-hotel" },
            "tags": ["prod"],
            "payload": "digest"
        });
        tr::normalize_trace(&mut d);
        d
    }

    /// 路由**真的挂上去了**（不是被 501 兜底接住）：axum 建路由时若有冲突会在这里 panic。
    #[tokio::test]
    async fn 整站路由表_本族端点已挂载() {
        let st = state("router").await;
        let app = crate::router::build(&st);
        // 词表不需要凭据
        let (code, v) = call(&app, get_req("/api/traces/kinds", "")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["spec"], tr::TRACE_SPEC);
        // 其余端点要凭据（401 而不是 501）
        for path in [
            "/api/traces",
            "/api/traces/",
            "/api/traces/stats",
            "/api/traces/export",
            "/api/traces/TR-1",
            "/api/traces/TR-1/labels",
        ] {
            let (code, v) = call(&app, get_req(path, "")).await;
            assert_eq!(code, StatusCode::UNAUTHORIZED, "{path} {v}");
            assert_eq!(v["error"]["code"], "unauthorized");
            assert_eq!(
                v["error"]["message"],
                "未认证或凭据无效（先 ncc login 或带 API-Key）"
            );
        }
    }

    #[tokio::test]
    async fn 词表字段与上限() {
        let st = state("kinds").await;
        let app = app(&st);
        let (code, v) = call(&app, get_req("/traces/kinds", "")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["kinds"][0]["id"], "hur-run");
        assert_eq!(v["kinds"][0]["zh"], "HUR 执行");
        assert_eq!(v["kinds"][1]["en"], "Agent session");
        assert_eq!(v["statuses"], json!(["ok", "error", "cancelled"]));
        assert_eq!(v["payloads"], json!(["digest", "preview", "full"]));
        assert_eq!(v["stepTypes"][0]["id"], "llm");
        assert_eq!(v["grades"][0], "pass");
        assert_eq!(v["splits"][2], "holdout");
        assert_eq!(v["labelKeys"][0]["key"], "grade");
        assert_eq!(v["labelKeys"][0]["zh"], "结论");
        assert_eq!(
            v["labelKeys"][0]["descZh"],
            "人工/模型给的结论：pass | fail | partial"
        );
        assert_eq!(v["limits"]["batchMax"], 500);
        assert_eq!(v["limits"]["exportLimit"], 20000);
        assert_eq!(v["limits"]["maxSteps"], 2000);
        assert_eq!(v["invariants"].as_array().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn 采集_列表_详情_标注_聚合_导出_删除() {
        let st = state("flow").await;
        let (_u, _ns, token) =
            seed(&st, "alice", &["trace:read", "trace:write", "trace:label"]).await;
        let app = app(&st);

        // 批量上报：一条收下
        let body = json!({"traces": [doc("TRC-1", "ok", "2026-09-26T10:00:00Z")]});
        let (code, v) = call(&app, json_req("POST", "/traces", &token, body.clone())).await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["accepted"], 1);
        assert_eq!(v["duplicates"], 0);
        assert_eq!(v["rejected"], 0);
        assert_eq!(v["namespace"]["slug"], "alice");
        assert_eq!(v["refs"][0]["traceId"], "TRC-1");
        assert_eq!(v["refs"][0]["payload"], "digest");
        let row_id = v["refs"][0]["id"].as_str().unwrap().to_string();

        // 重传：同 id 同摘要 → duplicates（网络重试是常态）
        let (code, v) = call(&app, json_req("POST", "/traces/", &token, body)).await;
        assert_eq!(code, StatusCode::CREATED);
        assert_eq!(
            (v["accepted"].as_i64(), v["duplicates"].as_i64()),
            (Some(0), Some(1))
        );

        // 同 id 不同内容 → 拒（而且是 trace_conflict）
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &token,
                json!({"traces": [doc("TRC-1", "error", "2026-09-26T10:00:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "trace_conflict");
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &token,
                json!({"traces": [doc("TRC-2", "error", "2026-09-26T10:05:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        assert_eq!(v["accepted"], 1);

        // 列表：默认 scope=visible，倒序（最新在前）
        let (code, v) = call(&app, get_req("/traces", &token)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 2);
        assert_eq!(v["page"], 1);
        assert_eq!(v["size"], 20);
        assert_eq!(v["scope"], "visible");
        assert_eq!(v["traces"][0]["traceId"], "TRC-2");
        assert_eq!(v["traces"][0]["kindLabel"], "Agent 会话");
        assert_eq!(v["traces"][0]["at"], "2026-09-26T10:05:00Z");
        assert_eq!(v["traces"][0]["steps"], 1);
        assert_eq!(v["traces"][0]["namespace"]["slug"], "alice");
        assert_eq!(v["traces"][0]["namespace"]["name"], "alice");
        assert_eq!(v["traces"][0]["tags"][0], "prod");
        assert_eq!(v["traces"][0]["labels"]["task"], "book-hotel");
        assert_eq!(v["traces"][0]["subject"]["ref"], "@alice/skill");
        assert_eq!(v["traces"][0]["model"]["calls"], 1);
        assert_eq!(v["traces"][0]["usage"]["costUsd"], "0.000700");
        assert_eq!(v["traces"][0]["evaluation"]["count"], 0);
        assert!(v["traces"][0]["evaluation"].get("grade").is_none());

        // 过滤
        let (code, v) = call(&app, get_req("/traces?status=ok&order=asc", &token)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["traces"][0]["traceId"], "TRC-1");
        let (_code, v) = call(&app, get_req("/traces?failures=1", &token)).await;
        assert_eq!(v["total"], 1, "{v}");
        let (_code, v) = call(&app, get_req("/traces?tag=prod", &token)).await;
        assert_eq!(v["total"], 2);
        let (_code, v) = call(&app, get_req("/traces?mine=1", &token)).await;
        assert_eq!(v["scope"], "mine");

        // 详情：带 trace 文档与标注历史
        let (code, v) = call(&app, get_req(&format!("/traces/{row_id}"), &token)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["trace"]["id"], "TRC-1");
        assert_eq!(v["trace"]["payload"], "digest");
        assert_eq!(v["evaluation"]["labels"].as_array().unwrap().len(), 0);
        // traceId 也能当 id 用
        let (code, v) = call(&app, get_req("/traces/TRC-1", &token)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["traceId"], "TRC-1");
        // 不存在
        let (code, v) = call(&app, get_req("/traces/NOPE", &token)).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "没有这条轨迹: NOPE");

        // 标注：grade / reward / score / split 四个动作四条记录
        for (i, body) in [
            json!({"key": "grade", "value": "pass", "note": "人工复核"}),
            json!({"reward": 1}),
            json!({"score": 850}),
            json!({"key": "split", "value": "eval"}),
        ]
        .iter()
        .enumerate()
        {
            let (code, v) = call(
                &app,
                json_req(
                    "POST",
                    &format!("/traces/{row_id}/labels"),
                    &token,
                    body.clone(),
                ),
            )
            .await;
            assert_eq!(code, StatusCode::CREATED, "{v}");
            assert_eq!(v["ok"], true);
            assert_eq!(v["traceId"], "TRC-1");
            if i == 1 {
                // reward 的数值是**千分位**（1 → 1000），不是浮点
                assert_eq!(v["label"]["key"], "reward");
                assert_eq!(v["label"]["valueNum"], 1000);
                assert_eq!(v["label"]["value"], "1");
            }
            if i == 2 {
                assert_eq!(v["label"]["valueNum"], 850);
            }
        }
        let (code, v) = call(&app, get_req(&format!("/traces/{row_id}/labels"), &token)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["count"], 4);
        assert_eq!(v["id"], row_id);
        assert_eq!(v["traceId"], "TRC-1");
        // 顺序与时间戳同精度相关，这里按集合断言（键与数值都对得上即可）
        let labels = v["labels"].as_array().unwrap();
        let mut keys: Vec<&str> = labels.iter().filter_map(|l| l["key"].as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec!["grade", "reward", "score", "split"]);
        let with_num: Vec<&Value> = labels
            .iter()
            .filter(|l| l.get("valueNum").is_some())
            .collect();
        assert_eq!(with_num.len(), 2, "只有 reward/score 带数值");
        let reward = labels.iter().find(|l| l["key"] == "reward").unwrap();
        // reward 的数值是**千分位**（1 → 1000），不是浮点
        assert_eq!(reward["valueNum"], 1000);
        assert_eq!(reward["value"], "1");
        let score = labels.iter().find(|l| l["key"] == "score").unwrap();
        assert_eq!(score["valueNum"], 850);
        let grade = labels.iter().find(|l| l["key"] == "grade").unwrap();
        assert!(grade.get("valueNum").is_none());
        assert_eq!(grade["note"], "人工复核");
        assert!(!grade["by"].as_str().unwrap().is_empty());

        // 投影回行（检索与聚合靠它）
        let (_code, v) = call(&app, get_req(&format!("/traces/{row_id}"), &token)).await;
        assert_eq!(v["evaluation"]["count"], 4);
        assert_eq!(v["evaluation"]["grade"], "pass");
        assert_eq!(v["evaluation"]["rewardMilli"], 1000);
        assert_eq!(v["evaluation"]["scoreMilli"], 850);
        assert_eq!(v["evaluation"]["split"], "eval");
        // 文档本身没被标注改过
        assert_eq!(v["trace"]["payload"], "digest");

        let (_code, v) = call(&app, get_req("/traces?grade=pass", &token)).await;
        assert_eq!(v["total"], 1);
        let (_code, v) = call(&app, get_req("/traces?labeled=1", &token)).await;
        assert_eq!(v["total"], 1);
        let (_code, v) = call(&app, get_req("/traces?labeled=0", &token)).await;
        assert_eq!(v["total"], 1);

        // 聚合
        let (code, v) = call(&app, get_req("/traces/stats", &token)).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["stats"]["total"], 2);
        assert_eq!(v["stats"]["byStatus"]["ok"], 1);
        assert_eq!(v["stats"]["byStatus"]["error"], 1);
        assert_eq!(v["stats"]["bySubjectVersion"]["@alice/skill@0.1.0"], 2);
        assert_eq!(v["stats"]["byModel"]["openai/gpt-4o-mini"], 2);
        assert_eq!(v["stats"]["byTag"]["prod"], 2);
        assert_eq!(v["stats"]["byPayload"]["digest"], 2);
        assert_eq!(v["stats"]["labeled"], 1);
        assert_eq!(v["stats"]["unlabeled"], 1);
        assert_eq!(v["stats"]["scoreAvgMilli"], 850);
        assert_eq!(v["stats"]["rewardSumMilli"], 1000);
        assert_eq!(v["stats"]["inputTokens"], 20);
        assert_eq!(v["stats"]["costUsdMicros"], 1400);
        assert_eq!(v["stats"]["firstAt"], "2026-09-26T10:00:00Z");
        assert_eq!(v["stats"]["lastAt"], "2026-09-26T10:05:00Z");
        assert_eq!(v["sampled"], 2);
        assert_eq!(v["truncated"], false);
        assert_eq!(v["scope"], "visible");
        assert_eq!(v["how"]["successRate"], "byStatus.ok / total");

        // 导出（默认 JSONL）
        let (code, headers, body) = call_raw(&app, get_req("/traces/export", &token)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            headers["content-type"].to_str().unwrap(),
            "application/x-ndjson; charset=utf-8"
        );
        assert_eq!(headers["x-ncc-trace-count"].to_str().unwrap(), "2");
        let digest = headers["x-ncc-dataset-digest"]
            .to_str()
            .unwrap()
            .to_string();
        assert!(digest.starts_with("sha256:"), "{digest}");
        assert_eq!(digest.len(), "sha256:".len() + 64);
        assert!(headers.get("x-ncc-truncated").is_none());
        let lines: Vec<&str> = body.trim_end().split('\n').collect();
        assert_eq!(lines.len(), 2);
        let rows: Vec<Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // 默认最新在前（at desc）：TRC-2（10:05）在前，TRC-1（10:00）在后。
        assert_eq!(rows[0]["meta"]["traceId"], "TRC-2");
        assert_eq!(rows[1]["meta"]["traceId"], "TRC-1");
        assert_eq!(rows[0]["spec"], "ncc-trace-dataset/v1");
        assert_eq!(rows[1]["trace"]["id"], "TRC-1");
        assert!(
            rows[0].get("evaluation").is_none(),
            "没标注的轨迹不带 evaluation"
        );
        assert_eq!(rows[1]["evaluation"][0]["key"], "grade");
        // 摘要就是这批字节的 sha256（可核对）
        assert_eq!(
            digest,
            format!("sha256:{}", ncc_core::crypto::sha256_hex(body.as_bytes()))
        );

        // 导出（manifest=1 → JSON）
        let (code, v) = call(
            &app,
            get_req("/traces/export?manifest=1&ref=@alice/skill", &token),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["manifest"]["spec"], "ncc-trace-dataset/v1");
        assert_eq!(v["manifest"]["count"], 2);
        assert_eq!(v["manifest"]["truncated"], false);
        assert_eq!(v["manifest"]["limit"], 1000);
        assert_eq!(v["manifest"]["query"]["ref"], "@alice/skill");
        assert_eq!(v["manifest"]["query"]["since"], "");
        assert_eq!(v["manifest"]["scope"], "visible");
        assert_eq!(v["rows"].as_array().unwrap().len(), 2);

        // 导出截断：limit=1 → 如实报（头 + manifest）
        let (code, headers, body) = call_raw(&app, get_req("/traces/export?limit=1", &token)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(headers["x-ncc-trace-count"].to_str().unwrap(), "1");
        assert_eq!(headers["x-ncc-truncated"].to_str().unwrap(), "1");
        assert_eq!(body.trim_end().split('\n').count(), 1);
        let (_code, v) = call(&app, get_req("/traces/export?limit=1&format=json", &token)).await;
        assert_eq!(v["manifest"]["truncated"], true);
        assert_eq!(v["manifest"]["limit"], 1);

        // 删除：连标注一起清掉
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/traces/{row_id}"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert_eq!(v["traceId"], "TRC-1");
        let (code, _v) = call(&app, get_req(&format!("/traces/{row_id}"), &token)).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trace_labels")
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(count, 0, "删轨迹要连标注一起清掉，别留孤儿行");
    }

    #[tokio::test]
    async fn 作用域三件事分开() {
        let st = state("scopes").await;
        // 只有采集权：能上报，但看不了、也下不了判断
        let (_u, _ns, token) = seed(&st, "alice", &["trace:write"]).await;
        let app = app(&st);
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &token,
                json!({"traces": [doc("TRC-1", "ok", "2026-09-26T10:00:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        let row_id = v["refs"][0]["id"].as_str().unwrap().to_string();
        let (code, v) = call(&app, get_req("/traces", &token)).await;
        // trace:write **蕴含** trace:read（与 Go 的 scopeImplies 一致），所以列表放行 ——
        // 但可见性照旧只给「我的」，不是「因为能写就能看所有人的」。
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["total"], 1);
        // 反过来：只有 trace:read 的凭据报不了（下一段验证）
        let (code, _v) = call(&app, get_req(&format!("/traces/{row_id}"), &token)).await;
        assert_eq!(code, StatusCode::OK);
        // 标注要 trace:label（写权不含它）
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                &format!("/traces/{row_id}/labels"),
                &token,
                json!({"key": "grade", "value": "pass"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "当前凭据缺少作用域 trace:label");

        // 只有读权：看得到，但报不了
        let (_u2, _ns2, ro) = seed(&st, "bob", &["trace:read"]).await;
        let (code, _v) = call(&app, get_req("/traces/kinds", &ro)).await;
        assert_eq!(code, StatusCode::OK);
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &ro,
                json!({"traces": [doc("TRC-X", "ok", "2026-09-26T10:00:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "当前凭据缺少作用域 trace:write");
    }

    #[tokio::test]
    async fn 可见性_跨人看不到_管理员要显式all() {
        let st = state("visibility").await;
        let (u1, _ns1, t1) =
            seed(&st, "alice", &["trace:read", "trace:write", "trace:label"]).await;
        let (_u2, _ns2, t2) = seed(&st, "bob", &["trace:read", "trace:write", "trace:label"]).await;
        let app = app(&st);
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &t1,
                json!({"traces": [doc("TRC-a", "ok", "2026-09-26T10:00:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        let row_id = v["refs"][0]["id"].as_str().unwrap().to_string();

        // 别人看不到：列表里没有，详情 403，标注 403
        let (code, v) = call(&app, get_req("/traces", &t2)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 0);
        let (code, v) = call(&app, get_req(&format!("/traces/{row_id}"), &t2)).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("这条轨迹不在你可见的范围内"));
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                &format!("/traces/{row_id}/labels"),
                &t2,
                json!({"key": "grade", "value": "fail"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "看不到这条轨迹，就不能标注它");
        let (code, _v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/traces/{row_id}"))
                .header("authorization", format!("Bearer {t2}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);

        // 非管理员带 all=1：**不当成看全部**，范围仍是 visible
        let (code, v) = call(&app, get_req("/traces?all=1", &t2)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["scope"], "visible");
        assert_eq!(v["total"], 0);

        // 管理员（会话 JWT）+ all=1 → 看全节点，scope=all
        sqlx::query("UPDATE users SET is_admin = 1 WHERE id = ?")
            .bind(&u1.id)
            .execute(st.pool())
            .await
            .unwrap();
        let jwt = crate::jwt::sign_user(
            &st.cfg().jwt_secret,
            &u1.id,
            "alice@x.com",
            std::time::Duration::from_secs(3600),
        );
        let (code, v) = call(&app, get_req("/traces?all=1", &jwt)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["scope"], "all");
        assert_eq!(v["total"], 1);
        // 会话管理员不看 all 时仍是「可见的」那一份（默认收窄）
        let (_code, v) = call(&app, get_req("/traces", &jwt)).await;
        assert_eq!(v["scope"], "visible");

        // 被授权者：bob 拿到 alice 的 trace 授权后可见
        let owner: String =
            sqlx::query_scalar("SELECT owner_id FROM namespaces WHERE slug = 'alice'")
                .fetch_one(st.pool())
                .await
                .unwrap();
        store::grants::create(st.pool(), &owner, &_u2.id, tr::KIND_TRACE, "", "")
            .await
            .unwrap();
        let (code, v) = call(&app, get_req("/traces", &t2)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 1, "{v}");
        // mine=1 时**不含**被授权的
        let (_code, v) = call(&app, get_req("/traces?mine=1", &t2)).await;
        assert_eq!(v["total"], 0);
        let (code, v) = call(&app, get_req(&format!("/traces/{row_id}"), &t2)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["traceId"], "TRC-a");
    }

    #[tokio::test]
    async fn 上报的边界与逐条拒收() {
        let st = state("edges").await;
        let (_u, ns, token) = seed(&st, "alice", &["trace:read", "trace:write"]).await;
        let app = app(&st);

        // 空 body → empty
        let (code, v) = call(&app, json_req("POST", "/traces", &token, json!({}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "empty");
        // 不是 JSON → bad_json
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/traces")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from("{不是 JSON"))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_json");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("请求体不是合法 JSON"));
        // 超批：一次最多 500 条
        let many: Vec<Value> = (0..501)
            .map(|i| doc(&format!("TRC-{i}"), "ok", "2026-09-26T10:00:00Z"))
            .collect();
        let (code, v) = call(
            &app,
            json_req("POST", "/traces", &token, json!({"traces": many})),
        )
        .await;
        assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(v["error"]["code"], "batch_too_large");
        assert_eq!(v["error"]["message"], "一次最多上报 500 条，收到 501 条");

        // 坏文档 + 好文档混在一批：**逐条给出原因**，好的一条照收
        let mut bad = doc("TRC-bad", "ok", "2026-09-26T10:00:00Z");
        bad["kind"] = json!("chat");
        let mut broken = doc("TRC-broken", "ok", "2026-09-26T10:00:00Z");
        broken["tags"] = json!("prod"); // 类型不对 → 不是一份轨迹文档
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &token,
                json!({"traces": [bad, broken, doc("TRC-good", "ok", "2026-09-26T10:00:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["accepted"], 1);
        assert_eq!(v["rejected"], 2);
        assert_eq!(v["rejects"][0]["index"], 0);
        assert_eq!(v["rejects"][0]["id"], "TRC-bad");
        assert_eq!(v["rejects"][0]["code"], "trace_invalid");
        assert!(v["rejects"][0]["issues"][0]["msg"]
            .as_str()
            .unwrap()
            .contains("kind 必须是 hur-run|agent"));
        assert_eq!(v["rejects"][1]["code"], "bad_json");
        assert!(v["rejects"][1].get("id").is_none(), "解析失败的没有 id");
        assert!(v["rejects"][1]["msg"]
            .as_str()
            .unwrap()
            .starts_with("不是一份轨迹文档"));
        assert_eq!(v["refs"][0]["traceId"], "TRC-good");

        // 全被拒 → 400，code 取唯一的那个（跟 Go 一样）
        let (code, v) = call(
            &app,
            json_req("POST", "/traces", &token, json!({"traces": [bad.clone()]})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "trace_invalid");
        assert_eq!(v["error"]["message"], "全部被拒：见 rejects");

        // 单条形态：整个 body 就是一份文档
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &token,
                doc("TRC-single", "ok", "2026-09-26T10:00:00Z"),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["accepted"], 1);
        assert_eq!(v["refs"][0]["traceId"], "TRC-single");

        // 指定命名空间：不是成员 → 403
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &token,
                json!({"namespace": "@other", "traces": [doc("TRC-ns", "ok", "2026-09-26T10:00:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(
            v["error"]["message"],
            "你不是命名空间 @other 的成员，不能把轨迹写进去"
        );
        // 显式给自己的空间（带 @）
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &token,
                json!({"namespace": "@alice", "traces": [doc("TRC-ns2", "ok", "2026-09-26T10:00:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["namespace"]["slug"], "alice");
        // 取用即声明：采集过的命名空间里有内置 trace 集合
        let kind: String =
            sqlx::query_scalar("SELECT kind FROM collections WHERE namespace_id = ?")
                .bind(&ns.id)
                .fetch_one(st.pool())
                .await
                .unwrap();
        assert_eq!(kind, "trace");
    }

    #[tokio::test]
    async fn 标注的非法输入() {
        let st = state("badlabels").await;
        let (_u, _ns, token) = seed(&st, "alice", &["trace:write", "trace:label"]).await;
        let app = app(&st);
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces",
                &token,
                json!({"traces": [doc("TRC-1", "ok", "2026-09-26T10:00:00Z")]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        let row_id = v["refs"][0]["id"].as_str().unwrap().to_string();
        let post =
            |body: Value| json_req("POST", &format!("/traces/{row_id}/labels"), &token, body);

        let (code, v) = call(&app, post(json!({"value": "x"}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_key");
        assert_eq!(
            v["error"]["message"],
            "缺 key（常用键见 GET /api/traces/kinds）"
        );
        let (code, v) = call(&app, post(json!({"key": "k".repeat(65)}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "key 太长（≤64）");
        let (code, v) = call(&app, post(json!({"score": 1001}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_score");
        assert_eq!(
            v["error"]["message"],
            "score 要在 -1..1 之间（千分位：-1000..1000）"
        );
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri(format!("/traces/{row_id}/labels"))
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from("not json"))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_json");
        // 自由键照收（键不封闭）
        let (code, v) = call(&app, post(json!({"key": "notes", "value": "一句话"}))).await;
        assert_eq!(code, StatusCode::CREATED);
        assert_eq!(v["label"]["key"], "notes");
        assert_eq!(v["label"]["value"], "一句话");
        // 不存在的轨迹 → 404（文案与 Go 一致）
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/traces/NOPE/labels",
                &token,
                json!({"key": "grade", "value": "pass"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "没有这条轨迹: NOPE");
    }

    #[tokio::test]
    async fn 时间解析_三种写法() {
        assert!(parse_trace_time("").is_none());
        assert!(parse_trace_time("不是时间").is_none());
        assert_eq!(
            parse_trace_time("2026-09-26T10:00:00Z")
                .unwrap()
                .to_rfc3339(),
            "2026-09-26T10:00:00+00:00"
        );
        assert!(parse_trace_time("2026-09-26").is_some());
        assert_eq!(
            parse_trace_time("1780000000").unwrap().timestamp(),
            1780000000
        );
    }
}
