//! 节点的 P2P 控制面（原实现 `ncc-registry/httpapi/p2p.go` + `internal/p2p/p2p.go`）。
//!
//! 与云端 ncc-platform 的 `/api/p2p/*` 是**互补**关系，不是同一件事：
//!
//! ```text
//! ncc-platform（hub） 信令 + 票据 + ICE 配置 —— 帮两端**找到彼此**
//! 本节点（node）      自己的 NAT 画像 + 真实对打 —— 回答**这台机器打不打得到**
//! ```
//!
//! 之所以放在节点侧：NAT 状况是每台机器的事（内网节点的出口常跟开发者笔记本完全
//! 不同），而且纯内网/跨网场景下两端可能都不方便走云端信令。红线不变：
//! **只做 STUN 探测与应答，不搬运业务字节**；TURN 由客户自托管。
//!
//! 刻意的取舍：
//!
//! * STUN 手写实现（RFC 5389：20 字节头 + `MAPPED-ADDRESS`/`XOR-MAPPED-ADDRESS`），
//!   不引第三方 ICE 库 —— 这里只需要「Binding 请求 / 响应」两件事。
//! * **连不上外部 STUN 不是 500**：拿不到映射就回 Go 里那种「不确定 / blocked」结论，
//!   附带怎么排查的建议。没外网的环境里这是最常见的情况，回 500 等于把「网络不通」
//!   说成「服务坏了」。
//! * 「可被打洞入口」（responder）是**进程内单例**：Go 侧挂在 `Server` 上，这里没有
//!   那个上帝对象，用一个模块级槽位代替（同一进程本来也只该有一个入口）。
//!   `NCCR_P2P_SERVE=1` 的自动开启做成**首次访问 P2P 端点时**自开而不是 main 启动时：
//!   本批迁移约定不许改 `main.rs`，而入口只在「有人要打洞」时才有意义。
//! * `p2p.check` 只写日志、不写治理审计：审计是**管理员动作**的账（谁改了用户/节点/
//!   服务），「谁跟谁试过打洞」属于运行观测，别混进治理面。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, Notify};

use ncc_core::error::{ApiError, ApiResult};

use crate::httpapi::{helpers, AppState, Auth};

/// 该族路由（相对 `/api`）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/p2p/self", get(p2p_self))
        .route("/p2p/check", axum::routing::post(p2p_check))
        .route("/p2p/serve", get(p2p_serve_get).post(p2p_serve_set))
}

/* ---------------- 处理器 ---------------- */

/// GET /api/p2p/self —— 本节点的打洞条件画像（含是否开了可被打洞入口）。
async fn p2p_self(State(state): State<AppState>, auth: Auth) -> ApiResult<Response> {
    auth.require_scope("p2p:read")?;
    ensure_auto_serve(&state).await;
    let servers = stun_servers(&state);
    let prof = probe(&servers, Duration::from_secs(3)).await;
    let cfg = state.cfg();
    Ok(helpers::ok_json(json!({
        "node": {"id": cfg.node_id, "name": cfg.node_name, "role": cfg.role, "region": cfg.node_region},
        "profile": prof,
        "serve": serve_state().await,
        "ice": {
            "stun": servers,
            "turn": cfg.p2p_turn,
            "note": "TURN 必须由客户自托管；无 TURN 且打洞失败时应明确报错，不降级为中心中转",
        },
    })))
}

#[derive(Debug, Deserialize, Default)]
struct P2PCheckReq {
    /// 对端映射地址 `ip:port`（由对端 `/api/p2p/self` 或 `/api/p2p/serve` 报出）。
    #[serde(default)]
    peer: String,
    #[serde(default, rename = "waitSec")]
    wait_sec: i64,
}

/// POST /api/p2p/check {peer, waitSec} —— 与一个已知映射地址做真实对打。
async fn p2p_check(
    State(state): State<AppState>,
    auth: Auth,
    body: Bytes,
) -> ApiResult<Response> {
    auth.require_scope("p2p:write")?;
    ensure_auto_serve(&state).await;
    let body: P2PCheckReq = parse_body(&body)?;

    let peer = parse_peer_addr(&body.peer)
        .await
        .map_err(|e| ApiError::bad_request("bad_request", e))?;
    let wait = if body.wait_sec <= 0 {
        10
    } else if body.wait_sec > 60 {
        return Err(ApiError::bad_request("bad_request", "waitSec 最长 60 秒"));
    } else {
        body.wait_sec
    };

    let res = check(
        peer,
        &stun_servers(&state),
        Duration::from_secs(wait as u64),
        Duration::from_secs(3),
    )
    .await;
    tracing::info!(
        "p2p.check peer={} ok={} rtt={}ms mine={}",
        body.peer,
        res.ok,
        res.rtt_ms,
        res.my_mapped
    );
    if !res.ok && res.my_mapped.is_empty() {
        // 连自己的映射都拿不到 = 本地出站就废了，与「对端不通」要分开报。
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "p2p_probe_failed",
            res.reason.clone(),
        ));
    }
    Ok(helpers::ok_json(json!({"result": res})))
}

/// GET /api/p2p/serve —— 看可被打洞入口的状态。
async fn p2p_serve_get(State(state): State<AppState>, auth: Auth) -> ApiResult<Response> {
    auth.require_scope("p2p:read")?;
    ensure_auto_serve(&state).await;
    Ok(helpers::ok_json(json!({"serve": serve_state().await})))
}

#[derive(Debug, Deserialize, Default)]
struct ServeReq {
    #[serde(default)]
    on: Option<bool>,
    #[serde(default)]
    peer: String,
}

/// POST /api/p2p/serve {on, peer} —— 开/关可被打洞入口，可选指定反向打洞对端。
///
/// 为什么要 peer：本机 NAT 若是 address/port-dependent filtering（常见），**纯被动
/// 应答收不到第一个包** —— 必须双方同时向对方映射发包。peer 就是「对端的映射地址」
/// （对端 `p2p self` / `p2p serve` 里报出的 mapped），给了它本机就会主动反向发。
///
/// 这是**显式动作**：开了之后本节点会对外应答 STUN Binding 请求（只回一行映射信息，
/// 不碰业务字节），等于告诉别人「这台机器在这儿」。所以要登录 + 作用域。
async fn p2p_serve_set(
    State(state): State<AppState>,
    auth: Auth,
    body: Bytes,
) -> ApiResult<Response> {
    auth.require_scope("p2p:write")?;
    let body: ServeReq = parse_body(&body)
        .map_err(|_| ApiError::bad_request("bad_request", SERVE_HINT))?;
    let Some(on) = body.on else {
        return Err(ApiError::bad_request("bad_request", SERVE_HINT));
    };

    let mut peers: Vec<SocketAddr> = Vec::new();
    for raw in body.peer.split(',') {
        if raw.trim().is_empty() {
            continue;
        }
        let p = parse_peer_addr(raw)
            .await
            .map_err(|e| ApiError::bad_request("bad_request", format!("peer 格式不对：{e}")))?;
        peers.push(p);
    }

    let mut slot = serve_slot().lock().await;
    if on {
        if slot.is_none() {
            match Responder::start(&stun_servers(&state), Duration::from_secs(3)).await {
                Ok(r) => {
                    tracing::info!("p2p.serve 已开启 listen={} mapped={}", r.addr(), r.mapped_addr());
                    *slot = Some(Arc::new(r));
                }
                Err(e) => {
                    return Err(ApiError::internal(format!("启动打洞入口失败：{e}")));
                }
            }
        }
        // 只有显式给了非空 peer 才覆盖，避免单纯的 --on 把已配的对端抹掉。
        if !peers.is_empty() {
            if let Some(r) = slot.as_ref() {
                r.set_peers(peers);
            }
        }
    } else if let Some(r) = slot.take() {
        r.close();
        tracing::info!("p2p.serve 已关闭");
    }
    let st = serve_state_of(slot.as_deref());
    Ok(helpers::ok_json(json!({"serve": st})))
}

