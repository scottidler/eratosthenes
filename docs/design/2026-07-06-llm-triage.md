# Design Document: LLM Triage Layer

**Author:** Scott Idler
**Date:** 2026-07-06
**Status:** Implemented
**Review Passes Completed:** 5/5 (2026-07-06, against v0.2.11)
**Amended:** 2026-09-06 against v0.3.0, in three passes.
Pass 1 (pre-panel): Phase 6 rescoped from a one-line summary to 3-7 bullets,
API key var corrected, all line citations rebased.
Pass 2 (post-panel round 1, all 5 must-fix addressed): the LLM transport
became the KEYLESS `claude` CLI, bucket labels renamed `triage/*` -> `llm/*`,
`BUDGET` raised 3500 -> 10000 with a stated 80-char per-bullet cap, the
unsatisfiable 30-thread acceptance criterion replaced by TWO criteria (a
reachable 10-thread MIXED one for rendering, a deliberately over-budget one for
the shrink ladder), `DigestItem` gained a typed `ask` field, and the
Anthropic-outage risk row was split by path.
Pass 3 (post-panel round 2): the `claude` argv gained the hardening flags that
make the blast-radius claim true (`--tools ""`, `--safe-mode`,
`--strict-mcp-config`, `--no-session-persistence`, `--max-turns 1`), plus a
version floor, a subprocess timeout with kill-and-reap, stdin payload framing,
and structured failure classification.
Round-2 findings: /tmp/review-panel/triage-r1/synthesis-r2.md
Round-1 findings: /tmp/review-panel/triage-r1/synthesis.md
Re-review REQUIRED: the 5/5 above covers NONE of this.

## Summary

Add an LLM classification pass (`eratosthenes triage`) in front of the existing
deterministic aging engine. The LLM buckets new inbox threads via Gmail labels;
the existing state-filter machinery ages each bucket on its own TTL. Digest
gains 3-7 bullets per pinned thread and a Needs Reply section; needs-reply threads get a
human-in-the-loop reply draft in Gmail Drafts. Nothing is ever sent.

## Problem Statement

### Background

- eratosthenes (v0.3.0; this doc was originally written against v0.2.11 and
  every line citation below was rebased 2026-09-06) is a purely deterministic
  inbox-zero engine:
  glob-based message filters (Star | Flag | Move) + TTL thread aging
  (INBOX -> Purgatory -> Oblivion), with `ttl: Keep` protecting Starred and
  Important threads.
- It has zero semantic understanding. Nothing decides which of the ~10 daily
  emails is the one real human thread vs LinkedIn noise. Scott does that
  manually, in Gmail's UI, which he hates.
- Digest (Mon,Thu -> Slack self-DM) is a bare list of Starred/Important
  threads: no summaries, no reply help.
- Work does not require email daily, but it cannot be ignored entirely. The
  target state: email comes to Scott as a triaged digest; Gmail is touched
  only via deep links for the 2-3 threads that matter.

### Problem

New inbox mail lands untriaged. The aging engine can only apply one blanket
TTL policy to unprotected inbox threads because nothing assigns meaning to
them. Real human threads and marketing noise ride the same rail.

### Goals

Every goal traces to Scott's request (session 2026-07-06, "draft the
triage-agent design", accepting the proposal in the same conversation):

- LLM classifies each new inbox thread into buckets:
  needs-reply | fyi-work | recruiting | receipts | noise.
- Buckets land as Gmail labels; the existing aging engine ages each bucket on
  its own TTL, with needs-reply protected (`ttl: Keep`).
- Digest gains a Needs Reply section and 3-7 LLM bullets per pinned thread,
  with any ask on Scott rendered as a marked FIRST bullet (amended 2026-09-06;
  originally a one-line summary).
- needs-reply threads get a reply draft in Gmail Drafts, written against
  Scott's voice profile. Draft, never send: human-in-the-loop.
- Scheduled + unattended on desk.lan. No third-party OAuth grants on work
  mail: existing eratosthenes token only. The LLM call shells out to the
  locally installed `claude` CLI, which owns its own auth, so this design
  holds NO Anthropic credential (amended 2026-09-06; originally "Scott's own
  Anthropic API key").

### Non-Goals

- Sending mail. Excluded permanently: the binary has no send path.
- Unsubscribe automation. Excluded (not requested).
- Home persona account. Parked: config is per-account by construction;
  revisit when a home eratosthenes account exists.
- Real-time triage (Gmail push / `+watch`). Parked: revisit if the daily
  cadence proves too slow.
- Re-triage of threads with NO new messages. Excluded: new inbound messages
  already re-trigger classification (message-level `llm/seen`, below);
  reclassifying idle threads buys nothing.
- Replacing the Gmail UI. The digest deep-links into it; that is the contract.

## Proposed Solution

### Overview

Extend eratosthenes itself (Option B) rather than wrapping a headless agent
around it. Rationale, in taste order:

- Copy the proven in-house pattern: clyde shells out to the locally installed
  `claude` CLI in headless print mode, KEYLESS, on a systemd user timer
  (`clyde/common/src/llm/cli.rs`, `clyde-enrich.service`).
  **CORRECTED 2026-09-06 (panel M4): this doc originally cited clyde as the
  Rust -> Anthropic-over-HTTP-with-a-key precedent, at a path
  (`clyde/open/report/src/summarize.rs`) that does not exist. Clyde DELETED
  that approach.** `clyde/docs/design/2026-07-29-excise-api-key.md`
  (Status: Implemented, three weeks after this doc was drafted): "After this,
  clyde reads, stores, and transmits no credential: the locally installed
  `claude` binary owns auth end to end, for every LLM call in the workspace."
  `clyde/common/src/llm/cli.rs:429` states clyde "handles no key at all and
  must never forward one to the child". Verified on desk 2026-09-06:
  `clyde-enrich.service` carries NO `EnvironmentFile` line and finished clean
  that morning (`Sep 06 03:04:56 ... Finished clyde-enrich.service`, 37s).
  So the in-house precedent moved to the OPPOSITE approach, and it is proven
  unattended under systemd on this host.
- eratosthenes already owns most of the machinery: full-scope Gmail OAuth
  (`src/gmail/auth.rs:9`), thread fetch/modify, label plumbing, systemd unit
  generation (`src/service.rs:97-161`), secret delivery
  (`service.rs:204-236`), digest posting, per-account config. The two real
  gaps the panel surfaced -- Move semantics for labeled filters and
  body/MIME plumbing -- are scoped as explicit phases, not assumed away.
- Fail closed by construction: a binary with no send path beats an agent one
  allowlist mistake away from `gws gmail +reply` (which sends immediately).
- Deterministic control flow, LLM only where judgment is needed. Idempotency,
  dry-run, and tests live in Rust, not in a prompt.
- Decompose along change frequency: the tunable surface (bucket taxonomy,
  bucket descriptions that become the classifier prompt, voice-profile path,
  models, TTLs) is config; the engine and the prompt scaffolding (JSON-schema
  instructions, draft template) are Rust. Tuning never rebuilds the binary;
  template overrides are parked until wanted.

### Architecture

Three timers, one binary. Data flow:

```
triage.timer (new, OnCalendar from config -- REQUIRED, no default)
  -> eratosthenes triage [account]
     -> gmail: messages.list q="in:inbox -label:llm/seen"  (capped,
        newest first) -> distinct thread ids -> threads.get
     -> anthropic: one batched classify call -> {thread-id -> bucket}
     -> gmail: threads.modify: remove any previous llm/* bucket label,
        add new bucket label + llm/seen
     -> needs-reply refresh: query in:inbox label:llm/needs-reply
        (each run, cheap -- few threads):
          answered (newest msg from Scott) -> remove llm/needs-reply
          no draft in thread + newest msg inbound -> anthropic draft body
            -> gmail: drafts.create (threaded RFC822, built in Rust)

run.timer (existing, 5min)
  -> aging engine, unchanged code; new state-filters in tatari.yml:
     llm/needs-reply -> Keep; llm/noise -> short TTL; etc.

digest.timer (existing, Mon,Thu)
  -> digest: sections Needs Reply | Starred | Important
     -> anthropic: 3-7 bullets per pinned thread, ask-first when there is an
        ask (generated at digest time, stateless -- nothing stored between runs)
```

Interplay with the aging engine, verified in the research pass and corrected
by the review panel:

- Keep semantics are label-driven (`src/cfg/state.rs:116`,
  `evaluate_thread` `src/engine.rs:843`); protection is thread-level. `llm/needs-reply`
  Keep is genuinely config-only.
- Cull matches ALL inbox threads (`evaluate_thread`, `src/engine.rs:843`), so bucket labels
  MUST get their own state-filter entries or they age on the default rail.
  Each bucket's TTL is a config knob.
- **Panel finding (verified): TTL'd bucket filters are NOT config-only.**
  `apply_state_action` (`src/engine.rs:905`) builds the Move remove-set
  from the filter's OWN labels, only falling back to `INBOX` when the filter
  has none. **RE-VERIFIED 2026-09-06 against v0.3.0: still true, unchanged.** A `llm/noise -> Purgatory` filter would add
  Purgatory, strip `llm/noise`, and leave the thread in INBOX forever.
  Today's semantics exist for stage transitions (match `Purgatory` -> Move
  `Oblivion` removes Purgatory -- correct because the match label IS a stage
  label). Fix (Phase 2): on Move, remove `INBOX` plus any current STAGE
  labels (`derive_stages`, `src/engine.rs:159`); match labels are criteria
  only, never removed. Behavior-preserving for every existing filter shape
  (stage-transition and bare-Cull), correct for bucket labels, and the bucket
  label survives as searchable provenance.
- `sanitize_stages` (`src/engine.rs:175`) only strips stage labels;
  `llm/*` labels are untouched.

### Data Model

New `triage:` block in the per-account YAML (lives in dotfiles like the rest
of `tatari.yml`). Repo ships ONE annotated example; values below are
illustrative defaults:

```yaml
triage:
  # NO api-key-env. This design is KEYLESS by construction: the LLM call
  # shells out to the locally installed `claude` CLI, which owns auth. See the
  # transport decision in Resolved Decisions (2026-09-06).
  claude-binary: ~/.local/bin/claude      # optional; resolved on PATH if unset
  schedule: "Mon..Fri 06:30:00"          # OnCalendar; required if block present
  max-threads: 50                        # per-run cap; hitting it logs LOUDLY
  body-chars: 4000                       # per-THREAD char budget, newest messages first
  classify-model: claude-haiku-4-5-20251001
  draft-model: claude-sonnet-5
  voice-profile: ~/Claude/writing/VOICE.md   # draft prompts only
  buckets:
    - name: needs-reply
      label: llm/needs-reply
      description: a real human wrote to Scott and expects a reply or action
      draft: true
    - name: fyi-work
      label: llm/fyi-work
      description: work-relevant notifications -- AWS, security, CI, vendors
    - name: recruiting
      label: llm/recruiting
      description: LinkedIn, recruiters, job alerts
    - name: receipts
      label: llm/receipts
      description: invoices, payments, renewals, order confirmations
    - name: noise
      label: llm/noise
      description: everything else -- marketing, newsletters, social
```

