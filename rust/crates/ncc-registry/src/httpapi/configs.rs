//! NCC Config：团队的网络 / 基础设施 / Agent 配置托管（HTTP 层）。
//!
//! 原实现：`ncc-registry/httpapi/configs.go`。与制品的分工别混：
//!
//! ```text
//! 制品   可分发的能力包：字节进 blob，可 fan-out 到 worker、公开即可匿名下载。
//! 配置   团队的权威数据：默认**私有**，就地改、每次写入留一版历史、可回滚，
//!        不参与 fan-out（只在被指向的那个节点上维护）。
//! ```
//!
//! 本族最容易写错的是三条判定，缺一不可：
//!
//! ```text
//! 读公开配置       -> 谁都能读（visibility=public 且 status=active）
//! 读非公开配置     -> 需要 config:read 作用域 **且**（命名空间成员 **或** 拿到 config 授权）
//! 写（含改内容）   -> 需要 config:write 作用域 **且** 是命名空间成员
//! ```
//!
//! 敏感值：`secret=true` 的内容**落库前**用 `state.seal()`（AES-256-GCM）加密，
//! 默认读取只回校验和与大小（打码），要明文必须显式 `?reveal=1`；且 secret 配置
//! 强制私有 —— 公开一条加密配置没有意义，只会让人误以为它是安全的。
//!
//! 刻意的取舍：
//!
//! * 请求体走 `Bytes` + 手工 `serde_json`：先判作用域再判 body，与 Go 里
//!   `requireScope` 中间件早于 `ShouldBindJSON` 的顺序一致，非法 body 也回
//!   `400 bad_request 请求体格式错误`（而不是 axum `Json<T>` 默认的 422）。
//! * **注释或回滚都不改写历史**：回滚是把旧内容作为**新版本**写回去，所以
//!   「谁在什么时候回滚过」同样留痕。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::web;

use crate::httpapi::{helpers, AppState, Auth};
use crate::store;

const CONFIG_MAX_PER_NAMESPACE: i64 = 500;
const MAX_CONFIG_TAGS: usize = 12;
const MAX_CONFIG_TAG_LEN: usize = 24;
const CONFIG_MAX_BYTES: usize = 128 * 1024;

/// 配置类型目录（顺序即展示顺序）→ [id, 中文, 英文, 中文说明, 英文说明]。
const CONFIG_KINDS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "network",
        "网络",
        "Network",
        "网段 / VLAN / 路由 / DNS / VPN / 防火墙策略",
        "Subnets, VLANs, routes, DNS, VPN, firewall policy",
    ),
    (
        "gateway",
        "网关与入口",
        "Gateway & ingress",
        "反向代理 / 域名与证书 / 对外入口",
        "Reverse proxy, domains and certificates, public ingress",
    ),
    (
        "infra",
        "基础设施",
        "Infrastructure",
        "主机 / 存储 / 集群 / 虚拟化参数",
        "Hosts, storage, clusters, virtualization parameters",
    ),
    (
        "registry",
        "制品源与镜像",
        "Registries & mirrors",
        "镜像源 / 制品源 / 代理与上游",
        "Image and artifact registries, proxies, upstreams",
    ),
    (
        "agent",
        "Agent 与模型",
        "Agents & models",
        "模型端点 / 工具清单 / 提示与人格参数",
        "Model endpoints, tool lists, prompt and persona settings",
    ),
    (
        "ci",
        "流水线与构建",
        "CI & build",
        "流水线参数 / 构建与发布变量",
        "Pipeline parameters, build and release variables",
    ),
    (
        "observability",
        "监控与告警",
        "Observability",
        "采集 / 看板 / 告警路由",
        "Collection, dashboards, alert routing",
    ),
    (
        "security",
        "安全与凭据",
        "Security & credentials",
        "凭据 / 证书 / 访问策略（建议 --secret）",
        "Credentials, certificates, access policy (use --secret)",
    ),
    (
        "app",
        "应用参数",
        "Application",
        "业务应用的运行参数",
        "Runtime settings for business applications",
    ),
    (
        "other",
        "其他",
        "Other",
        "不属于以上分类的配置",
        "Anything else",
    ),
];

const CONFIG_FORMATS: &[&str] = &["json", "yaml", "toml", "env", "ini", "text", "shell"];
const CONFIG_ENVS: &[&str] = &["any", "dev", "staging", "prod"];

fn valid_config_kind(k: &str) -> bool {
    CONFIG_KINDS.iter().any(|row| row.0 == k)
}

fn valid_config_env(e: &str) -> bool {
    CONFIG_ENVS.contains(&e)
}

fn valid_config_format(f: &str) -> bool {
    CONFIG_FORMATS.contains(&f)
}

fn valid_config_status(s: &str) -> bool {
    matches!(s, "active" | "archived")
}

/// 类型的中英标签（未知类型回原样）。
fn config_kind_label(k: &str, lang: &str) -> String {
    match CONFIG_KINDS.iter().find(|row| row.0 == k) {
        Some(row) => {
            if lang == "en" {
                row.2.to_string()
            } else {
                row.1.to_string()
            }
        }
        None => k.to_string(),
    }
}

/// 与制品 slug 同族（引用写作 `@ns/slug`，别引入第二套规则）。
fn valid_config_slug(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 2 || b.len() > 48 {
        return false;
    }
    for (i, raw) in b.iter().enumerate() {
        let c = *raw as char;
        let ok = (c >= 'a' && c <= 'z')
            || (c >= '0' && c <= '9')
            || (c == '-' && i > 0 && i < b.len() - 1);
        if !ok {
            return false;
        }
    }
    true
}

fn config_format_ext(f: &str) -> String {
    match f {
        "env" => ".env".to_string(),
        "text" => ".txt".to_string(),
        "shell" => ".sh".to_string(),
        _ => format!(".{f}"),
    }
}

/// 建议落盘名：`@team/network` + prod → `team-network.prod.yaml`。
fn bundle_filename(r: &store::configs::ConfigRow, env: &str) -> String {
    let mut base = format!("{}-{}", r.ns_slug.clone().unwrap_or_default(), r.slug);
    if !r.environment.is_empty() && r.environment != "any" {
        base.push('.');
        base.push_str(&r.environment);
    } else if !env.is_empty() {
        base.push('.');
        base.push_str(env);
    }
    base + &config_format_ext(&r.format)
}

fn checksum_of(s: &str) -> String {
    ncc_core::crypto::sha256_hex(s.as_bytes())
}

/// 按 Unicode 字符截断（Go 的 `[]rune` 语义，避免把一个汉字切成半个）。
fn truncate_chars(s: &str, max: usize) -> String {
    let v: Vec<char> = s.chars().collect();
    if v.len() <= max {
        s.to_string()
    } else {
        v[..max].iter().collect()
    }
}

/* ---------------- 路由 ---------------- */

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/configs/kinds", get(config_kind_catalog))
        .route("/configs/bundle", get(config_bundle))
        .route("/configs", get(list_configs).post(create_config))
        .route("/configs/", get(list_configs).post(create_config))
        // 引用有两种形态：C-…（单段）与 @ns/slug（两段），与制品同一套写法。
        .route(
            "/configs/{id}",
            get(get_config).patch(update_config).delete(delete_config),
        )
        .route(
            "/configs/{id}/{slug}",
            get(get_config).patch(update_config).delete(delete_config),
        )
        .route("/configs/{id}/revisions", get(config_revisions))
        .route("/configs/{id}/{slug}/revisions", get(config_revisions))
        .route("/configs/{id}/rollback", post(rollback_config))
        .route("/configs/{id}/{slug}/rollback", post(rollback_config))
}

/// 本族没有顶层公开页（公开配置仍走 `/api/configs/...`）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
}

/* ---------------- 序列化 ---------------- */

