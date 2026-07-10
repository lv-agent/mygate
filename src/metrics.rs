//! cr-202: Prometheus metrics
//!
//! 关键指标：
//! - `mygate_requests_total{alias, status}` — 请求总数（按 alias 和状态 success/error 分组）
//! - `mygate_fallback_attempts_total{alias, provider, outcome}` — fallback 链每次尝试（success/fallback/4xx_error）
//! - `mygate_request_duration_seconds{alias, provider}` — 请求耗时分布
//! - `mygate_active_streams` — 当前活跃流式连接数
//! - `mygate_config_reload_total{trigger}` — 配置重载次数
//!
//! 暴露方式：`GET /metrics`（Prometheus 文本格式）

use prometheus::{
    register_counter_vec, register_gauge, register_histogram_vec, CounterVec, Encoder, Gauge,
    HistogramVec, Registry, TextEncoder,
};
use std::sync::OnceLock;

pub struct Metrics {
    pub registry: Registry,
    pub requests_total: CounterVec,
    pub fallback_attempts_total: CounterVec,
    pub request_duration_seconds: HistogramVec,
    pub active_streams: Gauge,
    #[allow(dead_code)]
    pub config_reload_total: CounterVec,
    /// cr-202: token 用量统计（prompt / completion 按 alias）
    #[allow(dead_code)]
    pub tokens_total: CounterVec,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn metrics() -> &'static Metrics {
    METRICS.get_or_init(|| {
        let registry = Registry::new();

        let requests_total = register_counter_vec!(
            "mygate_requests_total",
            "Total chat completion requests by alias and status",
            &["alias", "status"]
        ).unwrap();

        let fallback_attempts_total = register_counter_vec!(
            "mygate_fallback_attempts_total",
            "Total fallback chain attempts by alias, provider, outcome",
            &["alias", "provider", "outcome"]
        ).unwrap();

        let request_duration_seconds = register_histogram_vec!(
            "mygate_request_duration_seconds",
            "Request duration in seconds",
            &["alias", "provider"],
            vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0]
        ).unwrap();

        let active_streams = register_gauge!(
            "mygate_active_streams",
            "Current number of active streaming connections"
        ).unwrap();

        let config_reload_total = register_counter_vec!(
            "mygate_config_reload_total",
            "Config reload count by trigger (sighup/http)",
            &["trigger"]
        ).unwrap();

        let tokens_total = register_counter_vec!(
            "mygate_tokens_total",
            "Token usage by alias and kind (prompt/completion)",
            &["alias", "kind"]
        ).unwrap();

        registry.register(Box::new(requests_total.clone())).unwrap();
        registry.register(Box::new(fallback_attempts_total.clone())).unwrap();
        registry.register(Box::new(request_duration_seconds.clone())).unwrap();
        registry.register(Box::new(active_streams.clone())).unwrap();
        registry.register(Box::new(config_reload_total.clone())).unwrap();
        registry.register(Box::new(tokens_total.clone())).unwrap();

        Metrics {
            registry,
            requests_total,
            fallback_attempts_total,
            request_duration_seconds,
            active_streams,
            config_reload_total,
            tokens_total,
        }
    })
}

/// 渲染 Prometheus 文本格式
pub fn render() -> Result<String, String> {
    let m = metrics();
    let mut buf = Vec::new();
    let encoder = TextEncoder::new();
    encoder
        .encode(&m.registry.gather(), &mut buf)
        .map_err(|e| e.to_string())?;
    String::from_utf8(buf).map_err(|e| e.to_string())
}

/// 处理 /metrics HTTP 请求
pub async fn metrics_handler() -> impl axum::response::IntoResponse {
    match render() {
        Ok(text) => (
            axum::http::StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
            text,
        ),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            [(axum::http::header::CONTENT_TYPE, "text/plain")],
            format!("error: {}", e),
        ),
    }
}
