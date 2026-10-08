//! 待迁移：stack 的 HTTP 层。
//!
//! 原实现：ncc-registry/httpapi/state.go
//!
//! 迁移前这一族的路由由 `/api` 的未迁移兜底回 501（见 `router.rs`）。
//! 迁移方式：照 `httpapi/artifacts.rs` 的写法实现处理器，再在下面的 `routes()` 挂上。
//! 期望挂载点：`/kb, /mem, /ckpt`（相对 `/api`）

use axum::Router;

use crate::httpapi::AppState;

/// 该族路由（相对 `/api`）。
pub fn routes() -> Router<AppState> {
    Router::new()
}

/// 该族的顶层公开页（`/a/...`、`/s/...`、`/j/...` 这类不带 `/api` 前缀的路径）。
pub fn public_routes() -> Router<AppState> {
    Router::new()
}
