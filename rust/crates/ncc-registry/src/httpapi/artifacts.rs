//! 制品路由：目录 / 列表 / 详情 / 下载 / 字节 / 上传 / 发布 / 修改 / 删除 / 签名。
//!
//! 三条贯穿本模块的规矩：
//!
//! 1. **字节即事实**：sha256 与清单由服务端从收到的字节算/读，不信客户端报的；
//! 2. **可见性两层**：先看条目本身（公开已发布 vs 私有草稿），再看授权；
//! 3. **私有条目给短时签名地址**：客户端拿字节时不会带 Authorization
//!    （`ncc download` 就是一次裸 GET），所以可见性通过的这一刻要发一张通行证。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::timeutil::now_unix;
use ncc_core::web;

use crate::config::Config;
use crate::httpapi::{helpers, AppState, Auth};
use crate::store;

/// 制品类型词表（与 CLI / 平台目录保持一致）。
pub const ARTIFACT_KINDS: &[&str] = &[
    "api",
    "harness",
    "hur",
    "skill",
    "mcp",
    "plugin",
    "scaffold",
    "docker-image",
    "benchmark",
    "living",
];

/// 类型的中文标签与说明（`/api/registry/kinds` 用）。
fn kind_meta(k: &str) -> (&'static str, &'static str) {
    match k {
        "api" => ("API", "可调用的服务接口"),
        "harness" => ("Harness", "按契约可装载的能力封装（含 loader/entry）"),
        "hur" => (
            "HUR",
            "Harness-Use Runtime 官方包（kind=agent 的包就是一个 Agent）",
        ),
        "skill" => ("Skill", "给 Agent 的操作手册（SKILL.md）"),
        "mcp" => ("MCP", "Model Context Protocol 服务"),
        "plugin" => ("Plugin", "宿主应用的插件"),
        "scaffold" => ("Scaffold", "项目脚手架"),
        "docker-image" => ("Docker Image", "容器镜像"),
        "benchmark" => ("Benchmark", "评测基准"),
        "living" => ("Living", "活体节点描述"),
        _ => ("", ""),
    }
}

pub fn valid_kind(k: &str) -> bool {
    ARTIFACT_KINDS.contains(&k)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/registry/kinds", get(kinds))
        .route("/registry/uploads", post(upload))
        .route("/registry", get(list).post(create_item))
        .route("/registry/", get(list).post(create_item))
        .route(
            "/registry/{id}",
            get(get_item).patch(patch_item).delete(delete_item),
        )
        .route(
            "/registry/{id}/{slug}",
            get(get_item).patch(patch_item).delete(delete_item),
        )
        .route("/registry/{id}/download", get(download))
        .route("/registry/{id}/bytes", get(bytes))
        .route("/registry/{id}/{slug}/download", get(download))
        .route("/registry/{id}/{slug}/bytes", get(bytes))
        .route("/registry/{id}/signature", put(attach_signature))
        .route("/registry/{id}/{slug}/signature", put(attach_signature))
}

/* ---------------- 引用与可见性 ---------------- */

/// 路由参数拼回引用：`@ns/slug` 会落在 `{id}/{slug}` 两段上（单段不匹配斜杠）。
fn ref_from_params(id: &str, slug: Option<&str>) -> String {
    match slug {
        Some(s) if !s.is_empty() => format!("{id}/{s}"),
        _ => id.to_string(),
    }
}

/// 命名空间写权限（owner 或成员）。
pub async fn can_manage(state: &AppState, ns_id: &str, user_id: &str) -> bool {
    helpers::can_manage(state, ns_id, user_id).await
}

/// 能否读到这份制品。
pub async fn can_read(
    state: &AppState,
    row: &store::artifacts::ArtifactRow,
    user_id: &str,
) -> bool {
    if row.is_public_published() {
        return true;
    }
    if user_id.is_empty() {
        return false;
    }
    if can_manage(state, &row.namespace_id, user_id).await {
        return true;
    }
    // 发布者把「制品获取权」授给了这个人（可按命名空间限定）
    store::grants::has(
        state.pool(),
        &row.created_by,
        user_id,
        store::grants::KIND_ARTIFACT,
        &row.namespace_id,
    )
    .await
}

