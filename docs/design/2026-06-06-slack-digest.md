# Design Document: Slack Pinned-Inbox Digest

**Author:** Scott Idler
**Date:** 2026-06-06
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Add an `eratosthenes digest` subcommand that posts a Slack message summarizing
the mail currently pinned in the inbox (Starred and Important), grouped and
deep-linked back to Gmail. A dedicated systemd user timer fires it Mon-Fri at
08:00 and 13:00, giving a twice-daily, in-your-face view of what eratosthenes
has kept so it gets acted on instead of silently accumulating.

## Problem Statement

### Background

eratosthenes is a Gmail "inbox zero" engine. Its `state-filters` protect
`Starred` and `Important` threads with `ttl: Keep`, meaning they stay in the
inbox forever while everything else ages off to Purgatory then Oblivion. That
protection is deliberate but one-directional: pinned mail never leaves on its
own, so the pinned set grows until the human clears it. There is currently no
surfacing of that set; it just sits in Gmail competing with everything else.

### Problem

The pinned (Starred + Important) inbox set is the human's real action queue, but
it is invisible until you open Gmail and scroll. There is no push, no recurring
reminder, and no single scannable list. As a result the queue grows unbounded.

### Goals

- A new `eratosthenes digest` subcommand that builds and posts the pinned-mail
  summary to Slack.
- Two grouped sections (Starred, Important) with counts and per-item
  date / sender / subject, where the subject deep-links to the Gmail thread.
- Posts via a Slack user token to your self-DM (configurable channel/token).
- Runs automatically on a configurable schedule (`slack.schedule`, default
  Mon-Fri at 08:00 and 13:00) via a dedicated systemd timer, independent of the
  5-minute `run` loop.
- Reuses the existing Gmail client and account model; no new Gmail auth.

### Non-Goals

- Changing how `run` flags or ages mail (filters/state-filters untouched).
- Two-way interactivity (no Slack buttons, no acting on mail from Slack).
- A "needs reply" detector or any new classification (separate future idea).
- Multi-workspace Slack support; one bot token, one destination per account.
- Storing or diffing prior digests ("post only when changed" was rejected).

## Proposed Solution

### Overview

A `digest` subcommand mirrors `run`: it resolves accounts, and for each account
(processed **sequentially** - the cadence is twice a weekday, so there is no
reason for the `JoinSet` concurrency `run` uses) authenticates to Gmail (reusing
`GmailClient`), queries the pinned set, formats a Slack `mrkdwn` message, and
posts it through a small Slack client. A separate systemd
`eratosthenes-digest.{service,timer}` pair drives the schedule. The Slack token
(a user token, `xoxp`, posting to your self-DM as a note-to-self) is supplied to
the service via an `EnvironmentFile`. A user token was chosen over a bot token
because the destination is the self-DM `D01G4Q7AWLV`, which only the owning user
token can post into; a bot is not a member of it.

The `slack` config block is **per-account and optional**. If an account has no
`slack` block, `digest` logs that it is skipping the account and moves on - it
is never an error. This lets the digest be enabled for one Gmail account without
forcing it on the others. Accounts are processed independently: a Gmail or Slack
failure on one is logged and collected, the loop continues, and the command
exits non-zero only at the end if any account failed.

### Architecture

New, isolated units with single responsibilities:

- `src/slack/` — Slack transport. A `SlackPoster` trait with one method,
  `post(channel, text) -> Result<()>`, and an implementation (`HttpSlackPoster`)
  that calls `chat.postMessage` over the SAME `hyper` + `hyper-rustls` stack the
  Gmail client already uses - no new HTTP/TLS dependency, and no second rustls
  crypto provider to clash with the `aws-lc-rs` default `init_tls()` installs. The
  trait lets the digest core be tested with an in-memory fake (DI via generics).
- `src/digest/` — pure assembly + formatting. Takes the fetched threads and
  produces the Slack `mrkdwn` string. No network, no I/O: fully unit-testable.
- `src/gmail/client.rs` — reused as-is for `list_threads` + `get_thread`
  (thread-level, so one Slack line per thread - see Data flow).
- `src/lib.rs` — new `digest()` entry point paralleling `run()`.
- `src/service.rs` — generalized to manage two unit sets (run + digest).
- `src/cli.rs`, `src/main.rs` — new `Digest` subcommand + dispatch.

Data flow:

```
digest subcommand
  -> resolve_accounts()
  -> for each account SEQUENTIALLY (errors collected, never abort the loop):
       skip with a log line if config.slack is None
       GmailClient::new()
       client.list_threads("in:inbox is:starred")     -> starred thread ids
       client.list_threads("in:inbox is:important")   -> important thread ids
       client.get_thread(id) for each unique thread   -> GmailThread
       digest::build(threads, starred_ids, important_ids) -> Vec<DigestItem>
       digest::format(items, browser_index)           -> String (mrkdwn)
       SlackPoster::post(channel, text)
  -> after all accounts: return Err if any account failed (aggregate, like run)
```

