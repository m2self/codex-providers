# codex-providers

`codex-providers` is a small Rust CLI for managing Codex model providers in
`~/.codex/config.toml`.

It is built around the config shape used by Codex model providers and focuses on
practical local workflows:

- list configured providers
- add providers with inline bearer tokens
- paste provider snippets into `add <id>` and extract `base_url` and token
- update provider URLs and tokens
- migrate legacy `env_key` providers to inline bearer tokens
- export and import portable provider bundles
- probe providers in order, select the first available one, and reorder config

## Requirements

- Rust toolchain with `cargo`
- Access to the Codex config file you want to manage

By default the CLI edits `~/.codex/config.toml`. You can override that with
`--config-path`.

## Build And Run

Run directly from the repo:

```powershell
cargo run -- --help
```

Build a release binary:

```powershell
cargo build --release
```

## Commands

Top-level commands:

- `list`
- `probe-select`
- `add`
- `update`
- `delete`
- `export`
- `import`
- `migrate-inline-token`

Global options:

- `--config-path <PATH>`
- `--no-env`
- `--dry-run`

## Common Workflows

Add a provider directly:

```powershell
cargo run -- add zapi --base-url https://example.com/v1 --key sk-example
```

Add a provider by pasting content into stdin:

```powershell
@'
OPENAI_BASE_URL=https://example.com/v1
OPENAI_API_KEY=sk-example
'@ | cargo run -- add zapi
```

Update an existing provider:

```powershell
cargo run -- update zapi --base-url https://new.example.com/v1 --key sk-new
```

Migrate all legacy `env_key` providers to inline bearer tokens:

```powershell
cargo run -- migrate-inline-token --yes
```

Migrate a single provider but keep the old environment variable:

```powershell
cargo run -- update zapi --migrate-inline-token --keep-env
```

Probe providers in config order, select the first working one, and reorder the
provider table for the next run:

```powershell
cargo run -- probe-select
```

Export providers:

```powershell
cargo run -- export --out .\providers.toml
```

Import providers:

```powershell
cargo run -- import --in .\providers.toml
```

## Notes

- New writes use `experimental_bearer_token` instead of `env_key`.
- `export` writes plaintext secrets. Treat exported bundles as sensitive files.
- `probe-select` considers a provider available only if
  `GET {base_url}/models` returns `2xx` with bearer auth.
- `Cargo.lock` is intentionally ignored in this repo and not meant to be
  tracked.

## Verification

Run the test suite:

```powershell
cargo test
```