const SERVE_HINT: &str = "请求体需要 {on: true|false, peer?: \"ip:port\"}";

fn bad_body() -> ApiError {
    ApiError::bad_request("bad_request", "请求体格式错误")
}

/// 解析 JSON 请求体（判据与 Go 的 `ShouldBindJSON` 一致）。
///
/// 顶层必须是对象：serde 的派生实现会把数组按字段顺序塞进结构体（`visit_seq`），
/// `[{...}]` 之类会被「解析成功」，而 Go 那边是 400。顶层 `null` 在 Go 里不报错
/// （留下零值结构体），这里照做。
fn parse_body<T: serde::de::DeserializeOwned + Default>(b: &Bytes) -> ApiResult<T> {
    let v: Value = serde_json::from_slice(b).map_err(|_| bad_body())?;
    if v.is_null() {
        return Ok(T::default());
    }
    if !v.is_object() {
        return Err(bad_body());
    }
    serde_json::from_value(v).map_err(|_| bad_body())
}

/// 生效的 STUN 列表（没配就用默认表）。
fn stun_servers(state: &AppState) -> Vec<String> {
    let cfg = &state.cfg().p2p_stun;
    if cfg.is_empty() {
        DEFAULT_STUN.iter().map(|s| s.to_string()).collect()
    } else {
        cfg.clone()
    }
}

/* ---------------- 进程内入口单例 ---------------- */

static SERVE: OnceLock<Mutex<Option<Arc<Responder>>>> = OnceLock::new();

fn serve_slot() -> &'static Mutex<Option<Arc<Responder>>> {
    SERVE.get_or_init(|| Mutex::new(None))
}

/// `NCCR_P2P_SERVE=1` 时把入口拉起来（幂等；失败只记日志，其余功能不受影响）。
async fn ensure_auto_serve(state: &AppState) {
    if !state.cfg().p2p_serve {
        return;
    }
    let mut slot = serve_slot().lock().await;
    if slot.is_some() {
        return;
    }
    match Responder::start(&stun_servers(state), Duration::from_secs(3)).await {
        Ok(r) => {
            tracing::info!("p2p.serve 随服务开启 listen={} mapped={}", r.addr(), r.mapped_addr());
            *slot = Some(Arc::new(r));
        }
        Err(e) => {
            tracing::warn!("p2p.serve 启动失败（打洞入口不可用，其余功能不受影响）: {e}")
        }
    }
}

async fn serve_state() -> Value {
    let slot = serve_slot().lock().await;
    serve_state_of(slot.as_deref())
}

/// 只读一个 responder 快照（未开时也给一句怎么开）。
fn serve_state_of(r: Option<&Responder>) -> Value {
    match r {
        None => json!({
            "on": false,
            "hint": "要让别人能打洞过来：NCCR_P2P_SERVE=1 起服务，或 `ncc registry p2p serve --on`\
                     （只应答 STUN，不接收业务字节）",
        }),
        Some(r) => json!({
            "on": true,
            "listen": r.addr(),
            "mapped": r.mapped_addr(), // 对端应该发往这个地址
            "requestsTaken": r.requests(),
            "responsesSeen": r.responses(),
            "peers": r.peers().iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            "note": "NAT 过滤若是 address/port-dependent（常见），只被动应答收不到第一个包：\
                     必须双方同时向对方映射发包（用 peer 指定对端）。",
        }),
    }
}

/* ---------------- STUN 原语（RFC 5389/5780） ---------------- */

const MAGIC_COOKIE: u32 = 0x2112_A442;
const BINDING_REQ: u16 = 0x0001;
const BINDING_OK: u16 = 0x0101;
const ATTR_MAPPED: u16 = 0x0001;
const ATTR_CHANGE: u16 = 0x0003;
const ATTR_XOR_MAPPED: u16 = 0x0020;
const ATTR_OTHER: u16 = 0x802c;

/// 改 IP + 改端口（RFC 5780 的过滤行为测试）。
const CHANGE_IP_PORT: u32 = 0x06;
/// 只改端口。
const CHANGE_PORT: u32 = 0x02;

const DEFAULT_STUN_PORT: u16 = 3478;

/// 默认 STUN 列表：必须 ≥2 台**不同公网 IP** 的服务器，否则判不了映射行为
/// （只能靠 RFC 5780 那台）。实测某些网络下 Google STUN 不可达，所以默认带上
/// 国内可达的两台；**单台超时不等于 NAT 不友好**。
pub const DEFAULT_STUN: &[&str] = &[
    "stun:stun.miwifi.com:3478",
    "stun:stun.qq.com:3478",
    "stun:stun.l.google.com:19302",
];

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

/// 12 字节随机事务 ID（响应必须对得上它，否则当噪声丢掉）。
fn new_txid() -> [u8; 12] {
    let mut b = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

/// STUN 属性（类型 + 长度 + 值 + 4 字节对齐填充）。
fn attr(t: u16, val: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + val.len() + 3);
    out.extend_from_slice(&t.to_be_bytes());
    out.extend_from_slice(&(val.len() as u16).to_be_bytes());
    out.extend_from_slice(val);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out
}

fn build_message(kind: u16, txid: &[u8], attrs: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + attrs.len());
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
    let mut id = [0u8; 12];
    let n = txid.len().min(12);
    id[..n].copy_from_slice(&txid[..n]);
    out.extend_from_slice(&id);
    out.extend_from_slice(attrs);
    out
}

fn binding_request(txid: &[u8], change: u32, with_change: bool) -> Vec<u8> {
    let attrs = if with_change {
        attr(ATTR_CHANGE, &change.to_be_bytes())
    } else {
        Vec::new()
    };
    build_message(BINDING_REQ, txid, &attrs)
}

/// 把观察到的对端源地址回填进 XOR-MAPPED-ADDRESS：对端因此在自己那侧也拿到
/// 「被穿透成功」的证据（ICE 双向检查的道理）。只做 IPv4 —— IPv6 场景本身不需要打洞。
fn binding_success(txid: &[u8], observed: &SocketAddr) -> Option<Vec<u8>> {
    let SocketAddr::V4(v4) = observed else {
        return None;
    };
    let mut val = vec![0x00, 0x01];
    val.extend_from_slice(&(v4.port() ^ (MAGIC_COOKIE >> 16) as u16).to_be_bytes());
    let m = MAGIC_COOKIE.to_be_bytes();
    for (i, o) in v4.ip().octets().iter().enumerate() {
        val.push(o ^ m[i]);
    }
    Some(build_message(BINDING_OK, txid, &attr(ATTR_XOR_MAPPED, &val)))
}

fn parse_attrs(msg: &[u8]) -> Vec<(u16, Vec<u8>)> {
    if msg.len() < 20 {
        return Vec::new();
    }
    let mut end = 20 + be16(&msg[2..4]) as usize;
    if end > msg.len() {
        end = msg.len();
    }
    let mut out = Vec::new();
    let mut i = 20;
    while i + 4 <= end {
        let t = be16(&msg[i..i + 2]);
        let l = be16(&msg[i + 2..i + 4]) as usize;
        if i + 4 + l > end {
            break;
        }
        out.push((t, msg[i + 4..i + 4 + l].to_vec()));
        i += 4 + l;
        if l % 4 != 0 {
            i += 4 - l % 4;
        }
    }
    out
}

