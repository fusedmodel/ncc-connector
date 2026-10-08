//! 节点账号面：注册 / 登录 / 我 / API-Key。
//!
//! 与平台账号面的关键差别：**本节点的第一个注册账号自动成为管理员**
//! （内网托管节点的部署形态是「谁先装谁是主人」），注册还要过 `NCCR_INVITE_CODE` 门禁。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use ncc_core::crypto::{hash_password, verify_password};
use ncc_core::error::{ApiError, ApiResult};
use ncc_core::scope;

use crate::httpapi::{helpers, AppState, Auth};
use crate::store;

#[derive(Debug, Deserialize, Default)]
struct CredReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    email: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    password: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    name: String,
    #[serde(
        default,
        rename = "inviteCode",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    invite_code: String,
}

#[derive(Debug, Deserialize, Default)]
struct PatchMeReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    name: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    password: String,
    #[serde(
        default,
        rename = "newPassword",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    new_password: String,
}

#[derive(Debug, Deserialize, Default)]
struct CreateKeyReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    label: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    scopes: Vec<String>,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/meta", get(auth_meta))
        .route("/auth/register", post(register))
        .route("/auth/login", post(login))
        .route("/auth/me", get(me).patch(patch_me))
        .route("/auth/keys", get(list_keys).post(create_key))
        .route("/auth/keys/{id}", delete(delete_key))
        .route("/auth/key-scopes", get(key_scopes))
}

/// GET /api/auth/meta —— 公开的注册前置信息。
async fn auth_meta(State(state): State<AppState>) -> ApiResult<Response> {
    let cfg = state.cfg();
    Ok(helpers::ok_json(json!({
        "inviteRequired": cfg.invite_required(),
        "node": {
            "id": cfg.node_id, "name": cfg.node_name, "role": cfg.role,
            "url": cfg.public_url, "region": cfg.node_region,
        },
    })))
}

/// POST /api/auth/register —— 注册即开通个人命名空间。
async fn register(State(state): State<AppState>, Json(body): Json<CredReq>) -> ApiResult<Response> {
    let code = body.invite_code.trim();
    if !state.cfg().invite_allows(code) {
        return Err(if code.is_empty() {
            ApiError::new(
                StatusCode::FORBIDDEN,
                "invite_required",
                "本节点需要邀请码才能注册",
            )
        } else {
            ApiError::new(StatusCode::FORBIDDEN, "invite_invalid", "邀请码不正确")
        });
    }

    let email = body.email.trim().to_lowercase();
    if !helpers::valid_email(&email) {
        return Err(ApiError::bad_request("bad_request", "邮箱格式不正确"));
    }
    if body.password.len() < 6 {
        return Err(ApiError::bad_request("bad_request", "密码至少 6 位"));
    }
    if store::users::email_taken(state.pool(), &email)
        .await
        .map_err(ApiError::from_db)?
    {
        return Err(ApiError::conflict("conflict", "该邮箱已注册"));
    }

    let mut name = body.name.trim().to_string();
    if name.is_empty() {
        name = email.split('@').next().unwrap_or("").to_string();
    }
    // 按**字符**截断（Go 用 []rune）：按字节截会切碎中文名
    if name.chars().count() > 40 {
        name = name.chars().take(40).collect();
    }

    let hash = hash_password(&body.password)
        .map_err(|e| ApiError::internal(format!("密码处理失败: {e}")))?;
    let u = store::users::create(state.pool(), &name, &email, &hash)
        .await
        .map_err(|e| {
            if e.to_string().to_lowercase().contains("unique") {
                ApiError::conflict("conflict", "该邮箱已注册")
            } else {
                ApiError::from_db(e)
            }
        })?;
    store::namespaces::create_account(
        state.pool(),
        &u.id,
        &u.name,
        email.split('@').next().unwrap_or(""),
    )
    .await
    .map_err(ApiError::from_db)?;

    let token = crate::jwt::sign_user(
        &state.cfg().jwt_secret,
        &u.id,
        &u.email,
        state.cfg().jwt_ttl,
    );
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({"token": token, "user": user_view(&u)}),
    ))
}

/// POST /api/auth/login
async fn login(State(state): State<AppState>, Json(body): Json<CredReq>) -> ApiResult<Response> {
    let email = body.email.trim().to_lowercase();
    let Some(u) = store::users::by_email(state.pool(), &email)
        .await
        .map_err(ApiError::from_db)?
    else {
        return Err(ApiError::unauthorized("邮箱或密码不正确"));
    };
    if !verify_password(&body.password, &u.pass_hash) {
        return Err(ApiError::unauthorized("邮箱或密码不正确"));
    }
    if u.disabled {
        let note = if u.admin_note.trim().is_empty() {
            String::new()
        } else {
            format!("（原因：{}）", u.admin_note.trim())
        };
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "account_disabled",
            format!("账号已被禁用，请联系管理员{note}"),
        ));
    }
    let _ = store::users::touch_login(state.pool(), &u.id).await;
    let token = crate::jwt::sign_user(
        &state.cfg().jwt_secret,
        &u.id,
        &u.email,
        state.cfg().jwt_ttl,
    );
    Ok(helpers::ok_json(
        json!({"token": token, "user": user_view(&u)}),
    ))
}

fn user_view(u: &store::users::User) -> serde_json::Value {
    json!({
        "id": u.id, "email": u.email, "name": u.name, "plan": u.plan,
        "createdAt": u.created_at, "isAdmin": u.is_admin, "disabled": u.disabled,
    })
}

