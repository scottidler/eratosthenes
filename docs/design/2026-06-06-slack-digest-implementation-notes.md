# Implementation Notes: Slack Pinned-Inbox Digest

Running, append-only record of how the implementation interprets or diverges
from `2026-06-06-slack-digest.md`. Newest entries supersede older ones; history
is never rewritten.

## Phase 0: Pre-existing working-tree state (not part of the design)

### Design decisions
- None.

### Deviations
- The working tree handed to the executor already contained uncommitted WIP
  ("request all config-referenced metadata headers" in `src/gmail/client.rs` +
  `src/lib.rs`). It was committed as-is on its own (`fix(gmail): ...`) so the
  digest work lands on a clean base. It is unrelated to this design doc.
- HEAD was formatted under an older rustfmt and tripped two deny-by-default
  clippy lints under the active rustc 1.96 (`cloned_ref_to_slice_refs` in
  `engine.rs`, `ptr_arg` in `logging.rs`). A `chore:` commit reformats the tree
  and applies those two trivial fixes so `otto ci` is green. No behavior change.

### Tradeoffs
- Committing the found WIP vs. leaving it unstaged: committed, because it is
  entangled with `lib.rs` (which the feature also edits) and per-commit
  buildability requires the `client.rs` definition and `lib.rs` call together.

### Open questions
- Confirm the metadata-headers WIP was meant to be committed (it was found
  uncommitted, complete, and passing CI).

## Phase 1: Config + Slack transport

### Design decisions
- `SlackConfig` added as `Option<SlackConfig>` on `Config` with `#[serde(default)]`
  so an absent `slack:` block deserializes to `None` (digest no-op) — `cfg/config.rs`.
- `HttpSlackPoster` reuses the exact hyper + hyper-rustls connector builder that
  `lib.rs::run` uses for Gmail, so no second TLS/crypto provider — `slack/mod.rs`.
- `SlackResponse` parses only `ok` + `error`; the `error` string is surfaced in
  the returned `eyre` error on `ok:false` — `slack/mod.rs::post`.

### Deviations
- The design says "No new crates," but `hyper`, `http`, `http-body-util`, and
  `bytes` were NOT direct dependencies (only transitive via `hyper-rustls` /
  `hyper-util` / `google-gmail1`). To issue a raw `chat.postMessage` POST they
  must be named directly, so they were added via `cargo add`. They are the same
  already-compiled crates in the lock tree — no new TLS stack, no `reqwest` — so
  this honors the design's intent ("reuse hyper+hyper-rustls, no reqwest, no
  second crypto provider") even though it adds Cargo.toml entries.
- `#[allow(async_fn_in_trait)]` on `SlackPoster`: native async-fn-in-trait (per
  the design, no `async-trait`) warns by default for public traits; the trait is
  only ever used generically in-crate (never `dyn`), so the warning is suppressed
  deliberately with a comment.

