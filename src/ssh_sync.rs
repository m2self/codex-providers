use crate::codex_config::{CodexConfig, ProviderAuthSource};
use anyhow::{Context as _, Result};
use chrono::Local;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::tempdir;
use toml_edit::{DocumentMut, Item};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineConfig {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    pub machines: Vec<MachineConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteConfig {
    Missing,
    Present(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineStatus {
    pub name: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub precheck_results: Vec<MachineStatus>,
    pub added_provider_ids: Vec<String>,
    pub conflict_ids: Vec<String>,
    pub apply_results: Vec<MachineStatus>,
}

impl SyncReport {
    pub fn has_failures(&self) -> bool {
        self.apply_results
            .iter()
            .any(|result| result.status.starts_with("failed"))
    }
}

pub trait RemoteTransport {
    fn read_config(&self, machine: &MachineConfig) -> Result<RemoteConfig>;
    fn write_config(&self, machine: &MachineConfig, contents: &str) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePaths {
    pub home_dir: PathBuf,
    pub codex_dir: PathBuf,
    pub config_path: PathBuf,
    pub temp_path: PathBuf,
    pub backup_path: PathBuf,
}

pub struct OpenSshTransport {
    runner: OpenSshSftpRunner,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SftpBatchResult {
    stdout: String,
    stderr: String,
    exit_code: i32,
}

trait SftpRunner {
    fn probe(&self) -> Result<()>;
    fn run(&self, target: &str, batch: &str) -> Result<SftpBatchResult>;
}

struct OpenSshSftpRunner;

#[derive(Debug, Deserialize)]
struct RawSyncConfig {
    machines: Option<Vec<String>>,
}

struct RemoteSnapshot {
    machine: MachineConfig,
    config: CodexConfig,
}

#[derive(Debug)]
struct MergePlan {
    canonical_items: Vec<(String, Item)>,
    added_provider_ids: Vec<String>,
    conflict_ids: Vec<String>,
}

pub fn default_sync_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codex-providers")
        .join("sync.toml")
}

impl OpenSshTransport {
    pub fn new() -> Result<Self> {
        let runner = OpenSshSftpRunner;
        runner.probe()?;
        Ok(Self { runner })
    }
}

impl RemoteTransport for OpenSshTransport {
    fn read_config(&self, machine: &MachineConfig) -> Result<RemoteConfig> {
        read_config_via_sftp(machine, &self.runner)
    }

    fn write_config(&self, machine: &MachineConfig, contents: &str) -> Result<()> {
        write_config_via_sftp(machine, contents, &self.runner)
    }
}

impl SftpRunner for OpenSshSftpRunner {
    fn probe(&self) -> Result<()> {
        match Command::new("sftp")
            .arg("-V")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                anyhow::bail!("local OpenSSH `sftp` command was not found in PATH")
            }
            Err(err) => Err(err).with_context(|| "failed to probe local OpenSSH `sftp` command"),
        }
    }

    fn run(&self, target: &str, batch: &str) -> Result<SftpBatchResult> {
        let mut child = Command::new("sftp")
            .args(sftp_command_args(target))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to launch OpenSSH sftp for '{target}'"))?;

        {
            let stdin = child
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("failed to open stdin for OpenSSH sftp"))?;
            stdin
                .write_all(batch.as_bytes())
                .with_context(|| format!("failed to send batch commands to OpenSSH sftp for '{target}'"))?;
        }

        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to wait for OpenSSH sftp against '{target}'"))?;

        Ok(SftpBatchResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}

impl SyncConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.eq_ignore_ascii_case("sync.conf"))
            .unwrap_or(false)
        {
            anyhow::bail!(
                "sync.conf is no longer supported; rename it to sync.toml and use `machines = [..]`"
            );
        }

        if !path.exists() {
            let legacy_path = path.with_file_name("sync.conf");
            if legacy_path.exists() {
                anyhow::bail!(
                    "found legacy sync.conf at {}; migrate it to sync.toml with `machines = [..]`",
                    legacy_path.display()
                );
            }
        }

        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read sync config at {}", path.display()))?;
        parse_sync_config_text(&text)
            .with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn selected_machines(&self, selected_names: &[String]) -> Result<Vec<MachineConfig>> {
        if selected_names.is_empty() {
            return Ok(self.machines.clone());
        }

        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let machine_map: BTreeMap<&str, &MachineConfig> = self
            .machines
            .iter()
            .map(|machine| (machine.name.as_str(), machine))
            .collect();

        for name in selected_names {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(machine) = machine_map.get(name.as_str()) else {
                anyhow::bail!("machine '{}' not found in sync.toml", name);
            };
            out.push((*machine).clone());
        }

        Ok(out)
    }
}

pub fn parse_sync_config_text(text: &str) -> Result<SyncConfig> {
    if text.trim().is_empty() {
        anyhow::bail!("sync.toml must define `machines = [..]`");
    }
    let value: toml::Value = toml::from_str(text).with_context(|| "invalid TOML in sync.toml")?;
    let Some(table) = value.as_table() else {
        anyhow::bail!("sync.toml must contain a top-level table");
    };

    if table.contains_key("version") {
        anyhow::bail!(
            "legacy sync.conf format is no longer supported; use `machines = [\"host\"]` in sync.toml"
        );
    }

    let Some(machines_value) = table.get("machines") else {
        anyhow::bail!("sync.toml must define `machines = [..]`");
    };
    for key in table.keys() {
        if key != "machines" {
            anyhow::bail!("sync.toml only supports the `machines` field");
        }
    }
    let Some(machines_array) = machines_value.as_array() else {
        anyhow::bail!("sync.toml `machines` must be an array of SSH host aliases");
    };
    if machines_array.is_empty() {
        anyhow::bail!("sync.toml `machines` must not be empty");
    }

    let raw: RawSyncConfig =
        toml::from_str(text).with_context(|| "failed to decode sync.toml fields")?;
    let raw_machines = raw
        .machines
        .ok_or_else(|| anyhow::anyhow!("sync.toml must define `machines = [..]`"))?;

    let mut seen = BTreeSet::new();
    let mut machines = Vec::with_capacity(raw_machines.len());
    for name in raw_machines {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            anyhow::bail!("sync.toml machine names must not be empty");
        }
        if !seen.insert(trimmed.to_string()) {
            anyhow::bail!("duplicate machine '{}' in sync.toml", trimmed);
        }
        machines.push(MachineConfig {
            name: trimmed.to_string(),
        });
    }

    Ok(SyncConfig { machines })
}

