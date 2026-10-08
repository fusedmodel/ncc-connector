//! 节点治理面（`/api/admin/*`）：用户 / 节点 / 服务的查看与处理，每个动作都写审计。
//!
//! 两种凭据都能进，判定逻辑与 Go 的 `requireAdmin` **逐条一致**：
//!
//! * 人     —— 本节点管理员账号的会话（`User.IsAdmin`，第一个注册用户自动获得）；
//! * 机器   —— `X-NCC-Admin-Key` + `X-NCC-Admin-Secret`（`AK-…` + secret，库里只存 sha256）。
//!
//! 机器凭据刻意**不并进认证提取器**：治理权（管人、管节点）与资产权（看/发制品、上报心跳）
//! 权限面完全不同，混在一条判定链上，迟早出「一个漏判放行全站」的事故。
//!
//! `feedback` 族也要判「你是不是管理员」（`all=1`、处置按钮），所以 `ensure_admin` 是
//! `pub` 的：那里不接受 401/403，只回答「是 / 不是」。

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};

use ncc_core::error::{ok, ok_status, ApiError, ApiResult};
use ncc_core::web;

use crate::httpapi::{helpers, nodes, AppState, Auth};
use crate::store;

/// 审计动作名（统一在这里定义，别在 handler 里写自由字符串）。
pub const ACT_USER_DISABLE: &str = "user.disable";
pub const ACT_USER_ENABLE: &str = "user.enable";
pub const ACT_USER_PASSWD: &str = "user.password.reset";
pub const ACT_NODE_DELETE: &str = "node.delete";
pub const ACT_SERVICE_ARCHIVE: &str = "service.archive";
pub const ACT_SERVICE_DELETE: &str = "service.delete";
pub const ACT_KEY_ROTATE: &str = "admin.key.rotate";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/admin/overview", get(admin_overview))
        .route("/admin/users", get(admin_list_users))
        .route("/admin/users/{id}", patch(admin_patch_user))
        .route("/admin/users/{id}/password", post(admin_reset_password))
        .route("/admin/nodes", get(admin_list_nodes))
        .route("/admin/nodes/{id}", delete(admin_delete_node))
        .route("/admin/services", get(admin_list_services))
        .route("/admin/services/{ref}", delete(admin_archive_service))
        .route(
            "/admin/services/{ref}/{slug}",
            delete(admin_archive_service_slug),
        )
        .route("/admin/audit", get(admin_list_audit))
        .route("/admin/keys", get(admin_list_keys))
        .route("/admin/keys/rotate", post(admin_rotate_key))
}

/// 本族没有顶层公开页（治理面只在 `/api/admin` 下）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
}

/* ---------------- 门禁 ---------------- */

/// 管理动作的执行者（人 / 机器）。
#[derive(Debug, Clone)]
pub struct AdminActor {
    pub kind: String,
    pub id: String,
    pub name: String,
}

fn bad_body() -> ApiError {
    ApiError::bad_request("bad_request", "请求体格式错误")
}

fn header_trim(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn first_non_empty(a: &str, b: &str) -> String {
    if a.trim().is_empty() {
        b.to_string()
    } else {
        a.to_string()
    }
}

/// 管理员门禁：会话管理员与 admin key/secret 等价。
pub async fn require_admin(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
) -> Result<AdminActor, ApiError> {
    if let Some(info) = auth.info() {
        if info.session {
            let hit = store::users::by_id(state.pool(), &info.user_id)
                .await
                .map_err(ApiError::from_db)?;
            return match hit {
                Some(u) if u.is_admin && !u.disabled => Ok(AdminActor {
                    kind: "user".to_string(),
                    id: u.id,
                    name: u.email,
                }),
                _ => Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "admin_required",
                    "本账号不是节点管理员",
                )),
            };
        }
    }

    let key = header_trim(headers, "x-ncc-admin-key");
    let secret = header_trim(headers, "x-ncc-admin-secret");
    if !key.is_empty() || !secret.is_empty() {
        if key.is_empty() || secret.is_empty() {
            return Err(ApiError::bad_request(
                "bad_request",
                "admin key 与 secret 必须同时提供",
            ));
        }
        let hit = store::admin::find_admin_key(state.pool(), &key)
            .await
            .map_err(ApiError::from_db)?;
        let good = hit
            .as_ref()
            .map(|k| k.active() && store::hash_secret(&secret) == k.secret_hash)
            .unwrap_or(false);
        if !good {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "admin_credentials_invalid",
                "admin key/secret 不正确或已被轮换",
            ));
        }
        let k = hit.expect("上面已确认命中");
        let _ = store::admin::touch_admin_key(state.pool(), &k.id).await;
        return Ok(AdminActor {
            kind: "admin_key".to_string(),
            id: k.key.clone(),
            name: first_non_empty(&k.label, &k.key),
        });
    }

    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "admin_required",
        "需要节点管理员身份：管理员账号登录，或带 admin key/secret",
    ))
}

