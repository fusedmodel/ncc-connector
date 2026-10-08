//! ncc-registry 的路由表。与 Go 版 `httpapi/server.go` 一一对应。
//!
//! 每一族路由由各自模块提供 `routes()`（相对 `/api`）与 `public_routes()`（顶层公开页），
//! 本文件只做拼装 —— 迁移某一族时只动一个文件，路由表不会变成公共热点。

use axum::Router;
use tower_http::services::ServeDir;

use crate::httpapi::{self, AppState};

/// API 路由（挂到 `/api`）。
pub fn api_router() -> Router<AppState> {
    Router::new()
        .merge(httpapi::auth::routes())
        .merge(httpapi::namespaces::routes())
        .merge(httpapi::artifacts::routes())
        .merge(httpapi::nodes::routes())
        .merge(httpapi::configs::routes())
        .merge(httpapi::traces::routes())
        .merge(httpapi::stack::routes())
        .merge(httpapi::records::routes())
        .merge(httpapi::index::routes())
        .merge(httpapi::access::routes())
        .merge(httpapi::p2p::routes())
        .merge(httpapi::shares::routes())
        .merge(httpapi::agentcards::routes())
        .merge(httpapi::conn::routes())
        .merge(httpapi::exec::routes())
        .merge(httpapi::feedback::routes())
        .merge(httpapi::admin::routes())
        .merge(httpapi::cluster::routes())
}

/// 顶层公开页（`/j/{key}`、`/s/{token}`、`/a/{token}`）。
pub fn public_router() -> Router<AppState> {
    Router::new()
        .merge(httpapi::access::public_routes())
        .merge(httpapi::shares::public_routes())
        .merge(httpapi::agentcards::public_routes())
}

/// 整站路由：`/api` + 顶层公开页 + 静态字节 + 未迁移兜底。
pub fn build(state: &AppState) -> Router {
    let cfg = state.cfg();

    let api = api_router()
        .route("/health", axum::routing::get(health))
        .route("/meta", axum::routing::get(httpapi::meta::meta))
        .fallback(ncc_core::error::fallback_not_migrated);

    // 上传端点要能吃大包（制品字节最大 256MB）；默认 body 上限只有 2MB。
    let api = api.layer(axum::extract::DefaultBodyLimit::max(256 << 20));

    let mut app = Router::new()
        .nest("/api", api)
        .merge(public_router())
        // 制品字节：公开静态目录
        .nest_service("/blobs", ServeDir::new(&cfg.blob_dir))
        .with_state(state.clone());

    // 内置控制台：从 `NCCR_CONSOLE_DIR` 指向的目录托管，目录不存在就不挂。
    //
    // 刻意**不用** `env!("CARGO_MANIFEST_DIR")`：那是编译期路径，二进制搬到别的机器上
    // 就指向一个不存在的目录，而且会随构建环境变化 —— 部署里没有比这更难查的事。
    if cfg.console && cfg.console_dir.is_dir() {
        tracing::info!("控制台   {} → /", cfg.console_dir.display());
        // ⚠️ 必须用 `fallback_service`：axum 0.8 对 `nest_service("/")` 直接 panic
        // （"Nesting at the root is no longer supported"）—— 而且是在**真实进程启动时**
        // 才炸，只建 router 的单测（控制台目录不存在时不挂）碰不到这条路径。
        // `/console` 是给人念的地址（Go 那边 302 跳到控制台根）：状态码照抄 302，
        // 不换成 301/307 —— 控制台可能换部署位置，缓存一个永久跳转会让人莫名其妙。
        app = app
            .route(
                "/console",
                axum::routing::get(|| async {
                    (
                        axum::http::StatusCode::FOUND,
                        [(axum::http::header::LOCATION, "/")],
                    )
                }),
            )
            .fallback_service(ServeDir::new(&cfg.console_dir));
    }

    app
}

/// GET /api/health
async fn health(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<axum::response::Response, ncc_core::error::ApiError> {
    let (artifacts, nodes, users) = httpapi::helpers::counts(&state).await;
    Ok(httpapi::helpers::ok_json(serde_json::json!({
        "ok": true,
        "service": "ncc-registry",
        "version": ncc_core::REGISTRY_VERSION,
        "role": state.cfg().role,
        "nodeId": state.cfg().node_id,
        "time": ncc_core::timeutil::now_rfc3339(),
        "artifacts": artifacts,
        "nodes": nodes,
        "users": users,
    })))
}
