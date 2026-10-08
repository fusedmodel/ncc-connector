//! NCC Agent Share（内网节点侧，原实现 `ncc-registry/httpapi/agentcards.go`）。
//!
//! `ncc agent share` / `ncc agent add` 在本节点上跑，与云端 ncc-platform 的
//! `/api/agent-cards` **同形**：同一份客户端把目标切到本节点就能用，一行都不用改。
//! 刻意不同的只有一处：token 按本仓规矩**只存 sha256**（所以列表里回不出可点链接）。
//!
//! 红线（与 `ncc-platform/prd/ncc-agent-share.md` §6 逐条对齐）：
//!  1. 名片 ≠ 分发渠道：不展示、不检索、没有全站列表；token 是秘密，可撤销 / 可过期 / 可限次。
//!  2. 名片 ≠ 授权：收下只代表**找得到**；私有制品 / 私有节点仍要 `grant`。
//!  3. 收下 ≠ 执行：服务端只投递字节。
//!  4. 字节即事实：sha256 与 hur.json 都由服务端从**收到的字节**算/读（不信客户端报的）。
//!
//! 刻意的取舍（多是「本 crate 不能新增依赖」逼出来的）：
//!
//! * **字节走 `state.blobs()`**，与 Go 一致地放进同一份本地存储（也挂在 `/blobs`
//!   静态路由下）—— 对象名 `agent-cards/<AC-…>.hur` 里的 id 随机，且 token 本身
//!   只存在链接里，所以不额外给名片开静态路径（复用 Blob，不新造存取路径）。
//! * **gzip / zip / DEFLATE 自己实现**：Go 用的是 `archive/zip` + `compress/gzip`，
//!   而本工作区不能加依赖。解压全程**不落盘**、解压后设上限 —— 这点比 Go 更严：
//!   压缩炸弹在这里被直接拒掉，而不是被悄悄截断。
//! * 不认 zip64（名片包不该到那个量级）：遇到就明确报错，而不是给出半个包。
//! * 落地页对插值做 HTML 转义（与 Go 的 `html.EscapeString` 同一套）。
//! * `node` 参数解析用一次性的窄查询（只取 id/kind/name/visibility/归属）：
//!   `store/nodes.rs` 属于别族，本次不动它。

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use ncc_core::error::{ApiError, ApiResult};
use ncc_core::ids::new_id;
use ncc_core::web;

use crate::config::Config;
use crate::httpapi::shares::{audit, ensure_admin, esc, page_params};
use crate::httpapi::{helpers, AppState, Auth};
use crate::store;

/// 名片里的包字节上限（8MB）—— 名片是点到点投递，不是分发渠道。
const MAX_CARD_SIZE: usize = 8 << 20;
/// 解压后上限：压缩炸弹在这里被挡住。
const MAX_CARD_UNZIP: usize = 64 << 20;
/// 只读 hur.json，且不给它撑爆内存的机会。
const MAX_CARD_MANIFEST: usize = 64 << 10;
const CARD_DEFAULT_TTL_HOURS: i64 = 7 * 24;
const CARD_MAX_TTL_DAYS: i64 = 365;
const CARD_MAX_USES: i64 = 10000;
/// 对象名前缀（在 `state.blobs()` 里）。
const CARD_BLOB_PREFIX: &str = "agent-cards/";

/// 该族路由（相对 `/api`）。
///
/// 读接口不挂登录中间件：**token 本身就是秘密**；写接口要登录（与会话同规矩）。
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/agent-cards",
            get(list_agent_cards).post(create_agent_card),
        )
        .route(
            "/agent-cards/",
            get(list_agent_cards).post(create_agent_card),
        )
        .route(
            "/agent-cards/{token}",
            get(get_agent_card).delete(delete_agent_card),
        )
        .route("/agent-cards/{token}/blob", get(download_agent_card_blob))
        .route("/agent-cards/{token}/accept", post(accept_agent_card))
}

/// 落地页（人可读；设了口令先解锁）：与 `/s/<token>` 同一脾气 —— 拿到链接的人才看得到。
pub fn public_routes() -> Router<AppState> {
    Router::new().route("/a/{token}", get(render_card_page).post(unlock_card_page))
}

/* ---------------- .hur 包：从字节里读 hur.json ---------------- */

/// DEFLATE 比特流读取（RFC1951 是 LSB-first）。
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

/// 解一条 DEFLATE 流，最多产出 `limit` 字节。
///
/// 只解压、不校验 CRC：清单还要过一遍 JSON 解析，CRC 不是这里的判定依据。
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
        return Err(bad()); // 保留位非零：与 Go 的 gzip 一样直接拒
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
    inflate(&raw[p..], MAX_CARD_UNZIP).map_err(|_| "gzip 层解压失败".to_string())
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

/// 在 zip 里找 `want` 条目，返回 (压缩方式, 压缩大小, 原大小, 数据起始偏移)。
fn zip_find(zip: &[u8], want: &str) -> Result<Option<(u16, usize, usize, usize)>, String> {
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
    let eocd = eocd.ok_or_else(|| "没有 zip 结尾记录".to_string())?;
    let count = u16_at(zip, eocd + 10).ok_or("zip 结尾记录损坏")? as usize;
    let cd = u32_at(zip, eocd + 16).ok_or("zip 结尾记录损坏")? as usize;
    let mut p = cd;
    for _ in 0..count {
        if u32_at(zip, p) != Some(0x0201_4b50) {
            break;
        }
        let method = u16_at(zip, p + 10).ok_or("中央目录损坏")?;
        let csize = u32_at(zip, p + 20).ok_or("中央目录损坏")?;
        let usize_ = u32_at(zip, p + 24).ok_or("中央目录损坏")?;
        let name_len = u16_at(zip, p + 28).ok_or("中央目录损坏")? as usize;
        let extra_len = u16_at(zip, p + 30).ok_or("中央目录损坏")? as usize;
        let comment_len = u16_at(zip, p + 32).ok_or("中央目录损坏")? as usize;
        let local = u32_at(zip, p + 42).ok_or("中央目录损坏")? as usize;
        let name_end = p + 46 + name_len;
        if name_end > zip.len() {
            break;
        }
        if &zip[p + 46..name_end] == want.as_bytes() {
            if csize == u32::MAX || usize_ == u32::MAX {
                return Err("zip64 暂不支持".to_string());
            }
            if u32_at(zip, local) != Some(0x0403_4b50) {
                return Err("本地文件头损坏".to_string());
            }
            let lname = u16_at(zip, local + 26).ok_or("本地文件头损坏")? as usize;
            let lextra = u16_at(zip, local + 28).ok_or("本地文件头损坏")? as usize;
            return Ok(Some((
                method,
                csize as usize,
                usize_ as usize,
                local + 30 + lname + lextra,
            )));
        }
        p = p + 46 + name_len + extra_len + comment_len;
    }
    Ok(None)
}

/// 从 `.hur` 字节里读出 `hur.json` 原文。
///
/// 容器是 gzip 包住的 zip（老包是裸 zip，两种都要认）。**不落盘解包**：只开这一个条目、
/// 解压后大小设上限 —— 这是别人上传的字节，这里不该有「解压到磁盘」这种动作。
fn hur_manifest_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    let zip_owned;
    let zip_bytes: &[u8] = if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        zip_owned = gunzip(raw)?;
        &zip_owned
    } else {
        raw
    };
    let found = zip_find(zip_bytes, "hur.json")
        .map_err(|_| "既不是 gzip 也不是 zip —— 这不是一个 hur 包".to_string())?;
    let Some((method, csize, usize_, off)) = found else {
        return Err("包里没有 hur.json".to_string());
    };
    if usize_ > MAX_CARD_MANIFEST {
        return Err("包里的 hur.json 过大（>64KB）".to_string());
    }
    let slice = zip_bytes
        .get(off..off.saturating_add(csize))
        .ok_or_else(|| "读 hur.json 失败".to_string())?;
    let data = match method {
        0 => slice.to_vec(),
        8 => inflate(slice, MAX_CARD_MANIFEST + 1).map_err(|_| "读 hur.json 失败".to_string())?,
        _ => return Err("读 hur.json 失败".to_string()),
    };
    if data.len() > MAX_CARD_MANIFEST {
        return Err("包里的 hur.json 过大（>64KB）".to_string());
    }
    Ok(data)
}

#[derive(Debug, Default, Deserialize)]
struct CardManifest {
    #[serde(default)]
    spec: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    profile: String,
    #[serde(default, rename = "id")]
    id_: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
}

fn card_sha256(b: &[u8]) -> String {
    ncc_core::crypto::sha256_hex(b)
}

