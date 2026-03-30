# Design Document: Multi-Account Support

**Author:** Scott Idler
**Date:** 2026-03-29
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Add support for multiple Gmail accounts in a single eratosthenes invocation. Each account gets its own config file, OAuth credentials, and rate limiter. Accounts execute in parallel via `tokio::JoinSet` since Gmail rate limits are per-account. Console and log output are prefixed with the account name for disambiguation.

## Problem Statement

### Background

Eratosthenes currently manages a single Gmail account (scott.idler@tatari.tv) with one config file, one set of OAuth credentials, and one rate limiter. The tool runs as a one-shot CLI or via a systemd timer.

### Problem

The user has two Gmail accounts - work (scott.idler@tatari.tv) and home (scott.a.idler@gmail.com) - each needing independent inbox zero processing with different filter rules. Today this requires running two separate invocations with separate configs, managing two systemd timers, and dealing with log file collisions.

### Goals

- Support N config files in a config directory, one per Gmail account
- Each account has independent OAuth credentials and token caches
- Accounts execute in parallel within a single `eratosthenes run` invocation
- Console output is prefixed with the account name (e.g., `[work]`, `[home]`)
- Log output includes account context for filtering
- `--config` continues to work for single-account runs (backwards compatible)
- All subcommands accept optional positional account names to target a subset
- Single systemd timer handles all accounts

### Non-Goals

- Shared filter rules across accounts (each config is fully independent)
- Cross-account operations (move email from one account to another)
- Switching to `tracing` for per-file log bifurcation (future enhancement)
- Account-level scheduling (different intervals per account)
- Google Workspace service account auth (stays with user OAuth)

## Proposed Solution

### Overview

