//! 制品分享链接的 HTTP 层（原实现 `ncc-registry/httpapi/share.go`）。
//!
//! 两条路，一种东西：
//!
//! ```text
//! /api/shares           管理自己的分享（创建 / 列表 / 撤销）；?all=1 是治理动作
//! /s/<token>            落地页：给人看「这是什么、还能用几次」（**不计数**）
//! /s/<token>/raw        直接下发字节：给 curl / Agent（**只有这里计数**）
//! ```
//!
//! 刻意的取舍：
//!
//! * 创建分享**不**要 grants:write，但要求「我本来就读得到这条制品」——
//!   否则分享链接就成了绕过授权的通道（复用 `httpapi::artifacts::can_read`）。
//! * `token` 明文只在创建那一刻回显；其余接口一律只看得到 `hint` 前缀。
//! * 落地页对插值做 HTML 转义（Go 侧是直接拼串）：制品名是用户内容，同一份
//!   页面在 Go 版里是个注入口 —— 这里顺手堵上，正常数据下的输出与 Go 逐字相同。
//! * 本模块还导出两个小工具给同族的 `agentcards` 用（`ensure_admin` / `page_params`）：
//!   `admin.rs` 与 `helpers.rs` 不属于这两族，动它们会把别人的活搅在一起。

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::ids::new_id;
use ncc_core::timeutil::now_go;
use ncc_core::web;

use crate::config::Config;
use crate::httpapi::{helpers, AppState, Auth};
use crate::store;

/// 该族路由（相对 `/api`）。
///
/// 列表与撤销**不**挂登录中间件：它们要同时接受「普通用户（自己的分享）」与
/// 「管理员凭据（全部）」，两套身份在处理器里判一次就好 —— 挂上反而会把
/// admin key 挡在 401（它本来就不走 `Authorization: Bearer`）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/shares", get(list_shares).post(create_share))
        .route("/shares/", get(list_shares).post(create_share))
        .route("/shares/info/{token}", get(share_info))
        .route("/shares/{id}", delete(delete_share))
}

/// 顶层公开页：`/s/<token>`（不需要登录，token 本身就是凭据）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/s/{token}", get(share_page))
        .route("/s/{token}/raw", get(share_raw))
}

/* ---------------- 与 agentcards 共用的两个小工具 ---------------- */

/// 分页参数（`limit` / `offset`，带上限，避免一次把全表拉出来）。
pub(crate) fn page_params(uri: &Uri) -> (i64, i64) {
    let mut limit = web::query_i64(uri, "limit", 50);
    if limit <= 0 || limit > 200 {
        limit = 50;
    }
    let offset = web::query_i64(uri, "offset", 0).max(0);
    (limit, offset)
}

/// 本次请求的管理员身份（会话管理员账号，或带正确 admin key/secret 的机器凭据）。
pub(crate) struct AdminActor {
    pub kind: &'static str,
    pub id: String,
    pub name: String,
}

/// `admin_keys` 一行（只在本文件用）。
#[derive(sqlx::FromRow)]
struct AdminKeyRow {
    id: String,
    label: String,
    #[allow(dead_code)]
    key: String,
    secret_hash: String,
    revoked_at: Option<String>,
}

/// 判断这次请求是否持有管理员身份，并按 Go 的行为顺手 touch 一次 admin key。
///
/// 判定逻辑与 Go 的 `ensureAdmin` 完全一致（分享列表 / 撤销挂在普通组下，
/// 所以这里得自己判一次）；`admin.rs` 迁移后应与此处合并成一份。
pub(crate) async fn ensure_admin(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
) -> Option<AdminActor> {
    if let Some(a) = auth.info() {
        if a.session {
            if let Ok(Some(u)) = store::users::by_id(state.pool(), &a.user_id).await {
                if u.is_admin && !u.disabled {
                    return Some(AdminActor {
                        kind: "user",
                        id: u.id,
                        name: u.email,
                    });
                }
            }
        }
    }
    let key = header_text(headers, "X-NCC-Admin-Key");
    let secret = header_text(headers, "X-NCC-Admin-Secret");
    if key.is_empty() || secret.is_empty() {
        return None;
    }
    let row: Option<AdminKeyRow> = sqlx::query_as(
        "SELECT id, label, `key`, secret_hash, revoked_at FROM admin_keys WHERE `key` = ?",
    )
    .bind(&key)
    .fetch_optional(state.pool())
    .await
    .ok()
    .flatten();
    let k = row?;
    if k.revoked_at.is_some() || store::hash_secret(&secret) != k.secret_hash {
        return None;
    }
    let _ = sqlx::query("UPDATE admin_keys SET last_used_at = ? WHERE id = ?")
        .bind(now_go())
        .bind(&k.id)
        .execute(state.pool())
        .await;
    Some(AdminActor {
        kind: "admin_key",
        id: k.id,
        name: if k.label.trim().is_empty() {
            k.key
        } else {
            k.label
        },
    })
}