pub fn derive_remote_paths(home_dir: &Path, machine_name: &str) -> RemotePaths {
    let home = normalize_remote_path(&home_dir.to_string_lossy());
    let codex_dir = join_remote_path(&home, ".codex");
    let config_path = join_remote_path(&codex_dir, "config.toml");
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    let temp_path = format!("{config_path}.tmp.{machine_name}.{stamp}");
    let backup_path = format!("{config_path}.bak.{stamp}");

    RemotePaths {
        home_dir: PathBuf::from(home),
        codex_dir: PathBuf::from(codex_dir),
        config_path: PathBuf::from(config_path),
        temp_path: PathBuf::from(temp_path),
        backup_path: PathBuf::from(backup_path),
    }
}

pub fn sync_providers(
    local_config: &mut CodexConfig,
    sync_config: &SyncConfig,
    selected_names: &[String],
    transport: &dyn RemoteTransport,
    dry_run: bool,
) -> Result<SyncReport> {
    let selected_machines = sync_config.selected_machines(selected_names)?;
    if selected_machines.is_empty() {
        anyhow::bail!("no machines configured in sync.toml");
    }

    let local_provider_items = collect_supported_provider_items(local_config, "local config.toml")?;
    let local_provider_ids: BTreeSet<String> = local_provider_items.keys().cloned().collect();

    let mut precheck_results = Vec::new();
    let mut remote_snapshots = Vec::new();

    for machine in selected_machines {
        match transport.read_config(&machine) {
            Ok(RemoteConfig::Missing) => {
                precheck_results.push(MachineStatus {
                    name: machine.name.clone(),
                    status: "missing config".to_string(),
                });
                remote_snapshots.push(RemoteSnapshot {
                    machine,
                    config: CodexConfig::empty_at(Path::new("config.toml")),
                });
            }
            Ok(RemoteConfig::Present(text)) => {
                let remote_config = CodexConfig::from_text(Path::new("config.toml"), &text)
                    .with_context(|| {
                        format!("failed to load remote config for '{}'", machine.name)
                    })?;
                collect_supported_provider_items(
                    &remote_config,
                    &format!("remote machine '{}'", machine.name),
                )?;
                precheck_results.push(MachineStatus {
                    name: machine.name.clone(),
                    status: "ok".to_string(),
                });
                remote_snapshots.push(RemoteSnapshot {
                    machine,
                    config: remote_config,
                });
            }
            Err(err) => {
                anyhow::bail!("failed to read remote config for '{}': {err:#}", machine.name);
            }
        }
    }

    let merge = build_merge_plan(&local_provider_items, &local_provider_ids, &remote_snapshots)?;

    let mut apply_results = Vec::new();
    let local_changed = replace_provider_table_if_needed(local_config, &merge.canonical_items)?;
    if dry_run {
        apply_results.push(MachineStatus {
            name: "local".to_string(),
            status: if local_changed {
                "would update".to_string()
            } else {
                "unchanged".to_string()
            },
        });
    } else if !local_changed {
        apply_results.push(MachineStatus {
            name: "local".to_string(),
            status: "unchanged".to_string(),
        });
    } else {
        match local_config.write_with_backup() {
            Ok(_) => apply_results.push(MachineStatus {
                name: "local".to_string(),
                status: "updated".to_string(),
            }),
            Err(err) => apply_results.push(MachineStatus {
                name: "local".to_string(),
                status: format!("failed: {err:#}"),
            }),
        }
    }

    for snapshot in &mut remote_snapshots {
        let changed = replace_provider_table_if_needed(&mut snapshot.config, &merge.canonical_items)?;
        if dry_run {
            apply_results.push(MachineStatus {
                name: snapshot.machine.name.clone(),
                status: if changed {
                    "would update".to_string()
                } else {
                    "unchanged".to_string()
                },
            });
            continue;
        }

        if !changed {
            apply_results.push(MachineStatus {
                name: snapshot.machine.name.clone(),
                status: "unchanged".to_string(),
            });
            continue;
        }

        let rendered = snapshot.config.render();
        match transport.write_config(&snapshot.machine, &rendered) {
            Ok(_) => apply_results.push(MachineStatus {
                name: snapshot.machine.name.clone(),
                status: "updated".to_string(),
            }),
            Err(err) => apply_results.push(MachineStatus {
                name: snapshot.machine.name.clone(),
                status: format!("failed: {err:#}"),
            }),
        }
    }

    Ok(SyncReport {
        precheck_results,
        added_provider_ids: merge.added_provider_ids,
        conflict_ids: merge.conflict_ids,
        apply_results,
    })
}

