mod bundle;
mod cli;
mod codex_config;
mod env_store;
mod util;

use anyhow::{Context as _, Result};
use clap::Parser;
use std::collections::{BTreeMap, BTreeSet};

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
    let mut config =
        codex_config::CodexConfig::load_or_default(&config_path).with_context(|| {
            format!("failed to load codex config at {}", config_path.display())
        })?;

    match cli.command {
        cli::Command::List => cmd_list(&config),
        cli::Command::Add(args) => cmd_add(&mut config, &opts, args),
        cli::Command::Update(args) => cmd_update(&mut config, &opts, args),
        cli::Command::Delete(args) => cmd_delete(&mut config, &opts, args),
        cli::Command::Export(args) => cmd_export(&config, &opts, args),
        cli::Command::Import(args) => cmd_import(&mut config, &opts, args),
    }
}

#[derive(Debug, Clone, Copy)]
struct GlobalOpts {
    no_env: bool,
    dry_run: bool,
}

fn cmd_list(config: &codex_config::CodexConfig) -> Result<()> {
    let providers = config.list_providers();
    if providers.is_empty() {
        println!("(no providers found)");
        return Ok(());
    }

    for p in providers {
        let base_url = p.base_url.unwrap_or_else(|| "<missing>".to_string());
        let env_key = p.env_key.unwrap_or_else(|| "<missing>".to_string());
        println!("{}\t{}\t{}", p.id, base_url, env_key);
    }
    Ok(())
}

fn cmd_add(
    config: &mut codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::AddArgs,
) -> Result<()> {
    util::validate_provider_id(&args.id)?;
    util::validate_base_url(&args.base_url)?;

    let env_key = util::generate_env_key(&args.id);

    if config.provider_exists(&args.id) {
        anyhow::bail!(
            "provider '{}' already exists in config.toml (use `update` to modify it)",
            args.id
        );
    }

    config.add_or_update_provider(&args.id, &args.base_url, &env_key)?;
    if !args.no_select {
        config.set_model_provider(&args.id)?;
    }

    if !opts.no_env && !opts.dry_run {
        env_store::set_secret(&env_key, &args.key)
            .with_context(|| format!("failed to persist env var {env_key}"))?;
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

    let env_key = util::generate_env_key(&args.id);

    let base_url = match args.base_url {
        Some(url) => url,
        None => config
            .get_provider_base_url(&args.id)?
            .ok_or_else(|| anyhow::anyhow!("provider '{}' has no base_url in config", args.id))?,
    };

    config.add_or_update_provider(&args.id, &base_url, &env_key)?;
    if args.select {
        config.set_model_provider(&args.id)?;
    }

    if let Some(key) = args.key {
        if !opts.no_env && !opts.dry_run {
            env_store::set_secret(&env_key, &key)
                .with_context(|| format!("failed to persist env var {env_key}"))?;
        }
    }

    finish_write_config(config, opts)
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

    let env_key = config
        .get_provider_env_key(&args.id)?
        .unwrap_or_else(|| util::generate_env_key(&args.id));

    config.delete_provider(&args.id)?;

    if !opts.no_env && !opts.dry_run {
        env_store::delete_secret(&env_key)
            .with_context(|| format!("failed to delete env var {env_key}"))?;
    }

    finish_write_config(config, opts)
}

fn cmd_export(
    config: &codex_config::CodexConfig,
    opts: &GlobalOpts,
    args: cli::ExportArgs,
) -> Result<()> {
    let mut provider_ids: Vec<String> = if args.providers.is_empty() {
        config.provider_ids()
    } else {
        args.providers.clone()
    };
    provider_ids.sort();
    provider_ids.dedup();
    for id in &provider_ids {
        util::validate_provider_id(id)?;
        if !config.provider_exists(id) {
            anyhow::bail!("provider '{}' does not exist in config.toml", id);
        }
    }

    let default_provider = config.get_model_provider();

    let mut model_providers = BTreeMap::new();
    let mut secrets = BTreeMap::new();
    let mut missing = BTreeSet::new();

    for id in provider_ids {
        let p = config.get_provider_export(&id)?;
        let env_key = p.env_key.clone();

        model_providers.insert(id, p);

        let secret = match env_store::read_secret(&env_key).unwrap_or(None) {
            Some(v) => Some(v),
            None => std::env::var(&env_key).ok(),
        };

        match secret {
            Some(v) if !v.is_empty() => {
                secrets.insert(env_key, v);
            }
            _ => {
                secrets.insert(env_key.clone(), String::new());
                missing.insert(env_key);
            }
        }
    }

    let bundle = bundle::BundleV1 {
        version: 1,
        default_provider,
        model_providers,
        secrets,
    };

    let rendered = bundle.render_pretty_toml()?;
    if opts.dry_run {
        print!("{rendered}");
        return Ok(());
    }

    bundle::write_file(&args.out, &rendered)
        .with_context(|| format!("failed to write export file at {}", args.out.display()))?;

    if !missing.is_empty() {
        eprintln!("warning: some secrets were not found (exported as empty):");
        for k in missing {
            eprintln!("  - {k}");
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
    let bundle = bundle::BundleV1::parse(&bundle_text)?;

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

    for id in &imported_ids {
        if args.skip_existing && config.provider_exists(id) {
            continue;
        }

        let p = bundle
            .model_providers
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("missing provider '{}' in bundle", id))?;
        util::validate_base_url(&p.base_url)?;

        let env_key_generated = util::generate_env_key(id);
        config.add_or_update_provider(id, &p.base_url, &env_key_generated)?;
        applied_ids.push(id.clone());

        if opts.no_env || opts.dry_run {
            continue;
        }

        let mut secret = bundle.secrets.get(&env_key_generated).cloned();
        if secret.is_none() && p.env_key != env_key_generated {
            secret = bundle.secrets.get(&p.env_key).cloned();
        }

        match secret {
            Some(v) if !v.is_empty() => {
                env_store::set_secret(&env_key_generated, &v)
                    .with_context(|| format!("failed to persist env var {env_key_generated}"))?;
            }
            Some(_) => {
                eprintln!("warning: secret for {env_key_generated} is empty; skipping");
            }
            None => {
                eprintln!("warning: secret for {env_key_generated} not found in bundle; skipping");
            }
        }
    }

    let desired_default = if let Some(dp) = bundle.default_provider.clone() {
        if config.provider_exists(&dp) {
            Some(dp)
        } else {
            eprintln!(
                "warning: bundle default_provider '{}' not present after import; ignoring",
                dp
            );
            None
        }
    } else if imported_ids.len() == 1 {
        Some(imported_ids[0].clone())
    } else {
        None
    };

    if let Some(dp) = desired_default {
        config.set_model_provider(&dp)?;
    }

    let after_default = config.get_model_provider();

    if applied_ids.is_empty() && before_default == after_default {
        eprintln!("(no changes applied)");
        return Ok(());
    }

    finish_write_config(config, opts)?;

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

    if !opts.no_env {
        eprintln!("note: persistent env changes may require opening a new terminal to take effect");
    }
    Ok(())
}
