//! NCC Trace：运行轨迹的采集与托管（文档规范 + 数据访问）。
//!
//! 原实现：`ncc-registry/store/trace.go` + `model/trace.go`。
//!
//! Go 侧把「文档规范」放在 `model` 包（纯函数、好测），Rust 侧没有 model 层，
//! 于是规范与数据访问都收在这一个文件里：**摘要与校验必须与 CLI 逐字节一致**
//! （`ncc-cli/cli/src/trace.rs` 里有一份同规则的实现与同一条基线向量），
//! 两处放在一起才好对比、也才好一起改。
//!
//! 三条不变量（与 Go 注释里的红线一致，别在别处忘了）：
//!
//! 1. **文档不可变**：写入后不再改（它带采集方算的 digest），评测标注单独一张表；
//! 2. **内容级别由采集方声明**：`payload` 只如实记录，服务端不补全也不降级；
//! 3. **默认私有**：轨迹没有「公开」档 —— 可见性只有「我的命名空间 / 被授权 / 管理员」。
//!
//! 摘要、校验、聚合都作用在 `serde_json::Value` 上（而不是反序列化出的结构体）：
//! CLI 就是这么算的，跟着它走才能保证「本地算的摘要 = 服务端复核的摘要」。
//! 结构体只用来做「类型门」（对应 Go 的 `json.Unmarshal` 进 `model.TraceDoc`）
//! 与取投影列。

use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::SqlitePool;

use ncc_core::ids::new_id;
use ncc_core::timeutil::{format_go, now_go};

use super::exists;

/* ---------------- 规范与词表 ---------------- */

/// 轨迹文档规范号（与 CLI 写死的字符串一致）。
pub const TRACE_SPEC: &str = "ncc-trace/v1";

pub const TRACE_KIND_HUR_RUN: &str = "hur-run";
pub const TRACE_KIND_AGENT: &str = "agent";

/// 全部种类（顺序即展示顺序）。
pub const TRACE_KINDS: &[&str] = &[TRACE_KIND_HUR_RUN, TRACE_KIND_AGENT];

pub fn valid_trace_kind(k: &str) -> bool {
    TRACE_KINDS.contains(&k)
}

/// 种类标签（lang = zh|en）。
pub fn trace_kind_label(k: &str, lang: &str) -> String {
    match (k, lang) {
        (TRACE_KIND_HUR_RUN, "en") => "HUR run".to_string(),
        (TRACE_KIND_HUR_RUN, _) => "HUR 执行".to_string(),
        (TRACE_KIND_AGENT, "en") => "Agent session".to_string(),
        (TRACE_KIND_AGENT, _) => "Agent 会话".to_string(),
        _ => k.to_string(),
    }
}

pub const TRACE_OK: &str = "ok";
pub const TRACE_ERROR: &str = "error";
pub const TRACE_CANCELLED: &str = "cancelled";

/// `cancelled` 与 `error` 分开：被主动中止不是失败，评测时要分开算。
pub const TRACE_STATUSES: &[&str] = &[TRACE_OK, TRACE_ERROR, TRACE_CANCELLED];

pub fn valid_trace_status(s: &str) -> bool {
    TRACE_STATUSES.contains(&s)
}

pub const TRACE_PAYLOAD_DIGEST: &str = "digest";
pub const TRACE_PAYLOAD_PREVIEW: &str = "preview";
pub const TRACE_PAYLOAD_FULL: &str = "full";

/// 内容级别 —— **默认 digest**（只有哈希与结构，不含任何原文）。
pub const TRACE_PAYLOAD_LEVELS: &[&str] = &[
    TRACE_PAYLOAD_DIGEST,
    TRACE_PAYLOAD_PREVIEW,
    TRACE_PAYLOAD_FULL,
];

pub fn valid_trace_payload(p: &str) -> bool {
    TRACE_PAYLOAD_LEVELS.contains(&p)
}

/// 级别强弱（「这条轨迹里最强的内容级别」统计用）。
#[allow(dead_code)]
pub fn trace_payload_rank(p: &str) -> i32 {
    match p {
        TRACE_PAYLOAD_FULL => 3,
        TRACE_PAYLOAD_PREVIEW => 2,
        TRACE_PAYLOAD_DIGEST => 1,
        _ => 0,
    }
}

pub const TRACE_STEP_LLM: &str = "llm";
pub const TRACE_STEP_TOOL: &str = "tool";
pub const TRACE_STEP_IO: &str = "io";
pub const TRACE_STEP_NOTE: &str = "note";
pub const TRACE_STEP_GUARD: &str = "guard";

pub const TRACE_STEP_TYPES: &[&str] = &[
    TRACE_STEP_LLM,
    TRACE_STEP_TOOL,
    TRACE_STEP_IO,
    TRACE_STEP_NOTE,
    TRACE_STEP_GUARD,
];

pub fn valid_trace_step_type(t: &str) -> bool {
    TRACE_STEP_TYPES.contains(&t)
}

/// 跨人可见所需的授权种类（与 artifact / node / config 并列）。
///
/// ⚠️ `store::grants::valid_kind` 里还没有 `trace`/`state`（那是授权族的文件，
/// 不在本族改动范围内）：所以**建** trace 授权目前会被 400 拦下，但本族的
/// 读取判断按 Go 的语义照常认它。
pub const KIND_TRACE: &str = "trace";

/// 上限（与 Go 的 `model` 常量一致）。
pub const TRACE_MAX_BYTES: i64 = 2 * 1024 * 1024;
pub const TRACE_MAX_STEPS: usize = 2000;
pub const TRACE_BATCH_MAX: usize = 500;
pub const TRACE_PREVIEW_MAX: usize = 512;
pub const TRACE_MAX_TAGS: usize = 32;

/// 常用标注键（顺序即展示顺序）。
pub const TRACE_LABEL_KEYS: &[&str] = &[
    "grade", "reward", "score", "task", "split", "failure", "reviewer", "note",
];

/// 常用标注键的说明：`[中文, 英文, 中文说明, 英文说明]`。
///
/// 键**不封闭**（自由键照收），但常用键给词表：评测与训练管线靠它对齐，
/// 否则一份数据集里 `grade` / `pass` / `is_good` 三种写法会同时存在。
pub fn trace_label_meta(key: &str) -> Option<[&'static str; 4]> {
    Some(match key {
        "grade" => [
            "结论",
            "Grade",
            "人工/模型给的结论：pass | fail | partial",
            "Human or model verdict: pass | fail | partial",
        ],
        "reward" => [
            "奖励",
            "Reward",
            "标量奖励（后训练用，如 1 / 0 / -1）",
            "Scalar reward for training (e.g. 1 / 0 / -1)",
        ],
        "score" => [
            "得分",
            "Score",
            "评分（0~1，评测用）",
            "Score in 0..1 (evaluation)",
        ],
        "task" => [
            "任务",
            "Task",
            "这类轨迹属于哪个业务任务（如 book-hotel）",
            "Which business task this trace belongs to (e.g. book-hotel)",
        ],
        "split" => [
            "数据切分",
            "Split",
            "train | eval | holdout（训练/评测时别混）",
            "train | eval | holdout (keep them separate)",
        ],
        "failure" => [
            "失败原因",
            "Failure",
            "失败归类（如 tool_timeout / wrong_answer）",
            "Failure taxonomy (e.g. tool_timeout / wrong_answer)",
        ],
        "reviewer" => [
            "标注人",
            "Reviewer",
            "谁做的标注（人工回来复盘时要找得到人）",
            "Who labelled it (so a human can follow up)",
        ],
        "note" => ["备注", "Note", "一句人话说明", "A free-form human note"],
        _ => return None,
    })
}

/// 建议的结论取值（不强制，但强烈建议用这三档）。
pub const TRACE_GRADES: &[&str] = &["pass", "fail", "partial"];

/// 建议的数据切分取值。
pub const TRACE_SPLITS: &[&str] = &["train", "eval", "holdout"];

/// 五类内容**共用**的口径（Go 的 `model.SharedInvariants`，五处返回同一份）。
///
/// 本仓其它四族（制品/节点/配置/记录）还没迁到 Rust，所以这份常量暂时只在本族
/// 返回；等它们迁移时应当抽到公共位置，而不是各抄一份。
pub fn shared_invariants() -> Vec<&'static str> {
    vec![
        "归档 ≠ 删除：归档只是默认不列出（指定 `archived=1` 还能看到），删要显式说",
        "读不到 ≠ 没有：读不到时说清原因（不存在 / 不是你的 / 已过期），不给一个空结果",
        "CRUD ≠ 授权：写要命名空间成员，跨空间读要 grant —— 能写不等于能读别人的",
        "过期即不存在：过期在**读时**判定（不等清理任务），gc 只是清垃圾",
        "动态 ≠ 无模式：没声明的字段、不能过滤的字段，明确拒绝而不是默默忽略",
    ]
}

/* ---------------- 摘要（跨语言一致，别改规则） ---------------- */

/// 轨迹摘要：`sha256:<hex>`，覆盖**身份 + 结构 + 标签**，不覆盖原文。
///
/// 为什么不用「序列化整个 JSON 再哈希」：JSON 的浮点/转义/键序在不同语言里不保证
/// 一致（Go 会把 `<` 转成 `\u003c`、Rust 不会；`1.0` 与 `1` 也不同），跨语言一对比
/// 就假报「被篡改」。所以用**长度前缀拼接**这种最笨也最确定的编码：
///
/// ```text
/// 每段写作 "<段名>=<字节长度>:<内容>\n"，内容原样（不转义、不规范化）。
/// ```
///
/// 覆盖：规范号、id、种类、时刻、用时、结论、来源、主体、模型、用量、每步的
/// (序号/类型/名字/耗时/结论/入摘要/出摘要)、标签（键排序后的 k=v）、标签数组、
/// 内容级别。**不含 In/Out 原文**（脱敏会改它，且可能很大）。
pub fn trace_digest(d: &Value) -> String {
    format!(
        "sha256:{}",
        ncc_core::crypto::sha256_hex(trace_digest_core(d).as_bytes())
    )
}

