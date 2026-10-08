//! NCC State 的 HTTP 层：知识库（kb）/ 记忆（mem）/ 检查点（ckpt）。
//!
//! 原实现：`ncc-registry/httpapi/state.go`。三样状态**是状态，不是制品**：制品是被安装、
//! 被运行的代码 + 清单（可分发），状态是被改写、会长大的数据（默认私有、生命周期跟着
//! Agent 走）。包只能在 `hur.json` 里**声明**自己要哪些（规则 R11），实际字节住在节点库里。
//!
//! 三样状态共同的规矩（与制品 / 配置 / 轨迹保持一致，别在某一处放宽）：
//!
//! ```text
//! · 默认私有：kb 默认 private，mem 没有公开档（记忆是私人/团队状态），ckpt 默认 private；
//!             跨人读要显式 grant（种类 kind='state'）。
//! · 写只给命名空间成员：被授权者只有读，永远不给写。
//! · fail-closed：没有可见范围就什么都查不到（store 侧拼 `1 = 0`）。
//! ```
//!
//! 刻意的取舍：
//!
//! * 请求体走 `Bytes` + 手工 `serde_json`：先判作用域再判 body，与 Go 里
//!   `requireScope` 中间件早于 `ShouldBindJSON` 的顺序一致（错误 code 仍与 Go 同名：
//!   `bad_json` / `kb_invalid` / `mem_invalid` / `ckpt_invalid`）。
//! * `?revision=` 取旧版正文时**只回那一版的内容**（历史列表本身不回正文）：
//!   否则一次列表就把几十版全文拉回来。
//! * 检查点字节接口不挂作用域中间件：它要同时接受**签名地址**（服务端签发，对方不带
//!   凭据）与可读凭据，两者在 handler 里判一次；签名域前缀 `ckpt:` 让检查点与制品的
//!   签名**不能互相顶替**。

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::Router;
use chrono::{DateTime, FixedOffset, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::ids::{slugify, valid_slug};
use ncc_core::scope::AuthInfo;
use ncc_core::timeutil::{now_unix, parse_time};
use ncc_core::web;

use crate::config::Config;
use crate::httpapi::{helpers, shares, AppState, Auth};
use crate::store;

/* ============================ 词表与上限 ============================ */

/// 文档类型：id → [中文, 英文, 中文说明, 英文说明]（顺序即展示顺序）。
const KB_KINDS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "doc",
        "文档",
        "Document",
        "普通文档/说明",
        "A general document",
    ),
    (
        "faq",
        "问答",
        "FAQ",
        "一问一答，适合直接命中",
        "Question/answer pairs that should hit directly",
    ),
    (
        "notes",
        "笔记",
        "Notes",
        "随手记/会议记录",
        "Scratch notes, meeting minutes",
    ),
    (
        "spec",
        "规范",
        "Spec",
        "接口/协议/约定",
        "Interface, protocol or convention",
    ),
    (
        "transcript",
        "对话记录",
        "Transcript",
        "人与 Agent 的对话导出",
        "Exported conversations",
    ),
];

const KB_FORMATS: &[&str] = &["markdown", "text", "json", "yaml"];

/// 单篇上限（1 MB）：KB 是**语料**（要进库、要能检索），不是大文件 ——
/// 大文件请走制品（blob）。整本书该拆成多篇。
const KB_MAX_BYTES: usize = 1 << 20;
const KB_MAX_TAGS: usize = 32;

/// 记忆种类（顺序即展示顺序）。
const MEM_KINDS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "fact",
        "事实",
        "Fact",
        "关于世界/业务的稳定事实",
        "A stable fact about the world or the business",
    ),
    (
        "preference",
        "偏好",
        "Preference",
        "这个人喜欢什么（对人不对事）",
        "What this person prefers",
    ),
    (
        "episode",
        "经历",
        "Episode",
        "上次发生了什么（常与 trace 关联）",
        "What happened last time (often tied to a trace)",
    ),
    (
        "summary",
        "小结",
        "Summary",
        "对一段历史的压缩结论",
        "A compressed conclusion about a stretch of history",
    ),
    (
        "pointer",
        "指针",
        "Pointer",
        "指向别处（kb / ckpt / trace id）",
        "Points elsewhere (kb / ckpt / trace id)",
    ),
];

/// 记忆是「小结论」不是文档：64 KB 是有意的，更大说明它其实是 kb 或 ckpt。
const MEM_MAX_VALUE_BYTES: usize = 64 * 1024;
const MEM_MAX_KEY_LEN: usize = 128;
const MEM_MAX_SUBJECT_LEN: usize = 64;

/// 打点粒度（顺序即展示顺序）。
const CKPT_LABELS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "episode",
        "回合",
        "Episode",
        "一个完整任务回合结束",
        "After one complete task episode",
    ),
    (
        "step",
        "步",
        "Step",
        "第 N 步（细粒度，量大）",
        "The Nth step (fine-grained, high volume)",
    ),
    (
        "run",
        "运行",
        "Run",
        "一次完整运行结束",
        "After one full run",
    ),
    (
        "release",
        "发布",
        "Release",
        "与某个制品版本对齐",
        "Aligned with a published version",
    ),
    (
        "handoff",
        "交接",
        "Handoff",
        "交给别人/别的 Agent 接管",
        "Handing over to someone else",
    ),
    (
        "manual",
        "手动",
        "Manual",
        "人手动打的点",
        "A checkpoint a human took",
    ),
];

/// 单个检查点上限（512 MB）：检查点是**快照**，上限的存在是为了挡住「模型权重」
/// 那种大家伙 —— 那种该走对象存储 / 制品。
const CKPT_MAX_BYTES: i64 = 512 << 20;
/// 上传字节的上限（raw body）。
const CKPT_UPLOAD_MAX_BYTES: usize = 256 << 20;
/// 签名地址有效期。
const CKPT_URL_TTL_SECS: i64 = 600;

const VISIBILITIES: &[&str] = &["private", "public"];

/// 五类内容（kb / mem / ckpt / trace 与用户自己声明的集合）**共用**的口径：
/// 一份常量、多处返回同一份，免得文档与代码各说各的。
fn shared_invariants() -> Vec<&'static str> {
    vec![
        "归档 ≠ 删除：归档只是默认不列出（指定 `archived=1` 还能看到），删要显式说",
        "读不到 ≠ 没有：读不到时说清原因（不存在 / 不是你的 / 已过期），不给一个空结果",
        "CRUD ≠ 授权：写要命名空间成员，跨空间读要 grant —— 能写不等于能读别人的",
        "过期即不存在：过期在**读时**判定（不等清理任务），gc 只是清垃圾",
        "动态 ≠ 无模式：没声明的字段、不能过滤的字段，明确拒绝而不是默默忽略",
    ]
}

fn valid_kb_kind(k: &str) -> bool {
    KB_KINDS.iter().any(|row| row.0 == k)
}

fn valid_kb_format(f: &str) -> bool {
    KB_FORMATS.contains(&f)
}

fn valid_visibility(v: &str) -> bool {
    matches!(v, "private" | "public")
}

fn valid_kb_status(s: &str) -> bool {
    matches!(s, "active" | "archived")
}

fn valid_mem_kind(k: &str) -> bool {
    MEM_KINDS.iter().any(|row| row.0 == k)
}

fn valid_ckpt_label(l: &str) -> bool {
    CKPT_LABELS.iter().any(|row| row.0 == l)
}

/// 从词表里取标签（中/英），未知取值回原样。
fn catalog_label(cat: &[(&str, &str, &str, &str, &str)], k: &str, lang: &str) -> String {
    match cat.iter().find(|row| row.0 == k) {
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

fn kb_kind_label(k: &str, lang: &str) -> String {
    catalog_label(KB_KINDS, k, lang)
}

fn mem_kind_label(k: &str, lang: &str) -> String {
    catalog_label(MEM_KINDS, k, lang)
}

fn ckpt_label_text(k: &str, lang: &str) -> String {
    catalog_label(CKPT_LABELS, k, lang)
}

/* ============================ 路由 ============================ */

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/kb/kinds", get(kb_kinds))
        .route("/kb/bundle", get(kb_bundle))
        .route("/kb", get(list_kb).post(upsert_kb))
        .route("/kb/", get(list_kb).post(upsert_kb))
        // 引用两种形态：`KD-…`（单段）与 `@ns/slug`（两段），与制品 / 配置同一套写法。
        .route("/kb/{id}/revisions", get(kb_revisions))
        .route("/kb/{id}/{slug}/revisions", get(kb_revisions))
        .route("/kb/{id}", get(get_kb).patch(patch_kb).delete(delete_kb))
        .route(
            "/kb/{id}/{slug}",
            get(get_kb).patch(patch_kb).delete(delete_kb),
        )
        // 记忆：**没有公开档**（公开「记忆」这件事本身就不合语义）。
        .route("/mem/kinds", get(mem_kinds))
        .route("/mem/lookup", get(lookup_mem))
        .route("/mem/gc", post(gc_mem))
        .route("/mem", get(list_mem).put(put_mem))
        .route("/mem/", get(list_mem).put(put_mem))
        .route("/mem/{id}", get(get_mem).delete(delete_mem))
        // 检查点：元数据先建、字节后传，血缘可回溯。
        .route("/ckpt/kinds", get(ckpt_kinds))
        .route("/ckpt/prune", post(prune_ckpt))
        .route("/ckpt", get(list_ckpt).post(create_ckpt))
        .route("/ckpt/", get(list_ckpt).post(create_ckpt))
        .route("/ckpt/{id}/blob", put(put_ckpt_blob))
        .route("/ckpt/{id}/lineage", get(ckpt_lineage))
        .route("/ckpt/{id}/bytes", get(ckpt_bytes))
        .route("/ckpt/{id}", get(get_ckpt).delete(delete_ckpt))
}

/// 本族没有顶层公开页（公开文档仍走 `/api/kb/...`）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
}

/* ============================ 小工具 ============================ */

/// 查询串里的整数，语义与 Go 的 `strconv.Atoi(c.DefaultQuery(k, def))` 对齐：
/// 键不存在 → 默认值；键存在但解析不出来（含空值）→ 0（由调用方兜底）。
fn query_atoi(uri: &Uri, key: &str, def: i64) -> i64 {
    match web::query(uri, key) {
        Some(v) => v.trim().parse::<i64>().unwrap_or(0),
        None => def,
    }
}