/// GET /api/auth/me —— 当前身份 + 命名空间 + 凭据能力（CLI 据此判断能做什么）。
async fn me(State(state): State<AppState>, auth: Auth) -> ApiResult<Response> {
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("需要登录"))?;
    let cfg = state.cfg();
    let node = json!({
        "id": cfg.node_id, "name": cfg.node_name, "role": cfg.role,
        "url": cfg.public_url, "region": cfg.node_region, "version": ncc_core::REGISTRY_VERSION,
    });
    let u = store::users::by_id(state.pool(), &a.user_id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;

    // 节点令牌（票据兑换来的）代表机器，不代表某个人的账号：只回身份与作用域。
    if a.kind == "node" {
        return Ok(helpers::ok_json(json!({
            "user": {"id": u.id, "name": u.name},
            "namespaces": [],
            "credential": {"kind": "node", "scopes": a.scopes, "session": false, "nodeId": a.node_id},
            "node": node,
        })));
    }

    let nss = store::namespaces::of_user(state.pool(), &u.id)
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<_> = nss
        .iter()
        .map(|n| {
            json!({
                "id": n.id, "slug": n.slug, "name": n.name, "type": n.ns_type,
                "visibility": n.visibility, "owner": n.owner_id == u.id, "createdAt": n.created_at,
            })
        })
        .collect();
    Ok(helpers::ok_json(json!({
        "user": user_view(&u),
        "namespaces": list,
        "credential": {"kind": a.kind, "scopes": a.scopes, "session": a.session},
        "admin": {"isAdmin": u.is_admin},
        "node": node,
    })))
}

/// PATCH /api/auth/me —— 改名 / 改密
async fn patch_me(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<PatchMeReq>,
) -> ApiResult<Response> {
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("需要登录"))?;
    // 改密必须来自用户会话：一把受限的 API-Key 不该能改掉主人的口令。
    let u = store::users::by_id(state.pool(), &a.user_id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;

    let name = body.name.trim();
    if !name.is_empty() && name != u.name {
        store::users::update_name(state.pool(), &u.id, name)
            .await
            .map_err(ApiError::from_db)?;
    }
    if !body.new_password.is_empty() {
        if !a.session {
            return Err(ApiError::forbidden(
                "改密码需要用户会话（API-Key 不可代改）",
            ));
        }
        if !verify_password(&body.password, &u.pass_hash) {
            return Err(ApiError::unauthorized("原密码不正确"));
        }
        if body.new_password.len() < 6 {
            return Err(ApiError::bad_request("bad_request", "新密码至少 6 位"));
        }
        let h = hash_password(&body.new_password)
            .map_err(|e| ApiError::internal(format!("密码处理失败: {e}")))?;
        store::users::update_pass(state.pool(), &u.id, &h)
            .await
            .map_err(ApiError::from_db)?;
    }

    let fresh = store::users::by_id(state.pool(), &u.id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("用户不存在"))?;
    Ok(helpers::ok_json(json!({"user": user_view(&fresh)})))
}

/// GET /api/auth/keys
async fn list_keys(State(state): State<AppState>, auth: Auth) -> ApiResult<Response> {
    // 列表只要登录：看自己有哪些 key 不该被作用域卡住；
    // 签发/吊销才要 keys:write（那是一把 key 能不能再生出更多 key 的边界）。
    let a = auth.require_login()?;
    let list = store::apikeys::list(state.pool(), &a.user_id)
        .await
        .map_err(ApiError::from_db)?;
    let items: Vec<_> = list
        .iter()
        .map(|k| {
            json!({
                "id": k.id, "label": k.label, "prefix": k.prefix,
                "scopes": store::parse_list(&k.scopes),
                "createdAt": k.created_at, "lastUsedAt": k.last_used_at,
            })
        })
        .collect();
    Ok(helpers::ok_json(
        json!({"keys": items, "total": items.len()}),
    ))
}

/// POST /api/auth/keys —— 需要 `keys:write`
async fn create_key(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateKeyReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("keys:write")?;
    let scopes: Vec<String> = if body.scopes.is_empty() {
        scope::DEFAULT_SCOPES
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        for s in &body.scopes {
            if !scope::valid_scope(s) {
                return Err(ApiError::bad_request(
                    "bad_request",
                    format!("未知作用域 {s}"),
                ));
            }
        }
        body.scopes.clone()
    };
    let (k, secret) = store::apikeys::create(state.pool(), &a.user_id, body.label.trim(), &scopes)
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({"key": secret, "id": k.id, "label": k.label, "prefix": k.prefix, "scopes": scopes}),
    ))
}

/// DELETE /api/auth/keys/{id}
async fn delete_key(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = auth.require_scope("keys:write")?;
    store::apikeys::delete(state.pool(), &id, &a.user_id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_json(json!({"ok": true})))
}

/// GET /api/auth/key-scopes
async fn key_scopes() -> ApiResult<Response> {
    let all: Vec<_> = scope::ALL_SCOPES
        .iter()
        .map(|s| {
            json!({
                "scope": s,
                "desc": scope::scope_desc(s),
                "default": scope::DEFAULT_SCOPES.contains(s),
            })
        })
        .collect();
    Ok(helpers::ok_json(json!({
        "scopes": all,
        "total": all.len(),
        "defaults": scope::DEFAULT_SCOPES,
    })))
}