fn collect_supported_provider_items(
    config: &CodexConfig,
    origin: &str,
) -> Result<BTreeMap<String, Item>> {
    let mut items = BTreeMap::new();
    for id in config.provider_ids_in_order() {
        let auth = config.get_provider_auth_info(&id)?;
        match auth.source() {
            ProviderAuthSource::Inline => {}
            ProviderAuthSource::EnvAndInline => {}
            ProviderAuthSource::Env => anyhow::bail!(
                "provider '{}' in {} still uses env_key; run `migrate-inline-token` first",
                id,
                origin
            ),
            ProviderAuthSource::Missing => anyhow::bail!(
                "provider '{}' in {} is missing bearer auth; ssh-sync supports only inline providers",
                id,
                origin
            ),
        }

        let mut item = config.get_provider_item(&id)?;
        normalize_provider_item_for_sync(&mut item)
            .with_context(|| format!("provider '{}' in {} is not syncable", id, origin))?;
        items.insert(id, item);
    }
    Ok(items)
}

fn normalize_provider_item_for_sync(item: &mut Item) -> Result<()> {
    let table = item
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("provider item is not a table"))?;
    table
        .get("experimental_bearer_token")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing experimental_bearer_token"))?;
    let _ = table.remove("env_key");
    table["requires_openai_auth"] = toml_edit::value(false);
    Ok(())
}

