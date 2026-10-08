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
use serde_json::{json, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::web;

use crate::httpapi::{helpers, AppState, Auth};
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
/// GET /api/nodes —— 「我的节点」+「我连接的节点」，形状照 Go：
/// `{"nodes":<我的>, "linked":<我连接的>, "links":<同一份>, "mine":N, "total":N+M, "online":K, "ttlSec":T}`。
///
/// ⚠️ 三处容易写歪、写歪了客户端就读不到东西：
/// * **默认（没有查询串）就要给 `nodes`**：它是我自己那些节点，不是「只有 `?mine=1` 才给」。
///   只回 `linked` 的话，`ncc nodes` 这类客户端第一眼就是空列表。
/// * **同时给 `linked` 与 `links`**：Go（以及平台侧）在这件事上分成了两个名字 —— 这边照 Go 是
///   `linked`，平台与 `ncc nodes list` 读的是 `links`。只给一个，同一个命令打过来就是"连了却显示 0 条"。
///   两份指向同一个数组（值语义，不是引用别名），谁都不必改。
/// * `?can=` 是「全都要」的过滤，`<id>@verified` 只认**自证**（与 `/discover` 同一套语义）。
async fn list_nodes(
    State(state): State<AppState>,
    auth: Auth,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let ttl = state.cfg().node_ttl;
    let uid = auth.user_id().unwrap_or_default();
    let wanted = offer_query(&uri);
    let hit = |r: &store::nodes::NodeRow| -> bool {
        wanted.iter().all(|(c, verified)| {
            if *verified {
                r.verified().iter().any(|x| x == c)
            } else {
                r.caps().iter().any(|x| x == c) || r.verified().iter().any(|x| x == c)
            }
        })
    };

    let mut mine: Vec<Value> = Vec::new();
    if !uid.is_empty() {
        let ns_ids = my_ns_ids(&state, &uid).await?;
        let rows = store::nodes::list_of_namespaces(state.pool(), &uid, &ns_ids)
            .await
            .map_err(ApiError::from_db)?;
        mine = rows
            .iter()
            .filter(|r| hit(r))
            .map(|n| node_json(n, ttl))
            .collect();
    }
    let linked: Vec<Value> = store::nodes::list_linked(state.pool(), &uid)
        .await
        .map_err(ApiError::from_db)?
        .iter()
        .filter(|r| hit(r))
        .map(|n| node_json(n, ttl))
        .collect();

    let online = mine
        .iter()
        .chain(linked.iter())
        .filter(|n| n["online"] == Value::Bool(true))
        .count();
    Ok(ncc_core::error::ok(json!({
        "nodes": mine,
        "linked": linked,
        // 平台侧与 CLI 读的键名（见函数头第 2 条）：同一份数据的第二个名字，不删 `linked`
        "links": linked,
        "mine": mine.len(),
        "total": mine.len() + linked.len(),
        "online": online,
        "ttlSec": ttl.as_secs() as i64,
    })))
}

/// 解析 `?can=`（可重复传、也可逗号分隔）：`<id>` 看声明或自证，`<id>@verified` 只认自证。
fn offer_query(uri: &axum::http::Uri) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for raw in web::query(uri, "can")
        .into_iter()
        .flat_map(|s| web::split_csv(&s))
    {
        let (id, verified) = match raw.strip_suffix("@verified") {
            Some(c) => (c.to_string(), true),
            None => (raw, false),
        };
        if id.is_empty() || out.contains(&(id.clone(), verified)) {
            continue;
        }
        out.push((id, verified));
    }
    out
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
    let wanted = offer_query(&uri);
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

/// GET /api/nodes/regions —— 区域覆盖（Agent 面：按区域找节点）。
///
/// 形状照 Go：`{"regions":[{region,total,online}], "ttlSec":N}`。
/// `ttlSec` 必须给：调用方要拿它判断「多久没心跳算掉线」，光给计数的话它只能自己猜。
async fn regions(State(state): State<AppState>) -> ApiResult<Response> {
    let ttl = store::nodes::ttl_of(state.cfg());
    let list = store::nodes::regions(state.pool(), ttl)
        .await
        .map_err(ApiError::from_db)?;
    let items: Vec<Value> = list
        .iter()
        .map(|r| json!({"region": r.region, "total": r.total, "online": r.online}))
        .collect();
    Ok(ncc_core::error::ok(json!({
        "regions": items,
        "ttlSec": ttl.as_secs() as i64,
    })))
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

/// POST /api/nodes/links 的请求体。
///
/// ⚠️ 字段**必须**是 `node`（不是 `nodeId`）：Go 的 tag 就是 `node`，CLI 与冒烟发的都是
/// `{"node":"@ns/slug"}` 或 `{"node":"ND-…"}`，而且它接受**节点引用**（`@命名空间/slug`）
/// 而不只是内部 id —— 只读 id 会让「按引用连接」这条最常用路径 400。
#[derive(Debug, Deserialize, Default)]
struct LinkReq {
    /// 指人的那个「节点引用」（`@命名空间/slug` 或 `ND-…`）。
    ///
    /// 三个名字都要收：Go 用 `node`，平台侧与 CLI 用 `ref`，早先的脚本用过 `nodeId` ——
    /// 而且 CLI 的 `agent add` 会**同时**发 `ref` 与 `node`（跨两端对冲字段名）。
    /// 所以这里用**三个独立字段 + 取第一个非空**，而不是 `alias`：
    /// `alias` 会让同一次请求里出现两个键时直接 `duplicate field` 报 422。
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    node: String,
    // ⚠️ 必须 `rename = "ref"`：字段名带下划线只是为了避开 Rust 关键字，JSON 那边的键是 `ref`
    // （CLI 发的就是它）。少了这一条，serde 会去读 `ref_`，请求里那个键就悄悄丢了 ——
    // 表现是"同一个命令打过来 404"，而脚本里 201 的那条（用 nodeId）却能过。
    #[serde(
        default,
        rename = "ref",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    ref_: String,
    #[serde(
        default,
        rename = "nodeId",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    node_id: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    label: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    note: String,
}

/// POST /api/nodes/links —— 连接一个节点（连接**不需要对方审批**，只代表「找得到」）。
///
/// 形状照 Go：已存在的那条是**改备注**（200 + `created:false`），新的是 201 + `created:true`；
/// 连接自己的节点 400，连不看不见的私有节点 403 `grant_required`（授权是**另一件事**，见 grants）。
async fn link_node(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<LinkReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("nodes:write")?;
    let node_ref = [&body.node, &body.ref_, &body.node_id]
        .into_iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let target = resolve_node_ref(&state, node_ref).await?;
    if target.owner_id.as_deref() == Some(a.user_id.as_str()) {
        return Err(ApiError::bad_request(
            "bad_request",
            "这是你自己的节点，不需要连接",
        ));
    }
    if !can_see_node(&state, &target, &a.user_id).await {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "grant_required",
            "该节点不是公开节点；需要对方用 ncc grant set --user @你 --kind node 授权",
        ));
    }
    let label = body.label.trim();
    let note = body.note.trim();
    if let Some(existing) = store::nodes::find_link(state.pool(), &a.user_id, &target.id)
        .await
        .map_err(ApiError::from_db)?
    {
        store::nodes::patch_link(
            state.pool(),
            &existing.id,
            &a.user_id,
            Some(label),
            Some(note),
        )
        .await
        .map_err(ApiError::from_db)?;
        return Ok(helpers::ok_json(json!({
            "link": {"id": existing.id, "nodeId": target.id, "label": label, "note": note},
            "created": false,
            // 下面是 CLI（`ncc nodes link`）读的字段：它与平台侧的 /api/nodes/links 同形，
            // 节点这边同时给两份，是为了让同一个命令在两个目标上都打印得出东西。
            "linkId": existing.id,
            "nodeId": target.id,
            "label": label,
            "note": note,
            "node": link_node_view(&target, label),
        })));
    }
    let Some(owner_id) = target.owner_id.clone() else {
        return Err(ApiError::not_found("节点不存在"));
    };
    let link = store::nodes::link(state.pool(), &a.user_id, &target.id, &owner_id, label, note)
        .await
        .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok_status(
        StatusCode::CREATED,
        json!({
            "link": {"id": link.id, "nodeId": link.node_id, "label": link.label, "note": link.note},
            "created": true,
            "linkId": link.id,
            "nodeId": target.id,
            "label": link.label,
            "note": link.note,
            "node": link_node_view(&target, &link.label),
        }),
    ))
}

