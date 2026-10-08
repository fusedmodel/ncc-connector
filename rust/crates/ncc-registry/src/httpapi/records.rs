//! 通用记录仓（NCC Store）的 HTTP 层：集合声明 + 记录 CRUD + 历史 + 索引副本之外的 GC。
//!
//! 原实现：`ncc-registry/httpapi/store.go`。
//!
//! 一句话：**集合是声明，记录是数据，服务端不认识业务字段。**
//!
//! 为什么值得有这一层：知识库 / 记忆 / 检查点 / 轨迹是四类内容，机械部分却是同一件事
//! （归属命名空间、按 key 取、分页、标签、可见性、版本、上限、过期、软删）。各写一遍
//! 就是抄四遍，再加一类内容（issue / log / …）再抄一遍。于是把「声明」与「数据」分开：
//! 新增一类内容**不改服务端**，声明一个集合即可。所以「集合声明」的字段（键规则、
//! 是否版本化、检索字段）与校验必须与 Go 一致 —— 那是这一族的契约面。
//!
//! 三条红线（写在实现里，别在某个 handler 里破掉）：
//!
//! 1. **CRUD ≠ 授权**：能写一个集合不等于能读别人的记录 —— 仍按命名空间归属 + 授权判。
//! 2. **动态 ≠ 无模式**：没在 `fields` 声明的字段写不进来（400），没在 `index` 声明的
//!    字段不能当过滤条件（也是 400，而不是默默返回空）。
//! 3. **不可变就是不可变**：`mutable=false` / `append_only=true` 的集合没有 PUT。
//!
//! 刻意的取舍：
//!
//! * 请求体走 `Bytes` + 手工 `serde_json`：先判作用域再判 body，与 Go 里
//!   `requireScope` 中间件早于 `ShouldBindJSON` 的顺序一致，非法 body 回 400
//!   `bad_request`（而不是 axum `Json<T>` 默认的 422）。
//! * **匿名写会被判成 403 而不是 panic**：Go 那边 `a.UserID` 在匿名时是空指针解引用
//!   （500）；这里按「无凭据 = 空用户名」处理 —— 结论一样是「不许写」，但不会 500。
//! * `meta` 的 JSON `null` 与「没给」等价（Go 会把 `null` 原样存成 `"null"`）：
//!   差别只在回显时 `meta` 是 `null` 还是 `{}`，语义一致。
//! * 未知字段的报错（`bad_field`）**按字段名排序**报第一个：Go 遍历 map 的顺序是随机的，
//!   同一个请求两次可能报不同的字段名；这里定死顺序，便于断言。
//! * 时间字段照库里存的文本回（与其它已迁移族一致），不做 RFC3339 转换。

use std::collections::{HashMap, HashSet};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Deserializer};
use serde_json::{json, Map, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::timeutil::{format_go, now_go};
use ncc_core::web;

use crate::httpapi::{helpers, AppState, Auth};
use crate::store;
use crate::store::namespaces::Namespace;
use crate::store::state::{Collection, Record};

/// 该族路由（相对 `/api`）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/store/kinds", get(store_kinds))
        .route(
            "/store",
            get(list_store_collections).post(declare_store_collection),
        )
        .route(
            "/store/",
            get(list_store_collections).post(declare_store_collection),
        )
        // 静态段优先于 `{col}`（`/store/gc` 不会被当成集合名 gc）。
        .route("/store/gc", post(gc_store_records))
        .route(
            "/store/{col}",
            get(list_store_records)
                .post(upsert_store_record)
                .delete(archive_store_collection),
        )
        .route(
            "/store/{col}/{key}",
            get(get_store_record)
                .put(put_store_record)
                .delete(delete_store_record),
        )
        .route("/store/{col}/{key}/history", get(store_record_history))
}

/// 本族没有顶层公开页（记录读口在 `/api/store/...` 下）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
}

/* ---------------- 集合声明：字段的语法与校验 ---------------- */

/// 一个声明字段：`string` / `text` / `int` / `bool` / `string[]` / `ref` / `enum:a|b`。
#[derive(Debug, Clone, PartialEq)]
struct FieldSpec {
    name: String,
    ftype: String,
    enums: Vec<String>,
    /// 这个字段的正文进搜索文本（`?q=` 能搜到）。
    search: bool,
    /// 写入必填。
    require: bool,
}

impl FieldSpec {
    /// 视图形态：`enum` / `search` / `require` 为空时**不出现**（Go 的 `omitempty`）。
    fn to_json(&self) -> Value {
        let mut o = Map::new();
        o.insert("name".into(), json!(self.name));
        o.insert("type".into(), json!(self.ftype));
        if !self.enums.is_empty() {
            o.insert("enum".into(), json!(self.enums));
        }
        if self.search {
            o.insert("search".into(), json!(true));
        }
        if self.require {
            o.insert("require".into(), json!(true));
        }
        Value::Object(o)
    }
}

/// 解析一条字段声明。语法故意简单到「看一眼就懂」：
///
/// ```text
/// "title:string"            必填？不，默认可选
/// "status:enum:open|closed"
/// "labels:string[]"
/// "body:text?search"
/// "owner:ref!"
/// ```
///
/// 尾部 `!` = 必填，`?search` = 进搜索文本。
fn parse_field_spec(raw: &str) -> Result<FieldSpec, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("空字段声明".to_string());
    }
    let (name, mut typ) = match s.find(':') {
        Some(i) if i > 0 => (&s[..i], &s[i + 1..]),
        _ => (s, "string"),
    };
    let name = name.trim();
    if !store::state::valid_kind(name) {
        return Err(format!(
            "字段名「{name}」不合法（小写字母数字与 -_.，字母开头）"
        ));
    }
    let mut fs = FieldSpec {
        name: name.to_string(),
        ftype: String::new(),
        enums: Vec::new(),
        search: false,
        require: false,
    };
    // 尾修饰可以叠加（`text?search!`），与 Go 的循环一致。
    loop {
        if let Some(t) = typ.strip_suffix('!') {
            fs.require = true;
            typ = t;
            continue;
        }
        if let Some(t) = typ.strip_suffix("?search") {
            fs.search = true;
            typ = t;
            continue;
        }
        break;
    }
    let typ = typ.trim();
    if let Some(rest) = typ.strip_prefix("enum:") {
        let vals: Vec<String> = rest.split('|').map(|v| v.trim().to_string()).collect();
        if vals.iter().any(|v| v.is_empty()) {
            return Err(format!("字段「{name}」的 enum 里有空值"));
        }
        fs.ftype = "enum".to_string();
        fs.enums = vals;
        return Ok(fs);
    }
    if !["string", "text", "int", "bool", "string[]", "ref"].contains(&typ) {
        return Err(format!(
            "字段「{name}」的类型「{typ}」不在白名单（string/text/int/bool/string[]/ref/enum:a|b）"
        ));
    }
    fs.ftype = typ.to_string();
    Ok(fs)
}

/// 解析集合的字段声明数组。
fn parse_fields(raw: &[String]) -> Result<Vec<FieldSpec>, String> {
    if raw.len() > store::state::MAX_FIELDS {
        return Err(format!(
            "字段最多 {} 个（收到 {}）—— 一个集合不是一张宽表",
            store::state::MAX_FIELDS,
            raw.len()
        ));
    }
    let mut out: Vec<FieldSpec> = Vec::with_capacity(raw.len());
    let mut seen: Vec<String> = Vec::new();
    for r in raw {
        let fs = parse_field_spec(r)?;
        if seen.contains(&fs.name) {
            return Err(format!("字段「{}」重复声明", fs.name));
        }
        seen.push(fs.name.clone());
        out.push(fs);
    }
    Ok(out)
}

/// 把存起来的 JSON 还原成字段声明；脏数据一律当「没有声明」（展示路径不该炸）。
fn decode_fields(s: &str) -> Vec<FieldSpec> {
    if s.trim().is_empty() {
        return Vec::new();
    }
    let raw: Vec<String> = match serde_json::from_str(s) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    parse_fields(&raw).unwrap_or_default()
}

/// 按声明校验一个字段值，返回它的**字符串形态**（索引用）。
fn validate_field_value(f: &FieldSpec, v: &Value) -> Result<Vec<String>, String> {
    let as_str = || {
        v.as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| format!("字段「{}」要字符串", f.name))
    };
    match f.ftype.as_str() {
        "string" | "text" | "ref" => {
            let s = as_str()?;
            // `ref`：引用。**不强行要求 `@ns/slug` 两段** ——「指派给 @alice 这个人」
            // 也是引用，要求写两段会逼出一堆没意义的填法。只要求它是 `@` 开头的
            // 非空引用，并把 `@ns/` 这种半个引用挡下来。
            if f.ftype == "ref" && !s.is_empty() {
                if !s.starts_with('@') {
                    return Err(format!(
                        "字段「{}」要写成引用（@某人 或 @命名空间/slug），收到 {}",
                        f.name,
                        go_quote(&s)
                    ));
                }
                let rest = &s[1..];
                if rest.is_empty() || rest.ends_with('/') || rest.contains("//") {
                    return Err(format!(
                        "字段「{}」的引用不完整（收到 {}）",
                        f.name,
                        go_quote(&s)
                    ));
                }
            }
            Ok(vec![s])
        }
        "enum" => {
            let s = as_str()?;
            if f.enums.iter().any(|x| *x == s) {
                return Ok(vec![s]);
            }
            Err(format!(
                "字段「{}」只能是 {}（收到 {}）",
                f.name,
                f.enums.join(" / "),
                go_quote(&s)
            ))
        }
        "int" => match v.as_f64() {
            // Go 把 JSON 数字都解成 float64 再截断，这里保持同一口径。
            Some(n) => Ok(vec![(n as i64).to_string()]),
            None => Err(format!("字段「{}」要整数", f.name)),
        },
        "bool" => match v.as_bool() {
            Some(b) => Ok(vec![b.to_string()]),
            None => Err(format!("字段「{}」要布尔", f.name)),
        },
        "string[]" => {
            let arr = v
                .as_array()
                .ok_or_else(|| format!("字段「{}」要字符串数组", f.name))?;
            let mut out = Vec::with_capacity(arr.len());
            for x in arr {
                match x.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(format!("字段「{}」要字符串数组（里面有非字符串）", f.name))
                    }
                }
            }
            Ok(out)
        }
        other => Err(format!("字段「{}」的类型「{}」不支持", f.name, other)),
    }
}

/// Go 的 `%q`（报错文案里的引号形态）。
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

/* ---------------- 内置集合目录 ---------------- */

