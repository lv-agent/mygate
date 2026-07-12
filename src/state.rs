//! Application shared state — holds config and HTTP client, plus streaming helpers.

use crate::config::AppConfig;
use reqwest::Client;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<AppConfig>>,
    pub client: Client,
}

/// cr-202: 流式 stream 守卫，drop 时 dec `active_streams` gauge
pub struct ActiveStreamsGuard<S> {
    inner: S,
    dec_on_drop: bool,
}

impl<S> ActiveStreamsGuard<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            dec_on_drop: true,
        }
    }
}

impl<S> Drop for ActiveStreamsGuard<S> {
    fn drop(&mut self) {
        if self.dec_on_drop {
            crate::metrics::metrics().active_streams.dec();
        }
    }
}

impl<S: futures::Stream + Unpin> futures::Stream for ActiveStreamsGuard<S> {
    type Item = S::Item;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// Record non-streaming tokens_total metrics (identical pattern in both handlers)
pub fn record_tokens(alias: &str, prompt_tokens: Option<u64>, completion_tokens: Option<u64>) {
    let m = crate::metrics::metrics();
    if let Some(t) = prompt_tokens {
        m.tokens_total
            .with_label_values(&[alias, "prompt"])
            .inc_by(t as f64);
    }
    if let Some(t) = completion_tokens {
        m.tokens_total
            .with_label_values(&[alias, "completion"])
            .inc_by(t as f64);
    }
}
