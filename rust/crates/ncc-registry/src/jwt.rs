//! 本节点的令牌。载荷格式与 Go 侧逐字段一致（`sub/email/kind/node/scope/iat/exp`，
//! 空字段省略），这样 CLI 与平台侧拿到的令牌在两边都能验。

use serde::{Deserialize, Serialize};

use ncc_core::scope::AuthInfo;
use ncc_core::timeutil::now_unix;

/// 令牌载荷。`kind` 决定它能做什么：
///
/// * `user` 用户会话（JWT 登录）：代表用户本人，不受作用域限制
/// * `node` 节点令牌（由接入票据兑换）：只能用票据给的作用域
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Claims {
    pub sub: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub email: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
    pub iat: i64,
    pub exp: i64,
}

/// 用户会话令牌。
pub fn sign_user(secret: &str, sub: &str, email: &str, ttl: std::time::Duration) -> String {
    sign(
        secret,
        Claims {
            sub: sub.to_string(),
            email: email.to_string(),
            kind: "user".to_string(),
            ..Default::default()
        },
        ttl,
    )
}

/// 任意载荷签名（节点令牌、接入票据兑换等都用它）。
pub fn sign(secret: &str, mut c: Claims, ttl: std::time::Duration) -> String {
    let now = now_unix();
    c.iat = now;
    c.exp = now + ttl.as_secs() as i64;
    let payload = serde_json::to_string(&c).unwrap_or_else(|_| "{}".to_string());
    ncc_core::jwt::sign_hs256(secret, &payload)
}

/// 校验令牌并折成认证上下文。
pub fn parse(secret: &str, token: &str) -> Result<AuthInfo, String> {
    let v = ncc_core::jwt::verify_hs256(secret, token)?;
    let c: Claims = serde_json::from_value(v).map_err(|e| e.to_string())?;
    if c.sub.is_empty() {
        return Err("jwt 缺少主体".to_string());
    }
    if c.exp <= now_unix() {
        return Err("jwt 已过期".to_string());
    }
    match c.kind.as_str() {
        "user" => Ok(AuthInfo {
            user_id: c.sub,
            kind: "user".to_string(),
            session: true,
            ..Default::default()
        }),
        "node" => Ok(AuthInfo {
            user_id: c.sub,
            kind: "node".to_string(),
            node_id: c.node,
            scopes: c.scope,
            session: false,
            ..Default::default()
        }),
        _ => Err("jwt 类型错误".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 会话令牌往返() {
        let t = sign_user("sec", "U-1", "a@b.c", std::time::Duration::from_secs(3600));
        let info = parse("sec", &t).unwrap();
        assert_eq!(info.user_id, "U-1");
        assert!(info.session);
        assert!(parse("other", &t).is_err());
    }

    #[test]
    fn 节点令牌带作用域() {
        let t = sign(
            "sec",
            Claims {
                sub: "U-1".to_string(),
                kind: "node".to_string(),
                node: "ND-1".to_string(),
                scope: vec!["nodes:write".to_string()],
                ..Default::default()
            },
            std::time::Duration::from_secs(60),
        );
        let info = parse("sec", &t).unwrap();
        assert_eq!(info.kind, "node");
        assert_eq!(info.node_id, "ND-1");
        assert!(!info.session);
        assert_eq!(info.scopes, vec!["nodes:write"]);
    }

    #[test]
    fn 过期令牌被拒() {
        let t = sign(
            "sec",
            Claims {
                sub: "U-1".to_string(),
                kind: "user".to_string(),
                ..Default::default()
            },
            std::time::Duration::from_secs(0),
        );
        assert!(parse("sec", &t).is_err());
    }
}
