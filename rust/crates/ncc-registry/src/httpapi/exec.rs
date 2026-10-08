//! 远程执行（Remote Cloud Computer / 云电脑）的 HTTP 层。
//!
//! 原实现：`ncc-registry/httpapi/exec.go` + `internal/execrun/{execrun,kill_unix,kill_windows}.go`。
//! 执行器没进 `internal/`（本批次不让改 `main.rs` / 模块表），就地放在本文件下半部分 ——
//! 它只服务这一族，没有第二个调用方。
//!
//! 一句话分工：云电脑是**算力在客户自己机器上**的形态（数据面不经过 NCC）；
//! 本服务只做三件事：说清自己能跑什么（kinds）、接活（runs）、如实回报（日志 + 退出码）。
//!
//! 红线（与 `prd/ncc-sandbox.md` 对齐）：
//!
//! 1. **执行权是最高权限**：`wasm` 是沙箱；`process` / `container` 是真的在这台机器上
//!    跑别人的命令 —— 默认**只放行 wasm**，其余要运维显式打开（`NCCR_EXEC_ALLOW`）。
//! 2. **每条任务都要 reason**：日志与审计要能回答「谁让这台机器干了什么、为什么」。
//! 3. **不继承服务端环境变量**：子进程只拿 `PATH/LANG` 这类必需品 + `NCC_EXEC_*` 元信息
//!    （服务端 env 里有 JWT secret、库路径）。
//! 4. **取消要真停**：整组回收子进程，不是把状态改成 `canceled` 了事。
//! 5. **超限要如实记**：日志超过上限会截断，但截断这件事要写进记录 / 响应头，
//!    否则看到半截日志的人会以为程序只输出了这些。
//!
//! 与 Go 的两处刻意差异（都在对应函数上写了原因）：
//!
//! * **杀进程组是 TERM → 宽限 → KILL**，Go 的 `kill_unix.go` 直接 `SIGKILL`；
//! * 任务**工作目录名与任务 id 相同**（Go 的 handler 会再生成一个随机串做目录名）。

use std::collections::HashMap;
use std::path::{Path as StdPath, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::watch;

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::web;

use crate::config::{valid_exec_engine, Config, EXEC_ENGINES};
use crate::httpapi::{helpers, shares, AppState, Auth};
use crate::store;

/// 包字节上限（32MB；比名片的 8MB 宽，因为要装真包）。
const MAX_EXEC_UPLOAD: i64 = 32 << 20;
/// reason 最长（与 R12 同档）。
pub(crate) const MAX_EXEC_REASON: usize = 400;
/// 状态接口里回带的日志尾巴上限。
const EXEC_LOG_TAIL_MAX: u64 = 64 << 10;
/// 单条目解包上限（与 Go 的 `io.LimitReader(rc, 64<<20)` 同档）。
const MAX_ENTRY_BYTES: usize = 64 << 20;
/// gzip 层解压上限（压缩炸弹在这里被挡住）。
const MAX_GUNZIP: usize = 256 << 20;
/// 终止宽限期：先 TERM，给子进程这么长时间收尾，之后 KILL。
const KILL_GRACE: Duration = Duration::from_secs(5);

/// 该族路由（相对 `/api`）。
///
/// `kinds` **公开**：路由要按「真的能跑」挑机器，而不是按节点自己贴的标签猜
/// （标签是声明，这里回答的是本机事实）。里面没有秘密：只有引擎名、限额与标签。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/exec/kinds", get(exec_kinds))
        .route("/exec/runs", get(list_exec_runs).post(create_exec_run))
        .route("/exec/runs/", get(list_exec_runs).post(create_exec_run))
        .route("/exec/runs/{id}", get(get_exec_run).delete(exec_run_action))
        .route("/exec/runs/{id}/log", get(exec_run_log))
}

/// 该族没有顶层公开页（`/a/...`、`/s/...` 那种），所以不导出 `public_routes()`：
/// `router.rs` 里没挂这一族的顶层页，导出一个不会被 merge 的函数只会变成死代码。

/* ================= 能力自述（Capability） ================= */

/// 一种引擎的可用性（本机事实 + 为什么）。
pub struct Kind {
    pub id: &'static str,
    pub enabled: bool,
    /// 谁来跑（人读一句话）。
    pub provider: String,
    /// 不可用时：为什么 / 要什么才能开。
    pub why: String,
}

/// 跑 wasm 的 HUR 运行时（默认从 PATH 找 `ncc`）。
pub struct RunnerInfo {
    pub path: String,
    pub ok: bool,
    pub note: String,
    /// 配置里写的名字（未解析前）。
    pub bin: String,
}

/// 这台机器的执行能力（`probe` 的产物，无副作用）。
pub struct Capability {
    /// 运维放行的引擎（`NCCR_EXEC_ALLOW`，默认 wasm）。
    pub allow: Vec<String>,
    pub kinds: Vec<Kind>,
    pub runner: RunnerInfo,
    pub shell: String,
    pub docker: String,
    /// container 允许的镜像（空 = 不限制）。
    pub images: Vec<String>,
    pub tags: Vec<String>,
    /// 单任务默认墙上限。
    pub timeout_ms: i64,
    pub max_output: i64,
    pub work_root: String,
}

impl Capability {
    pub(crate) fn is_allowed(&self, engine: &str) -> bool {
        self.allow.iter().any(|e| e == engine)
    }

    pub(crate) fn is_enabled(&self, engine: &str) -> bool {
        self.kinds.iter().any(|k| k.id == engine && k.enabled)
    }

    pub(crate) fn why_of(&self, engine: &str) -> String {
        self.kinds
            .iter()
            .find(|k| k.id == engine)
            .map(|k| k.why.clone())
            .unwrap_or_default()
    }

    fn to_json(&self) -> Value {
        json!({
            "allow": self.allow,
            "kinds": self.kinds.iter().map(|k| json!({
                "id": k.id, "enabled": k.enabled, "provider": k.provider, "why": k.why,
            })).collect::<Vec<_>>(),
            "runner": {
                "path": self.runner.path, "ok": self.runner.ok,
                "note": self.runner.note, "bin": self.runner.bin,
            },
            "shell": self.shell,
            "docker": self.docker,
            "images": self.images,
            "tags": self.tags,
            "timeoutMs": self.timeout_ms,
            "maxOutputBytes": self.max_output,
            "workRoot": self.work_root,
        })
    }
}

/// 「为什么现在不能跑」——能跑就给空串（与 Go 的 `why()` 逐字一致）。
fn why_text(allowed: bool, ready: bool, need_allow: &str, need_ready: &str) -> String {
    match (allowed, ready) {
        (false, false) => format!("{need_allow}；另外 {need_ready}"),
        (false, true) => need_allow.to_string(),
        (true, false) => need_ready.to_string(),
        (true, true) => String::new(),
    }
}

/// 探查本机能力。**只看事实，不改状态**（每次都现探：runner / 容器运行时要能在装好之后
/// 立刻生效，不该因为启动时没装就一直说"不可用"）。
pub(crate) fn probe(cfg: &Config) -> Capability {
    let mut cap = Capability {
        allow: cfg.exec_allow.clone(),
        kinds: Vec::new(),
        runner: RunnerInfo {
            path: String::new(),
            ok: false,
            note: String::new(),
            bin: String::new(),
        },
        shell: cfg.exec_shell.clone(),
        docker: cfg.exec_docker.clone(),
        images: cfg.exec_images.clone(),
        tags: cfg.exec_tags.clone(),
        timeout_ms: cfg.exec_timeout.as_millis() as i64,
        max_output: cfg.exec_max_output,
        work_root: cfg.exec_dir.to_string_lossy().to_string(),
    };

    // wasm：要一个 HUR 运行时（默认 `ncc`）。
    let bin = {
        let b = cfg.exec_runner.trim().to_string();
        if b.is_empty() {
            "ncc".to_string()
        } else {
            b
        }
    };
    cap.runner.bin = bin.clone();
    match which(&bin) {
        Some(p) => {
            cap.runner.path = p.to_string_lossy().to_string();
            cap.runner.ok = true;
        }
        None => {
            cap.runner.note = format!(
                "PATH 里找不到 {bin} —— 装上 ncc（或设 NCCR_EXEC_RUNNER 指到它）后 wasm 引擎才可用"
            );
        }
    }

    // process：只要有 shell 就能跑（真正的门槛是运维放行，不是技术条件）。
    let shell_ok = which(&cfg.exec_shell).is_some();
    // container：要有容器运行时。
    let docker_ok = which(&cfg.exec_docker).is_some();

    let wasm_allowed = cap.is_allowed("wasm");
    let runner_ok = cap.runner.ok;
    let runner_note = cap.runner.note.clone();
    let proc_allowed = cap.is_allowed("process");
    let cont_allowed = cap.is_allowed("container");

    cap.kinds = vec![
        Kind {
            id: "wasm",
            enabled: wasm_allowed && runner_ok,
            provider: format!("本机 HUR 沙箱（`{bin} hur run --exec`，限额由包内策略算）"),
            why: why_text(
                wasm_allowed,
                runner_ok,
                "wasm 不在 NCCR_EXEC_ALLOW 里",
                &runner_note,
            ),
        },
        Kind {
            id: "process",
            enabled: proc_allowed && shell_ok,
            provider: format!(
                "本机进程（`{} -c`）—— OS 敏感任务走这条：docker build / 编译 / apt",
                cfg.exec_shell
            ),
            why: why_text(
                proc_allowed,
                shell_ok,
                "process 需要运维显式放行：NCCR_EXEC_ALLOW=wasm,process",
                &format!("PATH 里找不到 {}", cfg.exec_shell),
            ),
        },
        Kind {
            id: "container",
            enabled: cont_allowed && docker_ok && shell_ok,
            provider: format!(
                "容器（`{} run --rm`，工作目录挂进 /work）—— 隔离更强；要构建镜像得自备 DinD",
                cfg.exec_docker
            ),
            why: why_text(
                cont_allowed && docker_ok,
                shell_ok,
                "container 需要运维显式放行：NCCR_EXEC_ALLOW=wasm,container",
                &format!("PATH 里找不到 {}", cfg.exec_docker),
            ),
        },
    ];
    cap
}

/// `exec.LookPath` 的最小版：可执行文件在不在。
fn which(bin: &str) -> Option<PathBuf> {
    let bin = bin.trim();
    if bin.is_empty() {
        return None;
    }
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return if is_executable(&p) { Some(p) } else { None };
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(bin);
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &StdPath) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(p: &StdPath) -> bool {
    p.is_file()
}

/* ================= 执行器（原 internal/execrun） ================= */

/// 这次要跑什么。
#[derive(Debug, Clone)]
pub struct Plan {
    pub engine: String,
    /// package | cmd
    pub kind: String,
    /// 子进程的 cwd。
    pub work_dir: PathBuf,
    /// 仅 kind=cmd。
    pub command: String,
    /// 仅 engine=container。
    pub image: String,
    /// 仅 engine=wasm：解包后的包根目录。
    pub package_dir: PathBuf,
}

impl Plan {
    /// 人可读的载荷描述（写进任务记录，列表里一眼看懂）。
    fn spec(&self) -> String {
        if self.kind == "package" {
            return self.package_dir.to_string_lossy().to_string();
        }
        if self.engine == "container" && !self.image.is_empty() {
            return format!("[{}] {}", self.image, self.command);
        }
        self.command.clone()
    }
}

