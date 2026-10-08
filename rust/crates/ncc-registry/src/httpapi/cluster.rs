//! 集群（master / worker）的 HTTP 层。
//!
//! 两种角色共用一组端点，靠 `cfg.role` 分流：
//!
//! * master —— 接受 worker 的注册 / 心跳（`join`/`heartbeat`），聚合它们的目录
//!   （`directory`/`workers`），并把制品**分发**到 worker（`replicate` → worker 的 `ingest`）。
//! * worker —— 接收 master 推来的副本（`ingest`）与回收指令（`revoke`）。
//!
//! 节点间调用一律走 `X-NCC-Cluster-Token`（没配就是「内网开放集群」，任何能连上的节点都放行，
//! 与单节点内网的信任模型一致）。
//!
//! 与 Go 的刻意差别（逐条）：
//!
//! 1. **没有后台循环**：Go 在 `clusterHub.start()` 里跑 worker 心跳 / master 清理协程。
//!    `main.rs` 在本批次是冻结的，所以循环没挂上去 —— 出站函数（`join_once`/`heartbeat_once`）
//!    与清理函数（`store::cluster::prune_workers`）都已实现并测过，差一行 spawn。
//! 2. **worker 视角不回显 master 心跳状态**：Go 用 hub 的内存状态给出 `lastHeartbeat`/`lastError`；
//!    没有循环就没有这份状态，`master.online` 恒为 false（= 还没连上过）。
//! 3. `/api/nodes/route`（clusterRoute）由 nodes 族实现，本文件不重复注册，免得路由冲突。
//! 4. 时间戳用 UTC RFC3339 秒精度（`timeNow()` 在 Go 是本地时区），字段名一致。

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use ncc_core::error::{ok, ApiError, ApiResult};
use ncc_core::timeutil::{now_rfc3339, now_unix};
use ncc_core::web;

use crate::httpapi::{helpers, AppState, Auth};
use crate::store;
use crate::store::cluster::{AdvertInput, ClusterWorker, WorkerInput};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/cluster/join", post(cluster_join))
        .route("/cluster/heartbeat", post(cluster_heartbeat))
        .route("/cluster", get(cluster_view))
        .route("/cluster/", get(cluster_view))
        .route("/cluster/workers", get(cluster_workers))
        .route("/cluster/directory", get(cluster_directory))
        .route("/cluster/ingest", post(cluster_ingest))
        .route("/cluster/revoke", post(cluster_revoke))
        .route("/cluster/replicate", post(replicate_artifact))
}

/// 本族没有顶层公开页（集群端点都在 `/api/cluster` 下）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
}

fn bad_body() -> ApiError {
    ApiError::bad_request("bad_request", "请求体格式错误")
}

/* ---------------- 鉴权 ---------------- */

/// 校验节点间接入身份。没配 token 就是「内网开放集群」。
fn check_cluster_token(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let want = state.cfg().cluster_token.trim();
    if want.is_empty() {
        return Ok(());
    }
    let mut got = headers
        .get("x-ncc-cluster-token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    if got.is_empty() {
        got = web::bearer(headers).unwrap_or_default().trim().to_string();
    }
    if got != want {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "cluster_token_invalid",
            "集群 token 不正确（对方配了 NCCR_CLUSTER_TOKEN）",
        ));
    }
    Ok(())
}

/// 只允许 worker 向 master 注册 / 心跳。
fn require_cluster_master(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if state.cfg().role != crate::config::ROLE_MASTER {
        return Err(ApiError::bad_request(
            "not_master",
            "本节点是 worker，不接受集群注册（请指向 master）",
        ));
    }
    check_cluster_token(state, headers)
}

/* ---------------- 请求体 ---------------- */

#[derive(Debug, Deserialize, Default)]
struct NodeInfo {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    id: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    name: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    url: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    version: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    region: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    capabilities: Vec<String>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    artifacts: i64,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    nodes: i64,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    users: i64,
}

#[derive(Debug, Deserialize, Default)]
struct ClusterJoinReq {
    #[serde(default)]
    node: NodeInfo,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    artifacts: Vec<AdvertReq>,
}

#[derive(Debug, Deserialize, Default)]
struct AdvertReq {
    #[serde(
        default,
        rename = "ref",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    ref_: String,
    #[serde(
        default,
        rename = "namespaceSlug",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    namespace_slug: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    slug: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    kind: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    name: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    version: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    summary: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    sha256: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    size: i64,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    downloads: i64,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<String>,
}

fn advert_inputs(list: &[AdvertReq]) -> Vec<AdvertInput> {
    list.iter()
        .map(|a| AdvertInput {
            ref_: a.ref_.clone(),
            namespace_slug: a.namespace_slug.clone(),
            slug: a.slug.clone(),
            kind: a.kind.clone(),
            name: a.name.clone(),
            version: a.version.clone(),
            summary: a.summary.clone(),
            tags: a.tags.clone(),
            sha256: a.sha256.clone(),
            size: a.size,
            downloads: a.downloads,
            updated_at: a.updated_at.clone(),
        })
        .collect()
}

/* ---------------- 视图 ---------------- */

fn worker_json(w: &ClusterWorker, ttl: Duration) -> Value {
    json!({
        "id": w.id, "name": w.name, "url": w.url, "version": w.version, "role": "worker",
        "region": w.region, "capabilities": w.caps(),
        "artifacts": w.artifacts, "nodes": w.nodes, "users": w.users,
        "online": w.online(ttl), "lastSeen": w.last_seen, "firstSeen": w.first_seen,
    })
}

/// 本节点的身份（集群总览用）。
async fn self_node_json(state: &AppState, artifacts: i64, nodes: i64, users: i64) -> Value {
    let cfg = state.cfg();
    // 提供能力里的「自证」部分来自远程执行探针（Go 的 execrun.Probe）。exec 族本批次未迁移，
    // 探不到运行器存在与否 —— 宁可报空，也不报一个「标签上写着能跑」的假能力。
    json!({
        "id": cfg.node_id, "name": cfg.node_name, "url": cfg.public_url,
        "role": cfg.role, "version": ncc_core::REGISTRY_VERSION, "region": cfg.node_region,
        "artifacts": artifacts, "nodes": nodes, "users": users,
        "online": true, "lastSeen": now_rfc3339(),
        "capabilities": [], "capabilitiesVerified": [], "tags": [],
    })
}

