//! 接入票据的 HTTP 层（原实现 `ncc-registry/httpapi/access.go` + `joinpage.go`）。
//!
//! 把「一个内网 registry」加进 Agent 有两种方式，用的是同一张票据：
//!
//! ```text
//! key + secret   手工填（key 短、可念；secret 只显示一次，库里只存哈希）
//! 接入短链        <publicURL>/j/<key>#<secret> —— secret 放 fragment，
//!                不进服务端日志、不进 Referer，点开/粘贴即可接入
//! ```
//!
//! 兑换（redeem）拿到的是一枚**节点令牌**：只能做票据给的事（默认：上报自己的心跳 +
//! 读公开制品），不能发布、不能改别人的东西。票据可限次、可过期、可停用。
//!
//! 刻意的取舍：
//!
//! * `secret` 是**秘密**：只在创建那一刻回显一次，其余接口一律回不出（列表只给
//!   `linkPattern`，里面是 `<secret>` 占位）。
//! * 加入页对插值做 HTML 转义（Go 侧是 html/template 的上下文转义；这里用与
//!   `shares` 同一份 `esc`），脚本里的字面量按 `jsonStringLiteral` 的口径转义
//!   （`<`/`>` 变 `\u003c`/`\u003e`，避免 `</script>` 提前闭合）。
//! * `GET /j/{key}` 不查登录：能拿到 key 就说明持有凭据（secret 才是那道门）。

use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::scope::{valid_scope, NODE_TICKET_SCOPES};

use crate::config::Config;
use crate::httpapi::shares::esc;
use crate::httpapi::{helpers, AppState, Auth};
use crate::jwt;
use crate::store;

/// 该族路由（相对 `/api`）。
///
/// `GET /tickets/{key}` 是公开概要、`DELETE /tickets/{id}` 要 `keys:write`：
/// Go 侧写了两个参数名（`:key` / `:id`），axum 的 matchit 把两条路径视作同一个
/// 形状（参数名不参与匹配），合成一条路径两种方法，语义完全一致。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/access/redeem", axum::routing::post(redeem))
        .route("/access/tickets", get(list_tickets).post(create_ticket))
        .route(
            "/access/tickets/{key}",
            get(ticket_info).delete(delete_ticket),
        )
}

/// 该族的顶层公开页：`/j/{key}`（接入短链的落地页）。
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/j/{key}", get(join_page))
}

/* ---------------- 请求体 ---------------- */

#[derive(Debug, Deserialize, Default)]
struct TicketCreateReq {
    #[serde(default)]
    label: String,
    /// 0 = 不限次。
    #[serde(default)]
    uses: i64,
    /// 0 = 不过期。
    #[serde(default, rename = "expiresInDays")]
    expires_in_days: i64,
    /// 缺省 = `NODE_TICKET_SCOPES`。
    #[serde(default)]
    scopes: Vec<String>,
    /// 缺省 = 创建者的个人命名空间。
    #[serde(default)]
    namespace: String,
}

#[derive(Debug, Deserialize, Default)]
struct TicketNodeReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    slug: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    os: String,
    #[serde(default)]
    arch: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RedeemReq {
    #[serde(default)]
    key: String,
    #[serde(default)]
    secret: String,
    /// 可选：兑换的同时把本机作为一个节点托管进来。
    #[serde(default)]
    node: Option<TicketNodeReq>,
}

fn bad_body() -> ApiError {
    ApiError::bad_request("bad_request", "请求体格式错误")
}

/// 解析 JSON 请求体（与 Go 的 `ShouldBindJSON` 同一套判据）。
///
/// 为什么要自己判「顶层必须是对象」：serde 的派生实现会**把数组按字段顺序塞进结构体**
/// （`visit_seq`），于是 `["x"]` 会被当成 `{"key":"x"}` —— 能跑、但语义全错。Go 这边
/// 是 400（`cannot unmarshal array into Go value of type ...`），所以这里显式挡掉。
/// 顶层 `null` 在 Go 里不报错（留下零值结构体），这里照做。
fn parse_body<T: serde::de::DeserializeOwned + Default>(b: &Bytes) -> ApiResult<T> {
    let v: Value = serde_json::from_slice(b).map_err(|_| bad_body())?;
    if v.is_null() {
        return Ok(T::default());
    }
    if !v.is_object() {
        return Err(bad_body());
    }
    serde_json::from_value(v).map_err(|_| bad_body())
}

/* ---------------- 票据管理 ---------------- */

/// POST /api/access/tickets —— 签发票据（需登录 + `keys:write`）。
///
/// 签发凭据属于敏感动作，沿用 `keys:write` 作用域（默认不发给普通 API-Key）。
async fn create_ticket(
    State(state): State<AppState>,
    auth: Auth,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth.require_scope("keys:write")?;
    let body: TicketCreateReq = parse_body(&body)?;

    // 不认识的作用域直接丢掉：票据是给对方的凭据，不能让它带一句谁也不懂的话。
    let mut scopes: Vec<String> = body
        .scopes
        .iter()
        .filter(|s| valid_scope(s.as_str()))
        .cloned()
        .collect();
    if scopes.is_empty() {
        scopes = NODE_TICKET_SCOPES.iter().map(|s| s.to_string()).collect();
    }

    let ns = ticket_namespace(&state, &a.user_id, &body.namespace).await?;

    let expires_at = if body.expires_in_days > 0 {
        Some(chrono::Local::now().fixed_offset() + chrono::Duration::days(body.expires_in_days))
    } else {
        None
    };
    let uses = body.uses.max(0);
    // 标签是人读的：掐到 60 个字，别让一长串把列表挤爆。
    let label: String = body.label.trim().chars().take(60).collect();

    let key = store::access::new_ticket_key();
    let secret = store::access::new_ticket_secret();
    let tk = store::access::create(
        state.pool(),
        &key,
        &secret,
        &label,
        &scopes,
        &ns.id,
        &a.user_id,
        uses,
        expires_at,
    )
    .await
    .map_err(|_| ApiError::internal("签发接入票据失败"))?;

    let cfg = state.cfg();
    let link = ticket_link(cfg, &tk.key, &secret);
    Ok(ncc_core::error::ok_status(
        StatusCode::CREATED,
        json!({
            "ticket": ticket_json(cfg, &tk, &ns.slug),
            "secret": secret, // 仅此一次返回
            "link": link,
            "howto": {
                "link": "把 link 发给对方：粘贴到 Agent 插件里，或 ncc registry add <link>",
                "manual": format!(
                    "或把 key/secret 分开给：ncc registry add --base {} --key {} --secret <secret>",
                    cfg.public_url, tk.key
                ),
            },
        }),
    ))
}