Querying at the THREAD level (not `search_messages`/`get_message`) guarantees
one digest line per thread. `is:starred`/`is:important` match at the message
level, so a thread with several starred replies would otherwise produce duplicate
lines and repeated deep links to the same thread. Sender, subject, and date come
from the thread's latest message (`GmailThread::last_activity`).

### Data Model

```rust
// src/digest/mod.rs
pub enum Pin { Starred, Important }

pub struct DigestItem {
    pub pin: Pin,                                // Starred wins if a thread is both
    pub date: chrono::DateTime<chrono::Utc>,     // thread's latest-message time
    pub sender: String,    // latest message's display name, falling back to email
    pub subject: String,   // latest message's subject
    pub thread_id: String, // for the Gmail deep link (one per thread)
}
```

```rust
// src/cfg/config.rs — new optional block on Config
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SlackConfig {
    #[serde(default = "default_token_env")]
    pub token_env: String,   // env var NAME holding the Slack token, never the token
    pub channel: String,     // self-DM Dxxxx (user token) or Uxxxx/Cxxxx
    #[serde(default)]
    pub browser_index: u8,   // Gmail multi-login slot for deep links (/u/N), default 0
    #[serde(default = "default_schedule")]
    pub schedule: String,    // systemd OnCalendar that drives the digest timer
}
fn default_token_env() -> String { "SLACK_XOXP_TOKEN".to_string() }
fn default_schedule() -> String { "Mon-Fri 08,13:00:00".to_string() }

pub struct Config {
    // ...existing fields...
    pub slack: Option<SlackConfig>,  // digest is a no-op if absent
}
```

Config YAML (per-account, e.g. `~/.config/eratosthenes/tatari.yml`):

```yaml
slack:
  token-env: SLACK_XOXP_TOKEN    # user token (xoxp); posts as you
  channel: D01G4Q7AWLV           # your self-DM channel (note-to-self)
  schedule: Mon-Fri 08,13:00:00  # systemd OnCalendar; controls the digest timer
```

### API Design

```rust
// src/slack/mod.rs
// Native `async fn` in trait (stable on edition 2024 / Rust 1.96). No `async-trait`,
// no `Box<dyn Future>`; consumers take `P: SlackPoster` generically (repo DI rule).
pub trait SlackPoster {
    async fn post(&self, channel: &str, text: &str) -> eyre::Result<()>;
}

// Built on hyper + hyper-rustls (same connector lib.rs builds for Gmail); no reqwest.
pub struct HttpSlackPoster { token: String, http: HyperRustlsClient }
impl HttpSlackPoster {
    // Reads the token from the env var NAMED by `token_env`; errors clearly if unset.
    pub fn from_env(token_env: &str) -> eyre::Result<Self>;
}
```

```rust
// src/digest/mod.rs
pub fn build(threads: &[GmailThread], starred_ids: &HashSet<String>,
             important_ids: &HashSet<String>) -> Vec<DigestItem>;
pub fn format(items: &[DigestItem], browser_index: u8) -> String;
```

```rust
// src/lib.rs
// Caller constructs the poster from the account's slack config and only calls
// this when `config.slack` is Some; a no-op skip for None is handled in main.
pub async fn digest<P: SlackPoster>(account: &str, config: &Config,
                                    poster: &P) -> Result<()>;
```

Gmail deep link: `https://mail.google.com/mail/u/{browser_index}/#all/{thread_id}`.
The `#all/` view resolves a thread regardless of which labels it carries. The
`/u/N` slot is positional across signed-in Google accounts; it comes from the
per-account `slack.browser-index` config (default 0), so a secondary Google login
can be linked correctly without a code change.

Slack message shape (`mrkdwn`, no em dashes, signed). This is a single DM
message - the two-message `:thread:` channel pattern from the Slack conventions
is for busy channels, not a personal DM digest:

```
*Pinned inbox digest* - 24 starred, 12 important

*:star: Starred (24)*
`Jun 04` *Mark Weiler* <https://mail.google.com/mail/u/0/#all/THREAD|Fwd: pentest proposal>
...

*:exclamation: Important (12)*
`Jun 02` *JP Ciceri* <https://...|Fwd: FW: DIRECTV Annual Attestation>
...

:giga-claude:
```

### Implementation Plan

#### Phase 1: Config + Slack transport
**Model:** sonnet
- No new crates: build the Slack client on the existing `hyper` + `hyper-rustls`
  stack (the connector `lib.rs` already builds for Gmail) plus `serde_json`. No
  `reqwest`, no `async-trait` (native `async fn` in the `SlackPoster` trait).
