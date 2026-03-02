use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "codex-providers")]
#[command(about = "Manage Codex model providers in ~/.codex/config.toml", long_about = None)]
pub struct Cli {
    /// Path to Codex config.toml (defaults to ~/.codex/config.toml)
    #[arg(long = "config-path", value_name = "PATH")]
    pub config_path: Option<PathBuf>,

    /// Only edit config.toml; do not persist/delete env vars
    #[arg(long = "no-env")]
    pub no_env: bool,

    /// Print resulting config/bundle to stdout; do not write files or env vars
    #[arg(long = "dry-run")]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// List providers in config.toml
    List,
    /// Add a provider
    Add(AddArgs),
    /// Update a provider
    Update(UpdateArgs),
    /// Delete a provider
    Delete(DeleteArgs),
    /// Export providers + secrets to a portable file
    Export(ExportArgs),
    /// Import providers + secrets from a portable file
    Import(ImportArgs),
}

#[derive(Args, Debug)]
pub struct AddArgs {
    /// Provider id (table name): [model_providers.<id>]
    pub id: String,

    /// Provider base_url (e.g. https://example.com/v1)
    #[arg(long = "base-url", value_name = "URL")]
    pub base_url: String,

    /// API key to persist as env var
    #[arg(long = "key", value_name = "KEY")]
    pub key: String,

    /// Do not set config.toml model_provider to the new provider
    #[arg(long = "no-select")]
    pub no_select: bool,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Provider id (table name): [model_providers.<id>]
    pub id: String,

    /// New base_url (e.g. https://example.com/v1)
    #[arg(long = "base-url", value_name = "URL")]
    pub base_url: Option<String>,

    /// New API key to persist as env var
    #[arg(long = "key", value_name = "KEY")]
    pub key: Option<String>,

    /// Set config.toml model_provider to this provider
    #[arg(long = "select")]
    pub select: bool,
}

#[derive(Args, Debug)]
pub struct DeleteArgs {
    /// Provider id (table name): [model_providers.<id>]
    pub id: String,
}

#[derive(Args, Debug)]
pub struct ExportArgs {
    /// Output file path
    #[arg(long = "out", value_name = "FILE")]
    pub out: PathBuf,

    /// Providers to export (comma-separated). Defaults to all.
    #[arg(long = "providers", value_delimiter = ',', value_name = "ID")]
    pub providers: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    /// Input file path
    #[arg(long = "in", value_name = "FILE")]
    pub input: PathBuf,

    /// If provider exists, keep local version and skip importing it
    #[arg(long = "skip-existing")]
    pub skip_existing: bool,

    /// If provider exists, error and exit without applying changes
    #[arg(long = "error-on-conflict")]
    pub error_on_conflict: bool,
}