/// 截断到 `max` 个**字符**（不是字节）：中文名按字算才对得上 Go 的 `[]rune` 截断。
fn clamp_runes(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

/* ---------------- 状态与视图 ---------------- */

fn card_state_msg(reason: &str) -> String {
    match reason {
        "revoked" => "这张名片已被作者撤销".to_string(),
        "expired" => "这张名片已过有效期，请让作者重新生成一张".to_string(),
        "exhausted" => "这张名片的收下次数已用完，请让作者重新生成一张".to_string(),
        _ => "这张名片不可用".to_string(),
    }
}

fn card_link(cfg: &Config, token: &str) -> String {
    format!("{}/a/{}", cfg.public_url.trim_end_matches('/'), token)
}

/// 名片视图：**字段名与云端逐字对齐**（同一份客户端两处都能解析）。
///
/// `token` 只能来自**调用方这次请求的那条路径** —— 库里只存 token 的 sha256，
/// 谁也拿不回明文（这是本仓的规矩，比云端更保守）。所以：
///   · 单条接口（取名片 / 页面）能回出真实可用的 url 与 blob；
///   · 列表接口回不出来（作者手上也没明文），只给 hint —— 客户端会把链接那行省掉。
fn card_json(
    cfg: &Config,
    card: &store::agentcards::AgentCard,
    token: &str,
    author: Value,
) -> Value {
    let node = if !card.node_ref.is_empty() || !card.node_id.is_empty() {
        json!({"ref": card.node_ref, "id": card.node_id, "kind": card.node_kind, "label": card.node_label})
    } else {
        Value::Null
    };
    let (blob, url) = if token.is_empty() {
        (String::new(), String::new())
    } else {
        (
            format!("/api/agent-cards/{token}/blob"),
            card_link(cfg, token),
        )
    };
    json!({
        "id": card.id, "name": card.name, "note": card.note,
        "author": author,
        "agent": {
            "id": card.agent_id, "version": card.agent_version,
            "kind": card.agent_kind, "profile": card.agent_profile,
            "sha256": card.sha256, "bytes": card.size,
            "blob": blob,
        },
        "manifest": card.manifest_value(),
        "node": node,
        "expiresAt": card.expires_at_utc(), "uses": card.uses, "maxUses": card.max_uses,
        "hasPassword": !card.pass_hash.is_empty(), "revoked": card.revoked(),
        "state": card.state(), "hint": card.token_hint,
        "url": url,
    })
}

/// 作者公开信息。本节点只有 users 表（用户名 + 邮箱），
/// 与平台侧的字段名保持一致（handle 取不到就给空串，客户端会跳过）。
async fn card_author(state: &AppState, owner_id: &str) -> Value {
    let mut display = String::new();
    if let Ok(Some(u)) = store::users::by_id(state.pool(), owner_id).await {
        display = u.name;
    }
    json!({"userId": owner_id, "handle": "", "displayName": display})
}

/* ---------------- 请求体解析（multipart / 查询串 / 表单） ---------------- */

/// 极简 multipart/form-data 解析：只取字段名、文件名与内容。
///
/// Go 用标准库 `c.FormFile`；本 crate 不能加依赖，所以手写这一小段 ——
/// 够用即可：名片上传只有「一个 file 字段 + 几个文本字段」这一种形态。
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

/// `application/x-www-form-urlencoded` 里的一个字段（Go 的 `c.PostForm`）。
fn form_field(body: &[u8], content_type: &str, key: &str) -> String {
    if !content_type.contains("application/x-www-form-urlencoded") {
        return String::new();
    }
    let text = String::from_utf8_lossy(body);
    for pair in text.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return web::percent_decode(v);
            }
        }
    }
    String::new()
}

fn first_non_empty(vals: &[String]) -> String {
    vals.iter()
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// Go 的 `parseInt64Default`：出现任何非数字就当 0，超过上限就夹到「上限 + 1」。
fn parse_int64_default(s: &str) -> i64 {
    let mut n: i64 = 0;
    for ch in s.trim().chars() {
        if !ch.is_ascii_digit() {
            return 0;
        }
        n = n * 10 + (ch as i64 - '0' as i64);
        if n > CARD_MAX_USES {
            return CARD_MAX_USES + 1;
        }
    }
    n
}

/// Go 的 `time.ParseDuration` 子集（客户端把 `7d` 换算成 `168h` 再发过来）。
fn parse_go_duration(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if body == "0" {
        return Some(chrono::Duration::zero());
    }
    if body.is_empty() {
        return None;
    }
    let mut total_ms = 0f64;
    let mut rest = body;
    while !rest.is_empty() {
        let num_end = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        if num_end == 0 {
            return None;
        }
        let num: f64 = rest[..num_end].parse().ok()?;
        let unit_end = rest[num_end..]
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .map(|i| num_end + i)
            .unwrap_or(rest.len());
        let ms_per_unit = match &rest[num_end..unit_end] {
            "ns" => 1e-6,
            "us" | "µs" | "μs" => 1e-3,
            "ms" => 1.0,
            "s" => 1000.0,
            "m" => 60_000.0,
            "h" => 3_600_000.0,
            _ => return None,
        };
        total_ms += num * ms_per_unit;
        rest = &rest[unit_end..];
    }
    let ms = if neg { -total_ms } else { total_ms };
    Some(chrono::Duration::milliseconds(ms as i64))
}

/* ---------------- 创建 ---------------- */

/// POST /api/agent-cards —— body = `.hur` 字节（或 multipart `file`）。
///
/// 与平台侧同一套参数（查询串或表单）：name / note / node / key / uses / expiresAt / expires / profile。
async fn create_agent_card(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> ApiResult<Response> {
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("未认证或凭据无效（先 ncc login 或带 API-Key）"))?
        .clone();

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();

    let raw: Vec<u8>;
    let name: String;
    let note: String;
    let key: String;
    let node_ref: String;
    let profile: String;
    let expires_raw: String;
    let mut uses_form = String::new();

    if content_type.starts_with("multipart/form-data") {
        let boundary = content_type
            .split(';')
            .filter_map(|p| p.trim().strip_prefix("boundary="))
            .next()
            .map(|v| v.trim_matches('"').to_string())
            .unwrap_or_default();
        let parts = parse_multipart(&body, &boundary);
        let Some(file) = parts.iter().find(|(n, _, _)| n == "file") else {
            return Err(ApiError::bad_request("bad_request", "缺少 file 字段"));
        };
        if file.2.len() > MAX_CARD_SIZE {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "包超过 8MB",
            ));
        }
        raw = file.2.clone();
        let field = |k: &str| -> String {
            parts
                .iter()
                .find(|(n, _, _)| n == k)
                .map(|(_, _, c)| String::from_utf8_lossy(c).to_string())
                .unwrap_or_default()
        };
        name = field("name");
        note = field("note");
        key = field("key");
        node_ref = field("node");
        profile = field("profile");
        expires_raw = first_non_empty(&[field("expires"), field("expiresAt")]);
        uses_form = field("uses");
    } else {
        raw = body.to_vec();
        name = web::query(&uri, "name").unwrap_or_default();
        note = web::query(&uri, "note").unwrap_or_default();
        key = web::query(&uri, "key").unwrap_or_default();
        node_ref = web::query(&uri, "node").unwrap_or_default();
        profile = web::query(&uri, "profile").unwrap_or_default();
        expires_raw = first_non_empty(&[
            web::query(&uri, "expires").unwrap_or_default(),
            web::query(&uri, "expiresAt").unwrap_or_default(),
        ]);
    }
    let uses = parse_int64_default(&first_non_empty(&[
        web::query(&uri, "uses").unwrap_or_default(),
        uses_form,
    ]));

    if raw.is_empty() {
        return Err(ApiError::bad_request("bad_request", "上传内容为空"));
    }
    if raw.len() > MAX_CARD_SIZE {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "包超过 8MB",
        ));
    }

    // 清单从**字节**里读：读不出来就不是包，直接拒。
    let mb = hur_manifest_bytes(&raw)
        .map_err(|e| ApiError::bad_request("bad_request", format!("这不是一个 hur 包：{e}")))?;
    let mf: CardManifest = serde_json::from_slice(&mb)
        .map_err(|_| ApiError::bad_request("bad_request", "包里的 hur.json 不是合法 JSON"))?;
    if mf.spec.is_empty() || mf.id_.is_empty() || mf.version.is_empty() {
        return Err(ApiError::bad_request(
            "bad_request",
            "hur.json 缺 spec / id / version —— 不是一个完整的包",
        ));
    }
    // profile：老包清单里可能没写（「没写就按 kind 推导」这条规则住在 hur-core），
    // 那就用作者侧 CLI 解析出来的那个填上。只用于展示 —— 权威的 kind/id/version 来自字节。
    let mf_profile = if mf.profile.trim().is_empty() {
        profile.trim().to_string()
    } else {
        mf.profile.clone()
    };
    let mut display_name = name.trim().to_string();
    if display_name.is_empty() {
        display_name = mf.name.clone();
    }
    if display_name.is_empty() {
        display_name = mf.id_.clone();
    }
    if uses < 0 || uses > CARD_MAX_USES {
        return Err(ApiError::bad_request(
            "bad_request",
            "收下次数需在 0-10000 之间（0 = 不限）",
        ));
    }

    let mut pass_hash = String::new();
    let mut pass_salt = String::new();
    if !key.is_empty() {
        let n = key.chars().count();
        if !(3..=16).contains(&n) {
            return Err(ApiError::bad_request("bad_request", "访问 key 长度需 3-16 位"));
        }
        pass_salt = store::agentcards::new_salt();
        pass_hash = store::agentcards::hash_pass(&pass_salt, &key);
    }

    // 有效期：`expires`（Go duration，客户端把 7d 换成 168h）或 `expiresAt`（RFC3339）；
    // `none` = 不过期。缺省 7 天 —— 名片是「给指定的人」的东西，默认不该永不过期。
    let now = chrono::Local::now().fixed_offset();
    let mut expires_at: Option<String> = None;
    let v = expires_raw.trim().to_string();
    if !v.is_empty() {
        if !v.eq_ignore_ascii_case("none") {
            let t = match chrono::DateTime::parse_from_rfc3339(&v) {
                Ok(t) => t,
                Err(_) => {
                    let Some(d) = parse_go_duration(&v).filter(|d| *d > chrono::Duration::zero())
                    else {
                        return Err(ApiError::bad_request(
                            "bad_request",
                            "expires 用 Go duration（168h）或 none；绝对时间用 expiresAt（RFC3339）",
                        ));
                    };
                    now + d
                }
            };
            if t > now + chrono::Duration::days(CARD_MAX_TTL_DAYS) {
                return Err(ApiError::bad_request("bad_request", "有效期最长 365 天"));
            }
            expires_at = Some(ncc_core::timeutil::format_go(t));
        }
    } else {
        expires_at = Some(ncc_core::timeutil::format_go(
            now + chrono::Duration::hours(CARD_DEFAULT_TTL_HOURS),
        ));
    }

    // 节点那一侧：现在解析一次，别把「对方点开才发现连不上」留给接受方。
    let mut node_id = String::new();
    let mut node_kind = String::new();
    let mut node_label = String::new();
    let node_ref_trim = node_ref.trim().to_string();
    if !node_ref_trim.is_empty() && node_ref_trim != "none" {
        // 找不到节点时，Go 换成人话再回（状态码与 code 沿用底层错误）
        let hit = resolve_node(&state, &a.user_id, &node_ref_trim)
            .await
            .map_err(|e| {
                ApiError::new(
                    e.status,
                    &e.code,
                    "找不到这个节点（用 ND-… 或 @命名空间/节点slug）",
                )
            })?;
        if !can_see_node(&state, &hit, &a.user_id).await {
            // 不是「别人的私有节点」那种边界（作者就是自己），而是：他要转手一台
            // 别人连不到的节点 —— 接受方永远连不上，当场说清比事后猜好。
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "grant_required",
                "这个节点不是公开节点，别人连不上：让它的主人授权或设为公开，或 --node none 只分享包",
            ));
        }
        node_id = hit.id;
        node_kind = hit.kind;
        node_label = hit.name;
    }

    let card_id = new_id("AC");
    let blob_name = format!("{CARD_BLOB_PREFIX}{card_id}.hur");
    if state.blobs().put(&blob_name, &raw).is_err() {
        return Err(ApiError::internal("写入名片字节失败"));
    }
    let created = store::agentcards::create(
        state.pool(),
        store::agentcards::Input {
            id: card_id,
            owner_id: a.user_id.clone(),
            name: clamp_runes(&display_name, 60),
            note: clamp_runes(&note, 400),
            blob_name: blob_name.clone(),
            size: raw.len() as i64,
            sha256: card_sha256(&raw),
            manifest: String::from_utf8_lossy(&mb).to_string(),
            agent_id: mf.id_.clone(),
            agent_version: mf.version.clone(),
            agent_kind: mf.kind.clone(),
            agent_profile: mf_profile,
            node_ref: node_ref_trim.clone(),
            node_id,
            node_kind,
            node_label,
            pass_hash,
            pass_salt,
            max_uses: uses,
            expires_at,
        },
    )
    .await;
    let (card, token) = match created {
        Ok(v) => v,
        Err(e) => {
            // 名片没落库，字节也不该留着（否则就是谁也清理不掉的垃圾）
            let _ = state.blobs().delete(&blob_name);
            tracing::error!("创建名片失败: {e}");
            return Err(ApiError::internal("创建名片失败"));
        }
    };

    let link = card_link(state.cfg(), &token);
    let admin = ensure_admin(&state, &auth, &headers).await;
    let ip = web::client_ip(&headers);
    audit(
        &state,
        admin.as_ref(),
        "share.create",
        &card.id,
        &card.name,
        "创建 Agent 名片",
        json!({"agent": card.agent_id, "node": card.node_ref, "uses": uses, "expiresAt": card.expires_at}),
        &ip,
    )
    .await;

    let author = card_author(&state, &card.owner_id).await;
    Ok(helpers::ok_status(
        StatusCode::CREATED,
        json!({
            "card": card_json(state.cfg(), &card, &token, author),
            // token 与 key 都只在创建这一刻回显（库里只存哈希，之后谁也取不回来）。
            "url": link,
            "key": key,
            "howto": {
                "human": "把 url 发给对方：浏览器打开是名片页，上面写着怎么收下",
                "agent": format!("对方一条命令收下：ncc agent add '{link}'"),
                "note": "收下只代表**找得到**：私有制品 / 私有节点仍要显式 grant",
            },
        }),
    ))
}