fn decode_xor_mapped(v: &[u8]) -> Option<SocketAddr> {
    if v.len() < 8 || v[1] != 0x01 {
        return None;
    }
    let port = be16(&v[2..4]) ^ (MAGIC_COOKIE >> 16) as u16;
    let m = MAGIC_COOKIE.to_be_bytes();
    let ip = Ipv4Addr::new(v[4] ^ m[0], v[5] ^ m[1], v[6] ^ m[2], v[7] ^ m[3]);
    Some(SocketAddr::new(IpAddr::V4(ip), port))
}

fn decode_plain(v: &[u8]) -> Option<SocketAddr> {
    if v.len() < 8 || v[1] != 0x01 {
        return None;
    }
    Some(SocketAddr::new(
        IpAddr::V4(Ipv4Addr::new(v[4], v[5], v[6], v[7])),
        be16(&v[2..4]),
    ))
}

/// 解析 `stun:host:port` / `host`（缺端口补 3478）。解析不了就 `None`（当作没配）。
async fn resolve_stun(s: &str) -> Option<SocketAddr> {
    let mut raw = s.trim();
    for p in ["stun:", "turn:", "stuns:", "turns:"] {
        if let Some(rest) = raw.strip_prefix(p) {
            raw = rest;
        }
    }
    if let Some(i) = raw.find('?') {
        raw = &raw[..i];
    }
    let hostport = if raw.contains(':') {
        raw.to_string()
    } else {
        format!("{raw}:{DEFAULT_STUN_PORT}")
    };
    tokio::net::lookup_host(hostport)
        .await
        .ok()?
        .find(|a| a.is_ipv4())
}

struct StunReply {
    xor_mapped: Option<SocketAddr>,
    other: Option<SocketAddr>,
}

/// 发一次 Binding 请求并等**事务 ID 匹配**的响应。
///
/// 刻意不校验来源：改 IP / 改端口的测试正是要收到**来自另一个地址**的响应。
async fn exchange(
    sock: &UdpSocket,
    dst: SocketAddr,
    change: u32,
    with_change: bool,
    timeout: Duration,
) -> Option<StunReply> {
    let txid = new_txid();
    let req = binding_request(&txid, change, with_change);
    let start = Instant::now();
    if sock.send_to(&req, dst).await.is_err() {
        return None;
    }
    let mut buf = [0u8; 1500];
    loop {
        let left = timeout.checked_sub(start.elapsed())?;
        if left.is_zero() {
            return None;
        }
        let Ok(Ok((n, _from))) = tokio::time::timeout(left, sock.recv_from(&mut buf)).await else {
            return None;
        };
        if n < 20 || buf[8..20] != txid {
            continue;
        }
        let mut rep = StunReply {
            xor_mapped: None,
            other: None,
        };
        for (t, v) in parse_attrs(&buf[..n]) {
            match t {
                ATTR_XOR_MAPPED => rep.xor_mapped = decode_xor_mapped(&v),
                ATTR_MAPPED => {
                    if rep.xor_mapped.is_none() {
                        rep.xor_mapped = decode_plain(&v);
                    }
                }
                ATTR_OTHER => rep.other = decode_plain(&v),
                _ => {}
            }
        }
        return Some(rep);
    }
}

/* ---------------- ① probe：本机 NAT 画像 ---------------- */

/// 打洞条件画像（字段与 Go / spike / PRD 一致，便于两端比对）。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Profile {
    #[serde(rename = "localIpv4")]
    pub local_ip: String,
    /// `public` | `private` | `cgnat`。
    #[serde(rename = "localAddrKind")]
    pub local_addr_kind: String,
    #[serde(rename = "localPort")]
    pub local_port: i64,
    #[serde(rename = "publicIp", skip_serializing_if = "String::is_empty")]
    pub public_ip: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub mapped: String,
    #[serde(rename = "serversTried")]
    pub servers_tried: i64,
    #[serde(rename = "serversReached")]
    pub servers_reached: i64,
    pub servers: Vec<String>,
    /// `endpoint_independent` | `address_dependent` | `address_and_port_dependent` | `unknown`。
    #[serde(rename = "mappingBehavior")]
    pub mapping_behavior: String,
    /// `rfc5780` | `multi-stun`。
    #[serde(rename = "mappingMethod")]
    pub mapping_method: String,
    #[serde(rename = "filteringBehavior")]
    pub filtering_behavior: String,
    /// `rfc5780` | `unsupported`。
    #[serde(rename = "filteringMethod")]
    pub filtering_method: String,
    #[serde(rename = "rfc5780Supported")]
    pub rfc5780: bool,
    /// `direct` | `likely_direct` | `relay_likely` | `blocked` | `unknown`。
    pub verdict: String,
    pub advice: String,
}

/// 本机地址属于哪类（内网 / 运营商级 NAT / 公网）。
fn host_addr_kind(ip: Ipv4Addr) -> &'static str {
    let o = ip.octets();
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        "cgnat"
    } else if o[0] == 10 || (o[0] == 192 && o[1] == 168) || (o[0] == 172 && (16..=31).contains(&o[1]))
    {
        "private"
    } else {
        "public"
    }
}

/// 本机出网 IP（不真的发包，让内核选路）。
async fn egress_ip() -> Option<IpAddr> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).await.ok()?;
    sock.connect(("1.1.1.1", 80)).await.ok()?;
    Some(sock.local_addr().ok()?.ip())
}