/// 记一条管理动作。失败只记日志 —— 审计写不进去不该让业务操作回滚，
/// 但一定要在服务端留下痕迹（否则就是「悄悄丢失的审计」）。
pub(crate) async fn audit(
    state: &AppState,
    actor: Option<&AdminActor>,
    action: &str,
    target: &str,
    target_name: &str,
    summary: &str,
    detail: Value,
    ip: &str,
) {
    let Some(actor) = actor else {
        // 没有管理员身份 = 不是治理动作（普通用户分享自己的制品不进审计）。
        return;
    };
    let raw = if detail.is_null() {
        "{}".to_string()
    } else {
        detail.to_string()
    };
    let res = sqlx::query(
        "INSERT INTO audit_logs (id, actor_kind, actor_id, actor_name, action, target, target_name, summary, detail, ip, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id("L"))
    .bind(actor.kind)
    .bind(&actor.id)
    .bind(&actor.name)
    .bind(action)
    .bind(target)
    .bind(target_name)
    .bind(summary)
    .bind(raw)
    .bind(ip)
    .bind(now_go())
    .execute(state.pool())
    .await;
    if let Err(e) = res {
        tracing::error!("审计写入失败 action={action} target={target}: {e}");
    }
}

fn header_text(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/* ---------------- 创建 / 列表 / 撤销（需登录） ---------------- */

#[derive(Deserialize)]
struct ShareCreateReq {
    #[serde(
        default,
        rename = "ref",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    artifact_ref: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    label: String,
    /// 0 = 不限次
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    uses: i64,
    /// 0 = 不过期
    #[serde(
        default,
        rename = "expiresInDays",
        deserialize_with = "crate::httpapi::helpers::de_or_default"
    )]
    expires_in_days: i64,
}

/// POST /api/shares
async fn create_share(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    body: Result<Json<ShareCreateReq>, JsonRejection>,
) -> ApiResult<Response> {
    // Go 用 requireAuth() 中间件，报文逐字对齐（注意 API-Key 的大小写是原文）
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("未认证或凭据无效（先 ncc login 或带 API-Key）"))?;
    let Json(body) = body.map_err(|_| ApiError::bad_request("bad_request", "请求体格式错误"))?;

    let ref_ = body.artifact_ref.trim().to_string();
    if ref_.is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺少 ref（@命名空间/slug 或 A-… id）",
        ));
    }
    let row = store::artifacts::by_ref(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("制品不存在"))?;
    // 只能分享「我本来就能读」的制品：否则分享链接就成了绕过授权的通道。
    if !crate::httpapi::artifacts::can_read(&state, &row, &a.user_id).await {
        return Err(ApiError::forbidden("你没有这条制品的读取权限，不能分享它"));
    }

    let uses = if body.uses < 0 { 0 } else { body.uses };
    let expires_at = if body.expires_in_days > 0 {
        Some(store::shares::expires_in_days(body.expires_in_days))
    } else {
        None
    };
    let (sh, token) = store::shares::create(
        state.pool(),
        &row.id,
        &row.namespace_id,
        &a.user_id,
        &body.label,
        uses,
        expires_at.clone(),
    )
    .await
    .map_err(|e| {
        tracing::error!("创建分享失败: {e}");
        ApiError::internal("创建分享失败")
    })?;

    let admin = ensure_admin(&state, &auth, &headers).await;
    let ip = web::client_ip(&headers);
    // 管理员分享进审计（治理动作）；普通用户分享自己的制品不进。
    audit(
        &state,
        admin.as_ref(),
        "share.create",
        &ref_,
        &row.name,
        "创建分享链接",
        json!({"shareId": sh.id, "uses": uses, "expiresAt": expires_at}),
        &ip,
    )
    .await;

    let link = share_link(state.cfg(), &token);
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({
            "share": share_json(state.cfg(), &sh, Some(&row), ""),
            "token": token, // 仅此一次
            "link": link,
            "rawLink": format!("{link}/raw"),
            "howto": {
                "human": "把 link 发给对方：浏览器打开是说明页，点按钮即可下载",
                "agent": format!("把 {link}/raw 交给 Agent：curl -OJ 即可拿到字节（不用登录）"),
                "note": "分享是临时放行：对方拿到字节即结束，不等于给他长期授权（那要 ncc grant）",
            },
        }),
    ))
}

