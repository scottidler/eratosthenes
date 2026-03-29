# Design Document: Subcommand Restructure and Systemd Timer Service

**Author:** Scott Idler
**Date:** 2026-03-29
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Restructure eratosthenes from a flat-flag CLI into a subcommand-based CLI (`run`, `auth`, `service`, `config`) and add a `service` subcommand that installs/manages systemd user timer units for automated periodic execution. This keeps the one-shot execution model while automating scheduling.

## Problem Statement

### Background

Eratosthenes is a Gmail API-native inbox zero engine that processes email in three phases: stage sanitization, message filtering, and thread age-off. It currently runs as a one-shot CLI invoked manually or via external scheduling. Authentication (`--login`, `--logout`) and execution (`--dry-run`) are flat flags on a single command.

### Problem

1. **No built-in scheduling** - users must manually set up cron or systemd timers externally to keep their inbox processed. There's no `eratosthenes install` path.

2. **Flat CLI doesn't scale** - `--login` and `--logout` are auth lifecycle operations mixed in with execution flags. As more capabilities are added (service management, config validation), a flat namespace becomes cluttered and confusing.

3. **Inconsistency with other tools** - `aka`, `cortex`, and `kondo` all have self-install capabilities for their scheduling needs. Eratosthenes should follow the same pattern.

### Goals

- Restructure CLI into logical subcommand groups: `run`, `auth`, `service`, `config`
- Bare `eratosthenes` (no subcommand) remains equivalent to `eratosthenes run` for backwards compatibility
- Add `service install` that generates systemd user timer + oneshot service units
- Add `service uninstall`, `service status`, `service start`, `service stop`
- Model service management after cortex's `daemon.rs` pattern (systemd user units)

### Non-Goals

- Long-running daemon process (eratosthenes stays one-shot)
- Docker containerization
- Gmail Pub/Sub push notification integration
- macOS launchd support (Linux-only for now, can add later)
- Crontab manipulation (systemd timers only)

## Proposed Solution

### Overview

Split the CLI into four subcommand groups. The `service` subcommand generates and manages systemd user units (a `.service` file of Type=oneshot and a `.timer` file) that periodically invoke `eratosthenes run`.

### Architecture

```
eratosthenes              # no subcommand = run (backwards compat)
eratosthenes run          # execute the engine
eratosthenes auth         # auth lifecycle
eratosthenes service      # systemd timer management
eratosthenes config       # config utilities
```

**New source files:**

```
src/
  cli.rs          # restructured: Cli + Command enum + per-subcommand opts
  main.rs         # dispatch on Command variant
  service.rs      # systemd unit generation, install/uninstall/status/start/stop
  lib.rs          # unchanged (run logic stays here)
```

### CLI Design

```rust
#[derive(Parser)]
#[command(name = "eratosthenes", about = "Gmail API-native inbox zero engine")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    // Global flags (available on all subcommands and bare invocation)
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    #[arg(short, long, global = true)]
    pub log_level: Option<String>,

    /// Dry run - show what would be done without making changes
    #[arg(long, global = true)]
    pub dry_run: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the inbox zero engine (default when no subcommand given)
    Run,

    /// Manage OAuth2 authentication
    Auth(AuthOpts),

    /// Manage systemd timer service
    Service(ServiceOpts),

    /// Config utilities
    Config(ConfigOpts),
}

#[derive(Args)]
pub struct AuthOpts {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Subcommand)]
pub enum AuthCommand {
    /// Force re-authentication (clear token cache, open browser)
    Login,
    /// Clear cached OAuth2 tokens
    Logout,
    /// Show current authentication status
    Status,
}

#[derive(Args)]
pub struct ServiceOpts {
    #[command(subcommand)]
    pub command: ServiceCommand,
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Install systemd user timer and service
    Install {
        /// Timer interval (default: 5min)
        #[arg(long, default_value = "5min")]
        interval: String,
    },
    /// Remove systemd user timer and service
    Uninstall,
    /// Reinstall (uninstall then install)
    Reinstall {
        #[arg(long, default_value = "5min")]
        interval: String,
    },
    /// Show service and timer status
    Status,
    /// Start the timer
    Start,
    /// Stop the timer
    Stop,
}

#[derive(Args)]
pub struct ConfigOpts {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Validate config file and show resolved filters
    Validate,
    /// Show resolved config path
    Show,
}
```

**Resulting help output:**