/// master 侧向 worker 回报的身份 + 集群规模。
async fn master_block(state: &AppState) -> Value {
    let (artifacts, nodes, users) = helpers::counts(state).await;
    let workers = store::cluster::list_workers(state.pool())
        .await
        .unwrap_or_default();
    let cfg = state.cfg();
    json!({
        "id": cfg.node_id, "name": cfg.node_name, "url": cfg.public_url,
        "role": crate::config::ROLE_MASTER, "version": ncc_core::REGISTRY_VERSION,
        "region": cfg.node_region, "online": true, "lastSeen": now_rfc3339(),
        "cluster": {
            "workers": workers.len(), "artifacts": artifacts,
            "hostedNodes": nodes, "users": users,
        },
    })
}

/* ---------------- master 侧：注册 / 心跳 ---------------- */

/// POST /api/cluster/join
async fn cluster_join(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ClusterJoinReq>, JsonRejection>,
) -> ApiResult<Response> {
    require_cluster_master(&state, &headers)?;
    let Json(body) = body.map_err(|_| bad_body())?;
    if body.node.id.trim().is_empty() || body.node.url.trim().is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "node.id 与 node.url 必填",
        ));
    }
    if body.node.id.trim() == state.cfg().node_id {
        return Err(ApiError::bad_request(
            "bad_request",
            "不能把自己注册成 worker",
        ));
    }
    let accepted = body.artifacts.len();
    upsert_and_sync(&state, &body).await?;
    tracing::info!(
        "[cluster] worker 加入: {} ({}) 目录 {} 条",
        body.node.name,
        body.node.url,
        accepted
    );
    Ok(ok(json!({
        "ok": true, "master": master_block(&state).await,
        "joined": body.node.id, "accepted": accepted,
    })))
}

/// POST /api/cluster/heartbeat
async fn cluster_heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ClusterJoinReq>, JsonRejection>,
) -> ApiResult<Response> {
    require_cluster_master(&state, &headers)?;
    let Json(body) = body.map_err(|_| bad_body())?;
    if body.node.id.trim().is_empty() {
        return Err(ApiError::bad_request("bad_request", "node.id 必填"));
    }
    let accepted = body.artifacts.len();
    upsert_and_sync(&state, &body).await?;
    Ok(ok(json!({
        "ok": true, "master": master_block(&state).await,
        "accepted": accepted, "time": now_rfc3339(),
    })))
}

async fn upsert_and_sync(state: &AppState, body: &ClusterJoinReq) -> ApiResult<()> {
    let n = &body.node;
    store::cluster::upsert_worker(
        state.pool(),
        &WorkerInput {
            id: n.id.clone(),
            name: n.name.clone(),
            url: n.url.clone(),
            version: n.version.clone(),
            region: n.region.clone(),
            capabilities: n.capabilities.clone(),
            artifacts: body.artifacts.len() as i64,
            nodes: n.nodes,
            users: n.users,
        },
    )
    .await
    .map_err(ApiError::from_db)?;
    store::cluster::replace_adverts(state.pool(), n.id.trim(), &advert_inputs(&body.artifacts))
        .await
        .map_err(ApiError::from_db)?;
    Ok(())
}