### Tradeoffs
- Dropped the `test_from_env_present_var_ok` happy-path test: it only proved that
  hyper-rustls can build a connector (needs a manually-installed crypto provider
  in tests since `main`'s `init_tls()` never runs), which tests the library, not
  our code. Kept the valuable `test_from_env_missing_var_errors` test, whose path
  returns before any TLS work and needs no crypto provider.
- Test mutex guards bound as `let guard = ...; ...; drop(guard);` (named, explicit
  drop) rather than `let _guard`, because the repo's `.otto.yml` lint grep bans
  any `_name` binding (stricter than the global drop-guard exception).

### Open questions
- None.

## Phase 2: Digest core

### Design decisions
- `Pin` derives `Copy, PartialEq, Eq` so `format` can partition by `i.pin == ...`
  — `digest/mod.rs`.
- Each section is sorted newest-first (`sort_by_key(Reverse(date))`) before
  rendering — the doc shows newest at top but does not state the rule — `format`.
- Empty subject renders as `(no subject)` so the deep link always has visible
  text — `line`.
- `escape_mrkdwn` escapes `&`, `<`, `>` (in that order). `<`/`>` are escaped so a
  `>` in subject/sender cannot prematurely close the `<url|text>` link.
- Truncation drops trailing items from the section with more *shown* items, one at
  a time, re-rendering until the body fits `BUDGET` (3500). A fully-hidden section
  still prints its exact header count and a `... +N more` line — `format`/`render`.

### Deviations
- Signature placement: the doc's empty-set example shows `:giga-claude:` inline
  on the "Inbox clear" line, but the doc's Signature section says it must be on
  its own last line for BOTH the normal and empty-set messages. Implemented the
  stricter Signature-section rule: the signature is always the final line,
  preceded by a blank line, including the empty-set message.

### Tradeoffs
- Dates are formatted from the stored `DateTime<Utc>` (`%b %d`) rather than
  converted to local time. Local would match the local schedule but makes tests
  machine-dependent and adds a tz dependency for a date-only label; chose UTC for
  deterministic output. Near-midnight dates may differ by a day from local.

### Open questions
- Should the `%b %d` date be rendered in the system local timezone (matching the
  local OnCalendar schedule) instead of UTC? Currently UTC for test determinism.

## Phase 3: CLI + library wiring

### Design decisions
- Extracted `build_gmail_client(config, prefix)` in `lib.rs` and refactored `run`
  to use it, so `run` and `digest` share one auth + hyper-rustls transport path
  rather than duplicating the hub-building block.
- `digest()` keeps the design's `(account, config, poster)` signature and reads
  `channel`/`browser_index` from `config.slack`, returning a clear error if it is
  `None` (defense-in-depth; `main` only calls it when `Some`).
- `cmd_digest` builds `HttpSlackPoster::from_env(slack.token_env)` per account
  inside the per-account `logging::ACCOUNT` scope, so a missing token fails that
  one account (collected) without aborting the others.
- The digest does NOT call `set_metadata_headers`: `GmailClient`'s default headers
  (To/Cc/From/Subject) already include the From + Subject the digest needs.

### Deviations
- None.

### Tradeoffs
- `digest` uses a per-account `[name] ` prefix unconditionally (unlike `run`,
  which only prefixes in multi-account mode). The digest commonly runs over all
  accounts from the timer, and the prefix disambiguates journald output; the
  signature is `(account, config, poster)` so no `multi` flag is threaded in.
- Pinned-set queries use text queries `in:inbox is:starred` / `in:inbox
  is:important` via `list_threads` (thread-level) rather than `is:important`'s
  per-message matching, guaranteeing one line per thread per the design.

### Open questions
- None.

## Phase 4: systemd digest units

### Design decisions
- `eratosthenes-digest.{service,timer}` are generated alongside the run units in
  `service.rs`. The service is `Type=oneshot`, `ExecStart=<bin> digest`, with
  `EnvironmentFile=-%h/.config/eratosthenes/digest.env` (optional via `-`).
- `validate_schedule` shells out to `systemd-analyze calendar <spec>` and bails on
  a non-zero exit before any unit is written.
- `install_digest_units` lays the digest units ONLY when >=1 discovered account
  has a `slack` block; otherwise it removes any stale digest units and prints a
  note. The run units still install unconditionally.
- `resolve_digest_schedule` uses the first slack-enabled account's `schedule` and
  warns (stderr) for every other slack-enabled account whose schedule differs.
- `write_digest_env` collects DISTINCT `token-env` names across slack-enabled
  accounts, writes `NAME=value` lines for those set in the install-time
  environment (mode 600 via `PermissionsExt`), and warns for any unset name.
- `uninstall`/`reinstall`/`status` manage both unit sets; `status` prints the
  digest timer only when its unit files exist.

### Deviations
- `uninstall` removes the digest unit *files* but intentionally leaves
  `digest.env` in place (it is a user-provided secret; deleting unprompted would
  violate the no-unrequested-deletes rule). `reinstall` rewrites it.

### Tradeoffs
- A single digest timer for all accounts (per the design) means one schedule
  governs every slack-enabled account; divergent schedules are surfaced as a
  warning rather than supported with per-account timers.

### Open questions
- None.

## Phase 5: Tests, docs, ship

### Design decisions
- Smoke tests in `tests/smoke.rs` run the compiled binary
  (`CARGO_BIN_EXE_eratosthenes`) for `digest --help` and `--help`, asserting the
  subcommand dispatches and is listed.
- Added the optional `slack:` block to the example `eratosthenes.yml` and wrote a
  real README (it was a 2-line stub) covering the digest command, config, token
  handling, and the timer.

### Deviations
- The example `eratosthenes.yml` keeps the existing list-of-dicts form for
  message/state filters (the repo has custom named-sequence deserializers); not
  migrated to keyed-maps, as that is unrelated to this feature.

### Tradeoffs
- None.

### Open questions
- Confirm `SLACK_XOXP_TOKEN` carries the `chat:write` scope (verified at first
  post; `ok:false` surfaces the error clearly otherwise).
- `service reinstall` (writes the live token to `digest.env` and enables timers)
  was intentionally NOT run by the executor; it is a system mutation requiring
  the token in the environment. Left for the user (see Rollout).