/// 配置视图。
///
/// `reveal=false` 时**绝不下发明文**（哪怕调用方有权限）：打码是默认，不是异常。
/// `reveal` 由调用方显式给出：读接口看 `?reveal=1`，**写接口直接给 true** ——
/// 写的人刚刚提供了内容，把回执打码只会让 CLI 无法回显「写进去的是什么」。
fn config_json(
    state: &AppState,
    row: &store::configs::ConfigRow,
    can_read: bool,
    can_write: bool,
    reveal: bool,
) -> Value {
    let mut out = json!({
        "id": row.id,
        "slug": row.slug,
        "ref": row.ref_of(),
        "name": row.name,
        "kind": row.kind,
        "kindLabel": config_kind_label(&row.kind, "zh"),
        "environment": row.environment,
        "format": row.format,
        "summary": row.summary,
        "tags": store::parse_list(&row.tags),
        "visibility": row.visibility,
        "status": row.status,
        "secret": row.secret,
        "encrypted": row.secret,
        "revision": row.revision,
        "checksum": row.checksum,
        "size": row.size,
        "namespace": {"id": row.namespace_id, "slug": row.ns_slug, "name": row.ns_name},
        "owner": {"id": row.owner_id, "name": row.owner_name},
        "createdBy": row.created_by,
        "updatedBy": row.updated_by,
        "createdAt": row.created_at,
        "updatedAt": row.updated_at,
        "canRead": can_read,
        "canWrite": can_write,
    });
    if !can_read {
        return out;
    }
    if !reveal {
        out["masked"] = json!(true);
        out["content"] = Value::Null;
        out["hint"] = json!("内容默认打码：加 ?reveal=1（CLI: --reveal）取明文");
        return out;
    }
    match state.seal().open(&row.content) {
        Ok(plain) => {
            out["masked"] = json!(false);
            out["content"] = json!(plain);
            // 校验和按**明文**算：Agent 拿到明文后能自己复核「落地的就是服务端记录的那份」。
            out["contentChecksum"] = json!(checksum_of(&plain));
        }
        Err(e) => {
            out["masked"] = json!(true);
            out["content"] = Value::Null;
            out["error"] = json!(e);
        }
    }
    out
}

/// 按需加密（secret=false 时原样存）。
fn seal_content(state: &AppState, plain: &str, secret: bool) -> ApiResult<String> {
    if !secret {
        return Ok(plain.to_string());
    }
    state.seal().seal(plain).map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "no_secret_key",
            "本节点没有可用的加密密钥，无法保存敏感配置",
        )
    })
}

/// 解密（非密文原样返回，见 secretbox 的兼容说明）。
fn open_content(state: &AppState, stored: &str) -> Result<String, String> {
    state.seal().open(stored)
}

/* ---------------- 权限判定 ---------------- */

/// 读权限（作用域由处理器的 `require_scope` / 显式 `allow` 管，这里只管「归属/授权」）。
async fn can_read_config(state: &AppState, row: &store::configs::ConfigRow, user_id: &str) -> bool {
    if row.is_public_active() {
        return true;
    }
    if user_id.is_empty() {
        return false;
    }
    if helpers::can_manage(state, &row.namespace_id, user_id).await {
        return true;
    }
    // 归属者把「配置读取权」授给了这个人（可按命名空间限定）
    store::grants::has(
        state.pool(),
        &row.owner_id.clone().unwrap_or_default(),
        user_id,
        store::grants::KIND_CONFIG,
        &row.namespace_id,
    )
    .await
}

/// 写权限 = 命名空间成员（配置是团队资产，写权限跟成员身份绑定，
/// 外部只有「读」的授权，不给写）。
async fn can_write_config(
    state: &AppState,
    row: &store::configs::ConfigRow,
    user_id: &str,
) -> bool {
    !user_id.is_empty() && helpers::can_manage(state, &row.namespace_id, user_id).await
}

/// 路由参数拼回引用：`@ns/slug` 会落在 `{id}/{slug}` 两段上（单段不匹配斜杠）。
fn ref_from_params(id: &str, slug: Option<&str>) -> String {
    match slug {
        Some(s) if !s.is_empty() => format!("{id}/{s}"),
        _ => id.to_string(),
    }
}

/// 按引用取配置：`C-…` id 或 `@ns/slug`。
async fn find_config(state: &AppState, ref_: &str) -> ApiResult<Option<store::configs::ConfigRow>> {
    store::configs::by_ref(state.pool(), ref_)
        .await
        .map_err(ApiError::from_db)
}

/// 解析写入落到哪个命名空间（为空 = 创建者的个人空间）。
async fn ticket_namespace(
    state: &AppState,
    user_id: &str,
    slug: &str,
) -> ApiResult<store::namespaces::Namespace> {
    let slug = slug.trim();
    if slug.is_empty() {
        return store::namespaces::personal(state.pool(), user_id)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| {
                ApiError::bad_request("bad_request", "当前账号没有个人命名空间，请重新注册")
            });
    }
    let ns = store::namespaces::by_slug(state.pool(), slug)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::bad_request("bad_request", format!("namespace {slug} 不存在")))?;
    if !helpers::can_manage(state, &ns.id, user_id).await {
        return Err(ApiError::forbidden("你不是该 namespace 的 owner/成员"));
    }
    Ok(ns)
}

fn reveal_asked(uri: &Uri) -> bool {
    matches!(
        web::query(uri, "reveal").as_deref(),
        Some("1") | Some("true")
    )
}

fn bad_body() -> ApiError {
    ApiError::bad_request("bad_request", "请求体格式错误")
}

/// 解析 JSON 请求体；失败回与 Go 同款的 400（axum 的 `Json<T>` 会回 422）。
fn parse_body<T: serde::de::DeserializeOwned>(b: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(b).map_err(|_| bad_body())
}

/* ---------------- 目录 ---------------- */