/// 与 Go 的 `strconv.Quote` 同形：非 ASCII 可打印字符**不转义**（`"中文"` 原样输出）。
///
/// 直接拿 `{:?}` 会输出 `"\u{4e2d}\u{6587}"` —— 校验报错是给人看的，转义了就读不懂。
fn go_quote(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// 库里 / 请求里的时间 → UTC RFC3339 秒精度（`2026-10-08T02:24:06Z`）。
fn rfc3339_utc(s: Option<&str>) -> String {
    s.and_then(parse_time)
        .map(|t| {
            t.with_timezone(&Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .unwrap_or_default()
}

fn parse_body<T: serde::de::DeserializeOwned>(b: &Bytes) -> ApiResult<T> {
    serde_json::from_slice(b)
        .map_err(|e| ApiError::bad_request("bad_json", format!("请求体不是合法 JSON: {e}")))
}

/* ============================ 可见范围与权限 ============================ */

/// 把一个请求身份折成三样状态共用的可见范围。
///
/// 与轨迹同一套：默认**只看我的 + 被授权给我的**；管理员要 `all=1` 才看全节点；
/// 没有凭据就什么都看不到（fail-closed）。
async fn state_scope_of(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    uri: &Uri,
) -> store::stack::StateScope {
    let mut sc = store::stack::StateScope::default();
    let Some(a) = auth.info() else {
        return sc;
    };
    if web::query(uri, "all").as_deref() == Some("1")
        && shares::ensure_admin(state, auth, headers).await.is_some()
    {
        sc.all = true;
        return sc;
    }
    if let Ok(nss) = store::namespaces::of_user(state.pool(), &a.user_id).await {
        sc.namespace_ids = nss.iter().map(|n| n.id.clone()).collect();
    }
    if web::query(uri, "mine").as_deref() != Some("1") {
        sc.granted_owners = store::grants::granted_owners(state.pool(), &a.user_id, "state").await;
    }
    sc
}

fn state_scope_label(auth: &Auth, uri: &Uri, all: bool) -> &'static str {
    if all {
        return "all";
    }
    if web::query(uri, "mine").as_deref() == Some("1") {
        return "mine";
    }
    if auth.info().is_none() {
        return "public";
    }
    "visible"
}

/// 解析目标命名空间（默认：调用者的个人空间）。三样状态共用。
async fn state_namespace(
    state: &AppState,
    a: &AuthInfo,
    want: &str,
) -> ApiResult<store::namespaces::Namespace> {
    let want = want.trim().trim_start_matches('@');
    let nss = store::namespaces::of_user(state.pool(), &a.user_id)
        .await
        .map_err(ApiError::from_db)?;
    if want.is_empty() {
        if let Some(ns) = nss.iter().find(|n| n.ns_type == "account") {
            return Ok(ns.clone());
        }
        if let Some(ns) = nss.first() {
            return Ok(ns.clone());
        }
        return Err(ApiError::bad_request(
            "no_namespace",
            "你还没有命名空间（先注册或建一个组织空间）",
        ));
    }
    if let Some(ns) = nss.iter().find(|n| n.slug == want) {
        return Ok(ns.clone());
    }
    Err(ApiError::forbidden(format!(
        "你不是命名空间 @{want} 的成员，不能把状态写进去"
    )))
}

/// 这个身份能不能读这个命名空间的状态（公开项另行判断）。
async fn state_readable(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    ns_id: &str,
    owner_id: &str,
) -> bool {
    let Some(a) = auth.info() else {
        return false;
    };
    if shares::ensure_admin(state, auth, headers).await.is_some() {
        return true;
    }
    if helpers::can_manage(state, ns_id, &a.user_id).await {
        return true;
    }
    store::stack::has_state_grant(state.pool(), owner_id, &a.user_id, ns_id).await
        || store::stack::has_state_grant(state.pool(), owner_id, &a.user_id, "").await
}

/// 写权限：**只有命名空间成员**（被授权者只读）。
async fn state_writable(state: &AppState, ns_id: &str, user_id: &str) -> bool {
    helpers::can_manage(state, ns_id, user_id).await
}

/// 把这个命名空间里的**内置内容**标成「用过了」（取用即声明）。
///
/// 失败**不影响主流程**：声明行只是清单与字段形状，写不进去不该让一次记忆写入失败。
async fn mark_builtin_used(state: &AppState, ns_id: &str, kind: &str) {
    if ns_id.is_empty() {
        return;
    }
    if let Err(e) = store::stack::ensure_builtin_collection(state.pool(), ns_id, kind).await {
        tracing::error!("内置集合声明写入失败 ns={ns_id} kind={kind}: {e}");
    }
}

/* ============================ 视图 ============================ */

fn kb_json(r: &store::stack::KbRow, with_content: bool) -> Value {
    let mut out = json!({
        "id": r.id,
        "ref": r.ref_of(),
        "slug": r.slug,
        "title": r.title,
        "kind": r.kind,
        "kindLabel": kb_kind_label(&r.kind, "zh"),
        "format": r.format,
        "summary": r.summary,
        "tags": store::parse_list(&r.tags),
        "visibility": r.visibility,
        "status": r.status,
        "revision": r.revision,
        "size": r.size,
        "checksum": r.checksum,
        "source": r.source,
        "namespace": {"slug": r.ns_slug.clone().unwrap_or_default(), "name": r.ns_name.clone().unwrap_or_default()},
        "owner": {"id": r.owner_id.clone().unwrap_or_default(), "name": r.owner_name.clone().unwrap_or_default()},
        "createdBy": r.created_by,
        "updatedBy": r.updated_by,
        "createdAt": rfc3339_utc(r.created_at.as_deref()),
        "updatedAt": rfc3339_utc(r.updated_at.as_deref()),
    });
    if with_content {
        out["content"] = json!(r.content);
    }
    out
}

fn mem_json(r: &store::stack::MemRow, now: DateTime<FixedOffset>) -> Value {
    let mut out = json!({
        "id": r.id,
        "subject": r.subject,
        "key": r.key,
        "value": r.value,
        "kind": r.kind,
        "kindLabel": mem_kind_label(&r.kind, "zh"),
        "tags": store::parse_list(&r.tags),
        "source": r.source,
        "confidence": r.confidence,
        "pinned": r.pinned,
        "revision": r.revision,
        "namespace": {"slug": r.ns_slug.clone().unwrap_or_default(), "name": r.ns_name.clone().unwrap_or_default()},
        "createdBy": r.created_by,
        "updatedBy": r.updated_by,
        "createdAt": rfc3339_utc(r.created_at.as_deref()),
        "updatedAt": rfc3339_utc(r.updated_at.as_deref()),
    });
    // 没有过期时间就不给这两个字段：给 `expiresAt: null` 会让人以为「查过、已过期」。
    if let Some(exp) = r.expires_at.as_deref() {
        if !exp.is_empty() {
            out["expiresAt"] = json!(rfc3339_utc(Some(exp)));
            out["expired"] = json!(r.expired_at(now));
        }
    }
    out
}

fn ckpt_json(state: &AppState, r: &store::stack::CkptRow, with_url: bool) -> Value {
    let mut out = json!({
        "id": r.id,
        "name": r.name,
        "label": r.label,
        "labelText": ckpt_label_text(&r.label, "zh"),
        "step": r.step,
        "summary": r.summary,
        "tags": store::parse_list(&r.tags),
        "visibility": r.visibility,
        "status": r.status,
        "subjectRef": r.subject_ref,
        "subjectVersion": r.subject_version,
        "parent": r.parent,
        "digest": r.digest,
        "size": r.size,
        "mediaType": r.media_type,
        "meta": web::parse_json_any(&r.meta),
        "namespace": {"slug": r.ns_slug.clone().unwrap_or_default(), "name": r.ns_name.clone().unwrap_or_default()},
        "createdBy": r.created_by,
        "createdAt": rfc3339_utc(r.created_at.as_deref()),
    });
    if with_url && !r.object_key.is_empty() {
        out["bytesUrl"] = json!(ckpt_bytes_url(state.cfg(), &r.id, CKPT_URL_TTL_SECS));
        out["bytesTtlSec"] = json!(CKPT_URL_TTL_SECS);
    }
    out
}

/// 短时字节地址：HMAC(secret, `ckpt:<id>|<exp>`)。
///
/// 域前缀 `ckpt:` 让检查点与制品的签名不能互相顶替 —— 少了它，一张制品的下载票
/// 就能当检查点的票用。
fn ckpt_bytes_url(cfg: &Config, id: &str, ttl_secs: i64) -> String {
    let exp = now_unix() + ttl_secs;
    let sig = ncc_core::crypto::hmac_sha256_b64url(&cfg.jwt_secret, &format!("ckpt:{id}|{exp}"));
    format!(
        "{}/api/ckpt/{}/bytes?exp={}&sig={}",
        cfg.public_url, id, exp, sig
    )
}

fn valid_ckpt_sig(cfg: &Config, id: &str, exp: &str, sig: &str) -> bool {
    let Ok(n) = exp.parse::<i64>() else {
        return false;
    };
    if n < now_unix() || sig.is_empty() {
        return false;
    }
    let expect = ncc_core::crypto::hmac_sha256_b64url(&cfg.jwt_secret, &format!("ckpt:{id}|{exp}"));
    ncc_core::crypto::constant_time_eq(sig.as_bytes(), expect.as_bytes())
}

/* ============================ 目录 ============================ */

/// GET /api/kb/kinds —— 词表与上限（CLI 取值来源）。
async fn kb_kinds() -> ApiResult<Response> {
    let kinds: Vec<Value> = KB_KINDS
        .iter()
        .map(|k| json!({"id": k.0, "zh": k.1, "en": k.2, "descZh": k.3, "descEn": k.4}))
        .collect();
    Ok(helpers::ok_json(json!({
        "resource": "kb",
        "kinds": kinds,
        "formats": KB_FORMATS,
        "visibilities": VISIBILITIES,
        "statuses": ["active", "archived"],
        "limits": {"maxBytes": KB_MAX_BYTES, "maxTags": KB_MAX_TAGS},
        "invariants": shared_invariants(),
        // 检索是**关键词**打分（标题/摘要/正文加权），不是向量检索 —— 说清楚。
        "search": "keyword (weighted title/summary/content); vector search is not implemented",
    })))
}

/// GET /api/mem/kinds
async fn mem_kinds() -> ApiResult<Response> {
    let kinds: Vec<Value> = MEM_KINDS
        .iter()
        .map(|k| json!({"id": k.0, "zh": k.1, "en": k.2, "descZh": k.3, "descEn": k.4}))
        .collect();
    Ok(helpers::ok_json(json!({
        "resource": "mem",
        "kinds": kinds,
        "limits": {
            "maxValueBytes": MEM_MAX_VALUE_BYTES,
            "maxKeyLen": MEM_MAX_KEY_LEN,
            "maxSubjectLen": MEM_MAX_SUBJECT_LEN,
        },
        "invariants": shared_invariants(),
        "semantics": {
            "upsert": "(namespace, subject, key) 唯一：同键再写就是更新（Revision+1）",
            "expiry": "ttl_days>0 时写 expiresAt；**读时判定**过期（过期即视为不存在），另有 gc 真正清理",
            "visibility": "记忆没有公开档：只有命名空间成员与拿到 state 授权的人能读",
        },
    })))
}

/// GET /api/ckpt/kinds
async fn ckpt_kinds() -> ApiResult<Response> {
    let labels: Vec<Value> = CKPT_LABELS
        .iter()
        .map(|k| json!({"id": k.0, "zh": k.1, "en": k.2, "descZh": k.3, "descEn": k.4}))
        .collect();
    Ok(helpers::ok_json(json!({
        "resource": "ckpt",
        "labels": labels,
        "limits": {
            "maxBytes": CKPT_MAX_BYTES,
            "signedUrlTtlSec": CKPT_URL_TTL_SECS,
            "uploadMaxBytes": CKPT_UPLOAD_MAX_BYTES,
        },
        "invariants": shared_invariants(),
        "semantics": {
            "immutable": "检查点不可改：要改就再打一个点（打点记录的是「当时是什么样」）",
            "lineage": "parent 指向上一个点，可一路回溯",
        },
    })))
}

/* ============================ 知识库 kb ============================ */

/// 从路由参数拼回引用。
///
/// 引用有两种形态（`KD-…` 单段、`@命名空间/slug` 两段），路由也注册了两套，所以
/// **必须把两段拼起来**再交给 store —— 只取 `{id}` 会让 `@ns/slug` 取不到。
///
/// 参数用 `HashMap` 取而不是 `Path<(String, Option<String>)>`：元组取参要求元素个数与
/// 路由参数**完全相等**，单段路由上会直接失败（`WrongNumberOfParameters`）—— 而单段
/// 形态恰恰是最常用的那种（按 `KD-…` 取）。
///
/// 需要类型化写法时用 [`super::helpers::IdSlug`]（`{id}` + 可选 `{slug}` 的共用载体），
/// 别自己再拼一遍 `HashMap`。
fn ref_from_params(params: &HashMap<String, String>) -> String {
    match params.get("slug") {
        Some(s) if !s.is_empty() => {
            format!("{}/{}", params.get("id").cloned().unwrap_or_default(), s)
        }
        _ => params.get("id").cloned().unwrap_or_default(),
    }
}

/// GET /api/kb?…
///
/// 读接口不挂作用域：公开文档匿名可读（与配置同一取舍），非公开的在 handler 里判 ——
/// 「该不该给这个人看」与「这篇是不是公开」必须一起判。
async fn list_kb(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let page = query_atoi(&uri, "page", 1);
    let size = query_atoi(&uri, "size", 20);
    let page = if page < 1 { 1 } else { page };
    let size = if size <= 0 || size > 200 { 20 } else { size };
    let q_raw = web::query(&uri, "q").unwrap_or_default();
    let kind = web::query(&uri, "kind").unwrap_or_default();
    if !kind.is_empty() && !valid_kb_kind(&kind) {
        return Err(ApiError::bad_request(
            "bad_kind",
            format!("未知文档类型: {kind}（见 GET /api/kb/kinds）"),
        ));
    }
    let opts = store::stack::KbListOpts {
        scope: state_scope_of(&state, &auth, &headers, &uri).await,
        ns_slug: web::query(&uri, "namespace")
            .unwrap_or_default()
            .trim()
            .trim_start_matches('@')
            .to_string(),
        kind,
        tag: web::query(&uri, "tag").unwrap_or_default(),
        status: web::query(&uri, "status").unwrap_or_default(),
        q: q_raw.trim().to_string(),
        include_arch: web::query(&uri, "archived").as_deref() == Some("1"),
        page,
        size,
        limit: 0,
        // 公开文档**在列表里也要出现**（否则「按引用取得到、列表里看不到」）。
        // 匿名调用者没有命名空间范围，这一档就是它唯一的可见面。
        include_public: true,
        rank: !q_raw.is_empty(),
    };
    let (rows, matched) = store::stack::list_kb_docs(state.pool(), &opts)
        .await
        .map_err(ApiError::from_db)?;

    // 行级过滤：公开项之外，还要这个身份真的能读（成员身份 / state 授权）。
    // store 侧已经按「我的 ∪ 被授权的 ∪ 公开的」取过一轮，这里是**第二道**（纵深防御）：
    // 可见性判错一次就等于泄漏，宁可多判一遍。
    let mut list: Vec<Value> = Vec::with_capacity(rows.len());
    for r in &rows {
        let public_active = r.is_public() && r.status == "active";
        let readable = match auth.info() {
            Some(_) => {
                state_readable(
                    &state,
                    &auth,
                    &headers,
                    &r.namespace_id,
                    &r.owner_id.clone().unwrap_or_default(),
                )
                .await
            }
            None => false,
        };
        if public_active || readable {
            list.push(kb_json(r, false));
        }
    }
    let scope = state_scope_label(&auth, &uri, opts.scope.all);
    Ok(helpers::ok_json(json!({
        "docs": list,
        // total 是**过滤之后**能看的条数；matched 是查询命中的条数（两者差说明
        // 有些命中项不在你的可见范围里 —— 说清楚比给一个含糊的数好）。
        "total": list.len(),
        "matched": matched,
        "page": page,
        "size": size,
        "scope": scope,
        "ranked": opts.rank,
    })))
}

/// GET /api/kb/bundle?namespace=&kind=&tag= —— Agent 把知识库拉到本地。
///
/// 与配置的 bundle 同形：这是 Agent 落地的第一步（先有语料，才谈得上用）。
/// 默认只给**读得到的**（公开 + 我的 + 被授权的），并带上 checksum 便于本地增量同步。
async fn kb_bundle(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let ns = web::query(&uri, "namespace")
        .unwrap_or_default()
        .trim()
        .trim_start_matches('@')
        .to_string();
    if ns.is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺 namespace（要拉哪个命名空间的库，如 @team）",
        ));
    }
    let (rows, _) = store::stack::list_kb_docs(
        state.pool(),
        &store::stack::KbListOpts {
            scope: state_scope_of(&state, &auth, &headers, &uri).await,
            ns_slug: ns.clone(),
            kind: web::query(&uri, "kind").unwrap_or_default(),
            tag: web::query(&uri, "tag").unwrap_or_default(),
            status: "active".to_string(),
            include_public: true,
            limit: 500,
            ..Default::default()
        },
    )
    .await
    .map_err(ApiError::from_db)?;

    let mut out: Vec<Value> = Vec::with_capacity(rows.len());
    for r in &rows {
        if !r.is_public()
            && !state_readable(
                &state,
                &auth,
                &headers,
                &r.namespace_id,
                &r.owner_id.clone().unwrap_or_default(),
            )
            .await
        {
            continue;
        }
        out.push(kb_json(r, true));
    }
    Ok(helpers::ok_json(json!({
        "namespace": ns,
        "docs": out,
        "count": out.len(),
        "how": "每条带 checksum 与 format —— 本地按 slug 落盘，据 checksum 做增量同步",
    })))
}

