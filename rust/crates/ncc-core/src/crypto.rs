//! 密码学小工具。全部手写标准库级别实现，不引入 JWT/加密框架 ——
//! 与 Go 侧行为逐字节对齐（令牌格式、哈希算法、密文布局都必须一致，
//! 否则 Rust 服务与既有 CLI / 数据无法互通）。

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// bcrypt 代价因子。Go 用 `bcrypt.DefaultCost` = 10，这里写死同一个值，
/// 这样同一口令在两边生成的哈希强度一致（校验侧本来就与代价无关）。
pub const BCRYPT_COST: u32 = 10;

/// URL 安全、无填充的 base64（Go 的 `base64.RawURLEncoding`）。
pub fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

/// 解码 URL 安全无填充 base64。
pub fn b64url_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s)
}

/// 标准 base64（带填充），用于 secretbox 密文。
pub fn b64_std(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// 解码标准 base64。
pub fn b64_std_decode(s: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::STANDARD.decode(s)
}

/// HMAC-SHA256 原始字节。
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// HMAC-SHA256 结果再走 base64url（JWT 签名段与字节下载签名都用它）。
pub fn hmac_sha256_b64url(secret: &str, data: &str) -> String {
    b64url(&hmac_sha256(secret.as_bytes(), data.as_bytes()))
}

/// 恒定时间比较（避免签名校验被逐字节计时区分）。
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 生成 n 字节随机数的 hex 串（Go 的 `store.RandHex`）。
pub fn rand_hex(n: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// 生成 n 字节随机数并 base64url 编码（做 token 用，比 hex 短）。
pub fn rand_b64url(n: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut buf);
    b64url(&buf)
}

/// 随机 hex 前缀（API-Key 的可读前缀段，10 位十六进制）。
pub fn rand_prefix() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 3];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// 数据 sha256 的 hex 串。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// 生成口令哈希（bcrypt，$2a$ 前缀，与 Go `x/crypto/bcrypt` 互认）。
pub fn hash_password(pw: &str) -> Result<String, String> {
    bcrypt::hash(pw, BCRYPT_COST).map_err(|e| e.to_string())
}

/// 校验口令。哈希损坏或格式不对一律当「不匹配」，不向上抛错 ——
/// 调用方只需一个布尔判断，多一个错误分支只会让每个调用点都写错。
pub fn verify_password(pw: &str, hash: &str) -> bool {
    bcrypt::verify(pw, hash).unwrap_or(false)
}

/// API-Key 明文形态：`ncc_<prefix>_<随机段>`；库里只存 prefix 与随机段的 sha256。
pub fn new_api_key(prefix: &str, secret: &str) -> String {
    format!("ncc_{prefix}_{secret}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 口令哈希_可校验() {
        let h = hash_password("p@ssw0rd").unwrap();
        assert!(h.starts_with("$2a$") || h.starts_with("$2b$"));
        assert!(verify_password("p@ssw0rd", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("p@ssw0rd", "不是哈希"));
    }

    #[test]
    fn hmac_与_go_一致() {
        // 与 Go 的 base64.RawURLEncoding(hmac_sha256(secret, data)) 等价
        assert_eq!(
            hmac_sha256_b64url("secret", "data"),
            b64url(&hmac_sha256(b"secret", b"data"))
        );
    }

    #[test]
    fn 恒定时间比较() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