async fn find_visible(
    state: &AppState,
    ref_: &str,
    user_id: &str,
) -> Result<store::artifacts::ArtifactRow, ApiError> {
    let row = store::artifacts::by_ref(state.pool(), ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("制品不存在"))?;
    if can_read(state, &row, user_id).await {
        return Ok(row);
    }
    Err(ApiError::not_found(
        "制品不存在或不可见（私有/草稿需要该命名空间成员身份）",
    ))
}

/// 短时有效的字节地址：HMAC(secret, `ref|exp`)。
fn signed_bytes_url(cfg: &Config, row: &store::artifacts::ArtifactRow, ttl_secs: i64) -> String {
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

/// 校验签名地址（过期或签名不对一律当无效）。
fn valid_bytes_sig(cfg: &Config, ref_: &str, exp: &str, sig: &str) -> bool {
    let Ok(n) = exp.parse::<i64>() else {
        return false;
    };
    if n < now_unix() || sig.is_empty() {
        return false;
    }
    let expect = ncc_core::crypto::hmac_sha256_b64url(&cfg.jwt_secret, &format!("{ref_}|{exp}"));
    ncc_core::crypto::constant_time_eq(sig.as_bytes(), expect.as_bytes())
}

/// 下载地址：公开条目给稳定地址（可缓存、可分享），私有/草稿给短时签名地址。
fn download_url(cfg: &Config, row: &store::artifacts::ArtifactRow) -> String {
    if row.storage_provider != "local" || row.blob_name.is_empty() {
        return row.storage_url.clone();
    }
    if row.is_public_published() {
        return format!(
            "{}/api/registry/{}/bytes",
            cfg.public_url.trim_end_matches('/'),
            row.ref_of()
        );
    }
    signed_bytes_url(cfg, row, 600)
}

/// 制品视图。
pub fn artifact_json(row: &store::artifacts::ArtifactRow) -> serde_json::Value {
    let mut out = json!({
        "id": row.id,
        "kind": row.kind,
        "name": row.name,
        "slug": row.slug,
        "version": row.version,
        "summary": row.summary,
        "tags": store::artifacts::tags_of(row),
        "visibility": row.visibility,
        "status": row.status,
        "manifest": web::parse_json_any(&row.manifest),
        "storage": {
            "provider": row.storage_provider,
            "url": row.storage_url,
            "sha256": row.sha256,
            "size": row.size,
        },
        "downloads": row.downloads,
        "namespace": {
            "id": row.namespace_id,
            "slug": row.ns_slug,
            "name": row.ns_name,
        },
        "ref": format!("{}@{}", row.ref_of(), row.version),
        "origin": row.origin,
        "via": {"kind": "self"},
        "createdAt": row.created_at,
        "updatedAt": row.updated_at,
    });
    if row.is_replica() {
        out["replicaOf"] = json!(row.origin_ref);
    }
    out
}

/* ---------------- 目录 ---------------- */

/// GET /api/registry/kinds
async fn kinds(State(state): State<AppState>) -> ApiResult<Response> {
    let counts = store::artifacts::kind_counts(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<_> = ARTIFACT_KINDS
        .iter()
        .map(|k| {
            let (label, desc) = kind_meta(k);
            json!({
                "kind": k,
                "label": if label.is_empty() { *k } else { label },
                "desc": desc,
                "published": counts.get(*k).copied().unwrap_or(0),
            })
        })
        .collect();
    Ok(helpers::ok_json(
        json!({"kinds": list, "total": list.len()}),
    ))
}

/// GET /api/registry?q&kind&tag&namespace&mine&page&size&sort&status
async fn list(
    State(state): State<AppState>,
    auth: Auth,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let page = web::query_i64(&uri, "page", 1);
    let size = web::query_i64(&uri, "size", 20);
    let ns_slug = web::query(&uri, "namespace").unwrap_or_default();
    let mine = web::query_bool(&uri, "mine");

    let mut opts = store::artifacts::ListOpts {
        q: web::query(&uri, "q").unwrap_or_default(),
        kind: web::query(&uri, "kind").unwrap_or_default(),
        tag: web::query(&uri, "tag").unwrap_or_default(),
        ns_slug: ns_slug.clone(),
        page,
        size,
        order_by: web::query(&uri, "sort").unwrap_or_default(),
        ..Default::default()
    };

    let user_id = auth.user_id();
    let mut ns_meta = None;
    let mut can_manage_ns = false;
    if !ns_slug.trim().is_empty() {
        if let Some(ns) = store::namespaces::by_slug(state.pool(), &ns_slug)
            .await
            .map_err(ApiError::from_db)?
        {
            if let Some(uid) = user_id.as_deref() {
                can_manage_ns = can_manage(&state, &ns.id, uid).await;
            }
            ns_meta = Some(ns);
        }
    }

    if mine {
        let Some(uid) = user_id.as_deref() else {
            return Err(ApiError::forbidden("mine=1 需要登录"));
        };
        let nss = store::namespaces::of_user(state.pool(), uid)
            .await
            .map_err(ApiError::from_db)?;
        opts.namespace_ids = nss.iter().map(|n| n.id.clone()).collect();
        if let Some(s) = web::query(&uri, "status") {
            opts.statuses = web::split_csv(&s)
                .into_iter()
                .filter(|v| matches!(v.as_str(), "draft" | "published" | "archived"))
                .collect();
        }
    } else if can_manage_ns {
        // 可管理该命名空间：不过滤可见性/状态（私有与草稿都能看到）
    } else {
        opts.public_only = true;
    }

    let res = store::artifacts::list(state.pool(), &opts)
        .await
        .map_err(ApiError::from_db)?;
    let items: Vec<_> = res.rows.iter().map(artifact_json).collect();
    let mut out = json!({
        "items": items,
        "page": page,
        "size": size,
        "total": res.total,
        "canManage": can_manage_ns,
    });
    if let Some(ns) = ns_meta {
        out["namespace"] = json!({
            "id": ns.id, "slug": ns.slug, "name": ns.name, "type": ns.ns_type,
            "visibility": ns.visibility, "owner": user_id.as_deref() == Some(ns.owner_id.as_str()),
            "createdAt": ns.created_at,
        });
    }
    Ok(helpers::ok_json(out))
}

/// GET /api/registry/{id}[/{slug}]
async fn get_item(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
) -> ApiResult<Response> {
    let ref_ = ref_from_params(&id, slug.as_deref());
    let uid = auth.user_id().unwrap_or_default();
    let row = find_visible(&state, &ref_, &uid).await?;
    Ok(helpers::ok_json(json!({ "item": artifact_json(&row) })))
}

/* ---------------- 下载 ---------------- */

/// GET /api/registry/{ref}/download
async fn download(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
) -> ApiResult<Response> {
    let ref_ = ref_from_params(&id, slug.as_deref());
    let uid = auth.user_id().unwrap_or_default();
    let row = find_visible(&state, &ref_, &uid).await?;
    let _ = store::artifacts::bump_downloads(state.pool(), &row.id).await;
    let cfg = state.cfg();
    Ok(helpers::ok_json(json!({
        "id": row.id,
        "name": row.name,
        "version": row.version,
        "namespaceSlug": row.ns_slug,
        "slug": row.slug,
        "url": download_url(cfg, &row),
        "provider": row.storage_provider,
        "sha256": row.sha256,
        "size": row.size,
        "downloads": row.downloads + 1,
        "via": {"role": "self", "nodeId": cfg.node_id},
    })))
}

/// GET /api/registry/{ref}/bytes —— 真字节。
///
/// 授权两条路：① 带凭据且读得到；② 带**签名地址**（私有条目靠它自证）。
async fn bytes(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
    uri: axum::http::Uri,
) -> ApiResult<Response> {
    let ref_ = ref_from_params(&id, slug.as_deref());
    let cfg = state.cfg();
    let some = store::artifacts::by_ref(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?;
    let Some(row) = some else {
        return Err(ApiError::not_found(
            "字节不存在（本节点与集群目录里都没有）",
        ));
    };
    let uid = auth.user_id().unwrap_or_default();
    let exp = web::query(&uri, "exp").unwrap_or_default();
    let sig = web::query(&uri, "sig").unwrap_or_default();
    if !can_read(&state, &row, &uid).await && !valid_bytes_sig(cfg, &ref_, &exp, &sig) {
        return Err(ApiError::not_found("制品不存在或不可见"));
    }

    // BYO 直链：302 过去（字节不在本节点）
    if row.storage_provider != "local" || row.blob_name.is_empty() {
        return Ok(axum::response::Redirect::temporary(&row.storage_url).into_response());
    }
    let data = state
        .blobs()
        .get(&row.blob_name)
        .map_err(|_| ApiError::not_found("字节已不在本节点（可能已被清理）"))?;

    let mut name = row.slug.clone();
    if let Some(ext) = std::path::Path::new(&row.blob_name).extension() {
        name.push('.');
        name.push_str(&ext.to_string_lossy());
    }
    let mut resp = web::bytes_response(data, "application/octet-stream", Some(&name));
    if !row.sha256.is_empty() {
        if let Ok(v) = axum::http::HeaderValue::from_str(&row.sha256) {
            resp.headers_mut().insert("x-ncc-sha256", v);
        }
    }
    Ok(resp)
}

/* ---------------- 上传 / 发布 / 修改 / 删除 ---------------- */

/// POST /api/registry/uploads —— raw body + X-Filename，响应与平台同形。
async fn upload(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth.require_scope("registry:publish")?;
    if body.is_empty() {
        return Err(ApiError::bad_request("bad_request", "请求体为空或读取失败"));
    }
    let filename = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("upload.bin");
    let mut ext = std::path::Path::new(filename)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    if ext.len() > 16 {
        ext = ext[..16].to_string();
    }
    let uid = &a.user_id;
    let short = if uid.len() > 6 {
        &uid[uid.len() - 6..]
    } else {
        uid.as_str()
    };
    let name = format!("{short}-{}{ext}", ncc_core::crypto::rand_hex(6));

    let url = state
        .blobs()
        .put(&name, &body)
        .map_err(|e| ApiError::internal(format!("写入制品字节失败: {e}")))?;
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({
            "storageUrl": url,
            "filename": name,
            "size": body.len(),
            "sha256": ncc_core::crypto::sha256_hex(&body),
        }),
    ))
}

#[derive(Debug, Deserialize, Default)]
struct CreateItemReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    kind: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    name: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    slug: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    version: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    summary: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    status: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    visibility: String,
    #[serde(default)]
    manifest: Option<serde_json::Value>,
    #[serde(
        default,
        rename = "namespaceId",
        deserialize_with = "crate::httpapi::helpers::de_str"
    )]
    namespace_id: String,
    #[serde(default)]
    storage: StorageRef,
    /// 分发规格：`"all"` 或 `["worker 名称/id"]`。给了就让这些节点也持有副本。
    /// 与 Go 的 `body.Replicate`（`*any`）同义：**不给 / null = 不分发**。
    #[serde(default)]
    replicate: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, Default)]