/// GET /api/shares?mine=1|all=1
async fn list_shares(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let mut created_by = auth.user_id().unwrap_or_default();
    if web::query(&uri, "all").as_deref() == Some("1") {
        // 「看全部」是治理动作：要么是这个节点上的管理员账号，要么带 admin key/secret。
        if ensure_admin(&state, &auth, &headers).await.is_none() {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "admin_required",
                "查看全部分享需要节点管理员身份",
            ));
        }
        created_by = String::new();
    } else if auth.info().is_none() {
        return Err(ApiError::unauthorized(
            "未认证或凭据无效（先 ncc login 或带 API-KEY）",
        ));
    }

    let (limit, offset) = page_params(&uri);
    let rows = store::shares::list(state.pool(), &created_by, limit, offset)
        .await
        .map_err(ApiError::from_db)?;
    let total = store::shares::count(state.pool(), &created_by)
        .await
        .unwrap_or(0);
    let list: Vec<Value> = rows
        .iter()
        .map(|r| share_row_json(state.cfg(), r))
        .collect();
    Ok(helpers::ok_json(json!({
        "shares": list, "total": total, "limit": limit, "offset": offset,
    })))
}

/// DELETE /api/shares/{id} —— 撤自己的；管理员可撤任意（留审计）。
async fn delete_share(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let sh = store::shares::by_id(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("分享不存在"))?;
    let admin = ensure_admin(&state, &auth, &headers).await;
    let uid = auth.user_id().unwrap_or_default();
    let is_admin = admin.is_some();
    if uid.is_empty() && !is_admin {
        return Err(ApiError::unauthorized(
            "未认证或凭据无效（先 ncc login 或带 API-Key）",
        ));
    }
    if !is_admin && sh.created_by != uid {
        return Err(ApiError::forbidden("只能撤销自己创建的分享"));
    }
    // 走到这里已确认归属，撤销时不再限定 created_by（管理员本来就该能撤任意）。
    if !store::shares::revoke(state.pool(), &id, "")
        .await
        .map_err(ApiError::from_db)?
    {
        return Err(ApiError::internal("撤销失败"));
    }
    let ip = web::client_ip(&headers);
    audit(
        &state,
        admin.as_ref(),
        "share.revoke",
        &sh.id,
        &sh.label,
        "撤销分享链接",
        json!({"artifact": sh.artifact_id, "byAdmin": is_admin}),
        &ip,
    )
    .await;
    Ok(helpers::ok_json(json!({"ok": true})))
}

/* ---------------- 领取（公开，不需登录） ---------------- */

/// GET /s/{token} —— 落地页。
///
/// 不计数：给人看的页面被打开多次不该消耗次数（次数是给**取字节**用的）。
async fn share_page(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let Some(sh) = store::shares::by_token(state.pool(), &token)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Ok(page_html(
            StatusCode::NOT_FOUND,
            share_page_html("", "", "链接不存在（可能已被撤销或删除）", false, 0, 0, 0),
        ));
    };
    let Some(row) = store::artifacts::by_id(state.pool(), &sh.artifact_id)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Ok(page_html(
            StatusCode::NOT_FOUND,
            share_page_html("", "", "制品已被删除", false, 0, 0, 0),
        ));
    };
    let usable = sh.usable();
    let note = if usable {
        "链接有效"
    } else {
        "链接已失效（已撤销、已过期或次数用尽）"
    };
    let ref_ = format!("{}@{}", row.ref_of(), row.version);
    Ok(page_html(
        StatusCode::OK,
        share_page_html(
            &row.name,
            &ref_,
            note,
            usable,
            row.size,
            sh.max_uses,
            sh.remaining_uses(),
        ),
    ))
}