/// 被摘要的那段规范文本（测试与排查用；与 CLI **必须逐字节一致**）。
pub fn trace_digest_core(d: &Value) -> String {
    let mut b = String::new();
    // 取字符串/整数的小工具：缺字段或类型不对一律当空 / 0（与 Go 的零值一致）。
    let s = |path: &str| -> String {
        d.pointer(path)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let i = |path: &str| -> i64 {
        match d.pointer(path) {
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().map(|f| f as i64))
                .unwrap_or(0),
            _ => 0,
        }
    };
    let seg = |b: &mut String, k: &str, v: &str| {
        b.push_str(k);
        b.push('=');
        b.push_str(&v.len().to_string());
        b.push(':');
        b.push_str(v);
        b.push('\n');
    };
    let segi = |b: &mut String, k: &str, v: i64| seg(b, k, &v.to_string());

    seg(&mut b, "spec", &s("/spec"));
    seg(&mut b, "id", &s("/id"));
    seg(&mut b, "kind", &s("/kind"));
    seg(&mut b, "at", &s("/at"));
    segi(&mut b, "durationMs", i("/durationMs"));
    seg(&mut b, "status", &s("/status"));
    for k in ["node", "host", "cli", "agent", "user", "region"] {
        seg(&mut b, &format!("source.{k}"), &s(&format!("/source/{k}")));
    }
    for k in ["ref", "kind", "version", "digest", "engine", "policy"] {
        seg(
            &mut b,
            &format!("subject.{k}"),
            &s(&format!("/subject/{k}")),
        );
    }
    seg(&mut b, "model.provider", &s("/model/provider"));
    seg(&mut b, "model.name", &s("/model/name"));
    segi(&mut b, "model.calls", i("/model/calls"));
    segi(&mut b, "usage.inputTokens", i("/usage/inputTokens"));
    segi(&mut b, "usage.outputTokens", i("/usage/outputTokens"));
    segi(&mut b, "usage.costUsdMicros", i("/usage/costUsdMicros"));
    seg(&mut b, "payload", &s("/payload"));
    seg(&mut b, "redaction.level", &s("/redaction/level"));

    let steps = d
        .get("steps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    segi(&mut b, "steps", steps.len() as i64);
    for st in &steps {
        let ss = |k: &str| st.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        let si = |k: &str| {
            st.get(k)
                .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                .unwrap_or(0)
        };
        segi(&mut b, "step.i", si("i"));
        seg(&mut b, "step.type", &ss("type"));
        seg(&mut b, "step.name", &ss("name"));
        segi(&mut b, "step.ms", si("ms"));
        seg(&mut b, "step.status", &ss("status"));
        seg(&mut b, "step.inDigest", &ss("inDigest"));
        seg(&mut b, "step.outDigest", &ss("outDigest"));
    }

    // 标签：键**排序**后逐个进摘要（map 迭代顺序随机，不排序摘要就不稳定）。
    let labels = d
        .get("labels")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    segi(&mut b, "labels", labels.len() as i64);
    let mut keys: Vec<&String> = labels.keys().collect();
    keys.sort();
    for k in keys {
        seg(&mut b, "label.k", k);
        seg(&mut b, "label.v", &trace_label_value(&labels[k]));
    }
    let tags = d
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    segi(&mut b, "tags", tags.len() as i64);
    for t in tags {
        seg(&mut b, "tag", t);
    }
    b
}

/// 标签值的规范文本。
///
/// 整数**一定**写成整数（`1`，不是 `1.0`）——否则两端会给出不同摘要。
/// ⚠️ 非整数浮点（如 `0.85`）在「最短表示」上不保证跨语言一致：轨迹里要存
/// 非整数就用**整数口径**（得分用千分位、金额用微美元），别把浮点塞进标签。
fn trace_label_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                return i.to_string();
            }
            match n.as_f64() {
                Some(f) if f.fract() == 0.0 => (f as i64).to_string(),
                Some(f) => format!("{f}"),
                None => n.to_string(),
            }
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 由内容生成的稳定 id（采集方没给 id 时用；两端一致）。
pub fn trace_id(d: &Value) -> String {
    let h = ncc_core::crypto::sha256_hex(trace_digest_core(d).as_bytes());
    format!("TRC-{}", &h[..20])
}

/// 补齐最小必需字段（spec / id / payload / digest）。**不修改内容**：只补空字段。
pub fn normalize_trace(d: &mut Value) {
    if !d.is_object() {
        return;
    }
    let get = |d: &Value, k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    if get(d, "spec").is_empty() {
        d["spec"] = Value::String(TRACE_SPEC.to_string());
    }
    if get(d, "id").is_empty() {
        let id = trace_id(d);
        d["id"] = Value::String(id);
    }
    if get(d, "payload").is_empty() {
        d["payload"] = Value::String(TRACE_PAYLOAD_DIGEST.to_string());
    }
    if get(d, "digest").is_empty() {
        let dg = trace_digest(d);
        d["digest"] = Value::String(dg);
    }
}

/* ---------------- 校验 ---------------- */

/// 一条校验结论（与制品校验的 Issue 同形：级别 + 人话）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TraceIssue {
    pub level: String,
    pub msg: String,
}

/// 上报前的校验。**宁可拒收，也不收一条算不出摘要的轨迹**：
/// 数据集一旦混入坏行，训练与评测的结论就都不可信了。
pub fn validate_trace(d: &Value) -> Vec<TraceIssue> {
    let mut out: Vec<TraceIssue> = Vec::new();
    // 用局部宏而不是闭包：两个闭包同时可变借用 `out` 在 Rust 里是编译错误。
    macro_rules! errf {
        ($m:expr) => {
            out.push(TraceIssue {
                level: "error".to_string(),
                msg: $m,
            })
        };
    }
    macro_rules! warnf {
        ($m:expr) => {
            out.push(TraceIssue {
                level: "warn".to_string(),
                msg: $m,
            })
        };
    }
    let s = |path: &str| -> String {
        d.pointer(path)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let n = |path: &str| -> i64 {
        d.pointer(path)
            .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
            .unwrap_or(0)
    };

    if s("/spec") != TRACE_SPEC {
        errf!(format!(
            "spec 必须是 {}，当前是 \"{}\"",
            TRACE_SPEC,
            s("/spec")
        ));
    }
    let id = s("/id");
    if id.trim().is_empty() {
        errf!("缺 id（采集方生成，用于幂等去重）".to_string());
    } else if id.len() > 128 {
        errf!("id 太长（≤128）".to_string());
    }
    let kind = s("/kind");
    if !valid_trace_kind(&kind) {
        errf!(format!(
            "kind 必须是 {}，当前是 \"{}\"",
            TRACE_KINDS.join("|"),
            kind
        ));
    }
    let status = s("/status");
    if !valid_trace_status(&status) {
        errf!(format!(
            "status 必须是 {}，当前是 \"{}\"",
            TRACE_STATUSES.join("|"),
            status
        ));
    }
    let payload = s("/payload");
    if !valid_trace_payload(&payload) {
        errf!(format!(
            "payload 必须是 {}，当前是 \"{}\"",
            TRACE_PAYLOAD_LEVELS.join("|"),
            payload
        ));
    }
    let at = s("/at");
    if at.trim().is_empty() {
        errf!("缺 at（RFC3339 时刻）".to_string());
    } else if DateTime::parse_from_rfc3339(&at).is_err() {
        // Go 会把 time.Parse 的原文带出来；Rust 的时间库错误文案不同，
        // 这里只说清「不是 RFC3339」—— code 与结构仍与 Go 对齐。
        errf!("at 不是 RFC3339".to_string());
    }
    if n("/durationMs") < 0 {
        errf!("durationMs 不能为负".to_string());
    }
    if n("/usage/inputTokens") < 0 || n("/usage/outputTokens") < 0 || n("/usage/costUsdMicros") < 0
    {
        errf!("usage 的计数不能为负".to_string());
    }
    if n("/model/calls") < 0 {
        errf!("model.calls 不能为负".to_string());
    }
    let steps = d.get("steps").and_then(|v| v.as_array());
    let step_count = steps.map(|a| a.len()).unwrap_or(0);
    if step_count > TRACE_MAX_STEPS {
        errf!(format!(
            "步骤太多：{}（上限 {}）",
            step_count, TRACE_MAX_STEPS
        ));
    }
    let tag_count = d
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if tag_count > TRACE_MAX_TAGS {
        errf!(format!(
            "tags 太多：{}（上限 {}）",
            tag_count, TRACE_MAX_TAGS
        ));
    }
    let step_str =
        |st: &Value, k: &str| st.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    if let Some(steps) = steps {
        for (i, st) in steps.iter().enumerate() {
            let ty = step_str(st, "type");
            if !valid_trace_step_type(&ty) {
                errf!(format!(
                    "步骤 {} 的类型 \"{}\" 不合法（{}）",
                    i,
                    ty,
                    TRACE_STEP_TYPES.join("|")
                ));
            }
            let si = st.get("i").and_then(|v| v.as_i64()).unwrap_or(0);
            if si != i as i64 {
                warnf!(format!(
                    "步骤 {} 的序号是 {}（建议与顺序一致，便于对照）",
                    i, si
                ));
            }
            if st.get("ms").and_then(|v| v.as_i64()).unwrap_or(0) < 0 {
                errf!(format!("步骤 {} 的 ms 不能为负", i));
            }
        }
    }
    // 内容级别与 In/Out 必须自洽：声明 digest 却带原文 = 说话不算数。
    if payload == TRACE_PAYLOAD_DIGEST {
        if let Some(steps) = steps {
            for (i, st) in steps.iter().enumerate() {
                if !step_str(st, "in").is_empty() || !step_str(st, "out").is_empty() {
                    errf!(format!(
                        "payload=digest 的轨迹不该带步骤 {} 的原文（要么删原文，要么把 payload 标成 preview/full）",
                        i
                    ));
                    break;
                }
            }
        }
    }
    if payload == TRACE_PAYLOAD_PREVIEW {
        if let Some(steps) = steps {
            for (i, st) in steps.iter().enumerate() {
                if step_str(st, "in").len() > TRACE_PREVIEW_MAX
                    || step_str(st, "out").len() > TRACE_PREVIEW_MAX
                {
                    warnf!(format!(
                        "payload=preview，但步骤 {} 的预览超过 {} 字节（服务端不截断，只提醒）",
                        i, TRACE_PREVIEW_MAX
                    ));
                    break;
                }
            }
        }
    }
    // 摘要：有值就核对（采集方必须算对；算错说明两端规范不一致，早点暴露）。
    let digest = s("/digest");
    if !digest.is_empty() {
        let want = trace_digest(d);
        if want != digest {
            errf!(format!(
                "digest 与文档内容不符：文档算出 {}，收到 {}",
                want, digest
            ));
        }
    }
    if out.is_empty() && digest.is_empty() {
        warnf!("没带 digest（服务端会自己补上；带上更利于跨端核对）".to_string());
    }
    out
}

/// 校验结论里有没有 error（warn 不拦）。
pub fn trace_has_error(issues: &[TraceIssue]) -> bool {
    issues.iter().any(|i| i.level == "error")
}

/// 轨迹里 In/Out 实际达到的内容级别（与声明的 payload 对照，防「标低了」）。
///
/// Go 的 `model.TraceMaxPayload` 同样只被测试与未来的 UI 用；本节点的采集路径
/// 不依赖它（采集方声明什么就记什么），所以这里显式标注：留着是为了对齐规范。
#[allow(dead_code)]
pub fn trace_max_payload(d: &Value) -> String {
    let declared = d
        .get("payload")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut max = if declared.is_empty() {
        TRACE_PAYLOAD_DIGEST.to_string()
    } else {
        declared
    };
    if let Some(steps) = d.get("steps").and_then(|v| v.as_array()) {
        for st in steps {
            let i = st.get("in").and_then(|v| v.as_str()).unwrap_or("").len();
            let o = st.get("out").and_then(|v| v.as_str()).unwrap_or("").len();
            if i == 0 && o == 0 {
                continue;
            }
            let longest = i.max(o);
            let lvl = if longest > TRACE_PREVIEW_MAX {
                TRACE_PAYLOAD_FULL
            } else {
                TRACE_PAYLOAD_PREVIEW
            };
            if trace_payload_rank(lvl) > trace_payload_rank(&max) {
                max = lvl.to_string();
            }
        }
    }
    max
}

/* ---------------- wire 文档（只用于类型门与取投影列） ---------------- */

/// 与 Go 的 `model.TraceDoc` 同形的类型：**唯一的用途**是「字段类型不对就拒收」
/// （Go 靠 `json.Unmarshal` 得到同样的效果），以及取落库的投影列。
///
/// 每个字段都带 `default`：Go 里缺字段不报错（零值），Rust 侧也要一样。
#[allow(dead_code)] // 字段只为「类型门」而存在（Go 靠 json.Unmarshal），未必都被读
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceDoc {
    #[serde(default)]
    pub spec: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub at: String,
    #[serde(rename = "durationMs", default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub source: TraceSource,
    #[serde(default)]
    pub subject: TraceSubject,
    #[serde(default)]
    pub model: TraceModel,
    #[serde(default)]
    pub usage: TraceUsage,
    #[serde(default)]
    pub steps: Option<Vec<TraceStep>>,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub payload: String,
    #[serde(default)]
    pub redaction: TraceRedaction,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub digest: String,
}

/// 这条轨迹从哪来。全部是**声明**（采集方填），服务端如实记录不做校验。
#[allow(dead_code)] // 字段只为「类型门」而存在（Go 靠 json.Unmarshal），未必都被读
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceSource {
    #[serde(default)]
    pub node: String,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub cli: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub region: String,
}

/// 这条轨迹是**关于谁**的：评测的落点就在这里。
#[allow(dead_code)] // 字段只为「类型门」而存在（Go 靠 json.Unmarshal），未必都被读
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceSubject {
    #[serde(rename = "ref", default)]
    pub ref_: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub engine: String,
    #[serde(default)]
    pub policy: String,
}

#[allow(dead_code)] // 字段只为「类型门」而存在（Go 靠 json.Unmarshal），未必都被读
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceModel {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub calls: i64,
}

/// 用量。**金额用整数微美元**：浮点在跨语言序列化上不一致，会毁掉摘要一致性。
#[allow(dead_code)] // 字段只为「类型门」而存在（Go 靠 json.Unmarshal），未必都被读
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceUsage {
    #[serde(rename = "inputTokens", default)]
    pub input_tokens: i64,
    #[serde(rename = "outputTokens", default)]
    pub output_tokens: i64,
    #[serde(rename = "costUsdMicros", default)]
    pub cost_usd_micros: i64,
}

#[allow(dead_code)] // 字段只为「类型门」而存在（Go 靠 json.Unmarshal），未必都被读
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceStep {
    #[serde(default)]
    pub i: i64,
    #[serde(rename = "type", default)]
    pub type_: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub ms: i64,
    #[serde(default)]
    pub status: String,
    #[serde(rename = "inDigest", default)]
    pub in_digest: String,
    #[serde(rename = "outDigest", default)]
    pub out_digest: String,
    #[serde(rename = "in", default)]
    pub in_: String,
    #[serde(rename = "out", default)]
    pub out_: String,
    #[serde(default)]
    pub meta: Option<BTreeMap<String, Value>>,
}

/// 脱敏记录：如实记下做过什么，这样拿到数据集的人知道「这份数据被处理成什么样」。
#[allow(dead_code)] // 字段只为「类型门」而存在（Go 靠 json.Unmarshal），未必都被读
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceRedaction {
    #[serde(default)]
    pub applied: bool,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub rules: Option<Vec<String>>,
}

/* ---------------- 落库的行 ---------------- */

/// 一条落库的轨迹 + 命名空间/归属者摘要（列表一次 join 出，避免 N+1）。
///
/// 列是 Doc 的**索引**（检索/聚合用），不替代 Doc：文档才是权威。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TraceRow {
    pub id: String,
    pub namespace_id: String,
    pub trace_id: String,
    pub kind: String,
    pub status: String,
    pub at: String,
    pub duration_ms: i64,
    pub subject_ref: String,
    pub subject_kind: String,
    pub subject_version: String,
    pub subject_digest: String,
    pub subject_engine: String,
    pub subject_policy: String,
    pub node_name: String,
    pub host: String,
    pub agent_name: String,
    pub user_name: String,
    pub model_provider: String,
    pub model_name: String,
    pub model_calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd_micros: i64,
    pub step_count: i64,
    pub payload_level: String,
    pub tags: String,
    /// 采集时带的标签（不可变文档的一部分，进摘要）。
    pub run_labels: String,
    /// 评测标注最新值的投影（完整历史在 trace_labels 表）。
    pub eval_grade: String,
    pub eval_reward: i64,
    pub eval_score: i64,
    pub eval_split: String,
    pub label_count: i64,
    pub digest: String,
    pub doc: String,
    pub created_by: String,
    pub created_at: Option<String>,
    /// 归属者：把 trace 授权给我的人，是按 owner 判的。
    pub owner_id: String,
    pub owner_name: Option<String>,
    pub ns_slug: String,
    pub ns_name: String,
}