/// 只做**单边本地预检**：UDP 出站能不能用、拿到什么公网映射、NAT 映射/过滤行为。
pub async fn probe(servers: &[String], timeout: Duration) -> Profile {
    let servers: Vec<String> = if servers.is_empty() {
        DEFAULT_STUN.iter().map(|s| s.to_string()).collect()
    } else {
        servers.to_vec()
    };
    let timeout = if timeout.is_zero() {
        Duration::from_secs(3)
    } else {
        timeout
    };
    let mut prof = Profile {
        servers_tried: servers.len() as i64,
        servers: servers.clone(),
        mapping_method: "multi-stun".to_string(),
        filtering_method: "unsupported".to_string(),
        ..Default::default()
    };

    let sock = match UdpSocket::bind(("0.0.0.0", 0)).await {
        Ok(s) => s,
        Err(e) => {
            prof.verdict = "unknown".to_string();
            prof.advice = format!("无法创建 UDP socket：{e}");
            return prof;
        }
    };
    prof.local_port = sock.local_addr().map(|a| a.port()).unwrap_or(0) as i64;
    if let Some(IpAddr::V4(v4)) = egress_ip().await {
        prof.local_ip = v4.to_string();
        prof.local_addr_kind = host_addr_kind(v4).to_string();
    }

    let mut mapped_seen: Vec<String> = Vec::new();
    let mut rfc_server: Option<String> = None;
    let mut rfc_other: Option<SocketAddr> = None;
    for s in &servers {
        let Some(dst) = resolve_stun(s).await else {
            continue;
        };
        let Some(rep) = exchange(&sock, dst, 0, false, timeout).await else {
            continue;
        };
        let Some(mapped) = rep.xor_mapped else {
            continue;
        };
        prof.servers_reached += 1;
        mapped_seen.push(mapped.to_string());
        if prof.mapped.is_empty() {
            prof.mapped = mapped.to_string();
            prof.public_ip = mapped.ip().to_string();
        }
        if rep.other.is_some() && rfc_server.is_none() {
            rfc_server = Some(s.clone());
            rfc_other = rep.other;
        }
    }

    let direct = !prof.public_ip.is_empty() && prof.public_ip == prof.local_ip;
    if !direct {
        if let (Some(server), Some(other)) = (rfc_server.as_deref(), rfc_other) {
            if let Some((mapping, filtering)) = rfc5780(&sock, server, other, timeout).await {
                prof.mapping_behavior = mapping;
                prof.mapping_method = "rfc5780".to_string();
                prof.filtering_behavior = filtering;
                prof.filtering_method = "rfc5780".to_string();
                prof.rfc5780 = true;
            }
        }
    }
    if prof.mapping_method != "rfc5780" {
        let distinct = {
            let mut v = mapped_seen.clone();
            v.sort();
            v.dedup();
            v.len()
        };
        prof.mapping_behavior = if prof.servers_reached == 0 {
            "unknown"
        } else if direct {
            "endpoint_independent"
        } else if prof.servers_reached < 2 {
            "unknown"
        } else if distinct == 1 {
            "endpoint_independent"
        } else {
            "address_and_port_dependent"
        }
        .to_string();
    }

    match () {
        _ if prof.mapping_behavior == "unknown" && prof.servers_reached == 0 => {
            prof.verdict = "blocked".to_string();
            prof.advice = "所有 STUN 都没响应：UDP 出站可能被封（企业网常见）。直连不可行 —— \
                           用客户自托管 TURN over TCP/TLS，或走中心搬运（master 代取字节）。"
                .to_string();
        }
        _ if direct => {
            prof.verdict = "direct".to_string();
            prof.advice = "本机在公网上（映射等于本机 IP）：对端可直接连，打洞不必要。".to_string();
        }
        _ if prof.mapping_behavior == "unknown" => {
            prof.verdict = "unknown".to_string();
            prof.advice = "只探到 1 台 STUN 且它不支持 RFC 5780，判不了映射行为：\
                           多配几台不同公网 IP 的 STUN 再测。"
                .to_string();
        }
        _ if prof.mapping_behavior == "address_and_port_dependent" => {
            prof.verdict = "relay_likely".to_string();
            prof.advice = "对称 NAT：只有对端是锥形且由对端发起时有机会；两端都对称基本只能 relay。\
                           请配 NCCR_P2P_TURN（客户自托管）。"
                .to_string();
        }
        _ if prof.mapping_behavior == "address_dependent" => {
            prof.verdict = "likely_direct".to_string();
            prof.advice = "地址相关映射：通常仍能打洞，但需要双方同时发起。用 \
                           `ncc registry p2p check --peer <对端映射地址>` 实测。"
                .to_string();
        }
        _ => {
            prof.verdict = "likely_direct".to_string();
            let mut advice = "锥形 NAT（端点无关映射".to_string();
            match prof.filtering_behavior.as_str() {
                "endpoint_independent" => advice.push_str("、端点无关过滤"),
                "address_dependent" => advice.push_str("、地址相关过滤"),
                "address_and_port_dependent" => advice.push_str("、地址端口相关过滤"),
                _ => {}
            }
            advice.push_str("）：与同为锥形的对端几乎必成；对方对称时需你主动先发。用 check 实测确认。");
            prof.advice = advice;
        }
    }
    if prof.local_addr_kind == "cgnat" {
        prof.advice
            .push_str("（本机在 100.64/10，属运营商级 NAT：直连成功率低，优先 relay。）");
    }
    prof
}

/// RFC 5780 的映射/过滤行为测试（`OTHER-ADDRESS` + `CHANGE-REQUEST`）。
async fn rfc5780(
    sock: &UdpSocket,
    server: &str,
    other: SocketAddr,
    timeout: Duration,
) -> Option<(String, String)> {
    let primary = resolve_stun(server).await?;
    if !other.is_ipv4() {
        return None;
    }
    let m1 = exchange(sock, primary, 0, false, timeout).await?;
    let m1m = m1.xor_mapped?;
    let m2 = exchange(
        sock,
        SocketAddr::new(other.ip(), primary.port()),
        0,
        false,
        timeout,
    )
    .await?;
    let mapping = match m2.xor_mapped {
        Some(x) if x == m1m => "endpoint_independent".to_string(),
        _ => {
            let m3 = exchange(sock, other, 0, false, timeout).await?;
            match (m3.xor_mapped, m2.xor_mapped) {
                (Some(a), Some(b)) if a == b => "address_dependent".to_string(),
                _ => "address_and_port_dependent".to_string(),
            }
        }
    };
    // 过滤：能收到来自另一个地址的响应 = 端点无关过滤。
    if exchange(sock, primary, CHANGE_IP_PORT, true, timeout).await.is_some() {
        return Some((mapping, "endpoint_independent".to_string()));
    }
    if exchange(sock, primary, CHANGE_PORT, true, timeout).await.is_some() {
        return Some((mapping, "address_dependent".to_string()));
    }
    Some((mapping, "address_and_port_dependent".to_string()))
}

/* ---------------- ② check：真实连通性检查 ---------------- */

/// 一次打洞检查的结果。
#[derive(Debug, Clone, Default, Serialize)]
pub struct CheckResult {
    pub ok: bool,
    #[serde(rename = "myMapped")]
    pub my_mapped: String,
    #[serde(rename = "peerMapped")]
    pub peer_mapped: String,
    #[serde(rename = "rttMs")]
    pub rtt_ms: i64,
    #[serde(rename = "peerRequestsSeen")]
    pub peer_requests_seen: i64,
    #[serde(rename = "peerResponsesSeen")]
    pub peer_responses_seen: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub advice: String,
    #[serde(rename = "stunUsed", skip_serializing_if = "String::is_empty")]
    pub stun_used: String,
}

