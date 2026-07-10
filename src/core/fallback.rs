use crate::config::AppConfig;
use crate::core::alias::{self, ResolvedTarget};
use crate::core::types::*;
use crate::error::{should_fallback, GatewayError};
use crate::metrics::metrics;
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::RwLock;

/// cr-201: 通过 provider_type 选 adapter
fn pick_adapter(provider_type: &str) -> Box<dyn crate::backend::BackendAdapter> {
    crate::backend::adapter_for(provider_type)
}

#[derive(Debug)]
pub struct FallbackResult {
    pub response: InternalResponse,
    #[allow(dead_code)]
    pub used_target: ResolvedTarget,
}

pub async fn execute_with_fallback(
    client: &Client,
    config: Arc<RwLock<AppConfig>>,
    request: &InternalRequest,
) -> Result<FallbackResult, GatewayError> {
    // cr-202: 度量
    let start = Instant::now();
    let m = metrics();

    let config = config.read().await;
    let chain = alias::resolve_alias(&config, &request.model_alias)?;
    let timeout = Duration::from_secs(config.server.timeout_seconds);
    let mut last_error: Option<GatewayError> = None;

    for target in &chain {
        let provider = match config.providers.get(&target.provider_name) {
            Some(p) => p,
            None => {
                tracing::error!(provider = %target.provider_name, "Provider not found");
                last_error = Some(GatewayError::Internal(format!(
                    "Provider '{}' not found",
                    target.provider_name
                )));
                continue;
            }
        };

        tracing::info!(alias = %request.model_alias, provider = %target.provider_name, model = %target.model, "Trying backend");

        // cr-201: 通过 trait 调度
        let result = {
            let adapter = pick_adapter(&provider.provider_type);
            adapter.send(client, provider, request, &target.model, timeout).await
        };
        match &result {
            Ok(_) => {
                let duration = start.elapsed().as_secs_f64();
                m.requests_total
                    .with_label_values(&[&request.model_alias, "success"])
                    .inc();
                m.fallback_attempts_total
                    .with_label_values(&[&request.model_alias, &target.provider_name, "success"])
                    .inc();
                m.request_duration_seconds
                    .with_label_values(&[&request.model_alias, &target.provider_name])
                    .observe(duration);
                tracing::info!(alias = %request.model_alias, provider = %target.provider_name, model = %target.model, "Backend succeeded");
            }
            Err(GatewayError::BackendError { status, .. }) if should_fallback(*status) => {
                m.fallback_attempts_total
                    .with_label_values(&[&request.model_alias, &target.provider_name, "fallback"])
                    .inc();
                tracing::warn!(alias = %request.model_alias, provider = %target.provider_name, model = %target.model, status = status, "Backend failed, trying next");
            }
            Err(_) => {
                m.fallback_attempts_total
                    .with_label_values(&[&request.model_alias, &target.provider_name, "error"])
                    .inc();
                tracing::warn!(alias = %request.model_alias, provider = %target.provider_name, model = %target.model, error = ?result, "Backend failed, trying next");
            }
        }
        match result {
            Ok(response) => {
                return Ok(FallbackResult {
                    response,
                    used_target: target.clone(),
                });
            }
            Err(GatewayError::BackendError {
                status,
                ref body,
            }) if should_fallback(status) => {
                last_error = Some(GatewayError::BackendError {
                    status,
                    body: format!(
                        "provider={}, model={}: {}",
                        target.provider_name, target.model, body
                    ),
                });
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    // 所有 fallback 都失败
    m.requests_total
        .with_label_values(&[&request.model_alias, "fallback_exhausted"])
        .inc();
    Err(GatewayError::AllFallbacksExhausted(format!(
        "{} — last error: {}",
        request.model_alias,
        last_error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    )))
}

pub async fn execute_streaming_fallback(
    client: &Client,
    config: Arc<RwLock<AppConfig>>,
    request: &InternalRequest,
) -> Result<(reqwest::Response, ResolvedTarget), GatewayError> {
    let config = config.read().await;
    let chain = alias::resolve_alias(&config, &request.model_alias)?;
    let timeout = Duration::from_secs(config.server.timeout_seconds);
    let mut last_error: Option<GatewayError> = None;

    for target in &chain {
        let provider = match config.providers.get(&target.provider_name) {
            Some(p) => p,
            None => {
                last_error = Some(GatewayError::Internal(format!(
                    "Provider '{}' not found",
                    target.provider_name
                )));
                continue;
            }
        };

        tracing::info!(alias = %request.model_alias, provider = %target.provider_name, model = %target.model, "Trying backend (streaming)");

        // cr-201: 通过 trait 调度
        let stream_result = {
            let adapter = pick_adapter(&provider.provider_type);
            adapter
                .send_streaming(client, provider, request, &target.model, timeout)
                .await
                .map(|resp| (resp, true)) // 第二个字段占位（未来记录 adapter 来源）
        };

        match stream_result {
            Ok((resp, _is_anthropic)) => return Ok((resp, target.clone())),
            Err(GatewayError::BackendError {
                status,
                ref body,
            }) if should_fallback(status) => {
                tracing::warn!(provider = %target.provider_name, model = %target.model, status = status, "Streaming backend failed");
                last_error = Some(GatewayError::BackendError {
                    status,
                    body: format!(
                        "provider={}, model={}: {}",
                        target.provider_name, target.model, body
                    ),
                });
            }
            Err(e) => {
                tracing::warn!(provider = %target.provider_name, model = %target.model, error = %e, "Streaming backend failed");
                last_error = Some(e);
            }
        }
    }

    Err(GatewayError::AllFallbacksExhausted(format!(
        "{} — last error: {}",
        request.model_alias,
        last_error
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".to_string())
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use std::collections::HashMap;

    fn test_config() -> AppConfig {
        let mut providers = HashMap::new();
        providers.insert(
            "p1".to_string(),
            ProviderConfig {
                base_url: "http://127.0.0.1:19999/v1".to_string(),
                api_key: "key".to_string(),
                provider_type: "openai".to_string(),
                auth_style: "bearer".to_string(),
            },
        );
        let mut aliases = HashMap::new();
        aliases.insert(
            "Test".to_string(),
            AliasConfig {
                description: None,
                chain: vec![ChainEntry {
                    provider: "p1".to_string(),
                    model: "m1".to_string(),
                    priority: 1,
                }],
            },
        );
        AppConfig {
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 8080,
                timeout_seconds: 1,
                admin_token: None,
            },
            providers,
            aliases,
        }
    }

    fn test_request() -> InternalRequest {
        InternalRequest {
            model_alias: "Test".to_string(),
            system: None,
            messages: vec![InternalMessage {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "hello".to_string(),
                }],
            }],
            stream: false,
            temperature: None,
            max_tokens: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            top_p: None,
            top_k: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            n: None,
            stream_options: None,
        }
    }

    #[tokio::test]
    async fn test_fallback_unknown_alias() {
        let config = Arc::new(RwLock::new(test_config()));
        let client = Client::new();
        let mut req = test_request();
        req.model_alias = "NonExistent".to_string();
        let result = execute_with_fallback(&client, config, &req).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown model alias"));
    }

    #[tokio::test]
    async fn test_fallback_all_exhausted() {
        let config = Arc::new(RwLock::new(test_config()));
        let client = Client::new();
        let req = test_request();
        let result = execute_with_fallback(&client, config, &req).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("All fallback attempts exhausted"));
    }

    /// cr-201: 验证 provider_type → adapter 派发
    #[test]
    fn test_pick_adapter_dispatch() {
        let a = pick_adapter("anthropic");
        assert_eq!(a.name(), "anthropic_passthrough");
        let o = pick_adapter("openai");
        assert_eq!(o.name(), "openai_compat");
        // 未知 provider_type 走 OpenAI 兼容（默认）
        let u = pick_adapter("unknown-type");
        assert_eq!(u.name(), "openai_compat");
    }
}