- Add `SlackConfig` to `cfg/config.rs` (optional `slack` field, kebab-case),
  including `schedule` (OnCalendar string, default `Mon-Fri 08,13:00:00`).
- Create `src/slack/` with `SlackPoster` trait, `HttpSlackPoster`
  (`chat.postMessage`, Bearer token, JSON body, parse `ok`/`error`), and a
  `FakeSlackPoster` for tests under `src/slack/tests.rs`.

#### Phase 2: Digest core
**Model:** opus
- Create `src/digest/` with `DigestItem`, `build` (input is threads; a thread that
  is both starred and important appears once, under Starred; sender/subject/date
  from the latest message), and `format` (grouping, date/sender/subject, deep links
  via `browser_index`, signature, mrkdwn escaping, truncation budget).
- Unit tests in `src/digest/tests.rs`: fixed thread fixtures -> expected mrkdwn,
  including the multi-starred-message-in-one-thread case (must yield one line) and
  the over-budget truncation case.

#### Phase 3: CLI + library wiring
**Model:** sonnet
- Add `Command::Digest { accounts }` to `cli.rs`.
- Add `lib::digest()` paralleling `run()` (auth, `list_threads` for both labels,
  `get_thread`, build, format, post). Reuse `GmailClient`.
- Add `cmd_digest` to `main.rs`: iterate accounts SEQUENTIALLY, skip those without
  a `slack` block (log, not error), collect per-account errors, and return `Err`
  at the end if any failed (mirror `cmd_run`'s aggregation).

#### Phase 4: systemd digest units
**Model:** opus
- Generalize `service.rs`: add `generate_digest_service()` (ExecStart `digest`,
  `EnvironmentFile=-%h/.config/eratosthenes/digest.env`) and
  `generate_digest_timer(schedule)` (`OnCalendar` from config, `Persistent=true`).
- The OnCalendar value comes from `slack.schedule` (default `Mon-Fri 08,13:00:00`).
  The digest is a single timer running one `eratosthenes digest`; if multiple
  slack-enabled accounts specify different schedules, install uses the first and
  warns. Validate the OnCalendar string before writing the unit.
- Install the digest units ONLY if at least one discovered account has a `slack`
  block; otherwise skip them (and remove on uninstall) so the timer never fires a
  no-op binary. The run units install unconditionally as today.
- Collect the DISTINCT `token-env` names across all slack-enabled accounts; for
  each that is set in the install-time environment, write a line into `digest.env`
  (mode 600). Warn for any referenced env var that is unset. Do not assume a single
  fixed token env var name (default is `SLACK_XOXP_TOKEN`).
- Extend `install/uninstall/reinstall/status` to manage both unit sets.

#### Phase 5: Tests, docs, ship
**Model:** sonnet
- Smoke test that `digest` parses/dispatches.
- Update README and add the `slack:` block to the example config.
- `otto ci`; `bump`; `cargo install --path .`; reinstall units.

## Alternatives Considered

### Alternative 1: Fold the digest into `run` with a time gate
- **Description:** Post inside the 5-minute `run` when the wall clock is near
  08:00 or 13:00.
- **Pros:** No second timer; one binary path.
- **Cons:** Couples two concerns; fragile clock math in a tight loop; risk of
  double-posts or missed windows; harder to test.
- **Why not chosen:** A dedicated `OnCalendar` timer is exactly the tool for a
  fixed schedule and keeps `run` single-purpose.

### Alternative 2: External cron + `gws` + `curl`
- **Description:** A shell script queries Gmail via the `gws` CLI and curls Slack.
- **Pros:** No Rust changes.
- **Cons:** Logic lives outside the app that owns the email model; duplicates
  auth/query; brittle.
- **Why not chosen:** User explicitly wants the capability inside eratosthenes.

### Alternative 3: Incoming webhook instead of bot token
- **Description:** Post via a channel-bound webhook URL.
- **Pros:** Simplest auth (one URL).
- **Cons:** Cannot target a personal DM; one channel per URL.
- **Why not chosen:** Destination is your self-DM, which a channel-bound webhook
  cannot target. A user token (`xoxp`) + `chat.postMessage` posts there as you.

## Technical Considerations

### Dependencies
- No new crates. The Slack client reuses `hyper` + `hyper-rustls` (already used for
  Gmail) and `serde_json` for the request/response body. No `reqwest`, no
  `async-trait` (native async-fn-in-trait). This also avoids a second rustls crypto
  provider competing with the `aws-lc-rs` default that `init_tls()` installs.
- Reused: `tokio`, `serde`, `serde_yaml`, `serde_json`, `chrono`, `GmailClient`.

### Performance
- Two `threads.list` calls + N `threads.get` per account, N ~ pinned thread count
  (tens). Twice a weekday. Negligible; well under Gmail quota and the existing
  rate limiter.

### Security
- Tokens are never stored in YAML; config names env vars. The systemd unit reads
  them from `digest.env` (mode 600, owner-only), which may hold more than one token
  line if accounts use different `token-env` names. Flagged as the secret(s) at
  rest; acceptable and standard for `EnvironmentFile`.

### Behavior and Edge Cases
- **Empty pinned set:** post a single positive line (`Inbox clear - 0 starred, 0
  important :giga-claude:`) rather than skipping. The twice-daily post then also
  serves as a liveness signal that the digest ran.
- **Slack post failure:** return `Err` so the oneshot service shows `failed` in
  `systemctl --user status` (same observability contract as `run`). The
  `chat.postMessage` `ok:false` `error` string is surfaced in the message.
- **Missing token at runtime** (`digest.env` absent/var unset):
  `HttpSlackPoster::from_env` returns a clear error naming the env var; the
  service fails visibly rather than silently posting nothing.
- **Per-thread fetch failure:** skip that thread id with a `warn!` and continue
  (the pattern `get_thread` already uses for malformed messages); one unreadable
  thread never sinks the whole digest.
- **Multi-account errors:** accounts are processed sequentially and independently;
  a Gmail or Slack failure on one is logged and collected, the loop continues, and
  the command returns `Err` only at the end if any account failed - one bad token
  never poisons the others' digests.
- **No slack-enabled accounts:** `service install` does not lay down the digest
  units at all (run units unaffected), so nothing is scheduled to no-op.
- **`is:important` semantics:** Gmail's `is:important` reflects *Gmail's* current
  importance marker, which includes both eratosthenes' `Flag` action and Gmail's
  own importance classifier. The digest reports what Gmail considers important
  now; it is not limited to mail eratosthenes flagged. Documented so the count is
  not surprising.
- **Schedule timezone:** `OnCalendar` uses the system local timezone, so
  `08:00`/`13:00` are local (PDT) as intended. `Persistent=true` means a window
  missed while the machine was asleep fires on resume (a possibly-late post),
  which is preferred over silently skipping.
- **Message length:** `chat.postMessage` accepts a large `text` field (hard cap
  ~40,000 chars, not the 3,000-4,000 figure that applies to Block Kit text objects
  - we send plain `text`, not blocks). For readability, not just the API limit,
  `format` enforces an explicit budget of 3,500 chars: when exceeded, the longer
  section is truncated with a trailing `... +N more` line. Header counts stay
  exact. (Ceiling and budget confirmed in Architect review, 2026-06-06.)
- **Signature (required):** every digest message ends with `:giga-claude:` on its
  own last line - the normal digest AND the empty-set message. Because the user
  token posts AS you, this signature is what marks the post as automated, so it
  must never be dropped. `format` always appends it and a unit test asserts its
  presence (per `~/repos/.claude/refs/slack.md`).

### Testing Strategy
- `digest::format` unit-tested against fixtures (pure function).
- Slack client tested via `FakeSlackPoster` capturing the posted text.
- Smoke test: binary runs `digest --help` / dispatch.

### Rollout Plan
- Ship binary, add `slack:` block to `tatari.yml`, run
  `eratosthenes service reinstall` to lay down both unit sets, verify with a
  manual `eratosthenes digest`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| systemd user service lacks the Slack token in env | High | High | `EnvironmentFile=-.../digest.env`; install captures every referenced `token-env` (600) |
| Wrong Slack channel/user id -> post fails or wrong place | Med | Med | Validate `chat.postMessage` `ok`; surface `error`; manual test first |
| Gmail deep link account index wrong | Low | Low | Per-account `slack.browser-index` config (default 0) |
| Pinned set large -> Slack message length limit | Low | Med | Explicit 3,500-char budget; per-section truncation with `... +N more` (see Behavior) |

## Open Questions
- [ ] (resolved) Delivery: user token (`SLACK_XOXP_TOKEN`) posting to the self-DM
      channel `D01G4Q7AWLV` as a note-to-self. No bot member-id or bot scopes needed.
- [ ] Confirm `SLACK_XOXP_TOKEN` carries the `chat:write` scope (verified at first
      post; the `ok:false` error is surfaced clearly if the scope is missing).
- [ ] (resolved) Schedule is config-driven via `slack.schedule` (OnCalendar),
      default `Mon-Fri 08,13:00:00` - no CLI flag.
- [ ] (resolved) HTTP/TLS: reuse the existing `hyper` + `hyper-rustls` stack; do
      not add `reqwest` or any new dependency.

## References
- `src/engine.rs`, `src/gmail/client.rs` - reused query/auth path
- `src/service.rs` - existing unit generation to generalize
- `~/repos/.claude/refs/slack.md` - mrkdwn, signing, ID reference conventions