struct StorageRef {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    url: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    sha256: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    size: i64,
}

/// 校验 HUR 清单：制品必须自描述，否则分不清「这份签名是不是这份产物的」。
fn validate_hur_manifest(
    manifest: &serde_json::Value,
    uploaded_sha: &str,
) -> Result<(), (String, String)> {
    let art = manifest.get("hur").and_then(|h| h.get("artifact"));
    let Some(art) = art else {
        return Err((
            "bad_manifest".to_string(),
            "manifest.hur.artifact 缺失（name / sha256 / bytes）".to_string(),
        ));
    };
    let art_sha = art.get("sha256").and_then(|v| v.as_str()).unwrap_or("");
    if art_sha.trim().is_empty() {
        return Err((
            "bad_manifest".to_string(),
            "manifest.hur.artifact.sha256 不能为空".to_string(),
        ));
    }
    if !uploaded_sha.trim().is_empty() && art_sha != uploaded_sha {
        return Err((
            "digest_mismatch".to_string(),
            "上传字节的 sha256 与 manifest.hur.artifact.sha256 不一致（上传的不是清单声明的那份产物）"
                .to_string(),
        ));
    }
    Ok(())
}

/// POST /api/registry —— 发布/登记一条制品。
async fn create_item(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<CreateItemReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("registry:publish")?;
    let kind = body.kind.trim().to_string();
    let name = body.name.trim().to_string();
    if kind.is_empty() {
        return Err(ApiError::bad_request("bad_request", "kind 不能为空"));
    }
    if !valid_kind(&kind) {
        return Err(ApiError::bad_request(
            "bad_request",
            format!("未知 kind {kind}（可用 /api/registry/kinds 查看）"),
        ));
    }
    if name.is_empty() {
        return Err(ApiError::bad_request("bad_request", "name 不能为空"));
    }
    if body.storage.url.trim().is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "storage.url 不能为空（先 POST /api/registry/uploads，或提供自有直链）",
        ));
    }

    // 归属命名空间：留空用个人命名空间
    let ns = if body.namespace_id.trim().is_empty() {
        store::namespaces::personal(state.pool(), &a.user_id)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| {
                ApiError::bad_request("bad_request", "当前账号没有个人命名空间，请重新注册")
            })?
    } else {
        store::namespaces::by_id(state.pool(), body.namespace_id.trim())
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| ApiError::bad_request("bad_request", "namespace 不存在"))?
    };
    if !can_manage(&state, &ns.id, &a.user_id).await {
        return Err(ApiError::forbidden("你不是该 namespace 的 owner/成员"));
    }

    let mut slug = ncc_core::ids::slugify(&body.slug);
    if body.slug.trim().is_empty() || slug == "x" {
        slug = ncc_core::ids::slugify(&name);
    }
    if slug == "x" {
        slug = format!("item-{}", ncc_core::crypto::rand_hex(3));
    }
    if store::artifacts::exists_ns_slug(state.pool(), &ns.id, &slug)
        .await
        .map_err(ApiError::from_db)?
    {
        return Err(ApiError::conflict(
            "conflict",
            format!("该 namespace 下 slug「{slug}」已存在"),
        ));
    }

    let status = if body.status == "published" {
        "published"
    } else {
        "draft"
    };
    let visibility = if body.visibility == "private" {
        "private"
    } else {
        "public"
    };
    let version = if body.version.trim().is_empty() {
        "1.0.0".to_string()
    } else {
        body.version.trim().to_string()
    };
    let mut tags = body.tags.clone();
    tags.truncate(20);

    let mut manifest = String::new();
    if let Some(m) = body.manifest.as_ref() {
        manifest = serde_json::to_string(m)
            .map_err(|_| ApiError::bad_request("bad_manifest", "manifest 无法序列化"))?;
        if kind == "harness" {
            let h = m.get("harness");
            let has = |k: &str| {
                h.and_then(|x| x.get(k))
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            };
            if !has("loader") && !has("entry") {
                return Err(ApiError::bad_request(
                    "bad_contract",
                    "kind=harness 的 manifest 需提供 harness.loader 或 harness.entry",
                ));
            }
        }
        if kind == "hur" {
            if let Err((code, msg)) = validate_hur_manifest(m, &body.storage.sha256) {
                return Err(ApiError::bad_request(&code, msg));
            }
        }
    }

    let blob_name =
        store::artifacts::blob_name_from_url(&state.cfg().public_url, &body.storage.url);
    let provider = if blob_name.is_empty() { "url" } else { "local" };

    let row = store::artifacts::create(
        state.pool(),
        store::artifacts::NewArtifact {
            namespace_id: ns.id.clone(),
            slug,
            kind,
            name,
            version,
            summary: body.summary.trim().to_string(),
            tags,
            visibility: visibility.to_string(),
            status: status.to_string(),
            manifest,
            storage_provider: provider.to_string(),
            storage_url: body.storage.url.trim().to_string(),
            blob_name,
            sha256: body.storage.sha256.trim().to_string(),
            size: body.storage.size,
            created_by: a.user_id.clone(),
        },
    )
    .await
    .map_err(ApiError::from_db)?;

    let mut out = json!({"item": artifact_json(&row)});
    // 分发（对齐 Go 的 `createItem`）：master 是发布入口，`--replicate all|<worker>`
    // 让指定节点也留一份副本。任一目标失败只写进 `replicated[]`，不影响创建本身成功 ——
    // 「发布」与「分发」是两件事，别让分发失败把已经落库的条目说成没发出去。
    if let Some(spec) = body.replicate.as_ref().filter(|v| !v.is_null()) {
        if let Ok(targets) = store::cluster::list_workers(state.pool()).await {
            match crate::httpapi::cluster::pick_workers(&targets, Some(spec)) {
                Ok(picked) => {
                    out["replicated"] =
                        json!(crate::httpapi::cluster::fanout(&state, &row, &picked).await);
                }
                Err(e) => out["replicateError"] = json!(e),
            }
        }
    }
    Ok(helpers::ok_status(StatusCode::CREATED, out))
}