/// 把 Plan 变成要执行的 argv。**引擎不认识 / 没放行 / 缺条件一律报错**，
/// 不做「尽力而为」的降级 —— 降级等于偷偷换一台机器或换一种隔离级别跑别人的东西。
fn build_argv(cap: &Capability, p: &Plan) -> Result<Vec<String>, String> {
    if !valid_exec_engine(&p.engine) {
        return Err(format!(
            "不认识的引擎 \"{}\"（可选：{}）",
            p.engine,
            EXEC_ENGINES.join("/")
        ));
    }
    if !cap.is_allowed(&p.engine) {
        return Err(format!(
            "这台机器没有放行 {} 引擎（运维要显式打开：NCCR_EXEC_ALLOW 加上 {}）",
            p.engine, p.engine
        ));
    }
    if !cap.is_enabled(&p.engine) {
        return Err(format!(
            "{} 引擎现在不可用：{}",
            p.engine,
            cap.why_of(&p.engine)
        ));
    }
    match p.engine.as_str() {
        "wasm" => {
            if p.package_dir.as_os_str().is_empty() {
                return Err("wasm 引擎要一个包（--package 或在 body 里传 .hur）".to_string());
            }
            Ok(vec![
                cap.runner.path.clone(),
                "hur".to_string(),
                "run".to_string(),
                p.package_dir.to_string_lossy().to_string(),
                "--exec".to_string(),
                "--json".to_string(),
            ])
        }
        "process" => {
            if p.command.trim().is_empty() {
                return Err("process 引擎要一条命令".to_string());
            }
            Ok(vec![cap.shell.clone(), "-c".to_string(), p.command.clone()])
        }
        "container" => {
            if p.command.trim().is_empty() || p.image.trim().is_empty() {
                return Err("container 引擎要 --image 与一条命令".to_string());
            }
            if !cap.images.is_empty() && !cap.images.iter().any(|i| i == &p.image) {
                return Err(format!(
                    "镜像 \"{}\" 不在 NCCR_EXEC_IMAGES 允许清单里",
                    p.image
                ));
            }
            Ok(vec![
                cap.docker.clone(),
                "run".to_string(),
                "--rm".to_string(),
                "-v".to_string(),
                format!("{}:/work", p.work_dir.to_string_lossy()),
                "-w".to_string(),
                "/work".to_string(),
                p.image.clone(),
                "sh".to_string(),
                "-c".to_string(),
                p.command.clone(),
            ])
        }
        _ => Err(format!("引擎 {} 还没接上", p.engine)),
    }
}

/// 一次执行的结果。
struct ExecResult {
    status: &'static str,
    exit_code: i32,
    log_bytes: i64,
    truncated: bool,
    err: Option<String>,
}

impl ExecResult {
    fn failed(msg: impl Into<String>) -> Self {
        Self {
            status: store::exec::EXEC_FAILED,
            exit_code: 0,
            log_bytes: 0,
            truncated: false,
            err: Some(msg.into()),
        }
    }
}

/// 子进程环境：**白名单**，绝不继承服务端 env（那里面有 JWT secret、库路径、blob 目录）。
fn child_env(p: &Plan) -> Vec<(String, String)> {
    let work = p.work_dir.to_string_lossy().to_string();
    let lang = match std::env::var("LANG") {
        Ok(v) if !v.trim().is_empty() => v,
        _ => "C.UTF-8".to_string(),
    };
    vec![
        (
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        ),
        ("HOME".to_string(), work.clone()),
        (
            "TMPDIR".to_string(),
            p.work_dir.join(".tmp").to_string_lossy().to_string(),
        ),
        ("LANG".to_string(), lang),
        ("NCC_EXEC_ENGINE".to_string(), p.engine.clone()),
        ("NCC_EXEC_WORKDIR".to_string(), work),
    ]
}

/// 写满上限后停止写入，但记下真实累计量与被截断这件事。
///
/// 超出上限时**对外仍报"写成功"**：否则子进程会拿到 EPIPE 而异常退出 ——
/// 那是我们限流造成的，不该让它看起来像任务失败。
struct LimitWriter {
    f: std::fs::File,
    limit: i64,
    n: i64,
    hit: bool,
}

impl LimitWriter {
    fn write_chunk(&mut self, b: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        let before = self.n;
        self.n += b.len() as i64;
        let left = self.limit - before;
        if left <= 0 {
            self.hit = true;
            return Ok(());
        }
        let chunk = if b.len() as i64 > left {
            self.hit = true;
            &b[..left as usize]
        } else {
            b
        };
        self.f.write_all(chunk)
    }
}

/// 把一条管道里的字节灌进日志（stdout / stderr 合流）。
async fn pump<R: tokio::io::AsyncRead + Unpin>(mut r: R, lw: Arc<Mutex<LimitWriter>>) {
    let mut buf = vec![0u8; 16 << 10];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut g = lw.lock().unwrap();
                if g.write_chunk(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

/// 给**进程组**发信号（负 pid）。
///
/// 只杀直接子进程是不够的：`sh -c "docker build …"` 被杀掉之后，docker 客户端
/// 还活着（甚至继续推送镜像）—— 那等于「取消了但没停」。
#[cfg(unix)]
fn signal_group(pid: i32, sig: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        let _ = kill(-pid, sig);
    }
}

/// 先 TERM（让子进程有机会收尾），宽限期后 KILL。
///
/// 与 Go 的 `killGroup` 的差别：那边直接 SIGKILL。宽限一步是刻意的 ——
/// 超时/取消不该让 `docker build` 半途留下半个构建缓存，能收尾就让它收尾。
async fn terminate_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            signal_group(pid as i32, SIGTERM);
        }
        if tokio::time::timeout(KILL_GRACE, child.wait())
            .await
            .is_err()
        {
            if let Some(pid) = child.id() {
                signal_group(pid as i32, SIGKILL);
            }
            let _ = child.wait().await;
        }
    }
    #[cfg(not(unix))]
    {
        // Windows 没有进程组语义（要 Job Object 才能整树回收）——只杀直接子进程。
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// 真的跑。日志：stdout+stderr 合流写 `log_path`，超过 `max_output` 后**继续跑但不再写**
/// （`truncated = true`）。
async fn run_plan(
    cap: &Capability,
    plan: &Plan,
    log_path: &StdPath,
    timeout: Duration,
    mut cancel: watch::Receiver<bool>,
) -> ExecResult {
    let argv = match build_argv(cap, plan) {
        Ok(a) => a,
        Err(e) => return ExecResult::failed(e),
    };
    if let Some(dir) = log_path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return ExecResult::failed(e.to_string());
        }
    }
    let file = match std::fs::File::create(log_path) {
        Ok(f) => f,
        Err(e) => return ExecResult::failed(e.to_string()),
    };
    let lw = Arc::new(Mutex::new(LimitWriter {
        f: file,
        limit: cap.max_output,
        n: 0,
        hit: false,
    }));

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(&plan.work_dir);
    cmd.env_clear();
    for (k, v) in child_env(plan) {
        cmd.env(k, v);
    }
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    // 父进程意外退出时别把子进程留成孤儿
    cmd.kill_on_drop(true);
    #[cfg(unix)]
    {
        // 自成进程组：取消 / 超时才能整组回收
        cmd.process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ExecResult::failed(e.to_string()),
    };

    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(tokio::spawn(pump(out, lw.clone())));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(tokio::spawn(pump(err, lw.clone())));
    }

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let mut wait_res: Option<std::io::Result<std::process::ExitStatus>> = None;
    let mut timed_out = false;
    let mut canceled = false;

    tokio::select! {
        r = child.wait() => { wait_res = Some(r); }
        _ = &mut deadline => { timed_out = true; }
        _ = cancel.changed() => { canceled = true; }
    }

    // 超时 / 取消：整组收掉，再把读取端排空（日志前半段要留住）
    if timed_out || canceled {
        terminate_group(&mut child).await;
        for h in pumps {
            let _ = h.await;
        }
        let (n, hit) = {
            let g = lw.lock().unwrap();
            (g.n, g.hit)
        };
        let (status, err) = if timed_out {
            (store::exec::EXEC_TIMEOUT, "context deadline exceeded")
        } else {
            (store::exec::EXEC_CANCELED, "context canceled")
        };
        return ExecResult {
            status,
            exit_code: -1,
            log_bytes: n,
            truncated: hit,
            err: Some(err.to_string()),
        };
    }

    for h in pumps {
        let _ = h.await;
    }
    let (n, hit) = {
        let g = lw.lock().unwrap();
        (g.n, g.hit)
    };
    match wait_res {
        Some(Ok(st)) => {
            let code = st.code().unwrap_or(-1);
            ExecResult {
                status: if code == 0 {
                    store::exec::EXEC_SUCCEEDED
                } else {
                    store::exec::EXEC_FAILED
                },
                exit_code: code,
                log_bytes: n,
                truncated: hit,
                err: None,
            }
        }
        Some(Err(e)) => ExecResult {
            status: store::exec::EXEC_FAILED,
            exit_code: -1,
            log_bytes: n,
            truncated: hit,
            err: Some(e.to_string()),
        },
        None => ExecResult::failed("进程没有回执"),
    }
}

/* ================= 在跑任务表（DELETE 要能真停） ================= */

/// 进程内的在跑任务表（重启即清空 —— 重启后 `running` 的行会被
/// `store::exec::stale` 标成 failed）。
///
/// 值是一个 watch 通道：DELETE 往里面发 `true`，执行器收到就走「取消」那条路
/// （整组回收），而不是只把状态改掉。
fn jobs() -> &'static Mutex<HashMap<String, watch::Sender<bool>>> {
    static JOBS: OnceLock<Mutex<HashMap<String, watch::Sender<bool>>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn job_set(id: &str, tx: watch::Sender<bool>) {
    jobs().lock().unwrap().insert(id.to_string(), tx);
}

fn job_cancel(id: &str) -> bool {
    match jobs().lock().unwrap().get(id) {
        Some(tx) => tx.send(true).is_ok(),
        None => false,
    }
}

fn job_drop(id: &str) {
    jobs().lock().unwrap().remove(id);
}

/// 真正跑一条任务：登记 → 跑 → 落终态。同步 / 异步两个入口共用这一份实现
/// （**别写两遍**：限额、环境变量白名单、进程组回收都在这条路上）。
pub(crate) async fn exec_one(
    state: AppState,
    rec: store::exec::ExecRunRow,
    plan: Plan,
    log_path: PathBuf,
) {
    let (tx, rx) = watch::channel(false);
    job_set(&rec.id, tx);
    let _ = store::exec::mark_running(state.pool(), &rec.id).await;

    let cap = probe(state.cfg());
    // 每条任务用它**自己那条**墙上限，不是节点上限（Go 的 execOne 收到的 timeout 就是
    // 提交时算好并写进 rec.TimeoutSec 的那个值）。错用节点上限会让 `timeoutSec: 2` 的
    // `sleep 30` 一直跑到自然结束（冒烟第 5 节抓的就是这个）。
    let timeout = if rec.timeout_sec > 0 {
        Duration::from_secs(rec.timeout_sec as u64)
    } else {
        // 老库里的行可能没写 timeoutSec：退回节点上限，别让它变成「上来就超时」。
        Duration::from_millis(cap.timeout_ms.max(0) as u64)
    };
    let res = run_plan(&cap, &plan, &log_path, timeout, rx).await;

    let mut err_msg = String::new();
    if let Some(e) = &res.err {
        err_msg = e.clone();
        if res.status == store::exec::EXEC_TIMEOUT {
            err_msg = format!("超过墙上限 {} 被终止", go_duration(timeout));
        }
    }
    // finish 会在「已被取消」时拒绝覆盖（取消是用户意志，不该被后到的完成事件改回去）。
    if let Err(e) = store::exec::finish(
        state.pool(),
        &rec.id,
        res.status,
        res.exit_code as i64,
        res.log_bytes,
        res.truncated,
        &err_msg,
    )
    .await
    {
        tracing::error!("exec {} 落终态失败（可能已被取消）：{e}", rec.id);
    }
    job_drop(&rec.id);
}

