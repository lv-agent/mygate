use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub aliases: HashMap<String, AliasConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    #[serde(default = "default_provider_type")]
    pub provider_type: String,
    /// cr-003: 鉴权风格。
    /// - "bearer"（默认）：`Authorization: Bearer <api_key>`
    /// - "anthropic"：用于真实 Anthropic API，发 `x-api-key: <api_key>` + `anthropic-version: 2023-06-01`
    #[serde(default = "default_auth_style")]
    pub auth_style: String,
}

fn default_provider_type() -> String {
    "openai".to_string()
}

fn default_auth_style() -> String {
    "bearer".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct AliasConfig {
    pub description: Option<String>,
    pub chain: Vec<ChainEntry>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ChainEntry {
    pub provider: String,
    pub model: String,
    pub priority: u32,
}

impl AliasConfig {
    /// Return chain entries sorted by priority (ascending).
    pub fn sorted_chain(&self) -> Vec<&ChainEntry> {
        let mut entries: Vec<&ChainEntry> = self.chain.iter().collect();
        entries.sort_by_key(|e| e.priority);
        entries
    }
}

impl AppConfig {
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: AppConfig = toml::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        for (alias_name, alias) in &self.aliases {
            for entry in &alias.chain {
                if !self.providers.contains_key(&entry.provider) {
                    return Err(format!(
                        "alias '{}' references unknown provider '{}'",
                        alias_name, entry.provider
                    )
                    .into());
                }
            }
            if alias.chain.is_empty() {
                return Err(format!("alias '{}' has empty chain", alias_name).into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_valid_config() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.glm]
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key = "test-key"

[aliases.Simple]
description = "test"
[[aliases.Simple.chain]]
provider = "glm"
model = "glm-4-flash"
priority = 1
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.timeout_seconds, 30);
        assert!(config.providers.contains_key("glm"));
        assert!(config.aliases.contains_key("Simple"));
        let chain = config.aliases["Simple"].sorted_chain();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].model, "glm-4-flash");
    }

    #[test]
    fn test_sorted_chain_orders_by_priority() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.glm]
base_url = "https://example.com"
api_key = "key"

[providers.ds]
base_url = "https://example.com"
api_key = "key"

[aliases.Test]
[[aliases.Test.chain]]
provider = "ds"
model = "model-b"
priority = 2
[[aliases.Test.chain]]
provider = "glm"
model = "model-a"
priority = 1
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        let chain = config.aliases["Test"].sorted_chain();
        assert_eq!(chain[0].model, "model-a");
        assert_eq!(chain[1].model, "model-b");
    }

    #[test]
    fn test_validate_unknown_provider() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 8080

[aliases.Test]
[[aliases.Test.chain]]
provider = "nonexistent"
model = "model"
priority = 1
"#;
        let config: Result<AppConfig, _> = toml::from_str(toml_str);
        let parsed = config.unwrap();
        assert!(parsed.validate().is_err());
    }

    /// cr-003 Polish: 验证 auth_style 字段的默认值和自定义值
    #[test]
    fn test_auth_style_default_is_bearer() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.real-anthropic]
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
provider_type = "anthropic"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.providers["real-anthropic"].auth_style, "bearer");
    }

    #[test]
    fn test_auth_style_explicit_anthropic() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.real-anthropic]
base_url = "https://api.anthropic.com"
api_key = "sk-ant-test"
provider_type = "anthropic"
auth_style = "anthropic"
"#;
        let config: AppConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.providers["real-anthropic"].auth_style, "anthropic");
    }
}