/// GET /s/{token}/raw —— 直接下发字节（curl / Agent 用）。**只有这里计数。**
async fn share_raw(
    State(state): State<AppState>,
    Path(token): Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    let Some(sh) = store::shares::by_token(state.pool(), &token)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Err(ApiError::not_found("链接不存在"));
    };
    let Some(row) = store::artifacts::by_id(state.pool(), &sh.artifact_id)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Err(ApiError::not_found("制品已被删除"));
    };

    // ?meta=1 只取元数据、不下发字节，也不计数 —— Agent 先看一眼再决定要不要拉。
    if web::query(&uri, "meta").as_deref() == Some("1") {
        return Ok(helpers::ok_json(json!({
            "share": share_json(state.cfg(), &sh, Some(&row), ""),
            "usable": sh.usable(),
            "file": {
                "name": row.slug, "kind": row.kind, "version": row.version,
                "sha256": row.sha256, "size": row.size, "ref": row.ref_of(),
            },
        })));
    }

    if !sh.usable() {
        return Err(ApiError::new(
            StatusCode::GONE,
            "share_expired",
            "链接已失效（已撤销、已过期或次数用尽）",
        ));
    }
    let _ = store::shares::mark_used(state.pool(), &sh.id).await;
    let _ = store::artifacts::bump_downloads(state.pool(), &row.id).await;

    let mut resp = if row.storage_provider != "local" || row.blob_name.is_empty() {
        // BYO 直链：字节不在本节点，302 过去
        axum::response::Redirect::temporary(&row.storage_url).into_response()
    } else {
        let data = state
            .blobs()
            .get(&row.blob_name)
            .map_err(|_| ApiError::not_found("字节已不在本节点（可能已被清理）"))?;
        let mut name = row.slug.clone();
        if let Some(ext) = std::path::Path::new(&row.blob_name).extension() {
            name.push('.');
            name.push_str(&ext.to_string_lossy());
        }
        let mut r = web::bytes_response(data, "application/octet-stream", Some(&name));
        if !row.sha256.is_empty() {
            if let Ok(v) = axum::http::HeaderValue::from_str(&row.sha256) {
                r.headers_mut().insert("x-ncc-sha256", v);
            }
        }
        r
    };
    let h = resp.headers_mut();
    if let Ok(v) = axum::http::HeaderValue::from_str(&sh.id) {
        h.insert("x-ncc-share", v);
    }
    // 与 Go 一致：只在「还有剩余次数」时给这个头，值是**这一次用完之后**的剩余量
    let left = sh.remaining_uses();
    if left > 0 {
        if let Ok(v) = axum::http::HeaderValue::from_str(&(left - 1).to_string()) {
            h.insert("x-ncc-share-remaining", v);
        }
    }
    Ok(resp)
}

/// GET /api/shares/info/{token} —— 公开的链接概要（CLI / 页面对账用；不消耗次数）。
async fn share_info(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let Some(sh) = store::shares::by_token(state.pool(), &token)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Err(ApiError::not_found("链接不存在"));
    };
    let Some(row) = store::artifacts::by_id(state.pool(), &sh.artifact_id)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Err(ApiError::not_found("制品已被删除"));
    };
    Ok(helpers::ok_json(json!({
        "share": share_json(state.cfg(), &sh, Some(&row), ""),
        "usable": sh.usable(),
    })))
}

/* ---------------- 视图 ---------------- */

pub(crate) fn share_link(cfg: &Config, token: &str) -> String {
    format!("{}/s/{}", cfg.public_url.trim_end_matches('/'), token)
}

/// 分享视图。`link` 是明文（只有创建时才有）—— 其余场合只给 hint 骨架。
fn share_json(
    cfg: &Config,
    sh: &store::shares::Share,
    row: Option<&store::artifacts::ArtifactRow>,
    link: &str,
) -> Value {
    let mut out = json!({
        "id": sh.id, "label": sh.label, "hint": sh.token_hint,
        "uses": {"max": sh.max_uses, "used": sh.used_count, "remaining": sh.remaining_uses()},
        "expiresAt": sh.expires_at, "revokedAt": sh.revoked_at,
        "lastUsedAt": sh.last_used_at, "createdAt": sh.created_at,
        "usable": sh.usable(),
        "link": if link.is_empty() { share_link(cfg, &format!("{}…", sh.token_hint)) } else { link.to_string() },
    });
    if let Some(row) = row {
        out["artifact"] = json!({
            "id": row.id, "name": row.name, "slug": row.slug, "kind": row.kind,
            "version": row.version, "ref": row.ref_of(), "sha256": row.sha256,
            "size": row.size, "status": row.status, "visibility": row.visibility,
            "namespace": {"slug": row.ns_slug, "name": row.ns_name},
        });
    }
    out
}

