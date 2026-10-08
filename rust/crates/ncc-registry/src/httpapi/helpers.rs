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

/// `/x/{id}` 与 `/x/{id}/{slug}` 共用同一个 handler 时的路由参数。
///
/// ⚠️ **不要改回 `Path<(String, Option<String>)>`**：axum 对元组取参要求元素个数与
/// 路由参数**完全相等**（`deserialize_tuple` 里先判 `url_params.len() != len`），
/// 于是单段路由 `/x/{id}` 上会直接被判「参数个数不对」而失败 —— 而这个错**只在真实
/// 请求里出现**，直接调 handler 的单测（自己传 `Path((..))`）永远抓不到。
/// 命名结构体走的是 map 反序列化，缺的字段由 `#[serde(default)]` 补成 `None`，
/// 一段、两段两种路由都能用。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IdSlug {
    /// 单段引用（`C-…` / `KD-…` / `AR-…`）或两段引用里的第一段（`@命名空间`）。
    pub id: String,
    /// 两段引用的第二段（slug）；单段路由上没有这个参数。
    #[serde(default)]
    pub slug: Option<String>,
}

/// `200 {"...": ...}`。
pub fn ok_json(body: Value) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

/// 指定状态码的成功响应（注册/发布回 201 等）。
pub fn ok_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// `null` / 缺省 → 空串（给 `Json<T>` 请求体的 `String` 字段用）。
///
/// Go 的 `encoding/json` 把 `null` 塞进 `string` 字段就是零值 `""`，serde 却会直接报
/// 「expected a string」→ 处理器把它翻成 400。而 CLI 的可选参数是按 `Option<String>`
/// 序列化的（`ncc register --email …` 不带 `--name` 就发 `null`），
/// 于是「没写名字」这种最常见的情况会变成 400 —— 特别难查。
pub fn de_str<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde::Deserialize as _;
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// `null` / 缺省 → 该类型的零值（`Vec`、数字、`bool` 等），与 [`de_str`] 同因。
pub fn de_or_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de> + Default,
{
    use serde::Deserialize as _;
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// 校验邮箱形态。
pub fn valid_email(s: &str) -> bool {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re =
        RE.get_or_init(|| regex::Regex::new(r"^[^\s@]+@[^\s@]+\.[^\s@]+$").expect("静态正则合法"));
    re.is_match(s.trim())
}

/// 要求调用方是本节点管理员。
pub async fn require_admin(state: &super::AppState, auth: &AuthInfo) -> Result<(), ApiError> {
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