/// 写入请求（创建与更新共用；按 `(namespace, slug)` upsert）。
#[derive(Debug, Deserialize, Default)]
struct KbReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    namespace: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    slug: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    title: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    kind: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    format: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    summary: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    visibility: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    source: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    content: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    note: String,
}

/// 写入前的自检（内容面；slug 的合法性在调用处单独判）。
fn kb_validate(
    title: &str,
    kind: &str,
    format: &str,
    visibility: &str,
    content: &str,
    tags: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if title.trim().is_empty() {
        out.push("缺 title（检索结果里先看到的就是它）".to_string());
    }
    if !valid_kb_kind(kind) {
        out.push(format!(
            "kind 必须是 {}，当前是 {}",
            KB_KINDS.iter().map(|k| k.0).collect::<Vec<_>>().join("|"),
            go_quote(kind)
        ));
    }
    if !valid_kb_format(format) {
        out.push(format!(
            "format 必须是 {}，当前是 {}",
            KB_FORMATS.join("|"),
            go_quote(format)
        ));
    }
    if !valid_visibility(visibility) {
        out.push(format!(
            "visibility 必须是 private|public，当前是 {}",
            go_quote(visibility)
        ));
    }
    if content.len() > KB_MAX_BYTES {
        out.push(format!(
            "内容太大（{} 字节，上限 {}）—— 大文件请走制品",
            content.len(),
            KB_MAX_BYTES
        ));
    }
    if tags.len() > KB_MAX_TAGS {
        out.push(format!("tags 太多（上限 {}）", KB_MAX_TAGS));
    }
    out
}

/// POST /api/kb —— 建或改（改 = 新版本）。
async fn upsert_kb(State(state): State<AppState>, auth: Auth, body: Bytes) -> ApiResult<Response> {
    let a = auth.require_scope("kb:write")?;
    let mut req: KbReq = parse_body(&body)?;
    if req.kind.is_empty() {
        req.kind = "doc".to_string();
    }
    if req.format.is_empty() {
        req.format = "markdown".to_string();
    }
    if req.visibility.is_empty() {
        req.visibility = "private".to_string();
    }
    if req.slug.trim().is_empty() {
        // slug 缺省从 title 生成（与制品/配置同一套清洗）。
        req.slug = slugify(&req.title);
    }
    if !valid_slug(&req.slug) {
        return Err(ApiError::bad_request(
            "bad_slug",
            format!(
                "slug 不合法: {}（小写字母数字与 - _ .，2..64 位）",
                go_quote(&req.slug)
            ),
        ));
    }
    let errs = kb_validate(
        &req.title,
        &req.kind,
        &req.format,
        &req.visibility,
        &req.content,
        &req.tags,
    );
    if !errs.is_empty() {
        return Err(ApiError::bad_request("kb_invalid", errs.join("; ")));
    }
    let ns = state_namespace(&state, a, &req.namespace).await?;
    mark_builtin_used(&state, &ns.id, "kb").await;

    let checksum = format!(
        "sha256:{}",
        ncc_core::crypto::sha256_hex(req.content.as_bytes())
    );
    let (row, created) = store::stack::upsert_kb_doc(
        state.pool(),
        &store::stack::KbInput {
            namespace_id: ns.id.clone(),
            slug: req.slug.clone(),
            title: req.title.clone(),
            kind: req.kind.clone(),
            format: req.format.clone(),
            summary: req.summary.clone(),
            tags: req.tags.clone(),
            visibility: req.visibility.clone(),
            source: req.source.clone(),
            content: req.content.clone(),
            checksum,
            note: req.note.clone(),
            author_id: a.user_id.clone(),
            author_name: a.email.clone(),
        },
    )
    .await
    .map_err(ApiError::from_db)?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(helpers::ok_status(
        status,
        json!({
            "doc": kb_json(&row, false),
            "created": created,
            "ref": row.ref_of(),
            "revision": row.revision,
        }),
    ))
}

/// GET /api/kb/{id}[/{slug}] —— 取一篇（含内容），`?revision=N` 取历史版本。
async fn get_kb(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
    uri: Uri,
) -> ApiResult<Response> {
    let ref_ = ref_from_params(&params);
    let row = store::stack::get_kb_doc(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found(format!("没有这篇文档: {ref_}")))?;
    let public = row.is_public();
    let readable = state_readable(
        &state,
        &auth,
        &headers,
        &row.namespace_id,
        &row.owner_id.clone().unwrap_or_default(),
    )
    .await;
    if !public && !readable {
        return Err(ApiError::forbidden(
            "这篇文档不在你可见的范围内（默认私有；跨人查看要 state 授权）",
        ));
    }
    let mut out = kb_json(&row, true);
    // 取历史版本：`?revision=N`（默认最新）。
    let rev = web::query(&uri, "revision").unwrap_or_default();
    if !rev.is_empty() && rev != "latest" {
        let Ok(n) = rev.parse::<i64>() else {
            return Err(ApiError::bad_request("bad_revision", "revision 要是正整数"));
        };
        if n <= 0 {
            return Err(ApiError::bad_request("bad_revision", "revision 要是正整数"));
        }
        let revs = store::stack::kb_revisions(state.pool(), &row.id)
            .await
            .map_err(ApiError::from_db)?;
        match revs.into_iter().find(|r| r.revision == n) {
            Some(r) => {
                out["content"] = json!(r.content);
                out["checksum"] = json!(r.checksum);
                out["title"] = json!(r.title);
                out["requestedRevision"] = json!(n);
            }
            None => return Err(ApiError::not_found(format!("没有第 {rev} 版"))),
        }
    }
    Ok(helpers::ok_json(out))
}

