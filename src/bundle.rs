use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub const BUNDLE_VERSION_V1: u32 = 1;
pub const BUNDLE_VERSION_V2: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub version: u32,
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub model_providers: BTreeMap<String, ProviderConfigExport>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secrets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfigExport {
    pub name: String,
    pub base_url: String,
    pub wire_api: String,
    pub requires_openai_auth: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental_bearer_token: Option<String>,
}

impl Bundle {
    pub fn parse(text: &str) -> Result<Self> {
        let bundle: Bundle = toml::from_str(text).with_context(|| "failed to parse bundle TOML")?;
        if bundle.version != BUNDLE_VERSION_V1 && bundle.version != BUNDLE_VERSION_V2 {
            anyhow::bail!(
                "unsupported bundle version {} (expected 1 or 2)",
                bundle.version
            );
        }
        Ok(bundle)
    }

    pub fn render_pretty_toml(&self) -> Result<String> {
        let mut s = toml::to_string_pretty(self).with_context(|| "failed to render bundle TOML")?;
        if !s.ends_with('\n') {
            s.push('\n');
        }
        Ok(s)
    }
}

pub fn write_file(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_v2_roundtrip() {
        let mut model_providers = BTreeMap::new();
        model_providers.insert(
            "zapi".to_string(),
            ProviderConfigExport {
                name: "OpenAI".to_string(),
                base_url: "https://z.example/v1".to_string(),
                wire_api: "responses".to_string(),
                requires_openai_auth: false,
                env_key: None,
                experimental_bearer_token: Some("sk-test".to_string()),
            },
        );

        let bundle = Bundle {
            version: BUNDLE_VERSION_V2,
            default_provider: Some("zapi".to_string()),
            model_providers,
            secrets: BTreeMap::new(),
        };

        let rendered = bundle.render_pretty_toml().unwrap();
        let parsed = Bundle::parse(&rendered).unwrap();
        assert_eq!(parsed.version, BUNDLE_VERSION_V2);
        assert_eq!(parsed.default_provider.as_deref(), Some("zapi"));
        assert_eq!(
            parsed
                .model_providers
                .get("zapi")
                .and_then(|provider| provider.experimental_bearer_token.as_deref()),
            Some("sk-test")
        );
        assert!(parsed.secrets.is_empty());
    }

    #[test]
    fn bundle_v1_parse_keeps_legacy_fields() {
        let parsed = Bundle::parse(
            r#"
version = 1

[model_providers.legacy]
name = "OpenAI"
base_url = "https://legacy.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "CODEX_LEGACY_KEY"

[secrets]
CODEX_LEGACY_KEY = "sk-test"
"#,
        )
        .unwrap();

        assert_eq!(parsed.version, BUNDLE_VERSION_V1);
        assert_eq!(
            parsed
                .model_providers
                .get("legacy")
                .and_then(|provider| provider.env_key.as_deref()),
            Some("CODEX_LEGACY_KEY")
        );
        assert_eq!(
            parsed.secrets.get("CODEX_LEGACY_KEY").map(|s| s.as_str()),
            Some("sk-test")
        );
    }
}
