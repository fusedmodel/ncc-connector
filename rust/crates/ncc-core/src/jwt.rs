//! 手写 HS256 JWT。刻意不引第三方 JWT 库：Go 侧就是手写的，
//! 两边的签名段（base64url(hmac-sha256)）与头（`{"alg":"HS256","typ":"JWT"}`）
//! 必须逐字节一致，引入框架反而要花力气去对齐它的默认行为。

use serde_json::Value;

/// 固定头（Go 侧字面量，无空格）。
const HEADER: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

/// 用给定的载荷 JSON 签名。载荷里应已含 `iat` / `exp`。
pub fn sign_hs256(secret: &str, payload_json: &str) -> String {
    let header = crate::crypto::b64url(HEADER.as_bytes());
    let payload = crate::crypto::b64url(payload_json.as_bytes());
    let signing_input = format!("{header}.{payload}");
    let sig = crate::crypto::hmac_sha256_b64url(secret, &signing_input);
    format!("{signing_input}.{sig}")
}

/// 校验签名并解出载荷。签名不对、段数不对、载荷不是 JSON 都返回 Err。
pub fn verify_hs256(secret: &str, token: &str) -> Result<Value, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("jwt 段数错误".to_string());
    }
    let expect = crate::crypto::hmac_sha256_b64url(secret, &format!("{}.{}", parts[0], parts[1]));
    if !crate::crypto::constant_time_eq(parts[2].as_bytes(), expect.as_bytes()) {
        return Err("jwt 签名无效".to_string());
    }
    let raw = crate::crypto::b64url_decode(parts[1]).map_err(|e| e.to_string())?;
    serde_json::from_slice::<Value>(&raw).map_err(|e| e.to_string())
}

/// 取声明的过期时间（`exp`，Unix 秒）。缺失或类型不对返回 None。
pub fn exp_of(claims: &Value) -> Option<i64> {
    claims.get("exp").and_then(|v| v.as_i64())
}

/// 令牌是否已过期。
///
/// 边界用 `<=`：`exp` 时刻本身就算过期。Go 版是 `exp < now`，两者只差一秒 ——
/// 这一秒的差别对「令牌能不能用」没有实际影响，但 `<=` 更贴近 exp 的语义。
pub fn expired(claims: &Value) -> bool {
    match exp_of(claims) {
        Some(exp) => exp <= crate::timeutil::now_unix(),
        None => true, // 没有 exp 的令牌一律当过期，避免「永久有效」的意外
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 签名与校验往返() {
        let payload = r#"{"sub":"U-1","kind":"user","iat":1,"exp":4102444800}"#;
        let t = sign_hs256("sec", payload);
        assert_eq!(t.split('.').count(), 3);
        let claims = verify_hs256("sec", &t).unwrap();
        assert_eq!(claims["sub"], "U-1");
        assert!(!expired(&claims));
    }

    #[test]
    fn 换密钥或改载荷都验不过() {
        let payload = r#"{"sub":"U-1","kind":"user","exp":4102444800}"#;
        let t = sign_hs256("sec", payload);
        assert!(verify_hs256("other", &t).is_err());
        // 改载荷（重放攻击的最朴素形态）
        let tampered = format!(
            "{}.{}",
            t.split('.').next().unwrap(),
            crate::crypto::b64url(b"{}")
        );
        assert!(verify_hs256("sec", &tampered).is_err());
    }

    #[test]
    fn 无_exp_视为过期() {
        assert!(expired(&serde_json::json!({"sub": "x"})));
    }
}