/// Go 的 `time.Duration.String()` 的整秒形态（`5s` / `1m0s` / `1h30m0s`）。
fn go_duration(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

/// 启动时收尾：把「上次进程死掉时还在跑」的任务标成 failed。
///
/// `main.rs` 属本批次不让改的文件，所以这里只导出、由整体收口的 agent 接线
/// （与 Go 的 `staleExecRuns()` 同一位置）。
#[allow(dead_code)]
pub async fn stale_exec_runs(state: &AppState) {
    match store::exec::stale(state.pool()).await {
        Ok(n) if n > 0 => tracing::warn!("收尾 {n} 条中断的远程执行任务（节点重启）"),
        Ok(_) => {}
        Err(e) => tracing::error!("收尾中断任务失败: {e}"),
    }
}

/* ================= 接活 ================= */

/// 接活的 JSON 载荷。`kind`（`cmd`/`package`）由载荷形态自己决定
/// （上传 .hur = package，JSON = cmd），所以这里不接它 —— serde 会照常忽略未知字段，
/// 与 Go 的行为一致（Go 的 `execCreateReq.Kind` 也是从头到尾没被读过）。
#[derive(Debug, Default, Deserialize)]
struct ExecCreateReq {
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    engine: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    cmd: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    image: String,
    #[serde(default, deserialize_with = "crate::httpapi::helpers::de_str")]
    reason: String,
    #[serde(
        default,
        rename = "timeoutSec",
        deserialize_with = "crate::httpapi::helpers::de_or_default"
    )]
    timeout: i64,
}

/// GET /api/exec/kinds —— **这台机器到底能跑什么**（公开，不鉴权）。
async fn exec_kinds(State(state): State<AppState>) -> ApiResult<Response> {
    let cap = probe(state.cfg());
    let any_on = cap.kinds.iter().any(|k| k.enabled);
    let cfg = state.cfg();
    Ok(helpers::ok_json(json!({
        "ok": true,
        "node": {
            "id": cfg.node_id, "name": cfg.node_name,
            "region": cfg.node_region, "url": cfg.public_url,
        },
        "exec": cap.to_json(),
        "limits": {
            "timeoutMs": cap.timeout_ms,
            "maxOutputBytes": cap.max_output,
            "maxUploadBytes": MAX_EXEC_UPLOAD,
        },
        "anyEnabled": any_on,
        "note": "engine=wasm 跑 HUR 包（沙箱，限额由包内策略算）；process/container 跑一条命令（OS 敏感任务，例如 docker build && docker push），必须运维显式放行（NCCR_EXEC_ALLOW）—— 默认只放行 wasm。",
    })))
}

/// POST /api/exec/runs —— 接一个任务。
///
/// 两种载荷：multipart `file`（.hur 包 → wasm 引擎）或 JSON（`engine` + `cmd`[+`image`]）；
/// 另外认 `application/octet-stream`（`ncc sandbox run --package` 走这条）。
async fn create_exec_run(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth
        .info()
        .ok_or_else(|| {
            ApiError::unauthorized(
                "未认证或凭据无效（先 ncc login / ncc sandbox init，或带 API-KEY）",
            )
        })?
        .clone();
    let cap = probe(state.cfg());
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let mut req = ExecCreateReq::default();
    let mut payload: Vec<u8> = Vec::new();
    let mut pkg = false;

    if content_type.starts_with("multipart/form-data") {
        let boundary = boundary_of(&content_type);
        let parts = parse_multipart(&body, &boundary);
        let Some(file) = parts.iter().find(|(n, _, _)| n == "file") else {
            return Err(ApiError::bad_request(
                "bad_request",
                "缺少 file 字段（或在 JSON body 里给 cmd）",
            ));
        };
        if file.2.len() as i64 > MAX_EXEC_UPLOAD {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "包超过 32MB",
            ));
        }
        payload = file.2.clone();
        let field = |k: &str| -> String {
            parts
                .iter()
                .find(|(n, _, _)| n == k)
                .map(|(_, _, c)| String::from_utf8_lossy(c).to_string())
                .unwrap_or_default()
        };
        req.engine = first_non_empty(&[field("engine"), "wasm".to_string()]);
        req.reason = field("reason");
        req.timeout = atoi_default(&field("timeoutSec"), 0);
        pkg = true;
    } else if content_type.starts_with("application/octet-stream") {
        // 客户端（`ncc sandbox run --package`）走这条：原样字节 + 查询串。
        // 为什么不做 multipart：CLI 的 HTTP 层只支持 JSON 或原样字节两形态。
        payload = body.to_vec();
        req.engine = first_non_empty(&[
            web::query(&uri, "engine").unwrap_or_default(),
            "wasm".to_string(),
        ]);
        req.reason = web::query(&uri, "reason").unwrap_or_default();
        req.timeout = atoi_default(&web::query(&uri, "timeoutSec").unwrap_or_default(), 0);
        pkg = true;
    } else {
        req = serde_json::from_slice(&body).map_err(|_| {
            ApiError::bad_request("bad_request", "请求体格式错误（JSON 需要 engine + cmd）")
        })?;
        req.engine = first_non_empty(&[req.engine.clone(), "process".to_string()]);
    }

    if req.engine.trim().is_empty() {
        req.engine = "process".to_string();
    }
    if !valid_exec_engine(&req.engine) {
        return Err(ApiError::bad_request(
            "bad_request",
            format!("不认识的引擎（可选：{}）", EXEC_ENGINES.join("/")),
        ));
    }
    // ⚠️ reason 必填：与 R12 同一条规矩（写操作必须说明为什么）。
    if req.reason.trim().is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "缺少 reason：这台机器替谁跑、为什么跑，要留在账本里",
        ));
    }
    // 放行检查放在最前面：没放行的引擎**连字节都不收**。
    if !cap.is_allowed(&req.engine) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "engine_not_allowed",
            format!(
                "这台机器没有放行 {} 引擎（运维要显式打开：NCCR_EXEC_ALLOW 加上它）",
                req.engine
            ),
        ));
    }
    if !cap.is_enabled(&req.engine) {
        return Err(ApiError::conflict(
            "engine_unavailable",
            format!("{} 引擎当前不可用：{}", req.engine, cap.why_of(&req.engine)),
        ));
    }

    let mut timeout = state.cfg().exec_timeout;
    if req.timeout > 0 {
        let want = Duration::from_secs(req.timeout as u64);
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

    let run_id = ncc_core::ids::new_id("ER");
    let work = state.cfg().exec_dir.join(&run_id);
    if let Err(e) = mkdir_mode(&work.join(".tmp"), 0o755) {
        tracing::error!("创建任务工作目录失败 {}: {e}", work.display());
        return Err(ApiError::internal("创建工作目录失败"));
    }
    let mut plan = Plan {
        engine: req.engine.clone(),
        kind: String::new(),
        work_dir: work.clone(),
        command: String::new(),
        image: String::new(),
        package_dir: PathBuf::new(),
    };

    if pkg {
        if req.engine != "wasm" {
            return Err(ApiError::bad_request(
                "bad_request",
                "上传 .hur 只能配 wasm 引擎；要跑命令请用 --cmd + --engine",
            ));
        }
        if payload.is_empty() {
            return Err(ApiError::bad_request("bad_request", "上传内容为空"));
        }
        let pkg_dir = work.join("pkg");
        if let Err(e) = unpack_hur(&payload, &pkg_dir) {
            let _ = std::fs::remove_dir_all(&work);
            return Err(ApiError::bad_request(
                "bad_request",
                format!("不是一个能解开的 hur 包：{e}"),
            ));
        }
        plan.kind = "package".to_string();
        plan.package_dir = pkg_dir;
    } else {
        if req.cmd.trim().is_empty() {
            return Err(ApiError::bad_request(
                "bad_request",
                "缺少 cmd（或上传一个 .hur 包）",
            ));
        }
        plan.kind = "cmd".to_string();
        plan.command = req.cmd.clone();
        plan.image = req.image.trim().to_string();
    }

    let log_path = work.join("output.log");
    let rec = match store::exec::create(
        state.pool(),
        store::exec::ExecRunInput {
            id: run_id,
            owner_id: a.user_id.clone(),
            requested_by: a.email.clone(),
            engine: req.engine.clone(),
            kind: plan.kind.clone(),
            spec: clamp_runes(&plan.spec(), 400),
            image: plan.image.clone(),
            work_dir: work.to_string_lossy().to_string(),
            log_path: log_path.to_string_lossy().to_string(),
            reason: clamp_runes(&req.reason, MAX_EXEC_REASON),
            timeout_sec: timeout.as_secs() as i64,
            ..Default::default()
        },
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("创建任务失败: {e}");
            let _ = std::fs::remove_dir_all(&work);
            return Err(ApiError::internal("创建任务失败"));
        }
    };

    let actor = shares::ensure_admin(&state, &auth, &headers).await;
    shares::audit(
        &state,
        actor.as_ref(),
        "exec.submit",
        &rec.id,
        &rec.spec,
        "提交远程执行任务",
        json!({
            "engine": req.engine, "kind": rec.kind, "reason": rec.reason,
            "timeoutSec": rec.timeout_sec,
        }),
        &web::client_ip(&headers),
    )
    .await;

    // 提交即返回：执行在后台跑，状态一路如实落库。
    let bg_state = state.clone();
    let bg_rec = rec.clone();
    tokio::spawn(async move {
        exec_one(bg_state, bg_rec, plan, log_path).await;
    });

    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({"run": exec_json(&rec), "kindsUrl": "/api/exec/kinds"}),
    ))
}

/// GET /api/exec/runs —— 我的任务（管理员可 `?all=1` 看全部）。
async fn list_exec_runs(
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
                "查看全部任务需要节点管理员身份",
            ));
        }
        owner = String::new();
    } else if owner.is_empty() {
        return Err(ApiError::unauthorized(
            "未认证或凭据无效（先 ncc login / ncc sandbox init）",
        ));
    }
    let (limit, offset) = shares::page_params(&uri);
    let rows = store::exec::list(state.pool(), &owner, limit, offset)
        .await
        .map_err(ApiError::from_db)?;
    let total = store::exec::count(state.pool(), &owner).await.unwrap_or(0);
    let list: Vec<Value> = rows.iter().map(exec_json).collect();
    Ok(helpers::ok_json(json!({
        "runs": list, "total": total, "limit": limit, "offset": offset,
    })))
}

/// GET /api/exec/runs/{id} —— 状态（带日志尾巴，省一次请求）。
async fn get_exec_run(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let rec = exec_for(&state, &auth, &headers, &id).await?;
    Ok(helpers::ok_json(json!({"run": exec_json_tail(&rec)})))
}