/* ---------------- 读 ---------------- */

/// 取名片：token（32 hex，现算哈希）或 id（`AC-…`）都认。
async fn find_card(state: &AppState, r: &str) -> Result<store::agentcards::AgentCard, ApiError> {
    let r = r.trim();
    let found = if r.starts_with("AC-") {
        store::agentcards::by_id(state.pool(), r).await
    } else {
        store::agentcards::by_token(state.pool(), r).await
    };
    found
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("名片不存在"))
}

/// 访问口令校验。作者本人不需要口令。
fn card_key_ok(auth: &Auth, card: &store::agentcards::AgentCard, key: &str, cookie: &str) -> bool {
    if card.pass_hash.is_empty() {
        return true;
    }
    if let Some(a) = auth.info() {
        if a.user_id == card.owner_id {
            return true;
        }
    }
    if !key.is_empty() {
        return store::agentcards::hash_pass(&card.pass_salt, key) == card.pass_hash;
    }
    !cookie.is_empty() && cookie == card.pass_hash
}

fn card_cookie_name(id: &str) -> String {
    format!("ncc_card_{id}")
}

/// 从 Cookie 头里取某个 cookie 的值（解锁页写的那一份）。
fn cookie_value(headers: &HeaderMap, name: &str) -> String {
    let Some(raw) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) else {
        return String::new();
    };
    let want = format!("{name}=");
    for part in raw.split(';') {
        if let Some(v) = part.trim().strip_prefix(&want) {
            return v.to_string();
        }
    }
    String::new()
}

/// GET /api/agent-cards/{token} —— token 即秘密（匿名可读）。
async fn get_agent_card(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let card = find_card(&state, &token).await?;
    let key = web::query(&uri, "key").unwrap_or_default();
    let cookie = cookie_value(&headers, &card_cookie_name(&card.id));
    if !card_key_ok(&auth, &card, &key, &cookie) {
        return Err(ApiError::forbidden("需要访问 key"));
    }
    if !card.readable() {
        let st = card.state();
        return Err(ApiError::new(StatusCode::GONE, st, card_state_msg(st)));
    }
    let _ = store::agentcards::bump_views(state.pool(), &card.id).await;
    let author = card_author(&state, &card.owner_id).await;
    Ok(helpers::ok_json(
        json!({"card": card_json(state.cfg(), &card, &token, author)}),
    ))
}

/// GET /api/agent-cards —— **只有自己的**（含已撤销 / 过期 / 用尽）。
async fn list_agent_cards(
    State(state): State<AppState>,
    auth: Auth,
    uri: Uri,
) -> ApiResult<Response> {
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("未认证或凭据无效（先 ncc login 或带 API-KEY）"))?;
    let (limit, offset) = page_params(&uri);
    let rows = store::agentcards::list(state.pool(), &a.user_id, limit, offset)
        .await
        .map_err(ApiError::from_db)?;
    let author = card_author(&state, &a.user_id).await;
    // token 明文在库里不存在（只存哈希）→ 列表里没有可点的链接，只给 hint。
    let list: Vec<Value> = rows
        .iter()
        .map(|c| card_json(state.cfg(), c, "", author.clone()))
        .collect();
    let total = list.len();
    Ok(helpers::ok_json(json!({
        "cards": list, "total": total, "limit": limit, "offset": offset,
        "note": "本节点只存 token 的 sha256：链接只在作者当初发出去的那一份里，列表回不出来",
    })))
}