/// GET /api/cluster —— 集群总览（master 与 worker 都返回同一形状）。
async fn cluster_view(State(state): State<AppState>) -> ApiResult<Response> {
    let cfg = state.cfg();
    let (artifacts, nodes, users) = helpers::counts(&state).await;
    let self_node = self_node_json(&state, artifacts, nodes, users).await;

    let workers = store::cluster::list_workers(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    let mut wlist: Vec<Value> = Vec::with_capacity(workers.len());
    let mut online_workers: i64 = 0;
    let (mut worker_artifacts, mut worker_nodes) = (0i64, 0i64);
    for w in &workers {
        let j = worker_json(w, cfg.node_ttl);
        if j["online"] == json!(true) {
            online_workers += 1;
        }
        worker_artifacts += w.artifacts;
        worker_nodes += w.nodes;
        wlist.push(j);
    }

    let mut out = json!({
        "role": cfg.role,
        "self": self_node,
        "workers": wlist,
        "totals": {
            "nodes": 1 + wlist.len() as i64,
            "workers": wlist.len() as i64,
            "workersOnline": online_workers,
            "onlineNodes": 1 + online_workers,
            "artifacts": artifacts + worker_artifacts,
            "hostedNodes": nodes + worker_nodes,
            "users": users,
        },
        "ttlSec": cfg.node_ttl.as_secs() as i64,
        "everySec": cfg.heartbeat_every.as_secs() as i64,
        "console": format!("{}/", cfg.public_url.trim_end_matches('/')),
    });

    // worker 视角：回显自己认识的 master（没连上过就是 url + online=false）。
    if cfg.role == crate::config::ROLE_WORKER {
        out["master"] = json!({"url": cfg.master_url, "online": false});
    }
    Ok(ok(out))
}

/// GET /api/cluster/workers
async fn cluster_workers(State(state): State<AppState>) -> ApiResult<Response> {
    let ttl = state.cfg().node_ttl;
    let workers = store::cluster::list_workers(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<Value> = workers.iter().map(|w| worker_json(w, ttl)).collect();
    Ok(ok(json!({
        "workers": list, "total": list.len(),
        "ttlSec": ttl.as_secs() as i64,
    })))
}

/// GET /api/cluster/directory —— 聚合目录（master 本地 + 各 worker 上报）。
async fn cluster_directory(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let cfg = state.cfg();
    let q = web::query(&uri, "q").unwrap_or_default();
    let kind = web::query(&uri, "kind").unwrap_or_default();
    let tag = web::query(&uri, "tag").unwrap_or_default();
    // Go 的 ListOpts 只认 "downloads"，其它值一律按 updated_at 排；这里同样只透传 downloads，
    // 免得 Rust 的 "name"/"created" 多出来的排序口径和 Go 产生分歧。
    let sort = web::query(&uri, "sort").unwrap_or_default();

    let res = store::artifacts::list(
        state.pool(),
        &store::artifacts::ListOpts {
            q: q.clone(),
            kind: kind.clone(),
            tag: tag.clone(),
            page: 1,
            size: 100,
            order_by: if sort.trim() == "downloads" {
                "downloads".to_string()
            } else {
                String::new()
            },
            public_only: true,
            statuses: vec!["published".to_string()],
            ..Default::default()
        },
    )
    .await
    .map_err(ApiError::from_db)?;

    let replicas = store::cluster::replica_targets_by_ref(state.pool())
        .await
        .unwrap_or_default();
    let mut items: Vec<Value> = Vec::with_capacity(res.rows.len());
    let mut seen: HashMap<String, bool> = HashMap::new();
    for row in &res.rows {
        let base = row.ref_of(); // @ns/slug（不含版本）
        seen.insert(base.clone(), true);
        let mut j = crate::httpapi::artifacts::artifact_json(row);
        j["via"] = json!({"role": "self", "nodeId": cfg.node_id, "nodeName": cfg.node_name});
        // 本地条目：标出已分发到哪些 worker（副本），便于判断「就近可拉」。
        if let Some(rows) = replicas.get(&format!("{}@{}", base, row.version)) {
            if !rows.is_empty() {
                j["replicas"] = json!(rows
                    .iter()
                    .map(|rt| json!({"nodeId": rt.worker_id, "nodeName": rt.worker_name, "size": rt.size}))
                    .collect::<Vec<_>>());
            }
        }
        items.push(j);
    }

    let adv = store::cluster::list_adverts(state.pool(), &q, &kind, &tag, 300)
        .await
        .map_err(ApiError::from_db)?;
    let mut remote = 0i64;
    for a in &adv {
        let base = format!("@{}/{}", a.namespace_slug, a.slug);
        if seen.contains_key(&base) {
            continue; // master 本地有同一份，以本地为准
        }
        seen.insert(base, true);
        remote += 1;
        items.push(json!({
            "id": a.id, "kind": a.kind, "name": a.name, "slug": a.slug, "version": a.version,
            "summary": a.summary, "tags": store::parse_list(&a.tags),
            "visibility": "public", "status": "published",
            "storage": {
                "provider": "cluster", "url": "", "sha256": a.sha256, "size": a.size,
            },
            "downloads": a.downloads,
            "namespace": {"slug": a.namespace_slug, "name": a.namespace_slug},
            "ref": a.ref_,
            "via": {
                "role": "worker", "nodeId": a.worker_id,
                "nodeName": a.worker_name, "nodeUrl": a.worker_url,
            },
            "updatedAt": a.updated_at,
        }));
    }

    Ok(ok(json!({
        "items": items, "total": items.len(),
        "local": res.rows.len(), "remote": remote,
        "role": cfg.role,
    })))
}

/* ---------------- worker 侧：落副本 / 回收副本 ---------------- */

#[derive(Debug, Deserialize, Default)]
struct IngestReq {
    #[serde(
        default,
        rename = "ref",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    ref_: String,
    #[serde(
        default,
        rename = "namespaceSlug",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    namespace_slug: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    slug: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    kind: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    name: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    version: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    summary: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    tags: Vec<String>,
    #[serde(default)]
    manifest: HashMap<String, Value>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    sha256: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    size: i64,
    #[serde(
        default,
        rename = "sourceUrl",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    source_url: String,
}

/// POST /api/cluster/ingest —— 落一份副本（幂等：同 ref 盖写）。
async fn cluster_ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<IngestReq>, JsonRejection>,
) -> ApiResult<Response> {
    check_cluster_token(&state, &headers)?;
    let Json(body) = body.map_err(|_| bad_body())?;
    if body.ref_.trim().is_empty()
        || body.namespace_slug.trim().is_empty()
        || body.slug.trim().is_empty()
        || body.source_url.trim().is_empty()
    {
        return Err(ApiError::bad_request(
            "bad_request",
            "ref / namespaceSlug / slug / sourceUrl 必填",
        ));
    }
    if !crate::httpapi::artifacts::valid_kind(body.kind.trim()) {
        return Err(ApiError::bad_request(
            "bad_request",
            format!("未知 kind {}", body.kind),
        ));
    }

    // 自己去来源拉字节（内部调用带集群 token；BYO 直链则由 http 客户端跟随重定向）。
    let data = fetch_blob(&state, body.source_url.trim())
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "source_unreachable",
                format!("拉取来源字节失败: {e}"),
            )
        })?;
    let got = ncc_core::crypto::sha256_hex(&data);
    if !body.sha256.trim().is_empty() && got != body.sha256.trim() {
        return Err(ApiError::bad_request(
            "digest_mismatch",
            format!(
                "字节校验不一致（期望 {}，实际 {}）",
                body.sha256.trim(),
                got
            ),
        ));
    }

    let ext = match body.slug.rfind('.') {
        Some(i) if i > 0 => &body.slug[i..],
        _ => "",
    };
    let blob_name = format!(
        "{}-{}{}",
        ncc_core::ids::slugify(&body.namespace_slug),
        ncc_core::crypto::rand_hex(6),
        ext
    );
    let storage_url = state
        .blobs()
        .put(&blob_name, &data)
        .map_err(|_| ApiError::internal("写入副本字节失败"))?;
    let ns = store::cluster::ensure_mirror_namespace(
        state.pool(),
        &body.namespace_slug,
        body.namespace_slug.trim(),
    )
    .await
    .map_err(ApiError::from_db)?;

    let manifest = if body.manifest.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&body.manifest).unwrap_or_default()
    };
    let row = store::cluster::upsert_replica(
        state.pool(),
        &ns.id,
        &store::cluster::NewReplica {
            namespace_slug: body.namespace_slug.clone(),
            kind: body.kind.trim().to_string(),
            name: body.name.clone(),
            slug: body.slug.clone(),
            version: body.version.clone(),
            summary: body.summary.clone(),
            tags: body.tags.clone(),
            manifest,
            provider: "local".to_string(),
            storage_url: storage_url.clone(),
            blob_name,
            sha256: got.clone(),
            size: data.len() as i64,
            created_by: "cluster".to_string(),
            origin_ref: body.ref_.trim().to_string(),
        },
    )
    .await
    .map_err(ApiError::from_db)?;

    tracing::info!(
        "[cluster] 已接收副本 {}（{} 字节，sha256 {}…）",
        body.ref_,
        data.len(),
        &got[..12.min(got.len())]
    );
    let cfg = state.cfg();
    Ok(ok(json!({
        "ok": true, "ref": body.ref_, "sha256": got, "size": data.len(),
        "nodeId": cfg.node_id, "nodeName": cfg.node_name,
        "artifactId": row.id, "storageUrl": row.storage_url,
    })))
}