/// 只回答「是不是管理员」，不抛错（分享列表 / 反馈可见性用）。
pub async fn ensure_admin(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
) -> Option<AdminActor> {
    if let Some(info) = auth.info() {
        if info.session {
            if let Ok(Some(u)) = store::users::by_id(state.pool(), &info.user_id).await {
                if u.is_admin && !u.disabled {
                    return Some(AdminActor {
                        kind: "user".to_string(),
                        id: u.id,
                        name: u.email,
                    });
                }
            }
        }
    }
    let key = header_trim(headers, "x-ncc-admin-key");
    let secret = header_trim(headers, "x-ncc-admin-secret");
    if key.is_empty() || secret.is_empty() {
        return None;
    }
    let k = store::admin::find_admin_key(state.pool(), &key)
        .await
        .ok()??;
    if !k.active() || store::hash_secret(&secret) != k.secret_hash {
        return None;
    }
    let _ = store::admin::touch_admin_key(state.pool(), &k.id).await;
    Some(AdminActor {
        kind: "admin_key".to_string(),
        id: k.key.clone(),
        name: first_non_empty(&k.label, &k.key),
    })
}

/// 记一条管理动作。失败只记日志 —— 审计写不进去不该让业务操作回滚，
/// 但一定要在服务端留下痕迹（否则就是「悄悄丢失的审计」）。
async fn audit(
    state: &AppState,
    actor: &AdminActor,
    headers: &HeaderMap,
    action: &str,
    target: &str,
    target_name: &str,
    summary: &str,
    detail: Value,
) {
    let raw = if detail.is_null() {
        "{}".to_string()
    } else {
        detail.to_string()
    };
    let entry = store::admin::NewAudit {
        actor_kind: actor.kind.clone(),
        actor_id: actor.id.clone(),
        actor_name: actor.name.clone(),
        action: action.to_string(),
        target: target.to_string(),
        target_name: target_name.to_string(),
        summary: summary.to_string(),
        detail: raw,
        ip: web::client_ip(headers),
    };
    if let Err(e) = store::admin::append_audit(state.pool(), &entry).await {
        tracing::warn!("审计写入失败 action={action} target={target}: {e}");
    }
}

fn audit_json(l: &store::admin::AuditLog) -> Value {
    json!({
        "id": l.id,
        "actor": {"kind": l.actor_kind, "id": l.actor_id, "name": l.actor_name},
        "action": l.action, "target": l.target, "targetName": l.target_name,
        "summary": l.summary, "detail": web::parse_json_any(&l.detail), "ip": l.ip,
        "createdAt": l.created_at,
    })
}

/// 「这个内网 registry 是什么」（与 access 族那段同形；跨族复用私有函数要改别人文件，故复刻）。
async fn registry_block(state: &AppState) -> Value {
    let cfg = state.cfg();
    let (artifacts, nodes, users) = helpers::counts(state).await;
    json!({
        "base": cfg.public_url, "role": cfg.role,
        "nodeId": cfg.node_id, "nodeName": cfg.node_name,
        "region": cfg.node_region, "version": ncc_core::REGISTRY_VERSION,
        "console": format!("{}/", cfg.public_url.trim_end_matches('/')),
        "counts": {"artifacts": artifacts, "hostedNodes": nodes, "users": users},
    })
}

/// 分页参数（limit 默认 50、封顶 200；offset 默认 0）。
fn page_params(uri: &axum::http::Uri) -> (i64, i64) {
    let mut limit = web::query_i64(uri, "limit", 50);
    if limit <= 0 || limit > 200 {
        limit = 50;
    }
    let mut offset = web::query_i64(uri, "offset", 0);
    if offset < 0 {
        offset = 0;
    }
    (limit, offset)
}

/* ---------------- 概览 ---------------- */

/// GET /api/admin/overview
async fn admin_overview(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let actor = require_admin(&state, &auth, &headers).await?;
    let users = store::admin::count_users_filtered(state.pool(), "")
        .await
        .unwrap_or(0);
    let admins = store::admin::count_admins(state.pool()).await.unwrap_or(0);
    let nodes_count = store::admin::count_all_nodes(state.pool(), "", "", "")
        .await
        .unwrap_or(0);
    let kinds = store::admin::node_kind_counts(state.pool())
        .await
        .unwrap_or_default();
    let services = store::admin::count_service_artifacts(state.pool(), "", "")
        .await
        .unwrap_or(0);
    let artifacts = store::admin::count_published_public_artifacts(state.pool())
        .await
        .unwrap_or(0);
    let configs = store::configs::count(state.pool()).await.unwrap_or(0);
    let shares = store::shares::count(state.pool(), "").await.unwrap_or(0);
    let active_shares = store::shares::count_active(state.pool()).await.unwrap_or(0);
    let actions = store::admin::count_audit(state.pool(), "")
        .await
        .unwrap_or(0);

    let node_kinds: Value = kinds
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect::<serde_json::Map<String, Value>>()
        .into();

    Ok(ok(json!({
        "node": registry_block(&state).await,
        "counts": {
            "users": users, "admins": admins,
            "nodes": nodes_count, "nodeKinds": node_kinds,
            "services": {
                "hostedNodes": kinds.get(store::nodes::NODE_SERVICE).copied().unwrap_or(0),
                "artifacts": services,
            },
            "artifacts": artifacts,
            "configs": configs,
            "shares": shares,
            "activeShares": active_shares,
            "auditActions": actions,
        },
        "credential": {"kind": actor.kind, "name": actor.name},
    })))
}