- Buckets are config, not code: names, labels, and the descriptions that
  become the classifier prompt all live here. Adding a bucket = YAML edit.
- Each bucket label gets a matching `state-filters` entry in the same file
  (existing schema): `llm/needs-reply` -> `Keep`, `llm/noise` -> short
  TTL, etc. One file, self-consistent.
- Idempotency marker: `llm/seen`, deliberately MESSAGE-level. Gmail labels
  attach to messages and new messages never inherit them, so
  `messages.list q="in:inbox -label:llm/seen"` finds both brand-new
  threads AND new inbound messages landing in already-seen threads. A noise
  thread that a real human replies into re-enters classification and gets its
  bucket label replaced (noise -> needs-reply is possible). Scott's own
  replies carry SENT, not INBOX, so they never re-trigger. Reruns with no new
  messages are no-ops.
- Cap order: candidates are sorted client-side by `internalDate` descending
  (no reliance on undocumented `messages.list` ordering); when `max-threads`
  bites, newest threads win, the remainder stays unseen and is picked up next
  run, and the cap logs loudly (no silent truncation).
- Mutation atomicity: one `threads.modify` per thread
  (removeLabelIds = previous `llm/*` bucket, addLabelIds = new bucket +
  `llm/seen`), applied per-thread after the batch classification returns.
  A thread is never half-mutated (single API call); process death mid-run
  leaves the remaining threads unseen, retried next run.
- Archived needs-reply edge: a thread Scott archives before the draft fires
  leaves INBOX and exits the `in:inbox label:llm/needs-reply` refresh
  query. Expected -- archiving IS the dismissal signal, same family as the
  stale-draft consequence above.
- Classifier contract: one batched call; JSON out
  `{"threads": [{"id": "...", "bucket": "needs-reply"}]}`. Exactly one bucket
  per thread; noise is the catch-all. Schema mismatch OR a bucket name not in
  config -> one retry -> skip thread with a loud log (thread stays unseen,
  retried next run; nothing half-written). Returned thread-ids must be a
  subset of the requested batch; unknown ids are ignored with a loud log.
- Labels: the engine ensures `llm/*` labels exist at run start
  (`labels.create`, idempotent, fail loudly). No operator label step.
- Draft dedup: the thread fetch already returns per-message labels; any
  message carrying `DRAFT` -> skip. Existing drafts are NEVER modified or
  deleted -- Scott may have edited them; his edits are sacred. Consequence:
  a draft goes stale if the counterparty replies again; Scott sees the newer
  message in the same thread when he reviews. Accepted.
- `llm/seen` deliberately gets NO state-filter: once a needs-reply thread
  is answered (bucket label removed), it rides the default Cull rail and ages
  away like anything else.

### Data Plumbing

Panel finding (verified): the current Gmail client cannot feed an LLM.
`get_thread` fetches `format("metadata")` only (`src/gmail/client.rs:224`,
the `.format("metadata")` call at `:232`), `GmailMessage` carries no body
(`src/gmail/message.rs:8-18`), and the default header set is
To/Cc/From/Subject. **RE-VERIFIED 2026-09-06 against v0.3.0: still true.** Triage,
summaries, and drafts all need more:

- Candidate threads are re-fetched `format("full")` (metadata stays the
  default everywhere else -- the aging engine is untouched).
- Body extraction: MIME walk preferring `text/plain`, html->text fallback,
  quoted-reply stripping, truncated at `body-chars` per thread (newest
  messages first).
- Header set extended with Message-ID, In-Reply-To, References, Reply-To,
  Date -- the RFC822 builder's inputs.
- Self-detection: `From` matched against the account's authenticated address
  (drives the answered-rule).

Scoped inside Phase 4 (triage engine) and consumed by Phases 6-7.

### API Design

```
eratosthenes triage [accounts...] [--dry-run]
```

- `--dry-run`: full classify pass, prints thread -> bucket table, zero
  mutations. Same flag semantics as the existing engine.
- LLM calls: **subprocess to the locally installed `claude` CLI in headless
  print mode. No HTTP client, no `ureq` dep, no credential.** Shape mirrors
  clyde's transport (`clyde/common/src/llm/cli.rs`):
  `claude -p <prompt> --model <model> --output-format json`, parse the JSON
  envelope, tolerating leading noise on stdout (an npm-installed Claude Code
  can print an update notice ahead of the JSON -- clyde hit exactly this,
  `cli.rs:474`; the belt is the `NO_UPDATE_NOTIFIER` env var below).
- **The hardening flags are MANDATORY and are the whole basis of the
  blast-radius claim in Security. Panel finding 2026-09-06 (R2-M2): an earlier
  version of this section said the shape "mirrors clyde's transport" while
  copying only clyde's argv SHAPE and none of its hardening.** Verified against
  the installed `claude` 2.1.263: `--tools` defaults to ALL built-in tools and
  `--safe-mode` is opt-in, so the un-hardened argv would process an adversarial
  email body as a full Claude Code session on this host, with tools live and
  this machine's CLAUDE.md, skills, hooks and MCP servers loaded. Copy clyde's
  argv (`clyde/common/src/llm/cli.rs:99-130`), each flag with its reason:

  | flag | why |
  |---|---|
  | `--tools ""` | Disables ALL built-in tools structurally. Deletes tool-list drift as a risk CLASS rather than mitigating it: nothing is enumerated, so nothing can drift. **This is the flag that makes the child a pure data transformer.** |
  | `--safe-mode` | No CLAUDE.md, skills, plugins, hooks, MCP, or agents; auth preserved. A temp cwd is NOT a substitute -- clyde measured that cwd only defeats PROJECT CLAUDE.md discovery while user and global customizations still load. |
  | `--strict-mcp-config` | No MCP servers from any config file. |
  | `--no-session-persistence` | Writes nothing to disk, so a triage run never becomes a session that `clyde` then catalogs. |
  | `--max-turns 1` | One turn. Accepted but UNDOCUMENTED as of 2.1.219, which is exactly why the version floor below exists. |

  Deliberately NOT passed, both recorded so a reader does not "restore" them:
  - `--fallback-model`, so the CLI cannot silently swap the model out from
    under a pinned choice.
  - `--system-prompt`, which clyde DOES pass (`cli.rs:110-111`). Clyde's stated
    reason (`cli.rs:108-109`) is to keep its API and CLI transports sending
    identical instructions. This design has no API path and its instruction
    rides `-p`, so the flag buys nothing here. No security property depends on
    it. (Panel finding 2026-09-06, R3-M5: an earlier version said "copy clyde's
    argv EXACTLY" over a line range that included `--system-prompt`, which made
    a false statement about code in the section Security leans on.)

  Implementation note, verified against clyde's own test
  (`cli/tests/argv.rs:26-35`): `--tools ""` is TWO argv elements, `--tools`
  followed by an empty string. Not one joined token.
- **Payload rides stdin as a FILE, not argv, and not a pipe.**
  `max-threads: 50` x `body-chars: 4000` is ~200KB, which fits this host's
  2097152 `ARG_MAX`, so an ARG_MAX justification would be both wrong and
  fragile. The real reason is deadlock, and the mechanism matters: clyde wires
  the child's stdin/stdout/stderr to TEMP FILES (`clyde/common/src/proc.rs:121-129` -- note it is NOT under `llm/`) precisely
  so that "no pipe exists, so no pipe can fill and no drain can deadlock". Say
  "stdin" without saying "file" and a reader implements a pipe, which is the
  exact deadlock being avoided. The fixed instruction rides argv; the thread
  bodies ride a temp file on stdin. (Panel finding 2026-09-06, R3-M6: an
  earlier version cited `cli.rs:100-101` for the deadlock rationale -- that
  line is the ARG_MAX comment this bullet repudiates.)
- **Version floor, logged not gated** (clyde's `MIN_CLAUDE_VERSION`,
  `cli.rs:32-42`): the argv above depends on five flags, one of them
  undocumented. Record a minimum version, log the resolved version on every
  call, and name it in every failure, so an unsupported-flag exit reads as
  "your claude is older than the floor" rather than as a mystery. Do NOT
  pre-flight gate on a parsed version string: the format is foreign and a
  brittle parse would fail closed on a CLI that actually works.
- **Timeout with kill AND reap.** `claude -p` can hang; nothing in this design
  bounds it today. Clyde uses a 900s `CLAUDE_TIMEOUT` and the `wait-timeout`
  crate (`common/Cargo.toml:35`). eratosthenes has no such dep, so this is a
  NEW DIRECT DEP or an equivalent tokio timeout on the child -- the Dependencies
  section names the choice. Kill is not sufficient on its own: reap, or the
  timer accumulates zombies.
- **Classify subprocess failures structurally**, not as one opaque error:
  auth-expired, rate-limited, and transport failures read differently to an
  operator and only the first is silent-and-permanent (see Risks).
- **The child environment is BUILT, not inherited: `env_clear()` then an
  explicit allowlist.** Copy clyde's reasoning verbatim (`cli.rs:420-434`),
  because it is a measured secret-exposure bug and not tidiness: a live agent
  session on this host carries 13 `CLAUDE*` variables, three of which are
  secrets (`CLAUDE_COST_ANTHROPIC_API_ADMIN_KEY` and two Slack tokens). An
  inherit-by-default child would hand an Anthropic ADMIN key to every triage
  run. Explicitly excluded: every `ANTHROPIC*` var (this design forwards no
  key, ever) and the `CLAUDE_CODE_*` session vars (a run must not present
  itself to the child as a nested session). **Explicitly INCLUDED:
  `NO_UPDATE_NOTIFIER=1`** (`cli.rs:476`, its comment at `:474`) -- an npm-installed Claude Code
  otherwise prints an update notice ahead of the JSON. That env var is the
  belt; the tolerant envelope parse below is the suspenders. An earlier version
  of this doc adopted the suspenders and cited the belt (panel R3-C1).
- Binary resolution: `claude-binary` from config if set, else PATH. **A
  systemd unit's PATH is not your shell's** -- the generated units already pin
  `Environment=PATH=...` (`src/service.rs`), so the `claude` install location
  must be on it or resolution fails under the timer while passing
  interactively. Phase 0 proves this, Phase 5 wires it.
- Drafts: `users.drafts.create` through the existing `google-gmail1` dep with
  `{raw: <base64url RFC822>, threadId}`. Rust builds the MIME: `In-Reply-To`
  + `References` from the original headers, `Re:` subject,
  `To:` = Reply-To || From of the newest inbound message, no Cc (Gmail
  "Reply" semantics -- Scott adds recipients when he reviews). Threading
  correctness is Phase 0 material, not assumed.