/// POST /api/agent-cards/{token}/accept —— 收下一次（扣名额）。
///
/// 名额是「投递次数」不是「安装次数」：客户端把它放在**下载之前**，
/// 扣不动就不再下载字节 —— 否则限次形同虚设。
async fn accept_agent_card(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
    Path(token): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    // Go 给这条路由挂了 requireAuth()
    if auth.info().is_none() {
        return Err(ApiError::unauthorized(
            "未认证或凭据无效（先 ncc login 或带 API-Key）",
        ));
    }
    let card = find_card(&state, &token).await?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let key = first_non_empty(&[
        web::query(&uri, "key").unwrap_or_default(),
        form_field(&body, &content_type, "key"),
    ]);
    let cookie = cookie_value(&headers, &card_cookie_name(&card.id));
    if !card_key_ok(&auth, &card, &key, &cookie) {
        return Err(ApiError::forbidden("需要访问 key"));
    }
    let st = card.state();
    if !st.is_empty() {
        return Err(ApiError::new(StatusCode::GONE, st, card_state_msg(st)));
    }
    let Some(uses) = store::agentcards::consume(state.pool(), &card.id)
        .await
        .map_err(ApiError::from_db)?
    else {
        // 并发下刚好被抢光：还是 exhausted，不是 500。
        return Err(ApiError::new(
            StatusCode::GONE,
            "exhausted",
            card_state_msg("exhausted"),
        ));
    };
    let remaining = if card.max_uses > 0 {
        card.max_uses - uses
    } else {
        -1
    };
    Ok(helpers::ok_json(json!({
        "uses": uses, "maxUses": card.max_uses, "remaining": remaining,
    })))
}

/// GET /api/agent-cards/{token}/blob —— `.hur` 字节。
async fn download_agent_card_blob(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let card = find_card(&state, &token).await?;
    let key = web::query(&uri, "key").unwrap_or_default();
    let cookie = cookie_value(&headers, &card_cookie_name(&card.id));
    if !card_key_ok(&auth, &card, &key, &cookie) {
        return Err(ApiError::forbidden("需要访问 key"));
    }
    if !card.readable() {
        let st = card.state();
        return Err(ApiError::new(StatusCode::GONE, st, card_state_msg(st)));
    }
    let mut data = state
        .blobs()
        .get(&card.blob_name)
        .map_err(|_| ApiError::not_found("名片里的包已不在本节点（可能已被清理）"))?;
    // 与 Go 的 LimitReader 一样兜一手：库里存的包本来就不该超过 8MB。
    if data.len() > MAX_CARD_SIZE {
        data.truncate(MAX_CARD_SIZE + 1);
    }
    let mut resp = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        data,
    )
        .into_response();
    let h = resp.headers_mut();
    // 指纹随字节一起给：接受方拿它比对（不一致就**不许**装）。
    if let Ok(v) = HeaderValue::from_str(&card.sha256) {
        h.insert("x-ncc-sha256", v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("{}@{}", card.agent_id, card.agent_version)) {
        h.insert("x-ncc-agent", v);
    }
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
    let filename = format!("{}-{}.hur", card.agent_id, card.agent_version);
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{ascii}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok(resp)
}

/// DELETE /api/agent-cards/{token} —— 撤销。
///
/// 撤销 = **标记 revoked + 删字节**：字节真的没了（不是「标记一下但还能下」），
/// 记录留着 —— 作者在 `ncc agent ls` 里要看得到自己发过什么、哪张已作废，
/// 对方点开也能得到一句「作者撤销了」而不是含糊的 404。
async fn delete_agent_card(
    State(state): State<AppState>,
    auth: Auth,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let card = find_card(&state, &token).await?;
    let a = auth
        .info()
        .ok_or_else(|| ApiError::unauthorized("未认证或凭据无效（先 ncc login 或带 API-KEY）"))?;
    if card.owner_id != a.user_id {
        return Err(ApiError::forbidden("只能撤销自己的名片"));
    }
    if !store::agentcards::revoke(state.pool(), &card.id, &a.user_id)
        .await
        .map_err(ApiError::from_db)?
    {
        return Err(ApiError::internal("撤销失败"));
    }
    let _ = state.blobs().delete(&card.blob_name);
    // Go 这条路由不判管理员（没有 admin 中间件），所以 adminOf(c) 永远是空的、
    // 审计实际不落库 —— 这里保留调用点让意图可见，但显式传 None 保持一致。
    audit(
        &state,
        None,
        "share.revoke",
        &card.id,
        &card.name,
        "撤销 Agent 名片",
        json!({"agent": card.agent_id}),
        "",
    )
    .await;
    Ok(helpers::ok_json(
        json!({"ok": true, "id": card.id, "revoked": true}),
    ))
}

/* ---------------- 节点解析（名片创建时用一次） ---------------- */

/// 名片要指向的节点：只取展示与可见性判定真正需要的几列。
#[derive(Debug, Clone, sqlx::FromRow)]
struct NodeHit {
    id: String,
    kind: String,
    name: String,
    visibility: String,
    namespace_id: String,
    owner_id: Option<String>,
}

const NODE_HIT_COLS: &str = "hosted_nodes.id, hosted_nodes.kind, hosted_nodes.name, \
     hosted_nodes.visibility, hosted_nodes.namespace_id, namespaces.owner_id AS owner_id";

/// 解析节点引用：`@命名空间/slug` 或 `ND-…` id。
async fn resolve_node(state: &AppState, _owner_id: &str, r: &str) -> Result<NodeHit, ApiError> {
    let r = r.trim();
    if let Some(body) = r.strip_prefix('@') {
        let Some((ns, slug)) = body.split_once('/') else {
            return Err(ApiError::not_found("节点不存在"));
        };
        if ns.is_empty() || slug.is_empty() {
            return Err(ApiError::not_found("节点不存在"));
        }
        // 与 Go 的 FindNodeByNsSlug 同形（owner_id 只为带出「我与它的连接」，这里用不上）
        let sql = format!(
            "SELECT {NODE_HIT_COLS} FROM hosted_nodes JOIN namespaces ON namespaces.id = hosted_nodes.namespace_id \
             WHERE namespaces.slug = ? AND hosted_nodes.slug = ? LIMIT 1"
        );
        return sqlx::query_as::<_, NodeHit>(&sql)
            .bind(ns.trim_start_matches('@'))
            .bind(slug)
            .fetch_optional(state.pool())
            .await
            .map_err(ApiError::from_db)?
            .ok_or_else(|| ApiError::not_found("节点不存在"));
    }
    let sql = format!(
        "SELECT {NODE_HIT_COLS} FROM hosted_nodes LEFT JOIN namespaces ON namespaces.id = hosted_nodes.namespace_id \
         WHERE hosted_nodes.id = ?"
    );
    sqlx::query_as::<_, NodeHit>(&sql)
        .bind(r)
        .fetch_optional(state.pool())
        .await
        .map_err(ApiError::from_db)?
        .ok_or_else(|| ApiError::not_found("节点不存在"))
}

/// 可见性：公开节点人人可见；私有节点要么归属自己，要么拿到 node 授权。
async fn can_see_node(state: &AppState, n: &NodeHit, user_id: &str) -> bool {
    if n.visibility == "public" {
        return true;
    }
    if user_id.is_empty() {
        return false;
    }
    if n.owner_id.as_deref() == Some(user_id) {
        return true;
    }
    if crate::httpapi::artifacts::can_manage(state, &n.namespace_id, user_id).await {
        return true;
    }
    store::grants::has(
        state.pool(),
        n.owner_id.as_deref().unwrap_or_default(),
        user_id,
        store::grants::KIND_NODE,
        &n.namespace_id,
    )
    .await
}

/* ---------------- 页面 ---------------- */

fn send_card_html(status: StatusCode, title: &str, body: &str) -> Response {
    let mut resp = (status, Html(card_page_shell(title, body))).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    // 点到点链接不进搜索引擎：这不是节点内容，只该被「拿到链接的人」看到。
    h.insert("x-robots-tag", HeaderValue::from_static("noindex, nofollow"));
    resp
}