#[derive(Debug, Deserialize, Default)]
struct PatchItemReq {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    manifest: Option<serde_json::Value>,
    #[serde(default)]
    storage: Option<StorageRef>,
}

/// PATCH /api/registry/{id}[/{slug}]
async fn patch_item(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
    Json(body): Json<PatchItemReq>,
) -> ApiResult<Response> {
    let a = auth.require_scope("registry:publish")?;
    let ref_ = ref_from_params(&id, slug.as_deref());
    let row = store::artifacts::by_ref(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("制品不存在"))?;
    if !can_manage(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只有该命名空间的 owner/成员能修改"));
    }

    let mut p = store::artifacts::Patch::default();
    if let Some(v) = body.name {
        if !v.trim().is_empty() {
            p.name = Some(v.trim().to_string());
        }
    }
    if let Some(v) = body.version {
        if !v.trim().is_empty() {
            p.version = Some(v.trim().to_string());
        }
    }
    if let Some(v) = body.summary {
        p.summary = Some(v);
    }
    if let Some(v) = body.tags {
        let mut t = v;
        t.truncate(20);
        p.tags = Some(t);
    }
    if let Some(v) = body.status {
        if !matches!(v.as_str(), "draft" | "published" | "archived") {
            return Err(ApiError::bad_request(
                "bad_request",
                "status 只能是 draft/published/archived",
            ));
        }
        p.status = Some(v);
    }
    if let Some(v) = body.visibility {
        if !matches!(v.as_str(), "public" | "private") {
            return Err(ApiError::bad_request(
                "bad_request",
                "visibility 只能是 public/private",
            ));
        }
        p.visibility = Some(v);
    }
    if let Some(m) = body.manifest {
        p.manifest = Some(
            serde_json::to_string(&m)
                .map_err(|_| ApiError::bad_request("bad_manifest", "manifest 无法序列化"))?,
        );
    }
    if let Some(s) = body.storage {
        if !s.url.trim().is_empty() {
            let blob = store::artifacts::blob_name_from_url(&state.cfg().public_url, &s.url);
            p.storage_url = Some(s.url.trim().to_string());
            p.blob_name = Some(blob.clone());
            if s.size > 0 {
                p.size = Some(s.size);
            }
            if !s.sha256.trim().is_empty() {
                p.sha256 = Some(s.sha256.trim().to_string());
            }
        }
    }

    store::artifacts::patch(state.pool(), &row.id, &p)
        .await
        .map_err(ApiError::from_db)?;
    let fresh = store::artifacts::by_id(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("制品不存在"))?;
    Ok(helpers::ok_json(json!({"item": artifact_json(&fresh)})))
}

