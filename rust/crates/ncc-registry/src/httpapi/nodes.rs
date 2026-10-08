//! 托管节点路由：列表 / 发现 / 心跳 / 连接 / 授权。
//!
//! 「在线」永远由**心跳时间 + TTL 现场算**（见 `store::nodes`），不落库 ——
//! 心跳是最高频写入，为它多写一次会把节点写死在磁盘 I/O 上。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::web;

use crate::httpapi::{AppState, Auth};
use crate::store;

/// `GET /api/nodes/route?ref=@命名空间/slug` —— 「这个能力该找谁」。
///
/// 原实现：`httpapi/cluster.go` 的 `clusterRoute`。master 先看自己有没有，再看哪个 worker
/// 上报过这条（`artifact_adverts`）；`preferred` 是给客户端的一个地址，字节可以由 master
/// 代理回来（`/api/registry/{ref}/bytes`），所以调用方不必自己去连 worker。
///
/// 注意这条路由**放在 `{id}` 之前**：axum 里静态段优先，但顺序无关紧要 —— 写前面只是让人
/// 一眼看到「route 不是节点 id」。
async fn route_ref(State(state): State<AppState>, uri: axum::http::Uri) -> ApiResult<Response> {
    let cfg = state.cfg();
    let ref_ = query_param(&uri, "ref").unwrap_or_default();
    if ref_.trim().is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "需要 ref（@命名空间/slug 或 A-… id）",
        ));
    }
    let ref_ = ref_.trim().to_string();
    let mut candidates: Vec<serde_json::Value> = Vec::new();

    // 自己（master）有：只有「已发布 + 公开」才算可路由
    if let Some(row) = store::artifacts::by_ref(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
    {
        if row.status == "published" && row.visibility == "public" {
            candidates.push(json!({
                "role": "self",
                "nodeId": cfg.node_id,
                "nodeName": cfg.node_name,
                "nodeUrl": cfg.public_url,
                "ref": format!("{}@{}", row.ref_of(), row.version),
                "sha256": row.sha256,
                "size": row.size,
                "download": format!("{}/api/registry/{}/bytes", cfg.public_url, row.ref_of()),
            }));
        }
    }

    // worker 上报的目录（兜底：worker 直接托管但 master 没有副本时也能路由到）
    let adverts = store::cluster::find_adverts_by_ref(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?;
    for a in &adverts {
        let url = a.worker_url.clone().unwrap_or_default();
        candidates.push(json!({
            "role": "worker",
            "nodeId": a.worker_id,
            "nodeName": a.worker_name.clone().unwrap_or_default(),
            "nodeUrl": url,
            "ref": a.ref_,
            "sha256": a.sha256,
            "size": a.size,
            "download": format!("{}/api/registry/{}/bytes", url.trim_end_matches('/'), a.ref_),
            // 在线由心跳时间现场算（与 Go 的 nodeOnline(a.SeenAt, NodeTTL*4) 同口径）
            "online": a.online(cfg.node_ttl * 4),
        }));
    }

    let resolved = !candidates.is_empty();
    // `preferred`：优先本节点，其次心跳最新的 worker（adverts 已按 seen_at 倒序）
    let preferred = candidates
        .iter()
        .find(|c| c["role"] == "self")
        .or_else(|| candidates.first())
        .cloned();
    let mut out = json!({
        "ref": ref_,
        "resolved": resolved,
        "candidates": candidates,
        "count": candidates.len(),
        "role": cfg.role,
    });
    if let Some(p) = preferred {
        out["preferred"] = p;
        out["download"] = json!(format!("{}/api/registry/{}/bytes", cfg.public_url, ref_));
    }
    Ok(crate::httpapi::helpers::ok_json(out))
}

/// 取一个查询参数（值做一次百分号解码）。
fn query_param(uri: &axum::http::Uri, key: &str) -> Option<String> {
    let q = uri.query()?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        (k == key).then(|| web::percent_decode(v))
    })
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/nodes", get(list_nodes))
        .route("/nodes/", get(list_nodes))
        .route("/nodes/kinds", get(node_kinds))
        .route("/nodes/offers", get(node_offers))
        .route("/nodes/discover", get(discover))
        .route("/nodes/regions", get(regions))
        .route("/nodes/heartbeat", post(heartbeat))
        .route("/nodes/links", post(link_node))
        .route("/nodes/links/{id}", patch(patch_link).delete(delete_link))
        .route("/nodes/route", get(route_ref))
        .route("/nodes/{id}", delete(delete_node))
        .route("/grants", get(list_grants).post(create_grant))
        .route("/grants/{id}", delete(delete_grant))
}

