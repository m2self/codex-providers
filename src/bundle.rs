use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BundleV1 {
    pub version: u32,
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub model_providers: BTreeMap<String, ProviderConfigExport>,
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfigExport {
    pub name: String,
    pub base_url: String,
    pub wire_api: String,
    pub requires_openai_auth: bool,
    pub env_key: String,
}

impl BundleV1 {
    pub fn parse(text: &str) -> Result<Self> {
        let bundle: BundleV1 =
            toml::from_str(text).with_context(|| "failed to parse bundle TOML")?;
        if bundle.version != 1 {
            anyhow::bail!("unsupported bundle version {} (expected 1)", bundle.version);
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
    fn bundle_roundtrip() {
        let mut model_providers = BTreeMap::new();
        model_providers.insert(
            "zapi".to_string(),
            ProviderConfigExport {
                name: "OpenAI".to_string(),
                base_url: "https://z.example/v1".to_string(),
                wire_api: "responses".to_string(),
                requires_openai_auth: true,
                env_key: "CODEX_ZAPI_KEY".to_string(),
            },
        );

        let mut secrets = BTreeMap::new();
        secrets.insert("CODEX_ZAPI_KEY".to_string(), "sk-test".to_string());

        let bundle = BundleV1 {
            version: 1,
            default_provider: Some("zapi".to_string()),
            model_providers,
            secrets,
        };

        let rendered = bundle.render_pretty_toml().unwrap();
        let parsed = BundleV1::parse(&rendered).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.default_provider.as_deref(), Some("zapi"));
        assert!(parsed.model_providers.contains_key("zapi"));
        assert_eq!(
            parsed.secrets.get("CODEX_ZAPI_KEY").map(|s| s.as_str()),
            Some("sk-test")
        );
    }
}