/// GET /api/configs/kinds —— 配置类型 / 格式 / 环境目录（公开）。
async fn config_kind_catalog(State(state): State<AppState>) -> ApiResult<Response> {
    let counts = store::configs::kind_counts(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    let kinds: Vec<Value> = CONFIG_KINDS
        .iter()
        .map(|row| {
            json!({
                "kind": row.0,
                "label": row.1,
                "en": row.2,
                "desc": row.3,
                "descEn": row.4,
                "count": counts.get(row.0).copied().unwrap_or(0),
            })
        })
        .collect();
    let env_counts = store::configs::env_counts(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    let publics = store::configs::count_public(state.pool())
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_json(json!({
        "kinds": kinds,
        "formats": CONFIG_FORMATS,
        "envs": CONFIG_ENVS,
        "envCounts": env_counts,
        "public": publics,
        // 计数口径写清楚：下面这些数字都是**公开且 active** 的配置，不含私有/归档。
        "countScope": "public+active",
        "limits": {"perNamespace": CONFIG_MAX_PER_NAMESPACE, "bytes": CONFIG_MAX_BYTES, "tags": MAX_CONFIG_TAGS},
        "access": {
            "readPublic": "公开且 active 的配置谁都能读",
            "readPrivate": "非公开配置需要 config:read 作用域，且是命名空间成员或拿到 config 授权",
            "write": "写入需要 config:write 作用域，且是命名空间成员",
            "secretAtRest": "secret=true 的内容在本节点静态加密（AES-256-GCM），读取默认打码",
            "grantKindHint": "授权：ncc grant set --user @某人 --kind config [--ns @团队]",
            "ticketScopeHint": "给 Agent 的长效凭据：ncc registry ticket create --scopes config:read,config:write",
        },
    })))
}

/// GET /api/configs?namespace=&kind=&env=&tag=&q=&mine=&status=&secrets=&page=&size=
///
/// 可见性规则：匿名只看「公开 + active」；带凭据时额外看到
///  1. 我（owner/member）命名空间里的全部配置；
///  2. 把 config 授权给我的那些人所在命名空间的配置。
///
/// 返回里每条都带 canRead/canWrite，前端不用猜。
async fn list_configs(State(state): State<AppState>, auth: Auth, uri: Uri) -> ApiResult<Response> {
    let page = web::query_i64(&uri, "page", 1);
    let size = web::query_i64(&uri, "size", 20);
    let ns_slug = web::query(&uri, "namespace").unwrap_or_default();
    let kind = web::query(&uri, "kind").unwrap_or_default();
    let env = web::query(&uri, "env").unwrap_or_default();
    let mut opts = store::configs::ListOpts {
        ns_slug: ns_slug.clone(),
        kind: kind.clone(),
        env: env.clone(),
        tag: web::query(&uri, "tag").unwrap_or_default(),
        q: web::query(&uri, "q").unwrap_or_default(),
        page,
        size,
        ..Default::default()
    };
    if !kind.trim().is_empty() && !valid_config_kind(kind.trim()) {
        return Err(ApiError::bad_request(
            "bad_request",
            format!("未知配置类型: {}", kind.trim()),
        ));
    }
    if !env.trim().is_empty() && !valid_config_env(env.trim()) {
        return Err(ApiError::bad_request(
            "bad_request",
            format!("未知环境: {}（可选 any|dev|staging|prod）", env.trim()),
        ));
    }
    if web::query(&uri, "secrets").as_deref() == Some("1") {
        opts.secrets_only = true;
    }

    let mine = web::query(&uri, "mine").as_deref() == Some("1");
    let scoped = !ns_slug.trim().is_empty() && ns_slug.trim() != "-";
    let user_id = auth.user_id();

    if mine {
        let Some(uid) = user_id.as_deref() else {
            return Err(ApiError::unauthorized("mine=1 需要登录或凭据"));
        };
        let nss = store::namespaces::of_user(state.pool(), uid)
            .await
            .map_err(ApiError::from_db)?;
        opts.namespace_ids = nss.iter().map(|n| n.id.clone()).collect();
        if opts.namespace_ids.is_empty() {
            return Ok(helpers::ok_json(json!({
                "configs": [], "total": 0, "page": page, "size": size, "canManage": false,
            })));
        }
    } else if scoped {
        // 指定命名空间：给它一个统一的判定 —— 匿名与无权限者只看到公开配置
        let slug = ns_slug.trim();
        let ns = store::namespaces::by_slug(state.pool(), slug)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| ApiError::not_found(format!("命名空间不存在: {slug}")))?;
        let mut granted = false;
        if let Some(uid) = user_id.as_deref() {
            if !helpers::can_manage(&state, &ns.id, uid).await {
                granted = store::grants::has(
                    state.pool(),
                    &ns.owner_id,
                    uid,
                    store::grants::KIND_CONFIG,
                    &ns.id,
                )
                .await
                    || store::grants::has(
                        state.pool(),
                        &ns.owner_id,
                        uid,
                        store::grants::KIND_CONFIG,
                        "",
                    )
                    .await;
            }
        }
        if !can_manage_opt(&state, &ns.id, user_id.as_deref()).await && !granted {
            opts.public_only = true;
        }
    } else if let Some(uid) = user_id.as_deref() {
        // 全局视图：公开 + 我的空间 + 被授权者
        opts.visible = true;
        if let Ok(nss) = store::namespaces::of_user(state.pool(), uid).await {
            opts.namespace_ids = nss.iter().map(|n| n.id.clone()).collect();
        }
        opts.granted_owners =
            store::grants::granted_owners(state.pool(), uid, store::grants::KIND_CONFIG).await;
    } else {
        opts.public_only = true;
    }
    if let Some(raw) = web::query(&uri, "status") {
        if !raw.trim().is_empty() {
            opts.statuses = web::split_csv(&raw)
                .into_iter()
                .filter(|v| valid_config_status(v))
                .collect();
        }
    }

    let (rows, total) = store::configs::list(state.pool(), &opts)
        .await
        .map_err(ApiError::from_db)?;
    let uid = user_id.unwrap_or_default();
    let reveal = reveal_asked(&uri);
    let mut list = Vec::with_capacity(rows.len());
    for row in &rows {
        let can_read = can_read_config(&state, row, &uid).await;
        let can_write = can_write_config(&state, row, &uid).await;
        list.push(config_json(&state, row, can_read, can_write, reveal));
    }
    let kind_counts = store::configs::kind_counts(state.pool())
        .await
        .unwrap_or_default();
    Ok(helpers::ok_json(json!({
        "configs": list, "page": page, "size": size, "total": total,
        "kindCounts": kind_counts, "mine": mine,
    })))
}

async fn can_manage_opt(state: &AppState, ns_id: &str, user_id: Option<&str>) -> bool {
    match user_id {
        Some(uid) => helpers::can_manage(state, ns_id, uid).await,
        None => false,
    }
}

/// GET /api/configs/{id}[/{slug}]?revision=&reveal= —— ref 为 `C-…` id 或 `@ns/slug`。
/// 非公开配置要求 config:read 作用域（节点令牌/API-Key 都按作用域判定）。
async fn get_config(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
    uri: Uri,
) -> ApiResult<Response> {
    let ref_ = ref_from_params(&id, slug.as_deref());
    let row = find_config(&state, &ref_)
        .await?
        .ok_or_else(|| ApiError::not_found("配置不存在"))?;
    let uid = auth.user_id().unwrap_or_default();
    let can_read = can_read_config(&state, &row, &uid).await;
    if !can_read {
        return Err(ApiError::not_found(
            "配置不存在或不可读（私有配置需要 config:read + 成员身份或 config 授权）",
        ));
    }
    if row.visibility != "public" && !auth.allow("config:read") {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "scope_required",
            "读取非公开配置需要作用域 config:read",
        ));
    }
    let reveal = reveal_asked(&uri);
    let can_write = can_write_config(&state, &row, &uid).await;

    // 指定历史版本：只回那一版（同样受 reveal 约束）
    if let Some(rev) = web::query(&uri, "revision") {
        if !rev.trim().is_empty() {
            let n: i64 = rev
                .trim()
                .parse()
                .map_err(|_| ApiError::bad_request("bad_request", "revision 需要是数字"))?;
            let r = store::configs::find_revision(state.pool(), &row.id, n)
                .await
                .map_err(ApiError::from_db)?
                .ok_or_else(|| ApiError::not_found("该版本不存在"))?;
            let mut out = config_json(&state, &row, can_read, can_write, reveal);
            out["revisionRequested"] = json!(n);
            out["revisionMeta"] = json!({
                "revision": r.revision, "note": r.note, "authorName": r.author_name,
                "createdAt": r.created_at, "checksum": r.checksum, "size": r.size,
            });
            if reveal {
                match open_content(&state, &r.content) {
                    Ok(plain) => {
                        out["content"] = json!(plain);
                        out["masked"] = json!(false);
                        out["contentChecksum"] = json!(checksum_of(&plain));
                    }
                    Err(e) => {
                        out["error"] = json!(e);
                    }
                }
            } else {
                out["content"] = Value::Null;
                out["masked"] = json!(true);
            }
            return Ok(helpers::ok_json(json!({"config": out})));
        }
    }
    Ok(helpers::ok_json(json!({
        "config": config_json(&state, &row, can_read, can_write, reveal),
    })))
}

/* ---------------- 写 ---------------- */

/// 空字符串容忍 JSON `null`（Go 的非指针 string 字段对 null 就是留零值）。
fn de_str<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

fn de_str_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

fn de_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(false))
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ConfigBody {
    #[serde(default, deserialize_with = "de_str")]
    namespace: String,
    #[serde(default, deserialize_with = "de_str")]
    slug: String,
    #[serde(default, deserialize_with = "de_str")]
    name: String,
    #[serde(default, deserialize_with = "de_str")]
    kind: String,
    #[serde(default, deserialize_with = "de_str")]
    environment: String,
    #[serde(default, deserialize_with = "de_str")]
    format: String,
    #[serde(default, deserialize_with = "de_str")]
    summary: String,
    #[serde(default, deserialize_with = "de_str_list")]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "de_str")]
    visibility: String,
    #[serde(default, deserialize_with = "de_str")]
    status: String,
    #[serde(default, deserialize_with = "de_bool")]
    secret: bool,
    #[serde(default, deserialize_with = "de_str")]
    content: String,
    #[serde(default, deserialize_with = "de_str")]
    note: String,
}