/// 节点视图（归一化后的能力 + 在线状态 + 我的连接）。
pub fn node_json(n: &store::nodes::NodeRow, ttl: std::time::Duration) -> serde_json::Value {
    let link = match n.link_id.as_deref() {
        Some(id) if !id.is_empty() => json!({
            "linked": true, "linkId": id, "label": n.link_label.clone().unwrap_or_default()
        }),
        _ => json!({"linked": false}),
    };
    let online = n.online(ttl);
    json!({
        "id": n.id,
        "slug": n.slug,
        "name": n.name,
        "kind": n.kind,
        "region": n.region,
        "url": n.url,
        "os": n.os,
        "arch": n.arch,
        "version": n.version,
        "agent": n.agent,
        "capabilities": n.caps(),
        "capabilitiesVerified": n.verified(),
        "visibility": n.visibility,
        "status": if online { "online" } else { "offline" },
        "online": online,
        "lastSeen": n.seen_at(),
        "namespace": {"slug": n.ns_slug, "name": n.ns_name},
        "owner": {"id": n.owner_id, "name": n.owner_name},
        "link": link,
    })
}

/// 取「我」（登录用户）能管理的命名空间 id 列表。
async fn my_ns_ids(state: &AppState, user_id: &str) -> Result<Vec<String>, ApiError> {
    Ok(store::namespaces::of_user(state.pool(), user_id)
        .await
        .map_err(ApiError::from_db)?
        .into_iter()
        .map(|n| n.id)
        .collect())
}

/// GET /api/nodes?mine=1 —— `mine=1` 是我命名空间下的节点，否则是我连接表里的节点。
async fn list_nodes(
    State(state): State<AppState>,
    auth: Auth,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let mine = web::query_bool(&uri, "mine");
    let ttl = state.cfg().node_ttl;
    let rows = if mine {
        let uid = auth
            .user_id()
            .ok_or_else(|| ApiError::unauthorized("mine=1 需要登录"))?;
        let ns_ids = my_ns_ids(&state, &uid).await?;
        store::nodes::list_of_namespaces(state.pool(), &uid, &ns_ids)
            .await
            .map_err(ApiError::from_db)?
    } else {
        let uid = auth.user_id().unwrap_or_default();
        store::nodes::list_linked(state.pool(), &uid)
            .await
            .map_err(ApiError::from_db)?
    };
    let items: Vec<_> = rows.iter().map(|n| node_json(n, ttl)).collect();
    Ok(ncc_core::error::ok(
        json!({"nodes": items, "total": items.len()}),
    ))
}

