//! HTTP 错误。响应体与 Go 侧一字不差：
//!
//! ```json
//! {"error": {"code": "not_found", "message": "制品不存在"}}
//! ```
//!
//! 这个形状是 `ncc` CLI 与前端 SPA 都在解析的契约，改成 `{message: ...}` 之类的
//! 「更简洁」形态会让两端各坏一半。

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// 业务错误：HTTP 状态 + 机器可读 code + 人类可读 message。
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_string(),
            message: message.into(),
        }
    }

    pub fn bad_request(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn conflict(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// 还没迁移到 Rust 的端点：明确回 501 而不是 404。
    ///
    /// 404 会让人以为是路由写错；501 让「这条路还没搬」在客户端与日志里一眼可见，
    /// 迁移期间排查问题不用再去翻代码确认。
    pub fn not_implemented(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_IMPLEMENTED, "not_implemented", message)
    }

    /// 把数据库错误折成内部错误：**不回显底层报文**（里面可能夹着 SQL 片段与表结构）。
    pub fn from_db(err: sqlx::Error) -> Self {
        tracing::error!("数据库错误: {err}");
        Self::internal("服务内部错误")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

/// `Result<T, ApiError>` 的短别名。
pub type ApiResult<T> = Result<T, ApiError>;

/// 数据库操作的统一收口：`?` 直接转 `ApiError`。
pub fn db<T>(r: Result<T, sqlx::Error>) -> ApiResult<T> {
    r.map_err(ApiError::from_db)
}

/// 成功响应：`200 {"...": ...}`。
pub fn ok(body: serde_json::Value) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

/// 「尚未迁移」的兜底路由：任何没被具体路由匹配上的 `/api/*` 请求都回 501。
///
/// 为什么要有这条兜底：迁移是一族一族做的，漏掉的路由如果静默 404，
/// 排查时最容易被误判成「路由写错」。501 + 明确提示能一眼看出是「还没搬」。
/// 给一个 Router 挂上「未迁移」兜底（只影响未匹配的路径）。
pub fn with_pending_fallback<S: Clone + Send + Sync + 'static>(
    router: axum::Router<S>,
) -> axum::Router<S> {
    router.fallback(fallback_not_migrated)
}

/// 兜底处理器：把请求路径原样报回，便于对着路由表核对。
pub async fn fallback_not_migrated(uri: axum::http::Uri) -> ApiError {
    ApiError::not_implemented(format!(
        "{} 没有这条路由（Rust 版未注册；路由总表见 README 的「迁移状态」一节）",
        uri.path()
    ))
}

/// 指定状态码的成功响应。
pub fn ok_status(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误响应形状() {
        let e = ApiError::not_found("制品不存在");
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        assert_eq!(e.code, "not_found");
        let body = json!({"error": {"code": e.code, "message": e.message}});
        assert_eq!(body["error"]["code"], "not_found");
    }
}