/// GET /api/exec/runs/{id}/log —— 日志全文（text/plain）。
///
/// 日志是任务的主要产物之一：CI 里 `curl` 一下就拿到，不必解析 JSON。
async fn exec_run_log(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let rec = exec_for(&state, &auth, &headers, &id).await?;
    let data = match std::fs::read(&rec.log_path) {
        Ok(b) => b,
        Err(_) => {
            return Ok((
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "日志不存在（任务可能还没开始，或被清理了）",
            )
                .into_response());
        }
    };
    let mut out = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("X-NCC-Exec-Status", &rec.status)
        .header("X-NCC-Exec-Exit", rec.exit_code.to_string());
    if rec.log_truncated {
        out = out.header("X-NCC-Exec-Truncated", "1");
    }
    Ok(out.body(Body::from(data)).unwrap())
}

/// DELETE /api/exec/runs/{id} —— 取消（还在跑）或删除（已结束，`?purge=1` 连工作目录）。
async fn exec_run_action(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    Path(id): Path<String>,
    uri: Uri,
) -> ApiResult<Response> {
    let rec = store::exec::find(state.pool(), &id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("任务不存在"))?;
    // 管理员可以处置任何任务（治理面）；普通用户只能动自己的。
    let admin = shares::ensure_admin(&state, &auth, &headers)
        .await
        .is_some();
    let mine = auth
        .info()
        .map(|a| a.user_id == rec.owner_id)
        .unwrap_or(false);
    if !admin && !mine {
        return Err(ApiError::forbidden("只能处置自己提交的任务"));
    }

    if rec.done() {
        if web::query(&uri, "purge").as_deref() == Some("1") {
            let _ = std::fs::remove_dir_all(&rec.work_dir);
            store::exec::delete(state.pool(), &rec.id, "")
                .await
                .map_err(ApiError::from_db)?;
            return Ok(helpers::ok_json(
                json!({"ok": true, "id": rec.id, "deleted": true}),
            ));
        }
        return Ok(helpers::ok_json(json!({
            "ok": true, "id": rec.id, "status": rec.status,
            "note": "任务已结束，无需取消（?purge=1 可连日志一起删）",
        })));
    }

    // 还在跑：先让执行器收掉进程组，再把状态置为 canceled（顺序不能反 ——
    // 反了的话完成事件可能抢在前面把状态改成 succeeded）。
    job_cancel(&rec.id);
    let canceled = store::exec::cancel(state.pool(), &rec.id, "")
        .await
        .map_err(ApiError::from_db)?;
    let actor = shares::ensure_admin(&state, &auth, &headers).await;
    shares::audit(
        &state,
        actor.as_ref(),
        "exec.cancel",
        &rec.id,
        &rec.spec,
        "取消远程执行任务",
        json!({"canceled": canceled}),
        &web::client_ip(&headers),
    )
    .await;
    Ok(helpers::ok_json(
        json!({"ok": true, "id": rec.id, "canceled": canceled}),
    ))
}

/// 任务详情的守卫：发起者或管理员，其余 403。
async fn exec_for(
    state: &AppState,
    auth: &Auth,
    headers: &HeaderMap,
    id: &str,
) -> ApiResult<store::exec::ExecRunRow> {
    let rec = store::exec::find(state.pool(), id)
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("任务不存在"))?;
    if auth
        .info()
        .map(|a| a.user_id == rec.owner_id)
        .unwrap_or(false)
    {
        return Ok(rec);
    }
    if shares::ensure_admin(state, auth, headers).await.is_some() {
        return Ok(rec);
    }
    // 任务详情里有日志（可能含敏感信息）→ 只有发起者与管理员能看。
    Err(ApiError::forbidden(
        "只能看自己提交的任务（管理员可看全部）",
    ))
}

/* ================= 视图 ================= */

/// 任务视图。
pub(crate) fn exec_json(r: &store::exec::ExecRunRow) -> Value {
    let mut out = json!({
        "id": r.id, "engine": r.engine, "kind": r.kind, "spec": r.spec,
        "status": r.status, "exitCode": r.exit_code,
        "reason": r.reason, "timeoutSec": r.timeout_sec,
        "durationMs": r.duration_ms(),
        "logBytes": r.log_bytes, "logTruncated": r.log_truncated,
        "logUrl": format!("/api/exec/runs/{}/log", r.id),
        "workDir": r.work_dir,
        "startedAt": r.started_at, "finishedAt": r.finished_at, "createdAt": r.created_at,
    });
    if !r.error.is_empty() {
        out["error"] = json!(r.error);
    }
    if !r.conn_id.is_empty() {
        out["connId"] = json!(r.conn_id);
    }
    if !r.image.is_empty() {
        out["image"] = json!(r.image);
    }
    if !r.package_id.is_empty() {
        out["package"] = json!({
            "id": r.package_id, "version": r.package_version, "sha256": r.package_sha,
        });
    }
    out
}

/// `exec_json` + 日志尾巴。状态接口和「通道上的同步执行」共用**同一份**
/// （后者刚跑完就该把结果一次交出去 —— 只回一个 task id 等于逼客户端再发一次请求）。
pub fn exec_json_tail(r: &store::exec::ExecRunRow) -> Value {
    let mut out = exec_json(r);
    if !r.log_path.is_empty() {
        if let Some((tail, cut)) = read_tail(StdPath::new(&r.log_path), EXEC_LOG_TAIL_MAX) {
            out["logTail"] = json!(tail);
            out["logTailTruncated"] = json!(cut);
        }
    }
    out
}

/// 读文件最后 n 字节（日志尾巴）。返回内容与「前面还有更多」。
fn read_tail(path: &StdPath, n: u64) -> Option<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let size = f.metadata().ok()?.len();
    let (off, cut) = if size > n {
        (size - n, true)
    } else {
        (0, false)
    };
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut buf = vec![0u8; n as usize];
    let mut got = 0usize;
    while got < buf.len() {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(_) => return None,
        }
    }
    buf.truncate(got);
    Some((String::from_utf8_lossy(&buf).to_string(), cut))
}

/* ================= 解包（.hur） ================= */

/// 把 `.hur`（gzip 包住的 zip，或裸 zip）解到 `dest`。
///
/// ⚠️ 防目录穿越：只接受相对路径、且拒绝任何 `..` 段 —— 这是别人上传的字节，
/// 解包写盘是唯一一处「上传的内容变成了这台机器上的文件」，必须按不可信处理。
fn unpack_hur(raw: &[u8], dest: &StdPath) -> Result<(), String> {
    let owned;
    let zip_bytes: &[u8] = if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        owned = gunzip(raw)?;
        &owned
    } else {
        raw
    };
    let entries = zip_entries(zip_bytes)?;
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    for e in entries {
        let target = safe_zip_target(dest, &e.name)?;
        if e.name.ends_with('/') {
            std::fs::create_dir_all(&target).map_err(|x| x.to_string())?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|x| x.to_string())?;
        }
        let end = e.data_off + e.csize;
        if end > zip_bytes.len() {
            return Err("zip 条目越界".to_string());
        }
        let chunk = &zip_bytes[e.data_off..end];
        let data = match e.method {
            0 => chunk.to_vec(),
            8 => inflate(chunk, MAX_ENTRY_BYTES)?,
            _ => return Err("zip: 不支持的压缩方式".to_string()),
        };
        // 单个条目最多 64MB（与 Go 的 `io.Copy(…, LimitReader(rc, 64<<20))` 同档）
        let cut = data.len().min(MAX_ENTRY_BYTES);
        std::fs::write(&target, &data[..cut]).map_err(|x| x.to_string())?;
    }
    Ok(())
}

/// 包内路径 → 落在 `dest` 里的绝对路径；越界一律拒。
fn safe_zip_target(dest: &StdPath, name: &str) -> Result<PathBuf, String> {
    if name.contains("..") || name.starts_with('/') || name.starts_with('\\') {
        return Err(format!("包里有不安全的路径：{name}"));
    }
    let clean = clean_slash_path(name);
    let rel = clean.trim_start_matches('/');
    let target = if rel.is_empty() {
        dest.to_path_buf()
    } else {
        dest.join(rel)
    };
    let dest_s = dest.to_string_lossy().to_string();
    let target_s = target.to_string_lossy().to_string();
    let prefix = format!("{dest_s}{}", std::path::MAIN_SEPARATOR);
    if !target_s.starts_with(&prefix) {
        return Err(format!("包里有越界路径：{name}"));
    }
    Ok(target)
}

/// 与 Go 的 `filepath.Clean("/"+name)` 等价（归一 `.` `..`，去掉结尾 `/`）。
fn clean_slash_path(name: &str) -> String {
    let normalized = name.replace('\\', "/");
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

/* ---- 最小 DEFLATE / gzip / zip 读取 ----
 *
 * Go 用标准库 `compress/gzip` + `archive/zip`；本 crate 不能加依赖，
 * 所以手写这一段（与 `httpapi/agentcards.rs` 里读名片清单的那份同源）。
 * 只认 zip 的「存储 / deflate」两种方式与单成员 gzip —— `.hur` 就是这个形态。
 */

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn bit(&mut self) -> Result<u32, String> {
        let byte = self.bit / 8;
        if byte >= self.data.len() {
            return Err("比特流提前结束".to_string());
        }
        let b = (self.data[byte] >> (self.bit % 8)) & 1;
        self.bit += 1;
        Ok(b as u32)
    }

    fn bits(&mut self, n: u32) -> Result<u32, String> {
        let mut v = 0u32;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }

    fn align(&mut self) {
        self.bit = self.bit.div_ceil(8) * 8;
    }

    fn take_byte(&mut self) -> Result<u8, String> {
        let byte = self.bit / 8;
        if byte >= self.data.len() {
            return Err("比特流提前结束".to_string());
        }
        self.bit += 8;
        Ok(self.data[byte])
    }
}

/// 规范哈夫曼表（构造与解码按 puff.c 的做法）。
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Self {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if l > 0 && (l as usize) < 16 {
                counts[l as usize] += 1;
            }
        }
        let mut offs = [0u16; 16];
        for l in 1..16 {
            offs[l] = offs[l - 1] + counts[l - 1];
        }
        let mut symbols = vec![0u16; lengths.iter().filter(|&&l| l > 0).count()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l > 0 && (l as usize) < 16 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Self { counts, symbols }
    }

    fn decode(&mut self, br: &mut BitReader) -> Result<u16, String> {
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for len in 1..16usize {
            code |= br.bit()? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                let i = (index + code - first) as usize;
                return self
                    .symbols
                    .get(i)
                    .copied()
                    .ok_or_else(|| "哈夫曼码越界".to_string());
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("无效的哈夫曼码".to_string())
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_trees() -> (Huffman, Huffman) {
    let mut lit = vec![0u8; 288];
    for (i, v) in lit.iter_mut().enumerate() {
        *v = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    (Huffman::new(&lit), Huffman::new(&[5u8; 30]))
}

fn dynamic_trees(br: &mut BitReader) -> Result<(Huffman, Huffman), String> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let hlit = br.bits(5)? as usize + 257;
    let hdist = br.bits(5)? as usize + 1;
    let hclen = br.bits(4)? as usize + 4;
    if hlit > 288 || hdist > 32 {
        return Err("动态哈夫曼头越界".to_string());
    }
    let mut cl = [0u8; 19];
    for i in 0..hclen {
        cl[ORDER[i]] = br.bits(3)? as u8;
    }
    let mut cl_tree = Huffman::new(&cl);
    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0usize;
    while i < lengths.len() {
        let sym = cl_tree.decode(br)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                if i == 0 {
                    return Err("重复码没有可复制的前一个长度".to_string());
                }
                let prev = lengths[i - 1];
                let n = 3 + br.bits(2)? as usize;
                if i + n > lengths.len() {
                    return Err("码长重复越界".to_string());
                }
                for _ in 0..n {
                    lengths[i] = prev;
                    i += 1;
                }
            }
            17 | 18 => {
                let n = if sym == 17 {
                    3 + br.bits(3)? as usize
                } else {
                    11 + br.bits(7)? as usize
                };
                if i + n > lengths.len() {
                    return Err("码长重复越界".to_string());
                }
                i += n;
            }
            _ => return Err("码长码无效".to_string()),
        }
    }
    Ok((
        Huffman::new(&lengths[..hlit]),
        Huffman::new(&lengths[hlit..]),
    ))
}