- `service install|reinstall` grows a third unit pair (triage.service/.timer)
  generated exactly like the digest pair, `OnCalendar` validated via
  `systemd-analyze calendar`. Timer shape mirrors the digest precedent
  (`generate_digest_timer`, `src/service.rs:147`): installed when >= 1 account has a `triage:` block,
  schedule read from the first triage-enabled account, and one fired run
  triages every triage-enabled account. **No `triage.env` and no secret
  delivery: the transport is keyless.** What the unit DOES need is a PATH that
  resolves `claude` -- see the binary-resolution bullet above.

### Implementation Plan

#### Phase 0: Prove the environmental assumptions -- zero code
**Model:** sonnet
- (a) Create a threaded reply draft by hand (`gws gmail users drafts create`
  with base64url RFC822 + threadId) against a real thread; verify in Gmail UI
  that it threads correctly and In-Reply-To is honored; delete it.
- (b) Confirm a plain API get (`threads.get`, the call the engine will use)
  does not mark mail read: observe a thread's UNREAD label before/after.
- (c) **Prove the keyless CLI transport works UNDER SYSTEMD, from a unit's
  own PATH and environment -- not from an interactive shell.** This replaces
  the doc's original "confirm the API key is hydrated" step, which the
  2026-09-06 transport decision deleted. Partial evidence already in hand:
  `clyde-enrich.service` runs `claude`-backed work on a user timer with NO
  `EnvironmentFile` and finished clean on 2026-09-06. What is NOT yet proven
  for eratosthenes: that `claude` resolves on the PATH the GENERATED unit pins
  (`Environment=PATH=%h/.cargo/bin:/usr/local/bin:/usr/bin:/bin`, which does
  NOT include `~/.local/bin`) and that an `env_clear()`ed child with the
  allowlist still authenticates. Run a throwaway
  `systemd-run --user --wait --pipe` with exactly the generated unit's PATH
  and **the full production argv**, not a bare `claude -p`. Phase 0c is the
  only point before Phase 4 that can prove the installed CLI accepts every
  hardening flag -- and one of them, `--max-turns`, this doc itself records as
  "accepted but UNDOCUMENTED as of 2.1.219", i.e. exactly the flag most likely
  to have moved. A spike that proves a two-flag argv works tells you nothing
  about the seven-flag argv the design ships (panel finding 2026-09-06,
  R3-M7).
- (d) Pin down google-gmail1 7.0.0's actual send-call surface (builder names
  and terminal call) and record the guard pattern the Phase 7 no-send test
  will match -- panel finding: a naive `messages_send` grep may never match
  the real call shape.
- **Success criteria:** draft appears inside the target thread in Gmail UI;
  UNREAD survives a read; a `systemd-run --user` invocation with the generated
  unit's exact PATH and an `env_clear()`ed allowlist environment, running the
  FULL production argv (`-p`, `--model`, `--output-format json`, `--tools ""`,
  `--safe-mode`, `--strict-mcp-config`, `--no-session-persistence`,
  `--max-turns 1`), returns a parsed envelope and exit 0, and the resolved
  `claude --version` is recorded as the version floor (and the failure
  mode is recorded if `claude` does NOT resolve on that PATH, since that is
  the likely outcome and it dictates the Phase 5 wiring); **the wall-clock
  elapsed time of that ONE call is recorded as a SINGLE-CALL BASELINE and
  nothing more** -- it does not confirm either timeout, because one call
  bears on neither a 50-thread triage run nor a 10-thread digest pass
  (panel R5-C1: an earlier version had this gate "confirm or amend" both
  values, which is a conclusion its measurement does not license). Confirmation
  of both values happens in Phase 4 against real runs; the guard pattern is
  written down and shown to match a sample send invocation compiled against
  the crate.

**Phase 0 results, observed 2026-09-06 against v0.3.0 (host: desk.lan):**

- **(a) BLOCKED, not failed.** The draft-create probe was refused by the local
  agent permission layer, not by Gmail. No API call was made. This gates
  Phase 7 ONLY; phases 1-6 do not depend on it. Must be run before Phase 7.
- **(b) PASS.** `threads.get format=full` does NOT clear UNREAD. Thread
  `1a0797a6bf20de2e` read `["UNREAD","CATEGORY_FORUMS","INBOX"]` before and
  the identical set after two consecutive full gets.
- **(c) RESOLVE-FAIL, exactly as this phase predicted, and it dictates Phase 5.**
  `claude` is installed at `/home/saidler/.local/bin/claude`, which is NOT on
  the generated unit PATH (`src/service.rs:106,139` pin
  `{cargo_bin}:/usr/local/bin:/usr/bin:/bin`). Under
  `systemd-run --user` with that exact PATH, `command -v claude` returns
  RESOLVE-FAIL. **Phase 5 must add the `claude` install dir to BOTH generated
  unit pairs' PATH; without it the timer-fired digest takes the no-bullets
  fallback 100% of the time while interactive runs pass.**
  - *Probe hygiene note:* the first attempt at this probe passed spuriously.
    `systemd-run --setenv=PATH=... claude --version` returns 0 because
    systemd-run resolves the binary from the CALLER's PATH and only then hands
    the child the pinned PATH. Resolution must be forced INSIDE the unit
    (`/bin/sh -c 'command -v claude'`) to test what `Command::new("claude")`
    will actually do. Anyone re-running this must use the inside-the-unit form.
- **(c) PASS, full production argv.** Under `systemd-run --user` with an
  `env -i` environment (allowlist: HOME, USER, PATH, `NO_UPDATE_NOTIFIER=1`;
  no `ANTHROPIC*`, no `CLAUDE_CODE_*`), the full seven-flag argv
  (`-p`, `--model claude-haiku-4-5-20251001`, `--output-format json`,
  `--tools ""`, `--safe-mode`, `--strict-mcp-config`,
  `--no-session-persistence`, `--max-turns 1`) returned exit 0 and a parsed
  JSON envelope: `"result":"ok"`, `"is_error":false`, `"num_turns":1`,
  `"permission_denials":[]`, `subagent_stats.spawned:0`. Every hardening flag
  is accepted, `--max-turns` included. The `env -i` child authenticated with
  no key present, confirming the keyless transport.
  - **Version floor recorded: `claude` 2.1.263.**
  - **SINGLE-CALL BASELINE: 2.87s wall clock** (unit runtime 2.847s, of which
    `duration_api_ms` 2042). This is one call and nothing more: it confirms
    NEITHER the 300s triage timeout NOR the 120s digest timeout, both of which
    Phase 4 confirms against real runs.
- **(d) PASS, and it CORRECTS this doc's stated assumption.** Resolved crate is
  `google-gmail1 7.0.0+20251215`. The claim that "a naive `messages_send` grep
  may never match the real call shape" is **disproven by measurement**:
  `messages_send(` is the real shape, at `api.rs:2130`. The actual hazard is
  different and worse: **a SECOND sender, `drafts_send(` at `api.rs:1733`**,
  which is the adjacent call that would send the very drafts Phase 7 creates.
  A guard greping only `messages_send` misses it. Terminal call is `.doit()`
  (`api.rs:12086`). A naive `send` grep is unusable: 257 matching lines in the
  crate, dominated by `settings_send_as_*` alias settings that send nothing.
  - **Guard pattern for Phase 7, validated:** `\b(messages_send|drafts_send)\s*\(`
    Matches both real senders in the crate; returns rg exit 1 (zero matches)
    against `src/` on main. Note this host's ripgrep has **no PCRE2**, so the
    guard test must use the default regex engine, not `--pcre2`.


#### Phase 1: Config schema + validation
**Model:** sonnet
- `triage:` block structs in `src/cfg/` (serde), wired into `Config`;
  `config validate|show` cover it. **Which fields default, explicitly:**
  `claude-binary` (unset -> resolve on PATH), `max-threads` (50),
  `body-chars` (4000), `classify-model`, `draft-model`, and the bucket list.
  **`schedule` has NO default and is REQUIRED when the block is present** --
  a missing schedule is a named config-load error, not a silent weekday
  guess.
- Update `eratosthenes.example.yml` with the annotated block.
- **Success criteria:** `eratosthenes config validate` passes with and
  without a `triage:` block; a bucket missing `label` exits nonzero naming
  the offending bucket and field.

#### Phase 2: Aging-engine Move semantics for labeled filters
**Model:** opus
- The panel-verified blocker (`apply_state_action`, `src/engine.rs:905`,
  re-verified 2026-09-06 against v0.3.0): change
  `apply_state_action` Move to remove `INBOX` + current stage labels
  (`derive_stages`); match labels become criteria only, never removed.
- **Signature/data-flow change the doc previously omitted** (panel finding
  2026-09-06): `apply_state_action` does not currently receive `state_filters`
  or the derived stage list, so it CANNOT compute "current stage labels"
  today. Phase 2 therefore includes threading `derive_stages`' output (or the
  filters themselves) into it. This is why Phase 2 is opus-tagged and not a
  two-line change.
- Regression tests for both existing filter shapes (stage-transition
  Purgatory->Oblivion, bare catch-all Cull) plus the new bucket-labeled
  shape; break the fix to prove each test bites.
- **Success criteria:** all three filter-shape tests pass and are
  demonstrated to fail against the old remove-own-labels behavior; a
  `llm/noise -> Purgatory` filter in a test fixture removes INBOX and
  leaves `llm/noise` intact.

#### Phase 3: Bucket labels + state-filters (config, dotfiles repo)
**Model:** sonnet
- Create `llm/*` labels in the work account (one-off `gws` calls; from
  Phase 4 on the engine ensures they exist itself); add state-filter entries
  to `tatari.yml` (needs-reply Keep; per-bucket TTLs), deploy via dotfiles.
- Ordering constraint (verified: Keep filters fire first in config order):
  all `Keep` entries precede TTL/Cull entries in the file.
- **Success criteria:** `config validate` passes; engine `--dry-run` output
  contains a `[state:age-noise]` line for a hand-labeled `llm/noise` test
  thread and a `protected by 'keep-needs-reply'` line for a hand-labeled
  `llm/needs-reply` thread (exact names per config).

#### Phase 4: Triage engine
**Model:** opus
- Data plumbing per the Data Plumbing section: `format("full")` fetch path,
  MIME walk + html->text + quote stripping, extended header set,
  self-detection.
- `messages.list q="in:inbox -label:llm/seen"` -> distinct threads sorted
  client-side by `internalDate` desc (capped at `max-threads`, loud log when
  the cap bites), bodies truncated at `body-chars` per thread, one batched
  Anthropic call, replace bucket label + apply seen (one `threads.modify`
  per thread). Idempotent; `--dry-run`.
- Eval gate: dry-run over a 50-thread sample of recent real mail
  (`newer_than:30d`, inbox + Purgatory -- the live inbox alone is aged too
  aggressively to hold 50) -> table for Scott's review; disagreements
  documented and bucket descriptions iterated until <= 5/50.