/// 校验并补齐默认值；返回错误消息（空 = 通过）。
fn normalize_config_body(mut in_: ConfigBody) -> (ConfigBody, String) {
    in_.slug = in_.slug.trim().to_lowercase();
    in_.name = in_.name.trim().to_string();
    if in_.name.is_empty() {
        in_.name = in_.slug.clone();
    }
    if !valid_config_slug(&in_.slug) {
        return (
            in_,
            "配置 slug 需为 2-48 位小写字母、数字或连字符".to_string(),
        );
    }
    in_.name = truncate_chars(&in_.name, 80);

    in_.kind = in_.kind.trim().to_string();
    if in_.kind.is_empty() {
        in_.kind = "other".to_string();
    }
    if !valid_config_kind(&in_.kind) {
        let msg = format!("未知配置类型: {}", in_.kind);
        return (in_, msg);
    }
    in_.environment = in_.environment.trim().to_string();
    if in_.environment.is_empty() {
        in_.environment = "any".to_string();
    }
    if !valid_config_env(&in_.environment) {
        let msg = format!("未知环境: {}（可选 any|dev|staging|prod）", in_.environment);
        return (in_, msg);
    }
    in_.format = in_.format.trim().to_string();
    if in_.format.is_empty() {
        in_.format = "text".to_string();
    }
    if !valid_config_format(&in_.format) {
        let msg = format!("未知格式: {}", in_.format);
        return (in_, msg);
    }
    in_.visibility = in_.visibility.trim().to_string();
    if in_.visibility.is_empty() {
        in_.visibility = "private".to_string();
    }
    if in_.visibility != "private" && in_.visibility != "public" {
        return (in_, "可见性只能是 private 或 public".to_string());
    }
    in_.status = in_.status.trim().to_string();
    if in_.status.is_empty() {
        in_.status = "active".to_string();
    }
    if !valid_config_status(&in_.status) {
        return (in_, "状态只能是 active 或 archived".to_string());
    }
    if in_.secret && in_.visibility == "public" {
        return (
            in_,
            "含敏感值的配置不能公开（去掉 public，或把 secret 关掉）".to_string(),
        );
    }
    in_.summary = truncate_chars(&in_.summary, 200);
    in_.note = truncate_chars(&in_.note, 200);
    if in_.content.len() > CONFIG_MAX_BYTES {
        return (in_, "配置内容超过上限（128 KB）".to_string());
    }
    let mut tags: Vec<String> = Vec::new();
    for t in &in_.tags {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        if tags.len() >= MAX_CONFIG_TAGS {
            break;
        }
        tags.push(truncate_chars(t, MAX_CONFIG_TAG_LEN));
    }
    in_.tags = tags;
    (in_, String::new())
}

/// POST /api/configs（需要 config:write + 命名空间成员身份）
async fn create_config(
    State(state): State<AppState>,
    auth: Auth,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth.require_scope("config:write")?;
    let body: ConfigBody = parse_body(&body)?;
    let (body, msg) = normalize_config_body(body);
    if !msg.is_empty() {
        return Err(ApiError::bad_request("bad_request", msg));
    }
    let ns = ticket_namespace(&state, &a.user_id, &body.namespace).await?;
    // Go 里这两处刻意忽略计数/存在性查询的错误（查库失败不该拦住写入）：
    // 保持一致，出错时按 0 / false 继续。
    if store::configs::count_in_namespace(state.pool(), &ns.id)
        .await
        .unwrap_or(0)
        >= CONFIG_MAX_PER_NAMESPACE
    {
        return Err(ApiError::bad_request(
            "bad_request",
            "该命名空间的配置数量已达上限",
        ));
    }
    if store::configs::slug_exists(state.pool(), &ns.id, &body.slug)
        .await
        .unwrap_or(false)
    {
        return Err(ApiError::conflict(
            "conflict",
            format!(
                "配置 @{}/{} 已存在（改 slug，或直接 PATCH 更新它）",
                ns.slug, body.slug
            ),
        ));
    }
    let sealed = seal_content(&state, &body.content, body.secret)?;
    let input = store::configs::ConfigInput {
        namespace_id: ns.id.clone(),
        slug: body.slug.clone(),
        name: body.name.clone(),
        kind: body.kind.clone(),
        environment: body.environment.clone(),
        format: body.format.clone(),
        summary: body.summary.clone(),
        tags: body.tags.clone(),
        visibility: body.visibility.clone(),
        status: body.status.clone(),
        secret: body.secret,
        content: sealed,
        checksum: checksum_of(&body.content),
        size: body.content.len() as i64,
        note: body.note.clone(),
        author_id: a.user_id.clone(),
        author_name: a.email.clone(),
    };
    let id = match store::configs::create(state.pool(), &input).await {
        Ok(id) => id,
        Err(e) => {
            if is_unique_violation(&e) {
                return Err(ApiError::conflict("conflict", "配置已存在"));
            }
            tracing::error!("创建配置失败: {e}");
            return Err(ApiError::internal("创建配置失败"));
        }
    };
    let full = store::configs::by_id(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::internal("创建配置失败"))?;
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({"config": config_json(&state, &full, true, true, true), "created": true}),
    ))
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.to_string().to_lowercase().contains("unique")
}

/// PATCH 的请求体。
///
/// 刻意不内嵌 `ConfigBody`：`secret` 与 `content` 都要能区分「没传」与「传了假值」，
/// 所以用 `Option`。共用一套 struct 会让「只想改标签」的请求意外把 secret 关掉。
#[derive(Debug, Default, Deserialize)]
struct ConfigPatchBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    secret: Option<bool>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// PATCH /api/configs/{id}[/{slug}]
///
/// 带 content 的更新会**追加一个版本**；只改元数据（标签 / 归档 / 可见性）不加版本 ——
/// 这样「历史」记录的才是真正的内容变化。
async fn update_config(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth.require_scope("config:write")?;
    let ref_ = ref_from_params(&id, slug.as_deref());
    let row = find_config(&state, &ref_)
        .await?
        .ok_or_else(|| ApiError::not_found("配置不存在"))?;
    if !can_write_config(&state, &row, &a.user_id).await {
        return Err(ApiError::forbidden("你不是该命名空间的成员，无法修改配置"));
    }
    let b: ConfigPatchBody = parse_body(&body)?;

    // 用现有值补齐，再整体校验一遍（避免「只传一个字段」绕过合法性检查）
    let mut merged = ConfigBody {
        slug: row.slug.clone(),
        name: row.name.clone(),
        kind: row.kind.clone(),
        environment: row.environment.clone(),
        format: row.format.clone(),
        summary: row.summary.clone(),
        tags: store::parse_list(&row.tags),
        visibility: row.visibility.clone(),
        status: row.status.clone(),
        secret: row.secret,
        ..Default::default()
    };
    if let Some(v) = b.name {
        merged.name = v;
    }
    if let Some(v) = b.kind {
        merged.kind = v;
    }
    if let Some(v) = b.environment {
        merged.environment = v;
    }
    if let Some(v) = b.format {
        merged.format = v;
    }
    if let Some(v) = b.summary {
        merged.summary = v;
    }
    if let Some(v) = b.tags {
        merged.tags = v;
    }
    if let Some(v) = b.visibility {
        merged.visibility = v;
    }
    if let Some(v) = b.status {
        merged.status = v;
    }
    if let Some(v) = b.note {
        merged.note = v;
    }
    if let Some(v) = b.secret {
        merged.secret = v;
    }
    let (merged, msg) = normalize_config_body(merged);
    if !msg.is_empty() {
        return Err(ApiError::bad_request("bad_request", msg));
    }

    if b.content.is_none() {
        // 只改元数据：不动内容，也不加版本
        let mut p = store::configs::MetaPatch {
            name: merged.name.clone(),
            kind: merged.kind.clone(),
            environment: merged.environment.clone(),
            format: merged.format.clone(),
            summary: merged.summary.clone(),
            tags: store::marshal_list(&merged.tags),
            visibility: merged.visibility.clone(),
            status: merged.status.clone(),
            updated_by: a.user_id.clone(),
            secret: None,
            content: None,
        };
        if b.secret.is_some() {
            // 打开 secret 时把现有明文重新加密；关掉时保留原文（解密后原样存回）
            let plain = open_content(&state, &row.content).map_err(ApiError::internal)?;
            let sealed = seal_content(&state, &plain, merged.secret)?;
            p.secret = Some(merged.secret);
            p.content = Some(sealed);
        }
        if let Err(e) = store::configs::patch(state.pool(), &row.id, &p).await {
            tracing::error!("更新配置元数据失败: {e}");
            return Err(ApiError::internal("更新失败"));
        }
        let fresh = store::configs::by_id(state.pool(), &row.id)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| ApiError::internal("更新失败"))?;
        return Ok(helpers::ok_json(json!({
            "config": config_json(&state, &fresh, true, true, true),
            "revisionAdded": false,
        })));
    }

    let plain = b.content.unwrap_or_default();
    if plain.len() > CONFIG_MAX_BYTES {
        return Err(ApiError::bad_request(
            "bad_request",
            "配置内容超过上限（128 KB）",
        ));
    }
    let sealed = seal_content(&state, &plain, merged.secret)?;
    let input = store::configs::ConfigInput {
        namespace_id: row.namespace_id.clone(),
        slug: merged.slug.clone(),
        name: merged.name.clone(),
        kind: merged.kind.clone(),
        environment: merged.environment.clone(),
        format: merged.format.clone(),
        summary: merged.summary.clone(),
        tags: merged.tags.clone(),
        visibility: merged.visibility.clone(),
        status: merged.status.clone(),
        secret: merged.secret,
        content: sealed,
        checksum: checksum_of(&plain),
        size: plain.len() as i64,
        note: merged.note.clone(),
        author_id: a.user_id.clone(),
        author_name: a.email.clone(),
    };
    if let Err(e) = store::configs::update(state.pool(), &row.id, &input).await {
        tracing::error!("更新配置失败: {e}");
        return Err(ApiError::internal("更新失败"));
    }
    let fresh = store::configs::by_id(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::internal("更新失败"))?;
    Ok(helpers::ok_json(json!({
        "config": config_json(&state, &fresh, true, true, true),
        "revisionAdded": true,
    })))
}

