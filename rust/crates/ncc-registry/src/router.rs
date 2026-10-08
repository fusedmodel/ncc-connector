//! ncc-registry 的路由表。与 Go 版 `httpapi/server.go` 一一对应。
//!
//! 每一族路由由各自模块提供 `routes()`（相对 `/api`）与 `public_routes()`（顶层公开页），
//! 本文件只做拼装 —— 迁移某一族时只动一个文件，路由表不会变成公共热点。

use axum::response::IntoResponse;
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

    // 内置控制台：优先从 `NCCR_CONSOLE_DIR` 指向的目录托管（可替换、可定制）；
    // 目录不存在时退回**编译进二进制**的同一份页面。
    //
    // 为什么要退回：Go 版用 `go:embed` 把控制台打进二进制，**拷到哪里都能开**；
    // 只认目录的话，`cp` 出来的二进制（脚本、容器、单机部署都是这么干的）会让 `/` 直接 404，
    // 而 404 看起来像「服务坏了」，不像「少给了一个目录」。目录优先保留定制能力。
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
    } else if cfg.console {
        tracing::info!(
            "控制台   内置（目录 {} 不存在）→ /",
            cfg.console_dir.display()
        );
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
            .fallback(builtin_console);
    }

    app
}

/// 编译进二进制的控制台页面（与 `rust/web/index.html` 是同一份文件）。
///
/// `include_str!` 是编译期读文件 —— 改了 `rust/web/index.html` 要重新构建才生效，
/// 这正是 Go 版 `go:embed` 的行为，也是「拷走一个二进制就能用」的前提。
const BUILTIN_CONSOLE: &str = include_str!("../../../web/index.html");

async fn builtin_console() -> axum::response::Response {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        BUILTIN_CONSOLE,
    )
        .into_response()
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
