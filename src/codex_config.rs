use crate::bundle;
use anyhow::{Context as _, Result};
use chrono::Local;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use toml_edit::{table, value, DocumentMut, Item, Table};

const SCHEMA_HEADER: &str = "#:schema https://developers.openai.com/codex/config-schema.json\n";

#[derive(Debug, Clone)]
pub struct ProviderSummary {
    pub id: String,
    pub base_url: Option<String>,
    pub env_key: Option<String>,
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
            Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
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
        let mut out: Vec<String> = self
            .model_providers_table()
            .map(|t| t.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default();
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
            let (base_url, env_key) = match item.as_table() {
                Some(tbl) => (
                    tbl.get("base_url").and_then(|v| v.as_str()).map(|s| s.to_string()),
                    tbl.get("env_key").and_then(|v| v.as_str()).map(|s| s.to_string()),
                ),
                None => (None, None),
            };
            out.push(ProviderSummary { id, base_url, env_key });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
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

    pub fn add_or_update_provider(&mut self, id: &str, base_url: &str, env_key: &str) -> Result<()> {
        self.ensure_model_providers_table();
        self.doc["model_providers"][id] = table();

        let tbl = self
            .doc
            .get_mut("model_providers")
            .and_then(|i| i.get_mut(id))
            .and_then(|i| i.as_table_mut())
            .ok_or_else(|| anyhow::anyhow!("failed to access [model_providers.{id}] table"))?;

        tbl["name"] = value("OpenAI");
        tbl["base_url"] = value(base_url);
        tbl["wire_api"] = value("responses");
        tbl["requires_openai_auth"] = value(true);
        tbl["env_key"] = value(env_key);
        Ok(())
    }

    pub fn delete_provider(&mut self, id: &str) -> Result<()> {
        let current_default = self.get_model_provider();

        if let Some(t) = self.model_providers_table_mut() {
            t.remove(id);
        }

        if current_default.as_deref() == Some(id) {
            let remaining = self.provider_ids();
            if let Some(first) = remaining.first() {
                self.set_model_provider(first)?;
            } else {
                self.remove_model_provider();
            }
        }

        Ok(())
    }

    pub fn get_provider_env_key(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .get_provider_table(id)
            .and_then(|t| t.get("env_key"))
            .and_then(|i| i.as_str())
            .map(|s| s.to_string()))
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

        let env_key = t
            .get("env_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("provider '{}' missing env_key", id))?
            .to_string();

        Ok(bundle::ProviderConfigExport {
            name: "OpenAI".to_string(),
            base_url,
            wire_api: "responses".to_string(),
            requires_openai_auth: true,
            env_key,
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

    fn model_providers_table(&self) -> Option<&Table> {
        self.doc.get("model_providers")?.as_table()
    }

    fn model_providers_table_mut(&mut self) -> Option<&mut Table> {
        self.doc.get_mut("model_providers")?.as_table_mut()
    }

    fn get_provider_table(&self, id: &str) -> Option<&Table> {
        self.model_providers_table()?.get(id)?.as_table()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util;

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

        cfg.add_or_update_provider("zapi", "https://z.example/v1", &util::generate_env_key("zapi"))
            .unwrap();

        assert!(cfg.provider_exists("existing"));
        assert!(cfg.provider_exists("zapi"));
        assert_eq!(cfg.get_provider_base_url("zapi").unwrap().as_deref(), Some("https://z.example/v1"));
        assert!(cfg.render().contains("model = \"gpt-5.2\""));
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
}