/// 连接成功后回给客户端的节点简介（CLI 读 `node.label` / `node.kind`）。
fn link_node_view(n: &NodeRef, label: &str) -> Value {
    json!({
        "id": n.id,
        "slug": n.slug,
        "name": n.name,
        "kind": n.kind,
        "label": label,
        "linked": true,
    })
}

/// 连接要指向的节点（`@命名空间/slug` 或 `ND-…`）：只取判定要用的几列。
struct NodeRef {
    id: String,
    namespace_id: String,
    owner_id: Option<String>,
    visibility: String,
    slug: String,
    name: String,
    kind: String,
}

/// 解析节点引用（Go 的 `resolveNode`）：`@ns/slug` 走命名空间 + slug 查，其余按 id 查。
///
/// 与 `agentcards.rs` 里那份解析是同形实现 —— 两份都只取 id/归属/可见性三样，且都要求
/// 引用非法时回 `404 节点不存在`（文案与 Go 一致）。合并留作后续清理，不在这轮动别人正在改的文件。
async fn resolve_node_ref(state: &AppState, r: &str) -> ApiResult<NodeRef> {
    let r = r.trim();
    if r.is_empty() {
        return Err(ApiError::not_found("节点不存在"));
    }
    if let Some(body) = r.strip_prefix('@') {
        let Some((ns, slug)) = body.split_once('/') else {
            return Err(ApiError::not_found("节点不存在"));
        };
        if ns.is_empty() || slug.is_empty() {
            return Err(ApiError::not_found("节点不存在"));
        }
        let row = fetch_node_ref(
            state,
            "SELECT hosted_nodes.id, hosted_nodes.namespace_id, namespaces.owner_id, hosted_nodes.visibility, \
                    hosted_nodes.slug, hosted_nodes.name, hosted_nodes.kind \
               FROM hosted_nodes JOIN namespaces ON namespaces.id = hosted_nodes.namespace_id \
              WHERE namespaces.slug = ? AND hosted_nodes.slug = ? LIMIT 1",
            &[ns, slug],
        )
        .await?;
        return row.ok_or_else(|| ApiError::not_found("节点不存在"));
    }
    fetch_node_ref(
        state,
        "SELECT hosted_nodes.id, hosted_nodes.namespace_id, namespaces.owner_id, hosted_nodes.visibility, \
                hosted_nodes.slug, hosted_nodes.name, hosted_nodes.kind \
           FROM hosted_nodes LEFT JOIN namespaces ON namespaces.id = hosted_nodes.namespace_id \
          WHERE hosted_nodes.id = ?",
        &[r],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("节点不存在"))
}