/// PATCH /api/kb/{id}[/{slug}] —— 改名 / 归档 / 恢复（内容更新走 POST upsert）。
#[derive(Debug, Deserialize, Default)]
struct KbPatchReq {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

async fn patch_kb(
    State(state): State<AppState>,
    auth: Auth,
    Path(params): Path<HashMap<String, String>>,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth.require_scope("kb:write")?;
    let ref_ = ref_from_params(&params);
    let row = store::stack::get_kb_doc(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这篇文档"))?;
    if !state_writable(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只有归属命名空间的成员能改这篇文档"));
    }
    let req: KbPatchReq = parse_body(&body)?;
    if let Some(v) = &req.visibility {
        if !valid_visibility(v) {
            return Err(ApiError::bad_request(
                "bad_visibility",
                "visibility 只能是 private|public",
            ));
        }
    }
    if let Some(s) = &req.status {
        if !valid_kb_status(s) {
            return Err(ApiError::bad_request(
                "bad_status",
                "status 只能是 active|archived",
            ));
        }
    }
    store::stack::patch_kb_doc(
        state.pool(),
        &row.id,
        &store::stack::KbPatch {
            title: req.title,
            summary: req.summary,
            tags: req.tags,
            visibility: req.visibility,
            status: req.status,
            updated_by: a.user_id.clone(),
        },
    )
    .await
    .map_err(ApiError::from_db)?;
    let got = store::stack::get_kb_doc(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这篇文档"))?;
    Ok(helpers::ok_json(json!({"doc": kb_json(&got, false)})))
}

/// DELETE /api/kb/{id}[/{slug}]
async fn delete_kb(
    State(state): State<AppState>,
    auth: Auth,
    Path(params): Path<HashMap<String, String>>,
) -> ApiResult<Response> {
    let a = auth.require_scope("kb:write")?;
    let ref_ = ref_from_params(&params);
    let row = store::stack::get_kb_doc(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这篇文档"))?;
    if !state_writable(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只有归属命名空间的成员能删这篇文档"));
    }
    store::stack::delete_kb_doc(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_json(
        json!({"ok": true, "id": row.id, "ref": row.ref_of()}),
    ))
}

/// GET /api/kb/{id}[/{slug}]/revisions —— 版本历史（**不回正文**）。
async fn kb_revisions(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(params): Path<HashMap<String, String>>,
) -> ApiResult<Response> {
    let ref_ = ref_from_params(&params);
    let row = store::stack::get_kb_doc(state.pool(), &ref_)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这篇文档"))?;
    let public = row.is_public();
    let readable = state_readable(
        &state,
        &auth,
        &headers,
        &row.namespace_id,
        &row.owner_id.clone().unwrap_or_default(),
    )
    .await;
    if !public && !readable {
        return Err(ApiError::forbidden("这篇文档不在你可见的范围内"));
    }
    let revs = store::stack::kb_revisions(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?;
    let out: Vec<Value> = revs
        .iter()
        .map(|r| {
            json!({
                "revision": r.revision,
                "title": r.title,
                "size": r.size,
                "checksum": r.checksum,
                "note": r.note,
                "author": r.author,
                "authorId": r.author_id,
                "at": rfc3339_utc(r.created_at.as_deref()),
            })
        })
        .collect();
    Ok(helpers::ok_json(json!({
        "ref": row.ref_of(),
        "revisions": out,
        "revision": row.revision,
    })))
}

/* ============================ 记忆 mem ============================ */

/// GET /api/mem?…
async fn list_mem(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    auth.require_scope("mem:read")?;
    let limit = query_atoi(&uri, "limit", 200);
    let limit = if limit <= 0 || limit > 1000 {
        200
    } else {
        limit
    };
    let opts = store::stack::MemListOpts {
        scope: state_scope_of(&state, &auth, &headers, &uri).await,
        subject: web::query(&uri, "subject").unwrap_or_default(),
        prefix: web::query(&uri, "prefix").unwrap_or_default(),
        kind: web::query(&uri, "kind").unwrap_or_default(),
        tag: web::query(&uri, "tag").unwrap_or_default(),
        source: web::query(&uri, "source").unwrap_or_default(),
        include_expired: web::query(&uri, "expired").as_deref() == Some("1"),
        pinned_only: web::query(&uri, "pinned").as_deref() == Some("1"),
        limit,
        now: None,
    };
    let (rows, total) = store::stack::list_mem_entries(state.pool(), &opts)
        .await
        .map_err(ApiError::from_db)?;
    let now = chrono::Local::now().fixed_offset();
    let out: Vec<Value> = rows.iter().map(|r| mem_json(r, now)).collect();
    let scope = state_scope_label(&auth, &uri, opts.scope.all);
    Ok(helpers::ok_json(json!({
        "memories": out,
        "total": total,
        "scope": scope,
    })))
}

/// GET /api/mem/lookup?namespace=&subject=&key= —— Agent 读一条记忆的主路径。
async fn lookup_mem(State(state): State<AppState>, auth: Auth, uri: Uri) -> ApiResult<Response> {
    let a = auth.require_scope("mem:read")?;
    let ns = state_namespace(
        &state,
        a,
        &web::query(&uri, "namespace").unwrap_or_default(),
    )
    .await?;
    let subject = web::query(&uri, "subject").unwrap_or_else(|| "self".to_string());
    let key = web::query(&uri, "key").unwrap_or_default();
    if key.is_empty() {
        return Err(ApiError::bad_request("bad_request", "缺 key"));
    }
    let now_txt = ncc_core::timeutil::now_go();
    let row = store::stack::get_mem_by_key(state.pool(), &ns.id, &subject, &key, Some(&now_txt))
        .await
        .map_err(ApiError::from_db)?;
    let Some(row) = row else {
        return Err(ApiError::not_found(format!(
            "没有这条记忆（或已过期）: {subject}/{key}"
        )));
    };
    Ok(helpers::ok_json(json!({
        "memory": mem_json(&row, chrono::Local::now().fixed_offset()),
    })))
}

/// 写记忆（upsert）。
#[derive(Debug, Deserialize, Default)]
struct MemPutReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    namespace: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    subject: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    key: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    value: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    kind: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    source: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    confidence: i64,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    pinned: bool,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    ttl_days: i64,
}

/// 写入前的自检。长度上限按**字节**算（Go 的 `len()`），别换成字符数 ——
/// 换个口径就意味着同一条记忆在 Go 侧被拒、在 Rust 侧能写进去。
fn mem_validate(
    subject: &str,
    key: &str,
    kind: &str,
    value: &str,
    confidence: i64,
    ttl_days: i64,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if subject.trim().is_empty() {
        out.push("缺 subject（谁的记忆；整个包共一份就写 self）".to_string());
    }
    if subject.len() > MEM_MAX_SUBJECT_LEN {
        out.push(format!("subject 太长（上限 {}）", MEM_MAX_SUBJECT_LEN));
    }
    if key.trim().is_empty() {
        out.push("缺 key（记忆的键）".to_string());
    }
    if key.len() > MEM_MAX_KEY_LEN {
        out.push(format!("key 太长（上限 {}）", MEM_MAX_KEY_LEN));
    }
    if !valid_mem_kind(kind) {
        out.push(format!(
            "kind 必须是 {}，当前是 {}",
            MEM_KINDS.iter().map(|k| k.0).collect::<Vec<_>>().join("|"),
            go_quote(kind)
        ));
    }
    if value.len() > MEM_MAX_VALUE_BYTES {
        out.push(format!(
            "value 太大（{} 字节，上限 {}）—— 更大的内容该是 kb 或 ckpt",
            value.len(),
            MEM_MAX_VALUE_BYTES
        ));
    }
    if !(0..=1000).contains(&confidence) {
        out.push(format!("confidence 是千分位 0..1000（收到 {confidence}）"));
    }
    if !(0..=3650).contains(&ttl_days) {
        out.push("ttl_days 要在 0..3650（0 = 不过期）".to_string());
    }
    out
}

/// PUT /api/mem —— 写一条记忆（同键即更新）。
async fn put_mem(State(state): State<AppState>, auth: Auth, body: Bytes) -> ApiResult<Response> {
    let a = auth.require_scope("mem:write")?;
    let mut req: MemPutReq = parse_body(&body)?;
    if req.subject.is_empty() {
        req.subject = "self".to_string();
    }
    if req.kind.is_empty() {
        req.kind = "fact".to_string();
    }
    let errs = mem_validate(
        &req.subject,
        &req.key,
        &req.kind,
        &req.value,
        req.confidence,
        req.ttl_days,
    );
    if !errs.is_empty() {
        return Err(ApiError::bad_request("mem_invalid", errs.join("; ")));
    }
    let ns = state_namespace(&state, a, &req.namespace).await?;
    mark_builtin_used(&state, &ns.id, "mem").await;
    let (row, created) = store::stack::upsert_mem_entry(
        state.pool(),
        &store::stack::MemInput {
            namespace_id: ns.id.clone(),
            subject: req.subject.clone(),
            key: req.key.clone(),
            value: req.value.clone(),
            kind: req.kind.clone(),
            tags: req.tags.clone(),
            source: req.source.clone(),
            confidence: req.confidence,
            pinned: req.pinned,
            ttl_days: req.ttl_days,
            author_id: a.user_id.clone(),
        },
    )
    .await
    .map_err(ApiError::from_db)?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(helpers::ok_status(
        status,
        json!({
            "memory": mem_json(&row, chrono::Local::now().fixed_offset()),
            "created": created,
            "revision": row.revision,
        }),
    ))
}

/// GET /api/mem/{id}
async fn get_mem(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    auth.require_scope("mem:read")?;
    let row = store::stack::get_mem_entry(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这条记忆"))?;
    let owner = row.owner_id.clone().unwrap_or_default();
    if !state_readable(&state, &auth, &headers, &row.namespace_id, &owner).await {
        return Err(ApiError::forbidden("这条记忆不在你可见的范围内"));
    }
    Ok(helpers::ok_json(json!({
        "memory": mem_json(&row, chrono::Local::now().fixed_offset()),
    })))
}

/// DELETE /api/mem/{id}
async fn delete_mem(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = auth.require_scope("mem:write")?;
    let row = store::stack::get_mem_entry(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这条记忆"))?;
    if !state_writable(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只有归属命名空间的成员能删这条记忆"));
    }
    store::stack::delete_mem_entry(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_json(
        json!({"ok": true, "id": row.id, "key": row.key}),
    ))
}

/// POST /api/mem/gc —— 真正删掉过期条目（读时已判过期，这里只是清垃圾）。
async fn gc_mem(State(state): State<AppState>, auth: Auth, uri: Uri) -> ApiResult<Response> {
    let a = auth.require_scope("mem:write")?;
    let ns = state_namespace(
        &state,
        a,
        &web::query(&uri, "namespace").unwrap_or_default(),
    )
    .await?;
    let now = ncc_core::timeutil::now_go();
    let removed = store::stack::gc_mem_entries(state.pool(), &ns.id, Some(&now))
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_json(json!({
        "ok": true,
        "removed": removed,
        "namespace": ns.slug,
    })))
}

/* ============================ 检查点 ckpt ============================ */

/// GET /api/ckpt?…
async fn list_ckpt(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    auth.require_scope("ckpt:read")?;
    let limit = query_atoi(&uri, "limit", 50);
    let limit = if limit <= 0 || limit > 200 { 50 } else { limit };
    let opts = store::stack::CkptListOpts {
        scope: state_scope_of(&state, &auth, &headers, &uri).await,
        subject_ref: web::query(&uri, "ref").unwrap_or_default(),
        label: web::query(&uri, "label").unwrap_or_default(),
        tag: web::query(&uri, "tag").unwrap_or_default(),
        status: web::query(&uri, "status").unwrap_or_default(),
        name: web::query(&uri, "q").unwrap_or_default(),
        limit,
        include_public: true,
    };
    let (rows, total) = store::stack::list_checkpoints(state.pool(), &opts)
        .await
        .map_err(ApiError::from_db)?;
    let out: Vec<Value> = rows.iter().map(|r| ckpt_json(&state, r, false)).collect();
    let scope = state_scope_label(&auth, &uri, opts.scope.all);
    Ok(helpers::ok_json(json!({
        "checkpoints": out,
        "total": total,
        "scope": scope,
    })))
}

/// 创建检查点（元数据）。
#[derive(Debug, Deserialize, Default)]
struct CkptCreateReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    namespace: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    subject_ref: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    subject_version: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    name: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    label: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    step: i64,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    summary: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    visibility: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    parent: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    digest: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_or_default")]
    size: i64,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    media_type: String,
    #[serde(default)]
    meta: Option<Value>,
}

/// 写入前的自检（字节面在服务端收到后再核对摘要）。
fn ckpt_validate(
    label: &str,
    name: &str,
    subject_ref: &str,
    digest: &str,
    size: i64,
    visibility: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !valid_ckpt_label(label) {
        out.push(format!(
            "label 必须是 {}，当前是 {}",
            CKPT_LABELS
                .iter()
                .map(|k| k.0)
                .collect::<Vec<_>>()
                .join("|"),
            go_quote(label)
        ));
    }
    if name.trim().is_empty() {
        out.push("缺 name（列表里先看到的就是它）".to_string());
    }
    // 引用一律用规范写法 `@命名空间/slug`：写 `alice/agent` 也能存进去，但按
    // `--ref @alice/agent` 过滤时就匹配不上了 —— 与其以后让人对着「存进去了却查不到」
    // 发愣，不如现在就要求写对。
    if !subject_ref.is_empty()
        && (!subject_ref.starts_with('@') || subject_ref.matches('/').count() != 1)
    {
        out.push(format!(
            "subject_ref 要么留空，要么写成 @命名空间/slug（收到 {}）",
            go_quote(subject_ref)
        ));
    }
    if !valid_visibility(visibility) {
        out.push(format!(
            "visibility 必须是 private|public，当前是 {}",
            go_quote(visibility)
        ));
    }
    if size < 0 {
        out.push("size 不能为负".to_string());
    }
    if size > CKPT_MAX_BYTES {
        out.push(format!(
            "太大（上限 {} 字节）—— 模型权重该走对象存储/制品",
            CKPT_MAX_BYTES
        ));
    }
    // size==0 且 digest 为空 = **先建元数据、字节稍后传**（合法的两步走：有的快照要
    // 先算很久，有的由另一个人上传字节）。这时两个字段必须**同时**空 —— 只声明摘要
    // 却没有大小，或者有大小却没摘要，都是打架的。
    if size > 0 && digest.is_empty() {
        out.push("给了 size 就也要 digest（服务端要用上传的字节重新核对）".to_string());
    } else if size == 0 && !digest.is_empty() {
        out.push("给了 digest 就要给 size（或者两个都不给 = 先建元数据、字节稍后传）".to_string());
    } else if !digest.is_empty()
        && (!digest.starts_with("sha256:") || digest.len() != "sha256:".len() + 64)
    {
        out.push("digest 必须是 sha256:<64 位十六进制>".to_string());
    }
    out
}

/// POST /api/ckpt —— 先建元数据，再 PUT 字节（两步：避免把大对象塞进 JSON）。
async fn create_ckpt(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth.require_scope("ckpt:write")?;
    let mut req: CkptCreateReq = parse_body(&body)?;
    if req.label.is_empty() {
        req.label = "manual".to_string();
    }
    if req.visibility.is_empty() {
        req.visibility = "private".to_string();
    }
    let errs = ckpt_validate(
        &req.label,
        &req.name,
        &req.subject_ref,
        &req.digest,
        req.size,
        &req.visibility,
    );
    if !errs.is_empty() {
        return Err(ApiError::bad_request("ckpt_invalid", errs.join("; ")));
    }
    // parent 必须存在且**你读得到**（别让血缘跨到看不见的别人的点上）。
    if !req.parent.is_empty() {
        let Some(p) = store::stack::get_checkpoint(state.pool(), &req.parent)
            .await
            .map_err(ApiError::from_db)?
        else {
            return Err(ApiError::bad_request(
                "bad_parent",
                format!("parent 不存在: {}", req.parent),
            ));
        };
        let owner = p.owner_id.clone().unwrap_or_default();
        if !p.namespace_id.is_empty()
            && !state_readable(&state, &auth, &headers, &p.namespace_id, &owner).await
        {
            return Err(ApiError::forbidden("parent 不在你可见的范围内"));
        }
    }
    let ns = state_namespace(&state, a, &req.namespace).await?;
    let meta = match &req.meta {
        Some(v) => v.to_string(),
        None => "{}".to_string(),
    };
    mark_builtin_used(&state, &ns.id, "ckpt").await;
    let row = store::stack::create_checkpoint(
        state.pool(),
        &store::stack::CkptInput {
            namespace_id: ns.id.clone(),
            subject_ref: req.subject_ref.clone(),
            subject_version: req.subject_version.clone(),
            name: req.name.clone(),
            label: req.label.clone(),
            step: req.step,
            summary: req.summary.clone(),
            tags: req.tags.clone(),
            visibility: req.visibility.clone(),
            parent: req.parent.clone(),
            digest: req.digest.clone(),
            size: req.size,
            media_type: req.media_type.clone(),
            meta,
            author_id: a.user_id.clone(),
        },
    )
    .await
    .map_err(ApiError::from_db)?;
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({
            "checkpoint": ckpt_json(&state, &row, false),
            "next": format!("PUT /api/ckpt/{}/blob （raw body，服务端会用 sha256 核对 digest）", row.id),
        }),
    ))
}

/// PUT /api/ckpt/{id}/blob —— 上传字节（服务端核对摘要后才落盘）。
async fn put_ckpt_blob(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
    body: Body,
) -> ApiResult<Response> {
    let a = auth.require_scope("ckpt:write")?;
    let row = store::stack::get_checkpoint(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这个检查点"))?;
    if !state_writable(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只有归属命名空间的成员能上传字节"));
    }
    if !row.object_key.is_empty() {
        return Err(ApiError::conflict(
            "already_uploaded",
            "这个检查点已经有字节了（检查点不可变：要改就再打一个点）",
        ));
    }
    let data = axum::body::to_bytes(body, CKPT_UPLOAD_MAX_BYTES + 1)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "请求体超过 256MB",
            )
        })?;
    if data.is_empty() {
        return Err(ApiError::bad_request("bad_request", "请求体为空或读取失败"));
    }
    if data.len() > CKPT_UPLOAD_MAX_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "请求体超过 256MB",
        ));
    }
    let got = format!("sha256:{}", ncc_core::crypto::sha256_hex(&data));
    if got != row.digest {
        return Err(ApiError::bad_request(
            "digest_mismatch",
            format!(
                "字节摘要与创建时声明的不一致（声明 {}，实际 {}）",
                row.digest, got
            ),
        ));
    }
    let object_key = format!("ckpt-{}-{}", row.id, ncc_core::crypto::rand_hex(6));
    if state.blobs().put(&object_key, &data).is_err() {
        return Err(ApiError::internal("写入字节失败"));
    }
    store::stack::set_checkpoint_object(state.pool(), &row.id, &object_key, data.len() as i64)
        .await
        .map_err(ApiError::from_db)?;
    let fresh = store::stack::get_checkpoint(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::internal("检查点刚写就不见了"))?;
    Ok(helpers::ok_json(json!({
        "ok": true,
        "checkpoint": ckpt_json(&state, &fresh, true),
    })))
}