/// 从一个已绑定的 UDP socket 上探自己的公网映射（打洞前必须先知道这个）。
///
/// 连试 3 轮、每轮间隔 600ms：STUN 的第一包丢一点很常见，一次失败不代表出站被封。
async fn discover_mapping(
    sock: &UdpSocket,
    servers: &[String],
    timeout: Duration,
) -> Result<(SocketAddr, String), String> {
    let servers: Vec<String> = if servers.is_empty() {
        DEFAULT_STUN.iter().map(|s| s.to_string()).collect()
    } else {
        servers.to_vec()
    };
    let mut last_err: Option<String> = None;
    for _ in 0..3 {
        for s in &servers {
            let Some(dst) = resolve_stun(s).await else {
                last_err = Some(format!("无法解析 STUN 地址 {s}"));
                continue;
            };
            match exchange(sock, dst, 0, false, timeout).await {
                Some(rep) => match rep.xor_mapped {
                    Some(m) => return Ok((m, s.clone())),
                    None => continue,
                },
                None => continue,
            }
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
    }
    Err(last_err.unwrap_or_else(|| "所有 STUN 都无响应".to_string()))
}

/// 与一个已知的映射地址做**双向对打**：我发 Binding 请求，同时回答对方发来的请求。
/// 收到对方的任何 UDP 包即证明「这条路径的 NAT 允许入向」—— 这就是 ICE connectivity check。
///
/// 用法：两端同时发起（各自 check 对方），或一端 check、另一端开着 responder。
pub async fn check(
    peer: SocketAddr,
    servers: &[String],
    wait: Duration,
    timeout: Duration,
) -> CheckResult {
    let mut res = CheckResult {
        peer_mapped: peer.to_string(),
        ..Default::default()
    };
    let wait = if wait.is_zero() {
        Duration::from_secs(10)
    } else {
        wait
    };
    let sock = match UdpSocket::bind(("0.0.0.0", 0)).await {
        Ok(s) => s,
        Err(e) => {
            res.reason = format!("无法创建 UDP socket：{e}");
            return res;
        }
    };
    let sock = Arc::new(sock);
    match discover_mapping(&sock, servers, timeout).await {
        Ok((mapped, used)) => {
            res.my_mapped = mapped.to_string();
            res.stun_used = used;
        }
        Err(e) => {
            res.reason = format!("拿不到自己的公网映射：{e}（UDP 出站可能被封；先看 p2p self 的结论）");
            res.advice = "先跑 `ncc registry p2p self` 看 NAT 画像；企业网禁 UDP 时只能用客户自托管 \
                          TURN 或中心搬运。"
                .to_string();
            return res;
        }
    }

    // 对打：每 300ms 一个 Binding 请求（tokio 的 interval 首次会立即触发，先吃掉一次）。
    let stop = Arc::new(Notify::new());
    let sender = {
        let s = sock.clone();
        let stop = stop.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PUNCH_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tokio::select! {
                    _ = stop.notified() => return,
                    _ = tick.tick() => {
                        let _ = s.send_to(&binding_request(&new_txid(), 0, false), peer).await;
                    }
                }
            }
        })
    };

    let deadline = Instant::now() + wait;
    let mut buf = [0u8; 1500];
    let mut last_sent: Option<Instant> = None;
    while Instant::now() < deadline {
        if let Ok(Ok((n, from))) = tokio::time::timeout(
            Duration::from_millis(200),
            sock.recv_from(&mut buf),
        )
        .await
        {
            if from.ip() == peer.ip() && from.port() == peer.port() && n >= 20 {
                match be16(&buf[0..2]) {
                    BINDING_REQ => {
                        res.peer_requests_seen += 1;
                        if let Some(reply) = binding_success(&buf[8..20], &from) {
                            let _ = sock.send_to(&reply, from).await;
                        }
                        res.ok = true;
                    }
                    BINDING_OK => {
                        res.peer_responses_seen += 1;
                        if res.rtt_ms == 0 {
                            res.rtt_ms = last_sent
                                .map(|t| t.elapsed().as_millis() as i64)
                                .unwrap_or(0);
                        }
                        res.ok = true;
                    }
                    _ => {}
                }
            }
        }
        if (res.peer_requests_seen > 0 || res.peer_responses_seen > 0) && res.rtt_ms > 0 {
            break;
        }
        if last_sent.map_or(true, |t| t.elapsed() >= PUNCH_EVERY) {
            let _ = sock.send_to(&binding_request(&new_txid(), 0, false), peer).await;
            last_sent = Some(Instant::now());
        }
    }
    stop.notify_one();
    let _ = sender.await;
    if !res.ok {
        res.reason = "对端没有任何回包".to_string();
        res.advice = "常见原因：①对端没在跑（check 或 p2p serve）；②双方 NAT 不兼容\
                      （尤其对称 NAT）；③企业网禁 UDP。"
            .to_string();
    }
    res
}

/* ---------------- ③ responder：让本节点可被「打进来」 ---------------- */

/// 反向打洞 / 主动探测的节拍（Go 侧同为 300ms）。
const PUNCH_EVERY: Duration = Duration::from_millis(300);

/// 在后台回答 STUN Binding 请求，让别的节点/CLI 能打洞过来。
///
/// ⚠️ 实测结论（别踩）：**被动应答只在「端点无关过滤」的 NAT 上够用**。若本机 NAT 是
/// address/port-dependent filtering（家用与企业网都常见），它会丢掉「我没先发过的对端」
/// 发来的包 —— 于是入口一个包都收不到（实测「已应答 0 次」）。解法就是打洞的本质：
/// **双方同时向对方的映射地址发包**。所以下面带了 peers：已知对端映射时，入口会主动
/// 反向发给它，两边同时开孔，包就能互相通过。
///
/// 这也是**显式开关**（`NCCR_P2P_SERVE=1` 或管理接口打开）：它确实对外暴露一个 UDP 入口。
pub struct Responder {
    addr: String,
    mapped: RwLock<String>,
    /// 反向打洞对端（对端的映射地址）—— 后台任务与处理器共享，所以套 Arc。
    peers: Arc<RwLock<Vec<SocketAddr>>>,
    reqs: Arc<AtomicI64>,
    resps: Arc<AtomicI64>,
    stop: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Responder {
    /// 绑定一个 UDP 端口并开始应答；同时探出自己的公网映射（供对端使用）。
    pub async fn start(servers: &[String], timeout: Duration) -> std::io::Result<Responder> {
        let conn = Arc::new(UdpSocket::bind(("0.0.0.0", 0)).await?);
        let addr = conn.local_addr()?.to_string();
        let mapped = match discover_mapping(&conn, servers, timeout).await {
            Ok((m, _)) => m.to_string(),
            Err(_) => String::new(),
        };
        let stop = Arc::new(Notify::new());
        let peers: Arc<RwLock<Vec<SocketAddr>>> = Arc::new(RwLock::new(Vec::new()));
        let reqs = Arc::new(AtomicI64::new(0));
        let resps = Arc::new(AtomicI64::new(0));
        let task = {
            let conn = conn.clone();
            let stop = stop.clone();
            let peers = peers.clone();
            let reqs = reqs.clone();
            let resps = resps.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1500];
                let mut tick = tokio::time::interval(PUNCH_EVERY);
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tick.tick().await;
                loop {
                    tokio::select! {
                        _ = stop.notified() => return,
                        _ = tick.tick() => {
                            // 反向打洞：每 300ms 给已知对端的映射发一个 Binding 请求，
                            // 好让本机 NAT 为它开一个过滤孔（hole punching 的另一半）。
                            let targets = peers.read().map(|p| p.clone()).unwrap_or_default();
                            for p in targets {
                                let _ = conn.send_to(&binding_request(&new_txid(), 0, false), p).await;
                            }
                        }
                        r = conn.recv_from(&mut buf) => {
                            let Ok((n, from)) = r else { continue };
                            if n >= 20 && be16(&buf[0..2]) == BINDING_REQ {
                                reqs.fetch_add(1, Ordering::SeqCst);
                                if let Some(reply) = binding_success(&buf[8..20], &from) {
                                    let _ = conn.send_to(&reply, from).await;
                                }
                            } else if n >= 20 && be16(&buf[0..2]) == BINDING_OK {
                                // 反向打洞的对方回了我们（说明这条路径真通了）
                                resps.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                    }
                }
            })
        };
        // socket 只由后台任务持有：`close()` 一 abort 任务，端口就跟着释放。
        Ok(Responder {
            addr,
            mapped: RwLock::new(mapped),
            peers,
            reqs,
            resps,
            stop,
            task,
        })
    }

    /// 本地监听地址（`ip:port`）。
    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// 对端应当发往的地址（公网映射）；探不到时退回空串。
    pub fn mapped_addr(&self) -> String {
        self.mapped.read().map(|m| m.clone()).unwrap_or_default()
    }

    /// 已经应答过多少次打洞请求（运维观测用）。
    pub fn requests(&self) -> i64 {
        self.reqs.load(Ordering::SeqCst)
    }

    /// 反向打洞时收到过多少次对端回包（>0 表示这条路径真通了）。
    pub fn responses(&self) -> i64 {
        self.resps.load(Ordering::SeqCst)
    }

    /// 设置「反向打洞」对端映射地址（对端 `p2p serve` 报出的 mapped）。
    pub fn set_peers(&self, peers: Vec<SocketAddr>) {
        if let Ok(mut p) = self.peers.write() {
            *p = peers;
        }
    }

    /// 当前的反向打洞对端。
    pub fn peers(&self) -> Vec<SocketAddr> {
        self.peers.read().map(|p| p.clone()).unwrap_or_default()
    }

    /// 停止应答并释放端口。
    pub fn close(&self) {
        self.stop.notify_one();
        self.task.abort();
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.close();
    }
}