/// 一条落库轨迹的**投影列**（不含 Doc）：聚合/取样用，别把整份文档读进内存。
///
/// `id` / `trace_id` 与 Go 的 `ScanTracesForStats` 一样照选（聚合当前用不到，
/// 但它们是这条投影行的身份，排查与将来的分组要靠它）。
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct TraceProjection {
    pub id: String,
    pub trace_id: String,
    pub kind: String,
    pub status: String,
    pub at: String,
    pub duration_ms: i64,
    pub subject_ref: String,
    pub subject_version: String,
    pub model_provider: String,
    pub model_name: String,
    pub step_count: i64,
    pub payload_level: String,
    pub tags: String,
    /// ⚠️ Go 的 `ScanTracesForStats` **漏了这一列**，于是它的失败归类永远是空的；
    /// 这里补上（聚合口径与 `model.TraceStatsOf` 的文档一致）。
    pub run_labels: String,
    pub agent_name: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd_micros: i64,
    pub eval_grade: String,
    pub eval_reward: i64,
    pub eval_score: i64,
    pub eval_split: String,
    pub label_count: i64,
}

/// 一条评测标注（只追加，不改写）。
///
/// 数值用**千分位整数**：`reward 1` → 1000、`score 0.85` → 850。
/// 不在库里存浮点，聚合与排序就不会因精度飘移而对不上。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TraceLabel {
    pub id: String,
    pub trace_id: String,
    pub key: String,
    pub value: String,
    pub value_num: i64,
    pub has_num: bool,
    pub by: String,
    pub note: String,
    pub created_at: Option<String>,
}

const ROW_COLS: &str = "t.id, t.namespace_id, t.trace_id, t.kind, t.status, t.at, t.duration_ms, \
     t.subject_ref, t.subject_kind, t.subject_version, t.subject_digest, t.subject_engine, t.subject_policy, \
     t.node_name, t.host, t.agent_name, t.user_name, t.model_provider, t.model_name, t.model_calls, \
     t.input_tokens, t.output_tokens, t.cost_usd_micros, t.step_count, t.payload_level, t.tags, t.run_labels, \
     t.eval_grade, t.eval_reward, t.eval_score, t.eval_split, t.label_count, t.digest, t.doc, t.created_by, \
     t.created_at, ns.owner_id AS owner_id, u.name AS owner_name, ns.slug AS ns_slug, ns.name AS ns_name";

const PROJ_COLS: &str = "t.id, t.trace_id, t.kind, t.status, t.at, t.duration_ms, t.subject_ref, \
     t.subject_version, t.model_provider, t.model_name, t.step_count, t.payload_level, t.tags, t.run_labels, \
     t.agent_name, t.input_tokens, t.output_tokens, t.cost_usd_micros, t.eval_grade, t.eval_reward, \
     t.eval_score, t.eval_split, t.label_count";

const LABEL_COLS: &str = "id, trace_id, `key`, value, value_num, has_num, `by`, note, created_at";

/* ---------------- 检索条件 ---------------- */

/// 轨迹检索条件。可见性由调用方折成 `namespace_ids` / `granted_owners` / `all`
/// —— 轨迹**没有「公开」这一档**（跑过的业务数据不该匿名可见）。
#[derive(Debug, Clone, Default)]
pub struct TraceListOpts {
    pub namespace_ids: Vec<String>,
    pub granted_owners: Vec<String>,
    /// 管理员视角：不限命名空间。
    pub all: bool,

    pub ref_: String,
    pub kind: String,
    pub status: String,
    pub agent: String,
    pub node: String,
    pub model: String,
    pub payload: String,
    pub tag: String,
    pub grade: String,
    pub split: String,
    /// 关键词：traceId / 引用 / 节点 / Agent / 备注。
    pub q: String,

    pub since: Option<DateTime<FixedOffset>>,
    pub until: Option<DateTime<FixedOffset>>,

    pub only_labeled: bool,
    pub only_unlabeled: bool,
    pub failures_only: bool,

    pub newest_first: bool,
    pub page: i64,
    pub size: i64,
    pub limit: i64,
}

impl TraceListOpts {
    /// 从查询串构造检索条件（`newest_first` 默认 true）。
    pub fn new() -> Self {
        Self {
            newest_first: true,
            size: 0,
            page: 1,
            ..Default::default()
        }
    }

