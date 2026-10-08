//! 通信基础设施：**连接（Connection）** —— 一条到 Cloud instance 的会话，通道上能
//! 反复执行命令、推/拉文件，且全程留在账本里。
//!
//! 原实现：`ncc-registry/httpapi/conn.go` + `store/conn.go` + `model/conn.go`。
//!
//! 与 `/api/exec/*`（任务）的关系：任务是一次动作；连接是一段会话。`conn exec` **复用**
//! `httpapi::exec` 的执行器（同一套限额 / 环境变量白名单 / 进程组回收），只是多了一层
//! **会话与文件面**。所以这里不重复实现执行器。
//!
//! 红线（`prd/ncc-conn.md`）：
//!
//! 1. **默认关**：通道能跑任意命令、写文件 —— `NCCR_CONN_ALLOW` 必须运维显式打开。
//! 2. **文件锁在工作目录**：`safe_join` 是本族唯一一道门 —— 只收相对路径（绝对路径
//!    直接拒），`..` 会被归一掉（`path.Clean` 语义，结果一定还在工作目录内）；
//!    另外 Rust 版**多拦一层软链接逃逸**（Go 没查这一层，见 `safe_join` 的注释）。
//! 3. **每个动作要 reason**（与 R12 同规矩）。
//! 4. **通道有 TTL**：过期即失效，且**过期不写状态**（`ConnRow::state_at` 推导），
//!    这样「过期」与「被人关掉」在接口上分得开（410 的文案不同）。
//! 5. 通道走**用户自己的线路**：目标机器是客户自己的，云端托管面不在数据路径上。

use std::path::{Path as StdPath, PathBuf};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::ids::new_id;
use ncc_core::web;

use crate::httpapi::exec::{self, Plan};
use crate::httpapi::{helpers, shares, AppState, Auth};
use crate::store;

/// 通道默认有效期（`NCCR_CONN_TTL` 没配或配了非正数时用它）。
const CONN_DEFAULT_TTL: i64 = 3600;
/// 通道最长有效期（8 小时）——再长就不叫「一段会话」了。
const CONN_MAX_TTL: i64 = 8 * 3600;
/// 单文件上限（与 exec 的包上限同档）。
const CONN_MAX_FILE: usize = 32 << 20;

/// 该族路由（相对 `/api`）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/conn/connections", get(list_conns).post(open_conn))
        .route("/conn/connections/", get(list_conns).post(open_conn))
        .route("/conn/connections/{id}", get(get_conn).delete(close_conn))
        .route("/conn/connections/{id}/exec", post(conn_exec))
        .route(
            "/conn/connections/{id}/files",
            post(conn_put_file).get(conn_get_file),
        )
}

/* ---------------- 建 / 列 / 看 / 关 ---------------- */

#[derive(Debug, Default, Deserialize)]
struct ConnOpenReq {
    #[serde(default)]
    name: String,
    #[serde(default)]
    note: String,
    #[serde(default, rename = "ttlSec")]
    ttl_sec: i64,
}

/// 「这台机器没开通道」的两种文案：建通道时说得详细些（要告诉运维怎么开），
/// 用通道时简短些（用户已经知道自己在干什么了）。
fn conn_not_allowed(verbose: bool) -> ApiError {
    if verbose {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "conn_not_allowed",
            "这台机器没有开放连接通道（通道能跑任意命令、写文件，属最高权限）—— 运维要显式打开：NCCR_CONN_ALLOW=1",
        )
    } else {
        ApiError::new(
            StatusCode::FORBIDDEN,
            "conn_not_allowed",
            "这台机器没有开放连接通道（NCCR_CONN_ALLOW=1 才开）",
        )
    }
}

/// POST /api/conn/connections —— 建一条通道。
///
/// 通道 = 一个**工作目录** + 一段有效期 + 一条审计线索。真正的执行在 `…/exec`，
/// 文件在 `…/files`。
async fn open_conn(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    if !state.cfg().conn_allow {
        return Err(conn_not_allowed(true));
    }
    let a = auth
        .info()
        .ok_or_else(|| {
            ApiError::unauthorized("未认证或凭据无效（先 ncc login / ncc conn open --key …）")
        })?
        .clone();

    // 空 body 允许（建一条不带名字的通道），有 body 就必须是合法 JSON。
    let mut req = ConnOpenReq::default();
    if !body.is_empty() {
        req = serde_json::from_slice(&body)
            .map_err(|_| ApiError::bad_request("bad_request", "请求体格式错误"))?;
    }

    let mut ttl = state.cfg().conn_ttl.as_secs() as i64;
    if ttl <= 0 {
        ttl = CONN_DEFAULT_TTL;
    }
    if req.ttl_sec > 0 {
        if req.ttl_sec > CONN_MAX_TTL {
            return Err(ApiError::bad_request(
                "bad_request",
                format!("ttlSec 不能超过 {CONN_MAX_TTL} 秒"),
            ));
        }
        ttl = req.ttl_sec;
    }

    let id = new_id("CN");
    let work = state.cfg().conn_dir.join(&id);
    if let Err(e) = exec::mkdir_mode(&work.join(".tmp"), 0o700) {
        tracing::error!("创建工作目录失败 {}: {e}", work.display());
        return Err(ApiError::internal("创建工作目录失败"));
    }
    let peer = format!("{}/{}", state.cfg().node_name, state.cfg().node_id);
    let expires = ncc_core::timeutil::format_go(
        chrono::Local::now().fixed_offset() + chrono::Duration::seconds(ttl),
    );
    let rec = match store::conn::create(
        state.pool(),
        store::conn::ConnInput {
            id: id.clone(),
            owner_id: a.user_id.clone(),
            requested_by: a.email.clone(),
            name: exec::clamp_runes(&req.name, 60),
            note: exec::clamp_runes(&req.note, 400),
            work_dir: work.to_string_lossy().to_string(),
            peer: peer.clone(),
            ttl_sec: ttl,
            expires_at: Some(expires),
        },
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("建立连接失败: {e}");
            let _ = std::fs::remove_dir_all(&work);
            return Err(ApiError::internal("建立连接失败"));
        }
    };

    let actor = shares::ensure_admin(&state, &auth, &headers).await;
    shares::audit(
        &state,
        actor.as_ref(),
        "conn.open",
        &rec.id,
        &rec.name,
        "建立连接通道",
        json!({"ttlSec": rec.ttl_sec, "peer": peer}),
        &web::client_ip(&headers),
    )
    .await;

    let base = format!("/api/conn/connections/{}", rec.id);
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({
            "connection": conn_json(&rec, chrono::Local::now().fixed_offset()),
            "howto": {
                "exec": format!("POST {base}/exec  {{\"cmd\":\"…\",\"reason\":\"…\"}}"),
                "push": format!("POST {base}/files?path=<相对路径>&reason=…  (body = 文件字节)"),
                "pull": format!("GET  {base}/files?path=<相对路径>"),
                "close": format!("DELETE {base}（?purge=1 连工作目录删）"),
                "note": "文件路径只能是相对的（锁在这条通道的工作目录里）；每个动作都要 reason。",
            },
        }),
    ))
}

