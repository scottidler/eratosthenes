# CLI Shakedown Report: eratosthenes v0.7.4

Shaken down 2026-10-05 against the live `tatari` account on desk, right after v0.7.4 shipped the backward-move guard (`285cf0f`, "never move a thread backward down the stage ladder").

## Summary

| Metric | Count |
|--------|-------|
| Commands discovered | 19 (6 top-level + 13 nested) |
| Commands tested | 9 |
| Commands passed | 9 |
| Commands failed | 0 |
| Commands skipped | 10 (mutating: Gmail writes, Slack posts, OAuth state, systemd units) |
| Edge cases tested | 10 |
| Bugs / findings | 5 (0 blocking) |

## Command Results

### Read-only (tested)

| Invocation | Exit | Result |
|---|---|---|
| `eratosthenes --version` / `-V` | 0 | `eratosthenes v0.7.4` |
| `eratosthenes config show` | 0 | account, creds paths, 4 message filters, 9 state filters, triage block |
| `eratosthenes config show tatari` | 0 | identical to the no-arg form |
| `eratosthenes config validate` | 0 | `Account 'tatari' config is valid.` plus resolved filters and 5 buckets |
| `eratosthenes -c ~/.config/eratosthenes/tatari.yml config validate` | 0 | valid, same output |
| `eratosthenes auth status` | 0 | `Status: AUTHENTICATED` |
| `eratosthenes service status` | 0 | 3 timers loaded and enabled: engine, digest (next Thu 07:00), triage (next Tue 06:30) |
| `eratosthenes run --dry-run tatari` | 0 | `Done: 0 messages matched filters, 223 threads transitioned (dry run)` (log only, see findings) |
| `eratosthenes triage --dry-run tatari` | 0 | 29-row thread -> bucket table, `29 threads classified, 0 labeled, 0 skipped (dry run)` |

### Skipped (mutating)

- `run` (Gmail writes), `run --mark-only` (irreversible marker stamps)
- `triage` (Gmail label writes)
- `digest` (posts to Slack)
- `auth login` (browser OAuth), `auth logout` (clears token cache)
- `service install | uninstall | reinstall | start | stop` (systemd units)

`run` was exercised anyway by the production timer; see Fix Verification.

## Fix Verification (production timer, v0.7.4)

Before the fix, `age-recruiting` / `age-*` bucket filters dragged Oblivion threads back to Purgatory and `Purge` sent them back the next run. Every run logged `643 threads transitioned`, and on 2026-10-05 17:24 PDT one of those writes came back `400 failedPrecondition` and failed the unit.

| Run (UTC) | Binary | Transitions | Writes Oblivion -> Purgatory (backward) |
|---|---|---|---|
| 00:28:27 | v0.7.3 | 643 | yes, half of all writes |
| 00:34:37 | v0.7.3 | 640 | yes |
| 00:40:46 | v0.7.4 | 367 | 0 (all `add=Label_60 remove=Label_61`, Purgatory -> Oblivion) |
| 00:45:50 | v0.7.4 | 0 | 0 (zero `modify_thread` calls; 639 `refusing backward move`) |

The first v0.7.4 run drains the threads the last v0.7.3 run had pulled back into Purgatory. From the second run on, the ladder sits at its fixed point: zero writes.

## Output Format Matrix

eratosthenes has no `--json` / `--csv` flags. Every command prints plain text; `run` prints only `Connecting to Gmail...` and writes everything else to `~/.local/share/eratosthenes/logs/<account>.log`.

| Command | Text | JSON | CSV |
|---|---|---|---|
| `config show` | yes | n/a | n/a |
| `config validate` | yes | n/a | n/a |
| `auth status` | yes | n/a | n/a |
| `service status` | yes (systemctl passthrough) | n/a | n/a |
| `triage --dry-run` | yes (table) | n/a | n/a |
| `run --dry-run` | no summary on stdout | n/a | n/a |

## Failures & Bugs

1. **Unknown `--log-level` is silently accepted** (bug). `eratosthenes -l LOUD config show` exits 0 and runs at `info`: `src/logging.rs:108` maps any unrecognized string to `LevelFilter::Info`. Should be a clap `ValueEnum` (`ignore_case = true`) so a typo is a usage error.
2. **Retry exhaustion drops the cause** (bug). With the network blocked, `run --dry-run` ended `labels.list failed after 5 retries` with no hint of the DNS failure underneath: `src/gmail/rate.rs:238` bails with a fresh error instead of wrapping the last one.
3. **Backoff is logged as rate limiting for every retryable error** (bug). A DNS failure logs `Rate limited, backing off for 2s` (`src/gmail/rate.rs:57`). The message names a cause that is not the cause.
4. **`run --dry-run` prints no plan to stdout** (suggestion). The `Done: N threads transitioned (dry run)` line only reaches the log, so a dry run at the terminal shows `Connecting to Gmail...` and nothing else. `triage --dry-run` prints its table; `run --dry-run` should print at least its summary.
5. **`triage --dry-run` table does not truncate subjects** (cosmetic). One row ran past 200 characters and wraps the terminal.

## Edge Cases

| Invocation | Exit | Behavior |
|---|---|---|
| `config validate nosuchaccount` | 1 | `unknown account 'nosuchaccount', available accounts: ["tatari"]` |
| `config show nosuchaccount` | 1 | same |
| `auth status nosuchaccount` | 1 | same |
| `run --dry-run nosuchaccount` | 1 | same, before any Gmail call |
| `config validate ""` | 1 | `unknown account ''` |
| `-c /nonexistent.yml config validate` | 1 | `Failed to read config file /nonexistent.yml: No such file or directory` |
| `-c <malformed yaml> config validate` | 1 | `Failed to parse YAML: did not find expected node content at line 2 column 1` |
| `eratosthenes bogus` | 2 | clap `unrecognized subcommand` + usage |
| `run --bogus-flag` | 2 | clap `unexpected argument` + usage |
| `-l LOUD config show` | 0 | silently runs at info (finding 1) |

Every error path exits non-zero with a clear message. All `Error:` output carries an eyre `Location:` line pointing at source, which is noise for an end user but harmless.

## Release Validation

- **Tag:** `v0.7.4` exists, annotated (`git cat-file -t` = `tag`), points at `9f22fa3` "Bump version to v0.7.4", whose parent is the fix `285cf0f`.
- **Release:** published 2026-10-06T00:39:58Z, not draft, not prerelease.
- **Assets:** all four targets present, each with a `.sha256`: `linux-amd64`, `linux-arm64`, `macos-arm64`, `macos-x86_64` (`.tar.gz`).
- **Binary test:** downloaded `eratosthenes-v0.7.4-linux-amd64.tar.gz`, `sha256sum -c` OK, extracted binary prints `eratosthenes v0.7.4`, matching the installed one.

## Observations

- `service install --interval` defaults to `5min`, but runs land 4-6 minutes apart because a oneshot unit's timer re-arms after the run. The zero-write v0.7.4 run still took 3m59s (00:45:50 -> 00:49:49), so run time is dominated by reads, not by the 643 writes the ping-pong added.
- Each steady-state run logs 639 `refusing backward move` DEBUG lines, one per bucket-labeled thread parked in Oblivion, and that count grows as Oblivion fills.
- Configured `draft-model: claude-sonnet-5` (also the code default, `src/cfg/triage.rs:109`). Not verified against the Claude CLI; inert today because every bucket has `draft: false`.
- `run --dry-run` and the production timer share one log file, so concurrent runs interleave their lines with no per-run id. Separating them during this shakedown meant filtering on `dry_run=true` and on `modify_thread` (which a dry run never calls).