```
$ eratosthenes --help
Gmail API-native inbox zero engine

Usage: eratosthenes [OPTIONS] [COMMAND]

Commands:
  run      Run the inbox zero engine (default when no subcommand given)
  auth     Manage OAuth2 authentication
  service  Manage systemd timer service
  config   Config utilities
  help     Print this message or the help of the given subcommand(s)

Options:
  -c, --config <CONFIG>        Path to config file
  -l, --log-level <LOG_LEVEL>  Log level (error, warn, info, debug, trace)
  -h, --help                   Print help
  -V, --version                Print version

$ eratosthenes service --help
Manage systemd timer service

Usage: eratosthenes service <COMMAND>

Commands:
  install    Install systemd user timer and service
  uninstall  Remove systemd user timer and service
  reinstall  Reinstall (uninstall then install)
  status     Show service and timer status
  start      Start the timer
  stop       Stop the timer

$ eratosthenes auth --help
Manage OAuth2 authentication

Usage: eratosthenes auth <COMMAND>

Commands:
  login   Force re-authentication (clear token cache, open browser)
  logout  Clear cached OAuth2 tokens
  status  Show current authentication status
```

### Systemd Units

The `service install` command generates two files in `~/.config/systemd/user/`:

**eratosthenes.service:**
```ini
[Unit]
Description=Eratosthenes Gmail Inbox Zero Engine

[Service]
Type=oneshot
ExecStart={binary} run --config {config_path}
Environment=PATH={cargo_bin}:/usr/local/bin:/usr/bin:/bin
```

**eratosthenes.timer:**
```ini
[Unit]
Description=Eratosthenes Periodic Timer

[Timer]
OnBootSec=2min
OnUnitActiveSec={interval}
Persistent=true

[Install]
WantedBy=timers.target
```

Key design choices:
- **Type=oneshot** - not a daemon, runs and exits
- **OnUnitActiveSec** - interval between completions, not fixed wall-clock schedule. This prevents overlap if a run takes longer than the interval.
- **Persistent=true** - if the machine was off when a timer would have fired, run immediately on next boot
- **No WantedBy on the service** - the timer manages the service lifecycle, not the user session
- **Config path baked in** - resolved and **canonicalized** at install time so the timer doesn't depend on working directory. Relative paths like `./eratosthenes.yml` are resolved to absolute paths.
- **Binary path from current_exe()** - same pattern as cortex, captures the exact installed binary. **Warn** if the path contains `target/debug` or `target/release` (suggests `cargo run`, not `cargo install`).

**Edge case: idempotent install.** Running `service install` when already installed overwrites the unit files and restarts the timer. This is safe and expected (e.g., changing the interval).

### Interval Parsing

The `--interval` flag accepts human-friendly duration strings that map to systemd timer syntax:

| Input | OnUnitActiveSec |
|-------|----------------|
| `5min` | `5min` |
| `10min` | `10min` |
| `1h` | `1h` |
| `30min` | `30min` |

Systemd natively parses these duration strings, so we pass them through directly. Validation: reject values under 1 minute (aggressive polling) and over 24 hours (defeats the purpose).

### Service Management Commands

**install:**
1. Resolve binary path via `std::env::current_exe()`
2. Resolve config path via `resolve_config_path(cli.config)` (same precedence as run: CLI > XDG > cwd). Fail with clear error if no config found.
3. Create `~/.config/systemd/user/` if needed
4. Write `eratosthenes.service` and `eratosthenes.timer`
5. Run `systemctl --user daemon-reload`
6. Run `systemctl --user enable --now eratosthenes.timer`
7. Print confirmation:
```
Installed: ~/.config/systemd/user/eratosthenes.service
Installed: ~/.config/systemd/user/eratosthenes.timer
Timer enabled and started (interval: 5min)
Hint: run `loginctl enable-linger $USER` for timer to run when not logged in
```

**uninstall:**
1. Run `systemctl --user stop eratosthenes.timer` (ignore error if not running)
2. Run `systemctl --user disable eratosthenes.timer` (ignore error if not enabled)
3. Remove `eratosthenes.service` and `eratosthenes.timer`
4. Run `systemctl --user daemon-reload`
5. Print confirmation

**status:**
1. Check if unit files exist in `~/.config/systemd/user/`
2. If not installed, print: `Service not installed. Run: eratosthenes service install`
3. If installed, run `systemctl --user status eratosthenes.timer` and pass through output (shows next fire time, last trigger, active state)

**start/stop:**
1. Run `systemctl --user start/stop eratosthenes.timer`

**reinstall:**
1. Run uninstall (suppress errors)
2. Run install with provided interval

### Auth Status Command

The new `auth status` command checks:
1. Does the token cache file exist?
2. Can we read and parse it?
3. Is the token expired? (check expiry timestamp if available)
4. Print: auth state, token path, expiry info

This is useful for debugging service failures ("why isn't my timer working?" - often a token expiry issue).

### Config Subcommands