Config files are stored one-per-account in `~/.config/eratosthenes/`. Discovery globs all `*.yml` files in that directory and validates each has an `auth:` section (confirming it's an eratosthenes config, not some stray yml). Account name = filename stem. `eratosthenes run` discovers all accounts and processes them in parallel by default. Optional positional args after each subcommand filter to a subset (e.g., `eratosthenes run home`). Each account gets its own authenticator, `GmailClient`, `RateLimiter`, and `LabelResolver` - zero shared mutable state between accounts.

### Architecture

**Config directory layout:**

```
~/.config/eratosthenes/
  work.yml                          # account config
  home.yml                          # account config
  work/
    client-secret.json              # Google Cloud OAuth app secret
    tokencache.json                 # cached OAuth tokens
  home/
    client-secret.json              # separate OAuth app (different GCP project)
    tokencache.json                 # separate token cache
```

Each config file is identical in schema to the existing `eratosthenes.yml` - no structural changes. The only difference is the auth paths point to per-account subdirectories:

```yaml
# work.yml
auth:
  client-secret-path: "~/.config/eratosthenes/work/client-secret.json"
  token-cache-path: "~/.config/eratosthenes/work/tokencache.json"
  callback-port: 13131

message-filters:
  - only-me-star:
      to: ['scott.idler@tatari.tv']
      cc: []
      from: '*@tatari.tv'
      label: INBOX
      action: Star
  # ... work-specific filters

state-filters:
  # ... work-specific state filters
```

```yaml
# home.yml
auth:
  client-secret-path: "~/.config/eratosthenes/home/client-secret.json"
  token-cache-path: "~/.config/eratosthenes/home/tokencache.json"
  callback-port: 13132

message-filters:
  # ... home-specific filters

state-filters:
  # ... home-specific state filters
```

**Callback ports must differ.** Each account needs a unique localhost port for the OAuth redirect flow, since both might be authenticated in the same session. Default ports: 13131, 13132, 13133, etc.

**Client secret can be shared.** A single Google Cloud OAuth app (client-secret.json) can authenticate any Google account - the client secret identifies the *app*, not the *user*. You can use the same client-secret.json for both accounts if you prefer, or use separate GCP projects. The token caches MUST be separate since they hold per-user refresh tokens.

### Execution Model

```
main()
  |
  +-- discover configs: glob ~/.config/eratosthenes/*.yml
  |     -> [("work", Config), ("home", Config)]
  |
  +-- setup_logging() once (single global logger)
  |
  +-- for each (account, config):
  |     tokio::JoinSet::spawn(run_account(account, config, dry_run))
  |
  +-- join all tasks
  |     -> collect per-account Results
  |
  +-- report: "[work] Done: 5 matched, 3 transitioned"
  |            "[home] Done: 12 matched, 0 transitioned"
  |            (or "[work] FAILED: OAuth token expired")
```

Each `run_account` task independently:
1. Builds an authenticator from the account's `AuthConfig`
2. Creates a `Gmail` hub (separate TLS connector)
3. Creates a `GmailClient` (separate `RateLimiter`, separate `LabelResolver`)
4. Calls `engine::execute()` with the account's config

Since `GmailClient::new()` creates its own `RateLimiter`, and each task creates its own `GmailClient`, rate limiting is naturally per-account with zero shared mutable state between tasks.

**Thread safety:** All types in the per-account path are `Send` - `GmailClient` contains a hyper-based `Hub` (Send), `RateLimiter` (AtomicU32 + Mutex), and `LabelResolver` (HashMaps). Each tokio task has exclusive ownership of its own client, so no `Arc`/`Mutex` wrapping is needed.

**OAuth during `run`:** If a cached token expires during execution, `yup_oauth2` tries to refresh it silently using the refresh token. If that fails (revoked refresh token), the task errors out rather than trying a browser flow. Interactive browser auth should only happen via `eratosthenes auth login --account <name>`, which is always single-account.

### Data Model

No changes to `Config`, `AuthConfig`, `MessageFilter`, `StateFilter`, or any cfg structs.

New types:

```rust
/// An account is a named config - name derived from the config filename stem.
pub struct Account {
    pub name: String,
    pub config: Config,
}
```

CLI changes - positional account args on each subcommand:

```rust
#[derive(Subcommand)]
pub enum Command {
    /// Run the inbox zero engine (default when no subcommand given)
    Run {
        /// Account(s) to run (default: all discovered)
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
    Auth(AuthOpts),
    Service(ServiceOpts),
    Config(ConfigOpts),
}

#[derive(Subcommand)]
pub enum AuthCommand {
    Login {
        /// Account to login (required when multiple exist)
        account: Option<String>,
    },
    Logout {
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
    Status {
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    Validate {
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
    Show {
        #[arg(num_args = 0..)]
        accounts: Vec<String>,
    },
}
```

Validation: after parsing, each `accounts: Vec<String>` is checked against the discovered account names. Unknown names produce a clear error:

```
error: unknown account 'bogus', available accounts: [home, work]
```

### CLI Changes

```
eratosthenes [OPTIONS] [COMMAND]

Options:
  -c, --config <PATH>       Run a single account from this config file (bypass discovery)
  -l, --log-level <LEVEL>   Log level
      --dry-run              Dry run mode

Commands:
  run      [ACCOUNTS]...  Run the inbox zero engine (default)
  auth     <COMMAND>      Manage OAuth2 authentication
  service  <COMMAND>      Manage systemd timer service
  config   <COMMAND>      Config utilities
```

**Discovery:** Glob all `*.yml` files in the XDG config dir (`~/.config/eratosthenes/`). Each file is validated as an eratosthenes config (must have an `auth:` section with `client-secret-path` and `token-cache-path`). Files that fail validation are skipped with a warning. Account name = filename stem.

**Account selection via positional args:**

Every subcommand that operates on accounts takes optional positional args. Empty = all discovered. Any names provided are validated against the discovered set.

| CLI invocation | Behavior |
|---------------|----------|
| `eratosthenes run` | Discover all accounts, run all in parallel |
| `eratosthenes run work` | Run only work |
| `eratosthenes run work home` | Run work and home (explicit) |
| `eratosthenes run bogus` | Error: unknown account 'bogus', available: [home, work] |
| `eratosthenes run --config path.yml` | Single file mode (bypass discovery) |
| `eratosthenes auth status` | Show status for all accounts |
| `eratosthenes auth status work` | Show status for work only |
| `eratosthenes auth login home` | Login for home only |

**Dynamic help (otto-rs pattern):** At `--help` time, discover accounts and list them:

```
$ eratosthenes run --help
Run the inbox zero engine

Usage: eratosthenes run [ACCOUNTS]...

Discovered accounts: home, work

Arguments:
  [ACCOUNTS]...  Account(s) to run [default: all discovered]

Options:
      --dry-run  Dry run mode
```

The discovered account list is populated dynamically from the config directory scan, similar to how otto-rs builds its help from discovered task names.

**Auth login special case:** `auth login` takes a single optional positional (not variadic). If omitted and multiple accounts exist, print the list and require the user to pick one. All other commands default to all.

### Console Output

All console output is prefixed with `[account]` when running in multi-account mode:

```
[work] Connecting to Gmail...
[home] Connecting to Gmail...
[work] === Phase 0: Stage Sanitization ===
[home] === Phase 0: Stage Sanitization ===
[work] [filter:only-me-star] searching: to:scott.idler@tatari.tv ...
[home] [filter:newsletters] searching: label:inbox ...
[work] Done: 5 messages matched filters, 3 threads transitioned
[home] Done: 12 messages matched filters, 0 threads transitioned
```

In single-account mode (`--config`), no prefix is added (backwards compatible).

**Implementation:** Thread account name through the call chain:

```rust
// lib.rs
pub async fn run(account_name: &str, config: &Config, dry_run: bool) -> Result<()>

// engine.rs
pub async fn execute(client: &mut GmailClient, config: &Config, account: &str, dry_run: bool) -> Result<()>
```

In single-account mode (`--config`), pass the filename stem. In multi-account mode, pass each account's name. All `println!()` and `info!()` calls gain an `[account]` prefix:

```rust
println!("[{}] [filter:{}] searching: {}", account, filter.name, query);
info!("[{}] execute: dry_run={}", account, dry_run);
```

### Logging

**Single file with account prefix:**

Log file remains `~/.local/share/eratosthenes/logs/eratosthenes.log`. All messages include `[account]` context:

```
[2026-03-29 10:00:00 INFO eratosthenes] [work] execute: dry_run=false, message_filters=4, state_filters=4
[2026-03-29 10:00:00 INFO eratosthenes] [home] execute: dry_run=false, message_filters=2, state_filters=3
```

Filtering by account: `grep '\[work\]' eratosthenes.log`

**Why not per-file:** `env_logger` supports only one global logger. True per-file bifurcation requires switching to `tracing` with per-subscriber file writers, which is a larger refactor. The prefixed single-file approach works now and is easy to grep. Per-file can be added later if needed.

### Systemd Service Changes

**Current:** `ExecStart={binary} run --config {config_path}`

**New:** `ExecStart={binary} run` (no `--config`, discovers all accounts)

The `service install` command no longer requires `--config` on the CLI:

```rust
fn generate_service(binary: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Eratosthenes Gmail Inbox Zero Engine\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={binary} run\n\
         Environment=PATH={cargo_bin}:/usr/local/bin:/usr/bin:/bin\n",
        binary = binary.display(),
        cargo_bin = cargo_bin_dir(),
    )
}
```

Single timer fires, single service runs, engine discovers and processes all accounts in parallel.

**Migration from old service:** Users with an existing service that has `--config eratosthenes.yml` baked in should run `eratosthenes service reinstall` after migrating configs. The new service unit drops the `--config` flag entirely, relying on directory discovery.

`service install` validation:
- Scan config dir for `.yml` files
- Warn if none found
- For each config, check if token cache exists (same warning as today)
- Print discovered accounts: `Found accounts: work, home`

### Implementation Plan

**Phase 1: Config discovery and Account type**
- Add `Account` struct (name + config)
- Add `discover_accounts()` function: globs `~/.config/eratosthenes/*.yml`, attempts to parse each as a `Config`, skips (with warning) any that lack an `auth:` section or fail to parse
- `--config` wraps into a single-element `Vec<Account>` with name from stem
- Existing `resolve_config_path` becomes a fallback for single-config discovery
- **Startup validation:** Before spawning tasks, validate all discovered accounts:
  - No duplicate callback ports across accounts (fail with clear error listing the conflict)
  - No duplicate account names (shouldn't happen with filesystem stems, but guard against it)
- Add positional `accounts: Vec<String>` to `Run` and other subcommand variants
- Add `validate_accounts()` helper: check positional args against discovered names, error on unknown

**Phase 2: Parallel execution**
- Refactor `lib.rs::run()` to accept account name
- Add account prefix to all `println!()` and `log` calls in engine.rs
- Add `run_account()` that wraps auth + client + engine for one account
- In `main.rs`, spawn each account into a `tokio::JoinSet`
- Apply positional account filter before spawning (if empty, run all)
- Collect results, report per-account success/failure

**Phase 3: All commands operate on all/filtered accounts**
- `auth status`, `auth logout`, `config validate`, `config show` - default to all accounts, respect positional account args
- `auth login` - requires an account name when multiple accounts exist (interactive browser flow is single-account)
- Dynamic `--help` text lists discovered accounts (otto-rs pattern): `Discovered accounts: home, work`

**Phase 4: Service updates**
- Update `generate_service()` to drop `--config` from ExecStart
- Update `service install` to scan and validate all accounts
- Update `service install` warnings for per-account token caches
- Print discovered accounts: `Found accounts: work, home`

### Migration Path

Existing `eratosthenes.yml` is discovered as account "eratosthenes" - no immediate rename needed. To add the second account:

1. Create credential directory: `mkdir -p ~/.config/eratosthenes/home`
2. Copy (or symlink) client-secret into `home/`: `cp ~/.config/eratosthenes/client-secret.json ~/.config/eratosthenes/home/client-secret.json`
3. Create `~/.config/eratosthenes/home.yml` with home-specific filters and auth paths pointing to `home/`
4. Auth the home account: `eratosthenes auth login --account home`
5. Test both: `eratosthenes run --dry-run`
6. Optionally rename old config: `mv eratosthenes.yml work.yml` and restructure work creds into `work/` subdirectory
7. Reinstall service: `eratosthenes service reinstall`

## Alternatives Considered

### Alternative 1: Separate binary invocations (no code changes)

- **Description:** Run `eratosthenes --config work.yml` and `eratosthenes --config home.yml` as separate commands, with two systemd timers
- **Pros:** Zero code changes, works today
- **Cons:** Two timers to manage, log file collision (both write to `eratosthenes.log`), no unified reporting, user must remember to run both
- **Why not chosen:** Log collision is a real bug (not just inconvenience), and managing N timers doesn't scale

### Alternative 2: Single config with `accounts:` wrapper

```yaml
accounts:
  work:
    auth:
      client-secret-path: ...
    message-filters: [...]
  home:
    auth:
      client-secret-path: ...
    message-filters: [...]
```

- **Pros:** Single file
- **Cons:** File grows unwieldy with two full filter sets. Can't test one account independently with `--config`. Requires schema changes to `Config`. Breaks backwards compatibility.
- **Why not chosen:** Separate files are more maintainable, independently testable, and require no schema changes

### Alternative 3: Per-account systemd services (no in-process parallelism)

- **Description:** `service install` creates `eratosthenes-work.service`/`.timer` and `eratosthenes-home.service`/`.timer`
- **Pros:** True process isolation, separate journal entries, can stop/start per account
- **Cons:** N timers to manage, no unified "run" for manual invocations, more complex service management code, timer fire times drift apart
- **Why not chosen:** In-process parallelism via `tokio::JoinSet` is simpler, gives unified output, and still achieves full rate-limit independence since each task has its own `GmailClient`

### Alternative 4: Switch to `tracing` for per-file logging

- **Description:** Replace `log`+`env_logger` with `tracing`+`tracing-subscriber`, use per-account file layers
- **Pros:** True per-file log bifurcation, structured logging, async-native
- **Cons:** Large dependency change, significant refactor of all log callsites, overkill for the current need
- **Why not chosen:** Account-prefixed single file is sufficient now. `tracing` migration can happen independently if needed.

## Technical Considerations

### What Doesn't Change

The core engine is completely untouched in logic:
- `Config`, `AuthConfig`, `MessageFilter`, `StateFilter` - no schema changes
- `engine::execute()` - same 3-phase pipeline, just gains an `account` string parameter for output prefixing
- `GmailClient`, `RateLimiter`, `LabelResolver` - no changes
- `gmail::auth` - no changes (already parameterized by `AuthConfig`)
- Filter matching, TTL evaluation, batch modification - all unchanged

### Dependencies

**New:** None. `tokio::JoinSet` is in `tokio` (already a dependency). Config directory scanning uses `std::fs::read_dir` + path extension matching.

**Existing (unchanged):** All current dependencies work as-is.

### Performance

True parallel execution. Two accounts run concurrently with independent rate limiters. Total wall-clock time is approximately `max(account_a_time, account_b_time)` instead of `sum`. Since rate limiting (15k tokens, 250/sec refill) is the bottleneck, this is a meaningful improvement for accounts with large mailboxes.

### Security

- Each account's OAuth credentials are isolated in separate subdirectories
- No cross-account data sharing within the process
- Token caches are per-account, so a compromised token for one account doesn't affect the other
- Callback ports must differ - if both use the same port, one OAuth flow would intercept the other's redirect

### Testing Strategy

- **Unit tests:** `discover_accounts()` with a temp directory containing 0, 1, and 2 `.yml` files. Account name derivation from filename. Config resolution precedence.
- **Engine tests:** Existing tests unchanged (engine doesn't know about accounts)
- **Integration test:** Two mock configs, verify both execute (in dry-run mode)
- **CLI tests:** `--config work.yml` produces single account. No args discovers all. `--account work` resolves correctly.

### Rollout Plan

1. Implement Phase 1-2 (discovery + parallel execution)
2. Test with existing single config (backwards compat)
3. Create `home.yml`, set up home OAuth
4. Run `eratosthenes run --dry-run` and verify both accounts process
5. Implement Phase 3-4 (account targeting + service updates)
6. `eratosthenes service reinstall`

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| OAuth callback port conflict | Medium | High | Each config must have a unique `callback-port`. Validate at startup: if two configs share a port, fail with a clear error. |
| One account failure at runtime | Medium | Low | `JoinSet` collects all results independently. Report per-account success/failure. One failing account does not cancel the other. Exit code is non-zero if any account failed. |
| One account has invalid config | Low | Medium | All configs are validated before any tasks spawn. Fail-fast: if any config is invalid, no accounts run. This prevents confusing partial runs where you think everything is fine but one account was silently skipped. |
| Interleaved console output is hard to read | Low | Low | Account prefix on every line. Output is already line-oriented (one `println!` per action). Tokio tasks yield at `.await` points, so interleaving happens at natural boundaries. |
| Config directory has non-account `.yml` files | Low | Low | Only glob `*.yml` at the top level of the config dir. Subdirectories (like `work/` and `home/`) are not scanned. Could add an ignore list or require a specific naming convention if this becomes a problem. |
| Token cache migration from existing layout | Low | Low | Clear migration instructions. Could also support the old `eratosthenes.yml` name as a fallback with a deprecation warning. |
| Shared log file under heavy parallel writes | Low | Low | `env_logger` writes are already mutex-protected. Lines are short. No corruption risk, just interleaving (which is expected and prefixed). |

## Open Questions

- [x] Should the old `eratosthenes.yml` filename be supported as a deprecated fallback, or should migration be a hard requirement? **Resolved:** Discovery globs all `*.yml` files. Any valid eratosthenes config (has `auth:` section) is treated as an account. `eratosthenes.yml` would be discovered with account name "eratosthenes" - no special handling needed. User renames it when ready.
- [x] Should `eratosthenes run work` be supported for running a single discovered account? **Yes.** Optional positional args on each subcommand. Empty = all discovered. Unknown names produce a CLI error.
- [x] Should commands operate on all accounts by default? **Yes.** All commands default to all accounts, with optional positional args to narrow down.
- [x] For the home account: does it need a separate Google Cloud project with its own OAuth app, or can the same client-secret work for both accounts? **Same client-secret works** - it identifies the app, not the user. Separate token caches are required.
- [x] Should config validation reject `.yml` files that don't parse as eratosthenes configs, or silently skip them? **Skip with a warning.** Allows other yml files to coexist in the config dir if needed.

## References

- Existing design docs: `docs/design/2026-03-29-subcommand-restructure.md`
- `tokio::task::JoinSet`: https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html
- Gmail API rate limits: https://developers.google.com/gmail/api/reference/quota
