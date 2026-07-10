use crate::config::AppConfig;
use crate::core::alias::{self, ResolvedTarget};
use crate::core::types::*;
use crate::error::{should_fallback, GatewayError};
use reqwest::Client;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

#[derive(Debug)]
pub struct FallbackResult {
    pub response: InternalResponse,
    pub used_target: ResolvedTarget,
}

pub async fn execute_with_fallback(
    client: &Client,
    config: Arc<RwLock<AppConfig>>,
    request: &InternalRequest,
) -> Result<FallbackResult, GatewayError> {
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

        let result = if provider.provider_type == "anthropic" {
            crate::backend::anthropic_passthrough::send_anthropic_request(client, provider, request, &target.model).await
        } else {
            crate::backend::openai_compat::send_non_streaming(client, provider, request, &target.model, timeout).await
        };
        match result {
            Ok(response) => {
                tracing::info!(alias = %request.model_alias, provider = %target.provider_name, model = %target.model, "Backend succeeded");
                return Ok(FallbackResult {
                    response,
                    used_target: target.clone(),
                });
            }
            Err(GatewayError::BackendError {
                status,
                ref body,
            }) if should_fallback(status) => {
                tracing::warn!(alias = %request.model_alias, provider = %target.provider_name, model = %target.model, status = status, "Backend failed, trying next");
                last_error = Some(GatewayError::BackendError {
                    status,
                    body: format!(
                        "provider={}, model={}: {}",
                        target.provider_name, target.model, body
                    ),
                });
            }
            Err(e) => {
                tracing::warn!(alias = %request.model_alias, provider = %target.provider_name, model = %target.model, error = %e, "Backend failed, trying next");
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

        match crate::backend::openai_compat::send_streaming(
            client,
            provider,
            request,
            &target.model,
            timeout,
        )
        .await
        {
            Ok(resp) => return Ok((resp, target.clone())),
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
}