fn build_merge_plan(
    local_provider_items: &BTreeMap<String, Item>,
    local_provider_ids: &BTreeSet<String>,
    remote_snapshots: &[RemoteSnapshot],
) -> Result<MergePlan> {
    let mut canonical_items: Vec<(String, Item)> = local_provider_items
        .iter()
        .map(|(id, item)| (id.clone(), item.clone()))
        .collect();
    let mut canonical_values: BTreeMap<String, toml::Value> = canonical_items
        .iter()
        .map(|(id, item)| provider_item_value(item).map(|value| (id.clone(), value)))
        .collect::<Result<_>>()?;
    let mut added_provider_ids = Vec::new();
    let mut conflict_ids = Vec::new();
    let mut seen_conflicts = BTreeSet::new();

    for snapshot in remote_snapshots {
        let remote_items = collect_supported_provider_items(
            &snapshot.config,
            &format!("remote machine '{}'", snapshot.machine.name),
        )?;
        for id in snapshot.config.provider_ids_in_order() {
            let Some(item) = remote_items.get(&id) else {
                continue;
            };
            let value = provider_item_value(item)?;
            match canonical_values.get(&id) {
                None => {
                    canonical_values.insert(id.clone(), value);
                    canonical_items.push((id.clone(), item.clone()));
                    if !local_provider_ids.contains(&id) {
                        added_provider_ids.push(id);
                    }
                }
                Some(existing) if *existing == value => {}
                Some(_) if local_provider_ids.contains(&id) => {
                    if seen_conflicts.insert(id.clone()) {
                        conflict_ids.push(id);
                    }
                }
                Some(_) => {}
            }
        }
    }

    Ok(MergePlan {
        canonical_items,
        added_provider_ids,
        conflict_ids,
    })
}

fn provider_item_value(item: &Item) -> Result<toml::Value> {
    let mut doc = DocumentMut::new();
    doc["provider"] = item.clone();
    let parsed: toml::Value =
        toml::from_str(&doc.to_string()).with_context(|| "failed to compare provider item")?;
    parsed
        .get("provider")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("failed to extract provider value"))
}

fn replace_provider_table_if_needed(
    config: &mut CodexConfig,
    canonical_items: &[(String, Item)],
) -> Result<bool> {
    let before = config.render();
    config.replace_provider_items(canonical_items)?;
    Ok(config.render() != before)
}

fn read_config_via_sftp(machine: &MachineConfig, runner: &dyn SftpRunner) -> Result<RemoteConfig> {
    let scratch = tempdir().with_context(|| "failed to create temporary directory for sftp")?;
    let paths = resolve_remote_paths_via_sftp(machine, runner)?;
    let local_path = scratch.path().join("remote-config.toml");
    let batch = format!(
        "-get {} {}\n",
        quote_sftp_path(&paths.config_path),
        quote_sftp_path(local_path.as_path())
    );
    let result = runner
        .run(&machine.name, &batch)
        .with_context(|| format!("OpenSSH sftp read failed for '{}'", machine.name))?;

    if local_path.exists() {
        let contents = fs::read_to_string(&local_path)
            .with_context(|| format!("failed to read downloaded config for '{}'", machine.name))?;
        return Ok(RemoteConfig::Present(contents));
    }

    if sftp_output_indicates_missing(&result, &paths.config_path) {
        return Ok(RemoteConfig::Missing);
    }

    anyhow::bail!(
        "OpenSSH sftp did not download remote config for '{}': {}",
        machine.name,
        summarize_sftp_output(&result)
    )
}