/// GET /api/nodes/kinds
async fn node_kinds(State(state): State<AppState>) -> ApiResult<Response> {
    let counts = store::nodes::kind_counts(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    let list = [
        ("service", "常驻服务", "长期在线的服务（模型网关、数据库代理…）"),
        ("agent", "Agent", "这台机器上可被调用的 Agent"),
        ("assigned", "被分配的 Agent", "别人指派到这台机器上跑的 Agent"),
    ]
    .iter()
    .map(|(k, label, desc)| {
        json!({"kind": k, "label": label, "desc": desc, "count": counts.get(*k).copied().unwrap_or(0)})
    })
    .collect::<Vec<_>>();
    Ok(ncc_core::error::ok(
        json!({"kinds": list, "total": list.len()}),
    ))
}

/// GET /api/nodes/offers —— 本实例上出现过的提供能力词表。
async fn node_offers(State(state): State<AppState>) -> ApiResult<Response> {
    let rows = store::nodes::list_public(state.pool(), "", &[], "", "", "", 200)
        .await
        .map_err(ApiError::from_db)?;
    let mut seen: Vec<String> = Vec::new();
    for r in &rows {
        for c in r.caps().into_iter().chain(r.verified()) {
            if !seen.contains(&c) {
                seen.push(c);
            }
        }
    }
    seen.sort();
    let verified: Vec<String> = seen.iter().filter(|c| c.contains(":")).cloned().collect();
    Ok(ncc_core::error::ok(json!({
        "offers": verified,
        "total": verified.len(),
    })))
}

/// GET /api/nodes/discover?kind&region&q&can&limit
async fn discover(
    State(state): State<AppState>,
    auth: Auth,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let uid = auth.user_id().unwrap_or_default();
    let granted = if uid.is_empty() {
        Vec::new()
    } else {
        store::grants::granted_owners(state.pool(), &uid, store::grants::KIND_NODE).await
    };
    let ttl = state.cfg().node_ttl;
    let rows = store::nodes::list_public(
        state.pool(),
        &uid,
        &granted,
        &web::query(&uri, "kind").unwrap_or_default(),
        &web::query(&uri, "region").unwrap_or_default(),
        &web::query(&uri, "q").unwrap_or_default(),
        web::query_i64(&uri, "limit", 50),
    )
    .await
    .map_err(ApiError::from_db)?;

    // 按能力筛选：`?can=serve:mcp` 看声明，`?can=run:remote@verified` 看自证。
    let mut wanted: Vec<(String, bool)> = Vec::new();
    for raw in web::query(&uri, "can")
        .into_iter()
        .flat_map(|s| web::split_csv(&s))
    {
        match raw.strip_suffix("@verified") {
            Some(c) => wanted.push((c.to_string(), true)),
            None => wanted.push((raw, false)),
        }
    }
    let hit = |r: &store::nodes::NodeRow| -> bool {
        wanted.iter().all(|(c, verified)| {
            if *verified {
                r.verified().iter().any(|x| x == c)
            } else {
                r.caps().iter().any(|x| x == c) || r.verified().iter().any(|x| x == c)
            }
        })
    };
    let items: Vec<_> = rows
        .iter()
        .filter(|r| hit(r))
        .map(|n| node_json(n, ttl))
        .collect();
    Ok(ncc_core::error::ok(
        json!({"nodes": items, "total": items.len()}),
    ))
}

/// GET /api/nodes/regions
async fn regions(State(state): State<AppState>) -> ApiResult<Response> {
    let list = store::nodes::regions(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok(
        json!({"regions": list, "total": list.len()}),
    ))
}

/// POST /api/nodes/heartbeat —— 与 `/api/namespaces/living` 是同一个动作。
async fn heartbeat(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<store::nodes::HeartbeatReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("nodes:write")?;
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
        json!({"node": node_json(&node, ttl), "created": created}),
    ))
}

#[derive(Debug, Deserialize, Default)]
struct LinkReq {
    #[serde(default, rename = "nodeId")]
    node_id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    note: String,
}

/// POST /api/nodes/links —— 连接一个节点（连接**不需要对方审批**，只代表「找得到」）。
async fn link_node(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<LinkReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("nodes:write")?;
    let node_id = body.node_id.trim();
    if node_id.is_empty() {
        return Err(ApiError::bad_request("bad_request", "nodeId 不能为空"));
    }
    let target = store::nodes::owner_of(state.pool(), node_id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("节点不存在"))?;
    let link = store::nodes::link(
        state.pool(),
        &a.user_id,
        node_id,
        &target,
        body.label.trim(),
        body.note.trim(),
    )
    .await
    .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok_status(
        StatusCode::CREATED,
        json!({
            "link": {
                "id": link.id, "nodeId": link.node_id, "targetUserId": link.target_user_id,
                "label": link.label, "note": link.note, "createdAt": link.created_at,
            }
        }),
    ))
}