**validate:**
1. Load and parse config file
2. Print resolved filter count and names
3. Report any warnings (unknown labels, suspicious patterns)
4. Exit 0 on success, non-zero on parse failure

**show:**
1. Print the resolved config file path
2. Print config contents (or a structured summary)

### Backwards Compatibility

`eratosthenes --dry-run` (no subcommand) continues to work. `--dry-run` is a global flag on `Cli`, so it works in any position: `eratosthenes --dry-run`, `eratosthenes run --dry-run`, `eratosthenes --dry-run run`. When `Command` is `None`, the CLI treats it as an implicit `run`.

**Config loading is conditional on the subcommand.** Not all subcommands need a parsed config:
- `run`, `auth login/logout/status` - need full config
- `service install` - needs config path (to bake into ExecStart) but not full parsing
- `service uninstall/status/start/stop` - need no config at all
- `config validate/show` - need the config path

```rust
// In main.rs dispatch:
match cli.command {
    None | Some(Command::Run) => {
        let config = load_config_or_exit(&cli)?;
        setup_logging(&resolve_log_level(cli.log_level.as_deref(), &config.log_level))?;
        run_engine(&config, cli.dry_run).await
    }
    Some(Command::Auth(opts)) => {
        let config = load_config_or_exit(&cli)?;
        handle_auth(&config, opts).await
    }
    Some(Command::Service(opts)) => {
        // Only install needs config path; status/start/stop/uninstall don't
        handle_service(&cli, opts)
    }
    Some(Command::Config(opts)) => {
        handle_config(&cli, opts)
    }
}
```

Note: `--login` and `--logout` as bare flags are removed. Users must use `eratosthenes auth login` and `eratosthenes auth logout`. This is the one breaking change, but it's early enough in the project lifecycle (v0.1.3) that this is acceptable.

### Implementation Plan

**Phase 1: CLI restructure** (no new functionality)
- Restructure `cli.rs`: add `Command` enum with `Run`, `Auth`, `Service`, `Config` variants
- Move `--dry-run` to global flag on `Cli` struct (works in any position)
- `Run` is a unit variant (no args) since `--dry-run` is global
- Update `main.rs` dispatch: match on `Command`, load config conditionally per subcommand
- Move `--login`/`--logout` handling into `auth login`/`auth logout` match arms
- All existing tests continue to pass
- `cargo install --path .` and verify `eratosthenes --help` shows subcommands

**Phase 2: Service subcommand**
- Create `src/service.rs` with systemd unit generation
- Implement `install`, `uninstall`, `status`, `start`, `stop`, `reinstall`
- Shell out to `systemctl --user` for lifecycle operations
- Unit tests for template generation (no systemctl in tests)

**Phase 3: Auth status and config subcommands**
- Implement `auth status` (token cache inspection)
- Implement `config validate` and `config show`
- These are convenience commands, lower priority

## Alternatives Considered

### Alternative 1: Crontab manipulation (kondo-style)
- **Description:** Write cron entries directly to the user's crontab
- **Pros:** Works everywhere, no systemd dependency
- **Cons:** Fragile (crontab parsing/editing), no built-in logging, no `Persistent=true` equivalent, no overlap prevention
- **Why not chosen:** systemd timers are strictly better on Linux - they handle missed runs, prevent overlap (Type=oneshot), integrate with journald, and are the pattern already used by cortex

### Alternative 2: Long-running daemon (aka-style)
- **Description:** Run a persistent daemon with sleep loop and optional IPC
- **Pros:** Could support instant-response triggers
- **Cons:** Eratosthenes doesn't need sub-second latency. A daemon that sleeps 99.9% of the time is wasted complexity. No IPC needed - there's no query protocol.
- **Why not chosen:** One-shot + timer is simpler, more robust, and matches the tool's execution model

### Alternative 3: Keep flat flags, just add --install
- **Description:** Add `--install` and `--uninstall` as top-level flags alongside `--login`/`--logout`
- **Pros:** Minimal change
- **Cons:** CLI becomes increasingly cluttered. Auth, service, and execution flags all compete in the same namespace. Doesn't scale.
- **Why not chosen:** Subcommands are the right abstraction now, before the flag count grows further

### Alternative 4: Separate `eratosthenes-timer` binary
- **Description:** A second binary in the workspace that handles service management
- **Pros:** Clean separation
- **Cons:** Two binaries to install and maintain, confusing for users
- **Why not chosen:** A subcommand in the same binary is simpler and follows the aka/cortex/kondo pattern

## Technical Considerations

### Dependencies

**New:**
- None required. `std::process::Command` handles systemctl invocation. `std::fs` handles unit file writing. `dirs` (already a dependency) provides XDG paths.

**Existing (unchanged):**
- `clap` with `derive` feature (already supports subcommands)
- `dirs` for `~/.config` resolution
- `eyre` for error handling