fn write_config_via_sftp(
    machine: &MachineConfig,
    contents: &str,
    runner: &dyn SftpRunner,
) -> Result<()> {
    let scratch = tempdir().with_context(|| "failed to create temporary directory for sftp")?;
    let paths = resolve_remote_paths_via_sftp(machine, runner)?;
    let local_path = scratch.path().join("upload-config.toml");
    fs::write(&local_path, contents)
        .with_context(|| format!("failed to stage config upload for '{}'", machine.name))?;

    let batch = format!(
        "-mkdir {}\n-rename {} {}\nput {} {}\nrename {} {}\n",
        quote_sftp_path(&paths.codex_dir),
        quote_sftp_path(&paths.config_path),
        quote_sftp_path(&paths.backup_path),
        quote_sftp_path(local_path.as_path()),
        quote_sftp_path(&paths.temp_path),
        quote_sftp_path(&paths.temp_path),
        quote_sftp_path(&paths.config_path),
    );
    let result = runner
        .run(&machine.name, &batch)
        .with_context(|| format!("OpenSSH sftp write failed for '{}'", machine.name))?;

    if result.exit_code != 0 {
        anyhow::bail!(
            "OpenSSH sftp could not update remote config for '{}': {}",
            machine.name,
            summarize_sftp_output(&result)
        );
    }

    Ok(())
}

fn resolve_remote_paths_via_sftp(machine: &MachineConfig, runner: &dyn SftpRunner) -> Result<RemotePaths> {
    let result = runner
        .run(&machine.name, "pwd\n")
        .with_context(|| format!("OpenSSH sftp pwd failed for '{}'", machine.name))?;
    let home = parse_sftp_home_dir(&result)?;
    Ok(derive_remote_paths(&home, &machine.name))
}

fn parse_sftp_home_dir(result: &SftpBatchResult) -> Result<PathBuf> {
    for line in combined_sftp_output(result).lines() {
        if let Some(home) = line.trim().strip_prefix("Remote working directory: ") {
            return Ok(PathBuf::from(home.trim()));
        }
    }

    anyhow::bail!(
        "failed to parse remote home directory from OpenSSH sftp output: {}",
        summarize_sftp_output(result)
    )
}

fn sftp_output_indicates_missing(result: &SftpBatchResult, remote_path: &Path) -> bool {
    let remote = normalize_remote_path(&remote_path.to_string_lossy());
    let combined = combined_sftp_output(result);
    combined.contains(&format!("File \"{remote}\" not found."))
        || combined.contains("No such file or directory")
        || combined.contains("not found")
}

fn combined_sftp_output(result: &SftpBatchResult) -> String {
    match (result.stdout.trim(), result.stderr.trim()) {
        ("", "") => String::new(),
        ("", stderr) => stderr.to_string(),
        (stdout, "") => stdout.to_string(),
        (stdout, stderr) => format!("{stdout}\n{stderr}"),
    }
}

fn summarize_sftp_output(result: &SftpBatchResult) -> String {
    let combined = combined_sftp_output(result);
    let trimmed = combined.trim();
    if trimmed.is_empty() {
        format!("exit code {}", result.exit_code)
    } else {
        trimmed.to_string()
    }
}

fn sftp_command_args(target: &str) -> Vec<String> {
    vec![
        "-b".to_string(),
        "-".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        target.to_string(),
    ]
}

fn quote_sftp_path(path: &Path) -> String {
    format!("\"{}\"", escape_sftp_path(path))
}

fn escape_sftp_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").replace('"', "\\\"")
}

fn normalize_remote_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