/* ---------------- 用户 ---------------- */

fn admin_user_json(r: &store::admin::AdminUserRow) -> Value {
    json!({
        "id": r.id, "email": r.email, "name": r.name, "plan": r.plan,
        "isAdmin": r.is_admin, "disabled": r.disabled, "disabledAt": r.disabled_at,
        "adminNote": r.admin_note, "lastLoginAt": r.last_login_at, "createdAt": r.created_at,
        "stats": {"nodes": r.nodes, "artifacts": r.artifacts},
    })
}

/// GET /api/admin/users?q=&limit=&offset=
async fn admin_list_users(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    require_admin(&state, &auth, &headers).await?;
    let q = web::query(&uri, "q").unwrap_or_default();
    let (limit, offset) = page_params(&uri);
    let rows = store::admin::list_users(state.pool(), &q, limit, offset)
        .await
        .map_err(ApiError::from_db)?;
    let total = store::admin::count_users_filtered(state.pool(), &q)
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<Value> = rows.iter().map(admin_user_json).collect();
    Ok(ok(
        json!({"users": list, "total": total, "limit": limit, "offset": offset}),
    ))
}

#[derive(Debug, Deserialize, Default)]
struct UserPatchReq {
    #[serde(default)]
    disabled: Option<bool>,
    #[serde(default, rename = "adminNote")]
    admin_note: Option<String>,
}

/// PATCH /api/admin/users/{id} —— 禁用 / 启用 + 备注。两条硬规则：
/// 不能禁用最后一个可用管理员，不能禁用自己的账号。
async fn admin_patch_user(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<UserPatchReq>, JsonRejection>,
) -> ApiResult<Response> {
    let me = require_admin(&state, &auth, &headers).await?;
    let mut target = store::users::by_id(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    let Json(body) = body.map_err(|_| bad_body())?;

    if let Some(next) = body.disabled {
        if next != target.disabled {
            // 「不能禁自己」只对人说的：admin key 不是账号，禁不到它头上。
            if next && me.kind == "user" && target.id == me.id {
                return Err(ApiError::bad_request("bad_request", "不能禁用自己的账号"));
            }
            if next && target.is_admin {
                if store::admin::count_admins(state.pool()).await.unwrap_or(0) <= 1 {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "last_admin",
                        "这是最后一个可用管理员，不能禁用",
                    ));
                }
            }
            let note = body.admin_note.clone().unwrap_or_default();
            store::admin::set_user_disabled(state.pool(), &target.id, next, Some(&note))
                .await
                .map_err(ApiError::from_db)?;
            let (action, summary) = if next {
                (ACT_USER_DISABLE, "禁用账号")
            } else {
                (ACT_USER_ENABLE, "启用账号")
            };
            audit(
                &state,
                &me,
                &headers,
                action,
                &target.id,
                &target.email,
                summary,
                json!({"note": note}),
            )
            .await;
            target.disabled = next;
        } else if let Some(note) = body.admin_note.clone() {
            store::admin::set_user_disabled(state.pool(), &target.id, target.disabled, Some(&note))
                .await
                .map_err(ApiError::from_db)?;
            audit(
                &state,
                &me,
                &headers,
                ACT_USER_ENABLE,
                &target.id,
                &target.email,
                "更新备注",
                json!({"note": note}),
            )
            .await;
        }
    }

    // 回读一次，保证响应是库里的真实状态。
    if let Some(fresh) = store::users::by_id(state.pool(), &target.id)
        .await
        .map_err(ApiError::from_db)?
    {
        target = fresh;
    }
    Ok(ok(json!({
        "user": {
            "id": target.id, "email": target.email, "name": target.name,
            "isAdmin": target.is_admin, "disabled": target.disabled,
            "disabledAt": target.disabled_at, "adminNote": target.admin_note,
        }
    })))
}

#[derive(Debug, Deserialize, Default)]
struct PassReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    password: String,
}

