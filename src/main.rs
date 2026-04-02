mod add_assist;
mod bundle;
mod cli;
mod codex_config;
mod env_store;
mod probe;
mod ssh_sync;
mod util;

use anyhow::{Context as _, Result};
use clap::Parser;
use std::collections::BTreeMap;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = cli::Cli::parse();

    let opts = GlobalOpts {
        no_env: cli.no_env,
        dry_run: cli.dry_run,
    };

    let config_path = cli
        .config_path
        .clone()
        .unwrap_or_else(codex_config::default_codex_config_path);
    let mut config = codex_config::CodexConfig::load_or_default(&config_path)
        .with_context(|| format!("failed to load codex config at {}", config_path.display()))?;

    match cli.command {
        cli::Command::List => cmd_list(&config),
        cli::Command::ProbeSelect => cmd_probe_select(&mut config, &opts),
        cli::Command::SshSync(args) => cmd_ssh_sync(&mut config, &opts, args),
        cli::Command::Add(args) => cmd_add(&mut config, &opts, args),
        cli::Command::Update(args) => cmd_update(&mut config, &opts, args),
        cli::Command::Delete(args) => cmd_delete(&mut config, &opts, args),
        cli::Command::Export(args) => cmd_export(&config, &opts, args),
        cli::Command::Import(args) => cmd_import(&mut config, &opts, args),
        cli::Command::MigrateInlineToken(args) => cmd_migrate_inline_token(&mut config, &opts, args),
    }
}

fn cmd_probe_select(config: &mut codex_config::CodexConfig, opts: &GlobalOpts) -> Result<()> {
    let runner = probe::HttpProbeRunner::new()?;
    cmd_probe_select_with_runner(config, opts, &runner)
}

fn cmd_probe_select_with_runner(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    runner: &dyn probe::ProbeRunner,
) -> Result<()> {
    let ordered_ids = config.provider_ids_in_order();
    if ordered_ids.is_empty() {
        anyhow::bail!("no providers found to probe");
    }

    let mut winner_index = None;
    let mut winner_id = None;

    for (index, id) in ordered_ids.iter().enumerate() {
        let result = probe_single_provider(config, id, runner)?;
        eprintln!("{}\t{}", result.id, result.summary());
        if result.is_success() {
            winner_index = Some(index);
            winner_id = Some(id.clone());
            break;
        }
    }

    let Some(winner_index) = winner_index else {
        anyhow::bail!("no available provider found");
    };
    let winner_id = winner_id.expect("winner id should be present");

    config.set_model_provider(&winner_id)?;
    config.reorder_providers(&reordered_probe_ids(&ordered_ids, winner_index))?;
    eprintln!("selected\t{winner_id}");

    finish_write_config(config, opts)
}