/// 解析对端映射地址（支持 `ip:port` 与 `host:port`）。
pub async fn parse_peer_addr(s: &str) -> Result<SocketAddr, String> {
    let raw = s.trim();
    if raw.is_empty() {
        return Err("对端地址为空（形如 1.2.3.4:5678）".to_string());
    }
    if !raw.contains(':') {
        return Err(format!("对端地址要带端口：{raw:?}"));
    }
    tokio::net::lookup_host(raw)
        .await
        .map_err(|e| format!("对端地址无法解析：{e}"))?
        .find(|a| a.is_ipv4())
        .ok_or_else(|| format!("对端地址无法解析：{raw}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use crate::store;

    fn be32(b: &[u8]) -> u32 {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }

    /// 入口监听在 `0.0.0.0`（Go 的 `Addr()` 也是这么报的），回环测试要显式拨 127.0.0.1
    /// —— 往 `0.0.0.0:port` 发包在 macOS 上直接 EHOSTUNREACH。
    fn loopback_of(listen: &str) -> SocketAddr {
        let port: u16 = listen.rsplit(':').next().unwrap().parse().unwrap();
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    use axum::http::Request;
    use ncc_core::storage::LocalStorage;
    use tower::ServiceExt;

    /// 起一个本地假 STUN 服务器：只回 XOR-MAPPED-ADDRESS = 观察到的源地址。
    ///
    /// 有了它，probe / check / responder 都能在回环上跑完整链路 —— 无外网的机器上
    /// 也能验证「判定逻辑 + 应答逻辑」，而不是只测几个纯函数。
    async fn fake_stun() -> SocketAddr {
        let sock = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                if n < 20 || be16(&buf[0..2]) != BINDING_REQ {
                    continue;
                }
                if let Some(reply) = binding_success(&buf[8..20], &from) {
                    let _ = sock.send_to(&reply, from).await;
                }
            }
        });
        addr
    }

    /// 哑 STUN：回一个**没有映射属性**的成功响应（探测「拿不到映射」的路径又不想等超时）。
    async fn mute_stun() -> SocketAddr {
        let sock = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                    return;
                };
                if n < 20 || be16(&buf[0..2]) != BINDING_REQ {
                    continue;
                }
                let _ = sock.send_to(&build_message(BINDING_OK, &buf[8..20], &[]), from).await;
            }
        });
        addr
    }

    /* ---------- 纯函数层 ---------- */

    #[test]
    fn 事务id与消息头往返() {
        let a = new_txid();
        let b = new_txid();
        assert_ne!(a, b);
        let req = binding_request(&a, 0, false);
        assert_eq!(req.len(), 20);
        assert_eq!(be16(&req[0..2]), BINDING_REQ);
        assert_eq!(be16(&req[2..4]), 0);
        assert_eq!(be32(&req[4..8]), MAGIC_COOKIE);
        assert_eq!(&req[8..20], &a);
        assert!(parse_attrs(&req).is_empty());

        // 带 CHANGE-REQUEST
        let chg = binding_request(&a, CHANGE_PORT, true);
        let attrs = parse_attrs(&chg);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].0, ATTR_CHANGE);
        assert_eq!(be32(&attrs[0].1), CHANGE_PORT);
        // 属性总长（含 4 字节头）要 4 字节对齐
        assert_eq!((chg.len() - 20) % 4, 0);
    }

    #[test]
    fn xor映射属性编解码() {
        let observed: SocketAddr = "203.0.113.7:51820".parse().unwrap();
        let msg = binding_success(&new_txid(), &observed).unwrap();
        assert_eq!(be16(&msg[0..2]), BINDING_OK);
        let attrs = parse_attrs(&msg);
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].0, ATTR_XOR_MAPPED);
        assert_eq!(attrs[0].1.len(), 8);
        assert_eq!(decode_xor_mapped(&attrs[0].1), Some(observed));

        // IPv6 不做（无需打洞）
        let v6: SocketAddr = "[2001:db8::1]:3478".parse().unwrap();
        assert!(binding_success(&new_txid(), &v6).is_none());
        // 非 IPv4 家族的属性值不认
        assert!(decode_xor_mapped(&[0x00, 0x02, 0, 0, 0, 0, 0, 0]).is_none());
        assert!(decode_plain(&[0x00, 0x01, 0x1f, 0x90, 10, 0, 0, 1]).is_some());
    }

    #[test]
    fn 地址归类() {
        assert_eq!(host_addr_kind(Ipv4Addr::new(100, 64, 1, 1)), "cgnat");
        assert_eq!(host_addr_kind(Ipv4Addr::new(100, 127, 1, 1)), "cgnat");
        assert_eq!(host_addr_kind(Ipv4Addr::new(100, 128, 1, 1)), "public");
        assert_eq!(host_addr_kind(Ipv4Addr::new(10, 1, 1, 1)), "private");
        assert_eq!(host_addr_kind(Ipv4Addr::new(192, 168, 1, 1)), "private");
        assert_eq!(host_addr_kind(Ipv4Addr::new(172, 16, 0, 1)), "private");
        assert_eq!(host_addr_kind(Ipv4Addr::new(172, 32, 0, 1)), "public");
        assert_eq!(host_addr_kind(Ipv4Addr::new(8, 8, 8, 8)), "public");
    }

    #[tokio::test]
    async fn 对端地址解析与错误文案() {
        let a = parse_peer_addr("127.0.0.1:3478").await.unwrap();
        assert_eq!(a.port(), 3478);
        assert_eq!(
            parse_peer_addr("   ").await.unwrap_err(),
            "对端地址为空（形如 1.2.3.4:5678）"
        );
        assert_eq!(
            parse_peer_addr("127.0.0.1").await.unwrap_err(),
            "对端地址要带端口：\"127.0.0.1\""
        );
        // 端口非法：本地就能判出来，不去碰 DNS（测试不该依赖网络）
        let e = parse_peer_addr("1.2.3.4:70000").await.unwrap_err();
        assert!(e.starts_with("对端地址无法解析："), "{e}");
    }

    #[test]
    fn 入口状态快照() {
        let off = serve_state_of(None);
        assert_eq!(off["on"], false);
        assert!(off["hint"].as_str().unwrap().contains("NCCR_P2P_SERVE=1"));
        assert!(off.get("listen").is_none());
    }

    /* ---------- 回环上的真实链路 ---------- */

    #[tokio::test]
    async fn probe_一台stun判不出映射行为() {
        let stun = fake_stun().await;
        let prof = probe(&[stun.to_string()], Duration::from_secs(3)).await;
        assert_eq!(prof.servers_tried, 1);
        assert_eq!(prof.servers_reached, 1);
        assert_eq!(prof.mapping_behavior, "unknown");
        assert_eq!(prof.mapping_method, "multi-stun");
        assert_eq!(prof.filtering_method, "unsupported");
        assert_eq!(prof.filtering_behavior, "");
        assert!(!prof.rfc5780);
        assert_eq!(prof.verdict, "unknown");
        assert_eq!(prof.public_ip, "127.0.0.1");
        assert!(prof.mapped.starts_with("127.0.0.1:"));
        assert!(prof.advice.contains("RFC 5780"));
        assert!(prof.local_port > 0);
        // 回环 + 无 OTHER-ADDRESS：过滤行为判不了，所以只给「多配几台 STUN」这一句
        assert!(!prof.advice.contains("锥形 NAT"));
    }

    #[tokio::test]
    async fn probe_两台stun看到同一映射() {
        let a = fake_stun().await;
        let b = fake_stun().await;
        let prof = probe(&[a.to_string(), b.to_string()], Duration::from_secs(3)).await;
        assert_eq!(prof.servers_reached, 2);
        assert_eq!(prof.mapping_behavior, "endpoint_independent");
        // 本机映射若正好等于出网 IP（不可能是回环）才叫 direct，两种都算「能直连」
        assert!(
            prof.verdict == "likely_direct" || prof.verdict == "direct",
            "{}",
            prof.verdict
        );
        assert!(prof.advice.contains("锥形 NAT"));
    }

    #[tokio::test]
    async fn probe_全连不上回blocked而不是报错() {
        // 回环上没人监听的端口：包发得出去、没人应答，超时即放弃
        let prof = probe(&["127.0.0.1:1".to_string()], Duration::from_millis(200)).await;
        assert_eq!(prof.servers_reached, 0);
        assert_eq!(prof.mapping_behavior, "unknown");
        assert_eq!(prof.verdict, "blocked");
        assert!(prof.mapped.is_empty());
        assert!(prof.advice.starts_with("所有 STUN 都没响应"));
    }

    #[tokio::test]
    async fn stun列表解析() {
        let a = resolve_stun("stun:127.0.0.1:3479").await.unwrap();
        assert_eq!(a.to_string(), "127.0.0.1:3479");
        // 缺端口补默认端口
        let b = resolve_stun("127.0.0.1").await.unwrap();
        assert_eq!(b.port(), DEFAULT_STUN_PORT);
        // turn: 前缀同样认
        let c = resolve_stun("turn:127.0.0.1:9999?transport=udp").await.unwrap();
        assert_eq!(c.port(), 9999);
        assert!(resolve_stun("").await.is_none());
    }

    #[tokio::test]
    async fn check_与入口真实对打() {
        let stun = fake_stun().await;
        let servers = vec![stun.to_string()];
        let r = Responder::start(&servers, Duration::from_secs(3)).await.unwrap();
        assert!(r.mapped_addr().starts_with("127.0.0.1:"));
        let peer = loopback_of(r.addr());
        let res = check(peer, &servers, Duration::from_secs(5), Duration::from_secs(3)).await;
        assert!(res.ok, "{res:?}");
        assert_eq!(res.peer_mapped, peer.to_string());
        assert!(!res.my_mapped.is_empty());
        assert_eq!(res.stun_used, stun.to_string());
        assert!(res.peer_responses_seen >= 1, "{res:?}");
        assert!(res.reason.is_empty(), "{res:?}");
        assert!(r.requests() >= 1, "入口应收到过打洞请求");
        r.close();
    }

    #[tokio::test]
    async fn 入口_应答与反向打洞() {
        let stun = fake_stun().await;
        let r = Responder::start(&[stun.to_string()], Duration::from_secs(3))
            .await
            .unwrap();
        assert_eq!(serve_state_of(Some(&r))["on"], true);
        assert_eq!(serve_state_of(Some(&r))["listen"], r.addr());
        let listen = loopback_of(r.addr());

        // 一个「对端」：先发一个 Binding 请求，之后对入口的反向打洞回包
        let client = UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
        let client_addr = client.local_addr().unwrap();
        r.set_peers(vec![client_addr]);
        assert_eq!(serve_state_of(Some(&r))["peers"][0], client_addr.to_string());
        client
            .send_to(&binding_request(&new_txid(), 0, false), listen)
            .await
            .unwrap();

        let mut buf = [0u8; 1500];
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut answered = false;
        while Instant::now() < deadline && !(answered && r.responses() > 0) {
            if let Ok(Ok((n, from))) =
                tokio::time::timeout(Duration::from_millis(200), client.recv_from(&mut buf)).await
            {
                if n >= 20 && be16(&buf[0..2]) == BINDING_REQ {
                    // 反向打洞：回一个带 XOR-MAPPED（即入口映射）的成功响应
                    if let Some(reply) = binding_success(&buf[8..20], &from) {
                        let _ = client.send_to(&reply, from).await;
                    }
                } else if n >= 20 && be16(&buf[0..2]) == BINDING_OK {
                    answered = true;
                }
            }
        }
        assert!(answered, "入口应对 Binding 请求回一个成功响应");
        assert_eq!(r.requests(), 1);
        assert!(r.responses() >= 1, "反向打洞的回包应被记数");

        // 关掉之后：端口不再应答（UDP 无连接，靠「不再收到回包」判断）
        r.close();
        r.set_peers(vec![]);
        assert!(r.peers().is_empty());
        assert_eq!(r.requests(), 1);
    }

    #[tokio::test]
    async fn check_对端不回包时报原因() {
        let stun = fake_stun().await;
        // 对端地址没人监听：自己映射拿得到，但等满 wait 也收不到回包
        let res = check(
            "127.0.0.1:1".parse().unwrap(),
            &[stun.to_string()],
            Duration::from_millis(800),
            Duration::from_secs(3),
        )
        .await;
        assert!(!res.ok);
        assert!(!res.my_mapped.is_empty());
        assert_eq!(res.reason, "对端没有任何回包");
        assert!(res.advice.contains("对端没在跑"));
    }

    #[tokio::test]
    async fn check_拿不到映射时给出出站结论() {
        let stun = mute_stun().await;
        let res = check(
            "127.0.0.1:1".parse().unwrap(),
            &[stun.to_string()],
            Duration::from_millis(300),
            Duration::from_millis(300),
        )
        .await;
        assert!(!res.ok);
        assert!(res.my_mapped.is_empty());
        assert!(res.reason.starts_with("拿不到自己的公网映射：所有 STUN 都无响应"), "{res:?}");
        assert!(res.advice.contains("p2p self"));
    }

    /* ---------- HTTP 层 ---------- */

    fn test_dir(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-blobs")
            .join(format!("p2p-{}-{name}", std::process::id()))
    }

    async fn state(name: &str, stun: &[SocketAddr], auto_serve: bool) -> AppState {
        let dir = std::env::temp_dir().join(format!("ncc-p2p-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = ncc_core::pool::open_sqlite(&dir.join("t.db")).await.unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL).await.unwrap();
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://10.0.0.9:8282".to_string();
        cfg.jwt_secret = "test-secret".to_string();
        cfg.node_id = "ND-TEST".to_string();
        cfg.node_name = "测试节点".to_string();
        cfg.p2p_stun = stun.iter().map(|s| s.to_string()).collect();
        cfg.p2p_turn = vec!["turn:127.0.0.1:3478".to_string()];
        cfg.p2p_serve = auto_serve;
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
        Router::new().merge(routes()).with_state(state.clone())
    }

    /// 建一个用户 + 一把带指定作用域的 API-Key。
    async fn key_with(state: &AppState, uid: &str, scopes: &[&str]) -> String {
        let u = store::users::create(
            state.pool(),
            &format!("用户{uid}"),
            &format!("{uid}@x.com"),
            "h",
        )
        .await
        .unwrap();
        store::namespaces::create_account(state.pool(), &u.id, &u.name, uid)
            .await
            .unwrap();
        let owned: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        let (_k, secret) = store::apikeys::create(state.pool(), &u.id, "t", &owned)
            .await
            .unwrap();
        secret
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    fn json_req(method: &str, path: &str, bearer: &str, body: Value) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if !bearer.is_empty() {
            b = b.header("authorization", format!("Bearer {bearer}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    fn get_req(path: &str, bearer: &str) -> Request<Body> {
        let mut b = Request::builder().uri(path);
        if !bearer.is_empty() {
            b = b.header("authorization", format!("Bearer {bearer}"));
        }
        b.body(Body::empty()).unwrap()
    }

    /// 入口是**进程内单例**：碰它的测试要串行，别互相把开关拨来拨去。
    static SERVE_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    async fn reset_serve() {
        if let Some(r) = serve_slot().lock().await.take() {
            r.close();
        }
    }

    #[tokio::test]
    async fn 整站路由表_本族端点已挂载() {
        let st = state("router", &[], false).await;
        let app = crate::router::build(&st);
        let (code, v) = call(&app, get_req("/api/p2p/self", "")).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
        let (code, v) = call(&app, get_req("/api/p2p/serve", "")).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
        let (code, v) = call(
            &app,
            json_req("POST", "/api/p2p/check", "", json!({})),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{v}");
    }

    #[tokio::test]
    async fn self_需p2p读作用域并回画像() {
        let stun = fake_stun().await;
        let st = state("self", &[stun], false).await;
        let app = app(&st);
        let reader = key_with(&st, "U-1", &["p2p:read"]).await;
        let writer = key_with(&st, "U-2", &["p2p:write"]).await;

        let (code, _) = call(&app, get_req("/p2p/self", "")).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);

        // 只有 p2p:write（不含 p2p:read）→ 403：读与写是两条独立的路
        let (code, _) = call(&app, get_req("/p2p/self", &writer)).await;
        assert_eq!(code, StatusCode::FORBIDDEN);

        let (code, v) = call(&app, get_req("/p2p/self", &reader)).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["node"]["id"], "ND-TEST");
        assert_eq!(v["node"]["name"], "测试节点");
        assert_eq!(v["node"]["role"], "master");
        assert_eq!(v["profile"]["serversReached"], 1);
        assert_eq!(v["profile"]["servers"][0], stun.to_string());
        assert_eq!(v["profile"]["mappingBehavior"], "unknown");
        assert_eq!(v["profile"]["verdict"], "unknown");
        // 只有 publicIp / mapped 带 omitempty；其余字段即使为空也要出现（与 Go 的 tag 一致）
        assert_eq!(v["profile"]["filteringBehavior"], "");
        assert_eq!(v["profile"]["mappingMethod"], "multi-stun");
        assert_eq!(v["profile"]["rfc5780Supported"], false);
        assert_eq!(v["profile"]["advice"].as_str().unwrap().contains("RFC 5780"), true);
        assert_eq!(v["ice"]["stun"][0], stun.to_string());
        assert_eq!(v["ice"]["turn"][0], "turn:127.0.0.1:3478");
        // 没开入口时给一句怎么开
        assert_eq!(v["serve"]["on"], false);
        assert!(v["serve"]["hint"].as_str().unwrap().contains("NCCR_P2P_SERVE=1"));
    }

    #[tokio::test]
    async fn 未配stun时用默认列表() {
        let st = state("default-stun", &[], false).await;
        assert_eq!(
            stun_servers(&st),
            DEFAULT_STUN.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn check_参数校验与成功路径() {
        let stun = fake_stun().await;
        let st = state("check", &[stun], false).await;
        let app = app(&st);
        let writer = key_with(&st, "U-1", &["p2p:write"]).await;

        // 体不是 JSON → 400 请求体格式错误
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri("/p2p/check")
                .header("authorization", format!("Bearer {writer}"))
                .header("content-type", "application/json")
                .body(Body::from("{不是 JSON"))
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "请求体格式错误");

        // 顶层是数组（serde 本会按字段顺序塞进去）→ 400，与 Go 一致
        let (code, v) = call(
            &app,
            json_req("POST", "/p2p/check", &writer, json!(["127.0.0.1:1"])),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(v["error"]["message"], "请求体格式错误");

        // peer 为空 → Go 的原话
        let (code, v) = call(
            &app,
            json_req("POST", "/p2p/check", &writer, json!({"peer": " "})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "对端地址为空（形如 1.2.3.4:5678）");

        // 缺端口
        let (code, v) = call(
            &app,
            json_req("POST", "/p2p/check", &writer, json!({"peer": "127.0.0.1"})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "对端地址要带端口：\"127.0.0.1\"");

        // waitSec 上限
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/p2p/check",
                &writer,
                json!({"peer": "127.0.0.1:1", "waitSec": 61}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "waitSec 最长 60 秒");

        // 成功路径：拿假 STUN 当「对端」，它会回 Binding 成功响应 → 打通
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/p2p/check",
                &writer,
                json!({"peer": stun.to_string(), "waitSec": 3}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["result"]["ok"], true);
        assert_eq!(v["result"]["peerMapped"], stun.to_string());
        assert_eq!(v["result"]["stunUsed"], stun.to_string());
        assert!(v["result"]["myMapped"].is_string());
        assert!(v["result"]["peerResponsesSeen"].as_i64().unwrap() >= 1);
        // 没发生的事不编字段（Go 的 omitempty）
        assert!(v["result"].get("reason").is_none());
    }

    #[tokio::test]
    async fn check_本地出站拿不到映射时回503() {
        let mute = mute_stun().await;
        let st = state("check-503", &[mute], false).await;
        let app = app(&st);
        let writer = key_with(&st, "U-1", &["p2p:write"]).await;
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/p2p/check",
                &writer,
                json!({"peer": "127.0.0.1:1", "waitSec": 1}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{v}");
        assert_eq!(v["error"]["code"], "p2p_probe_failed");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("拿不到自己的公网映射"));
    }

    #[tokio::test]
    async fn serve_开关与状态() {
        let _g = SERVE_TEST_LOCK.lock().await;
        reset_serve().await;
        let stun = fake_stun().await;
        let st = state("serve", &[stun], false).await;
        let app = app(&st);
        let reader = key_with(&st, "U-1", &["p2p:read"]).await;
        let writer = key_with(&st, "U-2", &["p2p:write"]).await;

        // 读状态：默认关
        let (code, v) = call(&app, get_req("/p2p/serve", &reader)).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["serve"]["on"], false);
        assert!(v["serve"]["hint"].as_str().unwrap().contains("p2p serve --on"));

        // 开：缺 on 字段 → 400（Go 的原话）
        let (code, v) = call(&app, json_req("POST", "/p2p/serve", &writer, json!({}))).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], SERVE_HINT);

        // 开 + 指定对端
        let (code, v) = call(
            &app,
            json_req(
                "POST",
                "/p2p/serve",
                &writer,
                json!({"on": true, "peer": "127.0.0.1:9999"}),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["serve"]["on"], true);
        assert_eq!(v["serve"]["peers"][0], "127.0.0.1:9999");
        assert_eq!(v["serve"]["requestsTaken"], 0);
        assert!(!v["serve"]["listen"].as_str().unwrap().is_empty());
        assert!(v["serve"]["mapped"].as_str().unwrap().starts_with("127.0.0.1:"));
        assert!(v["serve"]["note"].as_str().unwrap().contains("address/port-dependent"));

        // 再开一次（幂等）：对端不被空 peer 抹掉
        let (code, v) = call(
            &app,
            json_req("POST", "/p2p/serve", &writer, json!({"on": true})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["serve"]["peers"][0], "127.0.0.1:9999");

        // peer 格式不对
        let (code, v) = call(
            &app,
            json_req("POST", "/p2p/serve", &writer, json!({"on": true, "peer": "127.0.0.1"})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("peer 格式不对：对端地址要带端口"));

        // 关
        let (code, v) = call(
            &app,
            json_req("POST", "/p2p/serve", &writer, json!({"on": false})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["serve"]["on"], false);
        reset_serve().await;
    }

    #[tokio::test]
    async fn serve_NCCR_P2P_SERVE开启后自开() {
        let _g = SERVE_TEST_LOCK.lock().await;
        reset_serve().await;
        let stun = fake_stun().await;
        let st = state("serve-auto", &[stun], true).await;
        let app = app(&st);
        let reader = key_with(&st, "U-1", &["p2p:read"]).await;

        let (code, v) = call(&app, get_req("/p2p/serve", &reader)).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["serve"]["on"], true, "{v}");
        // 幂等：再问一次还是同一个（不会开第二个端口）
        let (_code, v2) = call(&app, get_req("/p2p/serve", &reader)).await;
        assert_eq!(v2["serve"]["listen"], v["serve"]["listen"]);
        reset_serve().await;
    }
}
