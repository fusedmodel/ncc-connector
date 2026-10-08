//! 配置内容的静态加密（AES-256-GCM）。
//!
//! 与 Go 侧 `internal/secretbox` 的密文布局**完全一致**，这是互通的前提：
//!
//! ```text
//! 密钥 = HMAC-SHA256(节点密钥, "ncc-registry/config-content-v1")   → 32 字节
//! 密文 = "enc:v1:" + base64(nonce ‖ ciphertext ‖ tag)              （每条随机 nonce）
//! ```
//!
//! 为什么需要它：基础设施配置里经常夹着凭据（Wi-Fi PSK、模型 API key、DB 口令）。
//! 「靠用户记得别写进去」不是工程手段，所以 `secret=true` 的配置在**落库前**就加密。

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

/// 密文标记。改算法时递增版本号（v2…），并让 open 同时认旧前缀。
pub const PREFIX: &str = "enc:v1:";

const NONCE_LEN: usize = 12;

/// 加解密器（拿节点密钥派生一次，进程内复用）。
pub struct SecretBox {
    cipher: Aes256Gcm,
}

impl SecretBox {
    /// 用节点密钥构造。密钥为空直接报错 —— 静默用空密钥加密会把「配置没配好」
    /// 变成「数据以后打不开」。
    pub fn new(secret: &str) -> Result<Self, String> {
        if secret.trim().is_empty() {
            return Err("secretbox: 节点密钥为空".to_string());
        }
        let key = crate::crypto::hmac_sha256(secret.as_bytes(), b"ncc-registry/config-content-v1");
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(|e| format!("secretbox: 初始化失败: {e}"))?;
        Ok(Self { cipher })
    }

    /// 加密明文，返回带前缀的密文。
    pub fn seal(&self, plain: &str) -> Result<String, String> {
        use aes_gcm::aead::OsRng;
        use aes_gcm::aead::rand_core::RngCore;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = self
            .cipher
            .encrypt(nonce, plain.as_bytes())
            .map_err(|e| format!("secretbox: 加密失败: {e}"))?;
        let mut buf = Vec::with_capacity(NONCE_LEN + ct.len());
        buf.extend_from_slice(&nonce_bytes);
        buf.extend_from_slice(&ct);
        Ok(format!("{PREFIX}{}", crate::crypto::b64_std(&buf)))
    }

    /// 解密；**不带前缀视为明文原样返回**（历史明文行、或某条配置把 secret 关掉后
    /// 仍然读得出来，不会因为一次切换就把数据读挂）。
    pub fn open(&self, stored: &str) -> Result<String, String> {
        let Some(rest) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_string());
        };
        let raw = crate::crypto::b64_std_decode(rest)
            .map_err(|e| format!("secretbox: 密文解码失败: {e}"))?;
        if raw.len() < NONCE_LEN + 16 {
            return Err("secretbox: 密文长度异常".to_string());
        }
        let (nonce_bytes, ct) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        let plain = self
            .cipher
            .decrypt(nonce, ct)
            .map_err(|_| "secretbox: 解密失败（密钥不匹配或密文被改）".to_string())?;
        String::from_utf8(plain).map_err(|e| format!("secretbox: 明文不是 UTF-8: {e}"))
    }
}

/// 内容是否已是密文。
pub fn sealed(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 加解密往返() {
        let b = SecretBox::new("node-secret").unwrap();
        let ct = b.seal("wifi-psk-123").unwrap();
        assert!(sealed(&ct));
        assert_eq!(b.open(&ct).unwrap(), "wifi-psk-123");
    }

    #[test]
    fn 明文原样返回() {
        let b = SecretBox::new("node-secret").unwrap();
        assert_eq!(b.open("plain-value").unwrap(), "plain-value");
    }

    #[test]
    fn 换密钥打不开() {
        let a = SecretBox::new("key-a").unwrap();
        let b = SecretBox::new("key-b").unwrap();
        let ct = a.seal("x").unwrap();
        assert!(b.open(&ct).is_err());
    }
}