    /// WHERE 片段与绑定值。绑定一律是字符串（列都是 TEXT）。
    fn conditions(&self) -> (Vec<String>, Vec<String>) {
        let mut cs: Vec<String> = Vec::new();
        let mut bs: Vec<String> = Vec::new();
        if !self.all {
            let mut ors: Vec<String> = Vec::new();
            if !self.namespace_ids.is_empty() {
                ors.push(format!(
                    "t.namespace_id IN ({})",
                    placeholders(self.namespace_ids.len())
                ));
                bs.extend(self.namespace_ids.iter().cloned());
            }
            if !self.granted_owners.is_empty() {
                ors.push(format!(
                    "ns.owner_id IN ({})",
                    placeholders(self.granted_owners.len())
                ));
                bs.extend(self.granted_owners.iter().cloned());
            }
            if ors.is_empty() {
                // 既没有命名空间也没被授权：查不到任何东西（fail-closed）。
                cs.push("1 = 0".to_string());
            } else {
                cs.push(format!("({})", ors.join(" OR ")));
            }
        }
        if !self.ref_.is_empty() {
            cs.push("t.subject_ref = ?".to_string());
            bs.push(self.ref_.clone());
        }
        if !self.kind.is_empty() {
            cs.push("t.kind = ?".to_string());
            bs.push(self.kind.clone());
        }
        if !self.status.is_empty() {
            cs.push("t.status = ?".to_string());
            bs.push(self.status.clone());
        }
        if !self.agent.is_empty() {
            cs.push("t.agent_name = ?".to_string());
            bs.push(self.agent.clone());
        }
        if !self.node.is_empty() {
            cs.push("t.node_name = ?".to_string());
            bs.push(self.node.clone());
        }
        if !self.model.is_empty() {
            // 模型用 name 或 provider/name 两种写法都能命中。
            cs.push(
                "(t.model_name = ? OR t.model_provider || '/' || t.model_name = ?)".to_string(),
            );
            bs.push(self.model.clone());
            bs.push(self.model.clone());
        }
        if !self.payload.is_empty() {
            cs.push("t.payload_level = ?".to_string());
            bs.push(self.payload.clone());
        }
        if !self.tag.is_empty() {
            cs.push("t.tags LIKE ?".to_string());
            bs.push(format!("%\"{}\"%", self.tag));
        }
        if !self.grade.is_empty() {
            cs.push("t.eval_grade = ?".to_string());
            bs.push(self.grade.clone());
        }
        if !self.split.is_empty() {
            cs.push("t.eval_split = ?".to_string());
            bs.push(self.split.clone());
        }
        // 时间列是 TEXT（GORM 落库形态）：把比较值也按同一种形态写进去，
        // 同格式的字符串比较才等于时间比较。写入时统一用 UTC，故这里也归一到 UTC。
        if let Some(t) = self.since {
            cs.push("t.at >= ?".to_string());
            bs.push(format_go(t.with_timezone(&Utc).fixed_offset()));
        }
        if let Some(t) = self.until {
            cs.push("t.at <= ?".to_string());
            bs.push(format_go(t.with_timezone(&Utc).fixed_offset()));
        }
        if self.failures_only {
            cs.push("t.status <> ?".to_string());
            bs.push(TRACE_OK.to_string());
        }
        if self.only_labeled {
            cs.push("t.label_count > 0".to_string());
        }
        if self.only_unlabeled {
            cs.push("t.label_count = 0".to_string());
        }
        if !self.q.is_empty() {
            cs.push(
                "(t.trace_id LIKE ? OR t.subject_ref LIKE ? OR t.node_name LIKE ? OR t.agent_name LIKE ? OR t.doc LIKE ?)"
                    .to_string(),
            );
            let like = format!("%{}%", self.q);
            for _ in 0..5 {
                bs.push(like.clone());
            }
        }
        (cs, bs)
    }

    fn order(&self) -> &'static str {
        if self.newest_first {
            "t.at DESC, t.id DESC"
        } else {
            "t.at ASC, t.id ASC"
        }
    }
}

fn placeholders(n: usize) -> String {
    std::iter::repeat("?").take(n).collect::<Vec<_>>().join(",")
}

fn where_sql(cs: &[String]) -> String {
    if cs.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", cs.join(" AND "))
    }
}

/* ---------------- 上报 ---------------- */

/// 上报写入的错误：**冲突与坏文档要能让调用方分辨**（reject 的 code 不同）。
#[derive(Debug)]
pub enum TraceWriteError {
    /// 不是一份轨迹文档（对应 Go 的 bad_json）。
    BadDoc(String),
    /// at 不是 RFC3339（对应 Go 的 store 错误）。
    BadAt(String),
    /// 同 id 不同摘要：说明采集方改了内容却没换 id。必须拒，否则数据集会静默丢数据。
    Conflict(String),
    Db(sqlx::Error),
}

impl TraceWriteError {
    pub fn message(&self) -> String {
        match self {
            TraceWriteError::BadDoc(m)
            | TraceWriteError::BadAt(m)
            | TraceWriteError::Conflict(m) => m.clone(),
            TraceWriteError::Db(e) => e.to_string(),
        }
    }
}

/// 解析好、可落库的一份轨迹文档。
#[derive(Debug)]
pub struct ParsedTrace {
    /// 归一化后的文档（落库的 doc 就是它）。
    pub value: Value,
    pub doc_json: String,
    pub id: String,
    pub digest: String,
    /// `at` 的原文（落库时再解析，与 Go 一样把「at 不合法」交给校验先拦）。
    at: String,
    doc: TraceDoc,
}

/// 类型门 + 归一化。**类型不对就拒**（等价于 Go 的 `json.Unmarshal` 失败）。
pub fn parse_trace_doc(mut value: Value) -> Result<ParsedTrace, TraceWriteError> {
    let bad = |e: serde_json::Error| TraceWriteError::BadDoc(format!("不是一份轨迹文档: {e}"));
    // 先用归一化之前的文档过类型门：与 Go 一样，「字段类型不对」优先于「字段值不合法」。
    TraceDoc::deserialize(&value).map_err(bad)?;
    normalize_trace(&mut value);
    let doc = TraceDoc::deserialize(&value)
        .map_err(|e| TraceWriteError::BadDoc(format!("不是一份轨迹文档: {e}")))?;
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let digest = value
        .get("digest")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let doc_json = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    let at = doc.at.clone();
    Ok(ParsedTrace {
        value,
        doc_json,
        id,
        digest,
        at,
        doc,
    })
}

/// 写入一条轨迹（按 (命名空间, traceId) 幂等）。
///
/// 三种结果：
///   - 新的 → `Created`；
///   - 同 id 同摘要 → `Duplicate`（**重传是正常的**：网络重试、离线补报）；
///   - 同 id 不同摘要 → `Err(Conflict)`（内容变了却复用 id，必须让采集方换 id）。
pub async fn insert_trace(
    pool: &SqlitePool,
    namespace_id: &str,
    created_by: &str,
    parsed: &ParsedTrace,
) -> Result<TraceInsertOutcome, TraceWriteError> {
    let at = DateTime::parse_from_rfc3339(&parsed.at)
        .map_err(|e| TraceWriteError::BadAt(format!("at 不是 RFC3339: {e}")))?;
    let db = TraceWriteError::Db;

    let existing: Option<TraceRow> = sqlx::query_as::<_, TraceRow>(&format!(
        "SELECT {ROW_COLS} FROM traces t JOIN namespaces ns ON ns.id = t.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id WHERE t.namespace_id = ? AND t.trace_id = ? LIMIT 1"
    ))
    .bind(namespace_id)
    .bind(&parsed.id)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    if let Some(old) = existing {
        if old.digest == parsed.digest {
            return Ok(TraceInsertOutcome::Duplicate(old));
        }
        return Err(TraceWriteError::Conflict(format!(
            "trace_conflict: traceId {} 已存在且内容不同（旧摘要 {}，新摘要 {}）—— 换一个 id",
            parsed.id, old.digest, parsed.digest
        )));
    }

    let d = &parsed.doc;
    let tags = match &d.tags {
        Some(t) => serde_json::to_string(t).unwrap_or_else(|_| "null".to_string()),
        // Go 的 nil 切片 marshal 出来就是 `null`，不是 `[]` —— 保持一致。
        None => "null".to_string(),
    };
    let run_labels = match &d.labels {
        Some(l) => serde_json::to_string(l).unwrap_or_else(|_| "null".to_string()),
        None => "null".to_string(),
    };
    let row_id = new_id("TR");
    sqlx::query(
        "INSERT INTO traces (id, namespace_id, trace_id, kind, status, at, duration_ms, \
         subject_ref, subject_kind, subject_version, subject_digest, subject_engine, subject_policy, \
         node_name, host, agent_name, user_name, model_provider, model_name, model_calls, \
         input_tokens, output_tokens, cost_usd_micros, step_count, payload_level, tags, run_labels, \
         eval_grade, eval_reward, eval_score, eval_split, label_count, digest, doc, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
         '', 0, 0, '', 0, ?, ?, ?, ?)",
    )
    .bind(&row_id)
    .bind(namespace_id)
    .bind(&parsed.id)
    .bind(&d.kind)
    .bind(&d.status)
    .bind(format_go(at.with_timezone(&Utc).fixed_offset()))
    .bind(d.duration_ms)
    .bind(&d.subject.ref_)
    .bind(&d.subject.kind)
    .bind(&d.subject.version)
    .bind(&d.subject.digest)
    .bind(&d.subject.engine)
    .bind(&d.subject.policy)
    .bind(&d.source.node)
    .bind(&d.source.host)
    .bind(&d.source.agent)
    .bind(&d.source.user)
    .bind(&d.model.provider)
    .bind(&d.model.name)
    .bind(d.model.calls)
    .bind(d.usage.input_tokens)
    .bind(d.usage.output_tokens)
    .bind(d.usage.cost_usd_micros)
    .bind(d.steps.as_ref().map(|s| s.len()).unwrap_or(0) as i64)
    .bind(&d.payload)
    .bind(&tags)
    .bind(&run_labels)
    .bind(&parsed.digest)
    .bind(&parsed.doc_json)
    .bind(created_by)
    .bind(now_go())
    .execute(pool)
    .await
    .map_err(db)?;

    let row = get_trace(pool, &row_id)
        .await
        .map_err(db)?
        .ok_or_else(|| TraceWriteError::Db(sqlx::Error::RowNotFound))?;
    Ok(TraceInsertOutcome::Created(row))
}

/// 一条上报的写入结果。
///
/// `Duplicate` 也把已存在的行带出来（Go 的 `InsertTrace` 一样）：调用方按
/// 「重传是正常现象」处理即可 —— 需要时还能回一句「早就收下了」。
#[derive(Debug)]
#[allow(dead_code)]
pub enum TraceInsertOutcome {
    Created(TraceRow),
    Duplicate(TraceRow),
}

/* ---------------- 读取 ---------------- */