/// GET /api/access/tickets —— 我签发的票据（需登录）。
async fn list_tickets(State(state): State<AppState>, auth: Auth) -> ApiResult<Response> {
    let a = auth.require_login()?;
    let rows = store::access::list_by_creator(state.pool(), &a.user_id)
        .await
        .map_err(ApiError::from_db)?;

    // Go 是「逐行查一次命名空间」；这里加一层缓存是为了不把同一句 SQL 发 N 次，
    // 结果完全一样（同一个 id 只会查一次库）。
    let mut slugs: HashMap<String, String> = HashMap::new();
    for t in &rows {
        if t.namespace_id.is_empty() || slugs.contains_key(&t.namespace_id) {
            continue;
        }
        if let Ok(Some(ns)) = store::namespaces::by_id(state.pool(), &t.namespace_id).await {
            slugs.insert(ns.id.clone(), ns.slug);
        }
    }
    let items: Vec<Value> = rows
        .iter()
        .map(|t| {
            let slug = slugs.get(&t.namespace_id).cloned().unwrap_or_default();
            ticket_json(state.cfg(), t, &slug)
        })
        .collect();
    Ok(helpers::ok_json(json!({"tickets": items, "total": items.len()})))
}

/// DELETE /api/access/tickets/{id} —— 停用即失效（已兑换的令牌到期前仍有效，但节点令牌的
/// TTL 由票据决定，重新签发一次即可收缩窗口）。
///
/// 只能删自己签发的；删不掉也回 `ok:true`（删除是幂等动作，与 Go 一致）。
async fn delete_ticket(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = auth.require_scope("keys:write")?;
    store::access::delete(state.pool(), &id, &a.user_id)
        .await
        .map_err(|_| ApiError::internal("删除失败"))?;
    Ok(helpers::ok_json(json!({"ok": true})))
}

/// GET /api/access/tickets/{key} —— 公开的票据概要（不含 secret）。
///
/// 接入页与 CLI 用它先确认「这是谁的内网 registry、票据还能用几次」。
async fn ticket_info(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> ApiResult<Response> {
    let Some(tk) = store::access::by_key(state.pool(), &key)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Err(ApiError::not_found("票据不存在"));
    };
    let ns_slug = namespace_slug(&state, &tk.namespace_id).await;
    let mut out = ticket_json(state.cfg(), &tk, &ns_slug);
    out["usable"] = json!(tk.usable());
    out["registry"] = registry_block(&state).await;
    let registry = registry_block(&state).await;
    Ok(helpers::ok_json(json!({"ticket": out, "registry": registry})))
}

/* ---------------- 兑换（公开：key + secret） ---------------- */