/// DELETE /api/configs/{id}[/{slug}] —— 连同历史一起删（需要成员身份）。
async fn delete_config(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
) -> ApiResult<Response> {
    let a = auth.require_scope("config:write")?;
    let ref_ = ref_from_params(&id, slug.as_deref());
    let row = find_config(&state, &ref_)
        .await?
        .ok_or_else(|| ApiError::not_found("配置不存在"))?;
    if !can_write_config(&state, &row, &a.user_id).await {
        return Err(ApiError::forbidden("你不是该命名空间的成员，无法删除配置"));
    }
    if let Err(e) = store::configs::delete(state.pool(), &row.id).await {
        tracing::error!("删除配置失败: {e}");
        return Err(ApiError::internal("删除失败"));
    }
    Ok(helpers::ok_json(json!({"ok": true, "ref": row.ref_of()})))
}

/* ---------------- 版本 ---------------- */

/// GET /api/configs/{id}[/{slug}]/revisions —— 版本历史（不含内容）。
async fn config_revisions(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
) -> ApiResult<Response> {
    let ref_ = ref_from_params(&id, slug.as_deref());
    let row = find_config(&state, &ref_)
        .await?
        .ok_or_else(|| ApiError::not_found("配置不存在"))?;
    let uid = auth.user_id().unwrap_or_default();
    if !can_read_config(&state, &row, &uid).await {
        return Err(ApiError::not_found("配置不存在或不可读"));
    }
    if row.visibility != "public" && !auth.allow("config:read") {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "scope_required",
            "读取非公开配置需要作用域 config:read",
        ));
    }
    let rows = store::configs::list_revisions(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?;
    let list: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "revision": r.revision, "checksum": r.checksum, "size": r.size,
                "secret": r.secret, "note": r.note, "authorName": r.author_name,
                "createdAt": r.created_at, "current": r.revision == row.revision,
            })
        })
        .collect();
    Ok(helpers::ok_json(json!({
        "ref": row.ref_of(), "current": row.revision, "revisions": list,
    })))
}

#[derive(Debug, Default, Deserialize)]
struct RollbackReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    revision: i64,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    note: String,
}

/// POST /api/configs/{id}[/{slug}]/rollback  body: `{revision, note}`
///
/// 回滚不重写历史：把旧版本的内容**作为新版本**写回去（revision+1），
/// 于是「谁在什么时候回滚过」同样留痕，历史永远只增不改。
async fn rollback_config(
    State(state): State<AppState>,
    auth: Auth,
    Path(helpers::IdSlug { id, slug }): Path<helpers::IdSlug>,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth.require_scope("config:write")?;
    let ref_ = ref_from_params(&id, slug.as_deref());
    let row = find_config(&state, &ref_)
        .await?
        .ok_or_else(|| ApiError::not_found("配置不存在"))?;
    if !can_write_config(&state, &row, &a.user_id).await {
        return Err(ApiError::forbidden("你不是该命名空间的成员，无法回滚配置"));
    }
    let need_rev = || ApiError::bad_request("bad_request", "需要 revision（见 /revisions）");
    let rb: RollbackReq = serde_json::from_slice(&body).map_err(|_| need_rev())?;
    if rb.revision <= 0 {
        return Err(need_rev());
    }
    let rev = store::configs::find_revision(state.pool(), &row.id, rb.revision)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("该版本不存在"))?;
    let mut note = rb.note.trim().to_string();
    if note.is_empty() {
        note = format!("回滚到 v{}", rb.revision);
    }
    note = truncate_chars(&note, 200);

    let input = store::configs::ConfigInput {
        namespace_id: row.namespace_id.clone(),
        slug: row.slug.clone(),
        name: row.name.clone(),
        kind: row.kind.clone(),
        environment: row.environment.clone(),
        format: row.format.clone(),
        summary: row.summary.clone(),
        tags: store::parse_list(&row.tags),
        visibility: row.visibility.clone(),
        status: row.status.clone(),
        secret: rev.secret,
        content: rev.content.clone(),
        checksum: rev.checksum.clone(),
        size: rev.size,
        note,
        author_id: a.user_id.clone(),
        author_name: a.email.clone(),
    };
    if let Err(e) = store::configs::update(state.pool(), &row.id, &input).await {
        tracing::error!("回滚配置失败: {e}");
        return Err(ApiError::internal("回滚失败"));
    }
    let fresh = store::configs::by_id(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::internal("回滚失败"))?;
    let mut out = config_json(&state, &fresh, true, true, true);
    out["rolledBackTo"] = json!(rb.revision);
    Ok(helpers::ok_json(json!({"config": out})))
}

/* ---------------- 成组拉取（Agent 的主入口） ---------------- */