- **Success criteria:** two consecutive live runs -- first applies labels,
  second is a verified no-op (journal shows zero mutations); **the wall-clock
  duration of a full 50-thread triage run AND of a 10-thread digest bullet pass
  are both recorded, and each provisional timeout must be at least 2x its
  measured duration -- if it is not, raise the timeout in this doc and record
  the measurement that moved it.** "At least 2x measured" is the threshold;
  without one, "amended if the measurement disagrees" left "disagrees"
  undefined and the gate was unfalsifiable in substance (panel R5-C2);
  eval table
  committed at `docs/eval/llm-triage-eval.md` and signed off at <= 5/50
  disagreements.

#### Phase 5: Timer wiring
**Model:** sonnet
- Third unit pair in `service.rs` (copy digest pair; shape per API Design --
  first triage-enabled account's schedule, one run covers all enabled
  accounts), `service reinstall`. **No env file is generated for triage: the
  transport carries no credential.** The unit's `Environment=PATH=` must
  include wherever `claude` is installed (Phase 0c measures this); extend the
  generated PATH rather than assuming the current one suffices.
- **The DIGEST unit needs the same PATH treatment, and this is the fix for
  panel finding M2.** Both seats flagged, independently, that Phase 6's LLM
  call had no route to its credential from `eratosthenes-digest.service`. The
  keyless transport deletes the credential, but it does NOT delete the
  underlying hazard, it MOVES it: the digest unit must resolve `claude` on its
  own pinned PATH. If it cannot, a timer-fired digest silently falls into the
  Phase 6 no-bullets fallback 100% of the time while an interactive run passes
  (it inherits your shell PATH). Same silent-under-timer-only shape, different
  cause. Phase 5 regenerates BOTH unit pairs' PATH, and Phase 6's live check
  must be run from a TIMER FIRE, never from an interactive invocation.
- **Success criteria:** `systemctl --user list-timers` shows the triage
  timer; a timer-fired run lands labels and exits 0 in **under 90s** at the
  50-thread cap (journal-verified) -- **90s is a TRIAGE performance TARGET,
  inside the 300s TRIAGE transport-timeout CEILING settled in Open Questions.
  It was 120s until panel R5-B3 pointed out that 120 was simultaneously the
  DIGEST transport timeout, so one number was naming two unrelated things
  across two subsystems;** `systemctl --user cat` on BOTH the triage
  and digest services shows a PATH containing the `claude` install dir, and a
  timer-fired digest (not an interactive one) produces bullets rather than the
  fallback banner.

#### Phase 6: Digest enrichment
**Model:** opus
- `DigestItem` gains `ask: Option<String>` AND `bullets: Vec<String>`
  (both empty == no LLM data). **The ask is a SEPARATE TYPED FIELD, not the
  first element of `bullets` carrying an `*Action:*` prefix.** Panel finding
  2026-09-06 (M3): with bare strings the shrink ladder would have to re-parse a
  presentation prefix back out of the text to know which bullet it must not
  drop, which makes a rendering detail load-bearing for a correctness rule.
  Rendering applies the marker; the ladder reads the field. Needs Reply
  section (`in:inbox label:llm/needs-reply`) ahead of Starred | Important;
  a thread appears exactly once, in its highest section
  (Needs Reply > Starred > Important); bullets generated at digest time,
  stateless.