fn share_row_json(cfg: &Config, r: &store::shares::ShareRow) -> Value {
    let sh = r.as_share();
    let mut out = share_json(
        cfg,
        &sh,
        None,
        &share_link(cfg, &format!("{}…", sh.token_hint)),
    );
    out["artifact"] = json!({
        "id": r.artifact_id, "name": r.artifact_name, "slug": r.artifact_slug,
        "kind": r.artifact_kind, "sha256": r.artifact_sha, "size": r.artifact_size,
        "status": r.artifact_status, "ref": format!("@{}/{}", r.ns_slug.clone().unwrap_or_default(), r.artifact_slug.clone().unwrap_or_default()),
        "namespace": {"slug": r.ns_slug, "name": r.ns_name},
    });
    out["createdBy"] = json!({"id": r.created_by, "name": r.created_by_name});
    out
}

/* ---------------- 页面 ---------------- */

fn page_html(status: StatusCode, html: String) -> Response {
    (status, Html(html)).into_response()
}

/// HTML 转义（落地页里的制品名 / 命名空间来自库，是用户内容）。
/// `agentcards` 的落地页也用这一份（两族共用同一个转义口径）。
pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// 分享落地页（单文件、无依赖，和接入页同一路子）。
fn share_page_html(
    name: &str,
    ref_: &str,
    note: &str,
    usable: bool,
    size: i64,
    max: i64,
    left: i64,
) -> String {
    let title = esc(if name.is_empty() {
        "分享链接"
    } else {
        name
    });
    let (state, state_cls) = if usable {
        ("可下载", "ok")
    } else {
        ("不可用", "bad")
    };
    let btn = if usable {
        r#"<a class="btn" href="raw">下载文件</a>
    <div class="cmd"><code>curl -OJ {{RAW}}</code></div>"#
    } else {
        ""
    };
    let size_text = if size > 0 {
        format!("{size} B")
    } else {
        String::new()
    };
    let uses_text = if max > 0 {
        format!("{left} / {max} 次剩余")
    } else {
        "不限次".to_string()
    };
    format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title} · NCC Share</title>