/// 内置集合：节点自己用的那几类内容（kb / mem / ckpt / trace）。
///
/// 它们**也是声明**，形状与用户自己声明的集合完全一样（同一套字段语法、同一套红线）——
/// 只是名字保留，用户不能 declare 同名集合。`ncc store ls` 靠这份目录回答
/// 「这台节点上有什么内容」：没用过的内置内容是「还没有行」的，
/// 只看得见用户声明的集合就是一张骗人的清单。
struct BuiltinDecl {
    kind: &'static str,
    title: &'static str,
    summary: &'static str,
    reason: &'static str,
    fields: &'static [&'static str],
    index: &'static [&'static str],
    append_only: bool,
}

/// 顺序即展示顺序。
const BUILTINS: &[BuiltinDecl] = &[
    BuiltinDecl {
        kind: "kb",
        title: "知识库",
        summary: "托管的语料：按 slug 取，可导出快照包",
        reason: "语料是 Agent 的长期上下文 —— 它按改名进版本，所以用 slug 当键（而不是 id）",
        fields: &[
            "slug:string!",
            "title:string!",
            "format:string",
            "kind:enum:doc|faq|notes|spec|transcript",
            "tags:string[]",
            "body:text?search",
        ],
        index: &["slug", "kind", "tags"],
        append_only: false,
    },
    BuiltinDecl {
        kind: "mem",
        title: "记忆",
        summary: "键值 + TTL + 来源：跨运行、跨机器记得住",
        reason: "记忆是「小结论」：按 (subject, key) 覆盖自己，读时判过期",
        fields: &[
            "subject:string!",
            "key:string!",
            "kind:enum:fact|preference|episode|summary|pointer",
            "source:string",
            "confidence:int",
            "pinned:bool",
            "value:text?search",
        ],
        index: &["subject", "kind"],
        append_only: false,
    },
    BuiltinDecl {
        kind: "ckpt",
        title: "检查点",
        summary: "不可变快照 + 血缘：交接与回滚的落点",
        reason: "检查点的价值是「拿回来的是原来那份」—— 所以只追加，字节进 blob、元数据进这里",
        fields: &[
            "name:string!",
            "label:enum:episode|step|run|release|handoff|manual",
            "digest:string!",
            "size:int",
            "subject_ref:string",
            "parent:string",
            "note:text?search",
        ],
        index: &["label", "subject_ref", "parent"],
        append_only: true,
    },
    BuiltinDecl {
        kind: "trace",
        title: "运行轨迹",
        summary: "一次运行的真实流水 + 评测：默认私有、显式采集",
        reason: "轨迹只追加：它记的是「当时发生了什么」，改一个字就不再是证据",
        fields: &[
            "kind:enum:hur-run|agent",
            "status:enum:ok|error|cancelled",
            "label:string",
            "model:string",
            "tool:string",
            "digest:string!",
            "steps:int",
            "note:text?search",
        ],
        index: &["kind", "status", "label", "model"],
        append_only: true,
    },
];

fn is_builtin_kind(kind: &str) -> bool {
    let k = kind.trim().to_lowercase();
    BUILTINS.iter().any(|b| b.kind == k)
}

fn builtin_kind_list() -> String {
    BUILTINS
        .iter()
        .map(|b| b.kind)
        .collect::<Vec<_>>()
        .join(" / ")
}

/// 五类内容（四类内置 + 用户自己声明的集合）**共用**的口径。
///
/// 这是一份代码里的常量，五个 `/kinds` 接口都返回它、且要求逐字相同 ——
/// 各写一遍，三个月后一定有一条不一样，然后用户就得记住「kb 是这样、mem 是那样」。
fn shared_invariants() -> Vec<&'static str> {
    vec![
        "归档 ≠ 删除：归档只是默认不列出（指定 `archived=1` 还能看到），删要显式说",
        "读不到 ≠ 没有：读不到时说清原因（不存在 / 不是你的 / 已过期），不给一个空结果",
        "CRUD ≠ 授权：写要命名空间成员，跨空间读要 grant —— 能写不等于能读别人的",
        "过期即不存在：过期在**读时**判定（不等清理任务），gc 只是清垃圾",
        "动态 ≠ 无模式：没声明的字段、不能过滤的字段，明确拒绝而不是默默忽略",
    ]
}

/// GET /api/store/kinds —— 词表与上限（CLI 取值来源；可离线读）。
async fn store_kinds() -> ApiResult<Response> {
    Ok(helpers::ok_json(json!({
        "collection": {
            "nameRule": "小写字母数字与 -_.，字母开头，≤48",
            "note": "集合属于**命名空间**：同一个 kind 在不同命名空间里是两份声明、两份数据",
        },
        "fieldTypes": ["string", "text", "int", "bool", "string[]", "ref", "enum:a|b|c"],
        "fieldFlags": {"!": "必填", "?search": "该字段进搜索文本"},
        "visibility": ["public", "private"],
        "dedupeBy": ["", "checksum"],
        "maxBytes": {
            "default": store::state::MAX_BYTES_DEFAULT,
            "hard": store::state::MAX_BYTES_HARD,
            "note": "通用仓只存**内联文本**；大对象走制品或检查点的 blob",
        },
        "maxFields": store::state::MAX_FIELDS,
        "maxRecords": store::state::MAX_RECORDS,
        "status": ["active", "archived"],
        "invariants": shared_invariants(),
        "invariantsExtra": [
            "没在 fields 里声明的字段写不进来（放进 meta 可以，但 meta 不可过滤）",
            "没在 index 里声明的字段不能当过滤条件",
            "mutable=false 或 append_only=true 的集合没有 PUT",
            "集合没声明 public 时，里面的记录永远不匿名可见（哪怕记录写了 public）",
        ],
    })))
}

/* ---------------- 序列化 ---------------- */

/// 集合的引用（`@命名空间/kind`）。
fn col_ref_of(ns_slug: &str, kind: &str) -> String {
    let s = ns_slug.trim_start_matches('@');
    format!("@{s}/{kind}")
}

fn col_json(ns_slug: &str, c: &Collection, records: i64) -> Value {
    json!({
        "id": c.id,
        "ref": col_ref_of(ns_slug, &c.kind),
        "kind": c.kind,
        "namespace": {"id": c.namespace_id, "slug": ns_slug},
        // builtin：这几类是节点自己在用的内置内容（名字保留）
        "title": c.title(),
        "summary": c.summary(),
        "reason": c.reason(),
        "builtin": is_builtin_kind(&c.kind),
        "mutable": c.mutable(),
        "history": c.history(),
        "appendOnly": c.append_only(),
        "visibility": c.visibility(),
        "maxBytes": c.max_bytes(),
        "fields": decode_fields(c.fields_raw()).iter().map(|f| f.to_json()).collect::<Vec<_>>(),
        "index": store::parse_list(c.index_raw()),
        "dedupeBy": c.dedupe_by(),
        "defaultTtlDays": c.default_ttl(),
        "status": c.status(),
        "records": records,
        "createdBy": c.created_by(),
        "createdAt": c.created_at,
        "updatedAt": c.updated_at,
    })
}

fn rec_json(r: &Record, with_body: bool) -> Value {
    let mut out = json!({
        "id": r.id,
        "key": r.key,
        "revision": r.revision(),
        "checksum": r.checksum(),
        "size": r.size(),
        "tags": store::parse_list(r.tags_raw()),
        "fields": decode_map(r.data_raw()),
        "meta": decode_map(r.meta_raw()),
        "visibility": r.visibility(),
        "status": r.status(),
        "source": r.source(),
        "lastNote": r.last_note(),
        "expiresAt": r.expires_at,
        "createdBy": r.created_by(),
        "updatedBy": r.updated_by(),
        "createdAt": r.created_at,
        "updatedAt": r.updated_at,
    });
    if with_body {
        out["body"] = json!(r.body());
    }
    out
}

/// 库里的 JSON 文本 → 对象。空串或非对象一律当 `{}`；`null` 保持 `null`
/// （Go 里 nil map 序列化出来就是 `null`），与 Go 的 `decodeMap` 同口径。
fn decode_map(s: &str) -> Value {
    if s.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(s) {
        Ok(Value::Object(m)) => Value::Object(m),
        Ok(Value::Null) => Value::Null,
        Ok(_) => json!({}),
        Err(_) => json!({}),
    }
}

/* ---------------- 范围与解析 ---------------- */

/// 一个身份能读到哪些命名空间的记录（管理员 = 全部）。
struct ReadScope {
    ids: Vec<String>,
    all: bool,
}

async fn is_admin(state: &AppState, auth: &Auth) -> bool {
    match auth.info() {
        Some(a) => helpers::require_admin(state, a).await.is_ok(),
        None => false,
    }
}

async fn read_scope(state: &AppState, auth: &Auth) -> ReadScope {
    let Some(a) = auth.info() else {
        return ReadScope {
            ids: Vec::new(),
            all: false,
        };
    };
    if is_admin(state, auth).await {
        return ReadScope {
            ids: Vec::new(),
            all: true,
        };
    }
    let mut ids: Vec<String> = store::namespaces::of_user(state.pool(), &a.user_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|n| n.id)
        .collect();
    let owners = store::grants::granted_owners(state.pool(), &a.user_id, "state").await;
    if !owners.is_empty() {
        // 被授权者：把「这些人的命名空间」也算进可读范围（与 kb 的 `?all=` 同一取舍）。
        if let Ok(granted) = store::state::namespace_ids_of_owners(state.pool(), &owners).await {
            ids.extend(granted);
        }
    }
    ReadScope { ids, all: false }
}

/// 解析目标命名空间（默认：调用者的个人空间，个人空间优先）。
async fn state_namespace(state: &AppState, auth: &Auth, want: &str) -> Result<Namespace, ApiError> {
    let uid = auth.user_id().unwrap_or_default();
    let want = want.trim().trim_start_matches('@').to_string();
    let nss = store::namespaces::of_user(state.pool(), &uid)
        .await
        .map_err(ApiError::from_db)?;
    if want.is_empty() {
        if let Some(n) = nss.iter().find(|n| n.ns_type == "account") {
            return Ok(n.clone());
        }
        if let Some(n) = nss.first() {
            return Ok(n.clone());
        }
        return Err(ApiError::bad_request(
            "no_namespace",
            "你还没有命名空间（先注册或建一个组织空间）",
        ));
    }
    if let Some(n) = nss.iter().find(|n| n.slug == want) {
        return Ok(n.clone());
    }
    Err(ApiError::forbidden(format!(
        "你不是命名空间 @{want} 的成员，不能把状态写进去"
    )))
}

/// 写权限：**只有命名空间成员**（被授权者只读）。
async fn state_writable(state: &AppState, ns_id: &str, user_id: &str) -> bool {
    helpers::can_manage(state, ns_id, user_id).await
}

