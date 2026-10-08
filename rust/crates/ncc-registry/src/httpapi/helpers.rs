//! 处理器共用的小工具。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;
use std::sync::OnceLock;

use ncc_core::error::ApiError;
use ncc_core::scope::AuthInfo;

use crate::config::Config;
use crate::store;

/// `200 {"...": ...}`。
pub fn ok_json(body: Value) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

/// 指定状态码的成功响应（注册/发布回 201 等）。
pub fn ok_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// 校验邮箱形态。
pub fn valid_email(s: &str) -> bool {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").expect("静态正则合法")
    });
    re.is_match(s.trim())
}

/// 要求调用方是本节点管理员。
pub async fn require_admin(
    state: &super::AppState,
    auth: &AuthInfo,
) -> Result<(), ApiError> {
    let u = store::users::by_id(state.pool(), &auth.user_id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::unauthorized("需要登录"))?;
    if !u.is_admin {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    Ok(())
}

/// 是否有权管理某个命名空间（owner 或成员）。
pub async fn can_manage(state: &super::AppState, ns_id: &str, user_id: &str) -> bool {
    if store::namespaces::is_owner(state.pool(), ns_id, user_id).await {
        return true;
    }
    store::namespaces::is_member(state.pool(), ns_id, user_id).await
}

/// 注册门禁：留空 = 内网开放注册（默认）；设了值 = 必须带邀请码。
pub fn invite_gate(cfg: &Config, code: &str) -> Result<(), ApiError> {
    if cfg.invite_allows(code) {
        Ok(())
    } else {
        Err(ApiError::forbidden("邀请码不正确（本节点开启了注册门禁）"))
    }
}

/// 本节点规模指标（`/api/meta`、`/api/health`、集群上报都用它）。
pub async fn counts(state: &super::AppState) -> (i64, i64, i64) {
    let artifacts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
        .fetch_one(state.pool())
        .await
        .unwrap_or(0);
    let nodes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM hosted_nodes")
        .fetch_one(state.pool())
        .await
        .unwrap_or(0);
    let users = store::users::count(state.pool()).await.unwrap_or(0);
    (artifacts, nodes, users)
}