fn note_html_card(note: &str) -> String {
    if note.trim().is_empty() {
        String::new()
    } else {
        format!(r#"<p class="note">{}</p>"#, esc(note))
    }
}

/// GET /a/{token} —— 这一条链接的落地页（noindex）。
///
/// 拿到链接的人（也许是没装 ncc 的同事）至少该看见：这是什么、谁给的、怎么收下。
/// 页面**不列任何人的其他名片** —— 它不是目录，只是一张名片的脸。
async fn render_card_page(
    State(state): State<AppState>,
    auth: Auth,
    headers: HeaderMap,
    uri: Uri,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    let Ok(card) = find_card(&state, &token).await else {
        return Ok((StatusCode::NOT_FOUND, "名片不存在").into_response());
    };
    let key = web::query(&uri, "key").unwrap_or_default();
    let cookie = cookie_value(&headers, &card_cookie_name(&card.id));
    if !card_key_ok(&auth, &card, &key, &cookie) {
        return Ok(render_card_unlock(&card, &token, ""));
    }
    if !card.readable() {
        let st = card.state();
        return Ok(send_card_html(
            StatusCode::GONE,
            "名片不可用",
            &esc(&card_state_msg(st)),
        ));
    }
    let _ = store::agentcards::bump_views(state.pool(), &card.id).await;

    let author = card_author(&state, &card.owner_id).await;
    let who = esc(author["displayName"].as_str().unwrap_or_default());
    let node_line = if card.node_ref.is_empty() {
        "（这张名片只有包，不附带节点）".to_string()
    } else {
        format!(
            "接受方会把该节点收进自己的连接表：<code>{}</code>",
            esc(&card.node_ref)
        )
    };
    let expire = card
        .expires_at_local_text()
        .unwrap_or_else(|| "长期有效".to_string());
    let uses = if card.max_uses > 0 {
        format!("限 {} 人（已收 {}）", card.max_uses, card.uses)
    } else {
        "不限次".to_string()
    };
    let warn = if card.state() == "exhausted" {
        r#"<p class="warn">这张名片的收下名额已用完 —— 已经收下过的人不受影响。</p>"#
    } else {
        ""
    };
    let link = card_link(state.cfg(), &token);
    let body = format!(
        r#"{warn}
<p class="by">{who} 通过本内网节点分享了一个 Agent 给你</p>
<h1>{name}</h1>
{note}
<table>
<tr><td>包</td><td><code>{agent}</code> · {profile}</td></tr>
<tr><td>指纹</td><td><code class="sha">{sha}</code></td></tr>
<tr><td>节点</td><td>{node_line}</td></tr>
<tr><td>有效期</td><td>{expire} · {uses}</td></tr>
</table>
<p class="how">收下它（装到本机 + 连上节点）：</p>
<pre>{cmd}</pre>
<p class="sub">这一步需要 ncc 客户端，并且要指向本节点（<code>{public_url}</code>）。</p>
<p class="sub">收下只是「找得到」；要取对方的私有制品仍需单独授权（grant）。</p>"#,
        warn = warn,
        who = who,
        name = esc(&card.name),
        note = note_html_card(&card.note),
        agent = esc(&format!("{}@{}", card.agent_id, card.agent_version)),
        profile = esc(&card.agent_profile),
        sha = esc(&card.sha256),
        node_line = node_line,
        expire = esc(&expire),
        uses = esc(&uses),
        cmd = esc(&format!("ncc agent add '{link}'")),
        public_url = esc(&state.cfg().public_url),
    );
    Ok(send_card_html(StatusCode::OK, "NCC Agent 名片", &body))
}

/// POST /a/{token} —— 提交访问 key 后写 7 天 Cookie 并跳回页面。
async fn unlock_card_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(token): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let Ok(card) = find_card(&state, &token).await else {
        return Ok((StatusCode::NOT_FOUND, "名片不存在").into_response());
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let key = form_field(&body, &content_type, "key");
    if !card.pass_hash.is_empty()
        && store::agentcards::hash_pass(&card.pass_salt, &key) == card.pass_hash
    {
        let cookie = format!(
            "{}={}; Path=/a/{}; Max-Age=604800; HttpOnly",
            card_cookie_name(&card.id),
            card.pass_hash,
            token
        );
        let mut resp = StatusCode::SEE_OTHER.into_response();
        let h = resp.headers_mut();
        if let Ok(v) = HeaderValue::from_str(&format!("/a/{token}")) {
            h.insert(header::LOCATION, v);
        }
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            h.insert(header::SET_COOKIE, v);
        }
        return Ok(resp);
    }
    Ok(render_card_unlock(&card, &token, "key 不正确，请重试"))
}

fn render_card_unlock(card: &store::agentcards::AgentCard, token: &str, err_msg: &str) -> Response {
    let err_html = if err_msg.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="err">{}</p>"#, esc(err_msg))
    };
    let body = format!(
        r#"<p class="by">这是一张点到点的 Agent 名片</p>
<h1>需要访问 key</h1>
<p class="sub">「{name}」需要作者给你的访问 key</p>
<form method="post" action="/a/{token}">
<input name="key" type="password" placeholder="输入访问 key" maxlength="16" autocomplete="off" autofocus>
<button type="submit">查看</button>
</form>{err_html}"#,
        name = esc(&card.name),
        token = esc(token),
        err_html = err_html,
    );
    send_card_html(StatusCode::OK, "需要访问 key", &body)
}

