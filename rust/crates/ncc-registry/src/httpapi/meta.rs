//! `GET /api/meta` —— 本节点自述（CLI `ncc registry status` 的第一跳）。
//!
//! 与 Go 版 `httpapi/server.go` 的 `func (s *Server) meta` **逐字段一致**：
//! CLI 靠 `capabilities` 决定哪些命令面可用（`ncc-cli/cli/src/capability.rs` 读这个数组），
//! 运维靠 `counts` / `storage` / `auth` 看这台节点托管了什么、字节落在哪、门开没开。
//!
//! 唯一与 Go 的形态差别是 `node.lastSeen`：Go 用本地时区的 `time.Now()`，
//! 这里用 UTC RFC3339 秒精度（`now_rfc3339()`），与本仓其它族保持一致 ——
//! 字段名与类型不变，只是时区/精度归一。

use axum::extract::State;
use axum::response::Response;
use serde_json::{json, Value};

use ncc_core::error::ApiResult;
use ncc_core::timeutil::now_rfc3339;

use crate::httpapi::{helpers, AppState};
use crate::store;

/// 接口面能力声明（**不是**节点「提供能力」，两者是不同的轴 —— 见 Go `model/capabilities.go`）。
///
/// 顺序与内容照抄 Go 的 `meta`：CLI 只判「在不在」，但冒烟脚本会逐项核对，
/// 所以逐字保留（含注释里说明的取舍）。
const CAPABILITIES: &[&str] = &[
    "registry", "config", "share", "nodes", "grants", "access", "cluster", "admin", "p2p", "trace",
    "kb", "mem", "ckpt",
    // 通用记录仓：集合是声明、记录是数据。**一个能力对全部集合生效** ——
    // 能力回答「这台节点支不支持这类功能」，不是「有哪些集合」。
    "store",
    // 索引与匹配：接收平台推来的索引副本，在内网本地做匹配。
    "index",
    // 远程执行：**具体能跑哪些引擎看 `/api/exec/kinds`** —— 能力面只说「有执行服务」。
    "exec",
    // 连接通道（`ncc conn`）：声明的是「有这道门」，放不放行由运维定（NCCR_CONN_ALLOW）。
    "conn", // 反馈：读写分开授权，可见性默认私有。
    "feedback",
];

/// 给人读的一句话能力说明（`capabilities` 是声明，这里只是文案）。
const FEATURES: &[&str] = &[
    "registry: artifact hosting & distribution",
    "config: team/infra config hosting (versioned, encrypted secrets, grant-scoped)",
    "share: expiring artifact links (no login for the receiver)",
    "admin: node governance (users / nodes / services) with audit log",
    "nodes: hosted agent/service discovery & linking",
    "grants: explicit access grants (connect != authorize)",
    "access: join by key/secret or one-click intranet link",
    "cluster: master/worker multi-node, routing, replicate & revoke",
    "p2p: NAT profile + real hole-punch check between nodes (no business bytes relayed)",
    "trace: run traces of agents / HUR packages (capability evaluation + post-training datasets; private, opt-in, digest-first)",
    "kb: hosted knowledge bases (namespace-scoped corpora with revision history, pullable by agents)",
    "mem: hosted agent memory (key/value, TTL, source-traceable; no public tier by design)",
    "ckpt: hosted checkpoints (immutable bytes + lineage, signed short-lived download URLs)",
];