/// GET /api/conn/connections —— 我的通道（管理员 `?all=1`）。
async fn list_conns(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let mut owner = auth.user_id().unwrap_or_default();
    if web::query(&uri, "all").as_deref() == Some("1") {
        if shares::ensure_admin(&state, &auth, &headers)
            .await
            .is_none()
        {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "admin_required",
                "查看全部通道需要节点管理员身份",
            ));
        }
        owner = String::new();
    } else if owner.is_empty() {
        return Err(ApiError::unauthorized("未认证或凭据无效"));
    }
    // 不用先「收尾」：过期是从 ExpiresAt 推导出来的（`ConnRow::state_at`），
    // 列表里会直接显示 expired，不必（也不该）把状态改写成 closed —— 那会丢掉区别。
    let (limit, offset) = shares::page_params(&uri);
    let rows = store::conn::list(state.pool(), &owner, limit, offset)
        .await
        .map_err(ApiError::from_db)?;
    let now = chrono::Local::now().fixed_offset();
    let list: Vec<Value> = rows.iter().map(|r| conn_json(r, now)).collect();
    Ok(helpers::ok_json(json!({
        "connections": list, "total": list.len(), "limit": limit, "offset": offset,
    })))
}

/// GET /api/conn/connections/{id} —— 状态 + 这条通道上跑过什么（任务列表尾巴）。
///
/// **关掉/过期的也照样能看**：通道是账本，`close` 只是把「能动手」这条关掉，
/// 不该把「这条通道上跑过什么」一起藏起来（归档 ≠ 删除，同一个道理）。
/// 所以这里**不复用 `conn_for`**（那个守卫会 410）—— 只做归属判断，状态原样回报。
async fn get_conn(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let rec = store::conn::find(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("连接不存在"))?;
    if !state.cfg().conn_allow {
        return Err(conn_not_allowed(false));
    }
    let admin = shares::ensure_admin(&state, &auth, &headers)
        .await
        .is_some();
    let mine = auth
        .info()
        .map(|a| a.user_id == rec.owner_id)
        .unwrap_or(false);
    if !admin && !mine {
        return Err(ApiError::forbidden(
            "只能看自己建立的连接（管理员可看全部）",
        ));
    }

    let runs = store::exec::list(state.pool(), &rec.owner_id, 20, 0)
        .await
        .unwrap_or_default();
    let on_conn: Vec<Value> = runs
        .iter()
        .filter(|r| r.conn_id == rec.id)
        .map(exec::exec_json)
        .collect();
    let now = chrono::Local::now().fixed_offset();
    let mut out = conn_json(&rec, now);
    out["execs"] = json!(on_conn);
    // 不可用了就把原因一并给出（客户端不用自己猜 state 到 410 的映射）
    if !rec.open_at(now) {
        out["blocked"] = json!(rec.state_at(now));
    }
    Ok(helpers::ok_json(json!({"connection": out})))
}

/// DELETE /api/conn/connections/{id} —— 关闭（`?purge=1` 连工作目录一起删）。
async fn close_conn(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    let rec = store::conn::find(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("连接不存在"))?;
    let admin = shares::ensure_admin(&state, &auth, &headers)
        .await
        .is_some();
    let mine = auth
        .info()
        .map(|a| a.user_id == rec.owner_id)
        .unwrap_or(false);
    if !admin && !mine {
        return Err(ApiError::forbidden("只能关闭自己建立的连接"));
    }
    let was_open = store::conn::close(state.pool(), &rec.id, "")
        .await
        .map_err(ApiError::from_db)?;

    let mut purged = false;
    if web::query(&uri, "purge").as_deref() == Some("1") {
        // 删工作目录前先确认它确实在这条连的目录里（别让一条脏数据删到别处）。
        if StdPath::new(&rec.work_dir).parent() == Some(state.cfg().conn_dir.as_path()) {
            let _ = std::fs::remove_dir_all(&rec.work_dir);
            purged = true;
        }
    }
    let actor = shares::ensure_admin(&state, &auth, &headers).await;
    shares::audit(
        &state,
        actor.as_ref(),
        "conn.close",
        &rec.id,
        &rec.name,
        "关闭连接通道",
        json!({"purge": purged}),
        &web::client_ip(&headers),
    )
    .await;
    Ok(helpers::ok_json(json!({
        "ok": true, "id": rec.id, "wasOpen": was_open, "purged": purged,
    })))
}

/* ---------------- 通道上：执行 ---------------- */

#[derive(Debug, Default, Deserialize)]
struct ConnExecReq {
    #[serde(default)]
    cmd: String,
    #[serde(default)]
    engine: String,
    #[serde(default)]
    reason: String,
    #[serde(default, rename = "timeoutSec")]
    timeout: i64,
    /// 默认 true：等它跑完（通道上多半是要看结果）。
    #[serde(default)]
    wait: Option<bool>,
    /// 相对工作目录的子目录（可选）。
    #[serde(default, rename = "cwd")]
    working_dir: String,
}

