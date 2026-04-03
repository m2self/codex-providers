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
- sync `[model_providers]` across machines over OpenSSH `sftp`

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
- `benchmark`
- `probe-select`
- `benchmark-select`
- `add`
- `update`
- `delete`
- `export`
- `import`
- `migrate-inline-token`
- `ssh-sync`

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

`add <id>` also accepts a Cherry Studio deep link and extracts `baseUrl` + `apiKey`:

```powershell
"cherrystudio://providers/api-keys?v=1&data=..." | cargo run -- add zapi
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

Benchmark every configured provider with the top-level `model` and print the
performance ranking without changing config:

```powershell
cargo run -- benchmark
```

Benchmark every configured provider with the top-level `model`, reorder
providers by performance, and select the recommended default provider:

```powershell
cargo run -- benchmark-select
```

Use three rounds instead of the default two:

```powershell
cargo run -- benchmark-select --rounds 3
```

Export providers:

```powershell
cargo run -- export --out .\providers.toml
```

Import providers:

```powershell
cargo run -- import --in .\providers.toml
```

Sync providers across this machine and every machine in
`~/.codex-providers/sync.toml`:

```powershell
cargo run -- ssh-sync
```

Sync only one configured machine:

```powershell
cargo run -- ssh-sync office-win
```

Example `~/.codex-providers/sync.toml`:

```toml
machines = ["work-linux", "office-win"]
```

## Notes

- New writes use `experimental_bearer_token` instead of `env_key`.
- `export` writes plaintext secrets. Treat exported bundles as sensitive files.
- `probe-select` considers a provider available only if a real
  `POST {base_url}/chat/completions` test succeeds with the top-level
  `model` from your local `config.toml`.
- `benchmark` uses the same provider benchmark as `benchmark-select`, but it is
  read-only: it prints sorted performance results and does not change
  `config.toml`.
- `benchmark-select` benchmarks providers with the same top-level `model`,
  reorders `[model_providers]` by the benchmark result, and updates
  `model_provider` to the recommended provider. It does not change the top-level
  `model`.
- `ssh-sync` only syncs `[model_providers]`. It does not change each machine's
  `model_provider`.
- `ssh-sync` only supports providers with inline bearer tokens. Migrate legacy
  `env_key` providers before syncing.
- `ssh-sync` depends on the local OpenSSH `sftp` client. Remote read/write
  stays on SFTP rather than remote shell scripts.
- `ssh-sync` resolves machine aliases from `~/.ssh/config`. In `sync.toml` you
  only list machine names; connection details belong in SSH config.
- `ssh-sync` follows your system OpenSSH defaults for `HostName`, `User`,
  `Port`, `IdentityFile`, and `ProxyJump`, and it passes
  `StrictHostKeyChecking=accept-new` by default.
- Unknown hosts are accepted and written to your default `known_hosts`; host
  key mismatches still fail and must be fixed manually.
- Remote Codex config is always assumed to live under the remote login home at
  `.codex/config.toml`.
- Old `sync.conf` is not supported anymore. Migrate to `sync.toml`.
- `Cargo.lock` is intentionally ignored in this repo and not meant to be
  tracked.

## Verification

Run the test suite:

```powershell
cargo test
```

## CI And Releases

- GitHub Actions builds Windows and Linux artifacts on normal CI runs.
- Pushing a Git tag triggers the same builds and then creates a GitHub Release
  for that tag with both binaries attached.
