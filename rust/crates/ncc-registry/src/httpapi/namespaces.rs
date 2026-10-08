//! 命名空间：我的 / 建组织 / 我的节点。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use ncc_core::error::{ApiError, ApiResult};

use crate::httpapi::{AppState, Auth};
use crate::store;

#[derive(Debug, Deserialize, Default)]
struct CreateNsReq {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    name: String,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/namespaces/mine", get(mine))
        .route("/namespaces", post(create))
        .route("/namespaces/", post(create))
        // 节点的注册与心跳合并：`living` 就是「我这台机器上的东西」
        .route("/namespaces/living", get(my_nodes).post(heartbeat))
}

fn ns_view(n: &store::namespaces::Namespace, owner: bool) -> serde_json::Value {
    json!({
        "id": n.id, "slug": n.slug, "name": n.name, "type": n.ns_type,
        "visibility": n.visibility, "owner": owner, "createdAt": n.created_at,
    })
}

/// GET /api/namespaces/mine
async fn mine(State(state): State<AppState>, auth: Auth) -> ApiResult<Response> {
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("需要登录"))?;
    let nss = store::namespaces::of_user(state.pool(), &a.user_id)
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<_> = nss.iter().map(|n| ns_view(n, n.owner_id == a.user_id)).collect();
    Ok(ncc_core::error::ok(json!({"namespaces": list})))
}

/// POST /api/namespaces —— 建组织命名空间（内网版不收钱）。
async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateNsReq>,
) -> ApiResult<Response> {
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("需要登录"))?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("bad_request", "名称不能为空"));
    }
    if body.slug.trim().is_empty() {
        return Err(ApiError::bad_request("bad_request", "slug 不能为空"));
    }
    let ns = store::namespaces::create_org(state.pool(), &a.user_id, body.slug.trim(), name)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| {
            ApiError::conflict(
                "conflict",
                "slug 不合法或已被占用（须为 2..64 位小写字母数字与 - _ .）",
            )
        })?;
    Ok(ncc_core::error::ok_status(
        StatusCode::CREATED,
        json!({"namespace": ns_view(&ns, true)}),
    ))
}

/// GET /api/namespaces/living —— 我（或我的组织）注册在这台节点上的节点。
async fn my_nodes(State(state): State<AppState>, auth: Auth) -> ApiResult<Response> {
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("需要登录"))?;
    let ttl = state.cfg().node_ttl;
    let items: Vec<_> = store::nodes::list_of_user(state.pool(), &a.user_id, ttl)
        .await
        .map_err(ApiError::from_db)?
        .iter()
        .map(|n| crate::httpapi::nodes::node_json(n, ttl))
        .collect();
    Ok(ncc_core::error::ok(
        json!({"nodes": items, "total": items.len()}),
    ))
}

/// POST /api/namespaces/living —— 注册/心跳（同一个端点是刻意的：
/// 「注册」与「还活着」对调用方是同一件事，分开只会让客户端每次都要选一个）。
async fn heartbeat(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<store::nodes::HeartbeatReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("nodes:write")?;
    // 归属命名空间：留空用个人命名空间（与 `/api/nodes/heartbeat` 同一套规则）
    let ns_id = if body.namespace_id.trim().is_empty() {
        store::namespaces::personal(state.pool(), &a.user_id)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| ApiError::bad_request("bad_request", "没有个人命名空间，请重新注册"))?
            .id
    } else {
        let ns = store::namespaces::by_id(state.pool(), body.namespace_id.trim())
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| ApiError::bad_request("bad_request", "namespace 不存在"))?;
        if !crate::httpapi::artifacts::can_manage(&state, &ns.id, &a.user_id).await {
            return Err(ApiError::forbidden("你不是该 namespace 的 owner/成员"));
        }
        ns.id
    };
    let ttl = state.cfg().node_ttl;
    let (node, created) = store::nodes::upsert(state.pool(), &ns_id, &body)
        .await
        .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok_status(
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        json!({"node": crate::httpapi::nodes::node_json(&node, ttl), "created": created}),
    ))
}