/// 生成临时密码：去掉容易看错的 0/O/1/l/I，方便电话里念。
fn random_password(n: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

/// POST /api/admin/users/{id}/password —— 重置密码并返回新值（仅此一次）。
async fn admin_reset_password(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<PassReq>, JsonRejection>,
) -> ApiResult<Response> {
    let me = require_admin(&state, &auth, &headers).await?;
    let target = store::users::by_id(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    // Go 里 ShouldBindJSON 的错误被忽略：请求体坏 = 没给密码 = 服务端生成一个。
    let body = body.map(|Json(v)| v).unwrap_or_default();
    let mut pw = body.password.trim().to_string();
    let generated = pw.is_empty();
    if generated {
        pw = random_password(12);
    }
    if pw.len() < 6 {
        return Err(ApiError::bad_request("bad_request", "密码至少 6 位"));
    }
    let hash =
        ncc_core::crypto::hash_password(&pw).map_err(|_| ApiError::internal("密码处理失败"))?;
    store::users::update_pass(state.pool(), &target.id, &hash)
        .await
        .map_err(ApiError::from_db)?;
    audit(
        &state,
        &me,
        &headers,
        ACT_USER_PASSWD,
        &target.id,
        &target.email,
        "重置密码",
        json!({"generated": generated, "by": me.name}),
    )
    .await;
    Ok(ok(json!({
        "ok": true, "userId": target.id, "email": target.email,
        "password": pw, "generated": generated,
        "note": "这个密码只在这里显示一次，请立刻告知本人并让其自行修改",
    })))
}

/* ---------------- 节点 ---------------- */

/// GET /api/admin/nodes?kind=&region=&q=
async fn admin_list_nodes(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    require_admin(&state, &auth, &headers).await?;
    let kind = web::query(&uri, "kind").unwrap_or_default();
    let region = web::query(&uri, "region").unwrap_or_default();
    let q = web::query(&uri, "q").unwrap_or_default();
    let (limit, offset) = page_params(&uri);
    let ttl = state.cfg().node_ttl;
    let rows = store::admin::list_all_nodes(state.pool(), &kind, &region, &q, limit, offset)
        .await
        .map_err(ApiError::from_db)?;
    let total = store::admin::count_all_nodes(state.pool(), &kind, &region, &q)
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut v = nodes::node_json(r, ttl);
            // 管理面要多一眼「是谁的节点」：给邮箱（name 只是昵称，找人不方便）。
            v["ownerEmail"] = json!(r.owner_email);
            v
        })
        .collect();
    Ok(ok(
        json!({"nodes": list, "total": total, "limit": limit, "offset": offset}),
    ))
}

