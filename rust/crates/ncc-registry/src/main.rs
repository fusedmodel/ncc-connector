//! ncc-registry：NCC 内网自托管节点（Rust / axum 重写版）。
//!
//! 一个二进制 + 一个数据目录就是一台可用的内网 Registry：
//! 制品托管 + 节点托管 + Agent 发现与互联，不依赖平台代码。
//!
//! 与 Go 版行为对齐的要点：
//!
//! * **同一份 SQLite schema**：建表语句是从 Go 服务建的库上 dump 出来的（`schema.rs`）；
//! * **同一套令牌与哈希**：HS256 JWT（载荷字段逐个对齐）、bcrypt 口令、token 只存 sha256；
//! * **同一套响应约定**：`{...}` / `{"error":{"code","message"}}`；
//! * **配置内容加密盒同构**：`enc:v1:` + base64(nonce‖ciphertext‖tag)，密钥由节点密钥派生
//!   —— 换实现不清库也读得出来。

mod config;
mod httpapi;
mod jwt;
mod router;
mod schema;
mod store;

use std::sync::Arc;

use ncc_core::secretbox::SecretBox;
use ncc_core::storage::LocalStorage;

use httpapi::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(e) = run().await {
        eprintln!("启动失败: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cfg = config::load()?;

    let pool = ncc_core::pool::open_sqlite(&cfg.db_path).await?;
    ncc_core::pool::migrate(&pool, schema::DDL).await?;

    let blobs = Arc::new(
        LocalStorage::new(&cfg.blob_dir, &cfg.public_url, "blobs")
            .map_err(|e| format!("初始化字节目录失败: {e}"))?,
    );
    // 配置内容的加密盒：密钥由节点密钥派生（换机器 / 丢数据目录就打不开，这是设计意图）
    let seal = Arc::new(
        SecretBox::new(&cfg.jwt_secret).map_err(|e| format!("初始化配置加密盒失败: {e}"))?,
    );

    let state = AppState {
        cfg: Arc::new(cfg.clone()),
        pool: pool.clone(),
        blobs,
        seal,
    };

    tracing::info!("节点     {} ({})", cfg.node_name, cfg.node_id);
    tracing::info!("角色     {}", cfg.role);
    tracing::info!("数据目录 {}", cfg.data_dir.display());
    tracing::info!("数据库   {}", cfg.db_path.display());
    tracing::info!("字节目录 {}", cfg.blob_dir.display());
    tracing::info!("监听     :{}", cfg.port);
    if cfg.invite_required() {
        tracing::info!("注册门禁 已开启（需要邀请码）");
    }
    if cfg.role == config::ROLE_WORKER {
        tracing::info!("master   {}", cfg.master_url);
    }

    let app = router::build(&state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(ncc_core::web::cors_layer(&cfg.cors_origins));

    let addr = format!("0.0.0.0:{}", cfg.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("监听 {addr} 失败: {e}"))?;
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("服务异常退出: {e}"))?;
    Ok(())
}