/// 这个身份能不能读这个命名空间的状态（公开项另行判断）。
async fn state_readable(state: &AppState, auth: &Auth, ns_id: &str, owner_id: &str) -> bool {
    let Some(a) = auth.info() else {
        return false;
    };
    if is_admin(state, auth).await || helpers::can_manage(state, ns_id, &a.user_id).await {
        return true;
    }
    store::grants::has(state.pool(), owner_id, &a.user_id, "state", ns_id).await
        || store::grants::has(state.pool(), owner_id, &a.user_id, "state", "").await
}

/// 把 `{col}`（+ `?namespace=`）解析成一个集合。
///
/// ⚠️ 集合属于命名空间，所以**匿名或跨空间读必须显式给 namespace** ——
/// 不给就只在「我自己能读的范围」里找，找不到就说清为什么（而不是猜一个）。
async fn resolve_collection(
    state: &AppState,
    auth: &Auth,
    col_param: &str,
    namespace_q: &str,
) -> Result<(Collection, Namespace), ApiError> {
    let kind = col_param.trim().to_lowercase();
    if !store::state::valid_kind(&kind) {
        return Err(ApiError::bad_request(
            "bad_collection",
            "集合名不合法（小写字母数字与 -_.，字母开头）",
        ));
    }
    let want = namespace_q.trim().trim_start_matches('@').to_string();

    if !want.is_empty() {
        let ns = store::namespaces::by_slug(state.pool(), &want)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "no_namespace",
                    format!("没有命名空间 @{want}"),
                )
            })?;
        let col = store::state::get_collection(state.pool(), &ns.id, &kind)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "no_collection",
                    format!(
                        "命名空间 @{} 还没声明集合「{}」（声明：POST /api/store，或 ncc store declare）",
                        ns.slug, kind
                    ),
                )
            })?;
        // 404 的 code 与 Go 一致
        return Ok((col, ns));
    }

    let Some(a) = auth.info() else {
        return Err(ApiError::bad_request(
            "need_namespace",
            "匿名读要指明命名空间：?namespace=@某人",
        ));
    };
    let nss = store::namespaces::of_user(state.pool(), &a.user_id)
        .await
        .map_err(|_| ApiError::internal("服务内部错误"))?;
    let mut ordered: Vec<Namespace> = Vec::with_capacity(nss.len());
    ordered.extend(nss.iter().filter(|n| n.ns_type == "account").cloned());
    ordered.extend(nss.iter().filter(|n| n.ns_type != "account").cloned());
    for ns in &ordered {
        if let Some(col) = store::state::get_collection(state.pool(), &ns.id, &kind)
            .await
            .map_err(ApiError::from_db)?
        {
            return Ok((col, ns.clone()));
        }
    }
    Err(ApiError::new(
        StatusCode::NOT_FOUND,
        "no_collection",
        format!("你的命名空间里没有集合「{kind}」（声明：POST /api/store）"),
    ))
}

/* ---------------- 查询串小工具 ---------------- */

/// 查询串的键值对（键与值都做百分号解码）。同名键按出现顺序保留多个值。
fn query_pairs(uri: &Uri) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(qs) = uri.query() else {
        return out;
    };
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.push((web::percent_decode(k), web::percent_decode(v)));
    }
    out
}

fn first_of(pairs: &[(String, String)], key: &str) -> Option<String> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

fn all_of(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .collect()
}

fn is_one(v: Option<String>) -> bool {
    v.as_deref().map(str::trim) == Some("1")
}

/// Go 的 `%v` 形态（`[a b c]` / `[]`），报错文案里要逐字一致。
fn go_slice(v: &[String]) -> String {
    if v.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", v.join(" "))
    }
}

/* ---------------- 集合：列 / 声明 / 归档 ---------------- */

/// GET /api/store?namespace=
async fn list_store_collections(
    State(state): State<AppState>,
    auth: Auth,
    uri: Uri,
) -> ApiResult<Response> {
    let scope = read_scope(&state, &auth).await;
    let want = web::query(&uri, "namespace").unwrap_or_default();
    let want = want.trim().to_string();

    let opts = if !want.is_empty() {
        let slug = want.trim_start_matches('@').to_string();
        let ns = store::namespaces::by_slug(state.pool(), &slug)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "no_namespace",
                    format!("没有命名空间 @{slug}"),
                )
            })?;
        if !scope.all && !scope.ids.contains(&ns.id) {
            return Err(ApiError::forbidden(format!(
                "你不在 @{slug} 里，看不到它的集合"
            )));
        }
        store::state::CollectionListOpts {
            namespace_id: Some(ns.id),
            namespace_ids: Vec::new(),
            all: scope.all,
        }
    } else {
        store::state::CollectionListOpts {
            namespace_id: None,
            namespace_ids: scope.ids.clone(),
            all: scope.all,
        }
    };
    let rows = store::state::list_collections(state.pool(), &opts)
        .await
        .map_err(ApiError::from_db)?;

    let mut out = Vec::with_capacity(rows.len());
    let mut present: HashSet<String> = HashSet::new();
    for row in &rows {
        present.insert(row.kind.clone());
        let ns = store::namespaces::by_id(state.pool(), &row.namespace_id)
            .await
            .map_err(ApiError::from_db)?;
        let n = store::state::count_records(state.pool(), &row.id)
            .await
            .unwrap_or(0);
        out.push(col_json(
            ns.as_ref().map(|n| n.slug.as_str()).unwrap_or_default(),
            row,
            n,
        ));
    }
    // 内置集合的**目录**（不管这台节点上有没有用过）：目录与实情分开给 ——
    // `builtins` 是节点声明支持的几类，`collections` 是实际有的。
    let builtins: Vec<Value> = BUILTINS
        .iter()
        .map(|b| {
            json!({
                "kind": b.kind,
                "title": b.title,
                "summary": b.summary,
                "reason": b.reason,
                "fields": b.fields,
                "index": b.index,
                "shape": if b.append_only { "append-only" } else { "mutable" },
                // 用过没有：没有就是「取用即声明」（第一次写它才出现）
                "present": present.contains(b.kind),
            })
        })
        .collect();
    Ok(helpers::ok_json(json!({
        "collections": out,
        "count": out.len(),
        "builtins": builtins,
    })))
}

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

fn de_i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(Option::<i64>::deserialize(d)?.unwrap_or(0))
}

#[derive(Debug, Default, Deserialize)]
struct DeclareBody {
    #[serde(default, deserialize_with = "de_str")]
    namespace: String,
    #[serde(default, deserialize_with = "de_str")]
    kind: String,
    #[serde(default, deserialize_with = "de_str")]
    title: String,
    #[serde(default, deserialize_with = "de_str")]
    summary: String,
    #[serde(default, deserialize_with = "de_str")]
    reason: String,
    #[serde(default)]
    mutable: Option<bool>,
    #[serde(default, deserialize_with = "de_bool")]
    history: bool,
    #[serde(default, deserialize_with = "de_bool")]
    append_only: bool,
    #[serde(default, deserialize_with = "de_str")]
    visibility: String,
    #[serde(default, deserialize_with = "de_i64")]
    max_bytes: i64,
    #[serde(default, deserialize_with = "de_str_list")]
    fields: Vec<String>,
    #[serde(default, deserialize_with = "de_str_list")]
    index: Vec<String>,
    #[serde(default, deserialize_with = "de_str")]
    dedupe_by: String,
    #[serde(default, deserialize_with = "de_i64")]
    default_ttl_days: i64,
}

/// POST /api/store —— 声明（或更新）一个集合。
///
/// 谁可以声明：**该命名空间的成员**（与写 kb 同一权限）。集合声明是「这个空间里有
/// 一类这样的内容」，不是节点级治理动作 —— 模式随命名空间走，两个团队各声明各的
/// `issue`，互不干扰。
async fn declare_store_collection(
    State(state): State<AppState>,
    auth: Auth,
    body: Bytes,
) -> ApiResult<Response> {
    auth.require_scope("store:write")?;
    let body: DeclareBody = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("bad_request", "请求体不是合法 JSON"))?;
    let kind = body.kind.trim().to_lowercase();
    if !store::state::valid_kind(&kind) {
        return Err(ApiError::bad_request(
            "bad_collection",
            "集合名不合法（小写字母数字与 -_.，字母开头，≤48）",
        ));
    }
    // 内置集合名是**保留**的：谁都能 declare 同名集合的话，`ncc store ls` 就开始说谎。
    if is_builtin_kind(&kind) {
        return Err(ApiError::bad_request(
            "builtin_kind",
            format!(
                "「{kind}」是节点的**内置集合名**（{}）—— 这几类内容由节点自己在用，名字保留（另起一个名字，比如 {kind}-mine）",
                builtin_kind_list()
            ),
        ));
    }
    let uid = auth.user_id().unwrap_or_default();
    let ns = state_namespace(&state, &auth, &body.namespace).await?;
    if !state_writable(&state, &ns.id, &uid).await {
        return Err(ApiError::forbidden("只有命名空间的成员能声明集合"));
    }
    let specs = parse_fields(&body.fields).map_err(|e| ApiError::bad_request("bad_field", e))?;
    let declared: HashSet<&str> = specs.iter().map(|f| f.name.as_str()).collect();
    let mut idx: Vec<String> = Vec::with_capacity(body.index.len());
    for x in &body.index {
        let x = x.trim().to_string();
        if !declared.contains(x.as_str()) {
            return Err(ApiError::bad_request(
                "bad_index",
                format!("索引字段「{x}」没在 fields 里声明 —— 没声明的字段不能当过滤条件"),
            ));
        }
        idx.push(x);
    }
    let vis = {
        let v = body.visibility.trim();
        if v.is_empty() {
            "private".to_string()
        } else {
            v.to_string()
        }
    };
    if vis != "public" && vis != "private" {
        return Err(ApiError::bad_request(
            "bad_visibility",
            "visibility 只能是 public 或 private",
        ));
    }
    let max_bytes = if body.max_bytes <= 0 {
        store::state::MAX_BYTES_DEFAULT
    } else {
        body.max_bytes
    };
    if max_bytes > store::state::MAX_BYTES_HARD {
        return Err(ApiError::bad_request(
            "too_large",
            format!(
                "max_bytes 上限 {}（再大就走制品或检查点的 blob）",
                store::state::MAX_BYTES_HARD
            ),
        ));
    }
    let mut mutable = body.mutable.unwrap_or(true);
    if body.append_only {
        mutable = false; // 只追加 = 不可改
    }
    if !body.dedupe_by.is_empty() && body.dedupe_by != "checksum" {
        return Err(ApiError::bad_request(
            "bad_dedupe",
            "dedupe_by 只支持 checksum（空 = 不去重）",
        ));
    }
    if body.default_ttl_days < 0 || body.default_ttl_days > 3650 {
        return Err(ApiError::bad_request(
            "bad_ttl",
            "default_ttl_days 要在 0..3650（0 = 不过期）",
        ));
    }

    let title = {
        let t = body.title.trim();
        if t.is_empty() {
            kind.clone()
        } else {
            t.to_string()
        }
    };
    let input = store::state::CollectionInput {
        namespace_id: ns.id.clone(),
        kind: kind.clone(),
        title,
        summary: body.summary.trim().to_string(),
        reason: body.reason.trim().to_string(),
        mutable,
        history: body.history || mutable,
        append_only: body.append_only,
        visibility: vis,
        max_bytes,
        // 存的是**原始声明串**（不是解析后的结构）：展示时还要按同一套语法解析回去。
        fields: store::marshal_list(&body.fields),
        index: store::marshal_list(&idx),
        dedupe_by: body.dedupe_by.clone(),
        default_ttl: body.default_ttl_days,
        created_by: uid,
    };
    let (saved, created) = store::state::upsert_collection(state.pool(), &input)
        .await
        .map_err(ApiError::from_db)?;
    let n = store::state::count_records(state.pool(), &saved.id)
        .await
        .unwrap_or(0);
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok(helpers::ok_status(
        status,
        json!({"collection": col_json(&ns.slug, &saved, n), "created": created}),
    ))
}