/// POST /api/conn/connections/{id}/exec —— 在这条通道上跑一条命令。
///
/// **复用 execrun**：限额、环境变量白名单、进程组回收都与 `/api/exec/runs` 一致；
/// 区别只是「工作目录挂在通道上」+ 记录带上 `connId`。
async fn conn_exec(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let rec = conn_for(&state, &auth, &headers, &id).await?;

    let req: ConnExecReq = serde_json::from_slice(&body).map_err(|_| {
        ApiError::bad_request("bad_request", "请求体格式错误（需要 cmd 与 reason）")
    })?;
    if req.cmd.trim().is_empty() {
        return Err(ApiError::bad_request("bad_request", "缺少 cmd"));
    }
    if req.reason.trim().is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺少 reason：这条通道上干了什么、为什么，要留在账本里",
        ));
    }
    let engine = exec::first_non_empty(&[req.engine.clone(), "process".to_string()]);
    if engine != "process" && engine != "container" {
        // 通道上不给 wasm：通道的语义是"在目标机上干活"
        // （wasm 请走 /api/exec/runs 或 `ncc sandbox run --package`）
        return Err(ApiError::bad_request(
            "bad_request",
            "通道上只支持 process / container 引擎（wasm 请走 ncc sandbox run --package）",
        ));
    }
    let cap = exec::probe(state.cfg());
    if !cap.is_allowed(&engine) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "engine_not_allowed",
            format!("这台机器没有放行 {engine} 引擎（NCCR_EXEC_ALLOW 加上它）"),
        ));
    }
    if !cap.is_enabled(&engine) {
        return Err(ApiError::conflict(
            "engine_unavailable",
            format!("{engine} 引擎当前不可用：{}", cap.why_of(&engine)),
        ));
    }

    let mut timeout = state.cfg().exec_timeout;
    if req.timeout > 0 {
        let want = std::time::Duration::from_secs(req.timeout as u64);
        if want > state.cfg().exec_timeout {
            return Err(ApiError::bad_request(
                "bad_request",
                format!(
                    "timeoutSec 不能超过节点上限（{} 秒）",
                    state.cfg().exec_timeout.as_secs()
                ),
            ));
        }
        timeout = want;
    }

    // cwd：只能是工作目录里的子目录（同样防越界）。
    let mut work = PathBuf::from(&rec.work_dir);
    if !req.working_dir.trim().is_empty() {
        let p = safe_join(&rec.work_dir, &req.working_dir)
            .map_err(|e| ApiError::bad_request("bad_request", e))?;
        if let Err(e) = exec::mkdir_mode(&p, 0o700) {
            tracing::error!("创建子目录失败 {}: {e}", p.display());
            return Err(ApiError::internal("创建子目录失败"));
        }
        work = p;
    }

    let run_id = new_id("ER");
    let task_dir = work.join(".ncc-exec").join(&run_id);
    if let Err(e) = exec::mkdir_mode(&task_dir.join(".tmp"), 0o700) {
        tracing::error!("创建任务目录失败 {}: {e}", task_dir.display());
        return Err(ApiError::internal("创建任务目录失败"));
    }
    let plan = Plan {
        engine: engine.clone(),
        kind: "cmd".to_string(),
        work_dir: work.clone(),
        command: req.cmd.clone(),
        image: String::new(),
        package_dir: PathBuf::new(),
    };
    let log_path = task_dir.join("output.log");
    // owner 用**通道的归属者**（不是当前凭据的持有人）：通道上的动作算在通道主人头上，
    // 这样管理员凭别人的 key 代跑也不会把任务记到 key 的主人名下。
    let (owner, req_by) = match auth.info() {
        Some(a) => (a.user_id.clone(), a.email.clone()),
        None => (rec.owner_id.clone(), String::new()),
    };
    let run = store::exec::create(
        state.pool(),
        store::exec::ExecRunInput {
            id: run_id,
            owner_id: owner,
            requested_by: req_by,
            conn_id: rec.id.clone(),
            engine: engine.clone(),
            kind: "cmd".to_string(),
            spec: exec::clamp_runes(&req.cmd, 400),
            work_dir: work.to_string_lossy().to_string(),
            log_path: log_path.to_string_lossy().to_string(),
            reason: exec::clamp_runes(&req.reason, exec::MAX_EXEC_REASON),
            timeout_sec: timeout.as_secs() as i64,
            ..Default::default()
        },
    )
    .await
    .map_err(|e| {
        tracing::error!("创建任务失败: {e}");
        ApiError::internal("创建任务失败")
    })?;
    let _ = store::conn::touch(state.pool(), &rec.id, 1, 0, 0, 0).await;

    let actor = shares::ensure_admin(&state, &auth, &headers).await;
    shares::audit(
        &state,
        actor.as_ref(),
        "conn.exec",
        &rec.id,
        &rec.name,
        "通道上执行命令",
        json!({"engine": engine, "task": run.id, "reason": run.reason}),
        &web::client_ip(&headers),
    )
    .await;

    let wait = req.wait.unwrap_or(true);
    if !wait {
        let _ = store::exec::mark_running(state.pool(), &run.id).await;
        let bg_state = state.clone();
        let bg_rec = run.clone();
        tokio::spawn(async move {
            exec::exec_one(bg_state, bg_rec, plan, log_path).await;
        });
        return Ok(helpers::ok_status(
            StatusCode::ACCEPTED,
            json!({
                "exec": exec::exec_json(&run),
                "note": "已提交，用 GET /api/exec/runs/:id 看结果",
            }),
        ));
    }
    // 等它跑完：通道的用法多半是"跑一条、看结果、再跑下一条"，
    // 让调用方自己轮询会把简单的脚本推给每一个客户端重写一遍。
    exec::exec_one(state.clone(), run.clone(), plan, log_path).await;
    let done = store::exec::find(state.pool(), &run.id)
        .await
        .ok()
        .flatten()
        .unwrap_or(run);
    // 同步返回**带上日志尾巴**（与 GET /api/exec/runs/{id} 同一份实现）：
    // 通道上一半的用法是「跑条命令看它说了什么」。
    Ok(helpers::ok_json(
        json!({"exec": exec::exec_json_tail(&done)}),
    ))
}

/* ---------------- 通道上：文件 ---------------- */

/// POST /api/conn/connections/{id}/files?path=<相对>&reason=… —— 推文件。
///
/// body = 原样字节。路径锁在这条通道的工作目录内（`safe_join`），
/// 目标目录不存在就建；`?sha256=` 可带上让服务端核对（不一致就不落盘）。
async fn conn_put_file(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> ApiResult<Response> {
    let rec = conn_for(&state, &auth, &headers, &id).await?;

    let rel = web::query(&uri, "path").unwrap_or_default();
    if rel.trim().is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺少 path（相对工作目录的路径，如 app/deploy.sh）",
        ));
    }
    if web::query(&uri, "reason")
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺少 reason：往目标机写了什么、为什么，要留在账本里",
        ));
    }
    let dst =
        safe_join(&rec.work_dir, &rel).map_err(|e| ApiError::bad_request("bad_request", e))?;
    if let Some(parent) = dst.parent() {
        if let Err(e) = exec::mkdir_mode(parent, 0o700) {
            tracing::error!("创建目录失败 {}: {e}", parent.display());
            return Err(ApiError::internal("创建目录失败"));
        }
    }

    let data = body.to_vec();
    if data.len() > CONN_MAX_FILE {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!("单文件上限 {}MB", CONN_MAX_FILE >> 20),
        ));
    }
    let sha = ncc_core::crypto::sha256_hex(&data);
    let want = web::query(&uri, "sha256").unwrap_or_default();
    if !want.trim().is_empty() && sha != want.trim() {
        return Err(ApiError::bad_request(
            "checksum_mismatch",
            "sha256 与声明的不一致 —— 一个字节都没落盘",
        ));
    }

    let executable = web::query(&uri, "mode").as_deref() == Some("700")
        || web::query(&uri, "executable").as_deref() == Some("1");
    let mode: u32 = if executable { 0o700 } else { 0o600 };
    if let Err(e) = write_file_mode(&dst, &data, mode) {
        tracing::error!("写文件失败 {}: {e}", dst.display());
        return Err(ApiError::internal("写文件失败"));
    }

    let _ = store::conn::touch(state.pool(), &rec.id, 0, data.len() as i64, 0, 0).await;
    let actor = shares::ensure_admin(&state, &auth, &headers).await;
    shares::audit(
        &state,
        actor.as_ref(),
        "conn.put",
        &rec.id,
        &rec.name,
        "通道上推送文件",
        json!({
            "path": rel, "bytes": data.len(), "sha256": sha,
            "reason": web::query(&uri, "reason").unwrap_or_default(),
        }),
        &web::client_ip(&headers),
    )
    .await;
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({
            "path": rel, "bytes": data.len(), "sha256": sha,
            "abs": dst.to_string_lossy(), "mode": format!("{mode:o}"),
        }),
    ))
}