/// 极简卡片页外壳（自带样式，不引任何外部资源）。
fn card_page_shell(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="robots" content="noindex,nofollow">
<title>{title}</title>
<style>
*{{box-sizing:border-box}}body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:24px;
background:linear-gradient(135deg,#0f172a,#1e293b 60%,#155e75);font-family:system-ui,-apple-system,"PingFang SC",sans-serif;color:#e2e8f0}}
.card{{background:rgba(255,255,255,.06);border:1px solid rgba(255,255,255,.16);border-radius:16px;
padding:28px 26px;width:min(94vw,560px)}}
h1{{font-size:22px;margin:6px 0 14px}}.by{{color:#67e8f9;font-size:13.5px;margin:0}}
.sub,.note{{color:#94a3b8;font-size:13.5px}}
table{{width:100%;border-collapse:collapse;margin:14px 0}}
td{{padding:6px 0;font-size:13.5px;vertical-align:top;border-top:1px solid rgba(255,255,255,.09)}}
td:first-child{{color:#94a3b8;width:64px}}
code{{background:rgba(255,255,255,.08);padding:1px 5px;border-radius:5px;font-size:12.5px}}
code.sha{{word-break:break-all}}
pre{{background:rgba(0,0,0,.35);padding:12px;border-radius:10px;overflow-x:auto;font-size:13px}}
input{{width:100%;padding:11px 14px;border-radius:10px;border:1px solid rgba(255,255,255,.25);
background:rgba(255,255,255,.08);color:#fff;font-size:16px;letter-spacing:.2em;text-align:center}}
input:focus{{outline:none;border-color:#22d3ee}}
button{{margin-top:14px;width:100%;padding:11px;border:none;border-radius:10px;background:#0891b2;color:#fff;font-size:15px;font-weight:600;cursor:pointer}}
button:hover{{background:#0e7490}}
.err{{color:#fca5a5;font-size:13px;margin:12px 0 0}}
.warn{{color:#fcd34d;font-size:13.5px;margin:0 0 12px}}
.how{{margin:18px 0 6px;font-size:13.5px}}
</style></head><body><div class="card">{body}</div></body></html>"#,
        title = esc(title),
        body = body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use ncc_core::storage::LocalStorage;
    use tower::ServiceExt;

    fn test_dir(name: &str) -> std::path::PathBuf {
        // 落在工作区的 target/ 里（已 gitignore），别污染 crate 源码树
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-blobs")
            .join(format!("agentcards-{}-{name}", std::process::id()))
    }

    async fn state(name: &str) -> AppState {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        ncc_core::pool::migrate(&pool, crate::schema::DDL).await.unwrap();
        let mut cfg = crate::config::load().expect("默认配置可加载");
        cfg.public_url = "http://localhost:8282".to_string();
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
            // 与 router.rs 一致：上传端点要能吃大包（默认上限只有 2MB，
            // 那样 8MB 的包会在提取器那一层就被拦掉，看不到我们自己的报错）
            .layer(axum::extract::DefaultBodyLimit::max(256 << 20))
            .with_state(state.clone())
    }

    /// 建一个用户并签发一把 API-Key，返回 (用户 id, bearer)。
    async fn seed_user(state: &AppState, name: &str) -> (String, String) {
        let u = store::users::create(state.pool(), name, &format!("{name}@x.com"), "h")
            .await
            .unwrap();
        let (_k, secret) = store::apikeys::create(state.pool(), &u.id, "t", &[]).await.unwrap();
        (u.id, secret)
    }

    /* ---- 构造测试用 .hur 包 ---- */

    /// 最小 zip（全部按「存储」写，不带 CRC —— 我们的读取不做 CRC 校验）。
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
            out.extend_from_slice(&0u16.to_le_bytes()); // 时间
            out.extend_from_slice(&0u16.to_le_bytes()); // 日期
            out.extend_from_slice(&0u32.to_le_bytes()); // crc
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // 扩展区长度
            out.extend_from_slice(nb);
            out.extend_from_slice(data);

            cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes()); // 生成方版本
            cd.extend_from_slice(&20u16.to_le_bytes()); // 需要的版本
            cd.extend_from_slice(&0u16.to_le_bytes()); // flags
            cd.extend_from_slice(&0u16.to_le_bytes()); // 方式
            cd.extend_from_slice(&0u16.to_le_bytes()); // 时间
            cd.extend_from_slice(&0u16.to_le_bytes()); // 日期
            cd.extend_from_slice(&0u32.to_le_bytes()); // crc
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(nb.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u32.to_le_bytes());
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

    /// 单条目 zip，压缩方式与原大小可指定（测 method=8 分支用）。
    fn zip_one(name: &str, method: u16, data: &[u8], usize_: u32) -> Vec<u8> {
        let nb = name.as_bytes();
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(nb);
        out.extend_from_slice(data);
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&usize_.to_le_bytes());
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 6]); // 扩展区 / 注释 / 磁盘号
        out.extend_from_slice(&0u16.to_le_bytes()); // 内部属性
        out.extend_from_slice(&0u32.to_le_bytes()); // 外部属性
        out.extend_from_slice(&0u32.to_le_bytes()); // 本地头偏移
        out.extend_from_slice(nb);
        let cd_size = out.len() as u32 - cd_offset;
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// gzip 外壳（内层是「存储型」DEFLATE 块，bfinal=1；尾部 CRC/ISIZE 我们不校验）。
    fn gzip_stored(data: &[u8]) -> Vec<u8> {
        assert!(data.len() <= u16::MAX as usize);
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

    fn manifest_json(id: &str, version: &str) -> String {
        format!(
            r#"{{"spec":"hur/1","kind":"agent","profile":"default","id":"{id}","name":"名字 {id}","version":"{version}"}}"#
        )
    }

    /// 真包：gzip 包住的 zip。
    fn hur_bytes(mf: &str) -> Vec<u8> {
        gzip_stored(&zip_stored(&[("hur.json", mf.as_bytes())]))
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn post_card(bearer: &str, query: &str, body: Vec<u8>) -> Request<Body> {
        let mut b = Request::builder()
            .method("POST")
            .uri(format!("/agent-cards{query}"))
            .header("content-type", "application/octet-stream");
        if !bearer.is_empty() {
            b = b.header("authorization", format!("Bearer {bearer}"));
        }
        b.body(Body::from(body)).unwrap()
    }

    /// 建一张名片，返回 (token, 响应体)。
    ///
    /// 注意：名片**没有**单独的 `token` 字段（与 Go 同形）—— 明文只在 `url` 里，
    /// 所以测试从这里反推 token（这正是「库里只有哈希，明文只在链接里」的样子）。
    async fn create_card(app: &Router, bearer: &str, query: &str) -> (String, Value) {
        let (code, v) = call(
            app,
            post_card(bearer, query, hur_bytes(&manifest_json("demo", "1.0.0"))),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        let url = v["url"].as_str().unwrap().to_string();
        (url.rsplit('/').next().unwrap().to_string(), v)
    }

    /* ---- 解压内核 ---- */

    #[test]
    fn deflate_存储块与固定哈夫曼都能解() {
        // 存储型块：bfinal=1 btype=00 → 0x01 + LEN + NLEN + 数据
        let mut stored = vec![0x01];
        stored.extend_from_slice(&5u16.to_le_bytes());
        stored.extend_from_slice(&(!5u16).to_le_bytes());
        stored.extend_from_slice(b"hello");
        assert_eq!(inflate(&stored, 100).unwrap(), b"hello");

        // 固定哈夫曼块，内容 "OK"：
        //   bfinal=1 btype=01 → 位流 1,1,0
        //   'O'=79 → 8 位码 0x30+79=01111111；'K'=75 → 0x30+75=01111011；256 → 7 位 0000000
        // 按 LSB-first 打包后是 F3 F7 06 00（推导见本测试注释）。
        assert_eq!(inflate(&[0xf3, 0xf7, 0x06, 0x00], 100).unwrap(), b"OK");

        // 超上限要报错（不是静默截断）
        assert!(inflate(&stored, 3).is_err());
        assert!(inflate(&[0xff, 0xff], 10).is_err());
    }

    #[test]
    fn hur_清单读取_gzip与非gzip都认() {
        let mf = manifest_json("demo", "2.0.0");
        assert_eq!(
            hur_manifest_bytes(&hur_bytes(&mf)).unwrap(),
            mf.as_bytes()
        );
        // 裸 zip（老包）
        let plain = zip_stored(&[("hur.json", mf.as_bytes())]);
        assert_eq!(hur_manifest_bytes(&plain).unwrap(), mf.as_bytes());
        // 没有 hur.json
        let nope = zip_stored(&[("other.txt", b"x")]);
        assert_eq!(hur_manifest_bytes(&nope).unwrap_err(), "包里没有 hur.json");
        // 既不是 gzip 也不是 zip
        assert_eq!(
            hur_manifest_bytes(b"not a package").unwrap_err(),
            "既不是 gzip 也不是 zip —— 这不是一个 hur 包"
        );
        // gzip 层是坏的
        assert_eq!(
            hur_manifest_bytes(&[0x1f, 0x8b, 0x08, 0x00]).unwrap_err(),
            "gzip 层打不开（不是有效的 .hur）"
        );
        // method=8（deflate）的条目也要能读：这里塞一段固定哈夫曼的 "OK"
        let zip = zip_one("hur.json", 8, &[0xf3, 0xf7, 0x06, 0x00], 2);
        assert_eq!(hur_manifest_bytes(&zip).unwrap(), b"OK");
        // 条目声明的原大小超上限 → 拒（连解压都不做）
        let big = zip_one("hur.json", 8, &[0xf3, 0xf7, 0x06, 0x00], (MAX_CARD_MANIFEST + 1) as u32);
        assert_eq!(
            hur_manifest_bytes(&big).unwrap_err(),
            "包里的 hur.json 过大（>64KB）"
        );
    }

    #[test]
    fn 时长与次数解析_与_go_一致() {
        assert_eq!(parse_go_duration("168h").unwrap().num_hours(), 168);
        assert_eq!(parse_go_duration("1h30m").unwrap().num_minutes(), 90);
        assert_eq!(parse_go_duration("90s").unwrap().num_seconds(), 90);
        assert_eq!(parse_go_duration("0").unwrap().num_seconds(), 0);
        assert!(parse_go_duration("7d").is_none(), "Go 的 ParseDuration 不认 d");
        assert!(parse_go_duration("abc").is_none());
        assert!(parse_go_duration("").is_none());

        assert_eq!(parse_int64_default(""), 0);
        assert_eq!(parse_int64_default("3"), 3);
        assert_eq!(parse_int64_default("abc"), 0);
        assert_eq!(parse_int64_default("99999"), CARD_MAX_USES + 1);
        assert_eq!(clamp_runes("  一二三四五  ", 3), "一二三");
    }

    /* ---- 创建 ---- */

    #[tokio::test]
    async fn 创建名片_未登录401_正常回token与url() {
        let st = state("create").await;
        let (uid, secret) = seed_user(&st, "张三").await;
        let app = app(&st);

        let (code, v) = call(
            &app,
            post_card("", "", hur_bytes(&manifest_json("demo", "1.0.0"))),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["code"], "unauthorized");
        assert_eq!(v["error"]["message"], "未认证或凭据无效（先 ncc login 或带 API-Key）");

        let (code, v) = call(&app, post_card(&secret, "", Vec::new())).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "上传内容为空");

        let (token, v) = create_card(&app, &secret, "?uses=2&note=给小李").await;
        assert_eq!(token.len(), 32);
        assert_eq!(v["card"]["hint"], &token[..6]);
        assert_eq!(v["card"]["name"], "名字 demo"); // 清单里的 name
        assert_eq!(v["card"]["note"], "给小李");
        assert_eq!(v["card"]["agent"]["id"], "demo");
        assert_eq!(v["card"]["agent"]["version"], "1.0.0");
        assert_eq!(v["card"]["agent"]["kind"], "agent");
        assert_eq!(v["card"]["agent"]["profile"], "default");
        assert_eq!(v["card"]["agent"]["bytes"], hur_bytes(&manifest_json("demo", "1.0.0")).len() as i64);
        assert_eq!(v["card"]["agent"]["blob"], format!("/api/agent-cards/{token}/blob"));
        assert_eq!(v["card"]["uses"], 0);
        assert_eq!(v["card"]["maxUses"], 2);
        assert_eq!(v["card"]["hasPassword"], false);
        assert_eq!(v["card"]["revoked"], false);
        assert_eq!(v["card"]["state"], "");
        assert_eq!(v["card"]["url"], format!("http://localhost:8282/a/{token}"));
        assert_eq!(v["card"]["author"]["displayName"], "张三");
        assert_eq!(v["card"]["manifest"]["id"], "demo");
        assert_eq!(v["card"]["node"], Value::Null);
        // 缺省 7 天有效期；形如 RFC3339 UTC
        let exp = v["card"]["expiresAt"].as_str().unwrap();
        assert!(exp.ends_with('Z') && exp.len() == 20, "{exp}");
        assert_eq!(v["url"], format!("http://localhost:8282/a/{token}"));
        assert_eq!(v["key"], "");
        assert!(v["howto"]["agent"].as_str().unwrap().contains("ncc agent add"));

        // 库里只有哈希 + hint
        let row: (String, String, String) =
            sqlx::query_as("SELECT token_hash, token_hint, owner_id FROM agent_cards")
                .fetch_one(st.pool())
                .await
                .unwrap();
        assert_eq!(row.0, store::hash_secret(&token));
        assert_eq!(row.1, token[..6]);
        assert_eq!(row.2, uid);
        assert_ne!(row.0, token);
        // 字节进了本地存储（与 Go 一样走同一份 Blob）
        let blob: String = sqlx::query_scalar("SELECT blob_name FROM agent_cards")
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(st.blobs().get(&blob).unwrap(), hur_bytes(&manifest_json("demo", "1.0.0")));
    }

    #[tokio::test]
    async fn 创建名片_参数与包体各种拒绝() {
        let st = state("reject").await;
        let (_uid, secret) = seed_user(&st, "李四").await;
        let app = app(&st);
        let bad = |body: Vec<u8>, q: &str| post_card(&secret, q, body);

        // 不是包
        let (code, v) = call(&app, bad(b"hello".to_vec(), "")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "这不是一个 hur 包：既不是 gzip 也不是 zip —— 这不是一个 hur 包"
        );
        // 包里没有 hur.json
        let (code, v) = call(&app, bad(zip_stored(&[("a.txt", b"x")]), "")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "这不是一个 hur 包：包里没有 hur.json");
        // hur.json 不是 JSON
        let (code, v) = call(&app, bad(hur_bytes("{不是 JSON"), "")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "包里的 hur.json 不是合法 JSON");
        // 缺 spec / id / version
        let (code, v) = call(&app, bad(hur_bytes(r#"{"id":"demo","version":"1"}"#), "")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "hur.json 缺 spec / id / version —— 不是一个完整的包"
        );
        // 次数越界
        let good = || hur_bytes(&manifest_json("demo", "1.0.0"));
        let (code, v) = call(&app, bad(good(), "?uses=99999")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "收下次数需在 0-10000 之间（0 = 不限）"
        );
        // 口令太短
        let (code, v) = call(&app, bad(good(), "?key=ab")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "访问 key 长度需 3-16 位");
        // expires 不合法（Go duration 与 RFC3339 都不是）
        let (code, v) = call(&app, bad(good(), "?expires=7d")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(
            v["error"]["message"],
            "expires 用 Go duration（168h）或 none；绝对时间用 expiresAt（RFC3339）"
        );
        // 超过 365 天（注意：8760h 恰好等于 365 天，Go 在比较时又取了一次
        // time.Now()，所以「刚好 365 天」是放行的 —— 这里用 9000h 越过线）
        let (code, v) = call(&app, bad(good(), "?expires=9000h")).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"]["message"], "有效期最长 365 天");
        // expires=none → 不过期
        let (_t, v) = create_card(&app, &secret, "?expires=none").await;
        assert_eq!(v["card"]["expiresAt"], Value::Null);
        // 不存在的节点
        let (code, v) = call(&app, bad(good(), "?node=ND-nope")).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["code"], "not_found");
        assert_eq!(
            v["error"]["message"],
            "找不到这个节点（用 ND-… 或 @命名空间/节点slug）"
        );
        // 超 8MB
        let big = vec![0u8; MAX_CARD_SIZE + 1];
        let (code, v) = call(&app, bad(big, "")).await;
        assert_eq!(code, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(v["error"]["code"], "payload_too_large");
        assert_eq!(v["error"]["message"], "包超过 8MB");
    }

    #[tokio::test]
    async fn 创建名片_multipart_与_口令() {
        let st = state("multipart").await;
        let (_uid, secret) = seed_user(&st, "王五").await;
        let app = app(&st);
        let boundary = "----nccboundary";
        let mf = manifest_json("multi", "3.1.0");
        let mut body: Vec<u8> = Vec::new();
        for (k, v) in [("name", "手工名字"), ("note", "备注"), ("key", "abc123"), ("uses", "3")] {
            body.extend_from_slice(
                format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n")
                    .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.hur\"\r\nContent-Type: application/octet-stream\r\n\r\n")
                .as_bytes(),
        );
        body.extend_from_slice(&hur_bytes(&mf));
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let req = Request::builder()
            .method("POST")
            .uri("/agent-cards")
            .header("authorization", format!("Bearer {secret}"))
            .header("content-type", format!("multipart/form-data; boundary={boundary}"))
            .body(Body::from(body))
            .unwrap();
        let (code, v) = call(&app, req).await;
        assert_eq!(code, StatusCode::CREATED, "{v}");
        assert_eq!(v["card"]["name"], "手工名字");
        assert_eq!(v["card"]["note"], "备注");
        assert_eq!(v["card"]["maxUses"], 3);
        assert_eq!(v["card"]["hasPassword"], true);
        assert_eq!(v["key"], "abc123", "口令只在创建这一刻回显");
        let token = v["url"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
        // 库里只有盐化哈希
        let (ph, ps): (String, String) =
            sqlx::query_as("SELECT pass_hash, pass_salt FROM agent_cards")
                .fetch_one(st.pool())
                .await
                .unwrap();
        assert!(!ph.is_empty() && !ps.is_empty());
        assert_ne!(ph, "abc123");
        assert_eq!(ph, store::agentcards::hash_pass(&ps, "abc123"));

        // 没带 key → 403；带错 key → 403；带对 key → 200
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/agent-cards/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "需要访问 key");
        let (code, _v) = call(
            &app,
            Request::builder()
                .uri(format!("/agent-cards/{token}?key=wrong"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/agent-cards/{token}?key=abc123"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["card"]["hasPassword"], true);
        // 作者本人不需要口令
        let (code, _v) = call(
            &app,
            Request::builder()
                .uri(format!("/agent-cards/{token}"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
    }

    /* ---- 读 / 收下 / 取字节 ---- */

    #[tokio::test]
    async fn 取名片与字节_撤销与过期后状态分开() {
        let st = state("read").await;
        let (uid, secret) = seed_user(&st, "赵六").await;
        let app = app(&st);
        let (token, _v) = create_card(&app, &secret, "").await;

        // 取名片：名字与字节都在
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/agent-cards/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["card"]["name"], "名字 demo");
        assert_eq!(v["card"]["url"], format!("http://localhost:8282/a/{token}"));

        // 取字节：指纹与包名都在头里
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/agent-cards/{token}/blob"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("x-ncc-agent").unwrap(),
            "demo@1.0.0"
        );
        assert_eq!(
            resp.headers().get("content-disposition").unwrap(),
            "attachment; filename=\"demo-1.0.0.hur\""
        );
        assert_eq!(resp.headers().get("x-robots-tag").unwrap(), "noindex, nofollow");
        let expected_sha = ncc_core::crypto::sha256_hex(&hur_bytes(&manifest_json("demo", "1.0.0")));
        assert_eq!(resp.headers().get("x-ncc-sha256").unwrap(), expected_sha.as_str());
        assert_eq!(
            axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap().to_vec(),
            hur_bytes(&manifest_json("demo", "1.0.0"))
        );
        // 视图计数 +1
        let views: i64 = sqlx::query_scalar("SELECT views FROM agent_cards WHERE owner_id = ?")
            .bind(&uid)
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(views, 1);

        // 撤销：字节真的没了、状态是 revoked、页面也说撤销
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/agent-cards/{token}"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["revoked"], true);
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/agent-cards/{token}/blob"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(v["error"]["code"], "revoked");
        assert_eq!(v["error"]["message"], "这张名片已被作者撤销");
        assert!(st.blobs().get("agent-cards/x.hur").is_err());

        // 过期：另一种状态
        let (token2, _v2) = create_card(&app, &secret, "?expires=168h").await;
        sqlx::query("UPDATE agent_cards SET expires_at = ? WHERE token_hint = ?")
            .bind(ncc_core::timeutil::format_go(
                chrono::Local::now().fixed_offset() - chrono::Duration::hours(1),
            ))
            .bind(&token2[..6])
            .execute(st.pool())
            .await
            .unwrap();
        let (code, v) = call(
            &app,
            Request::builder()
                .uri(format!("/agent-cards/{token2}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(v["error"]["code"], "expired");
        assert_eq!(v["error"]["message"], "这张名片已过有效期，请让作者重新生成一张");
    }

    #[tokio::test]
    async fn 收下_扣名额_用尽410_但取字节不受影响() {
        let st = state("accept").await;
        let (_uid, secret) = seed_user(&st, "孙七").await;
        let app = app(&st);
        let (token, _v) = create_card(&app, &secret, "?uses=1").await;

        // 未登录 → 401（Go 给这条路由挂了 requireAuth）
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri(format!("/agent-cards/{token}/accept"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["message"], "未认证或凭据无效（先 ncc login 或带 API-Key）");

        // 第一次：扣到 1，剩余 0
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri(format!("/agent-cards/{token}/accept"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["uses"], 1);
        assert_eq!(v["maxUses"], 1);
        assert_eq!(v["remaining"], 0);

        // 第二次：名额没了
        let (code, v) = call(
            &app,
            Request::builder()
                .method("POST")
                .uri(format!("/agent-cards/{token}/accept"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::GONE);
        assert_eq!(v["error"]["code"], "exhausted");
        assert_eq!(
            v["error"]["message"],
            "这张名片的收下次数已用完，请让作者重新生成一张"
        );

        // ⚠️ 用尽只拦 accept：读名片与取字节还得通（否则已拿到名额的人被关在门外）
        let (code, _v) = call(
            &app,
            Request::builder()
                .uri(format!("/agent-cards/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/agent-cards/{token}/blob"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 不限次（uses 缺省 0）：accept 永远成功
        let (token2, _v2) = create_card(&app, &secret, "").await;
        for _ in 0..3 {
            let (code, v) = call(
                &app,
                Request::builder()
                    .method("POST")
                    .uri(format!("/agent-cards/{token2}/accept"))
                    .header("authorization", format!("Bearer {secret}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(code, StatusCode::OK);
            assert_eq!(v["remaining"], -1);
        }
    }

    #[tokio::test]
    async fn 列表_只列自己的_且不回链接() {
        let st = state("list").await;
        let (_u1, s1) = seed_user(&st, "甲").await;
        let (_u2, s2) = seed_user(&st, "乙").await;
        let app = app(&st);
        create_card(&app, &s1, "").await;
        create_card(&app, &s1, "?expires=none").await;
        create_card(&app, &s2, "").await;

        let (code, v) = call(
            &app,
            Request::builder().uri("/agent-cards").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["message"], "未认证或凭据无效（先 ncc login 或带 API-KEY）");

        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/agent-cards?limit=1")
                .header("authorization", format!("Bearer {s1}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 1);
        assert_eq!(v["limit"], 1);
        assert_eq!(v["cards"][0]["author"]["displayName"], "甲");
        // 列表回不出明文 token → 没有可点链接
        assert_eq!(v["cards"][0]["url"], "");
        assert_eq!(v["cards"][0]["agent"]["blob"], "");
        assert_eq!(v["cards"][0]["hint"].as_str().unwrap().len(), 6);
        assert!(v["note"].as_str().unwrap().contains("sha256"));

        let (code, v) = call(
            &app,
            Request::builder()
                .uri("/agent-cards")
                .header("authorization", format!("Bearer {s1}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["total"], 2, "只有自己的两张");
    }

    #[tokio::test]
    async fn 撤销_越权与非本人() {
        let st = state("delete").await;
        let (_u1, s1) = seed_user(&st, "甲").await;
        let (_u2, s2) = seed_user(&st, "乙").await;
        let app = app(&st);
        let (token, _v) = create_card(&app, &s1, "").await;

        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri("/agent-cards/AC-nope")
                .header("authorization", format!("Bearer {s1}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"]["message"], "名片不存在");

        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/agent-cards/{token}"))
                .header("authorization", format!("Bearer {s2}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["message"], "只能撤销自己的名片");

        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/agent-cards/{token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"]["message"], "未认证或凭据无效（先 ncc login 或带 API-KEY）");

        // 按 id（AC-…）也能撤
        let id: String = sqlx::query_scalar("SELECT id FROM agent_cards")
            .fetch_one(st.pool())
            .await
            .unwrap();
        let (code, v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/agent-cards/{id}"))
                .header("authorization", format!("Bearer {s1}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["id"], id);
        assert_eq!(v["revoked"], true);
        // 记录还在（作者要看得到自己发过什么）
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_cards")
            .fetch_one(st.pool())
            .await
            .unwrap();
        assert_eq!(n, 1);
        // 撤销是幂等的：再来一次还是打在那个 id 上
        let (code, _v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/agent-cards/{id}"))
                .header("authorization", format!("Bearer {s1}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
    }

    /* ---- 顶层公开页 ---- */

    #[tokio::test]
    async fn 落地页_解锁_与_失效页() {
        let st = state("page").await;
        let (_uid, secret) = seed_user(&st, "周八").await;
        let app = app(&st);
        let (token, _v) = create_card(&app, &secret, "").await;

        // 人可读的名片页
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/a/{token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("x-robots-tag").unwrap(),
            "noindex, nofollow"
        );
        let html = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains("名字 demo"), "{html}");
        assert!(html.contains("周八 通过本内网节点分享了一个 Agent 给你"));
        assert!(html.contains(&format!("ncc agent add &#39;http://localhost:8282/a/{token}&#39;")));
        assert!(html.contains("（这张名片只有包，不附带节点）"));

        // 不存在
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/a/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], "名片不存在".as_bytes());

        // 设了口令 → 解锁页
        let (ptoken, _pv) = create_card(&app, &secret, "?key=abc123").await;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/a/{ptoken}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let html = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains("需要访问 key"), "{html}");
        assert!(html.contains(&format!("action=\"/a/{ptoken}\"")));

        // 口令不对 → 还是解锁页 + 提示
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/a/{ptoken}"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("key=wrong"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains("key 不正确，请重试"));

        // 口令对 → 303 + Cookie；带 Cookie 再访问就直接看到名片
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/a/{ptoken}"))
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("key=abc123"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        assert_eq!(resp.headers().get("location").unwrap(), &format!("/a/{ptoken}"));
        let cookie = resp
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.contains(&format!("ncc_card_")));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains(&format!("Path=/a/{ptoken}")));
        let (name, value) = cookie.split(';').next().unwrap().split_once('=').unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/a/{ptoken}"))
                    .header("cookie", format!("{name}={value}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let html = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains("名字 demo"));

        // 撤销后：页面上说「作者撤销了」（410），不是含糊的 404
        let (code, _v) = call(
            &app,
            Request::builder()
                .method("DELETE")
                .uri(format!("/agent-cards/{token}"))
                .header("authorization", format!("Bearer {secret}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/a/{token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::GONE);
        let html = String::from_utf8(
            axum::body::to_bytes(resp.into_body(), 1 << 22).await.unwrap().to_vec(),
        )
        .unwrap();
        assert!(html.contains("这张名片已被作者撤销"));
    }

    /* ---- 节点参数 ---- */

    #[tokio::test]
    async fn 节点参数_公开可带_私有要授权() {
        let st = state("node").await;
        let (uid, secret) = seed_user(&st, "钱九").await;
        let ns = store::namespaces::create_account(st.pool(), &uid, "钱九", "qianjiu")
            .await
            .unwrap();
        // 两台节点：一公开一私有
        let mut req = store::nodes::HeartbeatReq {
            slug: "pub".to_string(),
            name: "公开节点".to_string(),
            kind: "service".to_string(),
            visibility: "public".to_string(),
            ..Default::default()
        };
        let (pub_node, _) = store::nodes::upsert(st.pool(), &ns.id, &req).await.unwrap();
        req.slug = "priv".to_string();
        req.name = "私有节点".to_string();
        req.visibility = "private".to_string();
        let (priv_node, _) = store::nodes::upsert(st.pool(), &ns.id, &req).await.unwrap();
        let app = app(&st);

        // 公开节点：ND-… id 与 @命名空间/slug 都认
        let (_t, v) = create_card(&app, &secret, &format!("?node={}", pub_node.id)).await;
        assert_eq!(v["card"]["node"]["id"], pub_node.id);
        assert_eq!(v["card"]["node"]["kind"], "service");
        assert_eq!(v["card"]["node"]["label"], "公开节点");
        let (_t2, v2) = create_card(&app, &secret, "?node=@qianjiu/pub").await;
        assert_eq!(v2["card"]["node"]["id"], pub_node.id);

        // 私有节点：主人自己（= 作者）可以看到 → 允许（Go 的 canSeeNode 里 owner 直接通过）
        let (_t3, v3) = create_card(&app, &secret, &format!("?node={}", priv_node.id)).await;
        assert_eq!(v3["card"]["node"]["id"], priv_node.id);

        // 别人（非 owner / 无授权）想分享这台私有节点 → 403 grant_required
        let (_u2, s2) = seed_user(&st, "别人").await;
        let (code, v) = call(
            &app,
            post_card(
                &s2,
                &format!("?node={}", priv_node.id),
                hur_bytes(&manifest_json("demo", "1.0.0")),
            ),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(v["error"]["code"], "grant_required");
        assert!(v["error"]["message"].as_str().unwrap().contains("不是公开节点"));
        // node=none：Go 不解析节点，但**照样把 "none" 存进 node_ref** —— 于是视图里
        // 会出现一个 ref="none" 的 node 块（这是 Go 的行为，照搬不改）。
        let (_t4, v4) = create_card(&app, &secret, "?node=none").await;
        assert_eq!(v4["card"]["node"]["ref"], "none");
        assert_eq!(v4["card"]["node"]["id"], "");
        // 完全不传 node → node 块是 null（「只有包」的那种名片）
        let (_t5, v5) = create_card(&app, &secret, "").await;
        assert_eq!(v5["card"]["node"], Value::Null);
    }
}