/// GET /api/ckpt/{id}
async fn get_ckpt(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    auth.require_scope("ckpt:read")?;
    let row = store::stack::get_checkpoint(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这个检查点"))?;
    let owner = row.owner_id.clone().unwrap_or_default();
    if !row.is_public() && !state_readable(&state, &auth, &headers, &row.namespace_id, &owner).await
    {
        return Err(ApiError::forbidden("这个检查点不在你可见的范围内"));
    }
    Ok(helpers::ok_json(json!({
        "checkpoint": ckpt_json(&state, &row, true),
    })))
}

/// GET /api/ckpt/{id}/lineage
async fn ckpt_lineage(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    auth.require_scope("ckpt:read")?;
    let row = store::stack::get_checkpoint(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这个检查点"))?;
    let owner = row.owner_id.clone().unwrap_or_default();
    if !row.is_public() && !state_readable(&state, &auth, &headers, &row.namespace_id, &owner).await
    {
        return Err(ApiError::forbidden("这个检查点不在你可见的范围内"));
    }
    let rows = store::stack::checkpoint_lineage(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?;
    let out: Vec<Value> = rows.iter().map(|r| ckpt_json(&state, r, false)).collect();
    let count = out.len();
    Ok(helpers::ok_json(json!({"lineage": out, "count": count})))
}

/// GET /api/ckpt/{id}/bytes —— 字节流。
///
/// 与制品同规矩：要么带签名参数（服务端签发的短时地址），要么带能读它的凭据。
async fn ckpt_bytes(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    let row = store::stack::get_checkpoint(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这个检查点"))?;
    let exp = web::query(&uri, "exp").unwrap_or_default();
    let sig = web::query(&uri, "sig").unwrap_or_default();
    let owner = row.owner_id.clone().unwrap_or_default();
    let sig_ok = valid_ckpt_sig(state.cfg(), &row.id, &exp, &sig);
    let readable = state_readable(&state, &auth, &headers, &row.namespace_id, &owner).await;
    if !sig_ok && (!row.is_public() || !readable) {
        return Err(ApiError::forbidden("需要签名地址（bytesUrl）或可读凭据"));
    }
    if row.object_key.is_empty() {
        return Err(ApiError::conflict(
            "no_bytes",
            "这个检查点只有元数据，没有字节（创建后还没上传）",
        ));
    }
    let data = state
        .blobs()
        .get(&row.object_key)
        .map_err(|_| ApiError::not_found("字节已不在（可能被清理）"))?;
    let mut resp = web::bytes_response(data, &row.media_type, None);
    if let Ok(v) = axum::http::HeaderValue::from_str(&row.digest) {
        resp.headers_mut().insert("x-ncc-digest", v);
    }
    Ok(resp)
}

/// DELETE /api/ckpt/{id} —— 元数据与字节一起删。
async fn delete_ckpt(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let a = auth.require_scope("ckpt:write")?;
    let row = store::stack::get_checkpoint(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这个检查点"))?;
    if !state_writable(&state, &row.namespace_id, &a.user_id).await {
        return Err(ApiError::forbidden("只有归属命名空间的成员能删这个检查点"));
    }
    let key = store::stack::delete_checkpoint(state.pool(), &row.id)
        .await
        .map_err(ApiError::from_db)?
        .unwrap_or_default();
    if !key.is_empty() {
        let _ = state.blobs().delete(&key);
    }
    Ok(helpers::ok_json(json!({
        "ok": true,
        "id": row.id,
        "name": row.name,
        "bytes": !key.is_empty(),
    })))
}

/// POST /api/ckpt/prune?ref=&keep=N —— 每个 subject 只留最新 N 个。
///
/// **标 pruned + 删字节，元数据留下**：这样「这里曾经有个点、后来被清理了」仍然可查 ——
/// 删干净会让历史出现无法解释的空洞。
async fn prune_ckpt(State(state): State<AppState>, auth: Auth, uri: Uri) -> ApiResult<Response> {
    let a = auth.require_scope("ckpt:write")?;
    let keep = query_atoi(&uri, "keep", 5);
    if keep <= 0 {
        return Err(ApiError::bad_request(
            "bad_keep",
            "keep 必须大于 0（要全删请逐个 DELETE）",
        ));
    }
    let ref_ = web::query(&uri, "ref").unwrap_or_default();
    let ns = state_namespace(
        &state,
        a,
        &web::query(&uri, "namespace").unwrap_or_default(),
    )
    .await?;
    if ref_.is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺 ref（要清理哪个制品的检查点，如 @you/agent）",
        ));
    }
    if !state_writable(&state, &ns.id, &a.user_id).await {
        return Err(ApiError::forbidden("只有归属命名空间的成员能清理检查点"));
    }
    // 只清理**目标命名空间内**的点：跨命名空间写入必须拦住（见 store 侧的说明）。
    let doomed = store::stack::prune_checkpoints(state.pool(), &ns.id, &ref_, keep)
        .await
        .map_err(ApiError::from_db)?;
    let mut removed = 0;
    for k in &doomed {
        if state.blobs().delete(k).is_ok() {
            removed += 1;
        }
    }
    Ok(helpers::ok_json(json!({
        "ok": true,
        "ref": ref_,
        "keep": keep,
        "pruned": doomed.len(),
        "bytesRemoved": removed,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use ncc_core::storage::LocalStorage;
    use tower::ServiceExt;

    fn test_dir(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-stack-blobs")
            .join(format!("stack-{}-{name}", std::process::id()))
    }

    /// 每个测试一个临时文件库（不要用 `sqlite::memory:` + 连接池）。
    async fn state(name: &str) -> AppState {
        let dir = std::env::temp_dir().join(format!("ncc-stack-api-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL)
            .await
            .unwrap();
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://10.0.0.9:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();
        cfg.blob_dir = test_dir(name);
        let blobs = LocalStorage::new(&cfg.blob_dir, &cfg.public_url, "blobs").unwrap();
        let seal = ncc_core::secretbox::SecretBox::new(&cfg.jwt_secret).unwrap();
        AppState {
            cfg: std::sync::Arc::new(cfg),
            pool,
            blobs: std::sync::Arc::new(blobs),
            seal: std::sync::Arc::new(seal),
        }
    }

    fn app(state: &AppState) -> Router {
        Router::new()
            .merge(routes())
            .merge(public_routes())
            .with_state(state.clone())
    }

    /// 建一个用户 + 个人命名空间 + 一把带指定作用域的 API-Key。
    async fn user_with_key(st: &AppState, uid: &str, scopes: &[&str]) -> (String, String) {
        let u = store::users::create(
            st.pool(),
            &format!("用户{uid}"),
            &format!("{uid}@x.com"),
            "h",
        )
        .await
        .unwrap();
        store::namespaces::create_account(st.pool(), &u.id, &u.name, uid)
            .await
            .unwrap();
        let owned: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        let (_k, secret) = store::apikeys::create(st.pool(), &u.id, "t", &owned)
            .await
            .unwrap();
        (u.id, secret)
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    fn req(method: &str, uri: &str, token: Option<&str>, body: Value) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        let payload = if body.is_null() {
            Vec::new()
        } else {
            serde_json::to_vec(&body).unwrap()
        };
        b.header("content-type", "application/json")
            .body(Body::from(payload))
            .unwrap()
    }

    fn raw_req(method: &str, uri: &str, token: Option<&str>, body: Vec<u8>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::from(body)).unwrap()
    }

    fn kb_write() -> Vec<&'static str> {
        vec!["kb:write", "kb:read"]
    }

    /* ---- 词表 ---- */

    #[tokio::test]
    async fn kinds_三套词表() {
        let st = state("kinds").await;
        let app = app(&st);

        let (s, v) = call(&app, req("GET", "/kb/kinds", None, Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["resource"], "kb");
        assert_eq!(v["kinds"][0]["id"], "doc");
        assert_eq!(v["kinds"][0]["zh"], "文档");
        assert_eq!(v["limits"]["maxBytes"], 1 << 20);
        assert_eq!(v["visibilities"], json!(["private", "public"]));
        assert!(v["invariants"].as_array().unwrap().len() >= 5);

        let (s, v) = call(&app, req("GET", "/mem/kinds", None, Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["kinds"][0]["id"], "fact");
        assert_eq!(v["limits"]["maxKeyLen"], 128);
        assert_eq!(
            v["semantics"]["visibility"].as_str().unwrap().is_empty(),
            false
        );

        let (s, v) = call(&app, req("GET", "/ckpt/kinds", None, Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["labels"][5]["id"], "manual");
        assert_eq!(v["limits"]["signedUrlTtlSec"], 600);
        assert_eq!(v["limits"]["uploadMaxBytes"], 256 << 20);
    }

    /* ---- 知识库 ---- */

    #[tokio::test]
    async fn kb_全链路_建取改历史补丁删() {
        let st = state("kb-full").await;
        let app = app(&st);
        let (_uid, key) = user_with_key(&st, "zhangsan", &kb_write()).await;

        let (s, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({
                    "title": "部署手册",
                    "slug": "deploy",
                    "kind": "spec",
                    "summary": "怎么部署",
                    "tags": ["ops"],
                    "content": "第一版内容",
                    "note": "初稿"
                }),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        assert_eq!(v["created"], true);
        assert_eq!(v["doc"]["slug"], "deploy");
        assert_eq!(v["doc"]["kindLabel"], "规范");
        assert_eq!(v["revision"], 1);
        let doc_id = v["doc"]["id"].as_str().unwrap().to_string();
        assert!(v["doc"]["checksum"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));

        // 取详情（含内容）
        let (s, v) = call(
            &app,
            req("GET", &format!("/kb/{doc_id}"), Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["content"], "第一版内容");
        assert_eq!(v["namespace"]["slug"], "zhangsan");

        // 单段 id 与两段引用都能取到
        let (s, v) = call(
            &app,
            req("GET", "/kb/@zhangsan/deploy", Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["id"], doc_id);

        // 改内容 = 新版本
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({"title": "部署手册", "slug": "deploy", "kind": "spec", "content": "第二版内容", "note": "改一版"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["created"], false);
        assert_eq!(v["revision"], 2);

        // 历史（不回正文）
        let (s, v) = call(
            &app,
            req(
                "GET",
                &format!("/kb/{doc_id}/revisions"),
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["revisions"].as_array().unwrap().len(), 2);
        assert_eq!(v["revisions"][0]["note"], "初稿");
        assert!(v["revisions"][0].get("content").is_none());

        // 取旧版正文
        let (s, v) = call(
            &app,
            req(
                "GET",
                &format!("/kb/{doc_id}?revision=1"),
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["content"], "第一版内容");
        assert_eq!(v["requestedRevision"], 1);

        let (s, v) = call(
            &app,
            req(
                "GET",
                &format!("/kb/{doc_id}?revision=9"),
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["code"], "not_found");

        let (s, v) = call(
            &app,
            req(
                "GET",
                &format!("/kb/{doc_id}?revision=abc"),
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_revision");

        // 补丁：改名 + 公开（不加版本）
        let (s, v) = call(
            &app,
            req(
                "PATCH",
                &format!("/kb/{doc_id}"),
                Some(&key),
                json!({"title": "部署手册v2", "visibility": "public"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["doc"]["title"], "部署手册v2");
        assert_eq!(v["doc"]["revision"], 2);
        assert_eq!(v["doc"]["visibility"], "public");

        // 公开之后匿名也能读
        let (s, v) = call(
            &app,
            req("GET", &format!("/kb/{doc_id}"), None, Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["title"], "部署手册v2");

        // 删除
        let (s, v) = call(
            &app,
            req("DELETE", &format!("/kb/{doc_id}"), Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], true);
        let (s, _) = call(
            &app,
            req("GET", &format!("/kb/{doc_id}"), Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn kb_权限_作用域与跨空间() {
        let st = state("kb-perm").await;
        let app = app(&st);
        // 只给读的 key 不能写
        let (_u1, ro) = user_with_key(&st, "reader", &["kb:read"]).await;
        let (_u2, wo) = user_with_key(&st, "writer", &kb_write()).await;

        let (s, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&ro),
                json!({"title": "x", "content": "y"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "forbidden");
        assert!(v["error"]["message"].as_str().unwrap().contains("kb:write"));

        // 匿名不能写（401）
        let (s, _) = call(
            &app,
            req("POST", "/kb", None, json!({"title": "x", "content": "y"})),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);

        // writer 建一篇私有文档
        let (_, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&wo),
                json!({"title": "secret", "content": "内部"}),
            ),
        )
        .await;
        let doc_id = v["doc"]["id"].as_str().unwrap().to_string();

        // 别人读不到（403，且**不泄漏存在性以外的信息**：Go 也是 403）
        let (s, v) = call(
            &app,
            req("GET", &format!("/kb/{doc_id}"), Some(&ro), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "forbidden");

        // 匿名读不到
        let (s, _) = call(
            &app,
            req("GET", &format!("/kb/{doc_id}"), None, Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // 列表里也看不到别人的私有文档
        let (s, v) = call(&app, req("GET", "/kb", Some(&ro), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 0);
        assert_eq!(v["scope"], "visible");

        // 匿名列表：只有公开档，scope=public
        let (s, v) = call(&app, req("GET", "/kb", None, Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["scope"], "public");
        assert_eq!(v["total"], 0);

        // 未知类型
        let (s, v) = call(&app, req("GET", "/kb?kind=nope", None, Value::Null)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_kind");
    }

    #[tokio::test]
    async fn kb_授权后可读_且公开档进列表() {
        let st = state("kb-grant").await;
        let app = app(&st);
        let (owner, wo) = user_with_key(&st, "owner", &kb_write()).await;
        let (friend, friend_key) = user_with_key(&st, "friend", &["kb:read"]).await;

        let (_, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&wo),
                json!({"title": "handbook", "slug": "handbook", "content": "内容"}),
            ),
        )
        .await;
        let doc_id = v["doc"]["id"].as_str().unwrap().to_string();

        // 把 state 读权授给 friend（一条不限命名空间的授权）
        store::grants::create(st.pool(), &owner, &friend, "state", "", "")
            .await
            .unwrap();

        // 没有凭据仍然读不到（授权是给具体人的）
        let (s, _) = call(
            &app,
            req("GET", &format!("/kb/{doc_id}"), None, Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // 拿到授权的 friend 能按引用读到（owner 维度授权）
        let (s, v) = call(
            &app,
            req(
                "GET",
                &format!("/kb/{doc_id}"),
                Some(&friend_key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");

        let (s, v) = call(&app, req("GET", "/kb", Some(&friend_key), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 1);

        // 公开档：匿名列表能看见（scope=public）
        let (s, v) = call(
            &app,
            req(
                "PATCH",
                &format!("/kb/{doc_id}"),
                Some(&wo),
                json!({"visibility": "public"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let (s, v) = call(&app, req("GET", "/kb", None, Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["docs"][0]["slug"], "handbook");

        // mine=1 只看自己的空间（赠人者视角：一条都在自己空间里）
        let (s, v) = call(&app, req("GET", "/kb?mine=1", Some(&wo), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["scope"], "mine");
    }

    #[tokio::test]
    async fn kb_bundle_带内容_缺_ns_报错() {
        let st = state("kb-bundle").await;
        let app = app(&st);
        let (_uid, key) = user_with_key(&st, "zhangsan", &kb_write()).await;
        call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({"title": "aa", "slug": "aa", "content": "AAA"}),
            ),
        )
        .await;
        call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({"title": "bb", "slug": "bb", "content": "BBB"}),
            ),
        )
        .await;

        let (s, v) = call(
            &app,
            req(
                "GET",
                "/kb/bundle?namespace=@zhangsan",
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["namespace"], "zhangsan");
        assert_eq!(v["count"], 2);
        assert_eq!(v["docs"][0]["content"].as_str().unwrap().len(), 3);

        let (s, v) = call(&app, req("GET", "/kb/bundle", Some(&key), Value::Null)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_request");
    }

    #[tokio::test]
    async fn kb_非法输入_逐条报错() {
        let st = state("kb-bad").await;
        let app = app(&st);
        let (_uid, key) = user_with_key(&st, "zhangsan", &kb_write()).await;

        // slug 校验排在内容校验之前（与 Go 同序）：title 全中文且没给 slug 时，
        // slugify 会得到 "x"，过不了 2..64 的规则 —— Go 也是这个结果。
        let (s, v) = call(
            &app,
            req("POST", "/kb", Some(&key), json!({"content": "x"})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_slug");

        // 缺 title（给了合法 slug 之后才轮到内容校验）
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({"slug": "abc", "content": "x"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "kb_invalid");
        assert!(v["error"]["message"].as_str().unwrap().contains("缺 title"));

        // 未知 kind（报错里带 Go 风格引号，不转义中文）
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({"title": "x", "slug": "abc", "kind": "中文类"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("当前是 \"中文类\""));

        // 非法 slug
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({"title": "x", "slug": "A"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_slug");

        // 非法 JSON
        let (s, v) = call(&app, raw_req("POST", "/kb", Some(&key), b"{".to_vec())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_json");

        // 标签太多
        let many: Vec<String> = (0..40).map(|i| format!("t{i}")).collect();
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({"title": "x", "slug": "abc", "tags": many}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("tags 太多"));

        // 写进别人的命名空间 → 403
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/kb",
                Some(&key),
                json!({"title": "x", "slug": "abc", "namespace": "别人的空间"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "forbidden");
    }

    /* ---- 记忆 ---- */

    #[tokio::test]
    async fn mem_写入_查询_列表_gc_删除() {
        let st = state("mem-flow").await;
        let app = app(&st);
        let (_uid, key) = user_with_key(&st, "zhangsan", &["mem:write", "mem:read"]).await;

        let (s, v) = call(
            &app,
            req(
                "PUT",
                "/mem",
                Some(&key),
                json!({"key": "lang", "value": "rust", "kind": "preference", "confidence": 900}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        assert_eq!(v["created"], true);
        assert_eq!(v["memory"]["subject"], "self");
        assert_eq!(v["memory"]["kindLabel"], "偏好");
        assert_eq!(v["memory"]["revision"], 1);
        assert!(v["memory"].get("expiresAt").is_none());
        let id = v["memory"]["id"].as_str().unwrap().to_string();

        // 同键再写 = 更新
        let (s, v) = call(
            &app,
            req(
                "PUT",
                "/mem",
                Some(&key),
                json!({"key": "lang", "value": "go"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["created"], false);
        assert_eq!(v["revision"], 2);

        // 主路径 lookup
        let (s, v) = call(
            &app,
            req("GET", "/mem/lookup?key=lang", Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["memory"]["value"], "go");

        let (s, v) = call(
            &app,
            req("GET", "/mem/lookup?key=没有这条", Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["code"], "not_found");

        let (s, _) = call(&app, req("GET", "/mem/lookup", Some(&key), Value::Null)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // 详情（按 id）
        let (s, v) = call(
            &app,
            req("GET", &format!("/mem/{id}"), Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["memory"]["key"], "lang");

        // 列表
        let (s, v) = call(&app, req("GET", "/mem", Some(&key), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["memories"][0]["key"], "lang");

        // TTL 过期：读时判定 + gc 真删
        let (s, v) = call(
            &app,
            req(
                "PUT",
                "/mem",
                Some(&key),
                json!({"key": "tmp", "value": "x", "ttl_days": 1}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED);
        assert!(v["memory"]["expiresAt"].as_str().unwrap().ends_with('Z'));
        assert_eq!(v["memory"]["expired"], false);

        // 把 TTL 改成过去 → 立刻视为不存在
        sqlx::query("UPDATE mem_entries SET expires_at = ? WHERE key = 'tmp'")
            .bind("2000-01-01 00:00:00+08:00")
            .execute(st.pool())
            .await
            .unwrap();
        let (s, _) = call(
            &app,
            req("GET", "/mem/lookup?key=tmp", Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, v) = call(&app, req("GET", "/mem", Some(&key), Value::Null)).await;
        assert_eq!(v["total"], 1);
        // 显式要过期项才看得到，并且带 expired=true
        let (s, v) = call(&app, req("GET", "/mem?expired=1", Some(&key), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 2);
        let expired = v["memories"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["key"] == "tmp")
            .unwrap();
        assert_eq!(expired["expired"], true);

        // gc
        let (s, v) = call(&app, req("POST", "/mem/gc", Some(&key), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["removed"], 1);
        assert_eq!(v["namespace"], "zhangsan");

        // 删除
        let (s, v) = call(
            &app,
            req("DELETE", &format!("/mem/{id}"), Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert_eq!(v["key"], "lang");
    }

    #[tokio::test]
    async fn mem_权限与非法输入() {
        let st = state("mem-perm").await;
        let app = app(&st);
        let (_u, ro) = user_with_key(&st, "reader", &["mem:read"]).await;
        let (_u2, wo) = user_with_key(&st, "writer", &["mem:write", "mem:read"]).await;

        // 没作用域（匿名）
        let (s, _) = call(
            &app,
            req("PUT", "/mem", None, json!({"key": "k", "value": "v"})),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        // 只读的 key 不能写
        let (s, _) = call(
            &app,
            req("PUT", "/mem", Some(&ro), json!({"key": "k", "value": "v"})),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        // 写作用域的 key 能读（写蕴含读）
        let (s, _) = call(&app, req("GET", "/mem", Some(&wo), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);

        // 非法：缺 key
        let (s, v) = call(&app, req("PUT", "/mem", Some(&wo), json!({"value": "v"}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "mem_invalid");
        assert!(v["error"]["message"].as_str().unwrap().contains("缺 key"));

        // 非法：confidence 越界
        let (s, v) = call(
            &app,
            req(
                "PUT",
                "/mem",
                Some(&wo),
                json!({"key": "k", "value": "v", "confidence": 1001}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"].as_str().unwrap().contains("千分位"));

        // 非法：ttl 越界
        let (s, v) = call(
            &app,
            req(
                "PUT",
                "/mem",
                Some(&wo),
                json!({"key": "k", "value": "v", "ttl_days": 9999}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"].as_str().unwrap().contains("ttl_days"));

        // 非法：value 太大
        let big = "x".repeat(MEM_MAX_VALUE_BYTES + 1);
        let (s, v) = call(
            &app,
            req("PUT", "/mem", Some(&wo), json!({"key": "k", "value": big})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("value 太大"));

        // 别人的私忆看不到
        let (_, v) = call(
            &app,
            req(
                "PUT",
                "/mem",
                Some(&wo),
                json!({"key": "mine", "value": "私人"}),
            ),
        )
        .await;
        let id = v["memory"]["id"].as_str().unwrap().to_string();
        let (s, _) = call(
            &app,
            req("GET", &format!("/mem/{id}"), Some(&ro), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    /* ---- 检查点 ---- */

    #[tokio::test]
    async fn ckpt_建点_传字节_血缘_签名下载_prune_删除() {
        let st = state("ckpt-full").await;
        let app = app(&st);
        let (_uid, key) = user_with_key(&st, "zhangsan", &["ckpt:write", "ckpt:read"]).await;

        let payload = b"checkpoint-bytes".to_vec();
        let digest = format!("sha256:{}", ncc_core::crypto::sha256_hex(&payload));

        // 元数据点（size+digest 先声明，字节后传）
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt",
                Some(&key),
                json!({
                    "name": "第一步",
                    "label": "run",
                    "subject_ref": "@zhangsan/agent",
                    "digest": digest,
                    "size": payload.len(),
                    "media_type": "application/octet-stream",
                    "meta": {"loss": 0.1},
                    "tags": ["train"]
                }),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        assert_eq!(v["checkpoint"]["labelText"], "运行");
        assert_eq!(v["checkpoint"]["meta"]["loss"], 0.1);
        assert!(v["next"].as_str().unwrap().contains("/blob"));
        let id = v["checkpoint"]["id"].as_str().unwrap().to_string();

        // 摘要对不上 → 400
        let (s, v) = call(
            &app,
            raw_req(
                "PUT",
                &format!("/ckpt/{id}/blob"),
                Some(&key),
                b"wrong".to_vec(),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "digest_mismatch");

        // 正常上传
        let (s, v) = call(
            &app,
            raw_req(
                "PUT",
                &format!("/ckpt/{id}/blob"),
                Some(&key),
                payload.clone(),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["checkpoint"]["size"], payload.len());
        assert!(v["checkpoint"]["bytesUrl"]
            .as_str()
            .unwrap()
            .contains("sig="));

        // 重复上传 → 409（检查点不可变）
        let (s, v) = call(
            &app,
            raw_req(
                "PUT",
                &format!("/ckpt/{id}/blob"),
                Some(&key),
                payload.clone(),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        assert_eq!(v["error"]["code"], "already_uploaded");

        // 一个子点，血缘两代
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt",
                Some(&key),
                json!({"name": "第二步", "parent": id, "subject_ref": "@zhangsan/agent"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let child = v["checkpoint"]["id"].as_str().unwrap().to_string();
        let (s, v) = call(
            &app,
            req(
                "GET",
                &format!("/ckpt/{child}/lineage"),
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["count"], 2);
        assert_eq!(v["lineage"][0]["id"], child);

        // 详情带签名地址
        let (s, v) = call(
            &app,
            req("GET", &format!("/ckpt/{id}"), Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let url = v["checkpoint"]["bytesUrl"].as_str().unwrap().to_string();
        assert_eq!(v["checkpoint"]["bytesTtlSec"], 600);

        // 没有凭据但带签名 → 200，且头部带摘要
        let uri = url.replace("http://10.0.0.9:8282/api", "");
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("x-ncc-digest")
                .unwrap()
                .to_str()
                .unwrap(),
            digest
        );
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(body.to_vec(), payload);

        // 匿名裸读（没有签名）→ 403
        let (s, v) = call(
            &app,
            req("GET", &format!("/ckpt/{id}/bytes"), None, Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "forbidden");

        // 伪造签名 → 403
        let (s, _) = call(
            &app,
            req(
                "GET",
                &format!("/ckpt/{id}/bytes?exp=9999999999&sig=伪造"),
                None,
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // 私有检查点裸读（没有签名）→ 403：判据里 `visibility != public` 那一半就成立，
        // 与「是不是成员」无关（与 Go 一致）。
        let (s, v) = call(
            &app,
            req(
                "GET",
                &format!("/ckpt/{child}/bytes"),
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "forbidden");

        // 带签名就能走到「只有元数据、没有字节」这一支：409 no_bytes
        let sig_of = |id: &str| -> String {
            let exp = ncc_core::timeutil::now_unix() + 600;
            let sig =
                ncc_core::crypto::hmac_sha256_b64url("test-secret", &format!("ckpt:{id}|{exp}"));
            format!("/ckpt/{id}/bytes?exp={exp}&sig={sig}")
        };
        let (s, v) = call(&app, req("GET", &sig_of(&child), None, Value::Null)).await;
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"]["code"], "no_bytes");

        // 把时间拨开，让 prune 的顺序确定（created_at 精度到微秒，同批建的点可能同刻）
        sqlx::query("UPDATE checkpoints SET created_at = '2026-01-01 00:00:00+08:00' WHERE id = ?")
            .bind(&id)
            .execute(st.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE checkpoints SET created_at = '2026-01-02 00:00:00+08:00' WHERE id = ?")
            .bind(&child)
            .execute(st.pool())
            .await
            .unwrap();

        // prune：只留最新 1 个（child 最新，留下；带字节的 id 被清理）
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt/prune?ref=@zhangsan/agent&keep=1",
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["pruned"], 1);
        assert_eq!(v["bytesRemoved"], 1);
        assert_eq!(v["ref"], "@zhangsan/agent");

        // 被清理的点元数据还在（status=pruned），字节已经不在了
        let (s, v) = call(
            &app,
            req("GET", &format!("/ckpt/{id}"), Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["checkpoint"]["status"], "pruned");
        let (s, v) = call(&app, req("GET", &sig_of(&id), None, Value::Null)).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["code"], "not_found");

        // 按状态过滤能查出来（默认列表只剩 active 的 child）
        let (s, v) = call(
            &app,
            req("GET", "/ckpt?status=pruned", Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["scope"], "visible");
        let (s, v) = call(&app, req("GET", "/ckpt", Some(&key), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["checkpoints"][0]["id"], child);

        // keep<=0 由上层拒掉
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt/prune?ref=@zhangsan/agent&keep=0",
                Some(&key),
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_keep");

        // 缺 ref
        let (s, v) = call(
            &app,
            req("POST", "/ckpt/prune?keep=2", Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_request");

        // 删除：元数据一起走
        let (s, v) = call(
            &app,
            req("DELETE", &format!("/ckpt/{id}"), Some(&key), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert_eq!(v["name"], "第一步");
        // `bytes` 说的是「行上还有对象名」：prune 只删字节、不擦元数据，
        // 所以被清理过的点仍然回报 true（与 Go 一致）。
        assert_eq!(v["bytes"], true);
    }

    #[tokio::test]
    async fn ckpt_校验与父子可见性() {
        let st = state("ckpt-bad").await;
        let app = app(&st);
        let (_u, key) = user_with_key(&st, "zhangsan", &["ckpt:write", "ckpt:read"]).await;
        let (_u2, other) = user_with_key(&st, "lisi", &["ckpt:write", "ckpt:read"]).await;

        // 缺 name
        let (s, v) = call(&app, req("POST", "/ckpt", Some(&key), json!({}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "ckpt_invalid");
        assert!(v["error"]["message"].as_str().unwrap().contains("缺 name"));

        // 未知 label
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt",
                Some(&key),
                json!({"name": "x", "label": "nope"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("label 必须是"));

        // subject_ref 写法不对
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt",
                Some(&key),
                json!({"name": "x", "subject_ref": "alice/agent"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("subject_ref"));

        // 给了 size 没给 digest
        let (s, v) = call(
            &app,
            req("POST", "/ckpt", Some(&key), json!({"name": "x", "size": 3})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("也要 digest"));

        // 给了 digest 没给 size
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt",
                Some(&key),
                json!({"name": "x", "digest": format!("sha256:{}", "0".repeat(64))}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("就要给 size"));

        // digest 形态不对
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt",
                Some(&key),
                json!({"name": "x", "size": 3, "digest": "md5:abc"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"].as_str().unwrap().contains("sha256:"));

        // parent 不存在
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt",
                Some(&key),
                json!({"name": "x", "parent": "CK-没有这个"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "bad_parent");

        // 建一个自己的点，然后别人拿它当 parent → 403（血缘不跨可见范围）
        let (_, v) = call(
            &app,
            req("POST", "/ckpt", Some(&key), json!({"name": "mine"})),
        )
        .await;
        let mine = v["checkpoint"]["id"].as_str().unwrap().to_string();
        let (s, v) = call(
            &app,
            req(
                "POST",
                "/ckpt",
                Some(&other),
                json!({"name": "steal", "parent": mine}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "forbidden");

        // 别人上传我的点字节 → 403
        let (s, _) = call(
            &app,
            raw_req(
                "PUT",
                &format!("/ckpt/{mine}/blob"),
                Some(&other),
                b"x".to_vec(),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);

        // 别人看我的点 → 403（私有）
        let (s, _) = call(
            &app,
            req("GET", &format!("/ckpt/{mine}"), Some(&other), Value::Null),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        // ckpt 列表要 ckpt:read 作用域（与 Go 一致）：匿名一律 401
        let (s, _) = call(&app, req("GET", "/ckpt", None, Value::Null)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        // 有凭据的人列表里看不到别人的私有检查点
        let (s, v) = call(&app, req("GET", "/ckpt", Some(&other), Value::Null)).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["total"], 0);
        assert_eq!(v["scope"], "visible");
    }

    /* ---- 单元 ---- */

    /// 路由表能装起来就说明没有冲突：`Router::merge` 撞了会 panic。
    ///
    /// 本族一个路径注册了两种形态（`{id}` 单段 / `{id}/{slug}` 两段），还和 `/kb/kinds`、
    /// `/ckpt/prune` 这种静态段同处一棵树 —— axum 允许静态优先，但写错就会在启动时炸，
    /// 所以这里把全量路由表装一次。
    #[test]
    fn 全量路由表_无冲突() {
        let _ = crate::router::api_router();
        let _ = crate::router::public_router();
        let _ = routes();
        let _ = public_routes();
    }

    #[test]
    fn 引用拼接与_go_风格引号() {
        let one = HashMap::from([("id".to_string(), "KD-1".to_string())]);
        assert_eq!(ref_from_params(&one), "KD-1");
        let two = HashMap::from([
            ("id".to_string(), "@ns".to_string()),
            ("slug".to_string(), "slug".to_string()),
        ]);
        assert_eq!(ref_from_params(&two), "@ns/slug");
        let empty = HashMap::from([
            ("id".to_string(), "KD-1".to_string()),
            ("slug".to_string(), String::new()),
        ]);
        assert_eq!(ref_from_params(&empty), "KD-1");
        assert_eq!(go_quote("doc"), "\"doc\"");
        assert_eq!(go_quote("中文"), "\"中文\"");
        assert_eq!(go_quote("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn 校验清单_与_go_逐条对齐() {
        assert!(kb_validate("", "doc", "markdown", "private", "", &[]).len() == 1);
        assert_eq!(
            kb_validate("t", "doc", "markdown", "private", "", &[]) as Vec<String>,
            Vec::<String>::new()
        );
        assert_eq!(
            mem_validate("self", "k", "fact", "v", 0, 0),
            Vec::<String>::new()
        );
        assert_eq!(mem_validate("", "", "fact", "v", 0, 0).len(), 2);
        assert!(ckpt_validate("manual", "n", "", "", 0, "private").is_empty());
        // size>0 且 digest 空 → 一条错
        assert_eq!(ckpt_validate("manual", "n", "", "", 3, "private").len(), 1);
        // 合法 digest
        let d = format!("sha256:{}", "a".repeat(64));
        assert!(ckpt_validate("manual", "n", "", &d, 3, "private").is_empty());
    }

    #[test]
    fn 签名_域前缀不可顶替() {
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://n:8282".to_string();
        cfg.jwt_secret = "s3cret".to_string();

        let url = ckpt_bytes_url(&cfg, "CK-1", 600);
        assert!(url.starts_with("http://n:8282/api/ckpt/CK-1/bytes?exp="));
        let query = url.split('?').nth(1).unwrap();
        let mut exp = String::new();
        let mut sig = String::new();
        for pair in query.split('&') {
            let (k, v) = pair.split_once('=').unwrap();
            match k {
                "exp" => exp = v.to_string(),
                "sig" => sig = v.to_string(),
                _ => {}
            }
        }
        assert!(valid_ckpt_sig(&cfg, "CK-1", &exp, &sig));
        // 换个 id 就不成立（签名绑 id）
        assert!(!valid_ckpt_sig(&cfg, "CK-2", &exp, &sig));
        // 换 secret 就不成立
        cfg.jwt_secret = "别的密钥".to_string();
        assert!(!valid_ckpt_sig(&cfg, "CK-1", &exp, &sig));
        // 过期
        cfg.jwt_secret = "s3cret".to_string();
        assert!(!valid_ckpt_sig(&cfg, "CK-1", "1", &sig));
        assert!(!valid_ckpt_sig(&cfg, "CK-1", "不是数字", &sig));
    }

    #[test]
    fn 查询整数_与_go_的_atoi_default_对齐() {
        let uri: Uri = "/mem?limit=5".parse().unwrap();
        assert_eq!(query_atoi(&uri, "limit", 200), 5);
        let uri: Uri = "/mem".parse().unwrap();
        assert_eq!(query_atoi(&uri, "limit", 200), 200);
        // 键在但值为空 → 0（由调用方兜底成默认上限）
        let uri: Uri = "/mem?limit=".parse().unwrap();
        assert_eq!(query_atoi(&uri, "limit", 200), 0);
        let uri: Uri = "/mem?limit=abc".parse().unwrap();
        assert_eq!(query_atoi(&uri, "limit", 200), 0);
    }

    #[test]
    fn 时间_输出_utc_rfc3339() {
        assert_eq!(
            rfc3339_utc(Some("2026-09-07 01:16:25.887763+08:00")),
            "2026-09-06T17:16:25Z"
        );
        assert_eq!(rfc3339_utc(None), "");
        assert_eq!(rfc3339_utc(Some("不是时间")), "");
    }
}
