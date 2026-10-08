//! ID / slug 生成。与 Go 侧 `store.NewID`、`Slugify`、`ValidSlug` 行为一致，
//! 保证同一个库里两类服务生成的 ID 长得一样（都是 `P-<ts36>-<rand8>`）。
//!
//! 时间戳用 base36 是因为它比十进制短、又比随机串可排序 —— 排查问题时
//! 一眼能看出条目是什么时候建的。

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const B36: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// 生成带前缀的短 ID，如 `A-mtq2pw9q-9b70b828`。
pub fn new_id(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    format!(
        "{}-{}-{}",
        prefix,
        base36(millis),
        crate::crypto::rand_hex(4)
    )
}

/// 十进制转 base36。
pub fn base36(mut n: i64) -> String {
    if n <= 0 {
        return "0".to_string();
    }
    let mut out: Vec<u8> = Vec::new();
    while n > 0 {
        out.push(B36[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8_lossy(&out).to_string()
}

/// slug 化：小写、非 `[a-z0-9]` 折成 `-`、去首尾 `-`、最长 48、空则 `x`。
///
/// 中文等非 ASCII 字符会被丢掉 —— 这是刻意的：slug 要进 URL 与包名，
/// 调用方对全中文输入要靠别的方式兜底（平台侧用的是用户 id / handle）。
pub fn slugify(s: &str) -> String {
    let lowered = s.trim().to_lowercase();
    let mut out = String::new();
    let mut prev_dash = false;
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    let out = if out.len() > 48 {
        out[..48].to_string()
    } else {
        out
    };
    if out.is_empty() {
        "x".to_string()
    } else {
        out
    }
}

fn slug_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[a-z0-9][a-z0-9._-]{1,63}$").expect("静态正则合法"))
}

/// 校验规范 slug（小写字母数字与 `- _ .`，2..64 位，首字符为字母数字）。
pub fn valid_slug(s: &str) -> bool {
    slug_re().is_match(s)
}

/// 生成一个不透明 token（用于分享、接入票据、agent card 等）。
pub fn new_token(bytes: usize) -> String {
    crate::crypto::rand_b64url(bytes)
}

/// 生成 token 的存储指纹：本仓规矩是 token **只存 sha256**，
/// 库里泄漏了也换不回可用凭据（列表里因此也回不出可点链接）。
pub fn token_fingerprint(token: &str) -> String {
    crate::crypto::sha256_hex(token.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_形状() {
        let id = new_id("A");
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], "A");
        assert_eq!(parts[2].len(), 8);
    }

    #[test]
    fn slug_规则() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("   "), "x");
        assert_eq!(slugify("中文"), "x");
        assert!(valid_slug("my-agent.v1"));
        assert!(!valid_slug("A"));
        assert!(!valid_slug("-bad"));
    }
}