/// DELETE /api/store/{col} —— 归档集合（**不删记录**）。
async fn archive_store_collection(
    State(state): State<AppState>,
    auth: Auth,
    Path(col): Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    auth.require_scope("store:write")?;
    let uid = auth.user_id().unwrap_or_default();
    let namespace_q = web::query(&uri, "namespace").unwrap_or_default();
    let (col, ns) = resolve_collection(&state, &auth, &col, &namespace_q).await?;
    if !state_writable(&state, &ns.id, &uid).await {
        return Err(ApiError::forbidden("只有命名空间的成员能归档集合"));
    }
    store::state::set_collection_status(state.pool(), &col.id, store::state::STATUS_ARCHIVED)
        .await
        .map_err(ApiError::from_db)?;
    let n = store::state::count_records(state.pool(), &col.id)
        .await
        .unwrap_or(0);
    Ok(helpers::ok_json(json!({
        "archived": true,
        "collection": col_ref_of(&ns.slug, &col.kind),
        "records": n,
        "note": "归档只是不再列出；记录还在（要真删就逐条 DELETE）",
    })))
}

/* ---------------- 记录：列 / 写 / 读 / 删 / 历史 / GC ---------------- */

/// 列表里要跳过的保留参数（其余非 `f.` 前缀的键都算「写错了」）。
const RESERVED: &[&str] = &[
    "q",
    "tag",
    "prefix",
    "state",
    "page",
    "size",
    "namespace",
    "archived",
    "expired",
    "all",
];

/// GET /api/store/{col}?q=&tag=&prefix=&state=&page=&size=&f.<字段>=v
async fn list_store_records(
    State(state): State<AppState>,
    auth: Auth,
    Path(col): Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    let namespace_q = web::query(&uri, "namespace").unwrap_or_default();
    let (col, ns) = resolve_collection(&state, &auth, &col, &namespace_q).await?;
    let scope = read_scope(&state, &auth).await;
    let pairs = query_pairs(&uri);

    let qtext = first_of(&pairs, "q").unwrap_or_default().trim().to_string();
    let page: i64 = first_of(&pairs, "page")
        .unwrap_or_else(|| "1".to_string())
        .trim()
        .parse()
        .unwrap_or(0);
    let size: i64 = first_of(&pairs, "size")
        .unwrap_or_else(|| "50".to_string())
        .trim()
        .parse()
        .unwrap_or(0);

    // 字段过滤：用 `f.<字段>=值` 前缀。
    //
    // 为什么不用裸名（`?status=open`）：**保留参数会跟同名字段撞车** —— 一个集合声明了
    // `status` 字段，而 `?status=` 又是记录状态（active/archived），两个意思一个 key。
    // 加了前缀就永远不会歧义，CLI 那边仍可以写成好看的 `--where status=open`。
    let declared: HashMap<String, FieldSpec> = decode_fields(col.fields_raw())
        .into_iter()
        .map(|f| (f.name.clone(), f))
        .collect();
    let index_list = store::parse_list(col.index_raw());
    let mut indexed: HashMap<String, String> = HashMap::new();
    let mut bad: Vec<String> = Vec::new();
    let mut unindexed: Vec<String> = Vec::new();
    for (k, v) in &pairs {
        if RESERVED.contains(&k.as_str()) {
            continue;
        }
        if let Some(name) = k.strip_prefix("f.") {
            if !declared.contains_key(name) {
                if !bad.contains(&name.to_string()) {
                    bad.push(name.to_string());
                }
                continue;
            }
            // 声明了字段不等于**能按它过滤**：只有 `index` 里列过的才有索引行。
            // 这里必须报错，不能默默返回空 —— 「查不到」与「这个字段根本不能查」
            // 是两件事，混在一起会让人以为「真的没有这样的记录」。
            if !index_list.contains(&name.to_string()) {
                if !unindexed.contains(&name.to_string()) {
                    unindexed.push(name.to_string());
                }
                continue;
            }
            indexed.entry(name.to_string()).or_insert_with(|| v.clone());
            continue;
        }
        if !bad.contains(k) {
            bad.push(k.clone());
        }
    }
    if !unindexed.is_empty() {
        unindexed.sort();
        return Err(ApiError::bad_request(
            "bad_filter",
            format!(
                "「{}」是声明过的字段，但没在 index 里 —— 没声明的过滤条件就是一次全表扫，所以这里不接（能过滤的：{}）",
                unindexed.join(", "),
                go_slice(&index_list)
            ),
        ));
    }
    if !bad.is_empty() {
        bad.sort();
        return Err(ApiError::bad_request(
            "bad_filter",
            format!(
                "「{}」不是这个集合声明的字段（字段过滤要写成 `f.字段=值`；能过滤的：{}）",
                bad.join(", "),
                go_slice(&index_list)
            ),
        ));
    }

    let opts = store::state::RecordListOpts {
        collection_id: col.id.clone(),
        namespace_ids: scope.ids.clone(),
        public: !scope.all && scope.ids.is_empty(),
        q: qtext.clone(),
        prefix: first_of(&pairs, "prefix").unwrap_or_default(),
        tags: all_of(&pairs, "tag"),
        state: first_of(&pairs, "state").unwrap_or_default(),
        all: scope.all,
        include_archived: is_one(first_of(&pairs, "archived")),
        include_expired: is_one(first_of(&pairs, "expired")),
        indexed,
        page,
        size,
        now: now_go(),
    };
    let (rows, total) = store::state::list_records(state.pool(), &opts)
        .await
        .map_err(ApiError::from_db)?;
    let out: Vec<Value> = rows.iter().map(|r| rec_json(r, false)).collect();
    Ok(helpers::ok_json(json!({
        "collection": col_ref_of(&ns.slug, &col.kind),
        "records": out,
        "total": total,
        "page": page,
        "size": size,
        "note": format!("`?q=` 是**关键词匹配**（空格分隔的几个词都要出现，命中位置决定排序：key 3 / 标签与 ?search 字段 2 / 正文 1）—— 不是索引检索、也不是向量检索；要精确就用 `f.字段=值`。排序在最多 {} 条候选上做（超出按更新时间截断）。", store::state::SEARCH_CANDIDATES),
    })))
}

/// POST /api/store/{col} —— 建一条（同 key 已存在时按集合规则处理）。
async fn upsert_store_record(
    State(state): State<AppState>,
    auth: Auth,
    Path(col): Path<String>,
    uri: Uri,
    body: Bytes,
) -> ApiResult<Response> {
    write_store_record(&state, &auth, &col, None, &uri, &body, false).await
}

/// PUT /api/store/{col}/{key} —— 改一条（不可变集合会拒）。
async fn put_store_record(
    State(state): State<AppState>,
    auth: Auth,
    Path((col, key)): Path<(String, String)>,
    uri: Uri,
    body: Bytes,
) -> ApiResult<Response> {
    write_store_record(&state, &auth, &col, Some(&key), &uri, &body, true).await
}

#[derive(Debug, Default, Deserialize)]
struct RecordBody {
    #[serde(default, deserialize_with = "de_str")]
    key: String,
    #[serde(default, deserialize_with = "de_str")]
    body: String,
    #[serde(default)]
    fields: Option<Value>,
    #[serde(default)]
    meta: Option<Value>,
    #[serde(default, deserialize_with = "de_str_list")]
    tags: Vec<String>,
    #[serde(default, deserialize_with = "de_str")]
    visibility: String,
    #[serde(default, deserialize_with = "de_str")]
    status: String,
    #[serde(default, deserialize_with = "de_str")]
    source: String,
    #[serde(default)]
    expires_at: Option<Value>,
    #[serde(default, deserialize_with = "de_i64")]
    ttl_days: i64,
    #[serde(default, deserialize_with = "de_i64")]
    revision: i64,
    #[serde(default, deserialize_with = "de_str")]
    note: String,
}