fn inflate_block(
    br: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &mut Huffman,
    dist: &mut Huffman,
    limit: usize,
) -> Result<(), String> {
    loop {
        let sym = lit.decode(br)?;
        if sym < 256 {
            if out.len() >= limit {
                return Err("解压后超过上限".to_string());
            }
            out.push(sym as u8);
            continue;
        }
        if sym == 256 {
            return Ok(());
        }
        let idx = (sym - 257) as usize;
        if idx >= LEN_BASE.len() {
            return Err("长度码无效".to_string());
        }
        let len = LEN_BASE[idx] as usize + br.bits(LEN_EXTRA[idx])? as usize;
        let dsym = dist.decode(br)? as usize;
        if dsym >= DIST_BASE.len() {
            return Err("距离码无效".to_string());
        }
        let back = DIST_BASE[dsym] as usize + br.bits(DIST_EXTRA[dsym])? as usize;
        if back == 0 || back > out.len() {
            return Err("距离越界".to_string());
        }
        if out.len() + len > limit {
            return Err("解压后超过上限".to_string());
        }
        let start = out.len() - back;
        for i in 0..len {
            let b = out[start + i];
            out.push(b);
        }
    }
}

/// 解一条 DEFLATE 流，最多产出 `limit` 字节。**不校验 CRC**：包内容不靠 CRC 判定，
/// 校验和只证明传输没错，不证明内容可信。
fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut br = BitReader::new(data);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = br.bit()? == 1;
        match br.bits(2)? {
            0 => {
                br.align();
                let len = br.bits(16)? as u16;
                let nlen = br.bits(16)? as u16;
                if len != !nlen {
                    return Err("存储块长度校验失败".to_string());
                }
                if out.len() + len as usize > limit {
                    return Err("解压后超过上限".to_string());
                }
                for _ in 0..len {
                    out.push(br.take_byte()?);
                }
            }
            1 => {
                let (mut lit, mut dist) = fixed_trees();
                inflate_block(&mut br, &mut out, &mut lit, &mut dist, limit)?;
            }
            2 => {
                let (mut lit, mut dist) = dynamic_trees(&mut br)?;
                inflate_block(&mut br, &mut out, &mut lit, &mut dist, limit)?;
            }
            _ => return Err("未知的 DEFLATE 块类型".to_string()),
        }
        if last {
            return Ok(out);
        }
    }
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(i)?,
        *b.get(i + 1)?,
        *b.get(i + 2)?,
        *b.get(i + 3)?,
    ]))
}

/// gzip 成员 → 明文（只认单成员，够用：`.hur` 就是「gzip 包住的 zip」）。
fn gunzip(raw: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "gzip 层打不开（不是有效的 .hur）".to_string();
    if raw.len() < 18 || raw[0] != 0x1f || raw[1] != 0x8b || raw[2] != 8 {
        return Err(bad());
    }
    let flg = raw[3];
    if flg & 0xe0 != 0 {
        return Err(bad());
    }
    let mut p = 10usize;
    if flg & 0x04 != 0 {
        let xlen = u16_at(raw, p).ok_or_else(bad)? as usize;
        p += 2 + xlen;
    }
    if flg & 0x08 != 0 {
        p = skip_cstr(raw, p).ok_or_else(bad)?;
    }
    if flg & 0x10 != 0 {
        p = skip_cstr(raw, p).ok_or_else(bad)?;
    }
    if flg & 0x02 != 0 {
        p += 2;
    }
    if p >= raw.len() {
        return Err(bad());
    }
    inflate(&raw[p..], MAX_GUNZIP).map_err(|_| "gzip 层解压失败".to_string())
}

fn skip_cstr(raw: &[u8], from: usize) -> Option<usize> {
    let mut p = from;
    while p < raw.len() {
        if raw[p] == 0 {
            return Some(p + 1);
        }
        p += 1;
    }
    None
}

struct ZipEntry {
    name: String,
    method: u16,
    csize: usize,
    data_off: usize,
}

/// 按中央目录列出 zip 里的全部条目。
fn zip_entries(zip: &[u8]) -> Result<Vec<ZipEntry>, String> {
    let bad_zip = || "既不是 gzip 也不是 zip —— 这不是一个 hur 包".to_string();
    // EOCD 从尾部往前找（注释最长 65535）
    let floor = zip.len().saturating_sub(22 + 65535);
    let mut eocd = None;
    let mut p = zip.len().saturating_sub(22);
    loop {
        if u32_at(zip, p) == Some(0x0605_4b50) {
            eocd = Some(p);
            break;
        }
        if p == 0 || p <= floor {
            break;
        }
        p -= 1;
    }
    let eocd = eocd.ok_or_else(bad_zip)?;
    let count = u16_at(zip, eocd + 10).ok_or_else(bad_zip)? as usize;
    let cd = u32_at(zip, eocd + 16).ok_or_else(bad_zip)? as usize;
    let mut out = Vec::new();
    let mut p = cd;
    for _ in 0..count {
        if u32_at(zip, p) != Some(0x0201_4b50) {
            break;
        }
        let broken = || "中央目录损坏".to_string();
        let method = u16_at(zip, p + 10).ok_or_else(broken)?;
        let csize = u32_at(zip, p + 20).ok_or_else(broken)?;
        let usize_ = u32_at(zip, p + 24).ok_or_else(broken)?;
        let name_len = u16_at(zip, p + 28).ok_or_else(broken)? as usize;
        let extra_len = u16_at(zip, p + 30).ok_or_else(broken)? as usize;
        let comment_len = u16_at(zip, p + 32).ok_or_else(broken)? as usize;
        let local = u32_at(zip, p + 42).ok_or_else(broken)? as usize;
        let name_end = p + 46 + name_len;
        if name_end > zip.len() {
            break;
        }
        if csize == u32::MAX || usize_ == u32::MAX {
            return Err("zip64 暂不支持".to_string());
        }
        if u32_at(zip, local) != Some(0x0403_4b50) {
            return Err("本地文件头损坏".to_string());
        }
        let lname = u16_at(zip, local + 26).ok_or_else(broken)? as usize;
        let lextra = u16_at(zip, local + 28).ok_or_else(broken)? as usize;
        out.push(ZipEntry {
            name: String::from_utf8_lossy(&zip[p + 46..name_end]).to_string(),
            method,
            csize: csize as usize,
            data_off: local + 30 + lname + lextra,
        });
        p = p + 46 + name_len + extra_len + comment_len;
    }
    if out.is_empty() && count > 0 {
        // 有 EOCD 说有条目却一条也读不出来 = 中央目录是坏的
        return Err(bad_zip());
    }
    Ok(out)
}

/* ================= 小工具 ================= */

/// 极简 multipart/form-data 解析：只取字段名、文件名与内容。
///
/// Go 用标准库 `c.FormFile` / `c.PostForm`；本 crate 不能加依赖（axum 的
/// `multipart` feature 没开），所以手写这一小段 —— 够用即可：这一族只有
/// 「一个 file 字段 + 几个文本字段」这一种形态。
fn parse_multipart(body: &[u8], boundary: &str) -> Vec<(String, String, Vec<u8>)> {
    let delim = format!("--{boundary}");
    let mut out = Vec::new();
    for chunk in split_on(body, delim.as_bytes()) {
        let mut c = chunk;
        if let Some(rest) = c.strip_prefix(b"\r\n") {
            c = rest;
        }
        if c.starts_with(b"--") || c.is_empty() {
            continue;
        }
        let Some(i) = find_subslice(c, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&c[..i]).to_string();
        let mut content = &c[i + 4..];
        if let Some(rest) = content.strip_suffix(b"\r\n") {
            content = rest;
        }
        out.push((
            quoted_param(&head, "name").unwrap_or_default(),
            quoted_param(&head, "filename").unwrap_or_default(),
            content.to_vec(),
        ));
    }
    out
}

fn quoted_param(head: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let i = head.find(&needle)? + needle.len();
    let rest = &head[i..];
    let j = rest.find('"')?;
    Some(rest[..j].to_string())
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn split_on<'a>(data: &'a [u8], delim: &[u8]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while let Some(i) = find_subslice(&data[start..], delim) {
        out.push(&data[start..start + i]);
        start += i + delim.len();
    }
    out.push(&data[start..]);
    out
}

/// 从 `Content-Type` 里取 boundary（去掉引号）。
fn boundary_of(content_type: &str) -> String {
    content_type
        .split(';')
        .filter_map(|p| p.trim().strip_prefix("boundary="))
        .next()
        .map(|v| v.trim_matches('"').to_string())
        .unwrap_or_default()
}