/// GET /api/conn/connections/{id}/files?path=<相对> —— 拉文件（文本/二进制都行）。
async fn conn_get_file(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    let rec = conn_for(&state, &auth, &headers, &id).await?;
    let rel = web::query(&uri, "path").unwrap_or_default();
    let src =
        safe_join(&rec.work_dir, &rel).map_err(|e| ApiError::bad_request("bad_request", e))?;
    let meta = match std::fs::metadata(&src) {
        Ok(m) if !m.is_dir() => m,
        _ => {
            return Err(ApiError::not_found("文件不存在（或是个目录）"));
        }
    };
    if meta.len() > CONN_MAX_FILE as u64 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            format!(
                "单文件上限 {}MB（大文件请分块或走制品托管）",
                CONN_MAX_FILE >> 20
            ),
        ));
    }
    let data = match std::fs::read(&src) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!("读文件失败 {}: {e}", src.display());
            return Err(ApiError::internal("读文件失败"));
        }
    };
    let _ = store::conn::touch(state.pool(), &rec.id, 0, 0, data.len() as i64, 1).await;

    let base = StdPath::new(&rel)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("X-NCC-Sha256", ncc_core::crypto::sha256_hex(&data))
        .header("X-Content-Type-Options", "nosniff")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{base}\""),
        )
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from(data))
        .unwrap())
}

/* ---------------- 守卫与视图 ---------------- */

/// 通道上可以动手吗（404 / 403 / 410 分开说）。
async fn conn_for(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    id: &str,
) -> ApiResult<store::conn::ConnRow> {
    let rec = store::conn::find(state.pool(), id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("连接不存在"))?;
    if !state.cfg().conn_allow {
        return Err(conn_not_allowed(false));
    }
    let mine = auth
        .info()
        .map(|a| a.user_id == rec.owner_id)
        .unwrap_or(false);
    if mine {
        return conn_usable(rec);
    }
    if shares::ensure_admin(state, auth, headers).await.is_some() {
        return conn_usable(rec);
    }
    // 通道里能跑任意命令、读文件 —— 只有当事人与管理员能碰。
    Err(ApiError::forbidden(
        "只能使用自己建立的连接（管理员可看全部）",
    ))
}

/// 状态守卫（关闭 / 过期分开说 —— 用户要知道是哪一种）。
fn conn_usable(rec: store::conn::ConnRow) -> ApiResult<store::conn::ConnRow> {
    match rec.state_at(chrono::Local::now().fixed_offset()) {
        store::conn::CONN_CLOSED => Err(ApiError::new(
            StatusCode::GONE,
            "conn_closed",
            "这条连接已关闭（重新 ncc conn open 即可）",
        )),
        "expired" => Err(ApiError::new(
            StatusCode::GONE,
            "conn_expired",
            "这条连接已过期（TTL 到了）——重新 ncc conn open，或建的时候就给更长的 --ttl",
        )),
        _ => Ok(rec),
    }
}

/// 通道视图。
fn conn_json(rec: &store::conn::ConnRow, now: chrono::DateTime<chrono::FixedOffset>) -> Value {
    json!({
        "id": rec.id, "name": rec.name, "note": rec.note,
        "state": rec.state_at(now),
        // usable 把「state 不是 open」翻译成「能不能动手」：客户端拿来灰置、拿来做提示，
        // 免得每个调用方自己写一遍 state → 能不能用的映射。
        "usable": rec.open_at(now),
        "workDir": rec.work_dir, "peer": rec.peer,
        "ttlSec": rec.ttl_sec, "expiresAt": rec.expires_at,
        "execCount": rec.exec_count, "bytesUp": rec.bytes_up,
        "bytesDown": rec.bytes_down, "pulls": rec.pull_count,
        "lastUsedAt": rec.last_used_at, "createdAt": rec.created_at,
        "closedAt": rec.closed_at,
        "url": format!("/api/conn/connections/{}", rec.id),
    })
}

/// 把「工作目录 + 用户给的相对路径」拼成一个**保证在工作目录内**的绝对路径。
///
/// 这是文件面的唯一一道门。四层把关：
///
/// 1. 空路径、含 `\0` 的路径直接拒；
/// 2. 绝对路径（`/`、`\` 开头）直接拒；
/// 3. 归一后（`path.Clean` 语义）必须落在工作目录里 —— 注意 `..` 是被**归一掉**的，
///    不是被拒的：`../x` 会变成工作目录下的 `x`。这看似宽松，但正是 Go 的行为，
///    而且「结果一定在工作目录内」这个不变量才是安全边界；
/// 4. **软链接逃逸**：路径上已存在的那一段若是符号链接、真身落在工作目录之外，拒绝。
///    这一条是 Rust 版**加严**的地方（Go 的 `safe_join` 只做前三层）——
///    别人可以在工作目录里放一个指向 `/etc` 的软链，之后所有路径检查都会看着像"在目录内"。
fn safe_join(root: &str, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("路径不能为空".to_string());
    }
    if rel.contains('\0') {
        return Err("路径里有非法字符".to_string());
    }
    if rel.starts_with('/') || rel.starts_with('\\') {
        return Err(format!("只接受相对路径（锁在这条通道的工作目录里）：{rel}"));
    }
    let clean = clean_slash(rel);
    if clean == "/" {
        return Err("路径不能是工作目录本身".to_string());
    }
    if clean.contains("..") {
        return Err(format!("路径不许跳出工作目录：{rel}"));
    }
    let root_path = StdPath::new(root);
    let full = root_path.join(clean.trim_start_matches('/'));
    let root_s = root_path.to_string_lossy().to_string();
    let full_s = full.to_string_lossy().to_string();
    if full_s != root_s && !full_s.starts_with(&format!("{root_s}{}", std::path::MAIN_SEPARATOR)) {
        return Err(format!("路径越界：{rel}"));
    }
    ensure_no_symlink_escape(root_path, &full)?;
    Ok(full)
}

/// 目标路径上「已存在的最深一段」的真身必须还在工作目录里。
///
/// 只查最深的那一段就够：`canonicalize` 会把整条链上的软链都解开，
/// 一旦中途有软链指向外面，解开后的结果就不在根里了。
fn ensure_no_symlink_escape(root: &StdPath, full: &StdPath) -> Result<(), String> {
    let Ok(root_c) = std::fs::canonicalize(root) else {
        // 工作目录还没建起来（或不可读）时不做这项检查：写文件时自然会在别处报错。
        return Ok(());
    };
    let mut probe = Some(full.to_path_buf());
    while let Some(p) = probe {
        match std::fs::symlink_metadata(&p) {
            Ok(_) => {
                let Ok(real) = std::fs::canonicalize(&p) else {
                    // 断掉的软链：真身不存在，按「跳出去了」处理更安全
                    return Err(format!("路径经软链接跳出工作目录：{}", p.display()));
                };
                if !real.starts_with(&root_c) {
                    return Err(format!("路径经软链接跳出工作目录：{}", p.display()));
                }
                return Ok(());
            }
            Err(_) => probe = p.parent().map(|x| x.to_path_buf()),
        }
    }
    Ok(())
}