async fn write_store_record(
    state: &AppState,
    auth: &Auth,
    col_param: &str,
    key_param: Option<&str>,
    uri: &Uri,
    body_bytes: &Bytes,
    is_put: bool,
) -> ApiResult<Response> {
    // 写盘前先过作用域（与 Go 的 `requireScope("store:write")` 同一位置：挡在解析之前）
    auth.require_scope("store:write")?;
    let namespace_q = web::query(uri, "namespace").unwrap_or_default();
    let (col, ns) = resolve_collection(state, auth, col_param, &namespace_q).await?;
    let uid = auth.user_id().unwrap_or_default();
    if !state_writable(state, &ns.id, &uid).await {
        return Err(ApiError::forbidden("只有命名空间的成员能写这个集合"));
    }
    if is_put && col.immutable() {
        // 只说一个或只追加：不可变集合没有「改」这条路
        return Err(ApiError::bad_request(
            "immutable",
            format!(
                "集合「{}」是{}，没有改这条路径（要留新内容请用新的 key）",
                col.kind,
                if col.append_only() {
                    "只追加"
                } else {
                    "不可变"
                }
            ),
        ));
    }
    let body: RecordBody = serde_json::from_slice(body_bytes)
        .map_err(|_| ApiError::bad_request("bad_request", "请求体不是合法 JSON"))?;

    let key = match key_param {
        Some(k) => k.trim().to_lowercase(),
        None => body.key.trim().to_lowercase(),
    };
    if !store::state::valid_record_key(&key) {
        return Err(ApiError::bad_request(
            "bad_key",
            "key 不合法（小写字母数字与 -_.，字母开头，≤96）",
        ));
    }
    if body.body.len() as i64 > col.max_bytes() {
        return Err(ApiError::bad_request(
            "too_large",
            format!(
                "正文 {} 字节，超过这个集合的上限 {}",
                body.body.len(),
                col.max_bytes()
            ),
        ));
    }

    // ① 声明的字段：**没声明就写不进来**（要么加进声明，要么放进 meta）
    let specs = decode_fields(col.fields_raw());
    let vals: Map<String, Value> = match body.fields.as_ref() {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => {
            return Err(ApiError::bad_request(
                "bad_field",
                "fields 必须是对象（{字段名: 值}）",
            ))
        }
    };
    let declared: HashSet<&str> = specs.iter().map(|f| f.name.as_str()).collect();
    let mut search: Vec<String> = Vec::new();
    let mut index_vals: HashMap<String, Vec<String>> = HashMap::new();
    for f in &specs {
        let v = vals.get(&f.name);
        let Some(v) = v.filter(|v| !v.is_null()) else {
            if f.require {
                return Err(ApiError::bad_request(
                    "missing_field",
                    format!("字段「{}」是必填的", f.name),
                ));
            }
            continue;
        };
        let strs = validate_field_value(f, v).map_err(|e| ApiError::bad_request("bad_field", e))?;
        if f.search {
            search.extend(strs.iter().cloned());
            if let Some(s) = v.as_str() {
                search.push(s.to_string());
            }
        }
        index_vals.insert(f.name.clone(), strs);
    }
    let mut unknown: Vec<&String> = vals
        .keys()
        .filter(|k| !declared.contains(k.as_str()))
        .collect();
    unknown.sort();
    if let Some(k) = unknown.first() {
        return Err(ApiError::bad_request(
            "bad_field",
            format!(
                "字段「{k}」没在集合声明里 —— 要么把它加进 fields，要么放进 meta（meta 不过滤）"
            ),
        ));
    }

    // ② 可见性：集合没声明 public 时，记录级 public 不作数（红线 3）
    let vis = {
        let v = body.visibility.trim();
        if v.is_empty() {
            "private".to_string()
        } else {
            v.to_string()
        }
    };
    if vis == "public" && col.visibility() != "public" {
        return Err(ApiError::bad_request(
            "bad_visibility",
            format!(
                "集合「{}」没声明 public，里面的记录不能公开（要公开就先改集合声明）",
                col.kind
            ),
        ));
    }
    let status = {
        let s = body.status.trim();
        if s.is_empty() {
            store::state::STATUS_ACTIVE.to_string()
        } else {
            s.to_string()
        }
    };
    if status != store::state::STATUS_ACTIVE && status != store::state::STATUS_ARCHIVED {
        return Err(ApiError::bad_request(
            "bad_status",
            "status 只能是 active 或 archived",
        ));
    }

    // ③ TTL：显式 expires_at 优先，其次 ttl_days，最后是集合声明的默认值
    let mut exp: Option<chrono::DateTime<chrono::FixedOffset>> = match body.expires_at.as_ref() {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let t = chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|_| ApiError::bad_request("bad_request", "请求体不是合法 JSON"))?;
            Some(t)
        }
        Some(_) => return Err(ApiError::bad_request("bad_request", "请求体不是合法 JSON")),
    };
    if exp.is_none() && body.ttl_days > 0 {
        exp = Some(chrono::Local::now().fixed_offset() + chrono::Duration::days(body.ttl_days));
    }
    if exp.is_none() && col.default_ttl() > 0 {
        exp = Some(chrono::Local::now().fixed_offset() + chrono::Duration::days(col.default_ttl()));
    }

    let data_json = if vals.is_empty() {
        "{}".to_string()
    } else {
        serde_json::to_string(&Value::Object(vals.clone())).unwrap_or_else(|_| "{}".to_string())
    };
    // `meta` 的 `null` 与「没给」等价（见文件头的取舍说明）
    let meta_json = match body.meta.as_ref() {
        None | Some(Value::Null) => "{}".to_string(),
        Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".to_string()),
    };

    let input = store::state::RecordInput {
        collection_id: col.id.clone(),
        namespace_id: ns.id.clone(),
        key: key.clone(),
        body: body.body.clone(),
        data: data_json,
        meta: meta_json,
        tags: store::marshal_list(&body.tags),
        visibility: vis,
        status,
        source: body.source.trim().to_string(),
        search_text: search
            .iter()
            .chain(body.tags.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" "),
        expires_at: exp.map(format_go),
        user_id: uid,
        expect_revision: body.revision,
        note: body.note.clone(),
        // 不变量在数据层再判一次：上面那处是给「用错动词」的人一句**具体**的话，
        // 这里是兜底 —— 绕过路由也绕不过它。
        immutable: col.immutable(),
    };
    let (rec, created, duplicate) = match store::state::upsert_record(state.pool(), &input).await {
        Ok(v) => v,
        Err(store::state::RecordError::Immutable) => {
            return Err(ApiError::bad_request(
                "immutable",
                format!(
                    "集合「{}」是{}，改不了（要留新内容请用新的 key）",
                    col.kind,
                    if col.append_only() {
                        "只追加"
                    } else {
                        "不可变"
                    }
                ),
            ))
        }
        Err(store::state::RecordError::Conflict) => {
            return Err(ApiError::conflict(
                "conflict",
                "别人先改了（revision 对不上）—— 取最新一份再改，别覆盖别人的修改",
            ))
        }
        Err(store::state::RecordError::TooMany) => {
            return Err(ApiError::bad_request(
                "too_many",
                format!(
                    "这个集合记录数到顶了（{}）—— 一类内容攒到这个量，说明它该分集合或该走制品",
                    store::state::MAX_RECORDS
                ),
            ))
        }
        Err(store::state::RecordError::Db(e)) => return Err(ApiError::from_db(e)),
    };

    // ④ 索引：**只给声明过的可过滤字段建行**
    store::state::set_index(
        state.pool(),
        &rec.id,
        &col.id,
        &store::parse_list(col.index_raw()),
        &index_vals,
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
            "record": rec_json(&rec, true),
            "created": created,
            "duplicate": duplicate,
            "collection": col_ref_of(&ns.slug, &col.kind),
        }),
    ))
}

/// GET /api/store/{col}/{key}?revision=N
async fn get_store_record(
    State(state): State<AppState>,
    auth: Auth,
    Path((col_param, key)): Path<(String, String)>,
    uri: Uri,
) -> ApiResult<Response> {
    let namespace_q = web::query(&uri, "namespace").unwrap_or_default();
    let (col, ns) = resolve_collection(&state, &auth, &col_param, &namespace_q).await?;
    let key = key.trim().to_lowercase();
    let rec = store::state::get_record_by_key(state.pool(), &col.id, &key)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "集合 {} 里没有「{}」",
                col_ref_of(&ns.slug, &col.kind),
                key
            ))
        })?;
    // 可见性：公开记录匿名可读；其余要可读凭据
    if !record_readable(&state, &auth, &col, &rec).await {
        return Err(ApiError::forbidden(
            "这条记录不是公开的（要读请带上能读这个命名空间的凭据）",
        ));
    }
    // `?revision=` 只是「想要某一版」的提示：历史**只记元数据**，取不到正文，
    // 所以这里明确忽略它（而不是假装返回了那一版）。
    let mut out = rec_json(&rec, true);
    out["expired"] = json!(rec.expired_at(chrono::Local::now().fixed_offset()));
    Ok(helpers::ok_json(json!({
        "record": out,
        "collection": col_ref_of(&ns.slug, &col.kind),
    })))
}

/// 一条记录能不能被这个身份读。
async fn record_readable(state: &AppState, auth: &Auth, col: &Collection, rec: &Record) -> bool {
    if rec.visibility() == "public" && col.visibility() == "public" {
        return true;
    }
    rec.namespace_id == col.namespace_id
        && state_readable(state, auth, &rec.namespace_id, col.created_by()).await
}

/// DELETE /api/store/{col}/{key} —— **归档**（软删；要真删用 `?hard=1`）。
async fn delete_store_record(
    State(state): State<AppState>,
    auth: Auth,
    Path((col_param, key)): Path<(String, String)>,
    uri: Uri,
) -> ApiResult<Response> {
    auth.require_scope("store:write")?;
    let uid = auth.user_id().unwrap_or_default();
    let namespace_q = web::query(&uri, "namespace").unwrap_or_default();
    let (col, ns) = resolve_collection(&state, &auth, &col_param, &namespace_q).await?;
    if !state_writable(&state, &ns.id, &uid).await {
        return Err(ApiError::forbidden(
            "只有命名空间的成员能删这个集合里的记录",
        ));
    }
    let key = key.trim().to_lowercase();
    let rec = store::state::get_record_by_key(state.pool(), &col.id, &key)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这条记录"))?;
    if is_one(web::query(&uri, "hard")) {
        store::state::delete_record(state.pool(), &rec.id)
            .await
            .map_err(ApiError::from_db)?;
        return Ok(helpers::ok_json(json!({
            "deleted": true, "hard": true, "key": key,
        })));
    }
    store::state::set_record_status(state.pool(), &rec.id, store::state::STATUS_ARCHIVED, &uid)
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_json(json!({
        "archived": true, "hard": false, "key": key,
        "note": "归档只是不再列出（?archived=1 还能看）；真删要 ?hard=1",
    })))
}

/// GET /api/store/{col}/{key}/history
async fn store_record_history(
    State(state): State<AppState>,
    auth: Auth,
    Path((col_param, key)): Path<(String, String)>,
    uri: Uri,
) -> ApiResult<Response> {
    let namespace_q = web::query(&uri, "namespace").unwrap_or_default();
    let (col, ns) = resolve_collection(&state, &auth, &col_param, &namespace_q).await?;
    let key = key.trim().to_lowercase();
    let rec = store::state::get_record_by_key(state.pool(), &col.id, &key)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("没有这条记录"))?;
    let rows = store::state::record_revisions(state.pool(), &rec.id, 50)
        .await
        .map_err(ApiError::from_db)?;
    let history: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "revision": r.revision.unwrap_or(0),
                "checksum": r.checksum.as_deref().unwrap_or_default(),
                "size": r.size.unwrap_or(0),
                "changedBy": r.changed_by.as_deref().unwrap_or_default(),
                "note": r.note.as_deref().unwrap_or_default(),
                "at": r.created_at,
            })
        })
        .collect();
    Ok(helpers::ok_json(json!({
        "collection": col_ref_of(&ns.slug, &col.kind),
        "key": key,
        "currentRevision": rec.revision(),
        "currentNote": rec.last_note(),
        "history": history,
        "note": "历史只记元数据（谁在什么时候写成了哪个摘要），不存正文副本；`note` 是**写进那个版本时**给的备注",
    })))
}