/// 列表（`size` 优先，其次 `limit`）。
pub async fn list_traces(
    pool: &SqlitePool,
    o: &TraceListOpts,
) -> Result<(Vec<TraceRow>, i64), sqlx::Error> {
    let (cs, bs) = o.conditions();
    let w = where_sql(&cs);
    let count_sql =
        format!("SELECT COUNT(*) FROM traces t JOIN namespaces ns ON ns.id = t.namespace_id{w}");
    let mut cq = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &bs {
        cq = cq.bind(b);
    }
    let total = cq.fetch_one(pool).await?;

    let mut sql = format!(
        "SELECT {ROW_COLS} FROM traces t JOIN namespaces ns ON ns.id = t.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id{w} ORDER BY {}",
        o.order()
    );
    if o.size > 0 {
        let page = if o.page < 1 { 1 } else { o.page };
        sql.push_str(&format!(" LIMIT {} OFFSET {}", o.size, (page - 1) * o.size));
    } else if o.limit > 0 {
        sql.push_str(&format!(" LIMIT {}", o.limit));
    }
    let mut q = sqlx::query_as::<_, TraceRow>(&sql);
    for b in &bs {
        q = q.bind(b);
    }
    Ok((q.fetch_all(pool).await?, total))
}

/// 取一条（含 Doc）。id 可以是 `TR-…` 行 id，也可以是 traceId。
pub async fn get_trace(pool: &SqlitePool, id: &str) -> Result<Option<TraceRow>, sqlx::Error> {
    sqlx::query_as::<_, TraceRow>(&format!(
        "SELECT {ROW_COLS} FROM traces t JOIN namespaces ns ON ns.id = t.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id WHERE t.id = ? OR t.trace_id = ? LIMIT 1"
    ))
    .bind(id)
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// 取一批**含 Doc** 的轨迹（数据集导出用）。
///
/// 返回 `truncated=true` 表示命中条数超过 limit：**如实告诉调用方被截断了** ——
/// 悄悄少给几行，训练集就少了一截，而且没人会发现。
pub async fn export_traces(
    pool: &SqlitePool,
    o: &TraceListOpts,
    limit: i64,
) -> Result<(Vec<TraceRow>, bool), sqlx::Error> {
    let limit = if limit <= 0 { 5000 } else { limit };
    let (cs, bs) = o.conditions();
    let sql = format!(
        "SELECT {ROW_COLS} FROM traces t JOIN namespaces ns ON ns.id = t.namespace_id \
         LEFT JOIN users u ON u.id = ns.owner_id{} ORDER BY {} LIMIT {}",
        where_sql(&cs),
        o.order(),
        limit + 1
    );
    let mut q = sqlx::query_as::<_, TraceRow>(&sql);
    for b in &bs {
        q = q.bind(b);
    }
    let mut rows = q.fetch_all(pool).await?;
    if rows.len() as i64 > limit {
        rows.truncate(limit as usize);
        return Ok((rows, true));
    }
    Ok((rows, false))
}

/// 取投影列（不含 Doc）用于聚合。上限防止一条 stats 查询把内存吃光。
pub async fn scan_traces_for_stats(
    pool: &SqlitePool,
    o: &TraceListOpts,
    cap: i64,
) -> Result<(Vec<TraceProjection>, bool), sqlx::Error> {
    let cap = if cap <= 0 { 200000 } else { cap };
    let (cs, bs) = o.conditions();
    let sql = format!(
        "SELECT {PROJ_COLS} FROM traces t JOIN namespaces ns ON ns.id = t.namespace_id{} \
         ORDER BY t.at ASC LIMIT {}",
        where_sql(&cs),
        cap + 1
    );
    let mut q = sqlx::query_as::<_, TraceProjection>(&sql);
    for b in &bs {
        q = q.bind(b);
    }
    let mut rows = q.fetch_all(pool).await?;
    if rows.len() as i64 > cap {
        rows.truncate(cap as usize);
        return Ok((rows, true));
    }
    Ok((rows, false))
}

/// 删一条轨迹（连同它的标注）。**删就是删**：轨迹不是审计记录，
/// 归属者有权把自己采集的数据删掉（审计面记的是「谁做了什么」，与轨迹数据无关）。
pub async fn delete_trace(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM trace_labels WHERE trace_id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM traces WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 全量轨迹条数（`/api/meta` 的 `counts.traces` 用它）。
pub async fn count(pool: &SqlitePool) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT COUNT(*) FROM traces")
        .fetch_one(pool)
        .await
}

/* ---------------- 评测标注 ---------------- */

/// 追加一条标注。
pub struct TraceLabelInput {
    /// 行 id（`TR-…`）。
    pub trace_id: String,
    pub key: String,
    pub value: String,
    pub value_num: i64,
    pub has_num: bool,
    pub by: String,
    pub note: String,
}

/// 追加一条标注，并把最新值投影回 traces 行（供检索与聚合）。
pub async fn add_trace_label(
    pool: &SqlitePool,
    in_: TraceLabelInput,
) -> Result<TraceLabel, sqlx::Error> {
    let l = TraceLabel {
        id: new_id("TL"),
        trace_id: in_.trace_id.clone(),
        key: in_.key.clone(),
        value: in_.value.clone(),
        value_num: in_.value_num,
        has_num: in_.has_num,
        by: in_.by.clone(),
        note: in_.note.clone(),
        created_at: Some(now_go()),
    };
    sqlx::query(&format!(
        "INSERT INTO trace_labels ({LABEL_COLS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
    ))
    .bind(&l.id)
    .bind(&l.trace_id)
    .bind(&l.key)
    .bind(&l.value)
    .bind(l.value_num)
    .bind(l.has_num)
    .bind(&l.by)
    .bind(&l.note)
    .bind(&l.created_at)
    .execute(pool)
    .await?;

    // 计数器与投影：失败也不该让标注本身丢（标注已经落库了），所以只把错误带出去。
    let upd: Option<(&str, i64)> = match in_.key.as_str() {
        "reward" => Some(("eval_reward", in_.value_num)),
        "score" => Some(("eval_score", in_.value_num)),
        _ => None,
    };
    match in_.key.as_str() {
        "grade" => {
            sqlx::query("UPDATE traces SET eval_grade = ? WHERE id = ?")
                .bind(&in_.value)
                .bind(&in_.trace_id)
                .execute(pool)
                .await?;
        }
        "split" => {
            sqlx::query("UPDATE traces SET eval_split = ? WHERE id = ?")
                .bind(&in_.value)
                .bind(&in_.trace_id)
                .execute(pool)
                .await?;
        }
        _ => {
            if let Some((col, v)) = upd {
                sqlx::query(&format!("UPDATE traces SET {col} = ? WHERE id = ?"))
                    .bind(v)
                    .bind(&in_.trace_id)
                    .execute(pool)
                    .await?;
            }
        }
    }
    sqlx::query("UPDATE traces SET label_count = label_count + 1 WHERE id = ?")
        .bind(&in_.trace_id)
        .execute(pool)
        .await?;
    Ok(l)
}

/// 一条轨迹的标注历史（按时间正序）。
pub async fn list_trace_labels(
    pool: &SqlitePool,
    trace_id: &str,
) -> Result<Vec<TraceLabel>, sqlx::Error> {
    sqlx::query_as::<_, TraceLabel>(&format!(
        "SELECT {LABEL_COLS} FROM trace_labels WHERE trace_id = ? ORDER BY created_at ASC"
    ))
    .bind(trace_id)
    .fetch_all(pool)
    .await
}

/* ---------------- 聚合（能力评估） ---------------- */

/// 一组耗时/步数的分位数（毫秒或步）。空样本全 0 —— 「没有数据」与「数据是 0」
/// 在展示上要靠 `total` 区分。
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TracePercentiles {
    pub min: i64,
    pub p50: i64,
    pub p90: i64,
    pub p99: i64,
    pub max: i64,
    pub avg: i64,
}

/// 一组轨迹的聚合结论 —— 「业务能力评估」的最小数据集。
#[derive(Debug, Clone, Serialize)]
pub struct TraceStats {
    pub total: i64,
    #[serde(rename = "byStatus")]
    pub by_status: BTreeMap<String, i64>,
    #[serde(rename = "byKind")]
    pub by_kind: BTreeMap<String, i64>,
    #[serde(rename = "byAgent")]
    pub by_agent: BTreeMap<String, i64>,
    #[serde(rename = "byModel")]
    pub by_model: BTreeMap<String, i64>,
    #[serde(rename = "byPayload")]
    pub by_payload: BTreeMap<String, i64>,
    /// 同一个 ref 的不同 version 分组 —— 评测改版前后的落点。
    #[serde(rename = "bySubjectVersion")]
    pub by_subject_version: BTreeMap<String, i64>,
    #[serde(rename = "byTag")]
    pub by_tag: BTreeMap<String, i64>,
    pub grades: BTreeMap<String, i64>,
    pub failures: BTreeMap<String, i64>,
    pub splits: BTreeMap<String, i64>,
    #[serde(rename = "durationMs")]
    pub duration_ms: TracePercentiles,
    pub steps: TracePercentiles,
    #[serde(rename = "inputTokens")]
    pub input_tokens: i64,
    #[serde(rename = "outputTokens")]
    pub output_tokens: i64,
    #[serde(rename = "costUsdMicros")]
    pub cost_usd_micros: i64,
    /// 标注覆盖率：没有它，「平均分」会被未标注的轨迹稀释。
    pub labeled: i64,
    pub unlabeled: i64,
    #[serde(rename = "rewardSumMilli")]
    pub reward_sum_milli: i64,
    #[serde(rename = "scoreSumMilli")]
    pub score_sum_milli: i64,
    #[serde(rename = "scoreAvgMilli")]
    pub score_avg_milli: i64,
    #[serde(rename = "firstAt", skip_serializing_if = "String::is_empty")]
    pub first_at: String,
    #[serde(rename = "lastAt", skip_serializing_if = "String::is_empty")]
    pub last_at: String,
}

impl Default for TraceStats {
    /// JSON 里给空对象而不是 null，调用方少一次判空。
    fn default() -> Self {
        Self {
            total: 0,
            by_status: BTreeMap::new(),
            by_kind: BTreeMap::new(),
            by_agent: BTreeMap::new(),
            by_model: BTreeMap::new(),
            by_payload: BTreeMap::new(),
            by_subject_version: BTreeMap::new(),
            by_tag: BTreeMap::new(),
            grades: BTreeMap::new(),
            failures: BTreeMap::new(),
            splits: BTreeMap::new(),
            duration_ms: TracePercentiles::default(),
            steps: TracePercentiles::default(),
            input_tokens: 0,
            output_tokens: 0,
            cost_usd_micros: 0,
            labeled: 0,
            unlabeled: 0,
            reward_sum_milli: 0,
            score_sum_milli: 0,
            score_avg_milli: 0,
            first_at: String::new(),
            last_at: String::new(),
        }
    }
}

/// 从**已排序**的样本算分位数（最近邻，不插值）。
pub fn percentiles(sorted: &[i64]) -> TracePercentiles {
    if sorted.is_empty() {
        return TracePercentiles::default();
    }
    let at = |q: f64| -> i64 {
        if q <= 0.0 {
            return sorted[0];
        }
        let mut idx = (q * (sorted.len() - 1) as f64 + 0.5) as i64;
        if idx < 0 {
            idx = 0;
        }
        if idx as usize >= sorted.len() {
            idx = sorted.len() as i64 - 1;
        }
        sorted[idx as usize]
    };
    let sum: i64 = sorted.iter().sum();
    TracePercentiles {
        min: sorted[0],
        p50: at(0.50),
        p90: at(0.90),
        p99: at(0.99),
        max: sorted[sorted.len() - 1],
        avg: sum / sorted.len() as i64,
    }
}

/// 由（投影列的）轨迹行算聚合结论。**纯函数**：喂同一批行，结论必须一样。
///
/// 放在这里而不是 HTTP 层：这是评测口径本身（成功怎么算、失败怎么归类、
/// 覆盖率怎么算），与传输无关；放这儿才能被单测钉住。
pub fn trace_stats_of(rows: &[TraceProjection]) -> TraceStats {
    let mut st = TraceStats::default();
    let mut durations: Vec<i64> = Vec::with_capacity(rows.len());
    let mut steps: Vec<i64> = Vec::with_capacity(rows.len());
    for r in rows {
        st.total += 1;
        bump(&mut st.by_status, &r.status);
        bump(&mut st.by_kind, &r.kind);
        bump(&mut st.by_payload, &r.payload_level);
        bump(&mut st.by_agent, &r.agent_name);
        bump(
            &mut st.by_model,
            format!("{}/{}", r.model_provider, r.model_name)
                // Go 是 Trim(TrimSpace(x), "/")：先去空白再去斜杠。
                .trim()
                .trim_matches('/'),
        );
        bump(
            &mut st.by_subject_version,
            &subject_key(&r.subject_ref, &r.subject_version),
        );
        for t in parse_string_list(&r.tags) {
            bump(&mut st.by_tag, &t);
        }
        durations.push(r.duration_ms);
        steps.push(r.step_count);
        st.input_tokens += r.input_tokens;
        st.output_tokens += r.output_tokens;
        st.cost_usd_micros += r.cost_usd_micros;
        if r.label_count > 0 {
            st.labeled += 1;
        } else {
            st.unlabeled += 1;
        }
        if !r.eval_grade.is_empty() {
            bump(&mut st.grades, &r.eval_grade);
        }
        if !r.eval_split.is_empty() {
            bump(&mut st.splits, &r.eval_split);
        }
        st.reward_sum_milli += r.eval_reward;
        st.score_sum_milli += r.eval_score;
        // 失败归类：只有跑失败/取消的才读 run-time labels 里的原因。
        if r.status != TRACE_OK {
            if let Some(f) = failure_of(r) {
                bump(&mut st.failures, &f);
            }
        }
        let at = rfc3339_utc(&r.at).unwrap_or_else(|| r.at.clone());
        if st.first_at.is_empty() || at < st.first_at {
            st.first_at = at.clone();
        }
        if st.last_at.is_empty() || at > st.last_at {
            st.last_at = at;
        }
    }
    durations.sort();
    steps.sort();
    st.duration_ms = percentiles(&durations);
    st.steps = percentiles(&steps);
    // 平均分只对**打过分的**算：拿未标注的轨迹去稀释分母，是不诚实的平均数。
    if st.labeled > 0 {
        st.score_avg_milli = st.score_sum_milli / st.labeled;
    }
    st
}

/// 从 run-time labels 里取失败归类（`failure` / `failReason` / `error` 都认）。
fn failure_of(r: &TraceProjection) -> Option<String> {
    if r.run_labels.trim().is_empty() || r.run_labels == "{}" {
        return None;
    }
    let m: Map<String, Value> = serde_json::from_str(&r.run_labels).ok()?;
    for k in ["failure", "failReason", "error"] {
        if let Some(Value::String(s)) = m.get(k) {
            if !s.trim().is_empty() {
                return Some(s.clone());
            }
        }
    }
    None
}

fn bump(m: &mut BTreeMap<String, i64>, k: &str) {
    if k.trim().is_empty() {
        return;
    }
    *m.entry(k.to_string()).or_insert(0) += 1;
}

/// 把「哪个制品的哪一版」合成一个键：评测要按它分组比较。
fn subject_key(ref_: &str, version: &str) -> String {
    let r = ref_.trim();
    let v = version.trim();
    match (r.is_empty(), v.is_empty()) {
        (true, true) => String::new(),
        (true, false) => v.to_string(),
        (false, true) => r.to_string(),
        _ => format!("{r}@{v}"),
    }
}

/// 解析库里的 JSON 字符串数组（脏值一律当空列表，别让一个字段把接口打成 500）。
fn parse_string_list(s: &str) -> Vec<String> {
    let t = s.trim();
    if t.is_empty() || t == "[]" {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(t).unwrap_or_default()
}

/// 库里/响应里的时间形态（GORM 落库的本地偏移形态）→ UTC 的 RFC3339。
pub fn rfc3339_utc(s: &str) -> Option<String> {
    let t = ncc_core::timeutil::parse_time(s)?;
    Some(
        t.with_timezone(&Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/* ---------------- 内置集合（取用即声明） ---------------- */

/// 第一次写入轨迹时，顺手在命名空间里建出内置的 `trace` 集合声明（幂等）。
///
/// 与 Go 的 `markBuiltinUsed(nsID, "trace")` → `EnsureBuiltinCollection` 同效：
/// 声明是代码里的常量，记录要挂在一行 `collections` 上，所以「取用即声明」。
/// 这些常量与 `model/builtin.go` 的声明逐字对应（字段语法、索引、只追加）。
pub async fn mark_builtin_used(
    pool: &SqlitePool,
    ns_id: &str,
    kind: &str,
) -> Result<(), sqlx::Error> {
    if ns_id.is_empty() {
        return Ok(());
    }
    let kind = kind.trim().to_lowercase();
    if exists(
        pool,
        "SELECT COUNT(*) FROM collections WHERE namespace_id = ? AND kind = ?",
        &[ns_id, kind.as_str()],
    )
    .await
    .unwrap_or(true)
    {
        return Ok(());
    }
    let (title, summary, reason, fields, index) = match kind.as_str() {
        "trace" => (
            "运行轨迹",
            "一次运行的真实流水 + 评测：默认私有、显式采集",
            "轨迹只追加：它记的是「当时发生了什么」，改一个字就不再是证据",
            // 声明语法与 Go 的 `BuiltinDecl.Fields`（`[]string` 的 "名字:类型"）一致。
            serde_json::json!([
                format!("kind:enum:{}", TRACE_KINDS.join("|")),
                format!("status:enum:{}", TRACE_STATUSES.join("|")),
                "label:string",
                "model:string",
                "tool:string",
                "digest:string!",
                "steps:int",
                "note:text?search",
            ])
            .to_string(),
            serde_json::json!(["kind", "status", "label", "model"]).to_string(),
        ),
        // 本族只认 trace；别的内置集合由各自的族在取用时声明。
        _ => return Ok(()),
    };
    let now = now_go();
    sqlx::query(
        "INSERT INTO collections (id, namespace_id, kind, title, summary, reason, mutable, history, \
         append_only, visibility, max_bytes, fields, `index`, dedupe_by, default_ttl, status, \
         created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 0, 0, 1, 'private', 262144, ?, ?, '', 0, 'active', '', ?, ?)",
    )
    .bind(new_id("C-"))
    .bind(ns_id)
    .bind(&kind)
    .bind(title)
    .bind(summary)
    .bind(reason)
    .bind(&fields)
    .bind(&index)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时文件库：**不要**用 `sqlite::memory:` + 连接池（sqlx 会把它落成同名文件，
    /// 同进程的多个测试互相打架）。
    async fn pool(name: &str) -> SqlitePool {
        let dir = std::env::temp_dir().join(format!("ncc-traces-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
        ncc_core::pool::migrate(&p, crate::schema::DDL)
            .await
            .unwrap();
        p
    }

    async fn mk_ns(p: &SqlitePool, slug: &str) -> (String, String) {
        let u = super::super::users::create(p, slug, &format!("{slug}@corp.com"), "h")
            .await
            .unwrap();
        let ns = super::super::namespaces::create_account(p, &u.id, slug, slug)
            .await
            .unwrap();
        (u.id, ns.id)
    }

    /// 与 `ncc-cli/cli/src/trace.rs::vector_doc` / Go `model/trace_test.go::vectorDoc`
    /// **逐字段相同**的跨语言向量。
    fn vector_doc() -> Value {
        serde_json::json!({
            "spec": TRACE_SPEC,
            "id": "TRC-0123456789abcdef0123",
            "kind": "agent",
            "at": "2026-09-26T10:00:00Z",
            "durationMs": 1234,
            "status": "ok",
            "source": {
                "node": "office-master", "host": "mac-1", "cli": "ncc/0.1.3",
                "agent": "harness-use", "user": "@me", "region": "shanghai-intranet"
            },
            "subject": {
                "ref": "@alice/hotel-skill", "kind": "skill", "version": "0.1.0",
                "digest": "sha256:aa11", "engine": "wasm", "policy": "strict"
            },
            "model": { "provider": "openai", "name": "gpt-4o-mini", "calls": 2 },
            "usage": { "inputTokens": 120, "outputTokens": 45, "costUsdMicros": 2100 },
            "steps": [
                { "i": 0, "type": "llm", "name": "plan", "ms": 400, "status": "ok",
                  "inDigest": "sha256:bb22", "outDigest": "sha256:cc33" },
                { "i": 1, "type": "tool", "name": "kb.search", "ms": 30, "status": "ok",
                  "inDigest": "sha256:dd44", "outDigest": "sha256:ee55" }
            ],
            "labels": { "task": "book-hotel", "grade": "pass", "reward": 1 },
            "tags": ["prod", "hotel"],
            "payload": "preview",
            "redaction": { "applied": true, "level": "strict", "rules": ["email", "api_key"] },
            "notes": "示例轨迹"
        })
    }

    fn simple_doc(id: &str, status: &str, ref_: &str, version: &str) -> Value {
        let mut d = serde_json::json!({
            "spec": TRACE_SPEC,
            "id": id,
            "kind": TRACE_KIND_AGENT,
            "at": "2026-09-26T10:00:00Z",
            "durationMs": 100,
            "status": status,
            "source": { "node": "n1", "agent": "harness-use" },
            "subject": { "ref": ref_, "version": version },
            "model": { "provider": "openai", "name": "gpt-4o-mini", "calls": 1 },
            "usage": { "inputTokens": 10, "outputTokens": 5, "costUsdMicros": 700 },
            "steps": [
                { "i": 0, "type": "llm", "name": "plan", "ms": 80, "status": "ok",
                  "inDigest": "sha256:a", "outDigest": "sha256:b" }
            ],
            "labels": { "task": "book-hotel" },
            "tags": ["prod"],
            "payload": "digest"
        });
        normalize_trace(&mut d);
        d
    }

    async fn seed(p: &SqlitePool, ns: &str, by: &str, d: Value) -> TraceRow {
        let parsed = parse_trace_doc(d).unwrap();
        match insert_trace(p, ns, by, &parsed).await.unwrap() {
            TraceInsertOutcome::Created(r) => r,
            TraceInsertOutcome::Duplicate(_) => panic!("不该是重复"),
        }
    }

    /// **跨语言基线**：这个十六进制串与 Go `model/trace_test.go`、CLI
    /// `cli/src/trace.rs` 里的常量必须一样。规则一改（加字段、换分隔符），
    /// 三处必须同时改 —— 否则上线后所有上报都会被服务端以 digest 不符拒掉。
    #[test]
    fn 摘要与跨语言基线一致() {
        let d = vector_doc();
        assert_eq!(
            trace_digest(&d),
            "sha256:867eb490e45d2ec56189ee2dd5513eec184465754e3e334ffee71d032be78526"
        );
        assert_eq!(trace_id(&d), "TRC-867eb490e45d2ec56189");
    }

    #[test]
    fn 摘要不含原文但覆盖结构与标签() {
        let a = vector_doc();
        let mut b = vector_doc();
        b["steps"][0]["in"] = Value::String("原始提示词 a@b.com".into());
        b["steps"][0]["out"] = Value::String("模型输出原文".into());
        assert_eq!(trace_digest(&a), trace_digest(&b), "原文不该进摘要");

        let mut c = vector_doc();
        c["steps"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({ "i": 2, "type": "note", "name": "extra" }));
        assert_ne!(trace_digest(&a), trace_digest(&c), "结构必须进摘要");

        let mut d = vector_doc();
        d["labels"]["grade"] = Value::String("fail".into());
        assert_ne!(trace_digest(&a), trace_digest(&d), "标签必须进摘要");
    }

    #[test]
    fn 摘要对标签顺序不敏感() {
        let a = vector_doc();
        let mut b = vector_doc();
        b["labels"] = serde_json::json!({ "reward": 1, "grade": "pass", "task": "book-hotel" });
        assert_eq!(trace_digest(&a), trace_digest(&b));
    }

    #[test]
    fn 校验拦住必须拦的() {
        let mut good = vector_doc();
        normalize_trace(&mut good);
        assert!(
            !trace_has_error(&validate_trace(&good)),
            "{:?}",
            validate_trace(&good)
        );

        let cases: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
            (
                "spec 不对",
                Box::new(|d: &mut Value| d["spec"] = Value::String("ncc-trace/v0".into())),
            ),
            (
                "缺 id",
                Box::new(|d: &mut Value| d["id"] = Value::String(String::new())),
            ),
            (
                "kind 不认识",
                Box::new(|d: &mut Value| d["kind"] = Value::String("chat".into())),
            ),
            (
                "status 不认识",
                Box::new(|d: &mut Value| d["status"] = Value::String("done".into())),
            ),
            (
                "payload 不认识",
                Box::new(|d: &mut Value| d["payload"] = Value::String("everything".into())),
            ),
            (
                "at 不是 RFC3339",
                Box::new(|d: &mut Value| d["at"] = Value::String("2026-09-26 10:00".into())),
            ),
            (
                "负耗时",
                Box::new(|d: &mut Value| d["durationMs"] = Value::from(-1)),
            ),
            (
                "负 token",
                Box::new(|d: &mut Value| d["usage"]["inputTokens"] = Value::from(-5)),
            ),
            (
                "步骤类型不认识",
                Box::new(|d: &mut Value| d["steps"][0]["type"] = Value::String("think".into())),
            ),
            (
                "声明 digest 却带原文",
                Box::new(|d: &mut Value| {
                    d["payload"] = Value::String("digest".into());
                    d["steps"][0]["in"] = Value::String("原文".into());
                }),
            ),
            (
                "摘要被改过",
                Box::new(|d: &mut Value| d["digest"] = Value::String("sha256:deadbeef".into())),
            ),
        ];
        for (name, f) in cases {
            let mut d = vector_doc();
            normalize_trace(&mut d);
            f(&mut d);
            assert!(
                trace_has_error(&validate_trace(&d)),
                "「{name}」应当被拒，但校验通过了：{:?}",
                validate_trace(&d)
            );
        }
    }

    #[test]
    fn 没带摘要只提醒不拦() {
        let mut d = vector_doc();
        normalize_trace(&mut d);
        d["digest"] = Value::String(String::new());
        let issues = validate_trace(&d);
        assert!(!trace_has_error(&issues), "{issues:?}");
        assert!(!issues.is_empty(), "没带摘要应当有提醒");
    }

    #[test]
    fn 内容级别标低了会被发现() {
        let mut d = vector_doc();
        d["payload"] = Value::String("digest".into());
        d["steps"][0]["out"] = Value::String("x".repeat(TRACE_PREVIEW_MAX + 1));
        assert_eq!(trace_max_payload(&d), TRACE_PAYLOAD_FULL);
    }

    #[test]
    fn 类型不对按坏文档拒收() {
        let mut d = simple_doc("TRC-type", TRACE_OK, "@a/x", "0.1.0");
        d["tags"] = Value::from("prod"); // Go 里 unmarshal 进 []string 会失败
        match parse_trace_doc(d) {
            Err(TraceWriteError::BadDoc(m)) => assert!(m.starts_with("不是一份轨迹文档")),
            other => panic!("应当按坏文档拒：{other:?}"),
        }
        // 归一化会补 id / payload / digest（缺字段不该被当成坏文档）
        let mut bare = serde_json::json!({ "kind": TRACE_KIND_AGENT, "status": TRACE_OK,
            "at": "2026-09-26T10:00:00Z", "payload": "digest" });
        normalize_trace(&mut bare);
        assert!(bare["id"].as_str().unwrap().starts_with("TRC-"));
        assert!(bare["digest"].as_str().unwrap().starts_with("sha256:"));
    }

    #[tokio::test]
    async fn 写入幂等与同id不同摘要冲突() {
        let p = pool("idempotent").await;
        let (u, ns) = mk_ns(&p, "alice").await;
        let d = simple_doc("TRC-1", TRACE_OK, "@alice/skill", "0.1.0");
        let parsed = parse_trace_doc(d.clone()).unwrap();
        let row = match insert_trace(&p, &ns, &u, &parsed).await.unwrap() {
            TraceInsertOutcome::Created(r) => r,
            _ => panic!("首次写入应当 created"),
        };
        assert_eq!(row.payload_level, TRACE_PAYLOAD_DIGEST);
        assert_eq!(row.step_count, 1);
        assert_eq!(row.ns_slug, "alice");
        assert_eq!(row.created_by, u);

        // 重传：网络重试 / 离线补报是常态，必须当「已收下」而不是错误。
        let again = match insert_trace(&p, &ns, &u, &parse_trace_doc(d).unwrap())
            .await
            .unwrap()
        {
            TraceInsertOutcome::Duplicate(r) => r,
            _ => panic!("同 id 同摘要应当 duplicate"),
        };
        assert_eq!(again.id, row.id);

        // 同 id 但内容变了：**必须拒** —— 静默覆盖会让数据集少掉一条且没人发现。
        let d2 = simple_doc("TRC-1", TRACE_ERROR, "@alice/skill", "0.1.0");
        let err = insert_trace(&p, &ns, &u, &parse_trace_doc(d2).unwrap())
            .await
            .unwrap_err();
        match err {
            TraceWriteError::Conflict(m) => {
                assert!(m.starts_with("trace_conflict: traceId TRC-1"), "{m}");
                assert!(m.contains("换一个 id"), "{m}");
            }
            other => panic!("应当报冲突：{other:?}"),
        }
    }

    #[tokio::test]
    async fn 可见性默认关闭() {
        let p = pool("visibility").await;
        let (u1, ns1) = mk_ns(&p, "alice").await;
        let (_u2, ns2) = mk_ns(&p, "bob").await;
        seed(
            &p,
            &ns1,
            &u1,
            simple_doc("TRC-a", TRACE_OK, "@alice/skill", "0.1.0"),
        )
        .await;
        seed(
            &p,
            &ns2,
            &u1,
            simple_doc("TRC-b", TRACE_OK, "@bob/other", "0.2.0"),
        )
        .await;

        // 只看自己的命名空间 → 只有自己那条。
        let o = TraceListOpts {
            namespace_ids: vec![ns1.clone()],
            ..TraceListOpts::new()
        };
        let (rows, total) = list_traces(&p, &o).await.unwrap();
        assert_eq!((rows.len() as i64, total), (1, 1));
        // 什么都没给 → 查不到（fail-closed，别把「没给条件」当成「看全部」）。
        assert_eq!(list_traces(&p, &TraceListOpts::new()).await.unwrap().1, 0);
        // 管理员视角：全部
        let all = TraceListOpts {
            all: true,
            ..TraceListOpts::new()
        };
        assert_eq!(list_traces(&p, &all).await.unwrap().1, 2);
        // 被授权者的命名空间可见（owner_id 是 ns2 的 owner）
        let owner: String = sqlx::query_scalar("SELECT owner_id FROM namespaces WHERE id = ?")
            .bind(&ns2)
            .fetch_one(&p)
            .await
            .unwrap();
        let granted = TraceListOpts {
            granted_owners: vec![owner],
            ..TraceListOpts::new()
        };
        assert_eq!(list_traces(&p, &granted).await.unwrap().1, 1);
    }

    #[tokio::test]
    async fn 过滤与导出截断如实上报() {
        let p = pool("filters").await;
        let (u, ns) = mk_ns(&p, "alice").await;
        for (i, (id, status, ver)) in [
            ("TRC-1", TRACE_OK, "0.1.0"),
            ("TRC-2", TRACE_ERROR, "0.1.0"),
            ("TRC-3", TRACE_OK, "0.2.0"),
        ]
        .iter()
        .enumerate()
        {
            let mut d = simple_doc(id, status, "@alice/skill", ver);
            // at 错开，便于验证时间过滤与排序。
            d["at"] = Value::String(format!("2026-09-26T10:0{i}:00Z"));
            normalize_trace(&mut d);
            seed(&p, &ns, &u, d).await;
        }
        let base = TraceListOpts {
            namespace_ids: vec![ns.clone()],
            ..TraceListOpts::new()
        };
        let with = |f: Box<dyn Fn(&mut TraceListOpts)>| {
            let mut o = base.clone();
            f(&mut o);
            o
        };
        assert_eq!(
            list_traces(
                &p,
                &with(Box::new(|o| {
                    o.ref_ = "@alice/skill".into();
                    o.status = TRACE_ERROR.into();
                }))
            )
            .await
            .unwrap()
            .1,
            1
        );
        assert_eq!(
            list_traces(&p, &with(Box::new(|o| o.status = TRACE_OK.into())))
                .await
                .unwrap()
                .1,
            2
        );
        assert_eq!(
            list_traces(&p, &with(Box::new(|o| o.tag = "prod".into())))
                .await
                .unwrap()
                .1,
            3
        );
        assert_eq!(
            list_traces(
                &p,
                &with(Box::new(|o| o.model = "openai/gpt-4o-mini".into()))
            )
            .await
            .unwrap()
            .1,
            3
        );
        assert_eq!(
            list_traces(&p, &with(Box::new(|o| o.model = "gpt-4o-mini".into())))
                .await
                .unwrap()
                .1,
            3
        );
        assert_eq!(
            list_traces(
                &p,
                &with(Box::new(|o| {
                    o.since = ncc_core::timeutil::parse_time("2026-09-26T10:01:00Z");
                }))
            )
            .await
            .unwrap()
            .1,
            2
        );
        assert_eq!(
            list_traces(&p, &with(Box::new(|o| o.only_unlabeled = true)))
                .await
                .unwrap()
                .1,
            3
        );
        assert_eq!(
            list_traces(&p, &with(Box::new(|o| o.failures_only = true)))
                .await
                .unwrap()
                .1,
            1
        );
        assert_eq!(
            list_traces(&p, &with(Box::new(|o| o.q = "TRC-2".into())))
                .await
                .unwrap()
                .1,
            1
        );
        // 倒序：最新的在前（at desc）
        let (rows, _) = list_traces(&p, &base).await.unwrap();
        assert_eq!(rows[0].trace_id, "TRC-3");

        // 导出：limit 截断必须**如实报**（悄悄少给几行，训练集就少一截）。
        let (rows, truncated) = export_traces(&p, &base, 2).await.unwrap();
        assert_eq!((rows.len(), truncated), (2, true));
        assert!(
            !rows[0].doc.is_empty(),
            "导出必须带 Doc（否则数据集没有内容）"
        );
        let (rows, truncated) = export_traces(&p, &base, 10).await.unwrap();
        assert_eq!((rows.len(), truncated), (3, false));
    }

    #[tokio::test]
    async fn 标注只追加并投影回行() {
        let p = pool("labels").await;
        let (u, ns) = mk_ns(&p, "alice").await;
        let d = simple_doc("TRC-1", TRACE_OK, "@alice/skill", "0.1.0");
        let digest = d["digest"].as_str().unwrap().to_string();
        let row = seed(&p, &ns, &u, d).await;

        let add = |key: &str, value: &str, num: i64, has: bool| TraceLabelInput {
            trace_id: row.id.clone(),
            key: key.into(),
            value: value.into(),
            value_num: num,
            has_num: has,
            by: u.clone(),
            note: String::new(),
        };
        add_trace_label(&p, add("grade", "pass", 0, false))
            .await
            .unwrap();
        add_trace_label(&p, add("reward", "1", 1000, true))
            .await
            .unwrap();
        add_trace_label(&p, add("score", "850", 850, true))
            .await
            .unwrap();
        add_trace_label(&p, add("split", "eval", 0, false))
            .await
            .unwrap();

        let got = get_trace(&p, &row.id).await.unwrap().unwrap();
        assert_eq!(
            (
                got.eval_grade.as_str(),
                got.eval_reward,
                got.eval_score,
                got.eval_split.as_str()
            ),
            ("pass", 1000, 850, "eval")
        );
        assert_eq!(got.label_count, 4);
        // 文档本身**不变**（标注不改写被判断的事实）。
        assert_eq!(got.digest, digest);
        // 历史留得住（只追加）。
        assert_eq!(list_trace_labels(&p, &row.id).await.unwrap().len(), 4);
        // 标注后能按 grade/split 过滤，且不再算「未标注」。
        let base = TraceListOpts {
            namespace_ids: vec![ns.clone()],
            ..TraceListOpts::new()
        };
        let grade = TraceListOpts {
            grade: "pass".into(),
            ..base.clone()
        };
        assert_eq!(list_traces(&p, &grade).await.unwrap().1, 1);
        let split = TraceListOpts {
            split: "eval".into(),
            ..base.clone()
        };
        assert_eq!(list_traces(&p, &split).await.unwrap().1, 1);
        let labeled = TraceListOpts {
            only_labeled: true,
            ..base.clone()
        };
        assert_eq!(list_traces(&p, &labeled).await.unwrap().1, 1);
        let unlabeled = TraceListOpts {
            only_unlabeled: true,
            ..base.clone()
        };
        assert_eq!(list_traces(&p, &unlabeled).await.unwrap().1, 0);

        // 删除会把标注一起删（否则留下孤儿行）。
        delete_trace(&p, &row.id).await.unwrap();
        assert!(list_trace_labels(&p, &row.id).await.unwrap().is_empty());
        assert!(get_trace(&p, &row.id).await.unwrap().is_none());
        // traceId 也能取（幂等键就是它）
        assert!(get_trace(&p, "TRC-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 取样聚合与上限保护() {
        let p = pool("stats").await;
        let (u, ns) = mk_ns(&p, "alice").await;
        for (i, s) in [TRACE_OK, TRACE_OK, TRACE_ERROR].iter().enumerate() {
            let mut d = simple_doc(&format!("TRC-{i}"), s, "@alice/skill", "0.1.0");
            d["at"] = Value::String(format!("2026-09-26T10:0{i}:00Z"));
            if *s == TRACE_ERROR {
                d["labels"] = serde_json::json!({ "failure": "tool_timeout" });
            }
            normalize_trace(&mut d);
            seed(&p, &ns, &u, d).await;
        }
        let base = TraceListOpts {
            namespace_ids: vec![ns.clone()],
            ..TraceListOpts::new()
        };
        let (rows, truncated) = scan_traces_for_stats(&p, &base, 100).await.unwrap();
        assert_eq!((rows.len(), truncated), (3, false));
        let st = trace_stats_of(&rows);
        assert_eq!(st.total, 3);
        assert_eq!(st.by_status.get(TRACE_OK), Some(&2));
        assert_eq!(st.by_status.get(TRACE_ERROR), Some(&1));
        assert_eq!(st.by_subject_version.get("@alice/skill@0.1.0"), Some(&3));
        assert_eq!(st.by_model.get("openai/gpt-4o-mini"), Some(&3));
        assert_eq!(st.by_tag.get("prod"), Some(&3));
        assert_eq!(st.failures.get("tool_timeout"), Some(&1));
        assert_eq!(st.unlabeled, 3);
        assert_eq!(st.first_at, "2026-09-26T10:00:00Z");
        assert_eq!(st.last_at, "2026-09-26T10:02:00Z");
        // 上限保护：cap 比实际条数小 → 如实报 truncated。
        assert!(scan_traces_for_stats(&p, &base, 2).await.unwrap().1);
    }

    #[test]
    fn 分位数取最近邻() {
        let p = percentiles(&[10, 20, 30, 40, 50]);
        assert_eq!((p.min, p.p50, p.max, p.avg), (10, 30, 50, 30));
        assert_eq!(percentiles(&[]), TracePercentiles::default());
    }

    /// 聚合口径（也就是「评测怎么算」）的单测：与 Go `model/trace_test.go` 同一条。
    #[test]
    fn 聚合口径() {
        let row = |id: &str, status: &str, at: &str, run_labels: &str| TraceProjection {
            id: id.into(),
            trace_id: id.into(),
            kind: TRACE_KIND_AGENT.into(),
            status: status.into(),
            at: at.into(),
            duration_ms: 100,
            subject_ref: "@a/x".into(),
            subject_version: "0.1.0".into(),
            model_provider: "openai".into(),
            model_name: "gpt-4o-mini".into(),
            step_count: 2,
            payload_level: TRACE_PAYLOAD_PREVIEW.into(),
            tags: r#"["prod"]"#.into(),
            run_labels: run_labels.into(),
            agent_name: "harness-use".into(),
            input_tokens: 10,
            output_tokens: 5,
            cost_usd_micros: 1000,
            eval_grade: String::new(),
            eval_reward: 0,
            eval_score: 0,
            eval_split: String::new(),
            label_count: 0,
        };
        let mut a = row("TR-1", TRACE_OK, "2026-09-26 10:00:00+00:00", "{}");
        a.eval_grade = "pass".into();
        a.eval_score = 900;
        a.label_count = 1;
        a.eval_split = "eval".into();
        let mut b = row(
            "TR-2",
            TRACE_ERROR,
            "2026-09-26 10:01:00+00:00",
            r#"{"failure":"tool_timeout"}"#,
        );
        b.duration_ms = 300;
        let mut c = row("TR-3", TRACE_OK, "2026-09-26 10:02:00+00:00", "");
        c.duration_ms = 200;
        c.subject_version = "0.2.0".into();
        c.payload_level = TRACE_PAYLOAD_FULL.into();

        let st = trace_stats_of(&[a, b, c]);
        assert_eq!(st.total, 3);
        assert_eq!(st.labeled, 1);
        assert_eq!(st.unlabeled, 2);
        // 平均分只对打过分的算：拿未标注的稀释分母是不诚实的平均数。
        assert_eq!(st.score_avg_milli, 900);
        assert_eq!(st.grades.get("pass"), Some(&1));
        assert_eq!(st.failures.get("tool_timeout"), Some(&1));
        assert_eq!(st.by_subject_version.get("@a/x@0.1.0"), Some(&2));
        assert_eq!(st.by_subject_version.get("@a/x@0.2.0"), Some(&1));
        assert_eq!(
            (st.duration_ms.min, st.duration_ms.p50, st.duration_ms.max),
            (100, 200, 300)
        );
        assert_eq!((st.input_tokens, st.cost_usd_micros), (30, 3000));
        assert_eq!(st.first_at, "2026-09-26T10:00:00Z");
        assert_eq!(st.last_at, "2026-09-26T10:02:00Z");
    }

    #[tokio::test]
    async fn 取用即声明内置集合() {
        let p = pool("builtin").await;
        let (_u, ns) = mk_ns(&p, "alice").await;
        mark_builtin_used(&p, &ns, "trace").await.unwrap();
        mark_builtin_used(&p, &ns, "trace").await.unwrap(); // 幂等
        let (kind, append_only, status): (String, i64, String) = sqlx::query_as(
            "SELECT kind, append_only, status FROM collections WHERE namespace_id = ?",
        )
        .bind(&ns)
        .fetch_one(&p)
        .await
        .unwrap();
        assert_eq!(
            (kind.as_str(), append_only, status.as_str()),
            ("trace", 1, "active")
        );
        let fields: String =
            sqlx::query_scalar("SELECT fields FROM collections WHERE namespace_id = ?")
                .bind(&ns)
                .fetch_one(&p)
                .await
                .unwrap();
        assert!(fields.contains("kind:enum:hur-run|agent"), "{fields}");
    }
}
