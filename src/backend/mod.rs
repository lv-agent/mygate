pub mod anthropic_passthrough;
pub mod openai_compat;

use crate::config::ProviderConfig;
use crate::core::types::{InternalRequest, InternalResponse};
use crate::error::GatewayError;
use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;

/// cr-201: 后端适配器抽象。每种南向协议（OpenAI compat / Anthropic 等）
/// 实现这个 trait。fallback 用 `provider_type` 通过 factory 选 adapter。
#[async_trait]
pub trait BackendAdapter: Send + Sync {
    /// 适配器标识（用于 logging / metrics）
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    /// 非流式请求
    async fn send(
        &self,
        client: &Client,
        provider: &ProviderConfig,
        request: &InternalRequest,
        model: &str,
        timeout: Duration,
    ) -> Result<InternalResponse, GatewayError>;

    /// 流式请求。返回 reqwest::Response，body 是后端原始流。
    async fn send_streaming(
        &self,
        client: &Client,
        provider: &ProviderConfig,
        request: &InternalRequest,
        model: &str,
        timeout: Duration,
    ) -> Result<reqwest::Response, GatewayError>;
}

/// cr-201: 根据 provider_type 选 adapter。fallback 调这个函数。
pub fn adapter_for(provider_type: &str) -> Box<dyn BackendAdapter> {
    match provider_type {
        "anthropic" => Box::new(anthropic_passthrough::AnthropicPassthroughAdapter),
        _ => Box::new(openai_compat::OpenAiCompatAdapter),
    }
}