/// POST /api/store/gc —— 真删过期记录。
async fn gc_store_records(
    State(state): State<AppState>,
    auth: Auth,
    uri: Uri,
) -> ApiResult<Response> {
    auth.require_scope("store:write")?;
    let uid = auth.user_id().unwrap_or_default();
    let namespace_q = web::query(&uri, "namespace").unwrap_or_default();
    let ns = state_namespace(&state, &auth, &namespace_q).await?;
    if !state_writable(&state, &ns.id, &uid).await {
        return Err(ApiError::forbidden("只有命名空间的成员能清理它的记录"));
    }
    let kind = web::query(&uri, "collection")
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let mut col_id = String::new();
    if !kind.is_empty() {
        let col = store::state::get_collection(state.pool(), &ns.id, &kind)
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::NOT_FOUND,
                    "no_collection",
                    format!("这个命名空间里没有集合「{kind}」"),
                )
            })?;
        col_id = col.id;
    }
    let removed = store::state::gc_records(state.pool(), &col_id, &now_go())
        .await
        .map_err(ApiError::from_db)?;
    Ok(helpers::ok_json(json!({
        "removed": removed,
        "namespace": ns.slug,
        "collection": kind,
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

    /// 独立临时库 + 一个组织命名空间 `@team`（机主 U-1、成员 U-2、路人 U-9）。
    async fn test_state(tag: &str) -> (AppState, std::path::PathBuf) {
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://localhost:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();

        let dir = std::env::temp_dir().join(format!(
            "ncc-records-http-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool: SqlitePool = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
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
        .bind(now_go())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO ns_members (namespace_id, user_id, role) VALUES ('NS-1', 'U-2', 'member')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let blobs =
            Arc::new(LocalStorage::new(&dir.join("blobs"), &cfg.public_url, "blobs").unwrap());
        let seal = Arc::new(SecretBox::new(&cfg.jwt_secret).unwrap());
        (
            AppState {
                cfg: Arc::new(cfg),
                pool,
                blobs,
                seal,
            },
            dir,
        )
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

    fn bjson(v: &Value) -> Bytes {
        Bytes::from(serde_json::to_vec(v).unwrap())
    }

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    /// 处理器成功回响应；失败回 `ApiError` —— 统一折成与线上一致的错误体，
    /// 便于直接断言状态码与 code/message。
    async fn json_of(r: ApiResult<Response>) -> (StatusCode, Value) {
        match r {
            Ok(resp) => {
                let status = resp.status();
                let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (
                    status,
                    serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                )
            }
            Err(e) => (
                e.status,
                json!({"error": {"code": e.code, "message": e.message}}),
            ),
        }
    }

    fn declare_body(kind: &str, fields: &[&str], index: &[&str]) -> Value {
        json!({
            "namespace": "team",
            "kind": kind,
            "title": "问题单",
            "summary": "团队的问题单",
            "fields": fields,
            "index": index,
            "visibility": "public",
        })
    }

    async fn declare(
        st: &AppState,
        auth: Auth,
        kind: &str,
        fields: &[&str],
        index: &[&str],
    ) -> (StatusCode, Value) {
        json_of(
            declare_store_collection(
                State(st.clone()),
                auth,
                bjson(&declare_body(kind, fields, index)),
            )
            .await,
        )
        .await
    }

    #[tokio::test]
    async fn 全链路_声明_写_读_列_历史_归档_硬删() {
        let (st, _d) = test_state("full").await;

        // 声明：新集合回 201，字段声明回显成对象数组
        let (status, v) = declare(
            &st,
            session_auth("U-1"),
            "issue",
            &[
                "title:string!",
                "status:enum:open|closed?search",
                "tags:string[]",
            ],
            &["status"],
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        assert_eq!(v["created"], json!(true));
        assert_eq!(v["collection"]["kind"], json!("issue"));
        assert_eq!(v["collection"]["ref"], json!("@team/issue"));
        assert_eq!(v["collection"]["namespace"]["slug"], json!("team"));
        assert_eq!(v["collection"]["builtin"], json!(false));
        assert_eq!(v["collection"]["records"], json!(0));
        assert_eq!(
            v["collection"]["fields"][0],
            json!({"name": "title", "type": "string", "require": true})
        );
        assert_eq!(
            v["collection"]["fields"][1],
            json!({"name": "status", "type": "enum", "enum": ["open", "closed"], "search": true})
        );
        assert_eq!(
            v["collection"]["fields"][2],
            json!({"name": "tags", "type": "string[]"})
        );
        assert_eq!(v["collection"]["index"], json!(["status"]));

        // 再声明一次：200 + created=false
        let (status, v) = declare(
            &st,
            session_auth("U-1"),
            "issue",
            &["title:string!", "status:enum:open|closed?search"],
            &["status"],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["created"], json!(false));

        // 写记录：201；同 key 同内容再写 → 200 duplicate
        let write = |body: Value| {
            upsert_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
                bjson(&body),
            )
        };
        let (status, v) = json_of(
            write(json!({
                "key": "BUG-1",
                "body": "登录超时",
                "fields": {"title": "登录超时", "status": "open"},
                "meta": {"who": "小王"},
                "tags": ["bug"],
            }))
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        assert_eq!(v["created"], json!(true));
        assert_eq!(v["duplicate"], json!(false));
        assert_eq!(v["collection"], json!("@team/issue"));
        assert_eq!(v["record"]["key"], json!("bug-1")); // key 统一小写
        assert_eq!(v["record"]["revision"], json!(1));
        assert_eq!(v["record"]["fields"]["status"], json!("open"));
        assert_eq!(v["record"]["meta"], json!({"who": "小王"}));
        assert_eq!(v["record"]["tags"], json!(["bug"]));
        assert_eq!(v["record"]["lastNote"], json!(""));
        assert_eq!(v["record"]["body"], json!("登录超时"));

        let (status, v) = json_of(
            write(json!({
                "key": "BUG-1",
                "body": "登录超时",
                "fields": {"title": "登录超时", "status": "open"},
                "tags": ["bug"],
            }))
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["duplicate"], json!(true));
        assert_eq!(v["record"]["revision"], json!(1));

        // PUT 改内容：版本 +1，历史留一条
        let (status, v) = json_of(
            put_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path(("issue".to_string(), "BUG-1".to_string())),
                uri("/api/store/issue/bug-1?namespace=team"),
                bjson(&json!({
                    "body": "登录超时（已定位）",
                    "fields": {"title": "登录超时", "status": "closed"},
                    "note": "关了",
                    "revision": 1,
                })),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["record"]["revision"], json!(2));
        assert_eq!(v["record"]["lastNote"], json!("关了"));

        // 读一条：带 expired 字段
        let (status, v) = json_of(
            get_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path(("issue".to_string(), "BUG-1".to_string())),
                uri("/api/store/issue/bug-1?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["record"]["expired"], json!(false));
        assert_eq!(v["collection"], json!("@team/issue"));

        // 列：字段过滤走索引表（closed 是改过之后的值）
        let (status, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team&f.status=closed"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["total"], json!(1));
        assert_eq!(v["page"], json!(1));
        assert_eq!(v["size"], json!(50));
        assert!(v["note"].as_str().unwrap().contains("关键词匹配"));

        // 历史：当前版本 2，历史里是 rev 1
        let (status, v) = json_of(
            store_record_history(
                State(st.clone()),
                session_auth("U-1"),
                Path(("issue".to_string(), "BUG-1".to_string())),
                uri("/api/store/issue/bug-1/history?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["currentRevision"], json!(2));
        assert_eq!(v["history"][0]["revision"], json!(1));

        // 归档集合：不删记录
        let (status, v) = json_of(
            archive_store_collection(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["archived"], json!(true));
        assert_eq!(v["records"], json!(1));
        // 归档后再列集合就看不见了
        let (_, v) = json_of(
            list_store_collections(
                State(st.clone()),
                session_auth("U-1"),
                uri("/api/store?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(v["count"], json!(0));
        assert_eq!(v["collections"].as_array().unwrap().len(), 0);

        // 软删记录 → 再硬删
        let (status, v) = json_of(
            delete_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path(("issue".to_string(), "BUG-1".to_string())),
                uri("/api/store/issue/bug-1?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["archived"], json!(true));
        assert!(v["note"].as_str().unwrap().contains("?hard=1"));
        let (status, v) = json_of(
            delete_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path(("issue".to_string(), "BUG-1".to_string())),
                uri("/api/store/issue/bug-1?namespace=team&hard=1"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["deleted"], json!(true));
        assert_eq!(v["hard"], json!(true));
        let (status, _v) = json_of(
            get_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path(("issue".to_string(), "BUG-1".to_string())),
                uri("/api/store/issue/bug-1?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
    }

    #[tokio::test]
    async fn 权限_作用域_命名空间_与匿名() {
        let (st, _d) = test_state("perm").await;

        // 声明：匿名 → 401；有凭据缺作用域 → 403；非成员 → 403
        let (status, v) = json_of(
            declare_store_collection(
                State(st.clone()),
                anon(),
                bjson(&declare_body("issue", &["title:string!"], &[])),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"]["code"], json!("unauthorized"));

        let (status, v) = json_of(
            declare_store_collection(
                State(st.clone()),
                key_auth("U-1", &["store:read"]),
                bjson(&declare_body("issue", &["title:string!"], &[])),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(v["error"]["code"], json!("forbidden"));

        // 路人指名要写进 @team：不是成员 → 403
        let (status, v) = json_of(
            declare_store_collection(
                State(st.clone()),
                session_auth("U-9"),
                bjson(&declare_body("issue", &["title:string!"], &[])),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(v["error"]["code"], json!("forbidden"));
        // 路人没有指名命名空间（也还没有任何命名空间）→ 400 no_namespace
        let (status, v) = json_of(
            declare_store_collection(
                State(st.clone()),
                session_auth("U-9"),
                bjson(&json!({"kind": "issue", "fields": ["title:string!"]})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("no_namespace"));

        // 建好集合后：成员能写，路人写不了（403），匿名也写不了
        declare(&st, session_auth("U-1"), "issue", &["title:string!"], &[]).await;
        let write = |auth: Auth| {
            upsert_store_record(
                State(st.clone()),
                auth,
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
                bjson(&json!({"key": "a-1", "body": "x", "fields": {"title": "t"}})),
            )
        };
        let (status, _) = json_of(write(session_auth("U-2")).await).await;
        assert_eq!(status, StatusCode::CREATED, "成员能写");
        // 成员 + store:write 的 key 也能写
        let (status, v) = json_of(write(key_auth("U-2", &["store:write"])).await).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["duplicate"], json!(true));
        // 成员但凭据只有 store:read：作用域不够 → 403（写盘要 store:write）
        let (status, _) = json_of(write(key_auth("U-2", &["store:read"])).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "只有 store:read 不能写");
        let (status, v) = json_of(write(session_auth("U-9")).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(
            v["error"]["message"],
            json!("只有命名空间的成员能写这个集合")
        );
        // 匿名：作用域挡在解析之前 → 401（与 Go 的 requireScope 一致；不是 500）
        let (status, _) = json_of(write(anon()).await).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "匿名写应是 401 而不是 500"
        );

        // 读：匿名没给 namespace → 400 need_namespace
        let (status, v) = json_of(
            get_store_record(
                State(st.clone()),
                anon(),
                Path(("issue".to_string(), "a-1".to_string())),
                uri("/api/store/issue/a-1"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("need_namespace"));

        // 匿名给了 namespace：集合是 public，但记录是 private → 403
        let (status, v) = json_of(
            get_store_record(
                State(st.clone()),
                anon(),
                Path(("issue".to_string(), "a-1".to_string())),
                uri("/api/store/issue/a-1?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(
            v["error"]["message"],
            json!("这条记录不是公开的（要读请带上能读这个命名空间的凭据）")
        );

        // 成员能读；路人不能
        let (status, _) = json_of(
            get_store_record(
                State(st.clone()),
                session_auth("U-2"),
                Path(("issue".to_string(), "a-1".to_string())),
                uri("/api/store/issue/a-1?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let (status, _) = json_of(
            get_store_record(
                State(st.clone()),
                session_auth("U-9"),
                Path(("issue".to_string(), "a-1".to_string())),
                uri("/api/store/issue/a-1?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");

        // 看别人命名空间的集合：403（不在它的成员里）
        let (status, v) = json_of(
            list_store_collections(
                State(st.clone()),
                session_auth("U-9"),
                uri("/api/store?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(
            v["error"]["message"],
            json!("你不在 @team 里，看不到它的集合")
        );
        // 不存在的命名空间：404 no_namespace
        let (status, v) = json_of(
            list_store_collections(
                State(st.clone()),
                session_auth("U-1"),
                uri("/api/store?namespace=nope"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["code"], json!("no_namespace"));
    }

    #[tokio::test]
    async fn 非法输入_都回_400_而不是默默忽略() {
        let (st, _d) = test_state("bad").await;
        let decl = |body: Value| {
            declare_store_collection(State(st.clone()), session_auth("U-1"), bjson(&body))
        };

        // 非法 body
        let (status, v) = json_of(
            declare_store_collection(
                State(st.clone()),
                session_auth("U-1"),
                Bytes::from_static(b"not json"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], json!("请求体不是合法 JSON"));

        // 集合名不合法（注意：大小写**不是**错误 —— 与 Go 一样先 lower 再校验）
        for kind in ["issue!", "1issue", "issue space"] {
            let (status, v) = json_of(decl(json!({"namespace": "team", "kind": kind})).await).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
            assert_eq!(v["error"]["code"], json!("bad_collection"));
        }
        let (status, v) = json_of(decl(json!({"namespace": "team", "kind": "Issue"})).await).await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        assert_eq!(v["collection"]["kind"], json!("issue"));

        // 内置集合名保留
        let (status, v) = json_of(decl(json!({"namespace": "team", "kind": "kb"})).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("builtin_kind"));
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("kb / mem / ckpt / trace"));

        // 字段声明不合法 / 索引字段没声明 / 可见性 / 上限 / dedupe / ttl
        for (body, code) in [
            (
                json!({"namespace": "team", "kind": "issue", "fields": ["Title:string"]}),
                "bad_field",
            ),
            (
                json!({"namespace": "team", "kind": "issue", "fields": ["a:text"], "index": ["b"]}),
                "bad_index",
            ),
            (
                json!({"namespace": "team", "kind": "issue", "visibility": "hidden"}),
                "bad_visibility",
            ),
            (
                json!({"namespace": "team", "kind": "issue", "max_bytes": 1 << 30}),
                "too_large",
            ),
            (
                json!({"namespace": "team", "kind": "issue", "dedupe_by": "title"}),
                "bad_dedupe",
            ),
            (
                json!({"namespace": "team", "kind": "issue", "default_ttl_days": 9999}),
                "bad_ttl",
            ),
            (
                json!({"namespace": "team", "kind": "issue", "fields": ["a:text", "a:string"]}),
                "bad_field",
            ),
        ] {
            let (status, v) = json_of(decl(body).await).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
            assert_eq!(v["error"]["code"], json!(code));
        }

        // 建一个正常集合，再试各种非法记录
        declare(
            &st,
            session_auth("U-1"),
            "issue",
            &["title:string!", "status:enum:open|closed", "ref:ref"],
            &["status"],
        )
        .await;
        let write = |body: Value| {
            upsert_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
                bjson(&body),
            )
        };
        // key 不合法
        let (status, v) = json_of(write(json!({"key": "1bad", "body": "x"})).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_key"));
        // 缺必填 + 未声明字段 + enum 越界 + ref 形态错
        for (body, expect) in [
            (json!({"key": "a-1", "body": "x"}), "字段「title」是必填的"),
            (
                json!({"key": "a-1", "body": "x", "fields": {"title": "t", "nope": 1}}),
                "没在集合声明里",
            ),
            (
                json!({"key": "a-1", "body": "x", "fields": {"title": "t", "status": "done"}}),
                "只能是 open / closed",
            ),
            (
                json!({"key": "a-1", "body": "x", "fields": {"title": "t", "ref": "alice"}}),
                "要写成引用",
            ),
            (
                json!({"key": "a-1", "body": "x", "fields": {"title": "t", "ref": "@ns/"}}),
                "引用不完整",
            ),
            (
                json!({"key": "a-1", "body": "x", "fields": [1, 2]}),
                "fields 必须是对象",
            ),
            (
                json!({"key": "a-1", "body": "x", "fields": {"title": 42}}),
                "要字符串",
            ),
        ] {
            let (status, v) = json_of(write(body).await).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{expect}");
            let msg = v["error"]["message"].as_str().unwrap();
            assert!(msg.contains(expect), "期望包含 {expect}，实际 {msg}");
        }
        // 正文超限
        let (status, v) = json_of(
            write(json!({"key": "a-1", "body": "x".repeat(300 * 1024),
                "fields": {"title": "t"}}))
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("too_large"));
        // 非法 JSON body
        let (status, bad_body) = json_of(
            upsert_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
                Bytes::from_static(b"{"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(bad_body["error"]["code"], json!("bad_request"));
        // 记录级 public 而集合是 public？这里集合是声明过的 public，所以能写
        let (status, _) = json_of(
            write(json!({"key": "a-1", "body": "x", "visibility": "public",
                "fields": {"title": "t"}}))
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");

        // 集合没声明 public 时，记录不能 public
        let (status, _) = json_of(
            declare_store_collection(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&json!({
                    "namespace": "team",
                    "kind": "note",
                    "fields": ["title:string!"],
                })),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        let (status, v) = json_of(
            upsert_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path("note".to_string()),
                uri("/api/store/note?namespace=team"),
                bjson(&json!({"key": "n-1", "body": "x", "visibility": "public",
                    "fields": {"title": "t"}})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_visibility"));

        // 没声明过的集合 → 404 no_collection
        let (status, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("nope".to_string()),
                uri("/api/store/nope?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("还没声明集合"));
        // 非法集合名 → 400
        let (status, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("BAD!".to_string()),
                uri("/api/store/BAD!?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_collection"));

        // 过滤器：未声明字段 / 未进 index 的字段
        let (status, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team&f.nope=1"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_filter"));
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("不是这个集合声明的字段"));
        let (status, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team&f.title=abc"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("没在 index 里"));
        // 裸名参数（没写 f. 前缀）也拒绝
        let (status, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team&status=open"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_filter"));
        // 非法 status 取值
        let (status, v) = json_of(
            write(json!({"key": "a-2", "body": "x", "status": "gone",
                "fields": {"title": "t"}}))
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_status"));
        // 非法 expires_at
        let (status, _) = json_of(
            write(json!({"key": "a-3", "body": "x", "expires_at": "明天",
                "fields": {"title": "t"}}))
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
    }

    #[tokio::test]
    async fn 不可变_冲突_只追加_与_gc() {
        let (st, _d) = test_state("imm").await;

        // 只追加集合
        let (status, v) = json_of(
            declare_store_collection(
                State(st.clone()),
                session_auth("U-1"),
                bjson(&json!({
                    "namespace": "team",
                    "kind": "snapshot",
                    "fields": ["digest:string!"],
                    "index": ["digest"],
                    "append_only": true,
                })),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        assert_eq!(v["collection"]["appendOnly"], json!(true));
        assert_eq!(v["collection"]["mutable"], json!(false));
        assert_eq!(v["collection"]["history"], json!(false));

        let put = |key: &str, body: Value| {
            put_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path(("snapshot".to_string(), key.to_string())),
                uri(&format!("/api/store/snapshot/{key}?namespace=team")),
                bjson(&body),
            )
        };
        // 只追加集合没有 PUT
        let (status, v) =
            json_of(put("s-1", json!({"body": "x", "fields": {"digest": "d1"}})).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("immutable"));
        assert!(v["error"]["message"].as_str().unwrap().contains("只追加"));

        // POST 能建；同 key 换内容仍然不可变（数据层兜底）
        let post = |body: Value| {
            upsert_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path("snapshot".to_string()),
                uri("/api/store/snapshot?namespace=team"),
                bjson(&body),
            )
        };
        let (status, _) =
            json_of(post(json!({"key": "s-1", "body": "x", "fields": {"digest": "d1"}})).await)
                .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        let (status, v) =
            json_of(post(json!({"key": "s-1", "body": "x2", "fields": {"digest": "d2"}})).await)
                .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("immutable"));
        // 同内容重试 → 幂等重复，不报错
        let (status, v) =
            json_of(post(json!({"key": "s-1", "body": "x", "fields": {"digest": "d1"}})).await)
                .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["duplicate"], json!(true));

        // 乐观并发：revision 对不上 → 409 conflict
        declare(&st, session_auth("U-1"), "issue", &["title:string!"], &[]).await;
        upsert_store_record(
            State(st.clone()),
            session_auth("U-1"),
            Path("issue".to_string()),
            uri("/api/store/issue?namespace=team"),
            bjson(&json!({"key": "a-1", "body": "x", "fields": {"title": "t"}})),
        )
        .await
        .unwrap();
        let put_issue = |body: Value| {
            put_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path(("issue".to_string(), "a-1".to_string())),
                uri("/api/store/issue/a-1?namespace=team"),
                bjson(&body),
            )
        };
        let (status, v) =
            json_of(put_issue(json!({"body": "y", "fields": {"title": "t"}, "revision": 7})).await)
                .await;
        assert_eq!(status, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"]["code"], json!("conflict"));
        // revision 对得上就能改（issue 是可变的）
        let (status, _v) =
            json_of(put_issue(json!({"body": "y", "fields": {"title": "t"}, "revision": 1})).await)
                .await;
        assert_eq!(status, StatusCode::OK, "{_v}");

        // TTL：ttl_days 落地成 expires_at；过期后默认不列、gc 能清
        let (status, v) = json_of(
            upsert_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
                bjson(&json!({"key": "a-2", "body": "会过期", "ttl_days": 1,
                    "fields": {"title": "t"}})),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        let exp = v["record"]["expiresAt"].clone();
        assert!(exp.is_string(), "ttl_days 应写进 expires_at，实际 {exp}");
        let (_, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(v["total"], json!(2));

        // gc：手工把一条造成过期，再清
        sqlx::query("UPDATE records SET expires_at = ? WHERE `key` = 'a-1'")
            .bind("2020-01-01 00:00:00+08:00")
            .execute(st.pool())
            .await
            .unwrap();
        let (status, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["total"], json!(1), "过期记录默认不列");
        let (_, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team&expired=1"),
            )
            .await,
        )
        .await;
        assert_eq!(v["total"], json!(2), "expired=1 能看到过期的");

        let (status, v) = json_of(
            gc_store_records(
                State(st.clone()),
                session_auth("U-1"),
                uri("/api/store/gc?namespace=team&collection=issue"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["removed"], json!(1));
        assert_eq!(v["namespace"], json!("team"));
        assert_eq!(v["collection"], json!("issue"));
        // gc 一个不存在的集合 → 404
        let (status, v) = json_of(
            gc_store_records(
                State(st.clone()),
                session_auth("U-1"),
                uri("/api/store/gc?namespace=team&collection=nope"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["code"], json!("no_collection"));
        // gc 缺作用域 → 403
        let (status, _) = json_of(
            gc_store_records(
                State(st.clone()),
                key_auth("U-1", &["store:read"]),
                uri("/api/store/gc?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{v}");
    }

    /// 起一遍真实的 `/api` 路由表，确认这一族**真挂上去了**，并且静态段没被参数段抢走。
    ///
    /// 为什么值得单测：路由是 `router.rs` 拼的，处理器级单测看不见「/store/gc 被 /store/{col} 抢走」
    /// 这种问题 —— 那种错只会在线上表现为「清理接口回 404 说没有集合 gc」。
    #[tokio::test]
    async fn 路由_挂载_与静态段优先() {
        let (st, _d) = test_state("route").await;
        let app = crate::router::api_router().with_state(st.clone());

        async fn call(
            app: &Router,
            method: &str,
            path: &str,
            body: Option<Value>,
            token: Option<&str>,
        ) -> (StatusCode, Value) {
            let mut req = axum::http::Request::builder().method(method).uri(path);
            if let Some(t) = token {
                req = req.header("authorization", format!("Bearer {t}"));
            }
            let req = match body {
                Some(b) => req
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(b.to_string()))
                    .unwrap(),
                None => req.body(axum::body::Body::empty()).unwrap(),
            };
            let resp = tower::ServiceExt::oneshot(app.clone(), req).await.unwrap();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            )
        }

        // 静态段 /store/kinds 不会被当成集合名 {col}
        let (status, v) = call(&app, "GET", "/store/kinds", None, None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert!(v["fieldTypes"].is_array());

        // 参数路由确实挂上了：集合没声明 → 404 no_collection
        let (status, v) = call(&app, "GET", "/store/issue?namespace=team", None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["code"], json!("no_collection"));

        // 匿名写 → 401（作用域挡在最前面）
        let (status, v) = call(
            &app,
            "POST",
            "/store",
            Some(json!({"namespace": "team", "kind": "issue"})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{v}");

        // 带 store:write 的 key 打 /store/gc：必须是 gc 处理器（400 no_namespace），
        // 而不是被 /store/{col} 抢走（那会回 404 no_collection，语义完全两回事）
        let (_k, secret) =
            store::apikeys::create(st.pool(), "U-9", "冒烟", &["store:write".to_string()])
                .await
                .unwrap();
        let (status, v) = call(&app, "POST", "/store/gc", None, Some(&secret)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("no_namespace"));

        // 带尾斜杠的形态也要能路由（Go 那边注册了 `/store/` 与 `/index/` 两套）
        let (status, v) = call(&app, "GET", "/store/", None, None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["collections"], json!([]));

        // 索引一族也挂上了（读口公开、写口要作用域）
        let (status, v) = call(&app, "GET", "/index", None, None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["source"], json!("node"));
        let (status, _v) = call(&app, "GET", "/index/", None, None).await;
        assert_eq!(status, StatusCode::OK);
        let (status, v) = call(&app, "GET", "/index/channels", None, None).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["channels"], json!([]));
        let (status, v) = call(&app, "GET", "/match", None, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["code"], json!("bad_request"));
        let (status, _v) = call(
            &app,
            "POST",
            "/index",
            Some(json!({"id": "IX-1", "channel": "food", "title": "t"})),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn 目录_内置集合_关键词检索与分页() {
        let (st, _d) = test_state("kinds").await;

        // /store/kinds 的词表与上限
        let (status, v) = json_of(store_kinds().await).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(
            v["fieldTypes"],
            json!([
                "string",
                "text",
                "int",
                "bool",
                "string[]",
                "ref",
                "enum:a|b|c"
            ])
        );
        assert_eq!(v["maxBytes"]["default"], json!(262144));
        assert_eq!(v["maxBytes"]["hard"], json!(1048576));
        assert_eq!(v["maxFields"], json!(32));
        assert_eq!(v["maxRecords"], json!(100000));
        assert_eq!(v["visibility"], json!(["public", "private"]));
        assert_eq!(v["status"], json!(["active", "archived"]));
        assert_eq!(v["invariants"].as_array().unwrap().len(), 5);
        assert_eq!(v["invariantsExtra"].as_array().unwrap().len(), 4);

        // 集合目录：内置 4 类始终在，present 表示这台节点上有没有真的用过
        let (_, v) = json_of(
            list_store_collections(
                State(st.clone()),
                session_auth("U-1"),
                uri("/api/store?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(v["count"], json!(0));
        assert_eq!(v["builtins"].as_array().unwrap().len(), 4);
        assert_eq!(v["builtins"][0]["kind"], json!("kb"));
        assert_eq!(v["builtins"][0]["present"], json!(false));
        assert_eq!(v["builtins"][0]["shape"], json!("mutable"));
        assert_eq!(v["builtins"][2]["kind"], json!("ckpt"));
        assert_eq!(v["builtins"][2]["shape"], json!("append-only"));
        assert_eq!(
            v["builtins"][0]["fields"][3],
            json!("kind:enum:doc|faq|notes|spec|transcript")
        );

        // 真的声明一个集合后，匿名也能从公开面看到它
        declare(
            &st,
            session_auth("U-1"),
            "issue",
            &["title:string!", "status:enum:open|closed"],
            &["status"],
        )
        .await;
        let (_, v) =
            json_of(list_store_collections(State(st.clone()), anon(), uri("/api/store")).await)
                .await;
        assert_eq!(v["count"], json!(1));
        assert_eq!(v["collections"][0]["kind"], json!("issue"));
        assert_eq!(v["collections"][0]["visibility"], json!("public"));
        assert_eq!(v["collections"][0]["namespace"]["slug"], json!("team"));

        // 关键词检索：词都要出现，命中位置决定排序（key > 标签/搜索字段 > 正文）
        let post = |body: Value| {
            upsert_store_record(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
                bjson(&body),
            )
        };
        post(json!({"key": "login-timeout", "body": "登录 超时",
            "fields": {"title": "登录超时", "status": "open"}, "tags": ["登录"]}))
        .await
        .unwrap();
        post(json!({"key": "other", "body": "顺手提一句登录",
            "fields": {"title": "别的", "status": "open"}}))
        .await
        .unwrap();
        let (status, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team&q=%E7%99%BB%E5%BD%95&size=1"),
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["total"], json!(2), "两个词都命中，只是排序不同");
        assert_eq!(v["size"], json!(1));
        assert_eq!(v["records"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["records"][0]["key"],
            json!("login-timeout"),
            "key 命中排前面"
        );
        // 分页取第二页
        let (_, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team&q=%E7%99%BB%E5%BD%95&size=1&page=2"),
            )
            .await,
        )
        .await;
        assert_eq!(v["records"][0]["key"], json!("other"));

        // 标签过滤 + 前缀过滤
        let (_, v) = json_of(
            list_store_records(
                State(st.clone()),
                session_auth("U-1"),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team&tag=%E7%99%BB%E5%BD%95&prefix=login"),
            )
            .await,
        )
        .await;
        assert_eq!(v["total"], json!(1));

        // 匿名列记录：集合 public + 记录 private → 看不到
        let (_, v) = json_of(
            list_store_records(
                State(st.clone()),
                anon(),
                Path("issue".to_string()),
                uri("/api/store/issue?namespace=team"),
            )
            .await,
        )
        .await;
        assert_eq!(v["total"], json!(0), "记录默认 private，匿名看不到");

        // 解析器与工具的边界
        assert_eq!(
            parse_field_spec("a:text?search!").unwrap(),
            FieldSpec {
                name: "a".into(),
                ftype: "text".into(),
                enums: vec![],
                search: true,
                require: true,
            }
        );
        assert!(parse_field_spec("").is_err());
        // 尾修饰可以叠加（Go 的循环就是这么写的）
        assert_eq!(
            parse_field_spec("body:text?search?search").unwrap().ftype,
            "text"
        );
        assert!(parse_field_spec("s:enum:a||b").is_err());
        assert!(parse_field_spec("s:unknown").is_err());
        assert!(parse_fields(&vec!["a:string".to_string(); 33]).is_err());
        assert!(decode_fields("not json").is_empty());
        assert_eq!(decode_map("null"), Value::Null);
        assert_eq!(decode_map("[]"), json!({}));
        assert_eq!(decode_map("{\"a\":1}")["a"], json!(1));
        assert_eq!(go_slice(&[]), "[]");
        assert_eq!(go_slice(&["a".into(), "b".into()]), "[a b]");
        assert_eq!(go_quote("说\"话\""), "\"说\\\"话\\\"\"");
    }
}
