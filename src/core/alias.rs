use crate::config::AppConfig;
use crate::error::GatewayError;

/// A resolved target: which provider and model to call.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub provider_name: String,
    pub model: String,
}

/// Resolve a model alias into an ordered fallback chain.
pub fn resolve_alias(
    config: &AppConfig,
    alias: &str,
) -> Result<Vec<ResolvedTarget>, GatewayError> {
    let alias_config = config
        .aliases
        .get(alias)
        .ok_or_else(|| GatewayError::UnknownAlias(alias.to_string()))?;

    let chain = alias_config.sorted_chain();
    let targets: Vec<ResolvedTarget> = chain
        .iter()
        .map(|entry| ResolvedTarget {
            provider_name: entry.provider.clone(),
            model: entry.model.clone(),
        })
        .collect();

    Ok(targets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use std::collections::HashMap;

    fn make_test_config() -> AppConfig {
        let mut providers = HashMap::new();
        providers.insert(
            "glm".to_string(),
            ProviderConfig {
                base_url: "https://glm.example.com/v1".to_string(),
                api_key: "glm-key".to_string(),
                provider_type: "openai".to_string(),
                auth_style: "bearer".to_string(),
            },
        );
        providers.insert(
            "deepseek".to_string(),
            ProviderConfig {
                base_url: "https://ds.example.com/v1".to_string(),
                api_key: "ds-key".to_string(),
                provider_type: "openai".to_string(),
                auth_style: "bearer".to_string(),
            },
        );

        let mut aliases = HashMap::new();
        aliases.insert(
            "Simple".to_string(),
            AliasConfig {
                description: Some("test".to_string()),
                chain: vec![
                    ChainEntry {
                        provider: "glm".to_string(),
                        model: "glm-4-flash".to_string(),
                        priority: 2,
                    },
                    ChainEntry {
                        provider: "deepseek".to_string(),
                        model: "ds-chat".to_string(),
                        priority: 1,
                    },
                ],
            },
        );

        AppConfig {
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 8080,
                timeout_seconds: 30,
                admin_token: None,
            },
            providers,
            aliases,
        }
    }

    #[test]
    fn test_resolve_existing_alias() {
        let config = make_test_config();
        let chain = resolve_alias(&config, "Simple").unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].provider_name, "deepseek");
        assert_eq!(chain[0].model, "ds-chat");
        assert_eq!(chain[1].provider_name, "glm");
        assert_eq!(chain[1].model, "glm-4-flash");
    }

    #[test]
    fn test_resolve_unknown_alias() {
        let config = make_test_config();
        let result = resolve_alias(&config, "NonExistent");
        assert!(result.is_err());
    }

    /// cr-000 Polish: 验证 anthropic provider_type 在别名链路中能被解析
    /// （不影响 resolve_alias 本身，但要确认 ProviderConfig::provider_type 字段
    /// 对 anthropic 类型 provider 也能正常加载和参与排序）
    #[test]
    fn test_resolve_alias_with_anthropic_provider() {
        let mut providers = HashMap::new();
        providers.insert(
            "anthropic-direct".to_string(),
            ProviderConfig {
                base_url: "https://api.anthropic.com".to_string(),
                api_key: "sk-ant-test".to_string(),
                provider_type: "anthropic".to_string(),
                auth_style: "anthropic".to_string(),
            },
        );
        let mut aliases = HashMap::new();
        aliases.insert(
            "Plan".to_string(),
            AliasConfig {
                description: Some("uses anthropic direct".to_string()),
                chain: vec![ChainEntry {
                    provider: "anthropic-direct".to_string(),
                    model: "claude-sonnet-4-5".to_string(),
                    priority: 1,
                }],
            },
        );
        let config = AppConfig {
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 8080,
                admin_token: None,
                timeout_seconds: 30,
            },
            providers,
            aliases,
        };
        let chain = resolve_alias(&config, "Plan").unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].provider_name, "anthropic-direct");
        assert_eq!(chain[0].model, "claude-sonnet-4-5");
    }
}