/// Go 的 `firstNonEmpty`：逐项 trim，取第一个非空的。
pub(crate) fn first_non_empty(vals: &[String]) -> String {
    vals.iter()
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Go 的 `atoiDefault`：空串给默认值，出现任何非数字给默认值，超过 1e6 也给默认值。
fn atoi_default(s: &str, def: i64) -> i64 {
    let s = s.trim();
    if s.is_empty() {
        return def;
    }
    let mut n: i64 = 0;
    for ch in s.chars() {
        if !ch.is_ascii_digit() {
            return def;
        }
        n = n * 10 + (ch as i64 - '0' as i64);
        if n > 1_000_000 {
            return def;
        }
    }
    n
}

/// Go 的 `clampRunesCard`：按**字符**（不是字节）截断。
pub(crate) fn clamp_runes(s: &str, max: usize) -> String {
    let r: Vec<char> = s.trim().chars().collect();
    if r.len() > max {
        r[..max].iter().collect()
    } else {
        r.into_iter().collect()
    }
}

/// 建目录并设权限（Go 的 `os.MkdirAll(p, mode)`）。
#[cfg(unix)]
pub(crate) fn mkdir_mode(p: &StdPath, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(crate) fn mkdir_mode(p: &StdPath, _mode: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(p)
}

/* ================= 测试 ================= */

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use ncc_core::storage::LocalStorage;
    use tower::ServiceExt;

    /// 临时目录（落在系统临时目录里，按 pid + tag 隔离）。
    fn test_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ncc-exec-http-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 一个假 runner：能被 `which` 认出来（绝对路径 + 可执行位），跑起来就退出 0。
    fn fake_runner(dir: &StdPath) -> String {
        let p = dir.join("fake-ncc");
        std::fs::write(&p, "#!/bin/sh\necho fake-runner \"$@\"\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p.to_string_lossy().to_string()
    }

    async fn state(tag: &str, allow: &[&str], max_output: i64, timeout_secs: u64) -> AppState {
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
        cfg.exec_allow = allow.iter().map(|s| s.to_string()).collect();
        cfg.exec_max_output = max_output;
        cfg.exec_timeout = Duration::from_secs(timeout_secs);
        cfg.exec_runner = fake_runner(&dir);
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
            .layer(axum::extract::DefaultBodyLimit::max(256 << 20))
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

    fn req_json(method: &str, uri: &str, secret: &str, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        if !secret.is_empty() {
            b = b.header("authorization", format!("Bearer {secret}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn req_get(uri: &str, secret: &str) -> Request<Body> {
        let mut b = Request::builder().method("GET").uri(uri);
        if !secret.is_empty() {
            b = b.header("authorization", format!("Bearer {secret}"));
        }
        b.body(Body::empty()).unwrap()
    }

    /// 轮询任务状态直到落在 want 里。
    async fn wait_status(app: &Router, secret: &str, id: &str, want: &[&str]) -> Value {
        for _ in 0..100 {
            let (_, v) = call(app, req_get(&format!("/api/exec/runs/{id}"), secret)).await;
            let st = v["run"]["status"].as_str().unwrap_or("").to_string();
            if want.contains(&st.as_str()) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("任务没有在 5s 内到达 {want:?}");
    }

    /// 最小 zip（条目按「存储」写，不带 CRC —— 读取端不看 CRC，但**要看中央目录里的
    /// 压缩/原大小**，所以这两处必须写对）。
    fn zip_stored(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut cd: Vec<u8> = Vec::new();
        for (name, data) in entries {
            let offset = out.len() as u32;
            let nb = name.as_bytes();
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes()); // 需要的版本
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // 方式 = 存储
            out.extend_from_slice(&[0u8; 4]); // 时间 + 日期
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // 压缩大小
            out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // 原大小
            out.extend_from_slice(&(nb.len() as u16).to_le_bytes()); // 名字长度
            out.extend_from_slice(&0u16.to_le_bytes()); // 扩展区长度
            out.extend_from_slice(nb);
            out.extend_from_slice(data);

            cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes()); // 生成方版本
            cd.extend_from_slice(&20u16.to_le_bytes()); // 需要的版本
            cd.extend_from_slice(&0u16.to_le_bytes()); // flags
            cd.extend_from_slice(&0u16.to_le_bytes()); // 方式
            cd.extend_from_slice(&[0u8; 4]); // 时间 + 日期
            cd.extend_from_slice(&0u32.to_le_bytes()); // crc
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes()); // 压缩大小
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes()); // 原大小
            cd.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes()); // 扩展区
            cd.extend_from_slice(&0u16.to_le_bytes()); // 注释
            cd.extend_from_slice(&0u16.to_le_bytes()); // 起始盘
            cd.extend_from_slice(&0u16.to_le_bytes()); // 内部属性
            cd.extend_from_slice(&0u32.to_le_bytes()); // 外部属性
            cd.extend_from_slice(&offset.to_le_bytes());
            cd.extend_from_slice(nb);
        }
        let cd_offset = out.len() as u32;
        let cd_size = cd.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn gzip_stored(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
        out.push(0x01);
        let len = data.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(data);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out
    }

    fn req_bytes(uri: &str, secret: &str, body: Vec<u8>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/octet-stream");
        if !secret.is_empty() {
            b = b.header("authorization", format!("Bearer {secret}"));
        }
        b.body(Body::from(body)).unwrap()
    }

    /* ---- kinds ---- */

    #[tokio::test]
    async fn 能力自述_公开_默认只放行wasm() {
        let st = state("kinds", &["wasm"], 1 << 20, 600).await;
        let app = mk_app(&st);
        let (code, v) = call(&app, req_get("/api/exec/kinds", "")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["ok"], true);
        assert_eq!(v["node"]["id"], st.cfg().node_id);
        assert_eq!(v["node"]["url"], "http://localhost:8282");
        assert_eq!(v["limits"]["maxUploadBytes"], MAX_EXEC_UPLOAD);
        assert_eq!(v["limits"]["timeoutMs"], 600_000);
        assert_eq!(v["exec"]["allow"], json!(["wasm"]));
        let kinds = v["exec"]["kinds"].as_array().unwrap();
        assert_eq!(kinds.len(), 3);
        assert_eq!(kinds[0]["id"], "wasm");
        assert_eq!(kinds[1]["id"], "process");
        assert_eq!(kinds[2]["id"], "container");
        // 没放行的引擎一律不可用，且 why 指向 NCCR_EXEC_ALLOW
        assert_eq!(kinds[1]["enabled"], false);
        assert!(kinds[1]["why"]
            .as_str()
            .unwrap()
            .contains("NCCR_EXEC_ALLOW"));
        assert_eq!(kinds[2]["enabled"], false);
        assert!(kinds[2]["provider"].as_str().unwrap().contains("docker"));
        // 只有 wasm 放行、假 runner 在 → 它是可用的
        assert_eq!(kinds[0]["enabled"], true);
        assert_eq!(v["anyEnabled"], true);
        assert_eq!(v["exec"]["runner"]["ok"], true);
        assert!(v["note"].as_str().unwrap().contains("NCCR_EXEC_ALLOW"));
    }

    #[tokio::test]
    async fn 能力自述_runner不在时wasm不可用且说明原因() {
        let mut st = state("kinds-norunner", &["wasm"], 1 << 20, 600).await;
        let mut cfg = (*st.cfg()).clone();
        cfg.exec_runner = "/definitely/not/a/runner".to_string();
        st.cfg = Arc::new(cfg);
        let app = mk_app(&st);
        let (_, v) = call(&app, req_get("/api/exec/kinds", "")).await;
        assert_eq!(v["exec"]["kinds"][0]["enabled"], false);
        assert!(v["exec"]["kinds"][0]["why"]
            .as_str()
            .unwrap()
            .contains("PATH 里找不到"));
        assert_eq!(v["anyEnabled"], false);
    }

    /// 整站路由表装配：确认本族端点**真的挂上去了**（不是被 501 兜底接住）——
    /// axum 建路由时若有冲突会在这里直接 panic。
    #[tokio::test]
    async fn 整站路由表_本族端点已挂载() {
        let st = state("router", &["wasm"], 1 << 20, 600).await;
        let app = crate::router::build(&st);
        let (code, v) = call(&app, req_get("/api/exec/kinds", "")).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["ok"], true);
        // 要登录（命中本族处理器 → unauthorized；未迁移兜底会是 not_implemented）
        let (code, v) = call(&app, req_get("/api/exec/runs", "")).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"]["code"], "unauthorized");
        let (code, v) = call(&app, req_get("/api/exec/runs/ER-nope/log", "")).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"]["message"], "任务不存在");
    }

    /* ---- 鉴权与参数 ---- */

    #[tokio::test]
    async fn 提交与列表_未登录401_未知引擎与缺reason400() {
        let st = state("auth", &["wasm"], 1 << 20, 600).await;
        let (_uid, secret) = seed_user(&st, "张三").await;
        let app = mk_app(&st);

        let (code, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                "",
                json!({"cmd": "ls", "reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["code"], "unauthorized");
        assert_eq!(
            v["error"]["message"],
            "未认证或凭据无效（先 ncc login / ncc sandbox init，或带 API-KEY）"
        );

        let (code, v) = call(&app, req_get("/api/exec/runs", "")).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(
            v["error"]["message"],
            "未认证或凭据无效（先 ncc login / ncc sandbox init）"
        );

        // 未知引擎
        let (code, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "os-shell", "cmd": "ls", "reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "不认识的引擎（可选：wasm/process/container）"
        );

        // 缺 reason（写操作必须说明为什么）
        let (code, v) = call(
            &app,
            req_json("POST", "/api/exec/runs", &secret, json!({"cmd": "ls"})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "缺少 reason：这台机器替谁跑、为什么跑，要留在账本里"
        );

        // 缺 cmd
        let (code, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "wasm", "reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "缺少 cmd（或上传一个 .hur 包）");

        // 没放行的引擎：连字节都不收
        let (code, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "ls", "reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "engine_not_allowed");
        assert_eq!(
            v["error"]["message"],
            "这台机器没有放行 process 引擎（运维要显式打开：NCCR_EXEC_ALLOW 加上它）"
        );

        // 不认识的引擎优先于「没放行」报出来（顺序与 Go 一致）
        assert_eq!(v["error"]["code"], "engine_not_allowed");
    }

    #[tokio::test]
    async fn 提交_wasm不可用409_超时限400() {
        let mut st = state("unavail", &["wasm", "process"], 1 << 20, 60).await;
        let mut cfg = (*st.cfg()).clone();
        cfg.exec_runner = "/definitely/not/a/runner".to_string();
        st.cfg = Arc::new(cfg);
        let (_uid, secret) = seed_user(&st, "张三").await;
        let app = mk_app(&st);

        let (code, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "wasm", "cmd": "x", "reason": "r"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(v["error"]["code"], "engine_unavailable");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("wasm 引擎当前不可用："));

        let (code, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "echo hi", "reason": "r", "timeoutSec": 61}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "timeoutSec 不能超过节点上限（60 秒）"
        );

        // 空 body 的 JSON 分支也报「请求体格式错误」
        let (code, v) = call(&app, req_json("POST", "/api/exec/runs", &secret, json!({}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "缺少 reason：这台机器替谁跑、为什么跑，要留在账本里"
        );
    }

    /* ---- 真正跑一条命令（process 引擎） ---- */

    #[tokio::test]
    async fn 跑命令_看状态与日志_并带上日志尾巴() {
        let st = state("run", &["process"], 1 << 20, 30).await;
        let (uid, secret) = seed_user(&st, "张三").await;
        let app = mk_app(&st);

        let (code, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "echo 你好; echo err 1>&2", "reason": "看看"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        let id = v["run"]["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("ER-"));
        assert_eq!(v["run"]["owner_id"], Value::Null); // 视图里不暴露 ownerId（与 Go 一致）
        assert_eq!(v["run"]["engine"], "process");
        assert_eq!(v["run"]["status"], "queued");
        assert_eq!(v["run"]["reason"], "看看");
        assert_eq!(v["run"]["logUrl"], format!("/api/exec/runs/{id}/log"));
        assert_eq!(v["kindsUrl"], "/api/exec/kinds");

        let v = wait_status(&app, &secret, &id, &["succeeded", "failed"]).await;
        assert_eq!(v["run"]["status"], "succeeded");
        assert_eq!(v["run"]["exitCode"], 0);
        assert_eq!(v["run"]["logTruncated"], false);
        assert!(v["run"]["logBytes"].as_i64().unwrap() > 0);
        assert!(v["run"]["logTail"].as_str().unwrap().contains("你好"));
        assert_eq!(v["run"]["logTailTruncated"], false);
        assert!(v["run"]["startedAt"].is_string());
        assert!(v["run"]["finishedAt"].is_string());
        assert!(v["run"]["durationMs"].as_i64().unwrap() >= 0);

        // 日志全文是 text/plain，带状态与退出码头
        let (code, headers, body) =
            call_raw(&app, req_get(&format!("/api/exec/runs/{id}/log"), &secret)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
        assert_eq!(headers["x-ncc-exec-status"], "succeeded");
        assert_eq!(headers["x-ncc-exec-exit"], "0");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("你好"), "{text}");
        assert!(text.contains("err"), "{text}");

        // 归属：本人能看到
        let rows = store::exec::list(st.pool(), &uid, 50, 0).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].spec, "echo 你好; echo err 1>&2");
        assert_eq!(rows[0].timeout_sec, 30);
    }

    #[tokio::test]
    async fn 跑命令_退出码非零算failed_日志超限要标截断() {
        let st = state("trunc", &["process"], 10, 30).await;
        let (_uid, secret) = seed_user(&st, "李四").await;
        let app = mk_app(&st);

        let (_, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "printf abcdefghijklmnopqrstuvwxyz", "reason": "截断"}),
            ),
        )
        .await;
        let id = v["run"]["id"].as_str().unwrap().to_string();
        let v = wait_status(&app, &secret, &id, &["succeeded", "failed"]).await;
        assert_eq!(v["run"]["status"], "succeeded");
        assert_eq!(v["run"]["logBytes"], 26); // 真实累计量，不是落盘量
        assert_eq!(v["run"]["logTruncated"], true);

        let (_, headers, body) =
            call_raw(&app, req_get(&format!("/api/exec/runs/{id}/log"), &secret)).await;
        assert_eq!(headers["x-ncc-exec-truncated"], "1");
        assert_eq!(body.len(), 10);

        // 非零退出码 → failed
        let (_, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "exit 3", "reason": "失败"}),
            ),
        )
        .await;
        let id2 = v["run"]["id"].as_str().unwrap().to_string();
        let v = wait_status(&app, &secret, &id2, &["succeeded", "failed"]).await;
        assert_eq!(v["run"]["status"], "failed");
        assert_eq!(v["run"]["exitCode"], 3);
    }

    #[tokio::test]
    async fn 超时_记为timeout并给出明确文案() {
        let st = state("timeout", &["process"], 1 << 20, 1).await;
        let (_uid, secret) = seed_user(&st, "王五").await;
        let app = mk_app(&st);
        let (_, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "sleep 30", "reason": "会超时"}),
            ),
        )
        .await;
        let id = v["run"]["id"].as_str().unwrap().to_string();
        let started = std::time::Instant::now();
        let v = wait_status(
            &app,
            &secret,
            &id,
            &["succeeded", "failed", "timeout", "canceled"],
        )
        .await;
        assert_eq!(v["run"]["status"], "timeout");
        assert_eq!(v["run"]["exitCode"], -1);
        assert_eq!(v["run"]["error"], "超过墙上限 1s 被终止");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "超时没有真的把进程停掉"
        );
    }

    /// Go 的规格：execOne 收到的 timeout 是**这条任务提交时定下的那个值**
    /// （写进 rec.TimeoutSec），不是 `NCCR_EXEC_TIMEOUT` 这个节点上限。
    /// 错用节点上限的话，下面 `timeoutSec: 1` 的 `sleep 30` 会一直跑到自然结束。
    #[tokio::test]
    async fn 超时_按每条任务自己的墙上限终止而不是节点上限() {
        let st = state("timeout-per-task", &["process"], 1 << 20, 60).await;
        let (_uid, secret) = seed_user(&st, "赵六").await;
        let app = mk_app(&st);
        let (_, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "sleep 30", "reason": "会超时", "timeoutSec": 1}),
            ),
        )
        .await;
        let id = v["run"]["id"].as_str().unwrap().to_string();
        assert_eq!(v["run"]["timeoutSec"], 1);
        let started = std::time::Instant::now();
        let v = wait_status(
            &app,
            &secret,
            &id,
            &["succeeded", "failed", "timeout", "canceled"],
        )
        .await;
        assert_eq!(v["run"]["status"], "timeout");
        assert_eq!(v["run"]["error"], "超过墙上限 1s 被终止");
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "节点上限（60s）盖过了任务自己的 1s，进程没被按时终止"
        );
    }

    /* ---- 取消 / 删除 ---- */

    #[tokio::test]
    async fn 取消_DELETE真的把进程组停掉() {
        let st = state("cancel", &["process"], 1 << 20, 60).await;
        let (_uid, secret) = seed_user(&st, "赵六").await;
        let app = mk_app(&st);
        let (_, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "sleep 2; echo x > marker.txt", "reason": "取消我"}),
            ),
        )
        .await;
        let id = v["run"]["id"].as_str().unwrap().to_string();
        let work = PathBuf::from(v["run"]["workDir"].as_str().unwrap());
        // 等执行器真的把这台任务登记进「在跑表」再取消：否则取消信号会丢，
        // 进程会自己跑完（DB 状态倒是 canceled，那是假取消）。
        for _ in 0..100 {
            let (_, v) = call(&app, req_get(&format!("/api/exec/runs/{id}"), &secret)).await;
            if v["run"]["status"] == "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/exec/runs/{id}"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["canceled"], true);

        let v = wait_status(&app, &secret, &id, &["canceled"]).await;
        assert_eq!(v["run"]["status"], "canceled");
        assert_eq!(v["run"]["error"], "用户取消");
        // 再等一会儿：如果进程组没被回收，`sleep 2` 会走完并写出 marker
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!work.join("marker.txt").exists(), "取消没有真的停掉子进程");
    }

    #[tokio::test]
    async fn 删除_已结束的任务purge连目录一起删_未结束的不给删() {
        let st = state("purge", &["process"], 1 << 20, 30).await;
        let (_uid, secret) = seed_user(&st, "钱七").await;
        let app = mk_app(&st);
        let (_, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &secret,
                json!({"engine": "process", "cmd": "echo done", "reason": "跑完就删"}),
            ),
        )
        .await;
        let id = v["run"]["id"].as_str().unwrap().to_string();
        let work = PathBuf::from(v["run"]["workDir"].as_str().unwrap());
        wait_status(&app, &secret, &id, &["succeeded"]).await;

        // 不带 purge：只是提示，不删
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/exec/runs/{id}"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["note"], "任务已结束，无需取消（?purge=1 可连日志一起删）");
        assert!(work.exists());

        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/exec/runs/{id}?purge=1"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["deleted"], true);
        assert!(!work.exists());
        assert!(store::exec::find(st.pool(), &id).await.unwrap().is_none());
    }

    /* ---- 可见性 ---- */

    #[tokio::test]
    async fn 可见性_能提交不等于能看别人的任务() {
        let st = state("vis", &["process"], 1 << 20, 30).await;
        // 第一个账号自动是管理员；管理员的判定走**会话**（API-Key 不是会话，
        // 与 Go 的 ensureAdmin 一致），所以这里签一张用户 JWT。
        let (admin_id, _admin_key) = seed_user(&st, "管理员").await;
        let admin_secret = crate::jwt::sign_user(
            &st.cfg().jwt_secret,
            &admin_id,
            "管理员@x.com",
            std::time::Duration::from_secs(3600),
        );
        let (_u2, s2) = seed_user(&st, "小二").await;
        let (_u3, s3) = seed_user(&st, "小三").await;
        let app = mk_app(&st);

        let (_, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &admin_secret,
                json!({"engine": "process", "cmd": "echo a", "reason": "a"}),
            ),
        )
        .await;
        let id1 = v["run"]["id"].as_str().unwrap().to_string();
        let (_, v) = call(
            &app,
            req_json(
                "POST",
                "/api/exec/runs",
                &s2,
                json!({"engine": "process", "cmd": "echo b", "reason": "b"}),
            ),
        )
        .await;
        let id2 = v["run"]["id"].as_str().unwrap().to_string();

        // 小二只看得到自己的
        let (code, v) = call(&app, req_get("/api/exec/runs", &s2)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["runs"][0]["id"], id2);

        // 别人的任务：403（能提交 ≠ 能看别人的任务）
        let (code, v) = call(&app, req_get(&format!("/api/exec/runs/{id1}"), &s2)).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(
            v["error"]["message"],
            "只能看自己提交的任务（管理员可看全部）"
        );
        let (code, _) = call(&app, req_get(&format!("/api/exec/runs/{id1}/log"), &s2)).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        // 处置别人的任务同样不行
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/exec/runs/{id1}"))
                .header("authorization", format!("Bearer {s2}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "只能处置自己提交的任务");

        // 管理员（会话）能看全部
        let (code, v) = call(&app, req_get("/api/exec/runs?all=1", &admin_secret)).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 2);
        // 普通用户 ?all=1 需要管理员身份
        let (code, v) = call(&app, req_get("/api/exec/runs?all=1", &s3)).await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "admin_required");
        // 匿名
        let (code, _) = call(&app, req_get("/api/exec/runs?all=1", "")).await;
        assert_eq!(code, StatusCode::FORBIDDEN);

        // 不存在的任务
        let (code, v) = call(&app, req_get("/api/exec/runs/ER-nope", &s2)).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "任务不存在");
    }

    #[tokio::test]
    async fn 日志不存在时给纯文本404() {
        let st = state("nolog", &["process"], 1 << 20, 30).await;
        let (uid, secret) = seed_user(&st, "孙八").await;
        let app = mk_app(&st);
        let rec = store::exec::create(
            st.pool(),
            store::exec::ExecRunInput {
                owner_id: uid,
                engine: "process".to_string(),
                kind: "cmd".to_string(),
                log_path: "/nonexistent/log.txt".to_string(),
                reason: "r".to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // 本人看得到这条任务，只是日志还没落盘
        let (code, headers, body) = call_raw(
            &app,
            req_get(&format!("/api/exec/runs/{}/log", rec.id), &secret),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
        assert_eq!(
            String::from_utf8_lossy(&body),
            "日志不存在（任务可能还没开始，或被清理了）"
        );
        // 别人的任务：守卫先拦（403，不是 404）
        let (_u2, s2) = seed_user(&st, "周九").await;
        let (code, _, _) = call_raw(
            &app,
            req_get(&format!("/api/exec/runs/{}/log", rec.id), &s2),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
    }

    /* ---- wasm 包上传 ---- */

    #[tokio::test]
    async fn 上传hur_octet_stream与multipart都认_坏包400() {
        let st = state("pkg", &["wasm"], 1 << 20, 30).await;
        let (_uid, secret) = seed_user(&st, "周九").await;
        let app = mk_app(&st);

        // 空包
        let (code, v) = call(
            &app,
            req_bytes("/api/exec/runs?reason=r", &secret, Vec::new()),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "上传内容为空");

        // 不是 zip
        let (code, v) = call(
            &app,
            req_bytes("/api/exec/runs?reason=r", &secret, b"hello".to_vec()),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "不是一个能解开的 hur 包：既不是 gzip 也不是 zip —— 这不是一个 hur 包"
        );

        // 没有 reason
        let zip = zip_stored(&[("app/deploy.sh", b"echo hi\n")]);
        let (code, v) = call(&app, req_bytes("/api/exec/runs", &secret, zip.clone())).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "缺少 reason：这台机器替谁跑、为什么跑，要留在账本里"
        );

        // 好包：201，跑完后 pkg 目录里有解出来的文件
        let (code, v) = call(
            &app,
            req_bytes("/api/exec/runs?reason=部署&engine=wasm", &secret, zip),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        assert_eq!(v["run"]["kind"], "package");
        assert_eq!(v["run"]["engine"], "wasm");
        let id = v["run"]["id"].as_str().unwrap().to_string();
        let work = PathBuf::from(v["run"]["workDir"].as_str().unwrap());
        let _ = wait_status(&app, &secret, &id, &["succeeded", "failed"]).await;
        assert_eq!(
            std::fs::read_to_string(work.join("pkg/app/deploy.sh")).unwrap(),
            "echo hi\n"
        );

        // gzip 包住的 zip 也认
        let gz = gzip_stored(&zip_stored(&[("hur.json", b"{}")]));
        let (code, v) = call(
            &app,
            req_bytes("/api/exec/runs?reason=gz&engine=wasm", &secret, gz),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);
        let id = v["run"]["id"].as_str().unwrap().to_string();
        let work = PathBuf::from(v["run"]["workDir"].as_str().unwrap());
        let _ = wait_status(&app, &secret, &id, &["succeeded", "failed"]).await;
        assert_eq!(
            std::fs::read_to_string(work.join("pkg/hur.json")).unwrap(),
            "{}"
        );

        // multipart
        let boundary = "NCCBOUNDARY";
        let mut body: Vec<u8> = Vec::new();
        for (k, v) in [
            ("engine", "wasm"),
            ("reason", "多部件"),
            ("timeoutSec", "10"),
        ] {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"a.hur\"\r\n\
              Content-Type: application/octet-stream\r\n\r\n",
        );
        body.extend_from_slice(&zip_stored(&[("hur.json", b"{\"id\":\"x\"}")]));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/api/exec/runs")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["run"]["reason"], "多部件");
        assert_eq!(v["run"]["timeoutSec"], 10);

        // multipart 缺 file 字段
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/api/exec/runs")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::from(format!("--{boundary}--\r\n")))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "缺少 file 字段（或在 JSON body 里给 cmd）"
        );
    }

    #[tokio::test]
    async fn 包上传_引擎不是wasm400_containernotallowed403() {
        let st = state("pkg-engine", &["wasm"], 1 << 20, 30).await;
        let (_uid, secret) = seed_user(&st, "吴十").await;
        let app = mk_app(&st);
        // octet-stream 默认 engine=wasm，显式给 process 就会被拦在「上传只能配 wasm」
        let (code, v) = call(
            &app,
            req_bytes(
                "/api/exec/runs?reason=r&engine=wasm",
                &secret,
                zip_stored(&[("a", b"b")]),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED);

        // multipart 声明 engine=process（放了行但上传只能配 wasm）
        let mut st2 = state("pkg-engine2", &["wasm", "process"], 1 << 20, 30).await;
        let mut cfg = (*st2.cfg()).clone();
        cfg.exec_allow = vec!["wasm".to_string(), "process".to_string()];
        st2.cfg = Arc::new(cfg);
        let (_u, secret2) = seed_user(&st2, "郑十一").await;
        let app2 = mk_app(&st2);
        let boundary = "B2";
        let mut body: Vec<u8> = Vec::new();
        for (k, val) in [("engine", "process"), ("reason", "r")] {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n{val}\r\n").as_bytes(),
            );
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            b"Content-Disposition: form-data; name=\"file\"; filename=\"a.hur\"\r\n\r\n",
        );
        body.extend_from_slice(&zip_stored(&[("a", b"b")]));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let (code, v) = call(
            &app2,
            Request::builder()
                .method("POST")
                .uri("/api/exec/runs")
                .header(
                    "content-type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .header("authorization", format!("Bearer {secret2}"))
                .body(Body::from(body))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "上传 .hur 只能配 wasm 引擎；要跑命令请用 --cmd + --engine"
        );

        // container 没放行 → 403（上传路径先撞放行检查）
        let (code, v) = call(
            &app,
            req_bytes(
                "/api/exec/runs?reason=r&engine=container",
                &secret,
                b"x".to_vec(),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "engine_not_allowed");
    }

    /* ---- 解包边界 ---- */

    #[test]
    fn 解包_目录穿越一律拒() {
        let dir = test_dir("unzip");
        let dest = dir.join("out");
        // 包里带 ..
        let zip = zip_stored(&[("../evil.sh", b"rm -rf /")]);
        let e = unpack_hur(&zip, &dest).unwrap_err();
        assert!(e.starts_with("包里有不安全的路径："), "{e}");
        assert!(!dir.join("evil.sh").exists());

        // 绝对路径
        let zip = zip_stored(&[("/etc/passwd", b"x")]);
        let e = unpack_hur(&zip, &dest).unwrap_err();
        assert!(e.starts_with("包里有不安全的路径："), "{e}");

        // 名字里只要出现 `..` 就拒（Go 是 `strings.Contains(name, "..")`，不做 Clean）
        let zip = zip_stored(&[("a/../b.txt", b"ok")]);
        let e = unpack_hur(&zip, &dest).unwrap_err();
        assert_eq!(e, "包里有不安全的路径：a/../b.txt");

        // 正常路径：`.` 与重复斜杠归一后照常落盘
        let zip = zip_stored(&[("a/./b.txt", b"ok")]);
        unpack_hur(&zip, &dest).unwrap();
        assert_eq!(std::fs::read_to_string(dest.join("a/b.txt")).unwrap(), "ok");

        // 同一路径重复解 → 覆盖（幂等）
        unpack_hur(&zip_stored(&[("a/b.txt", b"second")]), &dest).unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("a/b.txt")).unwrap(),
            "second"
        );
    }

    #[test]
    fn 安全拼接_与go行为一致() {
        assert_eq!(clean_slash_path("a/b"), "/a/b");
        assert_eq!(clean_slash_path("a//b/"), "/a/b");
        assert_eq!(clean_slash_path("a/../b"), "/b");
        assert_eq!(clean_slash_path(".."), "/");
        assert_eq!(clean_slash_path("./x"), "/x");

        let dest = PathBuf::from("/w");
        assert_eq!(
            safe_zip_target(&dest, "a/b").unwrap(),
            PathBuf::from("/w/a/b")
        );
        assert!(safe_zip_target(&dest, "..").is_err());
        // 含 `..` 一律拒（Go 的 unpackHur 就是这么判的，比 Clean 更保守）
        assert!(safe_zip_target(&dest, "a/../../b").is_err());
        assert!(safe_zip_target(&dest, "\\x").is_err());
        assert!(safe_zip_target(&dest, "/etc/passwd").is_err());
    }

    /// 直接造一个 Capability（不探本机），把三种引擎的 argv 与拒绝文案钉住。
    fn cap(allow: &[&str], images: &[&str], enabled: &[&str]) -> Capability {
        let kind = |id: &'static str| Kind {
            id,
            enabled: enabled.contains(&id),
            provider: String::new(),
            why: format!("{id} 没准备好"),
        };
        Capability {
            allow: allow.iter().map(|s| s.to_string()).collect(),
            kinds: vec![kind("wasm"), kind("process"), kind("container")],
            runner: RunnerInfo {
                path: "/usr/bin/ncc".to_string(),
                ok: true,
                note: String::new(),
                bin: "ncc".to_string(),
            },
            shell: "sh".to_string(),
            docker: "docker".to_string(),
            images: images.iter().map(|s| s.to_string()).collect(),
            tags: Vec::new(),
            timeout_ms: 1000,
            max_output: 1024,
            work_root: "/w".to_string(),
        }
    }

    fn plan(engine: &str, cmd: &str, image: &str) -> Plan {
        Plan {
            engine: engine.to_string(),
            kind: "cmd".to_string(),
            work_dir: PathBuf::from("/w/job"),
            command: cmd.to_string(),
            image: image.to_string(),
            package_dir: PathBuf::new(),
        }
    }

    #[test]
    fn 执行计划_argv与拒绝文案() {
        // process
        let c = cap(
            &["wasm", "process", "container"],
            &[],
            &["wasm", "process", "container"],
        );
        assert_eq!(
            build_argv(&c, &plan("process", "echo hi", "")).unwrap(),
            vec!["sh", "-c", "echo hi"]
        );
        assert_eq!(
            build_argv(&c, &plan("process", "  ", "")).unwrap_err(),
            "process 引擎要一条命令"
        );

        // wasm：要包
        let mut p = plan("wasm", "", "");
        assert_eq!(
            build_argv(&c, &p).unwrap_err(),
            "wasm 引擎要一个包（--package 或在 body 里传 .hur）"
        );
        p.kind = "package".to_string();
        p.package_dir = PathBuf::from("/w/job/pkg");
        assert_eq!(
            build_argv(&c, &p).unwrap(),
            vec![
                "/usr/bin/ncc",
                "hur",
                "run",
                "/w/job/pkg",
                "--exec",
                "--json"
            ]
        );
        assert_eq!(p.spec(), "/w/job/pkg");

        // container：镜像白名单 + argv
        let c2 = cap(&["container"], &[], &["container"]);
        let p2 = plan("container", "make", "alpine:3");
        assert_eq!(
            build_argv(&c2, &p2).unwrap(),
            vec![
                "docker",
                "run",
                "--rm",
                "-v",
                "/w/job:/work",
                "-w",
                "/work",
                "alpine:3",
                "sh",
                "-c",
                "make"
            ]
        );
        assert_eq!(p2.spec(), "[alpine:3] make");
        assert_eq!(
            build_argv(&c2, &plan("container", "make", "")).unwrap_err(),
            "container 引擎要 --image 与一条命令"
        );
        // 白名单为空 = 不限制；配了清单就必须命中
        let c3 = cap(&["container"], &["alpine:3"], &["container"]);
        assert!(build_argv(&c3, &p2).is_ok());
        assert_eq!(
            build_argv(&c3, &plan("container", "make", "ubuntu")).unwrap_err(),
            "镜像 \"ubuntu\" 不在 NCCR_EXEC_IMAGES 允许清单里"
        );

        // 不认识的引擎 → 报在放行检查之前
        assert_eq!(
            build_argv(&c, &plan("js", "x", "")).unwrap_err(),
            "不认识的引擎 \"js\"（可选：wasm/process/container）"
        );
        // 没放行
        let c4 = cap(&[], &[], &["process"]);
        assert_eq!(
            build_argv(&c4, &plan("process", "echo hi", "")).unwrap_err(),
            "这台机器没有放行 process 引擎（运维要显式打开：NCCR_EXEC_ALLOW 加上 process）"
        );
        // 放行了但本机不具备条件
        let c5 = cap(&["process"], &[], &[]);
        assert_eq!(
            build_argv(&c5, &plan("process", "echo hi", "")).unwrap_err(),
            "process 引擎现在不可用：process 没准备好"
        );
    }

    #[test]
    fn 取整与截断_与go一致() {
        assert_eq!(atoi_default("", 50), 50);
        assert_eq!(atoi_default(" 12 ", 50), 12);
        assert_eq!(atoi_default("1x", 50), 50);
        assert_eq!(atoi_default("2000000", 50), 50); // 超过 1e6 → 默认
        assert_eq!(clamp_runes("  一二三四五  ", 3), "一二三");
        assert_eq!(clamp_runes("abc", 5), "abc");
        assert_eq!(go_duration(Duration::from_secs(5)), "5s");
        assert_eq!(go_duration(Duration::from_secs(60)), "1m0s");
        assert_eq!(go_duration(Duration::from_secs(5400)), "1h30m0s");
    }

    #[test]
    fn 解包_压缩炸弹与坏gzip都被挡() {
        let dir = test_dir("bomb");
        let dest = dir.join("out");
        // gzip 头对、deflate 层坏
        let mut bad = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];
        bad.extend_from_slice(&[0xffu8; 8]);
        let e = unpack_hur(&bad, &dest).unwrap_err();
        assert_eq!(e, "gzip 层解压失败");

        // gzip 头都不对
        let e = unpack_hur(&[0x1f, 0x8b, 0x00, 0x00], &dest).unwrap_err();
        assert_eq!(e, "gzip 层打不开（不是有效的 .hur）");
    }

    #[test]
    fn 日志尾巴_读最后n字节并标截断() {
        let dir = test_dir("tail");
        let p = dir.join("log.txt");
        std::fs::write(&p, b"0123456789").unwrap();
        let (t, cut) = read_tail(&p, 4).unwrap();
        assert_eq!(t, "6789");
        assert!(cut);
        let (t, cut) = read_tail(&p, 100).unwrap();
        assert_eq!(t, "0123456789");
        assert!(!cut);
        assert!(read_tail(&dir.join("nope"), 4).is_none());
    }
}