/// POST /api/cluster/revoke —— 回收一份副本。
async fn cluster_revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> ApiResult<Response> {
    check_cluster_token(&state, &headers)?;
    let ref_ = body
        .ok()
        .and_then(|Json(v)| {
            v.get("ref")
                .and_then(|x| x.as_str())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_default();
    if ref_.is_empty() {
        return Err(ApiError::bad_request("bad_request", "需要 ref"));
    }
    let (id, blob_name, removed) = store::cluster::delete_replica(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?;
    if removed && !blob_name.is_empty() {
        let _ = state.blobs().delete(&blob_name);
    }
    if removed {
        tracing::info!("[cluster] 已回收副本 {ref_}");
    }
    let cfg = state.cfg();
    Ok(ok(json!({
        "ok": true, "ref": ref_, "removed": removed, "artifactId": id,
        "nodeId": cfg.node_id, "nodeName": cfg.node_name,
    })))
}

/// 拉取来源字节（内部调用带集群 token）。
async fn fetch_blob(state: &AppState, url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(url);
    let token = state.cfg().cluster_token.trim().to_string();
    if !token.is_empty() {
        req = req.header("X-NCC-Cluster-Token", token);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(format!("来源返回 {status}"));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    // Go 用 LimitReader(256MB+1) 截断；这里直接判超限并报错 —— 少读一截比悄悄落半份副本安全。
    if bytes.len() > 256 << 20 {
        return Err("来源字节超过 256MB 上限".to_string());
    }
    Ok(bytes.to_vec())
}

/* ---------------- master 侧：分发 / 回收 ---------------- */

#[derive(Debug, Deserialize, Default)]
struct ReplicateReq {
    #[serde(
        default,
        rename = "ref",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    ref_: String,
    /// "all" 或 ["workerId"/"workerName", …]；缺省时报错（避免误分发）。
    #[serde(default)]
    targets: Option<Value>,
}

/// POST /api/cluster/replicate —— 把已存在的制品分发到 worker。
async fn replicate_artifact(
    State(state): State<AppState>,
    auth: Auth,
    body: Result<Json<ReplicateReq>, JsonRejection>,
) -> ApiResult<Response> {
    let a = auth.require_scope("registry:publish")?;
    let Json(body) = body.map_err(|_| bad_body())?;
    let row = store::artifacts::by_ref(state.pool(), body.ref_.trim())
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("制品不存在"))?;
    if !crate::httpapi::artifacts::can_manage(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("你不是该条目的 owner/成员"));
    }
    if row.is_replica() {
        return Err(ApiError::bad_request(
            "bad_request",
            "这是别人分发下来的副本，请到源头节点上操作",
        ));
    }
    let targets = store::cluster::list_workers(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    let picked = pick_workers(&targets, body.targets.as_ref())
        .map_err(|e| ApiError::bad_request("bad_request", e))?;
    let results = fanout(&state, &row, &picked).await;
    Ok(ok(json!({
        "ref": row.ref_of(), "results": results, "targets": picked.len(),
    })))
}

/// 解析分发目标：`"all"` / `["名称或 id"]`；没给就报错（避免误分发）。
pub(crate) fn pick_workers(
    all: &[ClusterWorker],
    spec: Option<&Value>,
) -> Result<Vec<ClusterWorker>, String> {
    let spec = match spec {
        None | Some(Value::Null) => {
            return Err("需要 targets（\"all\" 或 worker 名称/id 列表）".to_string())
        }
        Some(v) => v,
    };
    match spec {
        Value::String(s) => {
            if s.trim().eq_ignore_ascii_case("all") {
                return Ok(all.to_vec());
            }
            if s.trim().is_empty() {
                return Err("targets 不能为空".to_string());
            }
            match_workers(all, &[s.clone()])
        }
        Value::Array(arr) => {
            let want: Vec<String> = arr
                .iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect();
            if want.is_empty() {
                return Err("targets 不能为空".to_string());
            }
            match_workers(all, &want)
        }
        _ => Err("targets 格式不支持".to_string()),
    }
}

fn match_workers(all: &[ClusterWorker], want: &[String]) -> Result<Vec<ClusterWorker>, String> {
    let mut out: Vec<ClusterWorker> = Vec::with_capacity(want.len());
    for w in want {
        let hit = all
            .iter()
            .find(|c| c.id.eq_ignore_ascii_case(w.trim()) || c.name.eq_ignore_ascii_case(w.trim()));
        match hit {
            Some(c) => out.push(c.clone()),
            None => {
                return Err(format!(
                    "找不到 worker {w:?}（用 /api/cluster/workers 看有哪些）"
                ))
            }
        }
    }
    Ok(out)
}

/// 把一份制品推给若干 worker：worker 自己去 `sourceUrl` 拉字节并校验。
pub(crate) async fn fanout(
    state: &AppState,
    row: &store::artifacts::ArtifactRow,
    workers: &[ClusterWorker],
) -> Vec<Value> {
    let cfg = state.cfg();
    let ref_ = format!("{}@{}", row.ref_of(), row.version);
    let payload = json!({
        "ref": ref_,
        "namespaceSlug": row.ns_slug.clone().unwrap_or_default(),
        "slug": row.slug,
        "kind": row.kind,
        "name": row.name,
        "version": row.version,
        "summary": row.summary,
        "tags": store::artifacts::tags_of(row),
        "manifest": web::parse_json_any(&row.manifest),
        "sha256": row.sha256,
        "size": row.size,
        // 给 worker 一个短时签名地址：私有条目也能分发，且不必把集群 token 当作下载凭据。
        "sourceUrl": signed_bytes_url(cfg, row, 1800),
    });
    let mut results: Vec<Value> = Vec::with_capacity(workers.len());
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
    {
        Ok(c) => c,
        Err(_) => return vec![json!({"nodeName": "-", "ok": false, "error": "序列化失败"})],
    };
    for w in workers {
        let mut item = json!({"nodeId": w.id, "nodeName": w.name, "nodeUrl": w.url, "ok": false});
        if !w.online(cfg.node_ttl * 4) {
            item["error"] = json!("节点离线（超过 TTL 没心跳）");
            results.push(item);
            continue;
        }
        let url = format!("{}/api/cluster/ingest", w.url.trim_end_matches('/'));
        let mut req = client.post(&url).json(&payload);
        if !cfg.cluster_token.trim().is_empty() {
            req = req.header("X-NCC-Cluster-Token", cfg.cluster_token.trim());
        }
        match req.send().await {
            Err(e) => {
                item["error"] = json!(format!("不可达：{e}"));
                results.push(item);
                continue;
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let out: Value = resp.json().await.unwrap_or(Value::Null);
                if status >= 400 {
                    let msg = out
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("返回 {status}"));
                    item["error"] = json!(msg);
                    results.push(item);
                    continue;
                }
                let got = out
                    .get("sha256")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let size = out.get("size").and_then(|v| v.as_i64()).unwrap_or(0);
                item["ok"] = json!(true);
                item["sha256"] = json!(got.clone());
                item["size"] = json!(size);
                // 分发成功即记账：下架回收以这份记录为准，不依赖 worker 心跳是否已上报。
                let _ = store::cluster::record_replica_target(
                    state.pool(),
                    &ref_,
                    &w.id,
                    &w.name,
                    &w.url,
                    &got,
                    size,
                )
                .await;
                results.push(item);
            }
        }
    }
    results
}

/// 回收该 ref 在各节点上的副本。
///
/// 目标来源：master 侧的分发记录为主，worker 上报的目录（adverts）为兜底 —— 后者依赖心跳，
/// 可能滞后；只认它会让「刚分发完就下架」漏掉回收。
///
/// 注：Go 里它由 `deleteItem`（下架）调用；Rust 的制品下架在 artifacts 族，本批次尚未接上，
/// 所以这里先作为可独立调用的能力提供（已用测试覆盖），待接线。
#[allow(dead_code)]
pub async fn revoke_replicas(state: &AppState, ref_: &str) -> Vec<Value> {
    let mut targets: HashMap<String, (String, String)> = HashMap::new();
    if let Ok(rows) = store::cluster::list_replica_targets(state.pool(), ref_).await {
        for t in rows {
            targets.insert(
                t.worker_id.clone(),
                (t.worker_name.clone(), t.worker_url.clone()),
            );
        }
    }
    if let Ok(adv) = store::cluster::find_adverts_by_ref(state.pool(), ref_).await {
        for a in adv {
            let (id, url) = (
                a.worker_id.clone(),
                a.worker_url.clone().unwrap_or_default(),
            );
            if id.is_empty() || url.is_empty() {
                continue;
            }
            targets
                .entry(id)
                .or_insert((a.worker_name.clone().unwrap_or_default(), url));
        }
    }
    if targets.is_empty() {
        return Vec::new();
    }

    let cfg = state.cfg();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut results: Vec<Value> = Vec::with_capacity(targets.len());
    let mut all_ok = true;
    for (id, (name, url)) in targets {
        let mut item = json!({"nodeId": id, "nodeName": name, "ok": false});
        let mut req = client
            .post(format!("{}/api/cluster/revoke", url.trim_end_matches('/')))
            .json(&json!({"ref": ref_}));
        if !cfg.cluster_token.trim().is_empty() {
            req = req.header("X-NCC-Cluster-Token", cfg.cluster_token.trim());
        }
        match req.send().await {
            Err(e) => {
                item["error"] = json!(format!("不可达：{e}"));
                all_ok = false;
            }
            Ok(resp) => {
                let status = resp.status().as_u16();
                let out: Value = resp.json().await.unwrap_or(Value::Null);
                let ok = status < 400;
                item["ok"] = json!(ok);
                item["removed"] = json!(out
                    .get("removed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false));
                if !ok {
                    all_ok = false;
                }
            }
        }
        results.push(item);
    }
    // 全部回收成功才清账；否则留着，下次下架/重试还能找到这些节点。
    if all_ok {
        let _ = store::cluster::delete_replica_targets(state.pool(), ref_).await;
    }
    results
}

/// 短时有效的字节地址：HMAC(secret, `ref|exp`)。
///
/// 与 artifacts 族那份**必须同构**（同一个 `/bytes` 处理器校验的），所以这里按同样的算法
/// 复刻一份 —— 跨族复用私有函数要改别人的文件，本批次约定不动别族。
fn signed_bytes_url(
    cfg: &crate::config::Config,
    row: &store::artifacts::ArtifactRow,
    ttl_secs: i64,
) -> String {
    let exp = now_unix() + ttl_secs;
    let ref_ = row.ref_of();
    let sig = ncc_core::crypto::hmac_sha256_b64url(&cfg.jwt_secret, &format!("{ref_}|{exp}"));
    format!(
        "{}/api/registry/{}/bytes?exp={}&sig={}",
        cfg.public_url.trim_end_matches('/'),
        ref_,
        exp,
        sig
    )
}

/* ---------------- worker 侧：向 master 注册 / 心跳 ---------------- */

/// worker 上报体：我是谁 + 我这里有什么。
#[allow(dead_code)]
async fn heartbeat_body(state: &AppState) -> Value {
    let cfg = state.cfg();
    let rows = store::cluster::list_advertisable_artifacts(state.pool(), 2000)
        .await
        .unwrap_or_default();
    let (artifacts, nodes, users) = helpers::counts(state).await;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "ref": format!("{}@{}", r.ref_of(), r.version),
                "namespaceSlug": r.ns_slug,
                "slug": r.slug,
                "kind": r.kind,
                "name": r.name,
                "version": r.version,
                "summary": r.summary,
                "tags": store::artifacts::tags_of(r),
                "sha256": r.sha256,
                "size": r.size,
                "downloads": r.downloads,
                "updatedAt": r.updated_at,
            })
        })
        .collect();
    json!({
        "node": {
            "id": cfg.node_id, "name": cfg.node_name, "url": cfg.public_url,
            "version": ncc_core::REGISTRY_VERSION, "region": cfg.node_region,
            "capabilities": ["registry", "nodes", "artifacts"],
            "artifacts": artifacts, "nodes": nodes, "users": users,
        },
        "artifacts": items,
    })
}

