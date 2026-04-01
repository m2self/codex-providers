use crate::bundle;
use anyhow::{Context as _, Result};
use chrono::Local;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use toml_edit::{table, value, DocumentMut, Item, Table};

const SCHEMA_HEADER: &str = "#:schema https://developers.openai.com/codex/config-schema.json\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthSource {
    Env,
    Inline,
    EnvAndInline,
    Missing,
}

impl ProviderAuthSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Inline => "inline",
            Self::EnvAndInline => "env+inline",
            Self::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderAuthInfo {
    pub env_key: Option<String>,
    pub experimental_bearer_token: Option<String>,
}

impl ProviderAuthInfo {
    pub fn source(&self) -> ProviderAuthSource {
        match (
            self.env_key.as_ref().map(|s| !s.is_empty()).unwrap_or(false),
            self.experimental_bearer_token
                .as_ref()
                .map(|s| !s.is_empty())
                .unwrap_or(false),
        ) {
            (true, false) => ProviderAuthSource::Env,
            (false, true) => ProviderAuthSource::Inline,
            (true, true) => ProviderAuthSource::EnvAndInline,
            (false, false) => ProviderAuthSource::Missing,
        }
    }

    pub fn is_legacy_env(&self) -> bool {
        self.source() == ProviderAuthSource::Env
    }
}

#[derive(Debug, Clone)]
pub struct ProviderSummary {
    pub id: String,
    pub base_url: Option<String>,
    pub auth_source: ProviderAuthSource,
}

pub fn default_codex_config_path() -> PathBuf {
    let Some(home) = dirs::home_dir() else {
        return PathBuf::from(".codex/config.toml");
    };
    home.join(".codex").join("config.toml")
}

pub struct CodexConfig {
    path: PathBuf,
    doc: DocumentMut,
    insert_schema_header_on_render: bool,
}