/// PUT /api/registry/{id}[/{slug}]/signature —— 附上 Minisign 签名摘要。
async fn attach_signature(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
    Json(sig): Json<serde_json::Value>,
) -> ApiResult<Response> {
    let a = auth.require_scope("registry:publish")?;
    let ref_ = ref_from_params(&id, slug.as_deref());
    let row = store::artifacts::by_ref(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("制品不存在"))?;
    if !can_manage(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只有该命名空间的 owner/成员能改签名"));
    }
    // 签名归属必须能核对：签名里的 sha256 与条目字节的 sha256 要一致，
    // 否则「这份签名」证明不了「这份产物」。
    let sig_sha = sig
        .get("sha256")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !sig_sha.is_empty() && !row.sha256.is_empty() && sig_sha != row.sha256 {
        return Err(ApiError::bad_request(
            "digest_mismatch",
            "签名里的 sha256 与制品字节的 sha256 不一致",
        ));
    }

    let mut manifest = web::parse_json_any(&row.manifest);
    if manifest.is_null() {
        manifest = json!({});
    }
    let obj = manifest.as_object_mut().ok_or_else(|| {
        ApiError::bad_request("bad_manifest", "现有 manifest 不是对象，无法附加签名")
    })?;
    obj.insert("signature".to_string(), sig);

    let p = store::artifacts::Patch {
        manifest: Some(manifest.to_string()),
        ..Default::default()
    };
    store::artifacts::patch(state.pool(), &row.id, &p)
        .await
        .map_err(ApiError::from_db)?;
    let fresh = store::artifacts::by_id(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("制品不存在"))?;
    Ok(helpers::ok_json(
        json!({"item": artifact_json(&fresh), "signed": web::parse_json_any(&fresh.manifest).get("signature").cloned().unwrap_or(serde_json::Value::Null)}),
    ))
}