/// 与 Go 的 `path.Clean("/"+s)` 等价（反斜杠先折成 `/`）。
fn clean_slash(rel: &str) -> String {
    let normalized = rel.replace('\\', "/");
    let mut out: Vec<&str> = Vec::new();
    for seg in normalized.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// 写文件并按需设权限（`?executable=1` 时 0700 —— 脚本要能直接跑）。
#[cfg(unix)]
fn write_file_mode(p: &StdPath, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(p, data)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn write_file_mode(p: &StdPath, data: &[u8], _mode: u32) -> std::io::Result<()> {
    std::fs::write(p, data)
}

/// 该族的启动收尾（只打日志，见 `store::conn::count_expired_now` 的注释）。
///
/// `main.rs` 属本批次不让改的文件，所以这里只导出、由整体收口的 agent 接线
/// （与 Go 的 `staleConns()` 同一位置）。
#[allow(dead_code)]
pub async fn stale_conns(state: &AppState) {
    match store::conn::count_expired_now(state.pool()).await {
        Ok(n) if n > 0 => tracing::warn!("这台机器上还挂着 {n} 条 TTL 已过的连接通道"),
        Ok(_) => {}
        Err(e) => tracing::error!("统计过期通道失败: {e}"),
    }
}

/* ================= 测试 ================= */

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use ncc_core::storage::LocalStorage;
    use std::sync::Arc;
    use tower::ServiceExt;

    fn test_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ncc-conn-http-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    async fn state(tag: &str, allow_conn: bool, exec_allow: &[&str]) -> AppState {
        let dir = test_dir(tag);
        let pool = ncc_core::pool::open_sqlite(&dir.join("t.db"))
            .await
            .unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL)
            .await
            .unwrap();
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://localhost:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();
        cfg.blob_dir = dir.join("blobs");
        cfg.exec_dir = dir.join("exec");
        cfg.conn_dir = dir.join("conn");
        cfg.conn_allow = allow_conn;
        cfg.exec_allow = exec_allow.iter().map(|s| s.to_string()).collect();
        // 假 runner 让 wasm 也「可用」，好在通道上验证「通道只收 process/container」
        let fake = dir.join("fake-ncc");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        cfg.exec_runner = fake.to_string_lossy().to_string();
        let blobs = LocalStorage::new(&cfg.blob_dir, &cfg.public_url, "blobs").unwrap();
        let seal = ncc_core::secretbox::SecretBox::new(&cfg.jwt_secret).unwrap();
        AppState {
            cfg: Arc::new(cfg),
            pool,
            blobs: Arc::new(blobs),
            seal: Arc::new(seal),
        }
    }

    fn mk_app(state: &AppState) -> Router {
        Router::new()
            // 与 `router.rs` 一致：本族挂在 `/api` 下
            .nest("/api", routes())
            .layer(axum::extract::DefaultBodyLimit::max(64 << 20))
            .with_state(state.clone())
    }

    async fn seed_user(state: &AppState, name: &str) -> (String, String) {
        let u = store::users::create(state.pool(), name, &format!("{name}@x.com"), "h")
            .await
            .unwrap();
        let (_k, secret) = store::apikeys::create(state.pool(), &u.id, "t", &[])
            .await
            .unwrap();
        (u.id, secret)
    }

    /// 直接在库里建一条通道（测试里不想每次都走 HTTP）。
    async fn seed_conn(state: &AppState, owner: &str, id: &str, ttl: i64) -> store::conn::ConnRow {
        let work = state.cfg().conn_dir.join(id);
        std::fs::create_dir_all(work.join(".tmp")).unwrap();
        store::conn::create(
            state.pool(),
            store::conn::ConnInput {
                id: id.to_string(),
                owner_id: owner.to_string(),
                requested_by: "a@x.com".to_string(),
                name: "通道".to_string(),
                note: String::new(),
                work_dir: work.to_string_lossy().to_string(),
                peer: "node/ND-1".to_string(),
                ttl_sec: ttl,
                expires_at: Some(ncc_core::timeutil::format_go(
                    chrono::Local::now().fixed_offset() + chrono::Duration::seconds(ttl),
                )),
            },
        )
        .await
        .unwrap()
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let b = axum::body::to_bytes(res.into_body(), 1 << 30)
            .await
            .unwrap();
        (status, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    async fn call_raw(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let b = axum::body::to_bytes(res.into_body(), 1 << 30)
            .await
            .unwrap();
        (status, headers, b.to_vec())
    }

    fn get(uri: &str, secret: &str) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if !secret.is_empty() {
            b = b.header("authorization", format!("Bearer {secret}"));
        }
        b.body(Body::empty()).unwrap()
    }

    fn del(uri: &str, secret: &str) -> Request<Body> {
        let mut b = Request::builder().method("DELETE").uri(uri);
        if !secret.is_empty() {
            b = b.header("authorization", format!("Bearer {secret}"));
        }
        b.body(Body::empty()).unwrap()
    }

    fn post_json(uri: &str, secret: &str, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json");
        if !secret.is_empty() {
            b = b.header("authorization", format!("Bearer {secret}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn post_bytes(uri: &str, secret: &str, data: Vec<u8>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/octet-stream");
        if !secret.is_empty() {
            b = b.header("authorization", format!("Bearer {secret}"));
        }
        b.body(Body::from(data)).unwrap()
    }

    /// 查询串里的路径参数要编码（`..`、`/`、反斜杠都不能原样进 URI）。
    fn urlencode(s: &str) -> String {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_' {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    }

    /// 整站路由表装配：确认本族端点**真的挂上去了**（不是被 501 兜底接住）。
    #[tokio::test]
    async fn 整站路由表_本族端点已挂载() {
        let st = state("router", true, &["process"]).await;
        let app = crate::router::build(&st);
        let (code, v) = call(&app, get("/api/conn/connections", "")).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"]["code"], "unauthorized");
        let (code, v) = call(&app, get("/api/conn/connections/CN-nope", "")).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["message"], "连接不存在");
    }

    /* ---- 默认关 ---- */

    #[tokio::test]
    async fn 通道默认关_一律403() {
        let st = state("closed", false, &["wasm", "process"]).await;
        let (uid, secret) = seed_user(&st, "张三").await;
        let rec = seed_conn(&st, &uid, "CN-1", 3600).await;
        let app = mk_app(&st);

        let (code, v) = call(&app, post_json("/api/conn/connections", &secret, json!({}))).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "conn_not_allowed");
        assert_eq!(
            v["error"]["message"],
            "这台机器没有开放连接通道（通道能跑任意命令、写文件，属最高权限）—— 运维要显式打开：NCCR_CONN_ALLOW=1"
        );

        for req in [
            get(&format!("/api/conn/connections/{}", rec.id), &secret),
            post_json(
                &format!("/api/conn/connections/{}/exec", rec.id),
                &secret,
                json!({"cmd": "ls", "reason": "r"}),
            ),
            post_bytes(
                &format!("/api/conn/connections/{}/files?path=a&reason=r", rec.id),
                &secret,
                b"x".to_vec(),
            ),
            get(
                &format!("/api/conn/connections/{}/files?path=a", rec.id),
                &secret,
            ),
        ] {
            let (code, v) = call(&app, req).await;
            assert_eq!(code, StatusCode::FORBIDDEN, "{v}");
            assert_eq!(v["error"]["code"], "conn_not_allowed");
            assert_eq!(
                v["error"]["message"],
                "这台机器没有开放连接通道（NCCR_CONN_ALLOW=1 才开）"
            );
        }

        // 不存在的通道：先 404（顺序与 Go 一致）
        let (code, v) = call(&app, get("/api/conn/connections/CN-nope", &secret)).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "连接不存在");
    }

    #[tokio::test]
    async fn 未登录401_认不出人() {
        let st = state("noauth", true, &["process"]).await;
        let app = mk_app(&st);
        let (code, v) = call(&app, post_json("/api/conn/connections", "", json!({}))).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(
            v["error"]["message"],
            "未认证或凭据无效（先 ncc login / ncc conn open --key …）"
        );
        let (code, v) = call(&app, get("/api/conn/connections", "")).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["message"], "未认证或凭据无效");
    }

    /* ---- 建 / 列 / 看 / 关 ---- */

    #[tokio::test]
    async fn 建通道_列表_详情_关闭与purge() {
        let st = state("crud", true, &["process"]).await;
        let (uid, secret) = seed_user(&st, "张三").await;
        let (_u2, secret2) = seed_user(&st, "李四").await;
        let app = mk_app(&st);

        let (code, v) = call(
            &app,
            post_json(
                "/api/conn/connections",
                &secret,
                json!({"name": "部署通道", "note": "给小王用", "ttlSec": 60}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        let id = v["connection"]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("CN-"));
        assert_eq!(v["connection"]["name"], "部署通道");
        assert_eq!(v["connection"]["note"], "给小王用");
        assert_eq!(v["connection"]["state"], "open");
        assert_eq!(v["connection"]["usable"], true);
        assert_eq!(v["connection"]["ttlSec"], 60);
        assert_eq!(v["connection"]["execCount"], 0);
        assert_eq!(v["connection"]["bytesUp"], 0);
        assert_eq!(v["connection"]["bytesDown"], 0);
        assert_eq!(v["connection"]["pulls"], 0);
        assert_eq!(
            v["connection"]["url"],
            format!("/api/conn/connections/{id}")
        );
        assert_eq!(
            v["connection"]["peer"],
            format!("{}/{}", st.cfg().node_name, st.cfg().node_id)
        );
        assert!(v["howto"]["exec"].as_str().unwrap().contains(&id));
        assert!(v["howto"]["push"]
            .as_str()
            .unwrap()
            .contains("path=<相对路径>&reason="));
        assert!(v["howto"]["pull"]
            .as_str()
            .unwrap()
            .starts_with("GET  /api/conn/connections/"));
        assert!(v["howto"]["close"].as_str().unwrap().contains("?purge=1"));
        // 工作目录真的建出来了，且带 .tmp
        let work = PathBuf::from(v["connection"]["workDir"].as_str().unwrap());
        assert!(work.join(".tmp").is_dir());

        // 列表：只看得到自己的
        let (code, v) = call(&app, get("/api/conn/connections", &secret)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["limit"], 50);
        let (_, v2) = call(&app, get("/api/conn/connections", &secret2)).await;
        assert_eq!(v2["total"], 0);
        // 普通用户 ?all=1 → 403
        let (code, v2) = call(&app, get("/api/conn/connections?all=1", &secret2)).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v2["error"]["code"], "admin_required");

        // 详情：别人看不到
        let (code, v2) = call(&app, get(&format!("/api/conn/connections/{id}"), &secret2)).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(
            v2["error"]["message"],
            "只能看自己建立的连接（管理员可看全部）"
        );
        let (code, v2) = call(&app, get(&format!("/api/conn/connections/{id}"), &secret)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v2["connection"]["execs"].as_array().unwrap().len(), 0);
        assert_eq!(v2["connection"]["blocked"], Value::Null);

        // 别人关不掉
        let (code, v2) = call(&app, del(&format!("/api/conn/connections/{id}"), &secret2)).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v2["error"]["message"], "只能关闭自己建立的连接");

        // 自己关：purge 连目录一起删
        let (code, v2) = call(
            &app,
            del(&format!("/api/conn/connections/{id}?purge=1"), &secret),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v2["wasOpen"], true);
        assert_eq!(v2["purged"], true);
        assert!(!work.exists());
        // 重复关闭：wasOpen=false（幂等，不报错）
        let (_, v2) = call(&app, del(&format!("/api/conn/connections/{id}"), &secret)).await;
        assert_eq!(v2["wasOpen"], false);
        assert_eq!(v2["purged"], false);
        // 关了以后还能看（归档 ≠ 删除），且带上 blocked
        let (code, v2) = call(&app, get(&format!("/api/conn/connections/{id}"), &secret)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v2["connection"]["state"], "closed");
        assert_eq!(v2["connection"]["usable"], false);
        assert_eq!(v2["connection"]["blocked"], "closed");

        let _ = uid;
    }

    #[tokio::test]
    async fn ttl上限与坏body() {
        let st = state("ttl", true, &["process"]).await;
        let (_uid, secret) = seed_user(&st, "张三").await;
        let app = mk_app(&st);
        let (code, v) = call(
            &app,
            post_json("/api/conn/connections", &secret, json!({"ttlSec": 28801})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "ttlSec 不能超过 28800 秒");

        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/api/conn/connections")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::from("{不是 JSON"))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "请求体格式错误");

        // 空 body 允许
        let (code, _) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/api/conn/connections")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
    }

    /* ---- 状态守卫 ---- */

    #[tokio::test]
    async fn 过期410_关闭410() {
        let st = state("state", true, &["process"]).await;
        let (uid, secret) = seed_user(&st, "张三").await;
        let expired = seed_conn(&st, &uid, "CN-OLD", -10).await;
        let closed = seed_conn(&st, &uid, "CN-CLOSE", 3600).await;
        store::conn::close(st.pool(), &closed.id, "").await.unwrap();
        let app = mk_app(&st);

        let (code, v) = call(
            &app,
            post_json(
                &format!("/api/conn/connections/{}/exec", expired.id),
                &secret,
                json!({"cmd": "ls", "reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(v["error"]["code"], "conn_expired");
        assert_eq!(
            v["error"]["message"],
            "这条连接已过期（TTL 到了）——重新 ncc conn open，或建的时候就给更长的 --ttl"
        );

        let (code, v) = call(
            &app,
            post_bytes(
                &format!("/api/conn/connections/{}/files?path=a&reason=r", expired.id),
                &secret,
                b"x".to_vec(),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(v["error"]["code"], "conn_expired");

        let (code, v) = call(
            &app,
            post_json(
                &format!("/api/conn/connections/{}/exec", closed.id),
                &secret,
                json!({"cmd": "ls", "reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(v["error"]["code"], "conn_closed");
        assert_eq!(
            v["error"]["message"],
            "这条连接已关闭（重新 ncc conn open 即可）"
        );

        // 过期通道在列表里显示 expired（不是 closed）
        let (_, v) = call(&app, get("/api/conn/connections", &secret)).await;
        let states: Vec<&str> = v["connections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["state"].as_str().unwrap())
            .collect();
        assert!(states.contains(&"expired"));
        assert!(states.contains(&"closed"));
    }

    /* ---- 通道上执行 ---- */

    #[tokio::test]
    async fn 通道执行_参数与引擎都拦得住_正常跑完带日志尾巴() {
        let st = state("exec", true, &["wasm", "process"]).await;
        let (uid, secret) = seed_user(&st, "张三").await;
        let rec = seed_conn(&st, &uid, "CN-1", 3600).await;
        let app = mk_app(&st);

        // 缺 cmd
        let (code, v) = call(
            &app,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec.id),
                &secret,
                json!({"reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "缺少 cmd");

        // 缺 reason
        let (code, v) = call(
            &app,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec.id),
                &secret,
                json!({"cmd": "ls"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "缺少 reason：这条通道上干了什么、为什么，要留在账本里"
        );

        // wasm 不给（通道的语义是"在目标机上干活"）
        let (code, v) = call(
            &app,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec.id),
                &secret,
                json!({"cmd": "ls", "reason": "r", "engine": "wasm"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "通道上只支持 process / container 引擎（wasm 请走 ncc sandbox run --package）"
        );

        // 坏 body
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri(format!("/api/conn/connections/{}/exec", rec.id))
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::from("{"))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "请求体格式错误（需要 cmd 与 reason）"
        );

        // 正常运行：同步返回 + 日志尾巴
        let (code, v) = call(
            &app,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec.id),
                &secret,
                json!({"cmd": "echo 通道你好", "reason": "打声招呼"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let e = &v["exec"];
        assert_eq!(e["status"], "succeeded");
        assert_eq!(e["exitCode"], 0);
        assert_eq!(e["engine"], "process");
        assert_eq!(e["kind"], "cmd");
        assert_eq!(e["connId"], "CN-1");
        assert_eq!(e["reason"], "打声招呼");
        assert!(e["logTail"].as_str().unwrap().contains("通道你好"));
        assert_eq!(e["logTailTruncated"], false);
        // 任务落进库里，工作目录在通道目录下（不是 exec_dir）
        assert!(PathBuf::from(e["workDir"].as_str().unwrap()).starts_with(&rec.work_dir));

        // 计数 + 详情里能看到这条任务
        let c = store::conn::find(st.pool(), &rec.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(c.exec_count, 1);
        assert!(c.last_used_at.is_some());
        let (_, v) = call(
            &app,
            get(&format!("/api/conn/connections/{}", rec.id), &secret),
        )
        .await;
        assert_eq!(v["connection"]["execCount"], 1);
        assert_eq!(v["connection"]["execs"].as_array().unwrap().len(), 1);
        assert_eq!(v["connection"]["execs"][0]["connId"], "CN-1");

        // 异步（wait=false）：202 + 提示，最后仍会落终态
        let (code, v) = call(
            &app,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec.id),
                &secret,
                json!({"cmd": "echo 异步", "reason": "r", "wait": false}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::ACCEPTED);
        assert_eq!(v["note"], "已提交，用 GET /api/exec/runs/:id 看结果");
        let id = v["exec"]["id"].as_str().unwrap().to_string();
        for _ in 0..100 {
            let r = store::exec::find(st.pool(), &id).await.unwrap().unwrap();
            if r.done() {
                assert_eq!(r.status, "succeeded");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(store::exec::find(st.pool(), &id)
            .await
            .unwrap()
            .unwrap()
            .done());
    }

    #[tokio::test]
    async fn 通道执行_引擎没放行403_超时限400_cwd越界400() {
        let st = state("exec-deny", true, &["wasm"]).await; // process 没放行
        let (_uid, secret) = seed_user(&st, "张三").await;
        let rec = seed_conn(&st, &_uid, "CN-1", 3600).await;
        let app = mk_app(&st);

        let (code, v) = call(
            &app,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec.id),
                &secret,
                json!({"cmd": "ls", "reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "engine_not_allowed");
        assert_eq!(
            v["error"]["message"],
            "这台机器没有放行 process 引擎（NCCR_EXEC_ALLOW 加上它）"
        );

        // 放行 process 之后：超时上限与 cwd 越界
        let mut st2 = state("exec-deny2", true, &["process"]).await;
        let mut cfg = (*st2.cfg()).clone();
        cfg.exec_timeout = std::time::Duration::from_secs(5);
        st2.cfg = Arc::new(cfg);
        let (uid2, secret2) = seed_user(&st2, "李四").await;
        let rec2 = seed_conn(&st2, &uid2, "CN-2", 3600).await;
        let app2 = mk_app(&st2);
        let (code, v) = call(
            &app2,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec2.id),
                &secret2,
                json!({"cmd": "ls", "reason": "r", "timeoutSec": 6}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "timeoutSec 不能超过节点上限（5 秒）");

        for cwd in ["/etc", "..", "\\\\etc"] {
            let (code, v) = call(
                &app2,
                post_json(
                    &format!("/api/conn/connections/{}/exec", rec2.id),
                    &secret2,
                    json!({"cmd": "ls", "reason": "r", "cwd": cwd}),
                ),
            )
            .await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "cwd={cwd}");
            assert_eq!(v["error"]["code"], "bad_request");
        }

        // `..` 归一后仍在工作目录里（子目录会被建出来）
        let (code, v) = call(
            &app2,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec2.id),
                &secret2,
                json!({"cmd": "pwd", "reason": "r", "cwd": "../outside"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert!(v["exec"]["workDir"].as_str().unwrap().ends_with("outside"));

        // 合法子目录会被建出来
        let (code, v) = call(
            &app2,
            post_json(
                &format!("/api/conn/connections/{}/exec", rec2.id),
                &secret2,
                json!({"cmd": "pwd", "reason": "r", "cwd": "app/deploy"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let wd = v["exec"]["workDir"].as_str().unwrap().to_string();
        assert!(wd.ends_with("app/deploy"), "{wd}");
        assert!(v["exec"]["logTail"]
            .as_str()
            .unwrap()
            .contains("app/deploy"));
    }

    /* ---- 文件推拉 ---- */

    #[tokio::test]
    async fn 文件推拉_校验与穿越都拦得住() {
        let st = state("files", true, &["process"]).await;
        let (uid, secret) = seed_user(&st, "张三").await;
        let rec = seed_conn(&st, &uid, "CN-1", 3600).await;
        let app = mk_app(&st);
        let sha = ncc_core::crypto::sha256_hex(b"echo hi\n");

        // 缺 path
        let (code, v) = call(
            &app,
            post_bytes(
                &format!("/api/conn/connections/{}/files?reason=r", rec.id),
                &secret,
                b"x".to_vec(),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "缺少 path（相对工作目录的路径，如 app/deploy.sh）"
        );
        // 缺 reason
        let (code, v) = call(
            &app,
            post_bytes(
                &format!("/api/conn/connections/{}/files?path=a.sh", rec.id),
                &secret,
                b"x".to_vec(),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "缺少 reason：往目标机写了什么、为什么，要留在账本里"
        );

        // 绝对路径 / 工作目录本身 / 反斜杠开头一律拒
        for p in ["/etc/passwd", "..", "\\evil", "/"] {
            let (code, v) = call(
                &app,
                post_bytes(
                    &format!(
                        "/api/conn/connections/{}/files?path={}&reason=r",
                        rec.id,
                        urlencode(p)
                    ),
                    &secret,
                    b"x".to_vec(),
                ),
            )
            .await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "path={p} → {v}");
        }
        // `..` 是被**归一掉**的（不是被拒的）：结果一定还在工作目录里 ——
        // 与 Go 的 `path.Clean` 语义一致，安全不变量是「落在工作目录内」。
        let (code, v) = call(
            &app,
            post_bytes(
                &format!(
                    "/api/conn/connections/{}/files?path={}&reason=r",
                    rec.id,
                    urlencode("a/../../inside.sh")
                ),
                &secret,
                b"x".to_vec(),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["path"], "a/../../inside.sh");
        assert_eq!(
            PathBuf::from(v["abs"].as_str().unwrap()),
            std::path::Path::new(&rec.work_dir).join("inside.sh")
        );

        // 软链接逃逸：工作目录里的软链指向外面，写进去要拒（Rust 版加严的地方）
        #[cfg(unix)]
        {
            let outside = test_dir("files-outside");
            let link = std::path::Path::new(&rec.work_dir).join("escape");
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let (code, v) = call(
                &app,
                post_bytes(
                    &format!(
                        "/api/conn/connections/{}/files?path=escape/x.sh&reason=r",
                        rec.id
                    ),
                    &secret,
                    b"x".to_vec(),
                ),
            )
            .await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
            assert!(v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("软链接跳出工作目录"));
            assert!(!outside.join("x.sh").exists());
        }

        // 正常推：可执行位 + sha 声明（对得上）
        let (code, v) = call(
            &app,
            post_bytes(
                &format!(
                    "/api/conn/connections/{}/files?path=app/deploy.sh&reason=部署&sha256={sha}&executable=1",
                    rec.id
                ),
                &secret,
                b"echo hi\n".to_vec(),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["path"], "app/deploy.sh");
        assert_eq!(v["bytes"], 8);
        assert_eq!(v["sha256"], sha);
        assert_eq!(v["mode"], "700");
        let abs = PathBuf::from(v["abs"].as_str().unwrap());
        assert_eq!(std::fs::read(&abs).unwrap(), b"echo hi\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&abs).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        // sha 对不上：一个字节都不落盘
        let (code, v) = call(
            &app,
            post_bytes(
                &format!(
                    "/api/conn/connections/{}/files?path=bad.sh&reason=r&sha256=deadbeef",
                    rec.id
                ),
                &secret,
                b"x".to_vec(),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["code"], "checksum_mismatch");
        assert_eq!(
            v["error"]["message"],
            "sha256 与声明的不一致 —— 一个字节都没落盘"
        );
        assert!(!std::path::Path::new(&rec.work_dir).join("bad.sh").exists());

        // 拉回来：字节 + 头
        let (code, headers, body) = call_raw(
            &app,
            get(
                &format!("/api/conn/connections/{}/files?path=app/deploy.sh", rec.id),
                &secret,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, b"echo hi\n");
        assert_eq!(headers["x-ncc-sha256"], sha);
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(
            headers["content-disposition"],
            "attachment; filename=\"deploy.sh\""
        );
        assert_eq!(headers["content-type"], "application/octet-stream");

        // 拉不存在的 / 拉目录
        let (code, v) = call(
            &app,
            get(
                &format!("/api/conn/connections/{}/files?path=nope.txt", rec.id),
                &secret,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "文件不存在（或是个目录）");
        let (code, _) = call(
            &app,
            get(
                &format!("/api/conn/connections/{}/files?path=app", rec.id),
                &secret,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        // 空 path
        let (code, v) = call(
            &app,
            get(
                &format!("/api/conn/connections/{}/files?path=", rec.id),
                &secret,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "路径不能为空");

        // 计数：推了 inside.sh(1) + deploy.sh(8)，拉了 1 次（8 字节）
        let c = store::conn::find(st.pool(), &rec.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(c.bytes_up, 9);
        assert_eq!(c.bytes_down, 8);
        assert_eq!(c.pull_count, 1);
    }

    #[tokio::test]
    async fn 单文件超限413() {
        let st = state("toolarge", true, &["process"]).await;
        let (uid, secret) = seed_user(&st, "张三").await;
        let rec = seed_conn(&st, &uid, "CN-1", 3600).await;
        let app = mk_app(&st);
        let big = vec![b'x'; CONN_MAX_FILE + 1];
        let (code, v) = call(
            &app,
            post_bytes(
                &format!(
                    "/api/conn/connections/{}/files?path=big.bin&reason=r",
                    rec.id
                ),
                &secret,
                big,
            ),
        )
        .await;
        assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(v["error"]["code"], "payload_too_large");
        assert_eq!(v["error"]["message"], "单文件上限 32MB");
        assert!(!std::path::Path::new(&rec.work_dir).join("big.bin").exists());
    }

    /* ---- safe_join 单测 ---- */

    #[test]
    fn 路径拼接_工作目录内才放行() {
        let root = "/srv/conn/CN-1";
        assert_eq!(
            safe_join(root, "app/deploy.sh").unwrap(),
            PathBuf::from("/srv/conn/CN-1/app/deploy.sh")
        );
        assert_eq!(
            safe_join(root, "  a/b/c  ").unwrap(),
            PathBuf::from("/srv/conn/CN-1/a/b/c")
        );
        // 归一后落在工作目录里（与 Go 的 path.Clean 一致）
        assert_eq!(
            safe_join(root, "a/../b").unwrap(),
            PathBuf::from("/srv/conn/CN-1/b")
        );
        assert_eq!(
            safe_join(root, "a/../../b").unwrap(),
            PathBuf::from("/srv/conn/CN-1/b")
        );

        // `..` 只归一、不逃逸：结果一定在工作目录里（Go 的 path.Clean 语义）
        assert_eq!(
            safe_join(root, "../evil.sh").unwrap(),
            PathBuf::from("/srv/conn/CN-1/evil.sh")
        );
        assert_eq!(
            safe_join(root, "..\\windows").unwrap(),
            PathBuf::from("/srv/conn/CN-1/windows")
        );

        assert_eq!(safe_join(root, "").unwrap_err(), "路径不能为空");
        assert_eq!(safe_join(root, "   ").unwrap_err(), "路径不能为空");
        assert_eq!(safe_join(root, "a\0b").unwrap_err(), "路径里有非法字符");
        assert_eq!(safe_join(root, "..").unwrap_err(), "路径不能是工作目录本身");
        assert_eq!(
            safe_join(root, "/etc/passwd").unwrap_err(),
            "只接受相对路径（锁在这条通道的工作目录里）：/etc/passwd"
        );
        assert_eq!(
            safe_join(root, "\\evil").unwrap_err(),
            "只接受相对路径（锁在这条通道的工作目录里）：\\evil"
        );
        // 反斜杠形态在归一之后才会露出 `..`（Go 同样先替换反斜杠）
        assert_eq!(clean_slash("a\\..\\..\\b"), "/b");
    }
}