/// 取一条节点引用（列顺序固定：id / namespace_id / owner_id / visibility / slug / name / kind）。
async fn fetch_node_ref(state: &AppState, sql: &str, binds: &[&str]) -> ApiResult<Option<NodeRef>> {
    type Row = (
        String,
        String,
        Option<String>,
        String,
        String,
        String,
        String,
    );
    let mut q = sqlx::query_as::<_, Row>(sql);
    for b in binds {
        q = q.bind(*b);
    }
    let row = q
        .fetch_optional(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    Ok(row.map(
        |(id, namespace_id, owner_id, visibility, slug, name, kind)| NodeRef {
            id,
            namespace_id,
            owner_id,
            visibility,
            slug,
            name,
            kind,
        },
    ))
}

/// 节点可见性（Go 的 `canSeeNode`）：公开人人可见；私有要么是自己的，要么拿到 `node` 授权。
async fn can_see_node(state: &AppState, n: &NodeRef, user_id: &str) -> bool {
    if n.visibility == "public" {
        return true;
    }
    if user_id.is_empty() {
        return false;
    }
    if n.owner_id.as_deref() == Some(user_id) {
        return true;
    }
    if crate::httpapi::artifacts::can_manage(state, &n.namespace_id, user_id).await {
        return true;
    }
    let Some(owner) = n.owner_id.as_deref() else {
        return false;
    };
    store::grants::has(
        state.pool(),
        owner,
        user_id,
        store::grants::KIND_NODE,
        &n.namespace_id,
    )
    .await
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
/// GET /api/grants?direction=outgoing|incoming —— 形状照 Go：
/// `{"grants":[grantJSON…], "direction":"outgoing|incoming", "total":N}`。
///
/// `direction` 缺省是**发出**的授权；`incoming` 时每条里指的是授权人（`grantee` 换成上游那位），
/// 因为「谁授权给我」才是我要看的人。
async fn list_grants(
    State(state): State<AppState>,
    auth: Auth,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let a = auth.require_scope("grants:read")?;
    let incoming = web::query(&uri, "direction").as_deref() == Some("incoming");
    let rows = if incoming {
        store::grants::list_received(state.pool(), &a.user_id).await
    } else {
        store::grants::list_owned(state.pool(), &a.user_id).await
    }
    .map_err(ApiError::from_db)?;
    let mut list = Vec::with_capacity(rows.len());
    for g in &rows {
        let other = if incoming {
            &g.owner_id
        } else {
            &g.grantee_user_id
        };
        list.push(grant_json(&state, g, other).await);
    }
    let dir = if incoming { "incoming" } else { "outgoing" };
    Ok(ncc_core::error::ok(json!({
        "grants": list,
        "direction": dir,
        "total": list.len(),
    })))
}

/// 一条授权对外长这样（Go 的 `grantJSON(g, userBrief(对方), nsBrief)`）：
/// 对方是**嵌套对象**（id / name / displayName / handle），不是一串 id —— 页面要直接显示。
async fn grant_json(state: &AppState, g: &store::grants::Grant, other_user_id: &str) -> Value {
    let ns = if g.namespace_id.is_empty() {
        Value::Null
    } else {
        let slug = store::namespaces::by_id(state.pool(), &g.namespace_id)
            .await
            .ok()
            .flatten()
            .map(|n| n.slug)
            .unwrap_or_default();
        json!({"id": g.namespace_id, "slug": slug})
    };
    json!({
        "id": g.id,
        "kind": g.kind,
        "namespaceId": g.namespace_id,
        "namespace": ns,
        "note": g.note,
        "grantee": user_brief(state, other_user_id).await,
        "createdAt": g.created_at,
    })
}

/// 用户简介（Go 的 `userBrief`）：`@handle` 取的是他的**个人命名空间** slug。
async fn user_brief(state: &AppState, user_id: &str) -> Value {
    let mut out = json!({"id": user_id});
    let Ok(Some(u)) = store::users::by_id(state.pool(), user_id).await else {
        return out;
    };
    out["name"] = json!(u.name);
    out["displayName"] = json!(u.name);
    if let Ok(Some(ns)) = store::namespaces::personal(state.pool(), &u.id).await {
        out["handle"] = json!(format!("@{}", ns.slug));
        out["namespace"] = json!(ns.slug);
    }
    out
}

/// POST /api/grants 的请求体：字段名与语义照 Go 的 `createGrant`。
///
/// `ref` 是**指人**的（`@handle` 或用户 id，`userId` 是它的别名），`namespace` 是**slug**
/// （不是内部 id）—— CLI 的 `ncc grant set --to @someone --namespace acme` 就是这么发的。
#[derive(Debug, Deserialize, Default)]
struct CreateGrantReq {
    // Go 的字段名就是 `ref`（`Ref string \`json:"ref"\``）；Rust 里 `ref` 是关键字，
    // 只能用 `ref_` + rename，漏了 rename 会让 CLI/脚本发的 `{"ref":…}` 静默丢掉。
    #[serde(
        default,
        rename = "ref",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    ref_: String,
    #[serde(
        default,
        rename = "userId",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    user_id: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    kind: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    namespace: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
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
            "kind 只能是 artifact | node | config | trace | state",
        ));
    }
    let who = if body.ref_.trim().is_empty() {
        body.user_id.trim()
    } else {
        body.ref_.trim()
    };
    let Some(grantee) = resolve_user(&state, who).await? else {
        return Err(ApiError::not_found(
            "找不到这个用户（用 @用户名 或用户 id）",
        ));
    };
    if grantee.id == a.user_id {
        return Err(ApiError::bad_request("bad_request", "不需要给自己授权"));
    }

    // 命名空间限定：只有 artifact 授权能限定，且必须是**你自己的**命名空间
    let ns_slug = body.namespace.trim();
    let mut ns_id = String::new();
    if !ns_slug.is_empty() {
        if kind != "artifact" {
            return Err(ApiError::bad_request(
                "bad_request",
                "只有 artifact 授权可以限定命名空间",
            ));
        }
        let slug = ns_slug.trim_start_matches('@').to_string();
        let Some(ns) = store::namespaces::by_slug(state.pool(), &slug)
            .await
            .map_err(ApiError::from_db)?
        else {
            return Err(ApiError::not_found(format!("命名空间不存在：{ns_slug}")));
        };
        if !crate::httpapi::artifacts::can_manage(&state, &ns.id, &a.user_id).await {
            return Err(ApiError::forbidden("只能授权你自己的命名空间"));
        }
        ns_id = ns.id;
    }

    let note = truncate_chars(body.note.trim(), 200);
    let g = store::grants::create(state.pool(), &a.user_id, &grantee.id, kind, &ns_id, &note)
        .await
        .map_err(ApiError::from_db)?;
    Ok(ncc_core::error::ok_status(
        StatusCode::CREATED,
        json!({ "grant": grant_json(&state, &g, &g.grantee_user_id).await }),
    ))
}

/// 按 `@handle`（个人命名空间 slug）或用户 id 指人（Go 的 `resolveUser`）。
async fn resolve_user(state: &AppState, ref_: &str) -> ApiResult<Option<store::users::User>> {
    let ref_ = ref_.trim();
    if ref_.is_empty() {
        return Ok(None);
    }
    if !ref_.starts_with('@') {
        if let Some(u) = store::users::by_id(state.pool(), ref_)
            .await
            .map_err(ApiError::from_db)?
        {
            return Ok(Some(u));
        }
    }
    let Some(ns) = store::namespaces::by_slug(state.pool(), ref_.trim_start_matches('@'))
        .await
        .map_err(ApiError::from_db)?
    else {
        return Ok(None);
    };
    store::users::by_id(state.pool(), &ns.owner_id)
        .await
        .map_err(ApiError::from_db)
}

/// 按**字符**（不是字节）截断，避免把中文截成半个字。
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
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