/// GET /api/meta —— 本节点自述。
///
/// 直接返回 Go 的 `out` 对象（Go 的 `ok()` 不做信封包装），字段一个不多一个不少。
pub async fn meta(State(state): State<AppState>) -> ApiResult<Response> {
    let cfg = state.cfg();

    // 规模指标：单项查询失败就报 0，与 Go 忽略单项错误（`configs, _ := …`）的行为一致 ——
    // 一个计数查不动不该让整份自述 500。
    let (artifacts, nodes, users) = helpers::counts(&state).await;
    let configs = store::configs::count(state.pool()).await.unwrap_or(0);
    let public_configs = store::configs::count_public(state.pool())
        .await
        .unwrap_or(0);
    let shares = store::shares::count_active(state.pool()).await.unwrap_or(0);
    let admins = store::admin::count_admins(state.pool()).await.unwrap_or(0);
    let node_kinds = store::admin::node_kind_counts(state.pool())
        .await
        .unwrap_or_default();
    // 服务数：节点侧 kind=service + 制品侧 kind=api，与 Go `CountServiceArtifacts("", "")` 同口径。
    let services = store::admin::count_service_artifacts(state.pool(), "", "")
        .await
        .unwrap_or(0);
    let traces = store::traces::count(state.pool()).await.unwrap_or(0);
    // 三样状态：知识库文档数 / 记忆条目数 / 检查点数。
    let (kb_docs, mem_entries, checkpoints) = store::stack::state_counts(state.pool())
        .await
        .unwrap_or((0, 0, 0));
    // 通用记录仓：集合数 + 记录数。
    let (collections, records) = store::state::store_counts(state.pool())
        .await
        .unwrap_or((0, 0));
    // 索引副本：平台推来多少条（内网本地检索能查到的量）。
    let index_entries = store::index::count_index(state.pool()).await.unwrap_or(0);
    let has_admin_key = store::admin::has_active_admin_key(state.pool())
        .await
        .unwrap_or(false);

    let mut out = json!({
        "product": "ncc-registry",
        "kind": "node",
        "about": "内网托管节点 · 制品托管 · 配置托管 · 分享 · Agent 发现与互联",
        "node": self_node_json(&state, artifacts, nodes, users),
        "capabilities": CAPABILITIES,
        "counts": {
            "artifacts": artifacts, "hostedNodes": nodes, "users": users,
            "configs": configs, "publicConfigs": public_configs,
            // 治理面：管理员数、服务数、有效分享数。
            "admins": admins, "services": services,
            "serviceNodes": node_kinds.get(store::nodes::NODE_SERVICE).copied().unwrap_or(0),
            "shares": shares,
            // 轨迹：既是「这台节点收了多少行为数据」，也是评测数据集的体量。
            "traces": traces,
            "kbDocs": kb_docs, "memEntries": mem_entries, "checkpoints": checkpoints,
            // 通用记录仓与索引副本。
            "collections": collections, "records": records,
            "indexEntries": index_entries,
        },
        // 存储目录：部署时最常被问的就是「字节到底落在哪」，直接报出来。
        "storage": {
            "driver": "local",
            "dataDir": cfg.data_dir.to_string_lossy(),
            "blobDir": cfg.blob_dir.to_string_lossy(),
            "dbPath": cfg.db_path.to_string_lossy(),
            "blobsBase": format!("{}/blobs/", cfg.public_url),
        },
        "features": FEATURES,
        "console": format!("{}/", cfg.public_url),
        "auth": {
            "inviteRequired": cfg.invite_required(),
            "clusterToken": !cfg.cluster_token.is_empty(),
            "adminKey": has_admin_key,
        },
    });

    // worker 视角：回显它要连的 master 地址（master 视角没有这个键）。
    if cfg.role == crate::config::ROLE_WORKER {
        out["masterUrl"] = json!(cfg.master_url);
    }

    Ok(helpers::ok_json(out))
}

/// 本节点的身份 —— 照 Go 的 `selfNodeJSON` 逐字段实现。
fn self_node_json(state: &AppState, artifacts: i64, nodes: i64, users: i64) -> Value {
    let cfg = state.cfg();

    // 「提供能力」的自证部分来自远程执行探针：按**本机事实**（runner 在不在、运维放行了什么）
    // 报「真的能跑」，而不是「标签上写着能跑」。
    let cap = crate::httpapi::exec::probe(cfg);
    let mut offers: Vec<String> = Vec::new();
    let mut verified: Vec<String> = Vec::new();
    let mut exec_on = false;
    for k in &cap.kinds {
        if k.enabled {
            exec_on = true;
            verified.push(format!("run:{}", k.id));
        }
    }
    if exec_on {
        offers.push("run:remote".to_string()); // 本机接活
    }

    json!({
        "id": cfg.node_id, "name": cfg.node_name, "url": cfg.public_url,
        "role": cfg.role, "version": ncc_core::REGISTRY_VERSION, "region": cfg.node_region,
        "artifacts": artifacts, "nodes": nodes, "users": users,
        "online": true, "lastSeen": now_rfc3339(),
        // 提供能力：声明 + 自证（自证 = 由本机硬事实推导）。
        "capabilities": normalize_offers(&offers),
        "capabilitiesVerified": normalize_offers(&verified),
        "tags": cap.tags,
    })
}