<style>
 :root{{color-scheme:dark}}
 body{{margin:0;min-height:100vh;display:grid;place-items:center;background:#0b1017;color:#e6edf3;
      font:15px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI",system-ui,"PingFang SC","Microsoft YaHei",sans-serif}}
 .card{{width:min(560px,92vw);background:#121a24;border:1px solid #1f2937;border-radius:14px;padding:28px}}
 .k{{color:#7d8590;font-size:12px;letter-spacing:.08em;text-transform:uppercase}}
 h1{{margin:.2em 0 .1em;font-size:20px}}
 .ref{{color:#6ee7b7;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:13px}}
 .note{{margin:14px 0;padding:10px 12px;border-radius:9px;background:#0e1620;border:1px solid #1f2937;color:#9fb0c0}}
 .btn{{display:inline-block;margin-top:14px;padding:10px 18px;border-radius:9px;background:#2f81f7;color:#fff;
      text-decoration:none;font-weight:600}}
 .cmd{{margin-top:12px;padding:10px 12px;border-radius:9px;background:#0b1017;border:1px solid #1f2937;
      font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12.5px;color:#c9d1d9;overflow:auto}}
 .meta{{margin-top:16px;color:#7d8590;font-size:12.5px}}
 .ok{{color:#4ade80}}.bad{{color:#f87171}}
</style></head><body><div class="card">
 <div class="k">NCC Registry · 分享</div>
 <h1>{title}</h1>
 <div class="ref">{ref_}</div>
 <div class="note">{note} —— <span class="{state_cls}">{state}</span></div>
 {btn}
 <div class="meta">大小 {size_text} · {uses_text}</div>
 <div class="meta">这条链接是临时放行：拿到字节即结束，不代表长期授权。</div>
</div>
<script>
 // 页面地址后面拼 /raw 即为直链（这里把当前地址补全给命令示例，避免硬编码 host）。
 var raw = location.href.replace(/#.*$/, '').replace(/\/+$/, '') + '/raw';
 document.querySelectorAll('.cmd code').forEach(function(el){{ el.textContent = el.textContent.replace('{{RAW}}', raw); }});
</script>
</body></html>"#,
        title = title,
        ref_ = esc(ref_),
        note = esc(note),
        state = state,
        state_cls = state_cls,
        btn = btn,
        size_text = size_text,
        uses_text = uses_text,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use ncc_core::storage::LocalStorage;
    use tower::ServiceExt;

    fn test_dir(name: &str) -> std::path::PathBuf {
        // 落在工作区的 target/ 里（已 gitignore），别污染 crate 源码树
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-blobs")
            .join(format!("shares-{}-{name}", std::process::id()))
    }

    async fn state(name: &str) -> AppState {
        let pool = sqlx::Pool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL)
            .await
            .unwrap();
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://localhost:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();
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

    /// 建一个用户 + 一条私有制品 + 一把可用的 API-Key，返回 (用户 id, 制品, bearer)。
    async fn seed(
        state: &AppState,
        uid: &str,
        slug: &str,
    ) -> (String, store::artifacts::ArtifactRow, String) {
        let u = store::users::create(
            state.pool(),
            &format!("用户{uid}"),
            &format!("{uid}@x.com"),
            "h",
        )
        .await
        .unwrap();
        let ns = store::namespaces::create_account(state.pool(), &u.id, &u.name, slug)
            .await
            .unwrap();
        let a = store::artifacts::create(
            state.pool(),
            store::artifacts::NewArtifact {
                namespace_id: ns.id.clone(),
                slug: "demo".to_string(),
                kind: "skill".to_string(),
                name: "演示包".to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                visibility: "private".to_string(),
                status: "published".to_string(),
                manifest: String::new(),
                storage_provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: "b".to_string(),
                sha256: "sha-x".to_string(),
                size: 5,
                created_by: u.id.clone(),
            },
        )
        .await
        .unwrap();
        state.blobs().put("b", b"hello").unwrap();
        let (_k, secret) = store::apikeys::create(state.pool(), &u.id, "t", &[])
            .await
            .unwrap();
        (u.id, a, secret)
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
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

    /// 整站路由表装配：确认本族的 `/api/shares`、`/s/<token>`、`/a/<token>` 都**真的
    /// 挂上去了**（不是被 501 兜底接住）—— axum 建路由时若有冲突会在这里直接 panic。
    #[tokio::test]
    async fn 整站路由表_本族端点已挂载() {
        let st = state("router").await;
        let app = crate::router::build(&st);
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/api/shares")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
        // 公开页：HTML 404（说明路由命中本族处理器，而不是未迁移兜底的 501）
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/s/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/a/nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/api/agent-cards")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
    }

    #[tokio::test]
    async fn 创建分享_未登录被拒_登录后回明文token() {
        let st = state("create").await;
        let (_uid, a, secret) = seed(&st, "U-1", "zhangsan").await;
        let app = app(&st);

        // 未登录 → 401
        let (code, v) = call(&app, json_req("POST", "/shares", "", json!({"ref": a.id}))).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["code"], "unauthorized");

        // 体格式错 → 400 bad_request + Go 的文案
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/shares")
                .header("authorization", format!("Bearer {secret}"))
                .header("content-type", "application/json")
                .body(Body::from("{不是 JSON"))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "请求体格式错误");

        // 正常创建
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/shares",
                &secret,
                json!({"ref": a.id, "label": "给小李", "uses": 2, "expiresInDays": 1}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        let token = v["token"].as_str().unwrap().to_string();
        assert_eq!(token.len(), 32);
        assert_eq!(v["share"]["hint"], &token[..6]);
        assert_eq!(v["share"]["uses"]["max"], 2);
        assert_eq!(v["share"]["uses"]["remaining"], 2);
        assert_eq!(v["share"]["usable"], true);
        assert_eq!(v["share"]["label"], "给小李");
        assert_eq!(v["share"]["artifact"]["ref"], "@zhangsan/demo");
        assert_eq!(v["share"]["artifact"]["namespace"]["slug"], "zhangsan");
        assert_eq!(v["link"], format!("http://localhost:8282/s/{token}"));
        assert_eq!(v["rawLink"], format!("http://localhost:8282/s/{token}/raw"));
        // 响应里绝不出现明文 token 之外的存储形态
        let raw: String = sqlx::query_scalar("SELECT token_hash FROM artifact_shares")
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(raw, store::hash_secret(&token));
    }

    #[tokio::test]
    async fn 分享别人的私有制品被拒() {
        let st = state("forbidden").await;
        let (_u1, _a1, s1) = seed(&st, "U-1", "zhangsan").await;
        // 第二个用户 + 他的私有制品
        let u2 = store::users::create(st.pool(), "李四", "lisi@x.com", "h")
            .await
            .unwrap();
        let ns2 = store::namespaces::create_account(st.pool(), &u2.id, "李四", "lisi")
            .await
            .unwrap();
        let a2 = store::artifacts::create(
            st.pool(),
            store::artifacts::NewArtifact {
                namespace_id: ns2.id.clone(),
                slug: "secret".to_string(),
                kind: "skill".to_string(),
                name: "别人的私有包".to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                visibility: "private".to_string(),
                status: "published".to_string(),
                manifest: String::new(),
                storage_provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: "b".to_string(),
                sha256: String::new(),
                size: 1,
                created_by: u2.id.clone(),
            },
        )
        .await
        .unwrap();
        let app = app(&st);

        let (code, v) = call(
            &app,
            json_req("POST", "/shares", &s1, json!({"ref": a2.id})),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "forbidden");
        assert_eq!(
            v["error"]["message"],
            "你没有这条制品的读取权限，不能分享它"
        );

        // 不存在的制品 → 404
        let (code, v) = call(
            &app,
            json_req("POST", "/shares", &s1, json!({"ref": "A-nope"})),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "制品不存在");
        // 缺 ref → 400
        let (code, v) = call(&app, json_req("POST", "/shares", &s1, json!({}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "缺少 ref（@命名空间/slug 或 A-… id）"
        );
    }

    #[tokio::test]
    async fn 公开页与raw_取字节才计数_用尽即410() {
        let st = state("raw").await;
        let (uid, a, _s) = seed(&st, "U-1", "zhangsan").await;
        let (sh, token) =
            store::shares::create(st.pool(), &a.id, &a.namespace_id, &uid, "", 1, None)
                .await
                .unwrap();
        let app = app(&st);

        // 落地页：200 HTML，不计数
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/s/{token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        let html = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("演示包"), "{html}");
        assert!(html.contains("@zhangsan/demo@1.0.0"));
        assert!(html.contains("1 / 1 次剩余"));
        let used: i64 = sqlx::query_scalar("SELECT used_count FROM artifact_shares WHERE id = ?")
            .bind(&sh.id)
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(used, 0, "落地页不该计数");

        // info：不消耗次数
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/shares/info/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["usable"], true);
        assert_eq!(v["share"]["hint"], &token[..6]);

        // raw：拿到字节 + 计数
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/s/{token}/raw"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("x-ncc-share").unwrap(), sh.id.as_str());
        assert_eq!(resp.headers().get("x-ncc-sha256").unwrap(), "sha-x");
        let resp_remaining = resp
            .headers()
            .get("x-ncc-share-remaining")
            .map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], b"hello");
        // 限 1 次的名片：这一次用完之后剩余 0（Go 也会给这个头）
        assert_eq!(resp_remaining.as_deref(), Some("0"));
        let used: i64 = sqlx::query_scalar("SELECT used_count FROM artifact_shares WHERE id = ?")
            .bind(&sh.id)
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(used, 1);

        // 名额用尽 → 410 share_expired（落地页仍在，但写「已失效」）
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/s/{token}/raw"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(v["error"]["code"], "share_expired");
        assert_eq!(
            v["error"]["message"],
            "链接已失效（已撤销、已过期或次数用尽）"
        );

        // ?meta=1 不计数、不下发字节
        let (_sh2, token2) =
            store::shares::create(st.pool(), &a.id, &a.namespace_id, &uid, "", 1, None)
                .await
                .unwrap();
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/s/{token2}/raw?meta=1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["file"]["name"], "demo");
        assert_eq!(v["file"]["ref"], "@zhangsan/demo");
        assert_eq!(v["usable"], true);
        let used: i64 = sqlx::query_scalar("SELECT used_count FROM artifact_shares WHERE id = ?")
            .bind(
                &store::shares::by_token(st.pool(), &token2)
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
            )
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(used, 0, "meta 请求不该计数");

        // 不存在的 token → 404
        let (code, _v) = call(
            &app,
            Request::builder()
                .uri("/s/unknown/raw")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/s/unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let html = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(html.contains("链接不存在"));
    }

    #[tokio::test]
    async fn 撤销_越权与管理员路径() {
        let st = state("revoke").await;
        let (uid, a, secret) = seed(&st, "U-1", "zhangsan").await;
        let (sh, _t) =
            store::shares::create(st.pool(), &a.id, &a.namespace_id, &uid, "label-x", 0, None)
                .await
                .unwrap();
        let app = app(&st);

        // 未登录 → 401
        let (code, _v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/shares/{}", sh.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        // 不存在 → 404
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri("/shares/SH-nope")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "分享不存在");
        // 别人撤不动 → 403
        let other = store::users::create(st.pool(), "王五", "ww@x.com", "h")
            .await
            .unwrap();
        let (_k, other_secret) = store::apikeys::create(st.pool(), &other.id, "t", &[])
            .await
            .unwrap();
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/shares/{}", sh.id))
                .header("authorization", format!("Bearer {other_secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "只能撤销自己创建的分享");
        // 本人撤销 → 200，且再取字节是 410
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/shares/{}", sh.id))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["ok"], true);
        let (code, v) = call(
            &app,
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/shares/info/{}",
                    store::shares::by_id(st.pool(), &sh.id)
                        .await
                        .unwrap()
                        .unwrap()
                        .token_hint
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        // hint 不是 token，查不到
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "链接不存在");
    }

    #[tokio::test]
    async fn 列表_只列自己的_all需要管理员() {
        let st = state("list").await;
        let (uid, a, secret) = seed(&st, "U-1", "zhangsan").await;
        store::shares::create(st.pool(), &a.id, &a.namespace_id, &uid, "我的一", 0, None)
            .await
            .unwrap();
        let app = app(&st);

        // 未登录 → 401
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/shares")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(
            v["error"]["message"],
            "未认证或凭据无效（先 ncc login 或带 API-KEY）"
        );

        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/shares?limit=10")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["limit"], 10);
        assert_eq!(v["shares"][0]["createdBy"]["name"], "用户U-1");
        assert_eq!(v["shares"][0]["label"], "我的一");
        // 列表只有 hint，没有明文 token
        assert_eq!(v["shares"][0]["hint"].as_str().unwrap().len(), 6);
        assert!(v["shares"][0]["link"].as_str().unwrap().ends_with('…'));

        // all=1 但只是普通用户 → 403 admin_required
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/shares?all=1")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "admin_required");
        assert_eq!(v["error"]["message"], "查看全部分享需要节点管理员身份");

        // 管理员账号（会话 JWT）→ 可以看全部，并写审计
        sqlx::query("UPDATE users SET is_admin = 1 WHERE id = ?")
            .bind(&uid)
            .execute(st.pool())
            .await
            .unwrap();
        let jwt = crate::jwt::sign_user(
            &st.cfg().jwt_secret,
            &uid,
            "u@x.com",
            std::time::Duration::from_secs(3600),
        );
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/shares?all=1")
                .header("authorization", format!("Bearer {jwt}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 1);

        // admin key / secret 也认（且会 touch last_used_at）
        let key = "AK-TEST01";
        let secret_tok = "s3cret";
        sqlx::query(
            "INSERT INTO admin_keys (id, label, `key`, secret_hash, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind("AKX-test")
        .bind("运维脚本")
        .bind(key)
        .bind(store::hash_secret(secret_tok))
        .bind(&uid)
        .bind(ncc_core::timeutil::now_go())
        .execute(st.pool())
        .await
        .unwrap();
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/shares?all=1")
                .header("X-NCC-Admin-Key", key)
                .header("X-NCC-Admin-Secret", secret_tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 1);
        let touched: Option<String> = sqlx::query_scalar("SELECT last_used_at FROM admin_keys")
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert!(touched.is_some(), "admin key 命中要 touch last_used_at");
        // 错 secret → 不认
        let (code, _v) = call(
            &app,
            Request::builder()
                .uri("/shares?all=1")
                .header("X-NCC-Admin-Key", key)
                .header("X-NCC-Admin-Secret", "wrong")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        // 已撤销的 admin key → 不认
        sqlx::query("UPDATE admin_keys SET revoked_at = ? WHERE id = 'AKX-test'")
            .bind(ncc_core::timeutil::now_go())
            .execute(st.pool())
            .await
            .unwrap();
        let (code, _v) = call(
            &app,
            Request::builder()
                .uri("/shares?all=1")
                .header("X-NCC-Admin-Key", key)
                .header("X-NCC-Admin-Secret", secret_tok)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
    }
}