/// DELETE /api/registry/{id}[/{slug}]
async fn delete_item(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
) -> ApiResult<Response> {
    let a = auth.require_scope("registry:publish")?;
    let ref_ = ref_from_params(&id, slug.as_deref());
    let row = store::artifacts::by_ref(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("制品不存在"))?;
    if !can_manage(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只有该命名空间的 owner/成员能删除"));
    }
    store::artifacts::delete(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?;
    // 字节也删：目录里没了的条目，字节留着只会变成谁也清理不掉的垃圾。
    if row.storage_provider == "local" && !row.blob_name.is_empty() {
        let _ = state.blobs().delete(&row.blob_name);
    }
    // 回收分发出去的副本（对齐 Go 的 `revokeReplicas`）：不回收的话 worker 上会留一份
    // 「源节点已经不认」的副本，客户端按目录路由过去还能下载到已删除的版本。
    let ref_ = format!("{}@{}", row.ref_of(), row.version);
    let revoked = crate::httpapi::cluster::revoke_replicas(&state, &ref_).await;
    Ok(helpers::ok_json(
        json!({"ok": true, "ref": ref_, "revoked": revoked, "id": row.id}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        // 直接构造一个够用的配置：本模块只用到 public_url / jwt_secret / node_id
        let mut c = crate::config::load().expect("默认配置可加载");
        c.public_url = "http://localhost:8282".to_string();
        c.jwt_secret = "test-secret".to_string();
        c
    }

    #[test]
    fn 签名地址可校验_过期与篡改被拒() {
        let c = cfg();
        let row = store::artifacts::ArtifactRow {
            id: "A-1".to_string(),
            namespace_id: "NS-1".to_string(),
            slug: "demo".to_string(),
            kind: "skill".to_string(),
            name: "演示".to_string(),
            version: "1.0.0".to_string(),
            summary: String::new(),
            tags: "[]".to_string(),
            visibility: "private".to_string(),
            status: "published".to_string(),
            manifest: String::new(),
            storage_provider: "local".to_string(),
            storage_url: String::new(),
            blob_name: "b".to_string(),
            sha256: String::new(),
            size: 0,
            origin: "local".to_string(),
            origin_ref: String::new(),
            created_by: "U-1".to_string(),
            downloads: 0,
            created_at: None,
            updated_at: None,
            ns_slug: Some("zhangsan".to_string()),
            ns_name: Some("张三".to_string()),
        };
        let url = signed_bytes_url(&c, &row, 600);
        let q = url.split('?').nth(1).unwrap();
        let mut exp = String::new();
        let mut sig = String::new();
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap();
            match k {
                "exp" => exp = v.to_string(),
                "sig" => sig = v.to_string(),
                _ => {}
            }
        }
        assert!(valid_bytes_sig(&c, "@zhangsan/demo", &exp, &sig));
        assert!(!valid_bytes_sig(&c, "@zhangsan/other", &exp, &sig));
        assert!(!valid_bytes_sig(&c, "@zhangsan/demo", "1", &sig)); // 已过期
        assert!(!valid_bytes_sig(&c, "@zhangsan/demo", &exp, "伪造"));
        // 私有条目给签名地址，公开已发布条目给稳定地址
        assert!(download_url(&c, &row).contains("sig="));
    }

    #[test]
    fn hur_清单校验() {
        assert!(validate_hur_manifest(&json!({}), "").is_err());
        assert!(validate_hur_manifest(&json!({"hur": {"artifact": {"sha256": "a"}}}), "a").is_ok());
        let e =
            validate_hur_manifest(&json!({"hur": {"artifact": {"sha256": "a"}}}), "b").unwrap_err();
        assert_eq!(e.0, "digest_mismatch");
    }

    #[test]
    fn 类型词表() {
        assert!(valid_kind("hur"));
        assert!(!valid_kind("unknown"));
        assert_eq!(kind_meta("hur").0, "HUR");
    }
}