/// 归一 + 去重（保持原顺序）。
///
/// Go 的 `model.NormalizeOffers` 还会查历史别名表；这里的入参全部由本机引擎 id
/// （wasm / process / container）拼出来，都是规范 id，别名表用不上 ——
/// 只保留「去空格、转小写、去重」这一步，空表仍序列化成 `[]`（与 Go 一致）。
fn normalize_offers(in_: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(in_.len());
    for raw in in_ {
        let v = raw.trim().to_lowercase();
        if v.is_empty() || out.contains(&v) {
            continue;
        }
        out.push(v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncc_core::storage::LocalStorage;
    use std::sync::Arc;

    fn test_dir(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-blobs")
            .join(format!("meta-{}-{name}", std::process::id()))
    }

    /// 每个测试一个临时**文件**库（不要用 `sqlite::memory:` + 连接池）。
    async fn state(name: &str) -> AppState {
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
        cfg.public_url = "http://10.0.0.9:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();
        cfg.node_id = "ND-TEST".to_string();
        cfg.node_name = "测试节点".to_string();
        cfg.node_region = "office".to_string();
        // 关掉全部执行引擎的放行：让「提供能力」的自证部分可预测（否则会随本机 PATH
        // 上有没有 `ncc` / shell / 容器运行时变化）。
        cfg.exec_allow = Vec::new();
        cfg.blob_dir = dir.join("blobs");
        let blobs = LocalStorage::new(&cfg.blob_dir, &cfg.public_url, "blobs").unwrap();
        let seal = ncc_core::secretbox::SecretBox::new(&cfg.jwt_secret).unwrap();
        AppState {
            cfg: Arc::new(cfg),
            pool,
            blobs: Arc::new(blobs),
            seal: Arc::new(seal),
        }
    }

    /// 直接调 handler（meta 尚未接进 `router.rs`，由父任务统一接线），取回 JSON。
    async fn call_meta(state: &AppState) -> (u16, Value) {
        let resp = meta(State(state.clone())).await.unwrap();
        let code = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (code, serde_json::from_slice(&bytes).unwrap())
    }

    /// 取一个字符串数组字段（同时断言它确实是数组）。
    fn arr(v: &Value, key: &str) -> Vec<String> {
        v[key]
            .as_array()
            .unwrap_or_else(|| panic!("{key} 应为数组"))
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect()
    }

    /// 空库下：形状、关键键、capabilities 与计数都要与 Go 一致。
    #[tokio::test]
    async fn 空库下形状与计数与_go_一致() {
        let st = state("shape").await;
        let (code, v) = call_meta(&st).await;

        assert_eq!(code, 200);
        // 顶层字段（Go meta 的全集）。
        assert_eq!(v["product"], json!("ncc-registry"));
        assert_eq!(v["kind"], json!("node"));
        assert_eq!(
            v["about"],
            json!("内网托管节点 · 制品托管 · 配置托管 · 分享 · Agent 发现与互联")
        );
        assert_eq!(v["console"], json!("http://10.0.0.9:8282/"));

        // capabilities：逐项与 Go 完全一致（顺序也一致）。
        let caps = arr(&v, "capabilities");
        assert_eq!(
            caps,
            vec![
                "registry", "config", "share", "nodes", "grants", "access", "cluster", "admin",
                "p2p", "trace", "kb", "mem", "ckpt", "store", "index", "exec", "conn", "feedback",
            ]
        );

        // features：13 条，且 key 前缀与 capabilities 对得上（文案与 Go 逐字一致）。
        let feats = arr(&v, "features");
        assert_eq!(feats.len(), 13);
        assert_eq!(feats[0], "registry: artifact hosting & distribution");
        assert_eq!(
            feats[12],
            "ckpt: hosted checkpoints (immutable bytes + lineage, signed short-lived download URLs)"
        );

        // counts：全集键都在，空库下全为 0。
        let counts = &v["counts"];
        let keys = [
            "artifacts",
            "hostedNodes",
            "users",
            "configs",
            "publicConfigs",
            "admins",
            "services",
            "serviceNodes",
            "shares",
            "traces",
            "kbDocs",
            "memEntries",
            "checkpoints",
            "collections",
            "records",
            "indexEntries",
        ];
        assert_eq!(counts.as_object().unwrap().len(), keys.len());
        for k in keys {
            assert_eq!(counts[k], json!(0), "counts.{k} 应为 0");
        }

        // node 子对象（照 Go 的 selfNodeJSON）。
        let node = &v["node"];
        assert_eq!(node["id"], json!("ND-TEST"));
        assert_eq!(node["name"], json!("测试节点"));
        assert_eq!(node["url"], json!("http://10.0.0.9:8282"));
        assert_eq!(node["role"], json!("master"));
        assert_eq!(node["version"], json!(ncc_core::REGISTRY_VERSION));
        assert_eq!(node["region"], json!("office"));
        assert_eq!(node["artifacts"], json!(0));
        assert_eq!(node["nodes"], json!(0));
        assert_eq!(node["users"], json!(0));
        assert_eq!(node["online"], json!(true));
        assert!(node["lastSeen"].is_string());
        // 没放行任何引擎时：声明与自证都是空数组（序列化成 [] 而不是 null）。
        assert_eq!(node["capabilities"], json!([]));
        assert_eq!(node["capabilitiesVerified"], json!([]));
        assert!(node["tags"].is_array());

        // storage：字节落点照报，blobsBase / console 都基于 publicUrl。
        let storage = &v["storage"];
        assert_eq!(storage["driver"], json!("local"));
        assert_eq!(storage["blobsBase"], json!("http://10.0.0.9:8282/blobs/"));
        assert!(storage["dataDir"].is_string());
        assert!(storage["blobDir"].is_string());
        assert!(storage["dbPath"].is_string());

        // auth：三键俱全；空库无机器凭据。
        assert_eq!(v["auth"]["inviteRequired"], json!(false));
        assert_eq!(v["auth"]["clusterToken"], json!(false));
        assert_eq!(v["auth"]["adminKey"], json!(false));

        // master 视角没有 masterUrl 键。
        assert!(v.get("masterUrl").is_none());
    }

    /// counts 会随真实数据变化（不是写死的 0）。
    #[tokio::test]
    async fn 计数随数据变化() {
        let st = state("counts").await;

        let u = store::users::create(st.pool(), "管理员", "a@x.com", "h")
            .await
            .unwrap();
        store::namespaces::create_account(st.pool(), &u.id, &u.name, "admin")
            .await
            .unwrap();
        store::admin::create_admin_key(st.pool(), "机器凭据", &u.id)
            .await
            .unwrap();

        let (_code, v) = call_meta(&st).await;
        assert_eq!(v["counts"]["users"], json!(1));
        // 首个注册用户自动是管理员（`store::users::create`），所以 admins 也是 1。
        assert_eq!(v["counts"]["admins"], json!(1));
        assert_eq!(v["auth"]["adminKey"], json!(true));
    }

    /// `normalize_offers` 与 Go 的 `model.NormalizeOffers` 同行为：去空格、转小写、去重、保序。
    #[test]
    fn 归一能力声明_去空格小写去重() {
        let got = normalize_offers(&[
            " run:Wasm ".to_string(),
            "run:wasm".to_string(),
            "".to_string(),
            "RUN:REMOTE".to_string(),
        ]);
        assert_eq!(got, vec!["run:wasm", "run:remote"]);
        assert!(normalize_offers(&[]).is_empty());
    }

    /// worker 角色多出 `masterUrl`（与 Go 的 `if Role == worker` 分支一致）。
    #[tokio::test]
    async fn worker_角色多出_master_地址() {
        let mut st = state("worker").await;
        {
            let mut cfg = (*st.cfg).clone();
            cfg.role = crate::config::ROLE_WORKER.to_string();
            cfg.master_url = "http://10.0.0.1:8282".to_string();
            st.cfg = Arc::new(cfg);
        }
        let (_code, v) = call_meta(&st).await;
        assert_eq!(v["node"]["role"], json!("worker"));
        assert_eq!(v["masterUrl"], json!("http://10.0.0.1:8282"));
    }

    /// 路由无关的健全性检查：响应是裸 JSON 对象（Go 的 `ok` 不加信封）。
    #[tokio::test]
    async fn 响应是裸_json_对象() {
        let st = state("raw").await;
        let resp = meta(State(st)).await.unwrap();
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
    }
}