impl CodexConfig {
    pub fn load_or_default(path: &Path) -> Result<Self> {
        let (doc, insert_schema_header_on_render) = match fs::read_to_string(path) {
            Ok(text) => (
                text.parse::<DocumentMut>()
                    .with_context(|| format!("invalid TOML in {}", path.display()))?,
                false,
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (
                "".parse::<DocumentMut>().expect("empty TOML is valid"),
                true,
            ),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", path.display()))
            }
        };

        Ok(Self {
            path: path.to_path_buf(),
            doc,
            insert_schema_header_on_render,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        if self.insert_schema_header_on_render {
            s.push_str(SCHEMA_HEADER);
        }
        s.push_str(&self.doc.to_string());
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s
    }

    pub fn write_with_backup(&mut self) -> Result<Option<PathBuf>> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create codex config directory at {}",
                    parent.display()
                )
            })?;
        }

        let backup_path = self.backup_path();
        let had_original = self.path.exists();

        if had_original {
            fs::rename(&self.path, &backup_path).with_context(|| {
                format!(
                    "failed to create backup (rename) {} -> {}",
                    self.path.display(),
                    backup_path.display()
                )
            })?;
        }

        let rendered = self.render();
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;

        let mut temp =
            NamedTempFile::new_in(parent).with_context(|| "failed to create temp file")?;
        temp.write_all(rendered.as_bytes())
            .with_context(|| "failed to write temp config")?;
        temp.flush().ok();

        match temp.persist(&self.path) {
            Ok(_) => Ok(had_original.then_some(backup_path)),
            Err(err) => {
                if had_original {
                    let _ = fs::rename(&backup_path, &self.path);
                }
                Err(anyhow::anyhow!(err).context("failed to persist config atomically"))
            }
        }
    }

    fn backup_path(&self) -> PathBuf {
        let timestamp = Local::now().format("%Y%m%d-%H%M%S");
        let filename = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "config.toml".to_string());
        let backup_name = format!("{filename}.bak.{timestamp}");
        self.path.with_file_name(backup_name)
    }

    pub fn provider_exists(&self, id: &str) -> bool {
        self.get_provider_table(id).is_some()
    }

    pub fn provider_ids(&self) -> Vec<String> {
        let mut out = self.provider_ids_in_order();
        out.sort();
        out
    }

    pub fn provider_ids_in_order(&self) -> Vec<String> {
        self.model_providers_table()
            .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default()
    }

    pub fn legacy_provider_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Some(table) = self.model_providers_table() else {
            return out;
        };

        for (id, item) in table.iter() {
            let Some(provider_table) = item.as_table() else {
                continue;
            };
            if provider_auth_info_from_table(provider_table).is_legacy_env() {
                out.push(id.to_string());
            }
        }
        out.sort();
        out
    }

    pub fn list_providers(&self) -> Vec<ProviderSummary> {
        let mut out = Vec::new();
        let Some(t) = self.model_providers_table() else {
            return out;
        };

        for (id, item) in t.iter() {
            let id = id.to_string();
            let (base_url, auth_source) = match item.as_table() {
                Some(tbl) => (
                    tbl.get("base_url").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    provider_auth_info_from_table(tbl).source(),
                ),
                None => (None, ProviderAuthSource::Missing),
            };
            out.push(ProviderSummary {
                id,
                base_url,
                auth_source,
            });
        }
        out
    }

    pub fn get_model_provider(&self) -> Option<String> {
        self.doc
            .get("model_provider")
            .and_then(|i| i.as_str())
            .map(|s| s.to_string())
    }

    pub fn set_model_provider(&mut self, id: &str) -> Result<()> {
        self.doc["model_provider"] = value(id);
        Ok(())
    }

    fn remove_model_provider(&mut self) {
        let _ = self.doc.as_table_mut().remove("model_provider");
    }

    pub fn add_or_update_provider_inline_token(
        &mut self,
        id: &str,
        base_url: &str,
        bearer_token: &str,
    ) -> Result<()> {
        let tbl = self.ensure_provider_table(id)?;
        write_common_provider_fields(tbl, base_url);
        write_inline_token_fields(tbl, bearer_token);
        Ok(())
    }

    pub fn add_or_update_provider_without_auth(&mut self, id: &str, base_url: &str) -> Result<()> {
        let tbl = self.ensure_provider_table(id)?;
        write_common_provider_fields(tbl, base_url);
        clear_auth_fields(tbl);
        tbl["requires_openai_auth"] = value(false);
        Ok(())
    }

    pub fn set_provider_base_url(&mut self, id: &str, base_url: &str) -> Result<()> {
        let tbl = self
            .get_provider_table_mut(id)
            .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", id))?;
        write_common_provider_fields(tbl, base_url);
        Ok(())
    }

    pub fn migrate_provider_to_inline_token(
        &mut self,
        id: &str,
        bearer_token: &str,
    ) -> Result<Option<String>> {
        let tbl = self
            .get_provider_table_mut(id)
            .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", id))?;
        let legacy_env_key = tbl
            .get("env_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        write_inline_token_fields(tbl, bearer_token);
        Ok(legacy_env_key)
    }

    pub fn delete_provider(&mut self, id: &str) -> Result<()> {
        let current_default = self.get_model_provider();

        if let Some(t) = self.model_providers_table_mut() {
            t.remove(id);
        }

        if current_default.as_deref() == Some(id) {
            let remaining = self.provider_ids_in_order();
            if let Some(first) = remaining.first() {
                self.set_model_provider(first)?;
            } else {
                self.remove_model_provider();
            }
        }

        Ok(())
    }

    pub fn reorder_providers(&mut self, ordered_ids: &[String]) -> Result<()> {
        self.ensure_model_providers_table();

        let mut current_ids = self.provider_ids_in_order();
        let mut desired_ids = ordered_ids.to_vec();
        current_ids.sort();
        desired_ids.sort();
        if current_ids != desired_ids {
            anyhow::bail!("reorder requires the same provider ids as the current config");
        }

        let reordered_items: Vec<(String, Item)> = {
            let table = self
                .model_providers_table()
                .ok_or_else(|| anyhow::anyhow!("missing [model_providers] table"))?;
            ordered_ids
                .iter()
                .map(|id| {
                    let item = table
                        .get(id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", id))?;
                    Ok((id.clone(), item))
                })
                .collect::<Result<Vec<_>>>()?
        };

        let mut table = Table::new();
        table.set_implicit(true);
        for (id, item) in reordered_items {
            let _ = table.insert(&id, item);
        }
        self.doc["model_providers"] = Item::Table(table);
        Ok(())
    }

    pub fn get_provider_auth_info(&self, id: &str) -> Result<ProviderAuthInfo> {
        let table = self
            .get_provider_table(id)
            .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", id))?;
        Ok(provider_auth_info_from_table(table))
    }

    pub fn get_provider_base_url(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .get_provider_table(id)
            .and_then(|t| t.get("base_url"))
            .and_then(|i| i.as_str())
            .map(|s| s.to_string()))
    }

    pub fn get_provider_export(&self, id: &str) -> Result<bundle::ProviderConfigExport> {
        let t = self
            .get_provider_table(id)
            .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", id))?;

        let base_url = t
            .get("base_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("provider '{}' missing base_url", id))?
            .to_string();

        let auth = provider_auth_info_from_table(t);
        let name = t
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("OpenAI")
            .to_string();
        let wire_api = t
            .get("wire_api")
            .and_then(|v| v.as_str())
            .unwrap_or("responses")
            .to_string();
        let requires_openai_auth = t
            .get("requires_openai_auth")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(bundle::ProviderConfigExport {
            name,
            base_url,
            wire_api,
            requires_openai_auth,
            env_key: auth.env_key,
            experimental_bearer_token: auth.experimental_bearer_token,
        })
    }

    fn ensure_model_providers_table(&mut self) {
        let needs_create = match self.doc.get("model_providers") {
            None => true,
            Some(item) => !item.is_table(),
        };
        if needs_create {
            let mut t = Table::new();
            t.set_implicit(true);
            self.doc["model_providers"] = Item::Table(t);
        }
    }

    fn ensure_provider_table(&mut self, id: &str) -> Result<&mut Table> {
        self.ensure_model_providers_table();
        let needs_create = self
            .get_provider_table(id)
            .map(|_| false)
            .unwrap_or(true);
        if needs_create {
            self.doc["model_providers"][id] = table();
        }

        self.get_provider_table_mut(id)
            .ok_or_else(|| anyhow::anyhow!("failed to access [model_providers.{id}] table"))
    }

    fn model_providers_table(&self) -> Option<&Table> {
        self.doc.get("model_providers")?.as_table()
    }

    fn model_providers_table_mut(&mut self) -> Option<&mut Table> {
        self.doc.get_mut("model_providers")?.as_table_mut()
    }

    fn get_provider_table(&self, id: &str) -> Option<&Table> {
        self.model_providers_table()?.get(id)?.as_table()
    }

    fn get_provider_table_mut(&mut self, id: &str) -> Option<&mut Table> {
        self.model_providers_table_mut()?.get_mut(id)?.as_table_mut()
    }
}

fn provider_auth_info_from_table(table: &Table) -> ProviderAuthInfo {
    ProviderAuthInfo {
        env_key: table
            .get("env_key")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        experimental_bearer_token: table
            .get("experimental_bearer_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

fn write_common_provider_fields(tbl: &mut Table, base_url: &str) {
    tbl["name"] = value("OpenAI");
    tbl["base_url"] = value(base_url);
    tbl["wire_api"] = value("responses");
}

fn clear_auth_fields(tbl: &mut Table) {
    let _ = tbl.remove("env_key");
    let _ = tbl.remove("experimental_bearer_token");
}

fn write_inline_token_fields(tbl: &mut Table, bearer_token: &str) {
    clear_auth_fields(tbl);
    tbl["requires_openai_auth"] = value(false);
    tbl["experimental_bearer_token"] = value(bearer_token);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_from(s: &str) -> DocumentMut {
        s.parse::<DocumentMut>().expect("valid toml")
    }

    #[test]
    fn add_provider_does_not_break_other_keys() {
        let mut cfg = CodexConfig {
            path: PathBuf::from("config.toml"),
            doc: doc_from(
                r#"
model = "gpt-5.2"

[model_providers.existing]
name = "OpenAI"
base_url = "https://a.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "CODEX_EXISTING_KEY"
"#,
            ),
            insert_schema_header_on_render: false,
        };

        cfg.add_or_update_provider_inline_token("zapi", "https://z.example/v1", "sk-inline")
            .unwrap();

        assert!(cfg.provider_exists("existing"));
        assert!(cfg.provider_exists("zapi"));
        assert_eq!(
            cfg.get_provider_base_url("zapi").unwrap().as_deref(),
            Some("https://z.example/v1")
        );
        let rendered = cfg.render();
        assert!(rendered.contains("model = \"gpt-5.2\""));
        assert!(rendered.contains("experimental_bearer_token = \"sk-inline\""));
        assert!(!rendered.contains("env_key = \"CODEX_ZAPI_KEY\""));
    }

    #[test]
    fn delete_provider_switches_default() {
        let mut cfg = CodexConfig {
            path: PathBuf::from("config.toml"),
            doc: doc_from(
                r#"
model_provider = "b"

[model_providers.a]
name = "OpenAI"
base_url = "https://a.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "CODEX_A_KEY"

[model_providers.b]
name = "OpenAI"
base_url = "https://b.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "CODEX_B_KEY"
"#,
            ),
            insert_schema_header_on_render: false,
        };

        cfg.delete_provider("b").unwrap();
        assert!(!cfg.provider_exists("b"));
        assert_eq!(cfg.get_model_provider().as_deref(), Some("a"));
    }

    #[test]
    fn list_providers_reports_auth_source() {
        let cfg = CodexConfig {
            path: PathBuf::from("config.toml"),
            doc: doc_from(
                r#"
[model_providers.env]
name = "OpenAI"
base_url = "https://env.example/v1"
wire_api = "responses"
env_key = "CODEX_ENV_KEY"

[model_providers.inline]
name = "OpenAI"
base_url = "https://inline.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-inline"
"#,
            ),
            insert_schema_header_on_render: false,
        };

        let providers = cfg.list_providers();
        assert_eq!(providers[0].auth_source, ProviderAuthSource::Env);
        assert_eq!(providers[1].auth_source, ProviderAuthSource::Inline);
    }

    #[test]
    fn migrate_provider_to_inline_token_removes_env_key() {
        let mut cfg = CodexConfig {
            path: PathBuf::from("config.toml"),
            doc: doc_from(
                r#"
[model_providers.legacy]
name = "OpenAI"
base_url = "https://legacy.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "CODEX_LEGACY_KEY"
"#,
            ),
            insert_schema_header_on_render: false,
        };

        let previous_key = cfg
            .migrate_provider_to_inline_token("legacy", "sk-inline")
            .unwrap();
        assert_eq!(previous_key.as_deref(), Some("CODEX_LEGACY_KEY"));

        let rendered = cfg.render();
        assert!(rendered.contains("experimental_bearer_token = \"sk-inline\""));
        assert!(rendered.contains("requires_openai_auth = false"));
        assert!(!rendered.contains("env_key = "));
    }

    #[test]
    fn list_and_delete_use_config_order() {
        let mut cfg = CodexConfig {
            path: PathBuf::from("config.toml"),
            doc: doc_from(
                r#"
model_provider = "b"

[model_providers.c]
name = "OpenAI"
base_url = "https://c.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-c"

[model_providers.a]
name = "OpenAI"
base_url = "https://a.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-a"

[model_providers.b]
name = "OpenAI"
base_url = "https://b.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-b"
"#,
            ),
            insert_schema_header_on_render: false,
        };

        assert_eq!(
            cfg.list_providers()
                .into_iter()
                .map(|provider| provider.id)
                .collect::<Vec<_>>(),
            vec!["c".to_string(), "a".to_string(), "b".to_string()]
        );

        cfg.delete_provider("b").unwrap();
        assert_eq!(cfg.get_model_provider().as_deref(), Some("c"));
    }

    #[test]
    fn reorder_providers_updates_table_order() {
        let mut cfg = CodexConfig {
            path: PathBuf::from("config.toml"),
            doc: doc_from(
                r#"
[model_providers.first]
name = "OpenAI"
base_url = "https://first.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-first"

[model_providers.second]
name = "OpenAI"
base_url = "https://second.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-second"

[model_providers.third]
name = "OpenAI"
base_url = "https://third.example/v1"
wire_api = "responses"
experimental_bearer_token = "sk-third"
"#,
            ),
            insert_schema_header_on_render: false,
        };

        cfg.reorder_providers(&[
            "third".to_string(),
            "first".to_string(),
            "second".to_string(),
        ])
        .unwrap();

        assert_eq!(
            cfg.provider_ids_in_order(),
            vec![
                "third".to_string(),
                "first".to_string(),
                "second".to_string()
            ]
        );
    }
}