- **Bullets, not a one-liner. This SUPERSEDES the doc's original
  `summary: Option<String>`, <= 100 chars.** Scott, 2026-09-06: "I want to add
  some bulleted details under each of these items ... short phrases|sentences
  that describe the point of the email and especially if I am asked to do
  something (that should be the first bullet)."
  - **3-7 bullets per thread, and the ask is NOT one of them.** `ask` is a
    separate typed field (see below), so an ask-bearing thread renders its ask
    line PLUS 3-7 bullets, i.e. up to 8 rendered lines. Stated because the
    budget arithmetic assumes 7 bullet lines per thread and an ambiguous
    reading makes every fixture off by one per ask-bearing thread
    (panel R5-C3). Rendered under the digest line as Slack `mrkdwn` list items,
    indented so the thread line stays scannable.
  - **EVERY pinned thread gets bullets**, in all three sections, not just
    Needs Reply (Scott, 2026-09-06: "bullets for every").
  - **The ask goes FIRST when there is one**, and must be visibly marked as an
    ask rather than blending into the summary: the first bullet is prefixed
    (e.g. `*Reply needed:*` / `*Action:*`) so it reads differently from a
    description at a glance. Scott, 2026-09-06: "someway of telling me that I
    need to reply in the first bullet."
  - **No ask -> no marker bullet, just summary bullets.** Do NOT emit a
    "No action needed" placeholder: Scott declined it explicitly
    (2026-09-06: "no ask is just bullets as summary"). Absence of a marked
    first bullet IS the signal.
  - **Per-bullet length is capped at 80 chars**, prompt-constrained AND
    hard-truncated in Rust (same belt-and-suspenders as the original one-liner
    rule). Panel finding 2026-09-06 (R2-M3): an earlier version said
    "hard-truncated" without stating the constant, which left this phase's own
    acceptance criterion undecidable -- at 60 chars a 10x7 set is ~5900 and
    fits, at 100 it is ~8700 and does not. 80 comes from the PRODUCT need (a
    short phrase or sentence, per Scott's ask); `BUDGET` is then sized to it,
    never the reverse.
- **`BUDGET` rises from 3500 to 10000, and that is a REQUIRED part of this
  phase, not a tuning nicety.** Panel finding 2026-09-06 (M1), arithmetic
  re-derived against live digest output: a rendered thread line is 104-157
  chars in practice (the Gmail deep-link URL alone is 54,
  `https://mail.google.com/mail/u/0/#all/<16-hex>`), and the floor with a
  1-char sender and subject is 72. So at 3500:

  | pinned set | approx chars | vs 3500 |
  |---|---|---|
  | 30 threads, 0 bullets | ~3660 | already over |
  | 30 threads x 1 bullet | ~5460 | 2x |
  | 30 threads x 7 bullets | ~16260 | 5x |
  | 10 threads x 7 bullets | ~5420 | 2x |
  | 5 threads x 7 bullets | ~2710 | fits |

  An earlier draft of this amendment set the acceptance criterion at
  "30 threads x 7 bullets stays under budget AND every thread keeps its ask
  bullet". That is arithmetically unsatisfiable at ANY budget the readability
  argument would tolerate, so a CORRECT implementation would fail it. It is
  replaced above by a 10-thread mixed set. **Do not restore the 30-thread
  form.**
  `BUDGET` is self-imposed for readability, not a Slack limit -- the constant's
  own comment (`src/digest/mod.rs:5-10`) records that `chat.postMessage`
  accepts ~40k. **10000** holds a 10-thread x 7-bullet set at the 80-char
  bullet cap (~7300 incl. the `  - ` prefixes, section headers and signature)
  with ~27% headroom. Derivation, so the next person can redo it: 10 lines x
  122 chars measured + 70 bullets x 84 + fixed overhead. **Do not trust that
  back-of-envelope line: the authoritative figures are the measured sweep cited
  in AC (2), which puts fixed overhead at 85/118/151 for one/two/three
  populated sections rather than a flat 200.** Raising `BUDGET` is a one-line change
  plus the ladder below; the ladder still exists for the tail case beyond 10.
  Why 10 and not 30 is now the honest number: the monotonic pinned pile this
  doc inherited was a DEFECT, fixed by v0.3.0's act-once work. Measured
  2026-09-06 after that shipped, the live pinned set is 5 threads (2 starred,
  3 important), down from 38 starred / 9 important the same morning. 30 was a
  number from the broken world.
- **The 10000-char budget is still a binding constraint past ~13 threads, and
  the existing shrink loop is WRONG for bullets.** Today `digest::format` drops whole
  trailing ITEMS from the longer section until the body fits
  (`src/digest/mod.rs`, the `s_show`/`i_show` loop). With 3-7 bullets per
  thread, a 30-thread pinned set is ~150 lines and the loop would silently
  delete entire threads from the digest to make room. Required behavior:
  **shed bullets before shedding threads.** Degrade in this order, and pin
  each step with a test: (1) all threads keep bullets; (2) drop to the first
  N bullets per thread, N shrinking, but NEVER drop the ask bullet;
  (3) only once every thread is down to its ask bullet alone, fall back to
  the existing drop-trailing-items behavior with the `... +N more` line.
  A thread must never lose its ask bullet while another thread still shows a
  descriptive one.
- **Which config turns bullets on, stated explicitly** (panel finding M2,
  Codex's extension): the digest reads the SAME per-account `triage:` block the
  triage subcommand uses, and uses its `classify-model` for bullets (bullets
  are a summarization job, not a drafting one; `draft-model` stays Phase 7's).
  **A Slack-enabled account with NO `triage:` block posts a digest with no
  bullets and no degradation banner** -- that is the un-enriched digest working
  as designed, not a failure, and it must not be reported as one. The banner
  fires only when bullets were EXPECTED (a `triage:` block exists) and could
  not be produced.
- **Digest pin semantics changed under this doc on 2026-09-06 and Phase 6
  inherits the new shape.** The digest used to ask `in:inbox is:starred` as one
  conjunctive query, which Gmail evaluates against a SINGLE message, so a
  thread whose only star sat on a SENT reply was missed. It now queries
  `in:inbox` and `is:starred` separately and intersects by thread id, so ANY
  starred message anywhere in a thread pins the thread (Scott, 2026-09-06: "I
  want any starred on any message in a thread to consider that thread starred
  as it pertains to this system"). Same for `is:important`. Phase 6 must build
  its sections on the intersected sets, not re-introduce the conjunctive query.
- Fallback (panel convergence, mandatory): any LLM failure -- `claude` not
  found on PATH, nonzero exit, unparseable envelope, timeout -> the digest
  still posts, subjects + deep links only, with a visible `bullets
  unavailable` line (renamed from `summaries unavailable`: the feature is
  bullets now). The digest contract never depends on the Anthropic API.
  Scott re-confirmed 2026-09-06 ("I agree with the fallback still posts in
  Slack").
- **Success criteria:** a live digest post shows all three sections with 3-7
  bullets under every pinned thread; a thread whose newest inbound message
  asks Scott for something renders that ask as a marked FIRST bullet, and a
  pure-FYI thread renders bullets with NO marked first bullet; deep links
  unchanged; **TWO separate budget criteria, because one test cannot do both
  jobs** (panel finding 2026-09-06, R2-M3 -- an earlier version used a single
  10-thread test and claimed it "fails against today's drop-trailing-items
  loop", which is false: at ~7300 of 10000 today's loop sheds nothing and an
  UNMODIFIED loop passes it. Round 1 rejected a criterion a correct
  implementation would fail; that replacement was one an incorrect
  implementation would pass):
  1. **Rendering, under budget.** A synthetic 10-thread MIXED pinned set (some
     threads with an ask, some pure-FYI), 7 bullets each at the 80-char cap,
     renders every thread, every bullet, every ask marker, and stays under
     `BUDGET`. The set MUST be mixed or ask-marking is untested. This pins the
     renderer and the typed `ask` field. It does NOT exercise the ladder and
     must not claim to.
  2. **The ladder, deliberately over budget at EVERY rung including the last.**
     A synthetic **70-thread** MIXED set, 7 bullets each.
     **The thread count is load-bearing and was wrong twice; here is the
     measurement so the next person does not have to redo it.** Per-thread
     rendered line is 123 chars incl. newline. **Do NOT hand-derive the totals:
     fixed overhead and the rendered ask bullet both VARY (85/118/151 for
     one/two/three populated sections; 95-101 for an ask bullet once its marker
     is applied), so any closed-form formula written here would be wrong for
     some renderer shape. Use the measured sweep below.**

     An earlier version of this criterion used 40 threads and asserted the
     final rung emits `... +N more`. **It cannot**: the 40-thread rung-2 floor
     MEASURES 8392-9123 across all nine section-count/marker variants, every one
     UNDER `BUDGET` 10000, so the ladder halts at rung 2 and rung 3 never fires
     (panel R3-M1, re-measured R6). The rejection holds under every variant.

     **AND THE 70-THREAD FIX ALONE IS STILL NOT ENOUGH, because the MIXED
     requirement changes the floor.** A pure-FYI thread contributes only its
     rendered line at the rung-2 floor, not a line plus a bullet, so the floor
     depends on the ask-BEARING count `m`, not on the thread count.

     **The numbers below are MEASURED OUTPUT, not arithmetic. They came from a
     simulator that transcribes `render()` and `line()` from
     `src/digest/mod.rs` and sweeps every section-count and ask-marker
     combination (provenance:
     `/tmp/review-panel/triage-r1/render-sim-r5.py`, review-panel round 5).
     If you change `BUDGET`, the bullet cap, the ask marker, the section count,
     or the renderer, RE-MEASURE the same way -- do not re-derive by hand.
     Phase 6 owns turning this into a checked-in Rust test; until then the
     numbers stand on that sweep. This method exists because FIVE
     consecutive review rounds rejected this criterion for arithmetic that was
     asserted instead of computed, and because the hand-built table that stood
     here was correct in exactly ONE of its nine combinations (panel R5-B2).
     Two of its inputs were wrong: fixed overhead is 85/118/151 for
     one/two/three populated sections, not a flat 200; and a rendered ask
     bullet is 95-101 chars once the `*Reply needed:* ` / `*Action:* ` marker
     is applied at render, not 85. **The rung-3 cliff therefore ranges m=13 to
     m=16 depending on section count and marker** -- so no single minimum is
     safe to quote, which is exactly why this doc now quotes a script instead.

     **Fixture: 70 threads, 35 ask-bearing, 35 pure-FYI.** Chosen because the
     simulator shows it drives rung 3 under ALL nine combinations, with the
     worst-case floor 11657 against `BUDGET` 10000 -- 1657 chars of margin, so
     it survives a marker change or the third section arriving. Re-measure if
     you change the fixture, the cap, the marker or the budget.
     **Second half, and it is not optional: the fixture's strings must be sized
     to these assumptions.** The 123-char line and 85-char bullet are the
     REAL-mail figures; in a synthetic fixture the test author picks the sender
     and subject lengths, and short ones drop the whole set under budget and
     silently defeat the test. Pin the fixture to a 123-char rendered line
     (assert it) and bullets at the 80-char cap.
     Asserts, in ladder order:
     - bullets are shed before ANY thread is dropped;
     - a thread never loses its ask bullet while a descriptive bullet on
       ANOTHER thread survives;
     - at the rung-2 floor, every **ASK-BEARING** thread still carries its ask
       bullet. **Not "every thread"** -- pure-FYI threads have no ask by
       construction, so at that rung they render with ZERO bullets and their
       digest line only. That is the intended shape, not a dropped thread, and
       the test must assert it explicitly (panel finding R3-M2: an earlier
       version demanded a MIXED set and then asserted a property only
       ask-bearing threads can have, which is unsatisfiable in both directions);
     - only once every thread is at that floor and the body is STILL over
       budget does rung 3 fire, dropping trailing items with the `... +N more`
       line.
     **Rung 3 needs a PRIORITY, and "the existing behavior" cannot supply one**
     (panel finding R5-B1, found independently by both seats). Today's loop
     (`src/digest/mod.rs:107-127`) has exactly two knobs, `s_show`/`i_show`, and
     sheds from whichever section is LONGER. Phase 6 adds a THIRD section, so
     "fall back to the existing drop-trailing-items behavior" names something
     that will not exist when this AC runs. Worse, the longest-first rule is
     actively wrong here: when Needs Reply is the longest section it would shed
     needs-reply threads while pure-FYI Important threads stay visible --
     protecting the ask BULLET at rung 2 and abandoning the ask-bearing THREAD
     one rung later.
     **Decision: rung 3 sheds by ACTIONABILITY, least first: Important, then
     Starred, then Needs Reply.** A Needs Reply thread is never dropped while
     any Starred or Important thread remains. Each section carries its own
     `... +N more`. This replaces the longest-first rule outright for the
     three-section renderer; it is not a tweak to it.
     **Both ACs must also state their fixture's section distribution.** Neither
     did (`grep -in section` over the AC block returned only prose). AC (1):
     10 threads as 3 Needs Reply / 4 Starred / 3 Important. AC (2): 70 threads
     as 20 Needs Reply / 25 Starred / 25 Important, with the 35 ask-bearing
     spread across all three so the priority rule is exercised rather than
     assumed.
     **Sequencing, because this test cannot be written against today's code**
     (panel finding R3-M3): `DigestItem` today has neither `ask` nor `bullets`
     (`src/digest/mod.rs:26-36`; `rg -n "bullets" src/` returns nothing), so a
     bullets test does not COMPILE against the current renderer, and
     "run it against the unmodified `format`/`render`" is not executable as
     written. Do it in this order: (a) land the typed `ask`/`bullets` fields
     and the bullet renderer -- AC (1)'s scope -- leaving the `s_show`/`i_show`
     loop untouched -- **note this is NOT purely additive: adding public fields
     to `DigestItem` breaks every struct-literal site, 9 in
     `src/digest/tests.rs` plus `build()` in `mod.rs`; mechanical and contained
     to the digest module, but it is part of step (a)'s scope**; (b) run THIS
     test and record the failure; (c) only then
     replace the loop with the ladder; (d) re-run and record the pass. Step (b)
     is what proves the test bites, and it is a recorded observation, not an
     assertion.
  Plus: with `claude` forced unresolvable (point `claude-binary` at a
  nonexistent path), the digest still posts complete minus bullets and carries
  the degradation banner, and exits 0.

#### Phase 7: Reply drafts
**Model:** opus
- For `draft: true` buckets: answered-rule (newest message from Scott ->
  remove bucket label), DRAFT-dedup, voice-profile prompt, RFC822 builder,
  `drafts.create`. No send path exists in the binary, enforced structurally:
  ALL Gmail mutations live behind one narrow module whose public surface is
  get | list | modify-labels | drafts-create only, plus a guard test using
  the exact send-call pattern proven in Phase 0d. The guard must BITE: add a
  real send call behind a test-only cfg and prove the guard fails on it
  (**Phase 0d measured this and the earlier worry was wrong in a useful way:**
  `messages_send(` and `drafts_send(` DO match the crate's real builder
  surface, `api.rs:2130` and `api.rs:1733`. Use the validated pattern
  `\b(messages_send|drafts_send)\s*\(` with the DEFAULT regex engine -- this
  host's ripgrep has no PCRE2 -- and note that `drafts_send` is the sender
  that matters here, since it is the one call that would send the drafts this
  phase creates).
- Failure recovery is free: a run that labels a thread but dies before
  drafting leaves no DRAFT in the thread, so the next needs-reply refresh
  retries it. Missing voice-profile file -> loud error, drafting skipped,
  classification unaffected (degrade visibly).
- **Success criteria:** a needs-reply thread gets exactly one threaded draft;
  rerun creates no second draft; Sent folder unchanged after a full run; the
  no-send guard test passes AND its bite is demonstrated (injected send ->
  guard fails).

#### Phase 8: Shakedown + docs true-up
**Model:** sonnet
- `/cli-shakedown` on the new surface; README + example.yml reflect shipped
  reality; operator steps enumerated (dotfiles deploy, `service reinstall`,
  `loginctl enable-linger` if not already set -- NO label step, the engine
  ensures labels itself).
- **Success criteria:** shakedown report committed under `docs/` with every
  finding marked fixed | accepted | ticketed, where **"ticketed" means a real
  issue URL is pasted next to it and "accepted" carries a one-line reason** --
  otherwise the three states are unfalsifiable and the gate passes vacuously
  (panel cheap-win, raised in both rounds); README documents `triage` with
  examples that were actually run.

## Acceptance Criteria

- [ ] `eratosthenes triage --dry-run` on the live work inbox prints a
      thread -> bucket table and exits 0 with zero Gmail mutations.
      **Observed on main (2026-09-06):** FAILS, as expected pre-build.
      `eratosthenes triage --dry-run` prints
      `error: unrecognized subcommand 'triage'` and exits 2. `eratosthenes
      --help` on v0.3.0 lists five subcommands: run, digest, auth, service,
      config. The `triage` subcommand and its `--dry-run` flag are specified
      at this doc's API Design (line 287) and built in Phase 4.
- [ ] After a timer-fired run, every inbox thread with previously-unseen
      messages carries exactly one `llm/*` bucket label plus
      `llm/seen`; an immediate rerun performs zero mutations
      (journal-verified).
      **Observed on main (2026-09-06):** FAILS, as expected pre-build.
      `rg -n 'llm/' src/` returns zero matches: no `llm/*` bucket label and
      no `llm/seen` marker are written by any path in the binary.
- [ ] A `llm/noise` thread leaves INBOX on its configured TTL while a
      `llm/needs-reply` thread survives past the same window.
      **Observed on main (2026-09-06):** FAILS, as expected pre-build.
      Neither label exists: `rg -n 'llm/' src/` returns zero matches, so no
      state-filter can key off `llm/noise` or protect `llm/needs-reply`.
- [ ] The Mon/Thu digest contains Needs Reply | Starred | Important sections
      with 3-7 bullets under EVERY pinned thread and working deep links,
      within the char budget; a thread that asks Scott for something renders
      that ask as a marked FIRST bullet and a pure-FYI thread renders no
      marked bullet; with the `claude` CLI unavailable it still posts, minus
      bullets, with a visible degradation line.
      **Observed on main (2026-09-06):** FAILS, as expected pre-build. The
      live digest posts two sections (Starred | Important), no Needs Reply
      section, no bullets, no LLM call anywhere in the binary
      (`rg -in 'anthropic|api[_-]key' src/` returns only the `:giga-claude:`
      signature). Latest post:
      https://tatari.slack.com/archives/D01G4Q7AWLV/p1788737068797219
- [ ] Every unanswered needs-reply thread has exactly one threaded reply
      draft in Gmail Drafts; the Sent folder is unchanged by any triage run;
      the send-path grep test exists and passes.
      **Observed on main (2026-09-06):** FAILS, as expected pre-build.
      `rg -n 'drafts|Drafts' src/` returns zero matches: no draft-creation
      path exists, and no send-path grep test exists to pass.

## Acceptance Criteria: post-build verification (2026-09-07)

Run after Phase 8, against the built binary at `b3db90d`. Nothing below is
assumed: a criterion that could not be executed says why.

| # | criterion | result |
|---|---|---|
| 1 | `triage --dry-run` prints a thread -> bucket table, exits 0, zero mutations | **PASS** |
| 2 | timer-fired run applies exactly one `llm/*` bucket + `llm/seen`; rerun is a no-op | **UNVERIFIED** |
| 3 | `llm/noise` leaves INBOX on TTL; `llm/needs-reply` survives | **UNVERIFIED** |
| 4 | digest renders three sections with 3-7 bullets, marked ask, budget, degradation line | **UNVERIFIED live** |
| 5 | exactly one threaded draft; Sent unchanged; send-path grep test exists and passes | **PARTIAL** |

- **(1) PASS.** Run three times this session. Latest (Phase 8 shakedown): exit 0,
  2m33s, `Triage: 50 threads classified, 0 labeled, 0 skipped (dry run)`. On
  `main` before this work the same command exited 2 with
  `error: unrecognized subcommand 'triage'`.
- **(2) and (3) UNVERIFIED, by the plan's own gate, not by omission.** Both
  require a LIVE labeling run. The Rollout Plan (line 1198) states Phase 4 runs
  `--dry-run` only until the eval gate passes, and the eval gate is Scott's
  sign-off on `docs/eval/llm-triage-eval.md` (<= 5/50 disagreements), which has
  not happened. (3) additionally needs real elapsed time: the fastest bucket TTL
  is 1d.
- **(4) UNVERIFIED LIVE.** The rendering, the 3-7 bullet range, the 80-char cap,
  the marked ask, the one-section-per-thread rule, the shrink ladder under
  BUDGET=10000, and the failure-class banner are all covered by passing tests,
  including a ladder test proven to bite against the pre-Phase-6 renderer. What
  is NOT observed is a real Slack post. Blocked twice over: the digest is
  eval-gated like (2), and `SLACK_XOXP_TOKEN` was destroyed on 2026-09-07 (see
  the INCIDENT entry in the implementation notes) and must be re-provided.
- **(5) PARTIAL.** The send-path guard EXISTS and PASSES, and its bite is
  demonstrated: injecting a real compiling `drafts_send(...)` into
  `src/gmail/client.rs` made it fail with
  `Found 1 send call(s): src/gmail/client.rs:538: drafts_send(`, and removing it
  restored green. Structurally there is no send path in `src/`. What is NOT
  observed is a draft actually landing inside its target thread in the Gmail UI:
  the Phase 0(a) probe was blocked by the permission layer and no
  `drafts.create` has ever run from this host. Tracked at
  https://github.com/scottidler/eratosthenes/issues/1

**No criterion FAILED.** Four of five are gated on two things outside the code:
Scott's eval sign-off, and one live draft-threading check.

## Resolved Decisions

- **2026-09-06 -- unattended triage does NOT compete with Scott's interactive
  Claude quota.** Both reviewers made this their hardest question in round 2,
  independently, and it is closed: this is Scott's COMPANY Claude plan and it
  is unlimited. There is no seat-quota interaction to design around, no
  rate-budget to allocate between the timer and interactive use, and no reason
  to prefer a cheaper model on quota grounds (model choice stays a
  latency/quality decision). Recorded here rather than in Open Questions
  because it is answered. **Do not re-open it.**
- **2026-09-06 -- the digest fallback banner names the failure CLASS.** Falls
  out of the keyless transport: an expired `claude` login exits cleanly
  non-zero, so the digest keeps posting, correctly, minus bullets, forever, and
  nothing else would ever tell Scott. A banner that only says "bullets
  unavailable" makes a weeks-long outage indistinguishable from a one-off blip.

- **2026-09-06 -- digest gets 3-7 BULLETS per pinned thread, not a one-line
  summary. Supersedes the 2026-07-06 Phase 6 wording.** Scott: "I want to add
  some bulleted details under each of these items ... especially if I am asked
  to do something (that should be the first bullet)." Settled in the same
  exchange: bullets go on EVERY pinned thread in all three sections ("bullets
  for every"); an ask is marked in the first bullet so it reads as an ask
  rather than a description ("someway of telling me that I need to reply in the
  first bullet"); and a thread with no ask gets plain summary bullets with NO
  placeholder line ("no ask is just bullets as summary"). The "No action
  needed" first bullet was explicitly considered and declined -- absence of a
  marked bullet is the signal. Do NOT restore the one-liner.
- **2026-09-06 -- the char-budget shrink loop must shed BULLETS before
  THREADS.** Consequence of the bullets decision, and it is a code change, not
  a prompt change: today's `digest::format` drops whole trailing items to fit
  3500 chars, which with bullets would silently delete entire threads from the
  digest. An ask bullet is never dropped while any descriptive bullet on
  another thread survives. Scoped into Phase 6 with the DELIBERATELY
  OVER-BUDGET ladder test described there. (An earlier version of this line
  said "a 30-thread x 7-bullet test"; the 30-thread form is retired -- see
  Phase 6.)
- **2026-09-06 -- the LLM transport is the KEYLESS `claude` CLI, not HTTP with
  an API key. SUPERSEDES this doc's original premise and an intermediate
  same-day amendment.** History, because both superseded forms look reasonable:
  1. Original (2026-07-06): raw HTTPS via `ureq` with `ANTHROPIC_API_KEY` in a
     0600 `EnvironmentFile`, justified by citing clyde as the in-house
     precedent.
  2. Intermediate (2026-09-06, morning): same shape, but the var corrected to
     `ESCOTE_ANTHROPIC_API_KEY` after tracing how `sb cortex` and `borg` get
     theirs. Bare `ANTHROPIC_API_KEY` does not exist in this environment, and
     the two bare `ANTHROPIC_*` vars that DO exist (`_API_ADMIN_KEY`,
     `_ENTERPRISE_SPEND_REPORTING_API_KEY`) are the wrong kind and cannot call
     `/v1/messages`.
  3. Current: no key at all. Chasing the clyde citation for panel finding M4
     showed the cited path does not exist AND that clyde deliberately excised
     the key three weeks after this doc was drafted
     (`clyde/docs/design/2026-07-29-excise-api-key.md`, Implemented). The
     precedent this design leaned on had moved to the opposite approach, and
     `clyde-enrich.service` proves it unattended under systemd on this host
     with no `EnvironmentFile`.
  Scott chose the keyless transport 2026-09-06 ("B"). Consequences, all
  folded in above: `api-key-env` deleted from config, `triage.env` never
  generated, the `ureq` dep never added, Phase 0c re-aimed at PATH resolution
  under systemd, and panel finding M2 (the digest has no route to the key)
  DISSOLVED rather than fixed -- there is no key to route. **Do NOT restore
  either key-bearing form.**
- **2026-09-06 -- this is NOT a reversal of the Option A rejection.** Option A
  was a headless `claude -p` agent driving `gws`, and it was rejected because
  `gws gmail +reply` SENDS immediately (`reply.rs` `create_reply_raw_message` -> `send_raw_email`),
  putting the design one allowlist mistake from disaster, and because it
  re-implements auth/query/label plumbing eratosthenes already owns. Neither
  reason touches the transport question. This design keeps every Gmail
  operation in Rust behind the no-send module and shells out ONLY for the LLM
  call -- the same split clyde uses. The one Option-A flaw this DOES retire is
  "Claude subscription auth under systemd is unproven": clyde proved it. That
  flaw was never load-bearing on its own.
- **2026-09-06 -- this doc is amended against v0.3.0 and its 5/5 review does
  NOT cover the amendments.** v0.3.0 shipped the message-filter act-once work
  (`docs/design/2026-09-03-message-filter-act-once.md`): a `Triaged` marker
  label, per-filter candidate scope, custom-label resolution in message
  filters, and a plan-then-write refactor (`plan_filter_writes` ->
  `apply_planned_write`). `src/engine.rs` went from ~600 to 1829 lines, so
  every line citation here was rebased and the two load-bearing claims
  (`apply_state_action`'s remove-set, `get_thread`'s metadata-only fetch) were
  re-verified as still true. Re-review is required before build.
- **2026-09-06 -- NAME COLLISION, and it is not cosmetic.** v0.3.0's marker
  label is `Triaged`; this doc's labels are `llm/*` plus `llm/seen`.
  Two different mechanisms with near-identical names on the same mailbox: the
  marker means "a message-filter handled this message" and is message-scoped
  and hidden; `llm/*` means "the LLM classified this thread". They do not
  functionally conflict -- v0.3.0 validates `marker-label` against state-filter
  labels and destinations, and `llm/*` are neither -- but a future reader
  WILL confuse them. **SETTLED 2026-09-06 after panel round 1: rename the
  BUCKETS to `llm/*` (`llm/needs-reply`, `llm/fyi-work`, `llm/recruiting`,
  `llm/receipts`, `llm/noise`, `llm/seen`); the `Triaged` marker keeps its
  name.** The seats split -- the Architect said rename the buckets, the Staff
  Engineer said rename the marker on the better semantic argument that
  `triage/*` is the literal namespace of the triage feature while `Triaged` is
  a misleading name for "a message filter handled this message". The semantic
  argument loses on cost, for a reason neither seat weighed: v0.3.0's
  `--mark-only` backfill (4491a89) has ALREADY stamped `Triaged` across the
  live mailbox -- 40 messages on 2026-09-06 -- so renaming the marker is a
  Gmail label migration on real mail, while renaming the buckets is
  find-and-replace in an unbuilt doc. Note also that `marker-label` is
  validated case-insensitively against state-filter labels and destinations at
  config load (`src/cfg/config.rs:137-149`), so there is no silent-breakage
  path here, only reader confusion -- which is exactly what the cheap fix
  buys off. The subcommand stays `eratosthenes triage` and the config block
  stays `triage:`; only the Gmail LABEL namespace changes.

- **2026-07-06 -- Option B (extend eratosthenes) over Option A (headless
  `claude -p` + gws).** Named flaws in A: Claude subscription auth under
  systemd is unproven; gws has no threading-aware draft helper (its `+reply`
  SENDS immediately -- one allowlist mistake from disaster; panel verified
  the call chain: `reply.rs` `create_reply_raw_message` -> `send_raw_email`); A re-implements
  auth/query/label plumbing eratosthenes already owns. B has direct in-house
  precedent (clyde). Panel round: both reviewers confirmed B, and correctly
  called out that B's real cost was undersold -- the Move-semantics engine
  change (Phase 2) and body/MIME plumbing (Data Plumbing, Phase 4) are B's
  price, now scoped explicitly. A recorded in Alternatives.
- **2026-07-06 -- needs-reply is a label, not a star.** The original pitch
  said "stars the needs-reply ones". Deviation, with cause -- rationale
  CORRECTED by the panel: star is not a manual-only signal; `tatari.yml`
  message-filters (`leadership`, `only-me-vip`) already apply Star
  programmatically as deterministic VIP routing, alongside Scott's manual
  pins. That makes the separation stronger, not weaker: a third,
  probabilistic writer on Star would blur two deterministic signals. The
  agent gets its own label with its own Keep filter and digest section;
  Star keeps its two existing writers.
- **2026-07-06 -- summaries are generated at digest time, stateless, WITH a
  mandatory no-BULLETS fallback.** Nothing stored between runs; ~10 threads
  2x/week makes freshness free and removes a storage design entirely. Panel
  round: the Architect wanted summaries cached at triage time so the digest
  never calls Anthropic; rejected as speculative storage at this volume
  (Addendum), but its underlying concern is folded in as the Phase 6
  fallback -- the digest contract never depends on the Anthropic API.
- **2026-07-06 -- `llm/seen` is message-level, not thread-level.** Pass-4
  finding: a thread-level marker permanently blinds triage to threads that
  later receive a real human reply, and the noise TTL would age them away.
  Message-level markers make new inbound messages re-trigger classification
  for free (Gmail's native label semantics). needs-reply threads are
  additionally re-checked each run (answered-rule + draft refresh).
- **2026-07-06 -- work account only for now.** Per-account config means home
  costs nothing to add later; no home account is configured today.
- **2026-07-06 (panel round) -- the aging integration is NOT config-only.**
  Staff Engineer finding, verified against `src/engine.rs:905` (re-verified
  2026-09-06 on v0.3.0): Move
  with a labeled filter removes the filter's own labels, not INBOX. Scoped
  as Phase 2 (remove INBOX + stage labels; match labels are criteria only).
  This corrects the doc's original premise.
- **2026-07-06 (panel round) -- timer shape mirrors the digest precedent**
  (`generate_digest_timer`, `service.rs:147`): one unit pair, first triage-enabled account's
  schedule, one run covers all triage-enabled accounts.
- **2026-07-06 (panel round) -- candidate ordering is client-side**
  (`internalDate` desc). `messages.list` ordering is undocumented; sorting
  ourselves deletes the environmental assumption instead of spiking it.
- **2026-07-06 (panel round) -- the no-send guarantee is structural.** One
  mutation module (get | list | modify-labels | drafts-create) + a guard
  test whose pattern is pinned in Phase 0d and whose bite is demonstrated in
  Phase 7. A grep alone was agreed insufficient by both reviewers.

## Alternatives Considered

### Alternative 1: Headless `claude -p` + gws on a systemd timer (Option A)
- **Description:** prompt-file agent; gws CLI for all Gmail ops; wrapper
  script hardened per architect-agy; no Rust changes.
- **Pros:** zero Rust; prompt-only iteration; the agent can read files
  (voice profile) natively.
- **Cons:** subscription auth under systemd unverified; no safe draft
  helper -- agent must hand-build RFC822 (fragile, and `+reply`/`+send` sit
  adjacent, send-capable); duplicates existing plumbing; idempotency and
  dry-run live in a prompt instead of code; OS-keyring lock post-reboot.
- **Why not chosen:** three named flaws vs zero for B; B has equal in-house
  precedent. The prompt-iteration advantage is mostly neutralized by making
  the taxonomy and bucket descriptions config-delivered in B.

### Alternative 2: Off-the-shelf (Inbox Zero self-hosted, Shortwave)
- **Description:** existing AI email products; Inbox Zero is open source and
  self-hostable.
- **Pros:** no build; polished UI.
- **Cons:** Shortwave = third-party OAuth grant with full work-mailbox access
  (awkward for the acting Head of Security) and it is a client again; Inbox
  Zero self-hosting = a web stack to run, and neither composes with
  eratosthenes' aging engine or Slack digest.
- **Why not chosen:** doesn't compose with the machinery that already works;
  OAuth posture violates the no-third-party-grants requirement.

### Alternative 3: Gmail MCP + scheduled claude.ai routine
- **Description:** the Gmail MCP already connected to Claude sessions, driven
  by a cloud-scheduled routine.
- **Pros:** zero local code; already authenticated interactively.
- **Cons:** interactively-authenticated MCP servers are unreliable headless;
  cloud runtime, least deterministic option; its `create_draft` exists but
  threading behavior is unverified.
- **Why not chosen:** weakest determinism and auth story of the three.

### Alternative 4: Hybrid (Rust classifies, `claude -p` drafts)
- **Description:** Option B for classify/summarize, Option A only for voice
  drafting.
- **Pros:** best drafting ergonomics.
- **Cons:** two runtimes, two auth paths, two failure modes for one feature;
  siblings stop behaving identically; RFC822 threading still belongs in Rust.
- **Why not chosen:** complexity buys only prompt convenience; the voice
  profile is a file on disk that Rust can read into a prompt just as well.

## Technical Considerations

### Dependencies
- **No new direct deps, and the subprocess timeout is SETTLED** (see Open
  Questions): `tokio::time::timeout` around the child plus explicit `kill()`
  AND `wait()`. eratosthenes already depends on
  `tokio = { features = ["full"] }`, so clyde's `wait-timeout = "0.2.1"`
  (`clyde/common/Cargo.toml:35`) buys nothing here. (Two corrections layered on
  this line: R2-M4 caught an earlier "No new direct deps" that ignored the
  timeout mechanism entirely; R4 caught that the R3 fix wrote the settlement
  into Open Questions and left this line still saying "undecided ... settle in
  Phase 4".)
- The 2026-09-06 keyless-transport decision removed
  the `ureq` dep this doc originally added: the LLM call is a subprocess to the
  locally installed `claude` CLI, so there is no HTTP client to wire. New
  RUNTIME dependency instead, and it is a real one worth naming: the `claude`
  binary must be installed and logged in on the host, and resolvable on the
  systemd unit's pinned PATH. Existing: `google-gmail1 7.0.0` (drafts.create
  already in scope -- token already holds full `https://mail.google.com/`).
- Cross-repo blast radius: `eratosthenes` (code) -> `dotfiles` (`tatari.yml`
  triage block, state-filters, prompt/voice paths). **`secrets` is NOT in the
  blast radius at all: the transport is keyless.** (Panel cheap-win 2026-09-06:
  this sentence inverted during the transport edit and read as a fragment.
  Phase 0c settles the PATH question instead of a credential question.) Ship
  order: eratosthenes release first, then
  dotfiles config deploy, then `service reinstall`.

### Performance
- ~10 threads/day, capped at 50/run. One batched classify call per run; one
  call per draft; one per digest. Latency and token cost are noise at this
  volume; the `max-threads` cap is the runaway guard and logs loudly when it
  bites (no silent truncation).

### Security
- No new OAuth grants, and **no new credential of any kind: the LLM transport
  is keyless** (2026-09-06 decision). Strictly better than the original design,
  which would have put an Anthropic key in two unit env files.
- **Two secrets exist and neither is new** (panel finding R4: an earlier
  version of the bullet above claimed "the only secret in play stays the
  existing file-based Gmail token", which is false, and the orphaned
  "Secrets named by env var in YAML" clause trailing it was the vestige of
  exactly the token it forgot):
  1. the Gmail OAuth token, a 0600 file on disk;
  2. `SLACK_XOXP_TOKEN`, the digest's Slack user token, named in YAML by env
     var only (`token-env`, `src/cfg/config.rs:65-66`,
     `eratosthenes.example.yml:31`) and never valued there.
  Both predate this design and neither is touched by it.
- Fail closed: no send path in the binary, enforced by test. Aging Moves
  labels; nothing is permanently deleted -- a misclassified thread is always
  recoverable by search.
- Prompt injection: email bodies are adversarial input to both models. The
  prompts frame content strictly as data. **The blast radius is bounded by the
  ARGV, not by the prompt, and that is the load-bearing point** -- panel
  finding 2026-09-06 (R2-M2) caught an earlier version of this section
  asserting "bounded by construction" while the API Design specified no
  hardening flags at all. Without `--tools ""` and `--safe-mode` the child is a
  full Claude Code session on this host with tools live and this machine's
  customizations loaded, and an adversarial body is executing against them.
  WITH them the child is a pure data transformer that can read nothing and
  write nothing, and only THEN is the worst case a wrong label (recoverable,
  and a human reply re-triggers classification) or a bad draft (never sent,
  always human-reviewed). Any change to the argv is a change to this claim.
- Work mail bodies (truncated at `body-chars`) are passed on stdin to the
  local `claude` CLI, which sends them to Anthropic under ITS OWN auth
  (Scott's company Claude plan). **eratosthenes holds, reads, and forwards no
  credential** (2026-09-06 keyless-transport decision; corrected from an
  earlier version of this line that said "under Scott's key"). The child's
  environment is `env_clear()`ed with every `ANTHROPIC*` var excluded by
  construction. Same data path Tatari already runs through Claude daily;
  flagged here for the record since Scott signs off security.

### Testing Strategy
- Unit: config validation, Move-semantics regression (all three filter
  shapes), RFC822 builder (threading headers), MIME walk + quote stripping,
  classifier JSON parse + retry/skip, truncation.
- Bite checks: break the Move fix and the RFC822 builder to prove their
  tests fail; inject a send call to prove the no-send guard fails.
- Eval: Phase 4's labeled 50-thread set; disagreements <= 5/50 before the
  timer goes live.
- Negative: no-send guard; rerun-is-noop; DRAFT-dedup; digest-without-API
  fallback.

### Rollout Plan
- Phases 0-1 land with zero behavior change (spike + schema). Phase 2 is the
  engine fix, regression-tested against every existing filter shape before
  any triage label exists to exercise it. Phase 3 is config.
- Phase 4 runs `--dry-run` only until the eval gate passes; then the timer
  (Phase 5) goes live with labels only. Drafts (Phase 7) arrive last, after
  digest enrichment (Phase 6) proves the classification is trustworthy in
  daily use.
- Backout: `systemctl --user disable --now` the triage timer + remove the
  config block. The aging engine reverts to today's behavior; labels left
  behind are inert.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Misclassification buries a real thread in noise | Med | High | needs-reply is Keep-protected; noise is aged, never deleted; eval gate before live; a new human reply re-triggers classification (message-level seen); digest still shows Starred/Important independently |
| needs-reply thread sits unseen until the next digest (Mon,Thu today) | Med | Med | digest cadence is already config (`slack.schedule`); tighten to daily the moment latency is an observed problem -- zero code |
| Draft threading breaks (orphan drafts) | Med | Low | Phase 0 spike proves RFC822 shape before any code; threading unit test with bite check |
| LLM output schema drift | Low | Low | strict parse, one retry, loud skip; thread stays unseen and retries next run |
| Timer doesn't fire post-reboot pre-login | Low | Low | same posture as existing eratosthenes timers (file tokens, no keyring); `enable-linger` in operator steps |
| `claude` CLI version/flag drift breaks the hardened argv | Med | Med | Version floor LOGGED on every call and named in every failure (not pre-flight gated -- a brittle parse would fail closed on a working CLI); Phase 0c spikes the full production argv, so an unsupported flag reads as "your claude is older than the floor" |
| Subscription auth expires under the unattended timer | Med | Med | Fails as a clean nonzero exit into the digest banner, NOT a hang; the banner names the failure class so it is actionable rather than silent (Phase 6) |
| `claude` CLI failure (not found, nonzero exit, unparseable envelope, timeout), TRIAGE and DRAFT paths | Low | Low | loud error, nonzero exit, zero mutations; next timer fire retries |
| `claude` CLI failure, DIGEST path | Low | Low | **Opposite rule, deliberately.** The digest still POSTS, minus bullets, with the degradation banner, and exits 0. Phase 6 calls this mandatory; the digest contract never depends on the Anthropic API. An earlier version of this table stated the triage rule over ALL paths, which contradicted Phase 6 (panel finding 2026-09-06, M5) |
| Cost runaway | Low | Low | `max-threads` cap + batched calls + `body-chars` truncation |

## Open Questions

**None.** Round 3 (2026-09-06) correctly refused an earlier version of this
section that listed five items and called them "genuine unknowns": three were
deferred DECISIONS, which is exactly what this gate exists to catch. They are
settled below and folded into the phases; the one real unknown became a Phase 0
deliverable with a provisional value; the quota item moved to Resolved
Decisions where an answered question belongs.

- **Subprocess timeout: SETTLED, no new dep.** `tokio::time::timeout` around
  the child, then explicit `kill()` AND `wait()` -- eratosthenes already
  depends on `tokio = { features = ["full"] }`, so `wait-timeout` (clyde's
  choice) buys nothing here.

  > **CORRECTED 2026-09-07 (audit C1).** This bullet continued: "The in-process
  > bound does NOT cover a child that ignores SIGTERM, so the generated units
  > also get `TimeoutStartSec`, set to the transport timeout plus 60s," with
  > provisional values of 300s for triage and 120s for a digest bullet pass.
  >
  > **The premise is false.** The code never sends SIGTERM to the child.
  > `src/triage/claude.rs` calls `child.start_kill()`, which is
  > `std::process::Child::kill()`, which is SIGKILL on unix (tokio 1.50.0
  > `process/mod.rs:1247`, and its own doc comment says so). SIGKILL cannot be
  > ignored, so the failure mode this decision was written for does not exist.
  > Orphaned grandchildren are the one real gap the in-process bound cannot
  > close.
  >
  > **AMENDED again, same day, after panel round 8.** The sentence here
  > previously claimed that gap "is handled by systemd's default
  > `KillMode=control-group` on unit stop, and not by `TimeoutStartSec` at all."
  > That is true about systemd's defaults and FALSE as protection, and citing it
  > was the error. `KillMode` applies on unit STOP; all three generated units are
  > `Type=oneshot`, and `man systemd.service` states the start timeout "is
  > disabled by default" for oneshot. So nothing ever stops a wedged oneshot and
  > the cgroup kill can never fire against the failure it was cited for.
  >
  > Worse for the run unit: its timer is `OnUnitActiveSec`, defined relative to
  > last activation. While a unit sits in `activating`, later start jobs MERGE
  > into the running one, so a wedge silently stops the 5-minute engine with no
  > failed unit and no log line. That is precisely the invisible-failure shape
  > the transport bounds were added to remove, reintroduced one level up.
  >
  > `TimeoutStartSec` is therefore REQUIRED, and is now emitted on all three
  > units (`src/service.rs`). The values are derived, not guessed:
  > `run_start_timeout()`, `digest_start_timeout()`, and
  > `triage_start_timeout(max_threads)` sum the per-call and per-child ceilings
  > they bound, giving 3760s, 2376s, and 26204s at `max-threads: 50`. Each sits
  > far above the worst LEGITIMATE run, because a bound that can kill healthy
  > work is worse than no bound, and the triage value tracks `max-threads` so
  > raising the cap cannot silently invalidate it. The "nobody has measured a
  > healthy ceiling" blocker recorded below was wrong: the ceiling is computable
  > from the constants already in the code.
  >
  > The arithmetic was also written for a unit that makes ONE transport call.
  > That holds for digest; triage makes one classify call plus up to
  > `max-threads` draft calls, so "transport timeout + 60s" (360s) would SIGKILL
  > a healthy triage run at its second draft. This is the same "one number doing
  > two unrelated jobs" error caught above for the 120s collision, and missed
  > here.
  >
  > **The real unbounded wait was the HTTP transport, which this doc never
  > examined.** `hyper_util`'s legacy client sets no request, response, or
  > connect timeout, and `HttpsConnectorBuilder::build()` supplies a default
  > `HttpConnector` with none either. Every Gmail call and the Slack post could
  > not fail, only hang, and a hang is invisible to `is_retryable` because it
  > never produces an error to classify. Fixed at the transport instead: see
  > `gmail::rate::REQUEST_TIMEOUT` (30s per call, inside the one `with_retry`
  > chokepoint that all 13 Gmail calls pass through), `CONNECT_TIMEOUT` (10s,
  > shared connector), and `slack::REQUEST_TIMEOUT` (15s over both the request
  > and the body read).
  >
  > `TimeoutStartSec` is not the mechanism that satisfies this section's intent
  > -- the per-call transport bounds are -- but it is a REQUIRED backstop for
  > the oneshot reason above, and it is implemented.
  > `gmail::rate::worst_case_call_duration()` (188s per call: five attempts plus
  > the backoff ladder) is the per-call derivation the unit values are built
  > from. They are deliberately generous: the derivations are additive worst
  > cases that assume every call burns its full ceiling, which is not physically
  > reachable, and that pessimism is the safety margin.

  Clyde's 900s is for a ~500KB report render and is the wrong shape to copy.
  **Naming, because `120` was doing two unrelated jobs** (panel R5-B3, and the
  R4 clarification caused it rather than curing it): the DIGEST TRANSPORT
  TIMEOUT is 120s, and Phase 5's "under 120s at the 50-thread cap" is a TRIAGE
  PERFORMANCE TARGET. Same number, different subsystems, coincidence not
  derivation. Phase 5's target is renamed to **90s** to kill the collision --
  it is a target, so tightening it costs nothing, and a distinct number cannot
  be misread as a reference to the digest timeout.
- **Auth expiry surfacing: SETTLED.** The realistic failure is a clean non-zero
  exit with no JSON envelope (`clyde/common/src/llm/cli.rs:168-170`), which
  lands in the digest's no-bullets fallback -- correct behavior, and silent
  potentially for weeks. Therefore the fallback banner MUST name the failure
  CLASS, not just say bullets are unavailable: an auth failure reads
  "bullets unavailable: claude not authenticated" and is actionable, where a
  transport blip reads differently and is ignorable. Folded into Phase 6.
- **CLI output/flag drift: SETTLED, and it was never an open question** -- it
  is a Risks row, now carried there, mitigated by the logged version floor.
- **The 30-thread caching revisit** stays where it belongs, in the Addendum,
  with its stale reasoning called out and a measured re-decision required after
  the first real bulleted digest. That is a scheduled revisit with a named
  trigger, not an unknown.

## Addendum: rejected and demoted findings

Recorded so they are not re-litigated.

- **Cache digest summaries at triage time (Architect, rejected).** Proposed
  so the digest never calls Anthropic. Rejected: at ~10 threads 2x/week the
  latency/cost argument for statelessness holds, and a storage layer is
  exactly the speculative design this doc avoids. The underlying concern
  (digest must survive an API outage) is legitimate and folded in as the
  Phase 6 no-bullets fallback -- the digest contract never depends on the
  Anthropic API. **REOPENED 2026-09-06 (panel round 1, both seats): this
  rejection no longer stands on its stated reasoning.** It was decided at ~10
  threads x ONE line each; bullets multiply the payload by up to 7x, and the
  amended Phase 6 writes a 10-thread x 7-bullet case into its own acceptance
  criteria. The original revisit condition below is therefore already in
  sight. Disposition for now: stay stateless, because the MEASURED live pinned
  set on 2026-09-06 is 5 threads (2 starred, 3 important) and v0.3.0's
  act-once work is what stops it growing -- the 38-thread pile that made
  caching look necessary was the defect, not the steady state. Re-decide with
  measured latency and token numbers from the first real bulleted digest, not
  from this estimate. Original revisit condition: pinned-set volume grows past ~30 threads
  or the digest goes daily AND summary latency is observed to matter.
- **`ureq` is binary bloat; reuse in-tree hyper (Architect, demoted by
  panel synthesis 2026-07-06).** MOOT as of 2026-09-06: the keyless transport
  adds no HTTP client at all, so neither `ureq` nor hyper is in question.
  Recorded because the original rejection reasoning ("clyde uses exactly
  `ureq` for Rust -> Anthropic") is now FALSE and would mislead anyone who
  re-opened it -- clyde excised that path entirely.
- **Option A (headless `claude -p` + gws) stays rejected, and the 2026-09-06
  transport change does NOT revive it.** Both reviewers independently
  confirmed B; the Staff Engineer verified A's send footgun in gws source
  (`reply.rs` `create_reply_raw_message` -> `send_raw_email`). A's defect was letting an AGENT drive
  MAIL. Shelling out to `claude -p` for the LLM call alone, with every Gmail
  operation staying in Rust behind the no-send module, shares only the
  subprocess and none of the hazard. The one A-rejection reason that HAS
  expired is "Claude subscription auth under systemd is unproven"; see the
  transport decision in Resolved Decisions.

## References

- Research brief: design-research agent, this session (file:line citations
  throughout are from it, verified against v0.2.11).
- Review panel: Architect (Gemini) + Staff Engineer (Codex), 2026-07-06;
  raw outputs `/tmp/review-panel/pntWkCFn/{arch,staff}.out`; both MUST-FIX
  findings verified against the code before folding in.
- Prior art: `~/repos/tatari-tv/clyde/common/src/llm/cli.rs` (the KEYLESS
  `claude -p` transport this design copies, including the hardening argv at
  `:99-130`, the version floor at `:32-42`, and the `env_clear()` allowlist at
  `:420-434`), `~/repos/tatari-tv/clyde/docs/design/2026-07-29-excise-api-key.md`
  (why clyde has no key), `eratosthenes/src/service.rs` (unit generation),
  `~/.claude/skills/architect-agy/script.sh` (headless hardening, Option A).
  **Corrected 2026-09-06 (panel R2-M1): this line previously cited
  `clyde/open/report/src/summarize.rs` as "Rust -> Anthropic on a timer". That
  path does not exist and that approach was deleted.**
- Voice profile: `~/Claude/writing/VOICE.md`.
- Existing digest design: `docs/design/2026-06-06-slack-digest.md`.