### Performance

No performance impact. The subcommand dispatch is compile-time via enum matching. Service management commands are fast filesystem + systemctl operations. The engine execution path is unchanged.

### Security

- Unit files are written to the user's own `~/.config/systemd/user/` directory - no root/sudo required
- The service runs as the user's own systemd user instance
- OAuth tokens remain in the user's config directory with standard permissions
- Binary path is captured from `current_exe()` at install time - if the binary moves, a `reinstall` is needed
- No secrets are embedded in unit files

### Testing Strategy

- **Unit tests for `service.rs`:** Test unit file template generation (string output), interval validation, path resolution. No actual systemctl calls in tests.
- **Integration test:** `eratosthenes service install --interval 5min` on a real system, verify files exist, verify `systemctl --user list-timers` shows the timer, then `eratosthenes service uninstall` and verify cleanup.
- **CLI tests:** Verify subcommand parsing - `eratosthenes run --dry-run`, `eratosthenes auth login`, bare `eratosthenes --dry-run` all route correctly.
- **Backwards compat test:** Verify that `eratosthenes --dry-run` still works (implicit `run` subcommand).

### Rollout Plan

1. Implement Phase 1 (CLI restructure) - no behavior change except `--login`/`--logout` become `auth login`/`auth logout`
2. Implement Phase 2 (service subcommand) - new functionality
3. Implement Phase 3 (auth status, config validate/show) - convenience
4. `bump -m` (minor version bump, since new subcommands are additive features)
5. `cargo install --path .` and `eratosthenes service install`
6. Verify with `systemctl --user status eratosthenes.timer`

### Expected User Workflow

First-time setup:
```bash
# 1. Install the binary
cargo install --path .

# 2. Create config
vim ~/.config/eratosthenes/eratosthenes.yml

# 3. Validate config
eratosthenes config validate

# 4. Authenticate (opens browser)
eratosthenes auth login

# 5. Test manually
eratosthenes run --dry-run
eratosthenes run

# 6. Install the timer
eratosthenes service install --interval 5min

# 7. Verify
eratosthenes service status
```

After updating the binary:
```bash
cargo install --path .
eratosthenes service reinstall   # refreshes ExecStart path if needed
```

Debugging:
```bash
eratosthenes auth status         # token OK?
eratosthenes service status      # timer running?
journalctl --user -u eratosthenes --since "1 hour ago"   # recent runs
```

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Token expires while running as timer | Medium | Medium | OAuth2 refresh tokens auto-renew. If refresh fails, `auth status` helps diagnose. Timer keeps retrying on next interval. |
| Refresh token revoked or expired (requires browser) | Low | High | Browser redirect can't work in headless systemd context. `service install` should check token status and warn if not authenticated. User must run `eratosthenes auth login` interactively before installing the service. |
| Binary moves after `cargo install` upgrade | Low | Low | `cargo install` always puts binaries in `~/.cargo/bin/`, so the path is stable across upgrades. Only an issue if installed from `target/debug/`. Install warns about non-standard paths. |
| systemd user session not enabled (e.g., no `loginctl enable-linger`) | Low | Medium | Timer only runs while user is logged in. For always-on, user needs `loginctl enable-linger`. Print hint in install output. |
| Breaking change: `--login`/`--logout` removed | Low | Low | Project is v0.1.x, few users. `auth login`/`auth logout` is more discoverable. |
| Interval too aggressive causes rate limiting | Low | Medium | Validate minimum interval (1 minute). Default to 5 minutes. Rate limiter in engine already handles 429s with backoff. |
| Config file path changes after install | Low | Low | Config path is baked into the service file at install time. If config moves, run `service reinstall`. |

## Open Questions

- [x] Should `service install` automatically run `eratosthenes auth status` first and warn if not authenticated? **Yes** - check if token cache exists and warn if not. Don't block install, just warn.
- [x] Should `service install` print a `loginctl enable-linger` hint? **Yes** - always print as a hint in the install output.
- [ ] Should `config validate` attempt a dry-run Gmail API call to verify credentials, or just validate YAML syntax? Leaning toward YAML-only for `config validate` and leaving credential checks to `auth status`.

## References

- cortex daemon.rs: `/home/saidler/repos/scottidler/second-brain/cortex/src/daemon.rs` - systemd unit generation pattern
- aka daemon: `/home/saidler/repos/scottidler/aka/src/bin/aka.rs` - ServiceManager pattern (more complex than needed here)
- kondo cron: `/home/saidler/repos/scottidler/kondo/src/main.rs` - crontab manipulation (not chosen)
- systemd.timer(5) manpage - OnUnitActiveSec, Persistent, Type=oneshot semantics