/// POST /api/access/redeem —— 客户端（CLI / Agent 插件 / 接入页）拿 key+secret 换一枚
/// 节点令牌。带上 `node` 字段就同时把本机托管进来 ——「用一条短链加进内网 registry」
/// 就是这一步。
async fn redeem(State(state): State<AppState>, body: Bytes) -> ApiResult<Response> {
    let body: RedeemReq = parse_body(&body)?;

    let Some(tk) = store::access::by_key(state.pool(), &body.key)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "ticket_not_found",
            "key 不存在（票据可能已被删除）",
        ));
    };
    if !tk.secret_matches(&body.secret) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "bad_secret",
            "secret 不正确",
        ));
    }
    if !tk.usable() {
        return Err(unusable());
    }

    let creator = match store::users::by_id(state.pool(), &tk.created_by)
        .await
        .map_err(ApiError::from_db)?
    {
        Some(u) => u,
        None => return Err(unusable_with("票据签发者已不存在")),
    };

    // 票据落到哪个命名空间：优先票据上记的，取不到就退回签发者的个人空间。
    let mut ns = store::namespaces::personal(state.pool(), &creator.id)
        .await
        .unwrap_or(None);
    if !tk.namespace_id.is_empty() {
        if let Some(n) = store::namespaces::by_id(state.pool(), &tk.namespace_id)
            .await
            .unwrap_or(None)
        {
            ns = Some(n);
        }
    }
    let Some(ns) = ns else {
        return Err(ApiError::internal("票据没有可用命名空间"));
    };

    let mut scopes = tk.scope_list();
    if scopes.is_empty() {
        scopes = NODE_TICKET_SCOPES.iter().map(|s| s.to_string()).collect();
    }

    // 可选：兑换即入网（注册 + 首次心跳）。
    let mut node_out = Value::Null;
    let mut node_id = String::new();
    if let Some(n) = body.node.as_ref() {
        let name = n.name.trim();
        if !name.is_empty() {
            let req = store::nodes::HeartbeatReq {
                name: name.to_string(),
                slug: first_non_empty(&n.slug, name),
                kind: n.kind.clone(),
                region: n.region.trim().to_string(),
                url: n.url.trim().to_string(),
                os: n.os.clone(),
                arch: n.arch.clone(),
                version: n.version.clone(),
                agent: n.agent.clone(),
                capabilities: n.capabilities.clone(),
                visibility: "public".to_string(),
                ..Default::default()
            };
            let (row, created) = store::nodes::upsert(state.pool(), &ns.id, &req)
                .await
                .map_err(|_| ApiError::internal("入网注册失败"))?;
            node_id = row.id.clone();
            // 取不到完整行时也给一个对象：Go 侧那行 `nodeOut["created"] = created`
            // 会对 nil map 赋值直接 panic，这里不复制那个崩溃。
            let mut out = match store::nodes::by_id(state.pool(), &creator.id, &row.id).await {
                Ok(Some(full)) => crate::httpapi::nodes::node_json(&full, state.cfg().node_ttl),
                _ => json!({}),
            };
            out["created"] = json!(created);
            node_out = out;
        }
    }

    // 令牌有效期：`access_ttl` 与票据剩余时间的**更短者** —— 票据一到期，
    // 拿在手里的令牌不该还活着。
    let mut ttl = state.cfg().access_ttl;
    if let Some(exp) = tk.expires() {
        let left = exp.signed_duration_since(chrono::Local::now().fixed_offset());
        if let Ok(left) = left.to_std() {
            if !left.is_zero() && left < ttl {
                ttl = left;
            }
        }
    }
    let token = jwt::sign(
        &state.cfg().jwt_secret,
        jwt::Claims {
            sub: creator.id.clone(),
            kind: "node".to_string(),
            node: node_id,
            scope: scopes.clone(),
            ..Default::default()
        },
        ttl,
    );
    let _ = store::access::mark_used(state.pool(), &tk.id).await;

    Ok(helpers::ok_json(json!({
        "token": token,
        "expiresAt": chrono::Local::now().fixed_offset()
            + chrono::Duration::from_std(ttl).unwrap_or_default(),
        "scopes": scopes,
        "node": node_out,
        "registry": registry_block(&state).await,
        "ticket": {"id": tk.id, "key": tk.key, "label": tk.label},
        "owner": {"id": creator.id, "name": creator.name, "namespace": ns.slug},
    })))
}

/* ---------------- 接入页（短链落地页） ---------------- */

/// GET /j/{key} —— 短链的可读落地页。
///
/// secret 在 fragment（`#`）里，浏览器不会把它发给服务端；页面用 JS 读出来，
/// 生成可复制的 CLI 命令，并提供「在这个浏览器里接入」按钮。
async fn join_page(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Response, ApiError> {
    let cfg = state.cfg();
    let Some(tk) = store::access::by_key(state.pool(), &key)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Ok(page_html(
            StatusCode::NOT_FOUND,
            join_page_html(
                &key,
                "票据不存在（可能已被删除）",
                false,
                &cfg.public_url,
                "",
            ),
        ));
    };
    let usable = tk.usable();
    let note = if usable {
        "票据有效"
    } else {
        "票据已停用、已过期或次数用尽"
    };
    let ns_slug = namespace_slug(&state, &tk.namespace_id).await;
    Ok(page_html(
        StatusCode::OK,
        join_page_html(&tk.key, note, usable, &cfg.public_url, &ns_slug),
    ))
}

/* ---------------- 工具 ---------------- */

fn unusable() -> ApiError {
    unusable_with("票据已停用、已过期或次数用尽")
}

/// 兑换过程里「票据用不了」的统一出口（403 ticket_unusable）。
fn unusable_with(msg: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "ticket_unusable", msg)
}

/// 命名空间的 slug（取不到就空串 —— 概要页缺个 slug 不该整个 500）。
async fn namespace_slug(state: &AppState, ns_id: &str) -> String {
    if ns_id.is_empty() {
        return String::new();
    }
    store::namespaces::by_id(state.pool(), ns_id)
        .await
        .ok()
        .flatten()
        .map(|n| n.slug)
        .unwrap_or_default()
}

/// 解析票据落到哪个命名空间（为空 = 创建者的个人空间）。
async fn ticket_namespace(
    state: &AppState,
    user_id: &str,
    slug: &str,
) -> Result<store::namespaces::Namespace, ApiError> {
    let slug = slug.trim();
    if slug.is_empty() {
        return store::namespaces::personal(state.pool(), user_id)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| {
                ApiError::bad_request("bad_request", "当前账号没有个人命名空间，请重新注册")
            });
    }
    let ns = store::namespaces::by_slug(state.pool(), slug)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| {
            ApiError::bad_request("bad_request", format!("namespace {slug} 不存在"))
        })?;
    if !helpers::can_manage(state, &ns.id, user_id).await {
        return Err(ApiError::forbidden("你不是该 namespace 的 owner/成员"));
    }
    Ok(ns)
}

/// 接入短链：secret 放 fragment。
fn ticket_link(cfg: &Config, key: &str, secret: &str) -> String {
    format!("{}/j/{}#{}", cfg.public_url.trim_end_matches('/'), key, secret)
}

fn ticket_json(cfg: &Config, t: &store::access::AccessTicket, ns_slug: &str) -> Value {
    json!({
        "id": t.id, "key": t.key, "label": t.label,
        "scopes": t.scope_list(),
        "uses": {"max": t.max_uses, "used": t.used_count},
        "namespace": {"id": t.namespace_id, "slug": ns_slug},
        "expiresAt": t.expires(), "disabled": t.disabled,
        "lastUsedAt": t.last_used(), "createdAt": t.created(),
        "usable": t.usable(),
        // 短链本身不带 secret（secret 只在创建时返回一次），这里给的是「补上 secret 后的样子」。
        "linkPattern": format!("{}/j/{}#<secret>", cfg.public_url.trim_end_matches('/'), t.key),
    })
}