fn cmd_ssh_sync(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::SshSyncArgs,
) -> Result<()> {
    let sync_config_path = args
        .sync_config
        .clone()
        .unwrap_or_else(ssh_sync::default_sync_config_path);
    let sync_config = ssh_sync::SyncConfig::load(&sync_config_path)
        .with_context(|| format!("failed to load sync config at {}", sync_config_path.display()))?;
    let transport = ssh_sync::OpenSshTransport::new()?;
    let report = ssh_sync::sync_providers(
        config,
        &sync_config,
        &args.names,
        &transport,
        opts.dry_run,
    )?;

    for result in &report.precheck_results {
        eprintln!("precheck\t{}\t{}", result.name, result.status);
    }
    if !report.added_provider_ids.is_empty() {
        eprintln!(
            "merge\tadded {}",
            report.added_provider_ids.join(", ")
        );
    }
    if !report.conflict_ids.is_empty() {
        eprintln!(
            "merge\tlocal-wins {}",
            report.conflict_ids.join(", ")
        );
    }
    for result in &report.apply_results {
        eprintln!("apply\t{}\t{}", result.name, result.status);
    }

    if report.has_failures() {
        anyhow::bail!("ssh-sync completed with failures");
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct GlobalOpts {
    no_env: bool,
    dry_run: bool,
}

#[derive(Debug, Clone)]
struct LegacyMigration {
    id: String,
    env_key: String,
    token: String,
}

fn cmd_list(config: &codex_config::CodexConfig) -> Result<()> {
    let providers = config.list_providers();
    if providers.is_empty() {
        println!("(no providers found)");
        return Ok(());
    }

    for provider in providers {
        let base_url = provider.base_url.unwrap_or_else(|| "<missing>".to_string());
        println!(
            "{}\t{}\t{}",
            provider.id,
            base_url,
            provider.auth_source.as_str()
        );
    }
    Ok(())
}

fn cmd_add(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::AddArgs,
) -> Result<()> {
    let mut prompt = add_assist::DialoguerPrompter;
    let io = if args.base_url.is_some() && args.key.is_some() {
        add_assist::AddCommandIO::direct()
    } else {
        add_assist::AddCommandIO::from_stdio()?
    };

    cmd_add_with_prompt(config, opts, args, io, &mut prompt)
}

fn cmd_add_with_prompt(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::AddArgs,
    io: add_assist::AddCommandIO,
    prompt: &mut dyn add_assist::AddPrompter,
) -> Result<()> {
    util::validate_provider_id(&args.id)?;

    if config.provider_exists(&args.id) {
        anyhow::bail!(
            "provider '{}' already exists in config.toml (use `update` to modify it)",
            args.id
        );
    }

    let resolved = match (&args.base_url, &args.key) {
        (Some(base_url), Some(key)) => add_assist::ResolvedAddInputs {
            base_url: base_url.clone(),
            key: key.clone(),
            used_assisted_flow: false,
        },
        _ => add_assist::resolve_add_inputs(
            add_assist::AddAssistRequest {
                base_url: args.base_url.as_deref(),
                key: args.key.as_deref(),
                pasted_content: io.pasted_content(),
                interactive: io.interactive(),
            },
            prompt,
        )?,
    };

    util::validate_base_url(&resolved.base_url)?;
    if resolved.key.trim().is_empty() {
        anyhow::bail!("key cannot be empty");
    }

    config.add_or_update_provider_inline_token(&args.id, &resolved.base_url, &resolved.key)?;
    if !args.no_select {
        config.set_model_provider(&args.id)?;
    }

    finish_write_config(config, opts)
}

fn cmd_update(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::UpdateArgs,
) -> Result<()> {
    util::validate_provider_id(&args.id)?;
    if !config.provider_exists(&args.id) {
        anyhow::bail!("provider '{}' does not exist (use `add` to create it)", args.id);
    }

    if let Some(ref url) = args.base_url {
        util::validate_base_url(url)?;
    }

    let base_url = match args.base_url {
        Some(url) => url,
        None => config
            .get_provider_base_url(&args.id)?
            .ok_or_else(|| anyhow::anyhow!("provider '{}' has no base_url in config", args.id))?,
    };

    let keep_env = args.keep_env || opts.no_env;
    let mut env_keys_to_delete = Vec::new();

    if args.migrate_inline_token {
        ensure_env_deletion_confirmed(opts, keep_env, args.yes)?;
        let migration = resolve_legacy_migration(config, &args.id)?;
        config.add_or_update_provider_inline_token(&args.id, &base_url, &migration.token)?;
        if !keep_env {
            env_keys_to_delete.push(migration.env_key);
        }
    } else if let Some(key) = args.key {
        config.add_or_update_provider_inline_token(&args.id, &base_url, &key)?;
    } else {
        config.set_provider_base_url(&args.id, &base_url)?;
    }

    if args.select {
        config.set_model_provider(&args.id)?;
    }

    finish_write_config(config, opts)?;
    if !opts.dry_run && !env_keys_to_delete.is_empty() {
        delete_legacy_env_keys(&env_keys_to_delete)?;
    }
    Ok(())
}

fn cmd_delete(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::DeleteArgs,
) -> Result<()> {
    util::validate_provider_id(&args.id)?;
    if !config.provider_exists(&args.id) {
        anyhow::bail!("provider '{}' does not exist", args.id);
    }

    let auth = config.get_provider_auth_info(&args.id)?;
    let env_keys_to_delete: Vec<String> = if opts.no_env {
        Vec::new()
    } else {
        auth.env_key.into_iter().collect()
    };

    config.delete_provider(&args.id)?;
    finish_write_config(config, opts)?;
    if !opts.dry_run && !env_keys_to_delete.is_empty() {
        delete_legacy_env_keys(&env_keys_to_delete)?;
    }
    Ok(())
}

fn cmd_export(
    config: &codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::ExportArgs,
) -> Result<()> {
    let provider_ids = normalized_provider_ids(&args.providers, config.provider_ids());
    for id in &provider_ids {
        util::validate_provider_id(id)?;
        if !config.provider_exists(id) {
            anyhow::bail!("provider '{}' does not exist in config.toml", id);
        }
    }

    let default_provider = config.get_model_provider();
    let mut model_providers = BTreeMap::new();
    let mut missing = Vec::new();

    for id in provider_ids {
        let mut provider = config.get_provider_export(&id)?;
        provider.requires_openai_auth = false;

        if provider
            .experimental_bearer_token
            .as_ref()
            .map(|token| token.trim().is_empty())
            .unwrap_or(true)
        {
            match provider
                .env_key
                .as_deref()
                .and_then(read_legacy_secret)
            {
                Some(token) => provider.experimental_bearer_token = Some(token),
                None if provider.env_key.is_some() => {
                    provider.experimental_bearer_token = Some(String::new());
                    missing.push(id.clone());
                }
                None => {}
            }
        }

        provider.env_key = None;
        model_providers.insert(id, provider);
    }

    let bundle = bundle::Bundle {
        version: bundle::BUNDLE_VERSION_V2,
        default_provider,
        model_providers,
        secrets: BTreeMap::new(),
    };

    let rendered = bundle.render_pretty_toml()?;
    if opts.dry_run {
        print!("{rendered}");
        return Ok(());
    }

    bundle::write_file(&args.out, &rendered)
        .with_context(|| format!("failed to write export file at {}", args.out.display()))?;

    if !missing.is_empty() {
        eprintln!("warning: some provider tokens were not found (exported as empty):");
        for id in missing {
            eprintln!("  - {id}");
        }
    }
    eprintln!("warning: export file contains secrets in plaintext; handle it carefully");
    Ok(())
}

fn cmd_import(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::ImportArgs,
) -> Result<()> {
    if args.skip_existing && args.error_on_conflict {
        anyhow::bail!("--skip-existing and --error-on-conflict are mutually exclusive");
    }

    let bundle_text = std::fs::read_to_string(&args.input)
        .with_context(|| format!("failed to read import file at {}", args.input.display()))?;
    let bundle = bundle::Bundle::parse(&bundle_text)?;

    let before_default = config.get_model_provider();
    let mut imported_ids: Vec<String> = bundle.model_providers.keys().cloned().collect();
    imported_ids.sort();

    for id in &imported_ids {
        util::validate_provider_id(id)?;
    }

    if args.error_on_conflict {
        let conflicts: Vec<String> = imported_ids
            .iter()
            .filter(|id| config.provider_exists(id))
            .cloned()
            .collect();
        if !conflicts.is_empty() {
            anyhow::bail!("conflicting providers already exist: {}", conflicts.join(", "));
        }
    }

    let mut applied_ids = Vec::new();
    let mut warnings = Vec::new();

    for id in &imported_ids {
        if args.skip_existing && config.provider_exists(id) {
            continue;
        }

        let provider = bundle
            .model_providers
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("missing provider '{}' in bundle", id))?;
        util::validate_base_url(&provider.base_url)?;

        match resolve_bundle_token(id, provider, &bundle.secrets) {
            Some(token) => {
                config.add_or_update_provider_inline_token(id, &provider.base_url, &token)?;
            }
            None => {
                config.add_or_update_provider_without_auth(id, &provider.base_url)?;
                warnings.push(format!("provider '{}' imported without a token", id));
            }
        }

        applied_ids.push(id.clone());
    }

    let desired_default = if let Some(default_provider) = bundle.default_provider.clone() {
        if config.provider_exists(&default_provider) {
            Some(default_provider)
        } else {
            eprintln!(
                "warning: bundle default_provider '{}' not present after import; ignoring",
                default_provider
            );
            None
        }
    } else if imported_ids.len() == 1 {
        Some(imported_ids[0].clone())
    } else {
        None
    };

    if let Some(default_provider) = desired_default {
        config.set_model_provider(&default_provider)?;
    }

    let after_default = config.get_model_provider();
    if applied_ids.is_empty() && before_default == after_default {
        eprintln!("(no changes applied)");
        return Ok(());
    }

    finish_write_config(config, opts)?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

fn cmd_migrate_inline_token(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::MigrateInlineTokenArgs,
) -> Result<()> {
    let keep_env = args.keep_env || opts.no_env;
    ensure_env_deletion_confirmed(opts, keep_env, args.yes)?;

    let provider_ids = normalized_provider_ids(&args.ids, config.legacy_provider_ids());
    if provider_ids.is_empty() {
        eprintln!("(no legacy env_key providers found)");
        return Ok(());
    }

    let migrations: Vec<LegacyMigration> = provider_ids
        .iter()
        .map(|id| resolve_legacy_migration(config, id))
        .collect::<Result<Vec<_>>>()?;

    for migration in &migrations {
        config.migrate_provider_to_inline_token(&migration.id, &migration.token)?;
    }

    finish_write_config(config, opts)?;
    if !opts.dry_run && !keep_env {
        let env_keys: Vec<String> = migrations
            .into_iter()
            .map(|migration| migration.env_key)
            .collect();
        delete_legacy_env_keys(&env_keys)?;
    }

    Ok(())
}

fn finish_write_config(config: &mut codex_config::CodexConfig, opts: &GlobalOpts) -> Result<()> {
    let rendered = config.render();
    if opts.dry_run {
        print!("{rendered}");
        return Ok(());
    }

    config
        .write_with_backup()
        .with_context(|| format!("failed to write {}", config.path().display()))?;
    Ok(())
}

fn normalized_provider_ids(ids: &[String], default_ids: Vec<String>) -> Vec<String> {
    let mut provider_ids = if ids.is_empty() {
        default_ids
    } else {
        ids.to_vec()
    };
    provider_ids.sort();
    provider_ids.dedup();
    provider_ids
}

fn read_legacy_secret(env_key: &str) -> Option<String> {
    env_store::read_secret(env_key)
        .unwrap_or(None)
        .or_else(|| std::env::var(env_key).ok())
        .filter(|value| !value.trim().is_empty())
}

fn resolve_legacy_migration(
    config: &codex_config::CodexConfig,
    id: &str,
) -> Result<LegacyMigration> {
    util::validate_provider_id(id)?;
    if !config.provider_exists(id) {
        anyhow::bail!("provider '{}' does not exist", id);
    }

    let auth = config.get_provider_auth_info(id)?;
    if !auth.is_legacy_env() {
        anyhow::bail!("provider '{}' is not a legacy env_key provider", id);
    }

    let env_key = auth
        .env_key
        .ok_or_else(|| anyhow::anyhow!("provider '{}' is missing env_key", id))?;
    let token = read_legacy_secret(&env_key).ok_or_else(|| {
        anyhow::anyhow!("provider '{}' token not found for env_key {}", id, env_key)
    })?;

    Ok(LegacyMigration {
        id: id.to_string(),
        env_key,
        token,
    })
}

fn ensure_env_deletion_confirmed(opts: &GlobalOpts, keep_env: bool, yes: bool) -> Result<()> {
    if !opts.dry_run && !keep_env && !yes {
        anyhow::bail!("deleting legacy env vars requires --yes (or use --keep-env)");
    }
    Ok(())
}

fn delete_legacy_env_keys(env_keys: &[String]) -> Result<()> {
    let mut failures = Vec::new();
    for env_key in env_keys {
        if let Err(err) = env_store::delete_secret(env_key) {
            failures.push(format!("{env_key}: {err:#}"));
        }
    }

    if failures.is_empty() {
        if !env_keys.is_empty() {
            eprintln!("note: legacy env vars were deleted; open a new terminal if needed");
        }
        return Ok(());
    }

    anyhow::bail!(
        "config updated, but failed to delete legacy env vars:\n{}",
        failures.join("\n")
    )
}

fn resolve_bundle_token(
    id: &str,
    provider: &bundle::ProviderConfigExport,
    secrets: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(token) = provider
        .experimental_bearer_token
        .as_ref()
        .filter(|token| !token.trim().is_empty())
    {
        return Some(token.clone());
    }

    let generated_env_key = util::generate_env_key(id);
    secrets
        .get(&generated_env_key)
        .cloned()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            provider
                .env_key
                .as_ref()
                .filter(|env_key| *env_key != &generated_env_key)
                .and_then(|env_key| secrets.get(env_key))
                .cloned()
                .filter(|value| !value.trim().is_empty())
        })
}