/// POST `<master>/api/cluster/{path}` —— 出站调用，带超时（别用无限超时把 master 挂死）。
#[allow(dead_code)]
pub async fn post_to_master(state: &AppState, path: &str, body: &Value) -> Result<Value, String> {
    let cfg = state.cfg();
    if cfg.master_url.trim().is_empty() {
        return Err("未配置 master 地址".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client
        .post(format!("{}{path}", cfg.master_url.trim_end_matches('/')))
        .json(body);
    if !cfg.cluster_token.trim().is_empty() {
        req = req.header("X-NCC-Cluster-Token", cfg.cluster_token.trim());
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let out: Value = resp
        .json()
        .await
        .map_err(|e| format!("master 响应无法解析: {e}"))?;
    if status >= 400 {
        let code = out
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_str())
            .unwrap_or("cluster_failed");
        let msg = out
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        return Err(format!("[{code}] {msg}"));
    }
    Ok(out)
}

/// worker 加入 master（`/api/cluster/join`）。
#[allow(dead_code)]
/// 起集群后台循环（对齐 Go `clusterHub.start()`）。
///
/// **worker**：先注册一次（失败只记日志，之后靠心跳重试），再按 `NCCR_HEARTBEAT`
/// （下限 3s，避免把 master 打爆）周期心跳。
/// **master**：每 30s 清掉 `NCCR_NODE_TTL × 4` 没心跳的 worker —— 不清的话目录里会
/// 永远挂着幽灵节点，客户端会一直往一个已经不存在的地址路由。
///
/// 这个循环不能放在 handler 里：它没有请求可依，必须在服务启动时挂上。
pub fn spawn_hub(state: &AppState) {
    let state = state.clone();
    if state.cfg().role == crate::config::ROLE_WORKER {
        tokio::spawn(async move {
            if let Err(e) = join_once(&state).await {
                tracing::warn!("[cluster] 注册到 master 失败（会继续重试）: {e}");
            }
            let interval = state.cfg().heartbeat_every.max(Duration::from_secs(3));
            loop {
                tokio::time::sleep(interval).await;
                match heartbeat_once(&state).await {
                    Ok(_) => {}
                    Err(e) => tracing::warn!("[cluster] 心跳失败: {e}"),
                }
            }
        });
        return;
    }

    tokio::spawn(async move {
        let ttl = state.cfg().node_ttl * 4;
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            match crate::store::cluster::prune_workers(state.pool(), ttl).await {
                Ok(n) if n > 0 => {
                    tracing::info!("[cluster] 已清理 {n} 个失联 worker（及其目录）")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("[cluster] 清理失联 worker 失败: {e}"),
            }
        }
    });
}

pub async fn join_once(state: &AppState) -> Result<Value, String> {
    post_to_master(state, "/api/cluster/join", &heartbeat_body(state).await).await
}

/// worker 心跳（`/api/cluster/heartbeat`）。
#[allow(dead_code)]
pub async fn heartbeat_once(state: &AppState) -> Result<Value, String> {
    post_to_master(
        state,
        "/api/cluster/heartbeat",
        &heartbeat_body(state).await,
    )
    .await
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
            .join(format!("cluster-{}-{name}", std::process::id()))
    }

    async fn state(name: &str, role: &str) -> AppState {
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
        cfg.role = role.to_string();
        cfg.blob_dir = dir.join("blobs");
        cfg.cluster_token = "tok".to_string();
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
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    fn json_req(method: &str, path: &str, token: &str, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if !token.is_empty() {
            b = b.header("x-ncc-cluster-token", token);
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn join_body(id: &str) -> Value {
        json!({
            "node": {"id": id, "name": "办公室", "url": "http://w1:9000",
                     "version": "ncc-registry/0.1.1", "region": "cn-east",
                     "capabilities": ["registry"], "nodes": 2, "users": 1},
            "artifacts": [
                {"ref": "@team/demo@1.0.0", "namespaceSlug": "team", "slug": "demo",
                 "kind": "skill", "name": "演示", "version": "1.0.0",
                 "summary": "s", "tags": ["a"], "sha256": "x", "size": 3, "downloads": 0,
                 "updatedAt": "2026-09-07T01:16:25+08:00"}
            ]
        })
    }

    #[tokio::test]
    async fn join_落库_并能被总览与目录看到() {
        let st = state("join", "master").await;
        let app = router_for(&st);
        let (code, v) = call(
            &app,
            json_req("POST", "/cluster/join", "tok", join_body("W1")),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["joined"], json!("W1"));
        assert_eq!(v["accepted"], json!(1));
        assert_eq!(v["master"]["role"], json!("master"));
        assert_eq!(v["master"]["cluster"]["workers"], json!(1));

        // 心跳：同样的 body，accepted 与 time 都在
        let (code, v) = call(
            &app,
            json_req("POST", "/cluster/heartbeat", "tok", join_body("W1")),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert!(v["time"].is_string());

        // 总览
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/cluster")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["role"], json!("master"));
        assert_eq!(v["workers"][0]["online"], json!(true));
        assert_eq!(v["totals"]["workers"], json!(1));
        assert_eq!(v["totals"]["onlineNodes"], json!(2));
        assert_eq!(v["workers"][0]["url"], json!("http://w1:9000"));

        // worker 列表
        let (_, v) = call(
            &app,
            Request::builder()
                .uri("/cluster/workers")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(v["total"], json!(1));

        // 聚合目录：本地没有 -> 远程一条
        let (_, v) = call(
            &app,
            Request::builder()
                .uri("/cluster/directory")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(v["total"], json!(1));
        assert_eq!(v["remote"], json!(1));
        assert_eq!(v["local"], json!(0));
        assert_eq!(v["items"][0]["via"]["role"], json!("worker"));
        assert_eq!(v["items"][0]["storage"]["provider"], json!("cluster"));
    }

    #[tokio::test]
    async fn 集群_token_与角色_门禁() {
        let st = state("token", "master").await;
        let app = router_for(&st);
        // 不带 token
        let (code, v) = call(&app, json_req("POST", "/cluster/join", "", join_body("W1"))).await;
        assert_eq!(code, SC::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"]["code"], json!("cluster_token_invalid"));
        // 错 token
        let (code, _) = call(
            &app,
            json_req("POST", "/cluster/join", "bad", join_body("W1")),
        )
        .await;
        assert_eq!(code, SC::UNAUTHORIZED);

        // worker 角色不接受注册
        let st2 = state("role", "worker").await;
        let app2 = router_for(&st2);
        let (code, v) = call(
            &app2,
            json_req("POST", "/cluster/join", "tok", join_body("W1")),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("not_master"));

        // worker 总览里有 master 块（url + online=false）
        let (_, v) = call(
            &app2,
            Request::builder()
                .uri("/cluster")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(v["role"], json!("worker"));
        assert_eq!(v["master"]["online"], json!(false));
    }

    #[tokio::test]
    async fn join_参数校验() {
        let st = state("joinbad", "master").await;
        let app = router_for(&st);
        // 缺 node.id / node.url
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/cluster/join",
                "tok",
                json!({"node": {"id": "", "url": ""}}),
            ),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("node.id 与 node.url 必填"));
        // 自己注册自己
        let mut b = join_body("W1");
        b["node"]["id"] = json!(st.cfg().node_id);
        let (code, v) = call(&app, json_req("POST", "/cluster/join", "tok", b)).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("不能把自己注册成 worker"));
        // 非 JSON
        let req = Request::builder()
            .method("POST")
            .uri("/cluster/join")
            .header("content-type", "application/json")
            .header("x-ncc-cluster-token", "tok");
        let (code, v) = call(&app, req.body(Body::from("不是 JSON")).unwrap()).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("请求体格式错误"));
    }

    #[tokio::test]
    async fn ingest_校验摘要_并落副本() {
        let st = state("ingest", "worker").await;
        let app = router_for(&st);
        let data = b"hello world";
        let sha = ncc_core::crypto::sha256_hex(data);

        // 起一个极小的「来源」服务：直接回字节
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let src =
            axum::Router::new().route("/blob", axum::routing::get(|| async { data.to_vec() }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, src).await;
        });

        // 摘要不对 -> digest_mismatch
        let bad = json!({
            "ref": "@team/demo@1.0.0", "namespaceSlug": "team", "slug": "demo",
            "kind": "skill", "name": "演示", "version": "1.0.0",
            "sha256": "deadbeef", "sourceUrl": format!("http://{addr}/blob"),
        });
        let (code, v) = call(&app, json_req("POST", "/cluster/ingest", "tok", bad)).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("digest_mismatch"));

        // 摘要对 -> 落副本
        let good = json!({
            "ref": "@team/demo@1.0.0", "namespaceSlug": "team", "slug": "demo",
            "kind": "skill", "name": "演示", "version": "1.0.0",
            "summary": "s", "tags": ["a"], "manifest": {"k": "v"},
            "sha256": sha, "sourceUrl": format!("http://{addr}/blob"),
        });
        let (code, v) = call(
            &app,
            json_req("POST", "/cluster/ingest", "tok", good.clone()),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["size"], json!(data.len() as i64));
        assert_eq!(v["sha256"], json!(sha));
        assert!(v["artifactId"].is_string());
        // 副本真的在库里，而且是 replica
        let row = store::artifacts::by_ns_slug(st.pool(), "team", "demo")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.origin, "replica");
        assert_eq!(row.size, data.len() as i64);

        // 幂等：再来一次不新增
        let (code, _) = call(&app, json_req("POST", "/cluster/ingest", "tok", good)).await;
        assert_eq!(code, SC::OK);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM artifacts")
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);

        // 未知 kind
        let bad_kind = json!({
            "ref": "@team/x@1.0.0", "namespaceSlug": "team", "slug": "x",
            "kind": "不存在", "sourceUrl": format!("http://{addr}/blob"),
        });
        let (code, v) = call(&app, json_req("POST", "/cluster/ingest", "tok", bad_kind)).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("未知 kind 不存在"));
    }

    #[tokio::test]
    async fn ingest_缺字段与来源不可达() {
        let st = state("ingest2", "worker").await;
        let app = router_for(&st);
        let (code, v) = call(
            &app,
            json_req("POST", "/cluster/ingest", "tok", json!({"ref": "x"})),
        )
        .await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(
            v["error"]["message"],
            json!("ref / namespaceSlug / slug / sourceUrl 必填")
        );

        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/cluster/ingest",
                "tok",
                json!({"ref": "@t/x@1.0.0", "namespaceSlug": "t", "slug": "x", "kind": "skill",
                       "sourceUrl": "http://127.0.0.1:1/nope"}),
            ),
        )
        .await;
        assert_eq!(code, SC::BAD_GATEWAY, "{v}");
        assert_eq!(v["error"]["code"], json!("source_unreachable"));
    }

    #[tokio::test]
    async fn revoke_回收副本_与鉴权() {
        let st = state("revoke", "worker").await;
        let ns = store::cluster::ensure_mirror_namespace(st.pool(), "team", "team")
            .await
            .unwrap();
        store::cluster::upsert_replica(
            st.pool(),
            &ns.id,
            &store::cluster::NewReplica {
                namespace_slug: "team".to_string(),
                kind: "skill".to_string(),
                name: "演示".to_string(),
                slug: "demo".to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                manifest: String::new(),
                provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: "b".to_string(),
                sha256: "s".to_string(),
                size: 1,
                created_by: "cluster".to_string(),
                origin_ref: "@team/demo@1.0.0".to_string(),
            },
        )
        .await
        .unwrap();

        let app = router_for(&st);
        // 缺 ref
        let (code, v) = call(&app, json_req("POST", "/cluster/revoke", "tok", json!({}))).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("需要 ref"));
        // 正确回收
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/cluster/revoke",
                "tok",
                json!({"ref": "@team/demo@1.0.0"}),
            ),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["removed"], json!(true));
        // 再来一次：已经没了
        let (_, v) = call(
            &app,
            json_req(
                "POST",
                "/cluster/revoke",
                "tok",
                json!({"ref": "@team/demo@1.0.0"}),
            ),
        )
        .await;
        assert_eq!(v["removed"], json!(false));
    }

    #[tokio::test]
    async fn replicate_分发到_worker_并记账() {
        let st = state("replicate", "master").await;
        // 造一个持有制品的用户 + 公开已发布条目
        let u = store::users::create(st.pool(), "甲", "a@x.com", "h")
            .await
            .unwrap();
        let ns = store::namespaces::create_account(st.pool(), &u.id, "甲", "jia")
            .await
            .unwrap();
        let row = store::artifacts::create(
            st.pool(),
            store::artifacts::NewArtifact {
                namespace_id: ns.id.clone(),
                slug: "demo".to_string(),
                kind: "skill".to_string(),
                name: "演示".to_string(),
                version: "1.0.0".to_string(),
                summary: String::new(),
                tags: vec![],
                visibility: "public".to_string(),
                status: "published".to_string(),
                manifest: String::new(),
                storage_provider: "local".to_string(),
                storage_url: String::new(),
                blob_name: "b".to_string(),
                sha256: "abc".to_string(),
                size: 3,
                created_by: u.id.clone(),
            },
        )
        .await
        .unwrap();
        st.blobs().put("b", b"hey").unwrap();

        // 起一个假的 worker：ingest 直接回 ok（不真的拉字节）
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let wk = axum::Router::new().route(
            "/api/cluster/ingest",
            axum::routing::post(|Json(b): Json<Value>| async move {
                Json(json!({"ok": true, "sha256": b["sha256"], "size": 3}))
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, wk).await;
        });

        store::cluster::upsert_worker(
            st.pool(),
            &WorkerInput {
                id: "W1".to_string(),
                name: "office".to_string(),
                url: format!("http://{addr}"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        // 普通 API-Key 没有 registry:publish 之外的问题，这里直接给一把带 publish 的 key
        let (_k, secret) =
            store::apikeys::create(st.pool(), &u.id, "t", &["registry:publish".to_string()])
                .await
                .unwrap();
        let app = router_for(&st);
        let req = Request::builder()
            .method("POST")
            .uri("/cluster/replicate")
            .header("content-type", "application/json")
            .header("authorization", auth_header(&secret));
        let (code, v) = call(
            &app,
            req.body(Body::from(
                json!({"ref": "@jia/demo", "targets": "all"}).to_string(),
            ))
            .unwrap(),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["targets"], json!(1));
        assert_eq!(v["results"][0]["ok"], json!(true));
        // 记账落地
        let targets = store::cluster::list_replica_targets(st.pool(), "@jia/demo@1.0.0")
            .await
            .unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].worker_id, "W1");
        let _ = (&row, secret);
    }

    #[tokio::test]
    async fn replicate_目标解析与校验() {
        let st = state("replicabad", "master").await;
        let u = store::users::create(st.pool(), "乙", "b@x.com", "h")
            .await
            .unwrap();
        let ns = store::namespaces::create_account(st.pool(), &u.id, "乙", "yi")
            .await
            .unwrap();
        store::artifacts::create(
            st.pool(),
            store::artifacts::NewArtifact {
                namespace_id: ns.id.clone(),
                slug: "demo".to_string(),
                kind: "skill".to_string(),
                name: "演示".to_string(),
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
        let (_k, secret) =
            store::apikeys::create(st.pool(), &u.id, "t", &["registry:publish".to_string()])
                .await
                .unwrap();
        let app = router_for(&st);

        let post = |body: Value| {
            Request::builder()
                .method("POST")
                .uri("/cluster/replicate")
                .header("content-type", "application/json")
                .header("authorization", auth_header(&secret))
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        // 没有 targets
        let (code, v) = call(&app, post(json!({"ref": "@yi/demo"}))).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(
            v["error"]["message"],
            json!("需要 targets（\"all\" 或 worker 名称/id 列表）")
        );
        // targets 格式不支持
        let (code, v) = call(&app, post(json!({"ref": "@yi/demo", "targets": 5}))).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("targets 格式不支持"));
        // 找不到 worker
        let (code, v) = call(&app, post(json!({"ref": "@yi/demo", "targets": ["nope"]}))).await;
        assert_eq!(code, SC::BAD_REQUEST, "{v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("找不到 worker"));
        // 制品不存在
        let (code, v) = call(&app, post(json!({"ref": "@yi/ghost", "targets": "all"}))).await;
        assert_eq!(code, SC::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["message"], json!("制品不存在"));
        // 没有作用域
        let (_k2, secret2) = store::apikeys::create(st.pool(), &u.id, "t2", &[])
            .await
            .unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/cluster/replicate")
            .header("content-type", "application/json")
            .header("authorization", auth_header(&secret2));
        let (code, _) = call(
            &app,
            req.body(Body::from(
                json!({"ref": "@yi/demo", "targets": "all"}).to_string(),
            ))
            .unwrap(),
        )
        .await;
        assert_eq!(code, SC::FORBIDDEN);
        let _ = secret;
    }

    #[tokio::test]
    async fn 出站_join_与_master_对话() {
        // 起一个假 master，校验 worker 侧出站调用（含 token 头）
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mk = axum::Router::new()
            .route(
                "/api/cluster/join",
                axum::routing::post(|headers: HeaderMap, Json(_): Json<Value>| async move {
                    let tok = headers
                        .get("x-ncc-cluster-token")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    Json(
                        json!({"ok": true, "master": {"id": "M", "url": "http://m"}, "token": tok}),
                    )
                }),
            )
            .route(
                "/api/cluster/heartbeat",
                axum::routing::post(|| async { Json(json!({"ok": true, "accepted": 0})) }),
            );
        tokio::spawn(async move {
            let _ = axum::serve(listener, mk).await;
        });

        let st = state("outbound", "worker").await;
        {
            let mut cfg = (*st.cfg()).clone();
            cfg.master_url = format!("http://{addr}");
            let st = AppState {
                cfg: std::sync::Arc::new(cfg),
                ..st.clone()
            };
            let out = join_once(&st).await.unwrap();
            assert_eq!(out["master"]["id"], json!("M"));
            assert_eq!(out["token"], json!("tok")); // token 头确实带上了
            let out = heartbeat_once(&st).await.unwrap();
            assert_eq!(out["ok"], json!(true));
        }
    }

    #[tokio::test]
    async fn 出站_master_报错与不可达() {
        let st = state("outbound2", "worker").await;
        // 没配 master 地址
        {
            let mut cfg = (*st.cfg()).clone();
            cfg.master_url = String::new();
            let st = AppState {
                cfg: std::sync::Arc::new(cfg),
                ..st.clone()
            };
            assert!(join_once(&st).await.is_err());
        }
        // master 回 4xx：错误 code/message 要透出来
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mk = axum::Router::new().route(
            "/api/cluster/join",
            axum::routing::post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": {"code": "not_master", "message": "本节点是 worker"}})),
                )
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, mk).await;
        });
        {
            let mut cfg = (*st.cfg()).clone();
            cfg.master_url = format!("http://{addr}");
            let st = AppState {
                cfg: std::sync::Arc::new(cfg),
                ..st.clone()
            };
            let e = join_once(&st).await.unwrap_err();
            assert!(e.contains("not_master"), "{e}");
            assert!(e.contains("本节点是 worker"), "{e}");
        }
    }

    /// 整站路由表装配：确认本族（以及同批复用的 feedback / admin）端点**真的挂上去了**，
    /// 而不是被未迁移兜底的 501 接住 —— axum 建路由时若有冲突会直接 panic。
    #[tokio::test]
    async fn 整站路由表_三族端点都已挂载() {
        let st = state("router", "master").await;
        let app = crate::router::build(&st);

        // 集群：带 content-type 的空 JSON -> 走到本族处理器，被集群 token 拦成 401
        let req = Request::builder()
            .method("POST")
            .uri("/api/cluster/join")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let (code, v) = call(&app, req).await;
        assert_eq!(code, SC::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"]["code"], json!("cluster_token_invalid"));

        // 集群总览（公开读）
        let (code, _) = call(
            &app,
            Request::builder()
                .uri("/api/cluster")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, SC::OK);

        // 反馈词表（离线可读）
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/api/feedback/kinds")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, SC::OK, "{v}");
        assert_eq!(v["statuses"].as_array().unwrap().len(), 4);

        // 治理面：没带凭据 -> 403 admin_required（不是 501）
        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/api/admin/overview")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, SC::FORBIDDEN, "{v}");
        assert_eq!(v["error"]["code"], json!("admin_required"));
    }
}