/// DELETE /api/admin/nodes/{id} —— 摘除节点（不论归属），并清掉指向它的连接。
async fn admin_delete_node(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let me = require_admin(&state, &auth, &headers).await?;
    let row = store::nodes::by_id(state.pool(), "", &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("节点不存在"))?;
    store::admin::delete_hosted_node_as_admin(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?;
    audit(
        &state,
        &me,
        &headers,
        ACT_NODE_DELETE,
        &row.id,
        &row.name,
        "摘除托管节点",
        json!({
            "kind": row.kind, "namespace": row.ns_slug,
            "owner": row.owner_name, "visibility": row.visibility,
        }),
    )
    .await;
    Ok(ok(json!({"ok": true})))
}

/* ---------------- 服务 ---------------- */

/// GET /api/admin/services?source=node|artifact|all&kind=&q=
///
/// 「服务」在 ncc-registry 里有两个落点：托管节点里 `kind=service` 的节点（正在跑的服务）
/// 与制品里 `kind=api` 的条目（被声明/交付的服务接口）。管理台要一次看全。
async fn admin_list_services(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    require_admin(&state, &auth, &headers).await?;
    let source = {
        let s = web::query(&uri, "source").unwrap_or_default();
        if s.trim().is_empty() {
            "all".to_string()
        } else {
            s
        }
    };
    let q = web::query(&uri, "q").unwrap_or_default();
    let (limit, offset) = page_params(&uri);
    let ttl = state.cfg().node_ttl;

    let mut node_services: Vec<Value> = Vec::new();
    let mut api_artifacts: Vec<Value> = Vec::new();
    let (mut node_total, mut art_total) = (0i64, 0i64);

    if source == "all" || source == "node" {
        let rows = store::admin::list_all_nodes(
            state.pool(),
            store::nodes::NODE_SERVICE,
            "",
            &q,
            limit,
            offset,
        )
        .await
        .map_err(ApiError::from_db)?;
        node_services = rows
            .iter()
            .map(|r| {
                let mut v = nodes::node_json(r, ttl);
                v["source"] = json!("node");
                v["ownerEmail"] = json!(r.owner_email);
                v
            })
            .collect();
        node_total =
            store::admin::count_all_nodes(state.pool(), store::nodes::NODE_SERVICE, "", &q)
                .await
                .unwrap_or(0);
    }

    if source == "all" || source == "artifact" {
        // 只在单看制品侧时才允许换 kind（默认 kind=api）。
        let kind = if source == "artifact" {
            web::query(&uri, "kind").unwrap_or_default()
        } else {
            String::new()
        };
        let rows = store::admin::list_service_artifacts(state.pool(), &kind, &q, limit, offset)
            .await
            .map_err(ApiError::from_db)?;
        api_artifacts = rows
            .iter()
            .map(|r| {
                let mut v = crate::httpapi::artifacts::artifact_json(r);
                v["source"] = json!("artifact");
                v
            })
            .collect();
        art_total = store::admin::count_service_artifacts(state.pool(), &kind, &q)
            .await
            .unwrap_or(0);
    }

    Ok(ok(json!({
        "source": source,
        "nodeServices": node_services,
        "apiArtifacts": api_artifacts,
        "total": {
            "nodeServices": node_total, "apiArtifacts": art_total,
            "all": node_total + art_total,
        },
        "limit": limit, "offset": offset,
    })))
}

async fn admin_archive_service(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(ref_): Path<String>,
) -> ApiResult<Response> {
    archive_service(&state, &auth, &headers, ref_).await
}

async fn admin_archive_service_slug(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path((ref_, slug)): Path<(String, String)>,
) -> ApiResult<Response> {
    archive_service(&state, &auth, &headers, format!("{ref_}/{slug}")).await
}

/// DELETE /api/admin/services/{ref}
///
/// ref 两种形态，处理方式不同（这是刻意的）：
///
/// * `ND-…`        托管节点 → **摘除**（服务已不在跑）
/// * `@ns/slug`    制品     → **归档**（从目录消失，字节保留，是否删除交给条目所属者）
async fn archive_service(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    ref_: String,
) -> ApiResult<Response> {
    let me = require_admin(state, auth, headers).await?;
    let ref_ = ref_.trim().to_string();

    // 节点侧：ND-… 或 @ns/slug（节点与制品都可能是 @ns/slug，所以先按节点找一次）
    if ref_.starts_with("ND-") {
        let row = store::nodes::by_id(state.pool(), "", &ref_)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| ApiError::not_found("节点不存在"))?;
        store::admin::delete_hosted_node_as_admin(state.pool(), &row.id)
            .await
            .map_err(ApiError::from_db)?;
        audit(
            state,
            &me,
            headers,
            ACT_SERVICE_DELETE,
            &row.id,
            &row.name,
            "摘除服务节点",
            json!({"kind": row.kind, "owner": row.owner_name}),
        )
        .await;
        return Ok(ok(json!({"ok": true, "target": "node", "id": row.id})));
    }

    if ref_.starts_with('@') {
        if let Some(row) = store::artifacts::by_ref(state.pool(), &ref_)
            .await
            .map_err(ApiError::from_db)?
        {
            store::admin::archive_artifact(state.pool(), &row.id)
                .await
                .map_err(ApiError::from_db)?;
            audit(
                state,
                &me,
                headers,
                ACT_SERVICE_ARCHIVE,
                &ref_,
                &row.name,
                "归档服务条目",
                json!({"kind": row.kind, "namespace": row.ns_slug, "before": row.status}),
            )
            .await;
            return Ok(ok(
                json!({"ok": true, "target": "artifact", "id": row.id, "status": "archived"}),
            ));
        }
        // 也可能是一条托管节点（@ns/slug 形式）——管理台列表里两处都用了这个引用。
        if let Some((ns, slug)) = ref_.trim_start_matches('@').split_once('/') {
            if let Some(n) = store::admin::find_node_by_ns_slug(state.pool(), ns, slug)
                .await
                .map_err(ApiError::from_db)?
            {
                store::admin::delete_hosted_node_as_admin(state.pool(), &n.id)
                    .await
                    .map_err(ApiError::from_db)?;
                audit(
                    state,
                    &me,
                    headers,
                    ACT_SERVICE_DELETE,
                    &n.id,
                    &n.name,
                    "摘除服务节点",
                    json!({"kind": n.kind, "owner": n.owner_name, "ref": ref_}),
                )
                .await;
                return Ok(ok(json!({"ok": true, "target": "node", "id": n.id})));
            }
        }
        return Err(ApiError::not_found("服务不存在"));
    }

    if let Some(row) = store::nodes::by_id(state.pool(), "", &ref_)
        .await
        .map_err(ApiError::from_db)?
    {
        store::admin::delete_hosted_node_as_admin(state.pool(), &row.id)
            .await
            .map_err(ApiError::from_db)?;
        audit(
            state,
            &me,
            headers,
            ACT_SERVICE_DELETE,
            &row.id,
            &row.name,
            "摘除服务节点",
            json!({"kind": row.kind}),
        )
        .await;
        return Ok(ok(json!({"ok": true, "target": "node", "id": row.id})));
    }
    Err(ApiError::not_found(
        "服务不存在（用 ND-… 节点 id 或 @命名空间/slug）",
    ))
}

