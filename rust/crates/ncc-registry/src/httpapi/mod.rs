//! ncc-registry 的 HTTP 层。响应约定与平台 / CLI 一致：
//!
//! ```text
//! 成功  {"...": ...}                      错误  {"error":{"code","message"}}
//! 鉴权  Authorization: Bearer <JWT 或 ncc_ API-Key>
//! ```

pub mod access;
pub mod admin;
pub mod agentcards;
pub mod artifacts;
pub mod auth;
pub mod cluster;
pub mod configs;
pub mod conn;
pub mod exec;
pub mod feedback;
pub mod helpers;
pub mod index;
pub mod meta;
pub mod namespaces;
pub mod nodes;
pub mod p2p;
pub mod records;
pub mod shares;
pub mod stack;
pub mod traces;

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use sqlx::SqlitePool;

use ncc_core::error::ApiError;
use ncc_core::scope::{self, AuthInfo};
use ncc_core::secretbox::SecretBox;
use ncc_core::storage::LocalStorage;
use ncc_core::web;

use crate::config::Config;
#[allow(unused_imports)]
use crate::store;

/// 服务依赖聚合。
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub pool: SqlitePool,
    pub blobs: Arc<LocalStorage>,
    /// 配置内容的静态加密器（secret=true 的配置）。密钥由节点密钥派生，
    /// 换节点 / 丢数据目录就打不开 —— 这是设计意图，不是缺陷。
    pub seal: Arc<SecretBox>,
}

impl AppState {
    pub fn cfg(&self) -> &Config {
        &self.cfg
    }
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
    pub fn blobs(&self) -> &LocalStorage {
        &self.blobs
    }
    pub fn seal(&self) -> &SecretBox {
        &self.seal
    }
}

/// 认证上下文提取器（可选：没带凭据时是 `Auth(None)`，由各处理器自己决定要不要拦）。
///
/// 与 Go 的 `authMiddleware` 行为一致：
///
/// 1. `ncc_` 前缀走 API-Key（命中即 touch `last_used_at`）；
/// 2. 否则按 HS256 JWT 验签，且**每次都回查一次库** —— 被禁用的账号旧令牌立即失效
///    （否则「禁用」就只是拦登录，已经登进来的人照样通行）。
pub struct Auth(pub Option<AuthInfo>);

impl Auth {
    pub fn info(&self) -> Option<&AuthInfo> {
        self.0.as_ref()
    }

    /// 当前用户 id（没有凭据时 `None`）。
    pub fn user_id(&self) -> Option<String> {
        self.0.as_ref().map(|a| a.user_id.clone())
    }

    /// 是否满足某个作用域。
    pub fn allow(&self, scope: &str) -> bool {
        scope::allow(self.0.as_ref(), scope)
    }

    /// 要求已登录（任一凭据类型都可以）。
    pub fn require_login(&self) -> Result<&AuthInfo, ApiError> {
        self.0
            .as_ref()
            .ok_or_else(|| ApiError::unauthorized("需要登录"))
    }

    /// 要求满足某个作用域，否则 403。
    pub fn require_scope(&self, scope: &str) -> Result<&AuthInfo, ApiError> {
        let Some(a) = self.0.as_ref() else {
            return Err(ApiError::unauthorized("需要登录"));
        };
        if !scope::allow(self.0.as_ref(), scope) {
            return Err(ApiError::forbidden(format!("缺少作用域 {scope}")));
        }
        Ok(a)
    }

    /// 要求用户会话（代表本人，而不是一把受限的 key）。
    pub fn require_session(&self) -> Result<&AuthInfo, ApiError> {
        match self.0.as_ref() {
            Some(a) if a.session => Ok(a),
            Some(_) => Err(ApiError::forbidden(
                "该操作需要用户会话（API-Key 不可代做）",
            )),
            None => Err(ApiError::unauthorized("需要登录")),
        }
    }
}

/// 按凭据解析认证上下文（无凭据 / 凭据无效都返回 None，与 Go 的中间件一致）。
pub async fn resolve_auth(state: &AppState, headers: &HeaderMap) -> Option<AuthInfo> {
    let token = web::bearer(headers)?;

    if token.starts_with("ncc_") {
        if let Ok(Some(k)) = store::apikeys::find_by_secret(state.pool(), &token).await {
            let _ = store::apikeys::touch(state.pool(), &k.id).await;
            if let Ok(Some(u)) = store::users::by_id(state.pool(), &k.user_id).await {
                if !u.disabled {
                    return Some(AuthInfo {
                        user_id: u.id,
                        email: u.email,
                        kind: "key".to_string(),
                        key_id: k.id,
                        scopes: store::parse_list(&k.scopes),
                        session: false,
                        ..Default::default()
                    });
                }
            }
        }
        return None;
    }

    let info = crate::jwt::parse(&state.cfg().jwt_secret, &token).ok()?;
    let u = store::users::by_id(state.pool(), &info.user_id)
        .await
        .ok()??;
    if u.disabled {
        return None;
    }
    Some(AuthInfo {
        email: u.email,
        // 只有用户会话代表本人（不受作用域限制）；节点令牌按票据作用域走。
        session: info.kind == "user",
        ..info
    })
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        // 认证失败不阻断请求：绝大多数端点允许匿名访问，是否强制由处理器决定。
        Ok(Auth(resolve_auth(state, &parts.headers).await))
    }
}