#[derive(Debug, Deserialize, Default)]
struct PatchLinkReq {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

async fn patch_link(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    Json(body): Json<PatchLinkReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("nodes:write")?;
    let ok = store::nodes::patch_link(
        state.pool(),
        &id,
        &a.user_id,
        body.label.as_deref(),
        body.note.as_deref(),
    )
    .await
    .map_err(ApiError::from_db)?;
    if !ok {
        return Err(ApiError::not_found("连接不存在（或没有改动）"));
    }
    Ok(ncc_core::error::ok(json!({"ok": true})))
}

async fn delete_link(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = auth.require_scope("nodes:write")?;
    store::nodes::delete_link(state.pool(), &id, &a.user_id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok(json!({"ok": true})))
}

/// DELETE /api/nodes/{id} —— 注销自己命名空间下的节点。
async fn delete_node(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = auth.require_scope("nodes:write")?;
    // 先取节点再判归属：先判归属要两次查询，而且拿不到「节点不存在」与「不是你的」
    // 的区别 —— 这两种错对调用方是完全不同的两件事。
    let node = store::nodes::by_id(state.pool(), &a.user_id, &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("节点不存在"))?;
    if !crate::httpapi::artifacts::can_manage(&state, &node.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只能注销自己命名空间下的节点"));
    }
    store::nodes::delete(state.pool(), &id, &node.namespace_id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok(json!({"ok": true, "id": id})))
}

/* ---------------- 授权 ---------------- */

/// GET /api/grants —— 我发出的 + 我收到的。
async fn list_grants(State(state): State<AppState>, auth: Auth) -> ApiResult<Response> {
    let a = auth.require_scope("grants:read")?;
    let owned = store::grants::list_owned(state.pool(), &a.user_id)
        .await
        .map_err(ApiError::from_db)?;
    let received = store::grants::list_received(state.pool(), &a.user_id)
        .await
        .map_err(ApiError::from_db)?;
    let view = |g: &store::grants::Grant| {
        json!({
            "id": g.id, "ownerId": g.owner_id, "granteeUserId": g.grantee_user_id,
            "kind": g.kind, "namespaceId": g.namespace_id, "note": g.note,
            "createdAt": g.created_at,
        })
    };
    Ok(ncc_core::error::ok(json!({
        "granted": owned.iter().map(view).collect::<Vec<_>>(),
        "received": received.iter().map(view).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize, Default)]
struct CreateGrantReq {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    email: String,
    #[serde(default, rename = "granteeUserId")]
    grantee_user_id: String,
    #[serde(default, rename = "namespaceId")]
    namespace_id: String,
    #[serde(default)]
    note: String,
}

async fn create_grant(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateGrantReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("grants:write")?;
    let kind = body.kind.trim();
    if !store::grants::valid_kind(kind) {
        return Err(ApiError::bad_request(
            "bad_request",
            "kind 只能是 artifact / node / config / p2p",
        ));
    }
    let grantee = if !body.grantee_user_id.trim().is_empty() {
        store::users::by_id(state.pool(), body.grantee_user_id.trim())
            .await
            .map_err(ApiError::from_db)?
    } else {
        store::users::by_email(state.pool(), body.email.trim())
            .await
            .map_err(ApiError::from_db)?
    }
    .ok_or_else(|| ApiError::not_found("被授权用户不存在"))?;
    if grantee.id == a.user_id {
        return Err(ApiError::bad_request("bad_request", "不需要给自己授权"));
    }

    // 命名空间授权要先证明你是那个命名空间的人
    let ns_id = body.namespace_id.trim();
    if !ns_id.is_empty() && !crate::httpapi::artifacts::can_manage(&state, ns_id, &a.user_id).await
    {
        return Err(ApiError::forbidden("你不是该 namespace 的 owner/成员"));
    }

    let g = store::grants::create(
        state.pool(),
        &a.user_id,
        &grantee.id,
        kind,
        ns_id,
        body.note.trim(),
    )
    .await
    .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok_status(
        StatusCode::CREATED,
        json!({
            "grant": {
                "id": g.id, "kind": g.kind, "granteeUserId": g.grantee_user_id,
                "granteeEmail": grantee.email, "namespaceId": g.namespace_id, "note": g.note,
            }
        }),
    ))
}

async fn delete_grant(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = auth.require_scope("grants:write")?;
    store::grants::delete(state.pool(), &id, &a.user_id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok(json!({"ok": true})))
}