/* ---------------- 审计与凭据 ---------------- */

/// GET /api/admin/audit?action=&limit=&offset=
async fn admin_list_audit(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    require_admin(&state, &auth, &headers).await?;
    let action = web::query(&uri, "action").unwrap_or_default();
    let (limit, offset) = page_params(&uri);
    let rows = store::admin::list_audit(state.pool(), &action, limit, offset)
        .await
        .map_err(ApiError::from_db)?;
    let total = store::admin::count_audit(state.pool(), &action)
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<Value> = rows.iter().map(audit_json).collect();
    Ok(ok(
        json!({"audit": list, "total": total, "limit": limit, "offset": offset}),
    ))
}

/// GET /api/admin/keys —— 机器管理凭据（只有前缀，secret 不可见）。
async fn admin_list_keys(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
) -> ApiResult<Response> {
    require_admin(&state, &auth, &headers).await?;
    let rows = store::admin::list_admin_keys(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<Value> = rows
        .iter()
        .map(|k| {
            json!({
                "id": k.id, "key": k.key, "label": k.label, "active": k.active(),
                "createdAt": k.created_at, "lastUsedAt": k.last_used_at, "revokedAt": k.revoked_at,
            })
        })
        .collect();
    Ok(ok(json!({"keys": list, "total": list.len()})))
}

/// POST /api/admin/keys/rotate —— 签发新的 admin key/secret 并撤销旧的。
///
/// 返回的 secret 只出现这一次；轮换后旧 secret 立即失效（`revoked_at` 一写就 `active=false`）。
async fn admin_rotate_key(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let me = require_admin(&state, &auth, &headers).await?;
    let label = {
        let l = web::query(&uri, "label").unwrap_or_default();
        if l.trim().is_empty() {
            "rotated".to_string()
        } else {
            l
        }
    };
    let (k, secret) = store::admin::create_admin_key(state.pool(), &label, &me.id)
        .await
        .map_err(ApiError::from_db)?;
    let revoked = store::admin::revoke_admin_keys(state.pool(), &k.id)
        .await
        .map_err(ApiError::from_db)?;
    audit(
        &state,
        &me,
        &headers,
        ACT_KEY_ROTATE,
        &k.key,
        &label,
        "轮换节点管理凭据",
        json!({"revoked": revoked, "by": me.name}),
    )
    .await;
    Ok(ok_status(
        StatusCode::CREATED,
        json!({
            "key": {"id": k.id, "key": k.key, "label": k.label, "createdAt": k.created_at},
            "secret": secret,
            "revoked": revoked,
            "howto": {
                "cli": format!("ncc registry admin login --key {} --secret <secret>", k.key),
                "note": "secret 只显示这一次；用上面的命令写进本机配置后即可管理本节点",
            },
        }),
    ))
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
            .join(format!("admin-{}-{name}", std::process::id()))
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

    /// 用 admin key/secret 造一个带凭据的请求。
    fn admin_req(method: &str, path: &str, key: &str, secret: &str) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if !key.is_empty() {
            b = b.header("x-ncc-admin-key", key);
            b = b.header("x-ncc-admin-secret", secret);
        }
        b.body(Body::empty()).unwrap()
    }

    fn json_admin_req(
        method: &str,
        path: &str,
        key: &str,
        secret: &str,
        body: Value,
    ) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if !key.is_empty() {
            b = b.header("x-ncc-admin-key", key);
            b = b.header("x-ncc-admin-secret", secret);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    async fn seed_admin_key(st: &AppState) -> (String, String) {
        let (k, s) = store::admin::create_admin_key(st.pool(), "测试", "U-1")
            .await
            .unwrap();
        (k.key, s)
    }

    #[tokio::test]
    async fn 门禁_三种凭据路径() {
        let st = state("gate").await;
        let app = router_for(&st);
        // 什么都没带 -> 403 admin_required
        let (code, v) = call(&app, admin_req("GET", "/admin/overview", "", "")).await;
        assert_eq!(code, SC::FORBIDDEN, "{v}");
        assert_eq!(v["error"]["code"], json!("admin_required"));

        // 只给 key 不给 secret -> 400
        let mut req = admin_req("GET", "/admin/overview", "AK-X", "");
        req.headers_mut().remove("x-ncc-admin-secret");
        let (code, v) = call(&app, req).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(
            v["error"]["message"],
            json!("admin key 与 secret 必须同时提供")
        );

        // 错的 secret -> 401
        let (key, secret) = seed_admin_key(&st).await;
        let (code, v) = call(&app, admin_req("GET", "/admin/keys", &key, "wrong")).await;
        assert_eq!(code, SC::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"]["code"], json!("admin_credentials_invalid"));

        // 正确凭据 -> 200，并回显 credential
        let (code, v) = call(&app, admin_req("GET", "/admin/overview", &key, &secret)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["credential"]["kind"], json!("admin_key"));
        assert_eq!(v["counts"]["users"], json!(0));
        assert_eq!(v["node"]["role"], json!(st.cfg().role));
    }

    #[tokio::test]
    async fn 轮换_旧_secret_立即失效_且写审计() {
        let st = state("rotate").await;
        let app = router_for(&st);
        let (key, secret) = seed_admin_key(&st).await;
        let (code, v) = call(
            &app,
            admin_req("POST", "/admin/keys/rotate?label=新", &key, &secret),
        )
        .await;
        assert_eq!(code, SC::CREATED, "{v}");
        assert_eq!(v["revoked"], json!(1));
        assert!(v["secret"].as_str().unwrap().len() >= 20);
        assert_eq!(v["key"]["label"], json!("新"));
        let new_key = v["key"]["key"].as_str().unwrap().to_string();
        let new_secret = v["secret"].as_str().unwrap().to_string();

        // 旧凭据立即失效
        let (code, _) = call(&app, admin_req("GET", "/admin/keys", &key, &secret)).await;
        assert_eq!(code, SC::UNAUTHORIZED);
        // 新凭据可用
        let (code, v) = call(&app, admin_req("GET", "/admin/keys", &new_key, &new_secret)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["total"], json!(2)); // 含已撤销的那份

        // 审计落了一条 rotate
        let (_, v) = call(
            &app,
            admin_req(
                "GET",
                "/admin/audit?action=admin.key.rotate",
                &new_key,
                &new_secret,
            ),
        )
        .await;
        assert_eq!(v["total"], json!(1));
        assert_eq!(v["audit"][0]["actor"]["kind"], json!("admin_key"));
        assert_eq!(v["audit"][0]["action"], json!("admin.key.rotate"));
    }

    #[tokio::test]
    async fn 用户列表_禁用_重置密码_与两条硬规则() {
        let st = state("users").await;
        let (key, secret) = seed_admin_key(&st).await;
        // 第一个注册用户是管理员
        let root = store::users::create(st.pool(), "管理员", "root@x.com", "h")
            .await
            .unwrap();
        let other = store::users::create(st.pool(), "甲", "jia@x.com", "h")
            .await
            .unwrap();
        let app = router_for(&st);

        let (code, v) = call(&app, admin_req("GET", "/admin/users", &key, &secret)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["total"], json!(2));
        assert_eq!(v["users"][0]["stats"]["nodes"], json!(0));

        // 禁用非管理员
        let (code, v) = call(
            &app,
            json_admin_req(
                "PATCH",
                &format!("/admin/users/{}", other.id),
                &key,
                &secret,
                json!({"disabled": true, "adminNote": "违规"}),
            ),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["user"]["disabled"], json!(true));
        assert_eq!(v["user"]["adminNote"], json!("违规"));

        // 不能禁用最后一个管理员
        let (code, v) = call(
            &app,
            json_admin_req(
                "PATCH",
                &format!("/admin/users/{}", root.id),
                &key,
                &secret,
                json!({"disabled": true}),
            ),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("last_admin"));

        // 重置密码：不给密码 -> 服务端生成
        let (code, v) = call(
            &app,
            json_admin_req(
                "POST",
                &format!("/admin/users/{}/password", other.id),
                &key,
                &secret,
                json!({}),
            ),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["generated"], json!(true));
        assert_eq!(v["password"].as_str().unwrap().len(), 12);

        // 太短 -> 400
        let (code, v) = call(
            &app,
            json_admin_req(
                "POST",
                &format!("/admin/users/{}/password", other.id),
                &key,
                &secret,
                json!({"password": "123"}),
            ),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("密码至少 6 位"));

        // 用户不存在 -> 404
        let (code, v) = call(
            &app,
            json_admin_req(
                "PATCH",
                "/admin/users/U-nope",
                &key,
                &secret,
                json!({"disabled": true}),
            ),
        )
        .await;
        assert_eq!(code, SC::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["message"], json!("用户不存在"));
    }

    #[tokio::test]
    async fn 会话管理员也能进_且不能禁自己() {
        let st = state("session").await;
        // 第一个注册用户 = 管理员
        let root = store::users::create(st.pool(), "管理员", "root@x.com", "h")
            .await
            .unwrap();
        let token = crate::jwt::sign_user(
            &st.cfg().jwt_secret,
            &root.id,
            &root.email,
            st.cfg().jwt_ttl,
        );
        let app = router_for(&st);

        // 会话管理员可以进管理面
        let req = Request::builder()
            .method("GET")
            .uri("/admin/overview")
            .header("authorization", auth_header(&token))
            .body(Body::empty())
            .unwrap();
        let (code, v) = call(&app, req).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["credential"]["kind"], json!("user"));

        // 不能禁用自己的账号
        let req = Request::builder()
            .method("PATCH")
            .uri(format!("/admin/users/{}", root.id))
            .header("content-type", "application/json")
            .header("authorization", auth_header(&token))
            .body(Body::from(json!({"disabled": true}).to_string()))
            .unwrap();
        let (code, v) = call(&app, req).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("不能禁用自己的账号"));

        // API-Key 不是会话，进不了管理面（即使属于管理员账号）
        let u2 = store::users::create(st.pool(), "乙", "yi@x.com", "h")
            .await
            .unwrap();
        let (_k, key_secret) = store::apikeys::create(st.pool(), &u2.id, "t", &[])
            .await
            .unwrap();
        let req = Request::builder()
            .method("GET")
            .uri("/admin/overview")
            .header("authorization", auth_header(&key_secret))
            .body(Body::empty())
            .unwrap();
        let (code, _) = call(&app, req).await;
        assert_eq!(code, SC::FORBIDDEN);
        let _ = key_secret;
    }

    #[tokio::test]
    async fn 节点与服务_列表_摘除_归档() {
        let st = state("nodes").await;
        let (key, secret) = seed_admin_key(&st).await;
        let u = store::users::create(st.pool(), "甲", "jia@x.com", "h")
            .await
            .unwrap();
        let ns = store::namespaces::create_account(st.pool(), &u.id, "甲", "jia")
            .await
            .unwrap();
        let (node, _) = store::nodes::upsert(
            st.pool(),
            &ns.id,
            &store::nodes::HeartbeatReq {
                slug: "svc".to_string(),
                name: "服务".to_string(),
                kind: "service".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let api = store::artifacts::create(
            st.pool(),
            store::artifacts::NewArtifact {
                namespace_id: ns.id.clone(),
                slug: "api1".to_string(),
                kind: "api".to_string(),
                name: "接口".to_string(),
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
                created_by: u.id.clone(),
            },
        )
        .await
        .unwrap();
        let app = router_for(&st);

        // 节点列表带 ownerEmail
        let (code, v) = call(&app, admin_req("GET", "/admin/nodes", &key, &secret)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["total"], json!(1));
        assert_eq!(v["nodes"][0]["ownerEmail"], json!("jia@x.com"));

        // 服务列表：节点侧 + 制品侧都看得到
        let (code, v) = call(&app, admin_req("GET", "/admin/services", &key, &secret)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["total"]["nodeServices"], json!(1));
        assert_eq!(v["total"]["apiArtifacts"], json!(1));
        assert_eq!(v["total"]["all"], json!(2));

        // 归档制品形服务
        let (code, v) = call(
            &app,
            admin_req("DELETE", "/admin/services/@jia/api1", &key, &secret),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["target"], json!("artifact"));
        assert_eq!(
            store::artifacts::by_id(st.pool(), &api.id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "archived"
        );

        // 摘除节点形服务（@ns/slug 会先当制品找，找不到再当节点）
        let (code, v) = call(
            &app,
            admin_req("DELETE", "/admin/services/@jia/svc", &key, &secret),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["target"], json!("node"));
        assert!(store::nodes::by_id(st.pool(), "", &node.id)
            .await
            .unwrap()
            .is_none());

        // 都不存在 -> 404
        let (code, v) = call(
            &app,
            admin_req("DELETE", "/admin/services/@jia/ghost", &key, &secret),
        )
        .await;
        assert_eq!(code, SC::NOT_FOUND, "{v}");
    }

    #[tokio::test]
    async fn 审计列表_与_动作过滤() {
        let st = state("audit").await;
        let (key, secret) = seed_admin_key(&st).await;
        // 先有一个管理员用户（第一个注册用户自动是管理员），再建要禁用的人
        store::users::create(st.pool(), "管理员", "root@x.com", "h")
            .await
            .unwrap();
        let other = store::users::create(st.pool(), "甲", "jia@x.com", "h")
            .await
            .unwrap();
        let app = router_for(&st);
        // 触发一次禁用：写审计
        let (code, _) = call(
            &app,
            json_admin_req(
                "PATCH",
                &format!("/admin/users/{}", other.id),
                &key,
                &secret,
                json!({"disabled": true}),
            ),
        )
        .await;
        assert_eq!(code, SC::OK);

        let (code, v) = call(&app, admin_req("GET", "/admin/audit", &key, &secret)).await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["total"], json!(1));
        assert_eq!(v["audit"][0]["action"], json!("user.disable"));
        assert_eq!(v["audit"][0]["targetName"], json!("jia@x.com"));
        assert_eq!(v["audit"][0]["detail"]["note"], json!(""));

        let (_, v) = call(
            &app,
            admin_req("GET", "/admin/audit?action=user.enable", &key, &secret),
        )
        .await;
        assert_eq!(v["total"], json!(0));
    }
}