fn resolve_probe_token(auth: &codex_config::ProviderAuthInfo) -> Option<String> {
    if let Some(token) = auth
        .env_key
        .as_deref()
        .filter(|env_key| !env_key.trim().is_empty())
        .and_then(read_legacy_secret)
    {
        return Some(token);
    }

    auth.experimental_bearer_token
        .as_ref()
        .filter(|token| !token.trim().is_empty())
        .cloned()
}

fn probe_single_provider(
    config: &codex_config::CodexConfig,
    id: &str,
    runner: &dyn probe::ProbeRunner,
) -> Result<probe::ProbeResult> {
    let Some(base_url) = config
        .get_provider_base_url(id)?
        .filter(|base_url| !base_url.trim().is_empty())
    else {
        return Ok(probe::ProbeResult::new(id, probe::ProbeOutcome::MissingBaseUrl));
    };

    let auth = config.get_provider_auth_info(id)?;
    let Some(token) = resolve_probe_token(&auth) else {
        return Ok(probe::ProbeResult::new(id, probe::ProbeOutcome::MissingToken));
    };

    Ok(runner.probe(id, &base_url, &token))
}

fn reordered_probe_ids(ordered_ids: &[String], winner_index: usize) -> Vec<String> {
    let mut reordered = Vec::with_capacity(ordered_ids.len());
    reordered.push(ordered_ids[winner_index].clone());
    reordered.extend(ordered_ids.iter().skip(winner_index + 1).cloned());
    reordered.extend(ordered_ids.iter().take(winner_index).cloned());
    reordered
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{ProbeOutcome, ProbeResult, ProbeRunner};
    use std::cell::RefCell;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    struct EnvVarGuard {
        key: String,
    }

    impl EnvVarGuard {
        fn set(label: &str, value: &str) -> Self {
            let key = format!("CODEX_PROVIDERS_TEST_{}_{}", label, std::process::id());
            unsafe {
                std::env::set_var(&key, value);
            }
            Self { key }
        }

        fn key(&self) -> &str {
            &self.key
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(&self.key);
            }
        }
    }

    fn opts(dry_run: bool) -> GlobalOpts {
        GlobalOpts {
            no_env: false,
            dry_run,
        }
    }

    fn load_config(path: &Path) -> codex_config::CodexConfig {
        codex_config::CodexConfig::load_or_default(path).expect("config should load")
    }

    fn write_config(path: &Path, contents: &str) {
        fs::write(path, contents).expect("config should write");
    }

    #[derive(Debug, Default)]
    struct FakeProbeRunner {
        calls: RefCell<Vec<String>>,
        results: BTreeMap<String, ProbeOutcome>,
    }

    impl FakeProbeRunner {
        fn with_results(results: [(&str, ProbeOutcome); 3]) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                results: results
                    .into_iter()
                    .map(|(id, outcome)| (id.to_string(), outcome))
                    .collect(),
            }
        }
    }

    impl ProbeRunner for FakeProbeRunner {
        fn probe(&self, id: &str, _base_url: &str, _token: &str) -> ProbeResult {
            self.calls.borrow_mut().push(id.to_string());
            let outcome = self
                .results
                .get(id)
                .cloned()
                .unwrap_or(ProbeOutcome::TransportError("unexpected".to_string()));
            ProbeResult::new(id, outcome)
        }
    }

    #[test]
    fn add_writes_inline_bearer_token() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let mut config = load_config(&config_path);

        cmd_add(
            &mut config,
            &opts(true),
            cli::AddArgs {
                id: "zapi".to_string(),
                base_url: Some("https://z.example/v1".to_string()),
                key: Some("sk-inline".to_string()),
                no_select: false,
            },
        )
        .expect("add should succeed");

        let rendered = config.render();
        assert!(rendered.contains("experimental_bearer_token = \"sk-inline\""));
        assert!(rendered.contains("requires_openai_auth = false"));
        assert!(!rendered.contains("env_key = "));
    }

    #[test]
    fn add_reads_piped_env_style_content() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let mut config = load_config(&config_path);

        let mut prompt = add_assist::FakePrompter::new(["https://env.example/v1", "sk-env"], true);
        let stdin = "OPENAI_BASE_URL=https://env.example/v1\nOPENAI_API_KEY=sk-env\n";

        cmd_add_with_prompt(
            &mut config,
            &opts(true),
            cli::AddArgs {
                id: "envpaste".to_string(),
                base_url: None,
                key: None,
                no_select: false,
            },
            add_assist::AddCommandIO::piped(stdin),
            &mut prompt,
        )
        .expect("add should succeed from piped content");

        let rendered = config.render();
        assert!(rendered.contains("model_provider = \"envpaste\""));
        assert!(rendered.contains("base_url = \"https://env.example/v1\""));
        assert!(rendered.contains("experimental_bearer_token = \"sk-env\""));
        assert!(!rendered.contains("env_key = "));
    }

    #[test]
    fn add_rejects_ambiguous_piped_content_without_prompting() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let mut config = load_config(&config_path);

        let mut prompt = add_assist::FakePrompter::default();
        let stdin = "OPENAI_BASE_URL=https://first.example/v1\nSECOND_BASE_URL=https://second.example/v1\nOPENAI_API_KEY=sk-env\n";

        let err = cmd_add_with_prompt(
            &mut config,
            &opts(true),
            cli::AddArgs {
                id: "ambiguous".to_string(),
                base_url: None,
                key: None,
                no_select: false,
            },
            add_assist::AddCommandIO::piped(stdin),
            &mut prompt,
        )
        .expect_err("ambiguous piped content should fail");

        assert!(err.to_string().contains("ambiguous"));
        assert_eq!(prompt.edit_calls.len(), 0);
        assert!(!config.render().contains("experimental_bearer_token"));
    }

    #[test]
    fn add_reads_piped_curl_content_and_normalizes_endpoint() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let mut config = load_config(&config_path);

        let mut prompt = add_assist::FakePrompter::default();
        let stdin =
            "curl https://curl.example.com/v1/chat/completions -H 'Authorization: Bearer sk-curl'\n";

        cmd_add_with_prompt(
            &mut config,
            &opts(true),
            cli::AddArgs {
                id: "curlpaste".to_string(),
                base_url: None,
                key: None,
                no_select: true,
            },
            add_assist::AddCommandIO::piped(stdin),
            &mut prompt,
        )
        .expect("add should succeed from piped curl content");

        let rendered = config.render();
        assert!(rendered.contains("base_url = \"https://curl.example.com/v1\""));
        assert!(rendered.contains("experimental_bearer_token = \"sk-curl\""));
        assert_eq!(prompt.edit_calls.len(), 0);
    }

    #[test]
    fn update_migrate_inline_token_reads_legacy_env() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let guard = EnvVarGuard::set("MIGRATE_ONE", "sk-migrated");
        write_config(
            &config_path,
            &format!(
                r#"
model_provider = "legacy"

[model_providers.legacy]
name = "OpenAI"
base_url = "https://legacy.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "{}"
"#,
                guard.key()
            ),
        );
        let mut config = load_config(&config_path);

        cmd_update(
            &mut config,
            &opts(true),
            cli::UpdateArgs {
                id: "legacy".to_string(),
                base_url: None,
                key: None,
                select: false,
                migrate_inline_token: true,
                keep_env: false,
                yes: false,
            },
        )
        .expect("migration update should succeed");

        let rendered = config.render();
        assert!(rendered.contains("base_url = \"https://legacy.example/v1\""));
        assert!(rendered.contains("experimental_bearer_token = \"sk-migrated\""));
        assert!(rendered.contains("requires_openai_auth = false"));
        assert!(!rendered.contains("env_key = "));
    }

    #[test]
    fn migrate_inline_token_batch_is_atomic_on_missing_secret() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let existing = EnvVarGuard::set("MIGRATE_BATCH_OK", "sk-present");
        let missing_key = format!(
            "CODEX_PROVIDERS_TEST_MIGRATE_BATCH_MISSING_{}",
            std::process::id()
        );
        write_config(
            &config_path,
            &format!(
                r#"
[model_providers.good]
name = "OpenAI"
base_url = "https://good.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "{}"

[model_providers.bad]
name = "OpenAI"
base_url = "https://bad.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "{}"
"#,
                existing.key(),
                missing_key
            ),
        );
        let mut config = load_config(&config_path);

        let err = cmd_migrate_inline_token(
            &mut config,
            &opts(true),
            cli::MigrateInlineTokenArgs {
                ids: Vec::new(),
                keep_env: false,
                yes: false,
            },
        )
        .expect_err("batch migrate should fail when any secret is missing");

        assert!(err.to_string().contains("bad"));
        let rendered = config.render();
        assert!(!rendered.contains("experimental_bearer_token = \"sk-present\""));
        assert!(rendered.contains(existing.key()));
    }

    #[test]
    fn migrate_inline_token_requires_yes_before_env_deletion() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let guard = EnvVarGuard::set("MIGRATE_CONFIRM", "sk-confirm");
        write_config(
            &config_path,
            &format!(
                r#"
[model_providers.legacy]
name = "OpenAI"
base_url = "https://legacy.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "{}"
"#,
                guard.key()
            ),
        );
        let mut config = load_config(&config_path);

        let err = cmd_migrate_inline_token(
            &mut config,
            &opts(false),
            cli::MigrateInlineTokenArgs {
                ids: vec!["legacy".to_string()],
                keep_env: false,
                yes: false,
            },
        )
        .expect_err("migrate should require --yes before deleting env vars");

        assert!(err.to_string().contains("--yes"));
        assert!(!config.render().contains("experimental_bearer_token"));
    }

    #[test]
    fn import_v1_bundle_normalizes_to_inline_token() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let bundle_path: PathBuf = dir.path().join("bundle.toml");
        fs::write(
            &bundle_path,
            r#"
version = 1
default_provider = "legacy"

[model_providers.legacy]
name = "OpenAI"
base_url = "https://legacy.example/v1"
wire_api = "responses"
requires_openai_auth = true
env_key = "CODEX_LEGACY_KEY"

[secrets]
CODEX_LEGACY_KEY = "sk-imported"
"#,
        )
        .expect("bundle should write");
        let mut config = load_config(&config_path);

        cmd_import(
            &mut config,
            &opts(true),
            cli::ImportArgs {
                input: bundle_path,
                skip_existing: false,
                error_on_conflict: false,
            },
        )
        .expect("import should succeed");

        let rendered = config.render();
        assert!(rendered.contains("experimental_bearer_token = \"sk-imported\""));
        assert!(rendered.contains("requires_openai_auth = false"));
        assert!(!rendered.contains("env_key = "));
    }

    #[test]
    fn probe_select_reorders_winner_untested_failed() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        write_config(
            &config_path,
            r#"
model_provider = "failfirst"

[model_providers.failfirst]
name = "OpenAI"
base_url = "https://fail.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-fail"

[model_providers.winner]
name = "OpenAI"
base_url = "https://winner.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-win"

[model_providers.untested]
name = "OpenAI"
base_url = "https://untested.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-later"
"#,
        );
        let mut config = load_config(&config_path);
        let runner = FakeProbeRunner::with_results([
            ("failfirst", ProbeOutcome::HttpStatus(503)),
            ("winner", ProbeOutcome::Success(200)),
            ("untested", ProbeOutcome::Success(200)),
        ]);

        cmd_probe_select_with_runner(&mut config, &opts(true), &runner)
            .expect("probe-select should succeed");

        assert_eq!(
            runner.calls.borrow().as_slice(),
            &["failfirst".to_string(), "winner".to_string()]
        );
        assert_eq!(config.get_model_provider().as_deref(), Some("winner"));
        assert_eq!(
            config.provider_ids_in_order(),
            vec![
                "winner".to_string(),
                "untested".to_string(),
                "failfirst".to_string()
            ]
        );
    }

    #[test]
    fn probe_select_uses_config_order_and_leaves_config_unchanged_on_total_failure() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        write_config(
            &config_path,
            r#"
model_provider = "zeta"

[model_providers.zeta]
name = "OpenAI"
base_url = "https://zeta.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-zeta"

[model_providers.alpha]
name = "OpenAI"
base_url = "https://alpha.example/v1"
wire_api = "responses"
requires_openai_auth = false
experimental_bearer_token = "sk-alpha"
"#,
        );
        let mut config = load_config(&config_path);
        let before = config.render();
        let runner = FakeProbeRunner {
            calls: RefCell::new(Vec::new()),
            results: [
                ("zeta".to_string(), ProbeOutcome::TransportError("timeout".to_string())),
                ("alpha".to_string(), ProbeOutcome::HttpStatus(401)),
            ]
            .into_iter()
            .collect(),
        };

        let err = cmd_probe_select_with_runner(&mut config, &opts(true), &runner)
            .expect_err("all-failure probe-select should error");

        assert!(err.to_string().contains("no available provider"));
        assert_eq!(
            runner.calls.borrow().as_slice(),
            &["zeta".to_string(), "alpha".to_string()]
        );
        assert_eq!(config.render(), before);
    }
}