fn join_remote_path(base: &str, segment: &str) -> String {
    let base = normalize_remote_path(base);
    let segment = segment.trim_start_matches('/');
    if base == "/" {
        format!("/{segment}")
    } else if base.is_empty() {
        segment.to_string()
    } else {
        format!("{base}/{segment}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex_config;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::tempdir;

    fn load_config(path: &Path) -> CodexConfig {
        codex_config::CodexConfig::load_or_default(path).expect("config should load")
    }

    fn write_file(path: &Path, contents: &str) {
        fs::write(path, contents).expect("file should write");
    }

    #[derive(Default)]
    struct FakeTransport {
        reads: BTreeMap<String, RemoteConfig>,
        writes: Vec<(String, String)>,
    }

    impl RemoteTransport for RefCell<FakeTransport> {
        fn read_config(&self, machine: &MachineConfig) -> Result<RemoteConfig> {
            Ok(self
                .borrow()
                .reads
                .get(&machine.name)
                .cloned()
                .unwrap_or(RemoteConfig::Missing))
        }

        fn write_config(&self, machine: &MachineConfig, contents: &str) -> Result<()> {
            self.borrow_mut()
                .writes
                .push((machine.name.clone(), contents.to_string()));
            Ok(())
        }
    }

    struct FakeSftpRunner {
        home_dir: String,
        downloaded_contents: Option<String>,
        batches: RefCell<Vec<(String, String)>>,
        write_exit_code: i32,
    }

    impl FakeSftpRunner {
        fn new(home_dir: &str, downloaded_contents: Option<&str>) -> Self {
            Self {
                home_dir: home_dir.to_string(),
                downloaded_contents: downloaded_contents.map(ToString::to_string),
                batches: RefCell::new(Vec::new()),
                write_exit_code: 0,
            }
        }
    }

    impl SftpRunner for FakeSftpRunner {
        fn probe(&self) -> Result<()> {
            Ok(())
        }

        fn run(&self, target: &str, batch: &str) -> Result<SftpBatchResult> {
            self.batches
                .borrow_mut()
                .push((target.to_string(), batch.to_string()));

            if batch.trim() == "pwd" {
                return Ok(SftpBatchResult {
                    stdout: format!(
                        "sftp> pwd\nRemote working directory: {}\n",
                        self.home_dir
                    ),
                    stderr: String::new(),
                    exit_code: 0,
                });
            }

            if batch.contains("-get ") {
                let quoted = batch.split('"').skip(1).step_by(2).collect::<Vec<_>>();
                let remote_path = quoted
                    .first()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("missing remote path in test batch"))?;
                let local_path = quoted
                    .get(1)
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("missing local path in test batch"))?;

                if let Some(contents) = &self.downloaded_contents {
                    if let Some(parent) = Path::new(local_path).parent() {
                        fs::create_dir_all(parent).expect("download parent dir should exist");
                    }
                    fs::write(local_path, contents).expect("download target should write");
                    return Ok(SftpBatchResult {
                        stdout: format!("sftp> -get \"{remote_path}\" \"{local_path}\"\n"),
                        stderr: String::new(),
                        exit_code: 0,
                    });
                }

                return Ok(SftpBatchResult {
                    stdout: format!("File \"{remote_path}\" not found.\n"),
                    stderr: String::new(),
                    exit_code: 0,
                });
            }

            Ok(SftpBatchResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: self.write_exit_code,
            })
        }
    }

    #[test]
    fn sync_toml_loads_machine_names() {
        let dir = tempdir().expect("tempdir");
        let sync_path = dir.path().join("sync.toml");
        write_file(
            &sync_path,
            r#"
machines = ["work-linux", "office-win"]
"#,
        );

        let sync = SyncConfig::load(&sync_path).expect("sync.toml should parse");
        assert_eq!(
            sync.machines,
            vec![
                MachineConfig {
                    name: "work-linux".to_string()
                },
                MachineConfig {
                    name: "office-win".to_string()
                }
            ]
        );
    }

    #[test]
    fn sync_toml_rejects_empty_or_duplicate_machine_names() {
        let empty_err = parse_sync_config_text("").expect_err("empty config should fail");
        assert!(empty_err.to_string().contains("machines"));

        let dup_err = parse_sync_config_text(r#"machines = ["dup", "dup"]"#)
            .expect_err("duplicate machine names should fail");
        assert!(dup_err.to_string().contains("duplicate"));
    }

    #[test]
    fn sync_toml_rejects_old_sync_conf_shape() {
        let err = parse_sync_config_text(
            r#"
version = 1

[[machines]]
name = "old-style"
"#,
        )
        .expect_err("old sync.conf shape should fail");
        assert!(err.to_string().contains("sync.conf"));
    }

    #[test]
    fn sync_toml_rejects_unknown_selected_machine() {
        let sync = SyncConfig {
            machines: vec![MachineConfig {
                name: "known".to_string(),
            }],
        };

        let err = sync
            .selected_machines(&["missing".to_string()])
            .expect_err("unknown machine should fail");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn sync_toml_rejects_legacy_sync_conf_path() {
        let dir = tempdir().expect("tempdir");
        let legacy_path = dir.path().join("sync.conf");
        write_file(&legacy_path, "machines = [\"work-linux\"]");

        let err = SyncConfig::load(&legacy_path).expect_err("sync.conf path should fail");
        assert!(err.to_string().contains("sync.conf"));
    }

    #[test]
    fn derive_remote_paths_uses_codex_home_under_remote_home() {
        let paths = derive_remote_paths(Path::new("/home/alice"), "work-linux");
        assert_eq!(paths.home_dir, PathBuf::from("/home/alice"));
        assert_eq!(paths.codex_dir, PathBuf::from("/home/alice/.codex"));
        assert_eq!(paths.config_path, PathBuf::from("/home/alice/.codex/config.toml"));
        assert!(paths.temp_path.to_string_lossy().contains("config.toml.tmp.work-linux"));
        assert!(paths.backup_path.to_string_lossy().contains("config.toml.bak."));
    }

    #[test]
    fn openssh_command_args_include_accept_new() {
        assert_eq!(
            sftp_command_args("cap00"),
            vec![
                "-b".to_string(),
                "-".to_string(),
                "-o".to_string(),
                "StrictHostKeyChecking=accept-new".to_string(),
                "cap00".to_string(),
            ]
        );
    }

    #[test]
    fn native_sftp_read_downloads_remote_config() {
        let runner = FakeSftpRunner::new(
            "/share/home/shark",
            Some(
                r#"
[model_providers.gm00]
name = "OpenAI"
base_url = "https://gm00.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-gm00"
"#,
            ),
        );
        let machine = MachineConfig {
            name: "gm00".to_string(),
        };

        let remote = read_config_via_sftp(&machine, &runner).expect("native sftp read should succeed");

        match remote {
            RemoteConfig::Present(text) => {
                assert!(text.contains("https://gm00.example/v1"));
            }
            RemoteConfig::Missing => panic!("expected remote config to be present"),
        }

        let batches = runner.batches.borrow();
        assert_eq!(batches[0].0, "gm00");
        assert_eq!(batches[0].1.trim(), "pwd");
        assert!(batches[1].1.contains("/share/home/shark/.codex/config.toml"));
    }

    #[test]
    fn native_sftp_read_returns_missing_for_missing_remote_config() {
        let runner = FakeSftpRunner::new("/share/home/shark", None);
        let machine = MachineConfig {
            name: "gm00".to_string(),
        };

        let remote = read_config_via_sftp(&machine, &runner)
            .expect("missing remote config should not be fatal");
        assert_eq!(remote, RemoteConfig::Missing);

        let batches = runner.batches.borrow();
        assert_eq!(batches[0].1.trim(), "pwd");
        assert!(batches[1].1.contains("/share/home/shark/.codex/config.toml"));
    }

    #[test]
    fn native_sftp_write_uses_mkdir_put_and_rename_sequence() {
        let runner = FakeSftpRunner::new("/C:/Users/inter", None);
        let machine = MachineConfig {
            name: "cap00".to_string(),
        };

        write_config_via_sftp(&machine, "model_provider = \"cap00\"\n", &runner)
            .expect("native sftp write should succeed");

        let batches = runner.batches.borrow();
        assert_eq!(batches[0].1.trim(), "pwd");
        let write_batch = &batches[1].1;
        assert!(write_batch.contains("-mkdir \"/C:/Users/inter/.codex\""));
        assert!(write_batch.contains("-rename \"/C:/Users/inter/.codex/config.toml\""));
        assert!(write_batch.contains("put \""));
        assert!(write_batch.contains("\" \"/C:/Users/inter/.codex/config.toml.tmp.cap00."));
        assert!(write_batch.contains(
            "rename \"/C:/Users/inter/.codex/config.toml.tmp.cap00."
        ));
        assert!(write_batch.contains("\" \"/C:/Users/inter/.codex/config.toml\""));
    }

    #[test]
    fn sync_merges_remote_providers_keeps_local_conflicts_and_order() {
        let dir = tempdir().expect("tempdir");
        let local_path = dir.path().join("local.toml");
        write_file(
            &local_path,
            r#"
model_provider = "alpha"

[model_providers.alpha]
name = "OpenAI"
base_url = "https://alpha.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-alpha"

[model_providers.shared]
name = "OpenAI"
base_url = "https://local.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-local"
"#,
        );
        let mut local = load_config(&local_path);
        let transport = RefCell::new(FakeTransport {
            reads: [(
                "work-linux".to_string(),
                RemoteConfig::Present(
                    r#"
model_provider = "shared"

[model_providers.shared]
name = "OpenAI"
base_url = "https://remote.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-remote"

[model_providers.remote_only]
name = "OpenAI"
base_url = "https://remote-only.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-remote-only"
"#
                    .to_string(),
                ),
            )]
            .into_iter()
            .collect(),
            writes: Vec::new(),
        });
        let sync = SyncConfig {
            machines: vec![MachineConfig {
                name: "work-linux".to_string(),
            }],
        };

        let report = sync_providers(&mut local, &sync, &[], &transport, true)
            .expect("sync should succeed");

        assert_eq!(report.added_provider_ids, vec!["remote_only".to_string()]);
        assert_eq!(report.conflict_ids, vec!["shared".to_string()]);
        assert_eq!(
            local.provider_ids_in_order(),
            vec![
                "alpha".to_string(),
                "shared".to_string(),
                "remote_only".to_string()
            ]
        );
        let rendered = local.render();
        assert!(rendered.contains("https://local.example/v1"));
        assert!(!rendered.contains("https://remote.example/v1"));
        assert!(rendered.contains("https://remote-only.example/v1"));
        assert!(transport.borrow().writes.is_empty());
    }

    #[test]
    fn sync_rejects_legacy_env_key_before_any_write() {
        let dir = tempdir().expect("tempdir");
        let local_path = dir.path().join("local.toml");
        write_file(
            &local_path,
            r#"
[model_providers.legacy]
name = "OpenAI"
base_url = "https://legacy.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "CODEX_LEGACY_KEY"
"#,
        );
        let mut local = load_config(&local_path);
        let before = local.render();
        let transport = RefCell::new(FakeTransport::default());
        let sync = SyncConfig {
            machines: vec![MachineConfig {
                name: "work-linux".to_string(),
            }],
        };

        let err = sync_providers(&mut local, &sync, &[], &transport, false)
            .expect_err("legacy env_key should block sync");

        assert!(err.to_string().contains("migrate-inline-token"));
        assert_eq!(local.render(), before);
        assert!(transport.borrow().writes.is_empty());
    }
}