/// GET /api/configs/bundle?namespace=@team&env=prod&kind=&tag=&secrets=1&reveal=1
///
/// 「把这一套配置一次拉全」是 Agent 落地基础设施的第一步：一条命令拿到该环境所有
/// 生效配置（含 any 通用项），每份都带校验和与建议文件名（CLI 直接落盘）。
///
/// 默认**跳过 secret 配置**：一次把凭据全下到磁盘不是好默认；需要时显式 secrets=1。
async fn config_bundle(State(state): State<AppState>, auth: Auth, uri: Uri) -> ApiResult<Response> {
    let ns_slug = web::query(&uri, "namespace").unwrap_or_default();
    let ns_slug = ns_slug.trim().to_string();
    if ns_slug.is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "需要 namespace（如 @team）",
        ));
    }
    let ns_slug = ns_slug.trim_start_matches('@').to_string();
    let row = store::namespaces::by_slug(state.pool(), &ns_slug)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found(format!("命名空间不存在: @{ns_slug}")))?;

    let uid = auth.user_id().unwrap_or_default();
    let member = !uid.is_empty() && helpers::can_manage(&state, &row.id, &uid).await;
    let mut granted = false;
    if !member && !uid.is_empty() {
        granted = store::grants::has(
            state.pool(),
            &row.owner_id,
            &uid,
            store::grants::KIND_CONFIG,
            &row.id,
        )
        .await
            || store::grants::has(
                state.pool(),
                &row.owner_id,
                &uid,
                store::grants::KIND_CONFIG,
                "",
            )
            .await;
    }
    if !member && !granted {
        return Err(ApiError::forbidden(
            "需要是該命名空间成员，或拿到 config 授权（ncc grant set --user @你 --kind config）",
        ));
    }
    if !auth.allow("config:read") {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "scope_required",
            "拉取配置需要作用域 config:read",
        ));
    }

    let env = web::query(&uri, "env").unwrap_or_default();
    let env = env.trim().to_string();
    if !env.is_empty() && !valid_config_env(&env) {
        return Err(ApiError::bad_request(
            "bad_request",
            format!("未知环境: {env}"),
        ));
    }
    let mut opts = store::configs::ListOpts {
        namespace_ids: vec![row.id.clone()],
        env: env.clone(),
        kind: web::query(&uri, "kind")
            .unwrap_or_default()
            .trim()
            .to_string(),
        tag: web::query(&uri, "tag")
            .unwrap_or_default()
            .trim()
            .to_string(),
        statuses: vec!["active".to_string()],
        limit: 200,
        ..Default::default()
    };
    let include_secrets = web::query(&uri, "secrets").as_deref() == Some("1");
    if !include_secrets {
        opts.no_secrets = true;
    }
    let (rows, _) = store::configs::list(state.pool(), &opts)
        .await
        .map_err(ApiError::from_db)?;
    let reveal = reveal_asked(&uri);

    let mut items: Vec<Value> = Vec::with_capacity(rows.len());
    let mut skipped = 0;
    for r in &rows {
        if !can_read_config(&state, r, &uid).await {
            skipped += 1;
            continue;
        }
        let can_write = can_write_config(&state, r, &uid).await;
        let mut item = config_json(&state, r, true, can_write, reveal);
        item["filename"] = json!(bundle_filename(r, &env));
        if !reveal {
            item["content"] = Value::Null;
            item["masked"] = json!(true);
        }
        if r.secret && reveal {
            match open_content(&state, &r.content) {
                Ok(plain) => {
                    item["content"] = json!(plain);
                    item["masked"] = json!(false);
                }
                Err(e) => {
                    item["content"] = Value::Null;
                    item["masked"] = json!(true);
                    item["error"] = json!(e);
                }
            }
        }
        items.push(item);
    }
    Ok(helpers::ok_json(json!({
        "namespace": {"slug": format!("@{}", row.slug), "name": row.name},
        "env": env, "count": items.len(), "skipped": skipped,
        "secretsIncluded": include_secrets, "revealed": reveal,
        "configs": items,
        "howto": format!(
            "CLI: ncc registry config bundle --ns @{} [--env {}] [--secrets --reveal] --out ./conf",
            row.slug, env
        ),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ncc_core::scope::AuthInfo;
    use ncc_core::secretbox::SecretBox;
    use ncc_core::storage::LocalStorage;
    use sqlx::SqlitePool;

    /// 内存库 + 一个组织命名空间 `@team`（机主 U-1、成员 U-2、路人 U-9）。
    async fn test_state() -> AppState {
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://localhost:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();

        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL)
            .await
            .unwrap();
        for (id, name) in [("U-1", "机主"), ("U-2", "成员"), ("U-9", "路人")] {
            sqlx::query(
                "INSERT INTO users (id, email, name, pass_hash, plan, is_admin, disabled) VALUES (?, ?, ?, '', 'free', 0, 0)",
            )
            .bind(id)
            .bind(format!("{}@example.com", id.to_lowercase()))
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO namespaces (id, slug, name, type, owner_id, visibility, created_at) VALUES ('NS-1', 'team', '团队', 'org', 'U-1', 'public', ?)",
        )
        .bind(ncc_core::timeutil::now_go())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ns_members (namespace_id, user_id, role) VALUES ('NS-1', 'U-2', 'member')",
        )
        .execute(&pool)
        .await
        .unwrap();

        // 字节目录只要求存在，配置族本身不写 blob；放 workspace 的 target 下，免得污染源码树。
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-configs-blobs");
        let blobs = Arc::new(LocalStorage::new(&dir, &cfg.public_url, "blobs").unwrap());
        let seal = Arc::new(SecretBox::new(&cfg.jwt_secret).unwrap());
        AppState {
            cfg: Arc::new(cfg),
            pool,
            blobs,
            seal,
        }
    }

    fn session_auth(uid: &str) -> Auth {
        Auth(Some(AuthInfo {
            user_id: uid.to_string(),
            email: format!("{}@example.com", uid.to_lowercase()),
            kind: "user".to_string(),
            session: true,
            ..Default::default()
        }))
    }

    fn key_auth(uid: &str, scopes: &[&str]) -> Auth {
        Auth(Some(AuthInfo {
            user_id: uid.to_string(),
            email: format!("{}@example.com", uid.to_lowercase()),
            kind: "key".to_string(),
            session: false,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }))
    }

    fn anon() -> Auth {
        Auth(None)
    }

    /// JSON → 请求字节（处理器收到的是裸 body）。
    fn bjson(v: &Value) -> Bytes {
        Bytes::from(serde_json::to_vec(v).unwrap())
    }

    fn create_body(slug: &str) -> Value {
        json!({
            "namespace": "team",
            "slug": slug,
            "name": "网络",
            "kind": "network",
            "environment": "prod",
            "format": "yaml",
            "tags": ["a", "b"],
            "content": "a: 1",
        })
    }

    /// 处理器成功回响应；失败回 `ApiError` —— 这里统一折成与线上一致的错误响应体，
    /// 好让测试直接断言状态码与 code/message。
    async fn json_of(r: ApiResult<Response>) -> (StatusCode, Value) {
        let resp = match r {
            Ok(resp) => resp,
            Err(e) => {
                return (
                    e.status,
                    json!({"error": {"code": e.code, "message": e.message}}),
                );
            }
        };
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    fn config_id(v: &Value) -> String {
        v["config"]["id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn 全链路_创建_读取_改内容_历史_回滚_删除() {
        let st = test_state().await;

        let (status, v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&create_body("network")),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(v["created"], json!(true));
        assert_eq!(v["config"]["ref"], json!("@team/network"));
        assert_eq!(v["config"]["revision"], json!(1));
        assert_eq!(v["config"]["kindLabel"], json!("网络"));
        assert_eq!(v["config"]["content"], json!("a: 1")); // 写接口直接回明文
        let id = config_id(&v);

        // 单段 id 与 @ns/slug 两种引用都能取
        let (status, by_id) = json_of(
            get_config(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
                uri("/api/configs"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(by_id["config"]["slug"], json!("network"));
        assert_eq!(by_id["config"]["content"], Value::Null); // 读接口默认打码
        assert_eq!(by_id["config"]["masked"], json!(true));

        let (_, by_ref) = json_of(
            get_config(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: "@team".to_string(),
                    slug: Some("network".to_string()),
                }),
                uri("/api/configs/@team/network?reveal=1"),
            )
            .await,
        )
        .await;
        assert_eq!(by_ref["config"]["content"], json!("a: 1"));
        assert_eq!(by_ref["config"]["masked"], json!(false));

        // 只改元数据：不加版本
        let (_, meta) = json_of(
            update_config(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
                bjson(&json!({"summary": "改个备注", "tags": ["c"]})),
            )
            .await,
        )
        .await;
        assert_eq!(meta["revisionAdded"], json!(false));
        assert_eq!(meta["config"]["revision"], json!(1));
        assert_eq!(meta["config"]["tags"], json!(["c"]));

        // 改内容：追加版本
        let (_, upd) = json_of(
            update_config(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
                bjson(&json!({"content": "a: 2", "note": "改端口"})),
            )
            .await,
        )
        .await;
        assert_eq!(upd["revisionAdded"], json!(true));
        assert_eq!(upd["config"]["revision"], json!(2));
        assert_eq!(upd["config"]["content"], json!("a: 2"));

        let (_, revs) = json_of(
            config_revisions(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(revs["current"], json!(2));
        assert_eq!(revs["revisions"].as_array().unwrap().len(), 2);
        assert_eq!(revs["revisions"][0]["current"], json!(true));
        assert_eq!(revs["revisions"][0]["note"], json!("改端口"));
        assert_eq!(revs["revisions"][1]["current"], json!(false));

        // 回滚到 v1：作为新版本写回，历史只增不改
        let (_, rb) = json_of(
            rollback_config(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
                bjson(&json!({"revision": 1})),
            )
            .await,
        )
        .await;
        assert_eq!(rb["config"]["revision"], json!(3));
        assert_eq!(rb["config"]["content"], json!("a: 1"));
        assert_eq!(rb["config"]["rolledBackTo"], json!(1));
        let (_, revs2) = json_of(
            config_revisions(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(revs2["revisions"].as_array().unwrap().len(), 3);
        assert_eq!(revs2["revisions"][0]["note"], json!("回滚到 v1"));

        let (_, del) = json_of(
            delete_config(
                State(st.clone()),
                session_auth("U-1"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(del["ok"], json!(true));
        assert_eq!(del["ref"], json!("@team/network"));

        let (status, _) = json_of(
            get_config(
                State(st.clone()),
                session_auth("U-1"),
                Path(helpers::IdSlug { id: id, slug: None }),
                uri("/api/configs"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn secret_落库为密文_默认打码_reveal_才回明文() {
        let st = test_state().await;
        let plain = "wifi-psk-123";

        let (status, v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&json!({
                    "namespace": "team", "slug": "wifi", "name": "WiFi",
                    "kind": "security", "secret": true, "content": plain,
                })),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(v["config"]["secret"], json!(true));
        assert_eq!(v["config"]["encrypted"], json!(true));
        let id = config_id(&v);

        // 库里读不到明文
        let stored: String = sqlx::query_scalar("SELECT content FROM config_entries WHERE id = ?")
            .bind(&id)
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert!(stored.starts_with("enc:v1:"), "落库应为密文: {stored}");
        assert!(!stored.contains(plain));
        assert_eq!(st.seal().open(&stored).unwrap(), plain);
        // 校验和/大小按明文算
        let (checksum, size): (String, i64) =
            sqlx::query_as("SELECT checksum, size FROM config_entries WHERE id = ?")
                .bind(&id)
                .fetch_one(st.pool())
                .await
                .unwrap();
        assert_eq!(checksum, checksum_of(plain));
        assert_eq!(size, plain.len() as i64);

        // 读取默认打码
        let (_, masked) = json_of(
            get_config(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
                uri("/api/configs"),
            )
            .await,
        )
        .await;
        assert_eq!(masked["config"]["masked"], json!(true));
        assert_eq!(masked["config"]["content"], Value::Null);
        assert!(masked["config"]["hint"].is_string());
        // 打码响应里不能出现密文
        assert!(!masked["config"].to_string().contains("enc:v1:"));

        // reveal 才回明文（以 API-Key 的 config:read 走一遍，验证作用域路径）
        let (_, plain_out) = json_of(
            get_config(
                State(st.clone()),
                key_auth("U-2", &["config:read"]),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
                uri("/api/configs?reveal=1"),
            )
            .await,
        )
        .await;
        assert_eq!(plain_out["config"]["content"], json!(plain));
        assert_eq!(plain_out["config"]["masked"], json!(false));
        assert_eq!(
            plain_out["config"]["contentChecksum"],
            json!(checksum_of(plain))
        );

        // 历史列表不含内容
        let (_, revs) = json_of(
            config_revisions(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(revs["revisions"][0]["secret"], json!(true));
        assert!(revs["revisions"][0].get("content").is_none());
        assert!(!revs.to_string().contains(plain));

        // 指定历史版本 + reveal 也只回那一版的明文
        let (_, rev1) = json_of(
            get_config(
                State(st.clone()),
                session_auth("U-2"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
                uri("/api/configs?revision=1&reveal=1"),
            )
            .await,
        )
        .await;
        assert_eq!(rev1["config"]["revisionRequested"], json!(1));
        assert_eq!(rev1["config"]["content"], json!(plain));
        assert_eq!(rev1["config"]["revisionMeta"]["revision"], json!(1));

        // 关掉 secret（PATCH 不带 content）：重新按明文存，仍要能读回原值
        let (_, off) = json_of(
            update_config(
                State(st.clone()),
                session_auth("U-1"),
                Path(helpers::IdSlug {
                    id: id.clone(),
                    slug: None,
                }),
                bjson(&json!({"secret": false})),
            )
            .await,
        )
        .await;
        assert_eq!(off["revisionAdded"], json!(false));
        assert_eq!(off["config"]["secret"], json!(false));
        assert_eq!(off["config"]["content"], json!(plain));
        let stored2: String = sqlx::query_scalar("SELECT content FROM config_entries WHERE id = ?")
            .bind(&id)
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(stored2, plain);
    }

    #[tokio::test]
    async fn 权限_公开谁都能读_私有要成员或授权_写要成员() {
        let st = test_state().await;
        // 公开配置
        let (_, pub_v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&json!({
                    "namespace": "team", "slug": "pub", "visibility": "public", "content": "x",
                })),
            )
            .await,
        )
        .await;
        let pub_id = config_id(&pub_v);
        // 私有配置
        let (_, priv_v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&create_body("priv")),
            )
            .await,
        )
        .await;
        let priv_id = config_id(&priv_v);

        // 匿名读公开：可以
        let (status, _) = json_of(
            get_config(
                State(st.clone()),
                anon(),
                Path(helpers::IdSlug {
                    id: pub_id.clone(),
                    slug: None,
                }),
                uri("/api/configs?reveal=1"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // 匿名读私有：当成不存在
        let (status, v) = json_of(
            get_config(
                State(st.clone()),
                anon(),
                Path(helpers::IdSlug {
                    id: priv_id.clone(),
                    slug: None,
                }),
                uri("/api/configs"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["code"], json!("not_found"));

        // 路人（非成员）带 config:read：仍不可读
        let (status, _) = json_of(
            get_config(
                State(st.clone()),
                key_auth("U-9", &["config:read"]),
                Path(helpers::IdSlug {
                    id: priv_id.clone(),
                    slug: None,
                }),
                uri("/api/configs"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // 成员但凭据没有 config:read：可读性够了，但缺作用域 → 403 scope_required
        let (status, v) = json_of(
            get_config(
                State(st.clone()),
                key_auth("U-2", &["registry:read"]),
                Path(helpers::IdSlug {
                    id: priv_id.clone(),
                    slug: None,
                }),
                uri("/api/configs"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], json!("scope_required"));
        assert_eq!(
            v["error"]["message"],
            json!("读取非公开配置需要作用域 config:read")
        );

        // 历史接口同样的作用域要求
        let (status, _) = json_of(
            config_revisions(
                State(st.clone()),
                key_auth("U-2", &["registry:read"]),
                Path(helpers::IdSlug {
                    id: priv_id.clone(),
                    slug: None,
                }),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // 机主（会话）读私有：OK；会话不受作用域限制
        let (status, _) = json_of(
            get_config(
                State(st.clone()),
                session_auth("U-1"),
                Path(helpers::IdSlug {
                    id: priv_id.clone(),
                    slug: None,
                }),
                uri("/api/configs"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // 写：非成员被拒
        let (status, v) = json_of(
            update_config(
                State(st.clone()),
                session_auth("U-9"),
                Path(helpers::IdSlug {
                    id: priv_id.clone(),
                    slug: None,
                }),
                bjson(&json!({"summary": "x"})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            v["error"]["message"],
            json!("你不是该命名空间的成员，无法修改配置")
        );

        // 写：缺 config:write 作用域
        let (status, _) = json_of(
            create_config(
                State(st.clone()),
                key_auth("U-1", &["config:read"]),
                bjson(&create_body("nope")),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // 成员（会话）可写
        let (status, _) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-2"),
                bjson(&create_body("by-member")),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn 非法输入与重复_slug() {
        let st = test_state().await;
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({"namespace": "team", "slug": "A", "content": "x"}),
                "配置 slug 需为 2-48 位小写字母、数字或连字符",
            ),
            (
                json!({"namespace": "team", "slug": "ok-slug", "kind": "nope"}),
                "未知配置类型: nope",
            ),
            (
                json!({"namespace": "team", "slug": "ok-slug", "environment": "nope"}),
                "未知环境: nope（可选 any|dev|staging|prod）",
            ),
            (
                json!({"namespace": "team", "slug": "ok-slug", "format": "nope"}),
                "未知格式: nope",
            ),
            (
                json!({"namespace": "team", "slug": "ok-slug", "visibility": "hidden"}),
                "可见性只能是 private 或 public",
            ),
            (
                json!({"namespace": "team", "slug": "ok-slug", "status": "gone"}),
                "状态只能是 active 或 archived",
            ),
            (
                json!({"namespace": "team", "slug": "ok-slug", "secret": true, "visibility": "public"}),
                "含敏感值的配置不能公开（去掉 public，或把 secret 关掉）",
            ),
        ];
        for (body, want) in cases {
            let (status, v) =
                json_of(create_config(State(st.clone()), session_auth("U-1"), bjson(&body)).await)
                    .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{want}");
            assert_eq!(v["error"]["code"], json!("bad_request"));
            assert_eq!(v["error"]["message"], json!(want));
        }

        // 内容超上限
        let (status, v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&json!({
                    "namespace": "team", "slug": "big", "content": "x".repeat(CONFIG_MAX_BYTES + 1),
                })),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], json!("配置内容超过上限（128 KB）"));

        // 非法 JSON：回 Go 同款 400（而不是 axum 默认的 422）
        let (status, v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                Bytes::from_static(b"{ not json"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], json!("请求体格式错误"));

        // 命名空间不存在
        let (status, v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&json!({"namespace": "ghost", "slug": "ok-slug"})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], json!("namespace ghost 不存在"));

        // 重复 slug
        let (status, _) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&create_body("dup")),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&create_body("dup")),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(v["error"]["code"], json!("conflict"));
        assert!(v["error"]["message"].as_str().unwrap().contains("已存在"));

        // 回滚缺 revision / body 非法：同一个 400
        let (_, created) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&create_body("rb")),
            )
            .await,
        )
        .await;
        let rb_id = config_id(&created);
        for bad in [json!({}), json!({"revision": 0}), json!({"revision": "x"})] {
            let (status, v) = json_of(
                rollback_config(
                    State(st.clone()),
                    session_auth("U-1"),
                    Path(helpers::IdSlug {
                        id: rb_id.clone(),
                        slug: None,
                    }),
                    bjson(&bad),
                )
                .await,
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(
                v["error"]["message"],
                json!("需要 revision（见 /revisions）")
            );
        }
        // 不存在的版本号
        let (status, v) = json_of(
            rollback_config(
                State(st.clone()),
                session_auth("U-1"),
                Path(helpers::IdSlug {
                    id: rb_id,
                    slug: None,
                }),
                bjson(&json!({"revision": 9})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], json!("该版本不存在"));
    }

    #[tokio::test]
    async fn 列表与_bundle_按可见性过滤_默认跳过_secret() {
        let st = test_state().await;
        // 公开（prod）+ 公开（any）+ 私有 + 私有 secret
        for (slug, visibility, env, secret) in [
            ("pub-prod", "public", "prod", false),
            ("pub-any", "public", "any", false),
            ("priv", "private", "prod", false),
            ("priv-secret", "private", "prod", true),
        ] {
            let (s, _) = json_of(
                create_config(
                    State(st.clone()),
                    session_auth("U-1"),
                    bjson(&json!({
                        "namespace": "team", "slug": slug, "visibility": visibility,
                        "environment": env, "secret": secret, "format": "yaml",
                        "content": format!("v-{slug}"),
                    })),
                )
                .await,
            )
            .await;
            assert_eq!(s, StatusCode::CREATED);
        }

        // 匿名列表：只看公开且 active
        let (_, anon_list) =
            json_of(list_configs(State(st.clone()), anon(), uri("/api/configs")).await).await;
        assert_eq!(anon_list["configs"].as_array().unwrap().len(), 2);
        assert!(anon_list["configs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["visibility"] == json!("public")));

        // 成员列表：公开 + 本空间私有
        let (_, member_list) = json_of(
            list_configs(State(st.clone()), session_auth("U-2"), uri("/api/configs")).await,
        )
        .await;
        assert_eq!(member_list["total"], json!(4));
        assert_eq!(member_list["kindCounts"]["other"], json!(2)); // 只算公开的

        // 按 kind 过滤：非法 kind 回 400
        let (status, v) =
            json_of(list_configs(State(st.clone()), anon(), uri("/api/configs?kind=nope")).await)
                .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], json!("未知配置类型: nope"));

        // mine=1 需要登录
        let (status, v) =
            json_of(list_configs(State(st.clone()), anon(), uri("/api/configs?mine=1")).await)
                .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["message"], json!("mine=1 需要登录或凭据"));

        // bundle：非成员被拒
        let (status, _) = json_of(
            config_bundle(
                State(st.clone()),
                session_auth("U-9"),
                uri("/api/configs/bundle?namespace=@team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // bundle：成员，默认跳过 secret，不回明文
        let (_, bundle) = json_of(
            config_bundle(
                State(st.clone()),
                session_auth("U-2"),
                uri("/api/configs/bundle?namespace=@team&env=prod"),
            )
            .await,
        )
        .await;
        assert_eq!(bundle["secretsIncluded"], json!(false));
        assert_eq!(bundle["revealed"], json!(false));
        // prod 命中 pub-prod / priv，any 也命中 pub-any；secret 那条被默认跳过
        assert_eq!(bundle["count"], json!(3));
        assert!(bundle["configs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["content"] == Value::Null && c["masked"] == json!(true)));
        assert!(bundle["configs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["slug"] != json!("priv-secret")));
        let any_item = bundle["configs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["slug"] == json!("pub-any"))
            .unwrap()
            .clone();
        assert_eq!(any_item["filename"], json!("team-pub-any.prod.yaml"));

        // bundle：显式 secrets=1 + reveal=1 才拿明文
        let (_, bundle2) = json_of(
            config_bundle(
                State(st.clone()),
                session_auth("U-2"),
                uri("/api/configs/bundle?namespace=@team&env=prod&secrets=1&reveal=1"),
            )
            .await,
        )
        .await;
        assert_eq!(bundle2["secretsIncluded"], json!(true));
        assert_eq!(bundle2["count"], json!(4));
        let secret_item = bundle2["configs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["slug"] == json!("priv-secret"))
            .unwrap()
            .clone();
        assert_eq!(secret_item["content"], json!("v-priv-secret"));
        assert_eq!(secret_item["masked"], json!(false));

        // bundle：缺 namespace
        let (status, v) = json_of(
            config_bundle(
                State(st.clone()),
                session_auth("U-2"),
                uri("/api/configs/bundle"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], json!("需要 namespace（如 @team）"));
    }

    #[tokio::test]
    async fn 类型目录与纯函数边界() {
        let st = test_state().await;
        let (status, v) = json_of(config_kind_catalog(State(st.clone())).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["kinds"].as_array().unwrap().len(), 10);
        assert_eq!(v["kinds"][0]["kind"], json!("network"));
        assert_eq!(v["kinds"][0]["label"], json!("网络"));
        assert_eq!(v["countScope"], json!("public+active"));
        assert_eq!(v["limits"]["bytes"], json!(131072));
        assert_eq!(v["envs"], json!(["any", "dev", "staging", "prod"]));
        assert_eq!(
            v["formats"],
            json!(["json", "yaml", "toml", "env", "ini", "text", "shell"])
        );

        assert!(valid_config_slug("ab"));
        assert!(!valid_config_slug("a"));
        assert!(!valid_config_slug("-ab"));
        assert!(!valid_config_slug("ab-"));
        assert!(!valid_config_slug("AB"));
        assert_eq!(config_format_ext("env"), ".env");
        assert_eq!(config_format_ext("yaml"), ".yaml");
        assert_eq!(truncate_chars("汉字汉字", 2), "汉字");
    }

    #[tokio::test]
    async fn slug_统一小写_名字缺省取_slug() {
        let st = test_state().await;
        let (status, v) = json_of(
            create_config(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&json!({
                    "namespace": "team", "slug": "  My-Network  ", "content": "x",
                })),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(v["config"]["slug"], json!("my-network"));
        assert_eq!(v["config"]["name"], json!("my-network"));
        assert_eq!(v["config"]["kind"], json!("other"));
        assert_eq!(v["config"]["environment"], json!("any"));
        assert_eq!(v["config"]["visibility"], json!("private")); // 默认私有
    }

    /// 路由表本身要能构建（axum 0.8 的 `{id}/{slug}` 与静态段 `/revisions`、`/rollback`
    /// 混在同一段位置上，写错就是启动即 panic）。
    #[test]
    fn 路由表可构建() {
        let _ = routes();
        let _ = public_routes();
    }
}