/// 「这个内网 registry 是什么」—— 接入前后都用同一段信息。
async fn registry_block(state: &AppState) -> Value {
    let cfg = state.cfg();
    let (artifacts, nodes, users) = helpers::counts(state).await;
    json!({
        "base": cfg.public_url, "role": cfg.role,
        "nodeId": cfg.node_id, "nodeName": cfg.node_name,
        "region": cfg.node_region, "version": ncc_core::REGISTRY_VERSION,
        "console": format!("{}/", cfg.public_url),
        "counts": {"artifacts": artifacts, "hostedNodes": nodes, "users": users},
    })
}

fn first_non_empty(a: &str, b: &str) -> String {
    if a.trim().is_empty() {
        b.to_string()
    } else {
        a.to_string()
    }
}

fn page_html(status: StatusCode, html: String) -> Response {
    (status, Html(html)).into_response()
}

/// 把任意字符串编码成可安全嵌进 `<script>` 的 JS 字符串字面量。
///
/// 顺带把 `<`/`>` 转成 `\u003c`/`\u003e`，防止 `</script>` 提前闭合脚本块。
fn json_string_literal(s: &str) -> String {
    let lit = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    lit.replace('<', "\\u003c").replace('>', "\\u003e")
}

/// 接入页模板。`@@xxx@@` 是占位符（刻意不用 format!：这页里 CSS/JS 的大括号太多，
/// 逐个转义成 `{{` 只会让人在改样式时提心吊胆）。
const JOIN_PAGE_TPL: &str = r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>接入内网 NCC Registry</title>
<meta name="robots" content="noindex,nofollow" />
<style>
:root{--bg:#fff;--bg2:#f9fafb;--bg3:#f3f4f6;--ink:#111827;--ink2:#4b5563;--ink3:#6b7280;
--ink4:#9ca3af;--line:#e5e7eb;--line2:#f3f4f6;--ok:#16a34a;--err:#dc2626;
--mono:'JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
--sans:'Inter',-apple-system,BlinkMacSystemFont,'Segoe UI','PingFang SC','Noto Sans SC',system-ui,sans-serif}
*{margin:0;padding:0;box-sizing:border-box}
body{font-family:var(--sans);color:var(--ink);background:var(--bg);line-height:1.65;-webkit-font-smoothing:antialiased}
.wrap{max-width:760px;margin:0 auto;padding:44px 24px 64px}
.head{display:flex;align-items:center;gap:12px;margin-bottom:6px}
.logo{display:grid;place-items:center;width:30px;height:30px;border-radius:9px;background:var(--ink);color:#fff;font-weight:800;font-size:14px}
h1{font-size:21px;font-weight:800;letter-spacing:-.015em}
.sub{color:var(--ink3);font-size:14px;margin:10px 0 22px}
.badge{display:inline-block;font-family:var(--mono);font-size:10.5px;font-weight:700;letter-spacing:.03em;
padding:2px 9px;border-radius:999px;border:1px solid var(--line);color:var(--ink3);background:#fff}
.badge.ok{color:var(--ok);border-color:#bbf7d0}
.badge.no{color:var(--err);border-color:#fecaca}
.kv{display:grid;grid-template-columns:repeat(auto-fit,minmax(170px,1fr));gap:1px;background:var(--line);
border:1px solid var(--line);border-radius:12px;overflow:hidden;margin:18px 0 22px}
.kv div{background:#fff;padding:11px 13px}
.kv b{display:block;font-size:10.5px;font-weight:700;letter-spacing:.06em;text-transform:uppercase;color:var(--ink4);margin-bottom:3px}
.kv i{font-style:normal;font-family:var(--mono);font-size:12.5px;word-break:break-all}
h2{font-size:12px;font-weight:700;letter-spacing:.08em;text-transform:uppercase;color:var(--ink3);margin:24px 0 10px}
pre{font-family:var(--mono);font-size:12.5px;background:var(--bg2);border:1px solid var(--line2);
border-radius:12px;padding:14px 16px;overflow-x:auto;color:var(--ink2);line-height:1.85}
pre b{color:var(--ink)}
button{font-family:inherit;font-size:13px;font-weight:600;border:1px solid var(--line);background:#fff;
color:var(--ink);border-radius:9px;padding:8px 14px;cursor:pointer;margin:0 8px 8px 0}
button:hover{border-color:var(--ink)}
button.primary{background:var(--ink);color:#fff;border-color:var(--ink)}
.warn{border:1px solid #fecaca;background:#fef2f2;color:#991b1b;border-radius:12px;padding:12px 14px;font-size:13.5px;margin:16px 0}
.msg{font-size:13.5px;margin-top:10px;white-space:pre-wrap}
.mono{font-family:var(--mono);font-size:12.5px}
.muted{color:var(--ink3)}
footer{margin-top:36px;color:var(--ink4);font-size:12.5px;border-top:1px solid var(--line2);padding-top:14px}
</style>
</head>
<body>
<div class="wrap">
  <div class="head">
    <div class="logo">N</div>
    <h1>接入内网 NCC Registry</h1>
  </div>
  <p class="sub">这是一个自托管的内网节点（制品托管 · 节点托管 · Agent 发现与互联）。链接里带着接入凭据，粘贴即可接入。</p>

  <p>
    <span class="badge @@CLS@@">@@NOTE@@</span>
    <span class="badge">key @@KEY@@</span>
    @@NSBADGE@@
  </p>

  <div class="kv">
    <div><b>服务地址</b><i>@@BASE@@</i></div>
    <div><b>key</b><i>@@KEY@@</i></div>
    <div><b>secret</b><i id="secretView">（读取链接片段…）</i></div>
    <div><b>控制台</b><i>@@BASE@@/</i></div>
  </div>

  <div id="noFragment" class="warn" style="display:none">
    这个链接里没有 secret（片段被去掉了）。请让签发者重新发一条完整短链，
    或用 <span class="mono">--key / --secret</span> 分开传入。
  </div>

  <h2>方式一：命令行接入（推荐）</h2>
  <pre id="cmdBox">ncc registry add &lt;这条链接&gt; --join</pre>
  <button class="primary" id="copyCmd">复制命令</button>
  <button id="copyLink">复制完整链接</button>
  <div class="msg muted" id="copyMsg"></div>

  <h2>方式二：手工填 key / secret</h2>
  <pre id="manualBox">ncc registry add --base @@BASE@@ --key @@KEY@@ --secret &lt;secret&gt;
# 或只登录、不托管本机：
ncc --base @@BASE@@ registry login --key @@KEY@@ --secret &lt;secret&gt;</pre>

  <h2>方式三：在这个浏览器里验证</h2>
  <p class="muted" style="font-size:13.5px">只做一次兑换请求，确认 key/secret 与这个节点的连通性；不会把凭据存到服务端。</p>
  <button id="redeemBtn">用本链接凭据接入</button>
  <pre id="result" style="display:none"></pre>

  <footer>
    凭据在链接的 <span class="mono">#</span> 片段里，浏览器不会把它发给服务端 —— 也就不进访问日志。
    接入后拿到的是**节点令牌**：默认只允许上报自己的心跳与读取公开制品。
  </footer>
</div>

<script>
const KEY = @@KEYJSON@@;
const BASE = @@BASEJSON@@;
const hash = location.hash.startsWith('#') ? location.hash.slice(1) : '';
const secret = hash.startsWith('s=') ? hash.slice(2) : hash;

const secretView = document.getElementById('secretView');
const cmdBox = document.getElementById('cmdBox');
const fullLink = BASE + '/j/' + KEY + '#' + secret;

if (!secret) {
  document.getElementById('noFragment').style.display = 'block';
  secretView.textContent = '—';
} else {
  secretView.textContent = secret.slice(0, 6) + '…（已读取，共 ' + secret.length + ' 位）';
  cmdBox.textContent = 'ncc registry add ' + fullLink + ' --join';
}

function flash(el, text) {
  el.textContent = text;
  setTimeout(() => { el.textContent = ''; }, 2500);
}
document.getElementById('copyCmd').onclick = async () => {
  try { await navigator.clipboard.writeText(cmdBox.textContent); flash(document.getElementById('copyMsg'), '已复制命令'); }
  catch (e) { flash(document.getElementById('copyMsg'), '复制失败，请手动选中'); }
};
document.getElementById('copyLink').onclick = async () => {
  try { await navigator.clipboard.writeText(fullLink); flash(document.getElementById('copyMsg'), '已复制链接'); }
  catch (e) { flash(document.getElementById('copyMsg'), '复制失败，请手动选中'); }
};
document.getElementById('redeemBtn').onclick = async () => {
  const out = document.getElementById('result');
  out.style.display = 'block';
  if (!secret) { out.textContent = '链接里没有 secret，无法兑换。'; return; }
  out.textContent = '兑换中…';
  try {
    const r = await fetch('/api/access/redeem', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ key: KEY, secret })
    });
    const d = await r.json();
    out.textContent = JSON.stringify(d, null, 2);
  } catch (e) {
    out.textContent = '请求失败：' + e;
  }
};
</script>
</body>
</html>
"#;

/// 渲染接入页。插值一律转义：key 由服务端生成，但 base 来自配置、note 未来可能带
/// 用户内容，别在这里留一个注入口。
fn join_page_html(key: &str, note: &str, usable: bool, base: &str, ns_slug: &str) -> String {
    let base = base.trim_end_matches('/');
    let ns_badge = if ns_slug.is_empty() {
        String::new()
    } else {
        format!(r#"<span class="badge">命名空间 @{}</span>"#, esc(ns_slug))
    };
    JOIN_PAGE_TPL
        .replace("@@KEYJSON@@", &json_string_literal(key))
        .replace("@@BASEJSON@@", &json_string_literal(base))
        .replace("@@NSBADGE@@", &ns_badge)
        .replace("@@CLS@@", if usable { "ok" } else { "no" })
        .replace("@@NOTE@@", &esc(note))
        .replace("@@KEY@@", &esc(key))
        .replace("@@BASE@@", &esc(base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use ncc_core::storage::LocalStorage;
    use tower::ServiceExt;

    fn test_dir(name: &str) -> std::path::PathBuf {
        // 落在工作区的 target/ 里（已 gitignore），别污染 crate 源码树
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-blobs")
            .join(format!("access-{}-{name}", std::process::id()))
    }

    /// 每个测试一个临时文件库（不要用 `sqlite::memory:` + 连接池）。
    async fn state(name: &str) -> AppState {
        let dir = std::env::temp_dir().join(format!("ncc-access-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = ncc_core::pool::open_sqlite(&dir.join("t.db")).await.unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL).await.unwrap();
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://10.0.0.9:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();
        cfg.node_id = "ND-TEST".to_string();
        cfg.node_name = "测试节点".to_string();
        cfg.blob_dir = test_dir(name);
        let blobs = LocalStorage::new(&cfg.blob_dir, &cfg.public_url, "blobs").unwrap();
        let seal = ncc_core::secretbox::SecretBox::new(&cfg.jwt_secret).unwrap();
        AppState {
            cfg: std::sync::Arc::new(cfg),
            pool,
            blobs: std::sync::Arc::new(blobs),
            seal: std::sync::Arc::new(seal),
        }
    }

    fn app(state: &AppState) -> Router {
        Router::new()
            .merge(routes())
            .merge(public_routes())
            .with_state(state.clone())
    }

    /// 建一个用户 + 个人命名空间 + 一把带指定作用域的 API-Key。
    async fn user_with_key(state: &AppState, uid: &str, scopes: &[&str]) -> (String, String) {
        let u = store::users::create(state.pool(), &format!("用户{uid}"), &format!("{uid}@x.com"), "h")
            .await
            .unwrap();
        store::namespaces::create_account(state.pool(), &u.id, &u.name, uid)
            .await
            .unwrap();
        let owned: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        let (_k, secret) = store::apikeys::create(state.pool(), &u.id, "t", &owned).await.unwrap();
        (u.id, secret)
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    async fn call_raw(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/html; charset=utf-8"
        );
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
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

    /// 整站路由表装配：`/api/access/*` 与 `/j/{key}` 都**真的挂上去了**。
    #[tokio::test]
    async fn 整站路由表_本族端点已挂载() {
        let st = state("router").await;
        let app = crate::router::build(&st);
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/api/access/tickets")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
        // 加入页：404 是本族处理器给的（未迁移兜底会是 501 + JSON）
        let (code, html) = call_raw(
            &app,
            Request::builder()
                .uri("/j/不存在的key")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(html.contains("票据不存在（可能已被删除）"), "{html}");
        // 兑换是公开的：JSON 体不对 → 400（而不是 501）
        let (code, v) = call(&app, json_req("POST", "/api/access/redeem", "", json!({}))).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["code"], "ticket_not_found");
    }

    #[tokio::test]
    async fn 签发票据_权限与形态() {
        let st = state("create").await;
        let app = app(&st);
        let (_uid, weak) = user_with_key(&st, "U-1", &["registry:read"]).await;
        let (_uid2, strong) = user_with_key(&st, "U-2", &["keys:write"]).await;

        // 未登录 → 401
        let (code, v) = call(
            &app,
            json_req("POST", "/access/tickets", "", json!({"label": "x"})),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");

        // 有凭据但缺 keys:write → 403
        let (code, _v) = call(
            &app,
            json_req("POST", "/access/tickets", &weak, json!({"label": "x"})),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);

        // 请求体不是 JSON → 400 请求体格式错误（不是 axum 的 422）
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/access/tickets")
                .header("authorization", format!("Bearer {strong}"))
                .header("content-type", "application/json")
                .body(Body::from("{不是 JSON"))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "请求体格式错误");

        // 正常签发
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/access/tickets",
                &strong,
                json!({"label": "  内网节点  ", "uses": 3, "expiresInDays": 1}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        let t = &v["ticket"];
        assert!(t["key"].as_str().unwrap().starts_with("NK-"));
        assert_eq!(t["label"], "内网节点");
        assert_eq!(t["uses"]["max"], 3);
        assert_eq!(t["uses"]["used"], 0);
        assert_eq!(t["disabled"], false);
        assert_eq!(t["usable"], true);
        assert!(t["expiresAt"].is_string());
        assert_eq!(t["linkPattern"], format!("http://10.0.0.9:8282/j/{}#<secret>", t["key"].as_str().unwrap()));
        // 默认作用域 = 票据作用域表
        assert_eq!(
            t["scopes"],
            json!(["nodes:write", "registry:read", "registry:download"])
        );
        // secret 32 位 hex，且只在创建时出现
        let secret = v["secret"].as_str().unwrap();
        assert_eq!(secret.len(), 32);
        assert_eq!(v["link"], format!("http://10.0.0.9:8282/j/{}#{secret}", t["key"].as_str().unwrap()));
        assert!(v["howto"]["manual"].as_str().unwrap().contains("--key"));
        // 库里只有哈希
        let row = store::access::by_key(st.pool(), t["key"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.secret_hash, store::hash_secret(secret));
        assert_ne!(row.secret_hash, secret);
    }

    #[tokio::test]
    async fn 签发票据_作用域与命名空间() {
        let st = state("create2").await;
        let app = app(&st);
        let (uid, strong) = user_with_key(&st, "U-1", &["keys:write"]).await;

        // 非法作用域被丢掉；圈外命名空间 → 403
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/access/tickets",
                &strong,
                json!({"scopes": ["registry:read", "并不存在的作用域"]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["ticket"]["scopes"], json!(["registry:read"]));

        // 只有非法作用域 → 回落到默认表
        let (_code, v) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"scopes": ["胡写"]})),
        )
        .await;
        assert_eq!(v["ticket"]["scopes"].as_array().unwrap().len(), 3);

        // namespace 不存在 → 400
        let (code, v) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"namespace": "nope"})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "namespace nope 不存在");

        // 不是我管理的命名空间 → 403
        let (_other, _) = user_with_key(&st, "U-9", &["keys:write"]).await;
        let (code, v) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"namespace": "u-9"})),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "你不是该 namespace 的 owner/成员");

        // uses 为负按 0（不限次）处理
        let (_code, v) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"uses": -5})),
        )
        .await;
        assert_eq!(v["ticket"]["uses"]["max"], 0);
        let _ = uid;
    }

    #[tokio::test]
    async fn 列票据_需登录且不回secret() {
        let st = state("list").await;
        let app = app(&st);
        let (_uid, strong) = user_with_key(&st, "U-1", &["keys:write"]).await;

        let (code, _) = call(
            &app,
            Request::builder()
                .uri("/access/tickets")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);

        let (code, created) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "甲"})),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        let key = created["ticket"]["key"].as_str().unwrap().to_string();

        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/access/tickets")
                .header("authorization", format!("Bearer {strong}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["total"], 1);
        let t = &v["tickets"][0];
        assert_eq!(t["key"], key);
        assert_eq!(t["label"], "甲");
        assert_eq!(t["namespace"]["slug"], "u-1");
        assert!(t.get("secret").is_none(), "列表绝不能回 secret");
        assert!(t["linkPattern"].as_str().unwrap().ends_with("#<secret>"));
    }

    #[tokio::test]
    async fn 公开概要_与删除只限自己() {
        let st = state("info").await;
        let app = app(&st);
        let (_uid, strong) = user_with_key(&st, "U-1", &["keys:write"]).await;
        let (_uid2, other_admin) = user_with_key(&st, "U-2", &["keys:write"]).await;

        let (_code, created) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "甲"})),
        )
        .await;
        let key = created["ticket"]["key"].as_str().unwrap().to_string();
        let id = created["ticket"]["id"].as_str().unwrap().to_string();

        // 公开概要：不带凭据也能看（不含 secret）
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/access/tickets/{key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["ticket"]["usable"], true);
        assert_eq!(v["ticket"]["key"], key);
        assert!(v["ticket"]["registry"]["base"].is_string());
        assert_eq!(v["registry"]["nodeId"], "ND-TEST");
        assert_eq!(v["registry"]["counts"]["users"], 2);
        assert!(v["ticket"].get("secret").is_none());

        // 不存在的 key → 404 票据不存在
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/access/tickets/NK-NOPE")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "票据不存在");

        // 删别人的 → 不回错，但票还在
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/access/tickets/{id}"))
                .header("authorization", format!("Bearer {other_admin}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert!(store::access::by_id(st.pool(), &id).await.unwrap().is_some());

        // 删自己的 → 真删
        let (code, _) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/access/tickets/{id}"))
                .header("authorization", format!("Bearer {strong}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(store::access::by_id(st.pool(), &id).await.unwrap().is_none());

        // 删除要 keys:write
        let (_u, weak) = user_with_key(&st, "U-3", &["registry:read"]).await;
        let (code, _) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/access/tickets/{id}"))
                .header("authorization", format!("Bearer {weak}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn 兑换_成功路径与令牌内容() {
        let st = state("redeem").await;
        let app = app(&st);
        let (_uid, strong) = user_with_key(&st, "U-1", &["keys:write"]).await;

        let (_code, created) = call(
            &app,
            json_req(
                "POST",
                "/access/tickets",
                &strong,
                json!({"label": "甲", "uses": 2, "expiresInDays": 1}),
            ),
        )
        .await;
        let key = created["ticket"]["key"].as_str().unwrap().to_string();
        let secret = created["secret"].as_str().unwrap().to_string();

        let (code, v) = call(
            &app,
            json_req("POST", "/access/redeem", "", json!({"key": key, "secret": secret})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["scopes"], json!(["nodes:write", "registry:read", "registry:download"]));
        assert_eq!(v["owner"]["namespace"], "u-1");
        assert_eq!(v["ticket"]["key"], key);
        assert_eq!(v["registry"]["role"], "master");
        assert!(v["node"].is_null());
        assert!(v["expiresAt"].is_string());

        // 令牌本身：kind=node、scopes 一致，且有效期是「票据剩余时间」（1 天，
        // 比默认 720 小时的 access_ttl 短）
        let token = v["token"].as_str().unwrap();
        let info = jwt::parse(&st.cfg().jwt_secret, token).unwrap();
        assert_eq!(info.kind, "node");
        assert!(!info.session);
        assert_eq!(info.scopes, vec!["nodes:write", "registry:read", "registry:download"]);
        let payload = ncc_core::jwt::verify_hs256(&st.cfg().jwt_secret, token).unwrap();
        let life = payload["exp"].as_i64().unwrap() - payload["iat"].as_i64().unwrap();
        assert!(life > 86000 && life <= 86400, "有效期应为一整天：{life}");

        // 次数记账
        let row = store::access::by_key(st.pool(), &key).await.unwrap().unwrap();
        assert_eq!(row.used_count, 1);
        assert!(row.last_used_at.is_some());
    }

    #[tokio::test]
    async fn 兑换_密钥与可用性错误() {
        let st = state("redeem2").await;
        let app = app(&st);
        let (_uid, strong) = user_with_key(&st, "U-1", &["keys:write"]).await;

        // key 不存在
        let (code, v) = call(
            &app,
            json_req("POST", "/access/redeem", "", json!({"key": "NK-NOPE", "secret": "s"})),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["code"], "ticket_not_found");

        // secret 错
        let (_code, created) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "甲", "uses": 1})),
        )
        .await;
        let key = created["ticket"]["key"].as_str().unwrap().to_string();
        let secret = created["secret"].as_str().unwrap().to_string();
        let (code, v) = call(
            &app,
            json_req("POST", "/access/redeem", "", json!({"key": key, "secret": "别猜"})),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["code"], "bad_secret");

        // 体格式错：顶层是数组（Go 也是 400 —— 不能因为 serde 会按字段顺序塞进去就放过）
        let (code, v) = call(&app, json_req("POST", "/access/redeem", "", json!(["不是对象"]))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], "请求体格式错误");
        // 字段类型不对（Go 同样是 400）
        let (code, _v) = call(&app, json_req("POST", "/access/redeem", "", json!({"key": 1}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        // 顶层 null：Go 的 json.Unmarshal 不报错，留下零值结构体 → 走到「key 不存在」
        let (code, v) = call(&app, json_req("POST", "/access/redeem", "", Value::Null)).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["code"], "ticket_not_found");

        // 次数用尽：第一次成功（这里不带上 node），第二次 403
        let (code, _) = call(
            &app,
            json_req("POST", "/access/redeem", "", json!({"key": key, "secret": secret})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let (code, v) = call(
            &app,
            json_req("POST", "/access/redeem", "", json!({"key": key, "secret": secret})),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "ticket_unusable");

        // 停用 / 过期同样是 403 ticket_unusable
        let (code, created) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "乙"})),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        let key2 = created["ticket"]["key"].as_str().unwrap().to_string();
        let secret2 = created["secret"].as_str().unwrap().to_string();
        sqlx::query("UPDATE access_tickets SET disabled = 1 WHERE `key` = ?")
            .bind(&key2)
            .execute(st.pool())
            .await
            .unwrap();
        let (code, v) = call(
            &app,
            json_req("POST", "/access/redeem", "", json!({"key": key2, "secret": secret2})),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "ticket_unusable");
    }

    #[tokio::test]
    async fn 兑换_带上节点即入网() {
        let st = state("redeem-node").await;
        let app = app(&st);
        let (_uid, strong) = user_with_key(&st, "U-1", &["keys:write"]).await;

        let (_code, created) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "甲"})),
        )
        .await;
        let key = created["ticket"]["key"].as_str().unwrap().to_string();
        let secret = created["secret"].as_str().unwrap().to_string();

        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/access/redeem",
                "",
                json!({"key": key, "secret": secret,
                       "node": {"name": " 内网机 01 ", "kind": "service", "capabilities": ["gpu"]}}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let node = &v["node"];
        assert_eq!(node["created"], true);
        assert_eq!(node["name"], "内网机 01");
        assert_eq!(node["slug"], "01");
        assert_eq!(node["visibility"], "public");
        assert_eq!(node["capabilities"], json!(["gpu"]));
        assert_eq!(node["namespace"]["slug"], "u-1");
        // 令牌里绑定了这个节点
        let info = jwt::parse(&st.cfg().jwt_secret, v["token"].as_str().unwrap()).unwrap();
        assert_eq!(info.node_id, node["id"].as_str().unwrap());

        // 同名再兑换一次：同一条节点记录，created=false（注册与心跳本就是一件事）
        let (_code, created2) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "乙"})),
        )
        .await;
        let key2 = created2["ticket"]["key"].as_str().unwrap().to_string();
        let secret2 = created2["secret"].as_str().unwrap().to_string();
        let (_code, v2) = call(
            &app,
            json_req(
                "POST",
                "/access/redeem",
                "",
                json!({"key": key2, "secret": secret2, "node": {"name": "内网机 01"}}),
            ),
        )
        .await;
        assert_eq!(v2["node"]["created"], false);
        assert_eq!(v2["node"]["id"], node["id"]);
        // name 为空时不动节点
        let (_code, created3) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "丙"})),
        )
        .await;
        let (_code, v3) = call(
            &app,
            json_req(
                "POST",
                "/access/redeem",
                "",
                json!({"key": created3["ticket"]["key"], "secret": created3["secret"],
                       "node": {"name": "   "}}),
            ),
        )
        .await;
        assert!(v3["node"].is_null());
    }

    #[tokio::test]
    async fn 兑换_签发者不存在时票据不可用() {
        let st = state("redeem-orphan").await;
        let app = app(&st);
        let (uid, strong) = user_with_key(&st, "U-1", &["keys:write"]).await;
        let (_code, created) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "甲"})),
        )
        .await;
        // 直接删用户（模拟签发者注销）
        sqlx::query("DELETE FROM users WHERE id = ?")
            .bind(&uid)
            .execute(st.pool())
            .await
            .unwrap();
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/access/redeem",
                "",
                json!({"key": created["ticket"]["key"], "secret": created["secret"]}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "ticket_unusable");
        assert_eq!(v["error"]["message"], "票据签发者已不存在");
    }

    #[tokio::test]
    async fn 加入页_有票与无票() {
        let st = state("join").await;
        let app = app(&st);
        let (_uid, strong) = user_with_key(&st, "U-1", &["keys:write"]).await;
        let (_code, created) = call(
            &app,
            json_req("POST", "/access/tickets", &strong, json!({"label": "甲"})),
        )
        .await;
        let key = created["ticket"]["key"].as_str().unwrap().to_string();

        let (code, html) = call_raw(
            &app,
            Request::builder()
                .uri(format!("/j/{key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(html.contains("票据有效"), "{html}");
        assert!(html.contains(&format!(r#"key {key}"#)), "{html}");
        assert!(html.contains(&format!(r#"const KEY = "{key}""#)), "{html}");
        assert!(html.contains("命名空间 @u-1"), "{html}");
        assert!(html.contains("http://10.0.0.9:8282"), "{html}");
        assert!(!html.contains("@@"), "占位符没被替换干净");

        // 停用后同一页：note 变不可用
        sqlx::query("UPDATE access_tickets SET disabled = 1 WHERE `key` = ?")
            .bind(&key)
            .execute(st.pool())
            .await
            .unwrap();
        let (_code, html) = call_raw(
            &app,
            Request::builder()
                .uri(format!("/j/{key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert!(html.contains("票据已停用、已过期或次数用尽"), "{html}");
    }

    #[test]
    fn 加入页_插值转义() {
        let html = join_page_html(
            "NK-\"><script>alert(1)</script>",
            "票据有效",
            true,
            "http://a/",
            "<b>ns</b>",
        );
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains(r"\u003cscript\u003e"), "{html}");
        assert!(html.contains("&lt;b&gt;ns&lt;/b&gt;"), "{html}");
        // base 去掉尾部斜杠，控制台地址只有一个斜杠
        assert!(html.contains("<i>http://a/</i>"));
        assert!(!html.contains("http://a//"));
    }
}
