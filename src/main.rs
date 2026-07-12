mod backend;
mod config;
mod core;
mod error;
mod metrics;
mod router;
mod server;
mod state;

use config::AppConfig;
use state::AppState;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::prelude::*;

#[tokio::main]
async fn main() {
    // ── 日志：终端 + 文件双输出 ──────────────────────────────
    let log_dir = std::env::var("MYGATE_LOG_DIR")
        .unwrap_or_else(|_| "logs".to_string());
    std::fs::create_dir_all(&log_dir).expect("Failed to create log directory");

    let file_appender = tracing_appender::rolling::daily(&log_dir, "mygate.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "mygate=info".parse().unwrap());

    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)   // 文件里不带 ANSI 颜色码
        .with_target(true); // 保留 target (模块路径) 方便定位

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    // 必须泄漏 _guard，否则离开 main 后 non_blocking writer 被 drop，
    // 剩余缓冲中的日志会丢失。
    // 优雅关闭时 tracing-appender 会自动 flush。
    Box::leak(Box::new(_guard));

    let config_path =
        std::env::var("MYGATE_CONFIG").unwrap_or_else(|_| "config.toml".to_string());

    let config = AppConfig::load(&config_path).unwrap_or_else(|e| {
        eprintln!("Failed to load config from {}: {}", config_path, e);
        std::process::exit(1);
    });

    tracing::info!(
        "MyGate loaded: {} aliases, {} providers",
        config.aliases.len(),
        config.providers.len()
    );

    let client = reqwest::Client::builder()
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("Failed to build HTTP client");
    let state = AppState {
        config: Arc::new(RwLock::new(config)),
        client,
    };

    let addr = {
        let cfg = state.config.read().await;
        format!("{}:{}", cfg.server.host, cfg.server.port)
    };
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("MyGate listening on {}", addr);

    let config_for_sighup = state.config.clone();
    let app = server::build_router(state);

    // SIGHUP handler for config hot reload
    tokio::spawn(async move {
        let mut stream = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("Failed to install SIGHUP handler");
        loop {
            stream.recv().await;
            let config_path = std::env::var("MYGATE_CONFIG").unwrap_or_else(|_| "config.toml".to_string());
            let result = AppConfig::load(&config_path).map_err(|e| e.to_string());
            match result {
                Ok(new_config) => {
                    let count = new_config.aliases.len();
                    *config_for_sighup.write().await = new_config;
                    // cr-202: config_reload_total counter
                    mygate::metrics::metrics()
                        .config_reload_total
                        .with_label_values(&["sighup"])
                        .inc();
                    tracing::info!("Config reloaded via SIGHUP: {} aliases", count);
                }
                Err(e) => tracing::error!("SIGHUP config reload failed: {}", e),
            }
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install CTRL+C handler");
    tracing::info!("Shutting down...");
}
