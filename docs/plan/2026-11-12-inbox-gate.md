# Design Document: Inbox Gate (the inbox holds only promoted mail)

**Author:** Scott Idler
**Date:** 2026-11-12
**Status:** Implemented (Phases 1-7 in code; Phase 0b/0c, 8 and 9 are operator steps pending release)
**Review Passes Completed:** 5/5

## Summary

A triage bucket label is a countdown today: the thread keeps sitting in INBOX for 1 to 7 days. This doc makes triage remove a thread from INBOX the moment it lands in an archiving bucket, behind a code-level guard that never archives a human mail addressed only to me. Triage moves from daily to hourly, a `senders` report shows who still reaches the inbox, three defects found on 2026-11-11 get fixed, and the personal account (10,324 inbox threads, no config at all) comes under the same engine.

## Problem Statement

### Background

- `eratosthenes run` fires every 5 min: message filters, then thread age-off INBOX -> Purgatory -> Oblivion.
- `eratosthenes triage` classifies inbox threads into `llm/*` buckets with one Haiku call through the `claude` CLI.
- `eratosthenes digest` posts Starred + Important to Slack, Mon and Thu.
- One account is configured: `tatari` (work). Config: `~/repos/scottidler/dotfiles/HOME/.config/eratosthenes/tatari.yml`. Timers run on ripr.
- Config fixes already live as of 2026-11-11 (not part of this doc):
  - three bulk-sender message filters (`file-detection`, `gemini-notes`, `domain-registrar`)
  - `max-threads` 50 -> 100, triage schedule weekdays -> daily
  - `llm/needs-reply` `ttl: Keep` -> 3d read | 7d unread
  - result: work inbox 485 threads -> 149 in one run, 110 on 2026-11-12

### Problem

Stated goal, 2026-09-08: "ONLY the fucking most important emails to break through my attention."
Stated 2026-11-12: labels get applied but the item stays in the INBOX, "so I still get affronted by the noise."

Measured 2026-11-12, work account:

- 110 inbox threads. 100 are not Starred, not Important, not `llm/needs-reply`. 80 of those are older than a day.
- Triage runs once a day. Mail arriving at 09:00 sits unjudged until 06:30 the next morning.
- A classified thread still sits in INBOX until its bucket TTL: `noise` 1d, `recruiting` 3d, `receipts` 7d, `fyi-work` 3d read | 7d unread.
- `llm/needs-reply` is the one bucket meant to stay in the inbox, and bots keep landing in it. Top senders on 2026-11-11: Greenhouse 12, Workday 8, Gemini notes 6, GitHub 3. The bucket description already says "NEVER this bucket for automated/no-reply senders". A prompt is the only thing enforcing it.

Measured 2026-11-12, personal account (`scott.a.idler@gmail.com`):

- 10,324 inbox threads, 9,314 unread. 7,901 messages older than a year.
- Inflow: 330 messages in 30 days, about 11 a day.
- 90-day sample (770 of 1,048 messages fetched, the rest rate-limited):
  - Gmail category: Updates 595 | Promotions 78 | Personal 66 | Forums 31
  - top senders: github.com 128, monarch 64, docusign 45, reddit 39, amazon 34, substack 32
  - about 63 messages are human or consequential: family, two law firms, a title company, DocuSign
  - 3 Important, 0 Starred
- `eratosthenes` has no config for it. Nothing ages. 12 hand-made Gmail filters are the only rules.

Defects found 2026-11-11 while applying the config fixes:

- D1: the rule that removes `llm/needs-reply` from an answered thread lives in `refresh_drafts` and returns early when no bucket sets `draft: true` (`src/triage/mod.rs:742`). The live config has had `draft: false` since September. The rule has not run since.
- D2: `eratosthenes service reinstall` left `eratosthenes.timer` in `active (elapsed)` with `Trigger: n/a`. The 5-minute engine stopped until the service was started by hand.
- D3: `README.md` documents a digest "Needs Reply" section that commit `2d54930` removed. `eratosthenes.example.yml` still shows `keep-needs-reply: ttl: Keep` and claims the engine clears the label once answered.
- D4: a thread carrying both a message-filter destination (`Bots`) and an `llm/*` label ages on to Purgatory, because bucket state filters match on the label alone. 87 threads on 2026-11-11.
- D5 (review panel, 2026-11-12): a state filter whose TTL has not expired does not own its thread. `evaluate_thread` falls through to later filters unless the filter is `Keep` (`src/engine.rs:1236-1247`). The live config's comment says a bucket filter ahead of `Cull` can apply a looser TTL. It cannot: a read `llm/receipts` thread, 4 days old, skips `age-receipts` (7d) and is taken by `Cull` (read 3d).

### Goals

Each goal names who asked.

- G1: a thread classified into an archiving bucket leaves INBOX at label time. (Scott, 2026-11-12: "Would it make sense to actually remove items from the INBOX?")
- G2: a human mail addressed only to me is never auto-archived, whatever the model says. (Guard for G1. Same line Scott drew for drafts on 2026-10-02: "never draft for anything other than an only-to-me scenario.")
- G3: an automated sender cannot hold a place in the inbox through `needs-reply`, and a thread I answered stops claiming one. (Scott's own bucket description, 2026-09-25: "NEVER this bucket for automated/no-reply senders." Also "kill all greenhouse reminders".)
- G4: new mail is judged within the hour. (Follows from G1: archiving is worth little if mail waits a day to be labeled.)
- G5: a read-only report of who still reaches the inbox, so the next filter rule is obvious. (Scott, 2026-11-12: "what can be done to control/mitigate the barrage/volume/noise?" Proposed in answer; Scott confirmed it 2026-11-12.)
- G6: the personal account runs under `eratosthenes` and its backlog is cleared. (Scott, 2026-11-12: "also consider the disaster that is my personal email box too.")
- G7: fix D1, D2, D3, D5. (D1 to D3 found 2026-11-11, D5 by the review panel. D1, D3 and D5 are claims the config and README make that the code does not keep. D2 stops the engine. D5 also blocks G6: without it Personal-category mail reaches Oblivion.)

### Non-Goals

Excluded:

- Changing what the digest posts. It reads Starred + Important and nothing else (`src/lib.rs:234-259`). Scott ruled on 2026-09-08 that a model-picked digest section is wrong.
- Re-enabling reply drafts. `draft: false` stays.
- Sending mail. `tests/no_send_guard.rs` stays.
- LLM triage on the personal account. Deterministic rules first (Alternative 4).
- Fixing D4. Accepted behavior, documented (Resolved Decisions).
- Changing the triage `TimeoutStartSec` derivation (`src/service.rs:59-63`). Per-call timeouts already bound a wedged run.

Parked, each with a revisit condition (Addendum):

- A Jev stage in triage.
- Writing Gmail's own server-side filters from config.
- Per-arrival triage inside the 5-minute `run`.

## Proposed Solution

### Overview

- Bucket config gains two keys: `archive` and `awaits-reply`.
- A state filter that matches a thread owns it: no fall-through to a later filter while its TTL runs (D5).
- At label time, triage removes INBOX from the classified messages of a thread, when the archive decision says so.
- The archive decision is pure code over headers and labels. The model picks the bucket and nothing else.
- Triage runs the account's message filters before it classifies, and classifies only mail that existed when that pass started. A Star or Flag filter has seen every mail the archive decision reads.
- Archived threads keep their bucket label and age to Purgatory -> Oblivion on today's bucket TTLs. No new aging code: bucket state filters already match on the label alone (`src/cfg/state.rs:116-121`).
- A reply to an archived thread comes back on its own: Gmail puts INBOX on the new message, it has no `llm/seen`, so it is a triage candidate again (`CANDIDATE_QUERY`, `src/triage/mod.rs:38`).
- Triage schedule goes hourly. Config only.
- The answered rule moves out of the draft pass and covers `awaits-reply` buckets.
- `eratosthenes senders` lists top senders in the inbox.
- Personal account: a `home.yml` of state filters on Gmail's category labels. No message filters, no triage block, no slack block.

### Architecture

Triage pass, per account, after the change:

```
T0 = now
message filters (the same pass `run` does)                 NEW, runs first, live runs only
discover candidates (in:inbox -label:llm/seen)             CHANGED: skip messages dated at or after T0
  -> fetch threads (format=full)                           unchanged
  -> classify (Haiku, one call)                            unchanged
  -> per thread: bucket label write (thread-level)         unchanged
  -> per thread: archive decision                          NEW
  -> seen marker, message-level, two batches               CHANGED
       stay set:    add [llm/seen]
       archive set: add [llm/seen], remove [INBOX]
  -> answered sweep over awaits-reply buckets              NEW (moved out of refresh_drafts)
  -> refresh_drafts (only when a bucket has draft: true)   unchanged
```

The archive decision, in order. First match wins:

| # | Condition | Result |
|---|-----------|--------|
| 1 | any message in the thread carries `STARRED` or `IMPORTANT` | stay (`pinned`) |
| 2 | the thread has no inbound message (every message is from me) | stay (`no-inbound`) |
| 3 | any inbound message is from a non-automated sender and is solely to me, or has no parseable To | stay (`sole-human`) |
| 4 | bucket is `awaits-reply` and the newest inbound sender is automated | archive (`automated`) |
| 5 | bucket has `archive: false` | stay |
| 6 | otherwise | archive |

- Row 3 reads every inbound message, not only the newest. An older human mail to me alone, followed by a bot notice on the same thread, still holds the thread.
- Row 3 fails closed. `is_solely_to` treats a missing To as "not sole" because it gates drafting. Reusing that for archiving would flip the safety, so a non-automated message with no parseable To protects the thread.
- Row 4 sits ahead of row 5 on purpose: it is the only way G3 overrides `archive: false`.
- A thread with a message the filter pass skipped this run (skip ledger) is not classified this run. Same rule the draft pass already follows.

Definitions (pure functions in `src/triage/thread.rs`):

- Inbound message: a non-draft message not from me. `plan_refresh` (`src/triage/draft.rs:81-90`) picks the newest non-draft message including my own, so this is a new helper, not a reuse.
- Solely to me: Cc empty, To has exactly one address, and it is mine after normalization.
  - normalization: lowercase, drop a `+tag`, and for `gmail.com` | `googlemail.com` drop dots in the local part
  - neither account has a send-as alias (`users.settings.sendAs.list`, 2026-11-12), so config carries no alias list
  - `is_from_self` and `is_solely_to` (`src/triage/thread.rs:117-138`) switch to the same normalization, or rows 2 and 3 disagree on one address
- Automated sender, header-only, any of:
  - `List-Id` present
  - `List-Unsubscribe` present
  - `Auto-Submitted` present and not `no`
  - `Precedence` is `bulk` | `list` | `junk`
  - From local part, with `-`, `_`, `.` removed, contains `noreply` or `donotreply`, or equals `mailerdaemon`

What "stay" means: the thread keeps its bucket label, stays in INBOX, and ages on the bucket TTL. That is today's behavior, so the worst case of a wrong "stay" is what happens now.

Why INBOX removal rides the seen-marker batch and not the bucket write:

- The bucket write is `threads.modify`, which touches every message in the thread at call time (`src/triage/mod.rs:399-427`).
- A reply landing between the fetch (`mod.rs:341`) and that write would be archived unclassified. `CANDIDATE_QUERY` is `in:inbox`, so it would never come back.
- The seen marker is already message-level over the classified message ids for this reason (`mod.rs:125-134`, `mod.rs:434-455`). INBOX removal has the same race and takes the same shape.
- A failed batch leaves the bucket label written and the messages unseen and in INBOX. The next run reclassifies.
- `batch_modify` chunks at 1,000 ids (`src/gmail/client.rs:491`), so above that an early chunk can land before a later one fails. The cap is 100 threads a run, one chunk in practice. Either way a message is archived only together with its seen marker.

Why triage runs message filters first:

- Message filters require `label: INBOX`. `run` gets to a new mail within 5 min.
- Without this step: a mail from the CEO, Cc'd to a list, arrives at 06:29. Triage at 06:30 files it `fyi-work` and archives it. The `leadership` Star filter never sees it.
- The filter pass alone leaves a gap: a mail landing after the filter queries and before candidate discovery. So triage records T0 before the filter pass, and discovery skips any message with `internalDate` at or after T0. The next hourly run takes it.
- This does not depend on the `run` timer being healthy, and D2 showed that timer can stall.
- `run` may execute the same filters at the same moment in another process. Label writes are idempotent and a concurrently created label is re-resolved (`src/gmail/label.rs:133-150`). Each process has its own rate limiter, so the two together can spend up to twice 250 units/s for the length of the pass.

Wiring, the steps most likely to be skipped:

- `execute_message_filters` (`src/engine.rs:310`) is private. It becomes `pub(crate)`. `triage::execute` already receives `&Config`.
- Header guards: `lib::run` asks the client to fetch every header a filter references (`src/lib.rs:103-116`). `lib::triage` (`src/lib.rs:427-437`) does not. Without it `List-Id` is never fetched, `MessageFilter::matches` reads it as absent, and the `leadership` and `only-me-ttv` guards pass on list mail. Lift that block into one function both callers use.
- Labels: `execute_message_filters` needs filter destinations and the marker registered by `ensure_labels` (`src/engine.rs:145`), or Gmail gets a label name where it wants an id. Live triage calls `ensure_labels` first.
- Dry run: `ensure_labels` creates labels, and `triage --dry-run` promises zero Gmail mutations (`mod.rs:286-293`). So a dry run skips the filter pass entirely.

State filter ownership (D5):

- Change in `evaluate_thread`: a filter that matches the thread's labels owns the thread while its TTL runs, and evaluation ends there. Today only `Keep` does that.
- One exception, before and after expiry: when the filter's planned move is already satisfied (`plan_state_move` returns a no-op: the thread is at or past the destination stage, `src/engine.rs:1302-1341`), the filter does not own the thread and evaluation falls through. That is how a bucket-labeled thread in Purgatory reaches `Purge` on `Purge`'s own 3 days.
  - Without the exception (panel round 2): an unread thread with Promotions and Updates messages enters Purgatory at day 1, and `updates` (7d) would hold it there until day 7 where `Purge` takes it at day 3 today.
- Effect on the live work config, inbox timing: read `llm/receipts` mail holds 7 days where `Cull` took it at 3. Nothing else moves: `Cull`'s unread TTL is 21d, longer than every bucket's, so no other bucket was being taken early.
- Purgatory and Oblivion age exactly as today.
- Effect on the personal config: a Personal-category thread is owned by `personal` and never reaches `Cull`.

Engine (`run`), for the personal account:

- A state filter on a Gmail category label works for inbox threads: they are in the active set through `in:inbox`.
- `build_active_threads_query` (`src/engine.rs:1414`) also emits `label:category_promotions` for it. Gmail answers that with zero results (personal account, 2026-11-12: `label:category_promotions in:inbox` -> 0, `category:promotions in:inbox` -> 201 estimated).
- The fix is to emit no clause for a category label. Emitting `category:promotions` would pull every Promotions thread in the mailbox, Oblivion included, into the active set of every 5-minute run. The engine hit Gmail 429s on active-set size on 2026-11-01.
- A category filter is then discovered through `in:inbox` only. It can still be evaluated on a Purgatory or Oblivion thread that reaches the active set through a stage clause; the backward-move guard makes that a no-op.
- `Label::new` uppercases a `category_*` name. Lowercase in config would otherwise never match and `ensure_labels` would create a user label with that name (`src/cfg/label.rs:18-31`).

### Data Model

`TriageBucket` (`src/cfg/triage.rs:10-19`) gains two keys:

```yaml
triage:
  schedule: '*-*-* *:30:00'
  buckets:
    - name: needs-reply
      label: llm/needs-reply
      draft: false
      archive: false        # stays in INBOX
      awaits-reply: true    # a bot is archived anyway; my reply clears the label
    - name: fyi-work
      label: llm/fyi-work
      archive: true
    - name: recruiting
      label: llm/recruiting
      archive: true
    - name: receipts
      label: llm/receipts
      archive: true
    - name: noise
      label: llm/noise
      archive: true
```

- Both keys default to `false`. An existing config behaves as today until it opts in.
- `archive: true` with `draft: true` is a config error naming the bucket: the draft collector lists INBOX + label (`mod.rs:699-702`), so an archived bucket could never get a draft.
- `awaits-reply: true` with `archive: true` is a config error: neither consequence of the key applies there.
- `awaits-reply` means one thing: a human is waiting on me. Two consequences follow: a bot cannot be waiting (row 4), and once I reply nobody is (the answered sweep).
- `TriageBucket` and `TriageConfig` get `deny_unknown_fields`. A typo like `archvie: true` is silently ignored today (2026-11-12: `config validate` exits 0 on an unknown triage key). For a key that removes mail from the inbox, a silent no-op is the wrong failure.
- Archive does not remove `UNREAD`. `Ttl::Detailed` keys on read state; marking mail read that I never read would shorten its TTL and make the label lie.

Personal account starter config (`home.yml`). The TTL values are Scott's to edit:

```yaml
auth:
  creds-path: home
  callback-port: 13132        # must differ from tatari's default 13131

state-filters:
  - Starred:    { labels: [Starred],   ttl: Keep }
  - Important:  { labels: [Important], ttl: Keep }
  # First of the categories, so a thread mixing Personal with anything else
  # is owned here. No action: leaves the inbox, gets no stage label.
  - personal:
      label: CATEGORY_PERSONAL
      ttl: { read: 7d, unread: 30d }
  - promotions: { label: CATEGORY_PROMOTIONS, ttl: 1d, action: Purgatory }
  - social:     { label: CATEGORY_SOCIAL,     ttl: 1d, action: Purgatory }
  - forums:     { label: CATEGORY_FORUMS,     ttl: 3d, action: Purgatory }
  - updates:
      label: CATEGORY_UPDATES
      ttl: { read: 3d, unread: 7d }
      action: Purgatory
  - Cull:
      label: INBOX
      ttl: { read: 3d, unread: 21d }
      action: Purgatory
  - Purge:      { label: Purgatory, ttl: 3d, action: Oblivion }
```

- No message filters. Gmail's categories do the sorting; `senders` shows what a filter should be written for.
- Personal-category mail (family, law firms, title company) is archived and stops there. It never gets a stage label, so a bulk trash of Oblivion cannot take it. This holds only with the D5 ownership change: today a read Personal thread, 4 days old, would fall through to `Cull`.
- No `triage:` and no `slack:` block, so the single triage and digest timers keep reading `tatari` alone. `home` sorts before `tatari`; with a triage block it would take over the timer schedule (`resolve_triage_schedule`, `src/service.rs:343`).
- First live run: about 9,400 threads go to Purgatory. TTL age counts from a thread's newest message, not from when it entered the stage (`src/cfg/state.rs:123`), so every backlog thread older than 3 days moves on to Oblivion on the next run, 5 minutes later. The Personal-category threads (924 in the mailbox) are archived. All of it stays searchable.
- The 9,400 is 10,324 inbox threads minus the Personal category. Phase 0c replaces it with the dry run's own count.

### API Design

CLI:

```
eratosthenes senders [accounts...] [--days <N>] [--limit <N>] [--format yaml|json]
```

- Read-only. Lists senders of mail in INBOX from the last `--days` (default 7), top `--limit` (default 25).
- Per sender: address, messages, threads, unread, automated (the guard's own predicate), bucket split.
- Output: yaml on a TTY, json when piped, `--format` overrides.

Triage `--dry-run` output gains one column per thread: `archive` | `archive (automated)` | `stay` | `stay (pinned|no-inbound|sole-human)`.

- A dry run skips the filter pass (it would create labels). A fresh mail a Star filter would catch can show `archive` in a dry run and `stay (pinned)` live.

Triage summary line:

```
Triage: 76 threads classified, 76 labeled, 61 archived, 4 protected, 0 skipped
```

Rust:

```rust
// src/triage/thread.rs
pub enum Protection { Pinned, NoInbound, SoleHuman }

impl TriageThread {
    pub fn inbound(&self, self_address: &str) -> impl Iterator<Item = &TriageMessage>;
    pub fn protection(&self, self_address: &str) -> Option<Protection>;
}
impl TriageMessage {
    pub fn is_automated(&self) -> bool;
}
pub fn normalize_address(addr: &str) -> String;

// src/triage/mod.rs
pub enum ArchiveDecision { Archive, ArchiveAutomated, Stay, StayProtected(Protection) }
pub fn plan_archive(thread: &TriageThread, bucket: &TriageBucket, self_address: &str) -> ArchiveDecision;
```

### Implementation Plan

#### Phase 0: Spikes, zero code
**Model:** sonnet
- 0a: reproduce D2 with throwaway user timers, replaying the `reinstall` sequence (stop, disable, remove units, reload, write, `enable --now`). Test the candidate fix.
- 0b: personal OAuth. `eratosthenes -c <path outside ~/.config/eratosthenes>/home.yml auth login` with `callback-port: 13132`. Operator step: needs a browser.
- 0c: `eratosthenes -c <same path> run --dry-run` against the starter `home.yml`.
- **Success criteria:**
  - 0a: the stall appears on the current timer shape and not on the fixed shape
  - 0b: `eratosthenes -c <path> auth status` reports authenticated
  - 0c: the dry run, at `--log-level debug`, exits 0 and its log holds at least one `[state:promotions]` line and one `[state:updates]` line (proves a category label matches thread labels; `run` prints no per-filter count at INFO)

**0a result (2026-11-12, ripr, systemd 259.9, PASSED):**

Four throwaway user timers over a `/bin/true` oneshot, each put through first install, then the `reinstall` sequence. `runs` counts completed service runs.

| Shape | `[Timer]` lines | After first install | After reinstall, +52s |
|---|---|---|---|
| a (today's run timer) | `OnBootSec=2min`, `OnUnitActiveSec=20s`, `Persistent=true` | fires, runs=1 | `elapsed`, runs=1: STALLED |
| b | a + `OnActiveSec=1s` | fires | `waiting`, runs climbing |
| d | a without `Persistent=true` | fires, runs=1 | `running`, runs=3 |
| f | `OnActiveSec=5s`, `OnUnitActiveSec=20s` | fires, runs=1 | `waiting`, runs=3 |

Follow-up, one unit installed as shape a and then reinstalled as shape d, with the stamp file from shape a left on disk: fires after every reinstall (runs 1 -> 2 -> 3 -> 4).

- Cause: `Persistent=true`. Shapes a and d differ by that line alone; a stalls and d does not.
- `man systemd.timer` says `Persistent=` "only has an effect on timers configured with OnCalendar=". On systemd 259.9 that is not what happens: the monotonic-only shape a writes `~/.local/share/systemd/timers/stamp-<unit>.timer`, and with the line present a re-enabled timer does not fire its already-elapsed `OnBootSec`.
- Fix for Phase 6: delete `Persistent=true` from the generated run timer (`src/service.rs:206-221`). The line was doing nothing the manual promises for this timer shape and is the one cause of the stall.
- The digest and triage timers are `OnCalendar` timers, where `Persistent=` does what the manual says. They are not touched.

0b and 0c: not run. Both need Scott at a browser.

#### Phase 1: Docs and config plumbing
**Model:** sonnet
- README: remove the digest "Needs Reply" text (D3). Document `archive`, `awaits-reply`, the archive decision table, state filter ownership, and D4 as accepted behavior.
- `archive` and `awaits-reply` on `TriageBucket`; `deny_unknown_fields` on both triage structs; the two config errors.
- `Label::new` uppercases a `category_*` name.
- **Success criteria:**
  - `cargo test --test triage_config` passes with new named cases: `archive_with_draft_is_rejected`, `awaits_reply_with_archive_is_rejected`, `unknown_bucket_key_is_rejected`
  - `eratosthenes -c <fixture> config validate` exits non-zero when a bucket sets `archive: true` and `draft: true` (exit 0 on main, 2026-11-12: the key is unknown and ignored)
  - `rg -c 'Needs Reply' README.md` prints nothing (5 matching lines on main)

#### Phase 2: State filter ownership (D5)
**Model:** opus
- `evaluate_thread`: a matching filter with an unexpired TTL ends evaluation, unless its planned move is already a no-op, which falls through before and after expiry.
- Fix the live-config comment's claim in `eratosthenes.example.yml` to match.
- **Success criteria:**
  - `cargo test engine` passes new named cases: `unexpired_bucket_filter_owns_thread_ahead_of_cull` (read `llm/receipts` thread, 4 days old, 7d bucket TTL, `Cull` read 3d: no move) and `personal_category_thread_never_gets_a_stage_label` (two consecutive runs over a read CATEGORY_PERSONAL thread, 4 days then 8 days old: no move, then INBOX removed with no stage label added)
  - `noop_move_still_falls_through_to_purge` passes: a bucket-labeled thread in Purgatory past both TTLs still reaches Oblivion
  - `unexpired_filter_does_not_hold_a_thread_already_in_purgatory` passes: an unread CATEGORY_UPDATES thread in Purgatory, 4 days old, 7d filter TTL, `Purge` 3d: moves to Oblivion
  - break-to-prove: restoring the fall-through makes the first case fail

#### Phase 3: Header facts and the archive decision
**Model:** opus
- Add `List-Id`, `List-Unsubscribe`, `Auto-Submitted`, `Precedence` to `TRIAGE_HEADERS`. They do not reach the prompt: `PayloadMessage` (`src/triage/classify.rs:24-32`) is unchanged.
- `normalize_address` (also used by `is_from_self` and `is_solely_to`), `is_automated`, `inbound`, `protection`, `plan_archive`. Pure functions, no Gmail calls.
- **Success criteria:**
  - `cargo test triage::thread` passes a positive and a negative case per automated-sender fact and per `Protection` variant, including `non_automated_message_without_to_protects_thread` and `older_sole_human_message_protects_thread_under_newer_bot_message`
  - `cargo test plan_archive_row` runs six tests, `plan_archive_row_1` to `plan_archive_row_6`, one per table row, and all pass
  - senders measured on 2026-11-11 are pinned as cases: `no-reply@greenhouse.io` and `drive-shares-dm-noreply@google.com` automated; `nick.andolina@tatari.tv` with no list headers not automated

#### Phase 4: Archive write path
**Model:** opus
- One function builds the metadata-header list from the filters; `lib::run` and `lib::triage` both call it.
- Live triage: `ensure_labels`, record T0, then `execute_message_filters` (now `pub(crate)`). Dry run skips all three.
- Discovery skips messages dated at or after T0. A thread with a message in the skip ledger is not classified this run.
- Split the seen-marker `batch_modify` into the stay set and the archive set.
- Dry run prints the archive column and writes nothing.
- Summary line reports archived and protected counts.
- **Success criteria:**
  - `cargo test --test triage_archive` passes: an archiving bucket sends `batchModify` with `removeLabelIds: ["INBOX"]` for the classified ids only; a message that arrived after the fetch is in neither batch
  - same target: a sole-to-me human thread classified `noise` produces no INBOX removal; a `no-reply` thread classified into an `awaits-reply` bucket does
  - same target: `list_mail_is_not_starred_by_a_list_id_guarded_filter_in_triage` (the M2 case) and `message_dated_after_filter_pass_start_is_not_classified` (the M4 case)
  - same target: a dry run issues zero non-GET requests
  - break-to-prove: deleting the `plan_archive` call makes a named test fail

#### Phase 5: Answered sweep
**Model:** opus
- Move the answered rule out of `refresh_drafts`: a sweep over INBOX threads carrying an `awaits-reply` bucket label, before the `draft_buckets.is_empty()` return. Metadata fetch is enough.
- Buckets without `awaits-reply` are untouched, so a config that has not opted in sees no change. The draft pass keeps its own answered arm for `draft: true` buckets.
- Remove dead `NEEDS_REPLY_BUCKET` (`mod.rs:44`).
- `eratosthenes.example.yml`: needs-reply on a TTL, the new keys, and an answered-rule comment that is now true.
- **Success criteria:**
  - `cargo test --test triage_isolation` gains `answered_awaits_reply_thread_loses_label_with_draft_false`: exactly one `threads.modify` removing the bucket label
  - same target: `answered_thread_in_plain_bucket_keeps_label`
  - `rg -c 'keep-needs-reply' eratosthenes.example.yml` prints nothing (1 matching line on main)

#### Phase 6: Timer fix and category clause
**Model:** sonnet
- D2: delete `Persistent=true` from the generated run timer (`src/service.rs:206-221`). Shape d in the Phase 0a result.
- `test_generate_timer` (`src/service.rs:938-946`) asserts `Persistent=true` today. Replace that assertion; do not add a second test beside it.
- `build_active_threads_query` emits no clause for a `CATEGORY_*` label.
- **Success criteria:**
  - `cargo test service` asserts the generated run timer's `[Timer]` section is exactly `OnBootSec=2min` and `OnUnitActiveSec=<interval>`, with no `Persistent=` line
  - `cargo test engine` asserts a `CATEGORY_PROMOTIONS` state filter adds no `label:category_` clause to the active query

#### Phase 7: `senders`
**Model:** sonnet
- New subcommand on the `Command` enum (`src/cli.rs:30`). Reuses `is_automated`.
- **Success criteria:**
  - wiremock test: `senders` issues zero non-GET requests
  - `eratosthenes senders --help` exits 0 (exit 2 on main)

#### Phase 8: Ship and turn on, work account
**Model:** sonnet
- Release. Then operator steps, each recorded in the implementation notes:
  - `eratosthenes config validate` on the live config with the new binary (catches a key `deny_unknown_fields` now rejects)
  - `eratosthenes triage --dry-run` from a config copy outside the config dir, with the new keys set. Scott reads only the rows marked `archive`. Gate: more than 2 rows he wants in the inbox, out of the run, and `fyi-work` stays `archive: false` until the cause is found.
  - edit `tatari.yml`: `archive`, `awaits-reply`, hourly schedule
  - `eratosthenes service reinstall`
- **Success criteria:**
  - `systemctl --user show eratosthenes.timer -p SubState --value` prints `waiting` or `running` one minute after the reinstall, with the service never started by hand
  - within a day, the journal for `eratosthenes-triage.service` holds a `Triage:` line with a non-zero archived count

#### Phase 9: Personal account
**Model:** sonnet
- Operator steps: Phase 0b and 0c, then `home.yml` into dotfiles and a symlink into the config dir. The file is live the moment it is in the config dir.
- **Success criteria:**
  - `eratosthenes config validate` exits 0 with both accounts
  - the first live run's `Done:` line for `[home]` is pasted into the implementation notes with 0 skipped
  - after the second live run: `gws-home gmail users threads list` for `category:personal {label:purgatory label:oblivion}` returns zero threads

## Acceptance Criteria

- [ ] 1. `cargo test --test triage_archive` exits 0.
  - Observed on main, 2026-11-12: exit 101, `no test target named triage_archive`. Cannot pass until Phase 4.
- [ ] 2. After `eratosthenes service reinstall`, with the service never started by hand: one minute later `systemctl --user show eratosthenes.timer -p SubState --value` prints `waiting` or `running`, and within 12 minutes the journal for `eratosthenes.service` holds two `Finished` lines dated after the reinstall.
  - Observed on v0.7.8, 2026-11-11: `Active: active (elapsed)`, `Trigger: n/a`. Not re-run today: the command stalls the live engine. The same sequence on throwaway timers is in the Phase 0a result.
- [ ] 3. Work account, two days after Phase 8, with `<date>` set to the day the config went live: `in:inbox -is:starred -is:important -label:llm-needs-reply older_than:1d after:<date>` returns at most 10 threads.
  - Observed on main, 2026-11-12, without the `after:` term: 80 threads (`gws gmail users threads list`, count of `.threads`).
  - The `after:` term is there because triage never revisits a thread already marked `llm/seen`. Mail labeled before the turn-on leaves on its bucket TTL, 7 days at most.
- [ ] 4. Personal account, after Phase 9's second live run: INBOX `threadsTotal` is under 500.
  - Observed on main, 2026-11-12: 10324 (`gws-home gmail users labels get`, `.threadsTotal`).
- [ ] 5. Personal account, after Phase 9's second live run: zero threads match `category:personal {label:purgatory label:oblivion}`.
  - Observed on main, 2026-11-12: neither label exists on the account yet (2 user labels: `Football`, `Nate`), so the query cannot be run. Criterion 4 can pass while Personal mail is being sent to Oblivion; this one cannot.

## Resolved Decisions

- 2026-11-12, Scott: the Jev stage and server-side Gmail filters stay parked (Addendum). Each gets its own doc when its revisit condition is met.
- 2026-11-12, Scott: `fyi-work` archives from the first live run, with the other three buckets. The Phase 8 dry-run read is the gate.
- 2026-11-12, Scott: full bankruptcy on the personal account. The first two live runs clear the backlog; nothing is deleted.
- 2026-11-12, Scott: `senders` (G5, Phase 7) stays.
- 2026-11-12: D4 is accepted behavior. A thread with a message-filter destination and a bucket label ages on the bucket TTL. Both review seats agree.
  - Filters move messages; triage labels threads. A thread holding one moved bot message and one inbox message still gets both labels, for new mail too.
  - The 87 threads on 2026-11-11 came from adding a filter over an already-labeled backlog.
  - The cost is bot mail reaching Oblivion on a bucket TTL where it would have sat in `Bots` for good.
  - The config-only fix (a `Keep` state filter on `Bots`) puts 1,325 Bots threads into the active set of every 5-minute run for good.
- 2026-11-12: archive does not mark read. Reason in Data Model.
- 2026-11-12: archived threads age on today's bucket TTLs. No new stage. It is the end state mail reaches today, minus the days in the inbox.
- 2026-11-12: no alias list in config. Neither account has a send-as alias.
- 2026-11-12: after the answered sweep removes a label, the thread stays in INBOX under `Cull`. I just replied to it; 3 days read is the right leash.
- 2026-11-12 (panel M6): the answered sweep covers `awaits-reply` buckets only. Covering every non-archiving bucket would strip labels from every bucket of a config that has not opted in.
- 2026-11-12 (panel M1): state filters own their threads (D5). Reordering the personal config alone cannot protect Personal mail; no config expresses "archive at 7d and never fall through" today.
- 2026-11-12 (panel W4, C2): the sole-human row reads every inbound message and fails closed on a missing To.
- 2026-11-12 (panel C5): both seats agree nothing in the Addendum is needed for the core design.
- 2026-11-12 (panel round 2): both seats withdrew M7 after the A/B result. Deleting `Persistent=true` stands. The staff seat cites systemd 259.9 `timer.c`: `timer_setup_persistent` writes the stamp without checking for `OnCalendar`.
- 2026-11-12 (panel round 2): ownership yields to a no-op move (see State filter ownership). Purgatory keeps aging on `Purge`'s TTL.
- 2026-11-12 (panel round 2, noted, no change): a reply to a thread already in Oblivion lands in INBOX with both labels. Stage sanitization (`sanitize_stages`, `src/engine.rs:223`) keeps the earliest stage and strips the later one, so the thread ages again from INBOX. Older than this doc; the reply is visible either way.
- 2026-11-12: a category state filter emits no active-set clause. Reason in Architecture.
- 2026-11-12: D2 fix is deleting `Persistent=true`, not adding `OnActiveSec=` and not a service start after `enable --now`. It removes the cause; the other two work around it. Measured in Phase 0a. (Panel M7 said the stamp file could not be the cause, citing the manual. The A/B result says it is.)

## Alternatives Considered

### Alternative 1: `ttl: 0d` on the bucket state filters
- **Description:** config only. A bucket state filter with `ttl: 0d` moves the thread out on the next 5-minute run.
- **Pros:** zero code.
- **Cons:** thread-level, every 5 min. A human reply into an archived-bucket thread is re-archived before triage sees it. No guard.
- **Why not chosen:** it hides replies. That is the one failure this design cannot have.

### Alternative 2: remove INBOX in the bucket write
- **Description:** add INBOX to `ThreadWrite.remove` in `plan_write`.
- **Pros:** smallest diff.
- **Cons:** `threads.modify` archives a reply that arrived mid-pass, unclassified and unseen.
- **Why not chosen:** the code already documents this race for the seen marker.

### Alternative 3: let the model decide protection
- **Description:** add "is this a human writing only to Scott" to the bucket descriptions.
- **Pros:** no header code.
- **Cons:** this is what the needs-reply description does today, and Greenhouse, Workday and Gemini notes are its top senders.
- **Why not chosen:** header facts are exact. A prompt is not.

### Alternative 4: LLM triage on the personal account
- **Description:** give `home.yml` a triage block.
- **Pros:** one model for both accounts.
- **Cons:** personal mail through the company Claude login. 704 of 770 sampled messages already carry a Gmail category other than Personal. `home` would take over the single triage timer's schedule.
- **Why not chosen:** deterministic rules cover it. Revisit if `senders` on the personal account shows a steady stream the categories miss.

### Alternative 5: one triage timer per account
- **Description:** templated units, a schedule per account.
- **Pros:** accounts stop sharing a schedule.
- **Cons:** code for a problem that needs two triage-enabled accounts. This design has one.
- **Why not chosen:** N=1. Revisit with Alternative 4.

### Alternative 6: skip candidates younger than 10 minutes
- **Description:** instead of running message filters inside triage, leave a fresh thread for the next hourly run so `run` gets to it first.
- **Pros:** no cross-module call.
- **Cons:** depends on the `run` timer being alive. D2 is that timer dying.
- **Why not chosen:** ordering by wall clock is a hope. Ordering by call sequence is a fact.

### Alternative 7: reject a bot's needs-reply assignment
- **Description:** when the model puts an automated sender in a `awaits-reply` bucket, drop the bucket label.
- **Pros:** the label never lies.
- **Cons:** the thread is then unlabeled in INBOX under `Cull` for up to 21 days.
- **Why not chosen:** it keeps the bot in view longer than today does.

## Technical Considerations

### Dependencies
- No new crates.
- Repos touched: `scottidler/eratosthenes` (code), `scottidler/dotfiles` (`tatari.yml`, new `home.yml`).
- Ship order: eratosthenes release and install on ripr first, then the dotfiles config.
  - A config carrying the new keys loads on the old binary and does nothing (unknown keys are ignored today).
  - The new binary rejects unknown triage keys, so `config validate` on the live config runs before `service reinstall`.

### Performance
- Hourly triage with no candidates costs the label check, the message-filter queries, one `messages.list`, and exits before any LLM call (`mod.rs:333`).
- With candidates: one `messages.get` per candidate message, then one `claude` call. 2026-11-12 daily run: 76 threads, under the 100 cap.
- Archive adds one `batchModify` per run.
- Answered sweep: one metadata `threads.get` per INBOX thread in a non-archiving bucket, each run. 6 such threads on 2026-11-11.
- `senders`: one metadata `messages.get` per inbox message in the window.
- Personal first run: about 10,000 `threads.get` at 10 units each through the existing limiter (250 units/s). After that the per-thread history cache carries it.

### Security
- No new credential, no new egress. The four new headers stay out of the prompt.
- The personal account gets its own token cache under `~/.config/eratosthenes/home/`. Scope stays `https://mail.google.com/`.

### Testing Strategy
- Archive decision: unit tests per table row, measured senders as fixtures.
- Write path: wiremock at the Gmail seam (`tests/common/mod.rs`), stub `claude` binary as in `tests/triage_isolation.rs`.
- Break-to-prove on the `plan_archive` call and on the D5 ownership change.
- Live: the Phase 8 dry-run eval before the keys go into the live config.

### Rollout Plan
- Both keys default to false, so shipping the binary archives nothing. It still changes four things for the live config:
  - state filters own their threads (D5): read `llm/receipts` mail stays 7 days where `Cull` took it at 3
  - live triage runs the message filters first
  - unknown triage keys are rejected
  - the run timer loses `Persistent=true` and re-arms after a reinstall
- Work: validate -> dry-run eval -> config edit -> reinstall -> watch one hourly run.
- Rollback: set `archive: false` on every bucket and `awaits-reply: false` on needs-reply. `archive: false` alone leaves row 4 archiving bots. Archived threads are one search away (`label:llm-fyi-work -in:inbox`).
- Personal: outside-path login and dry run -> move the file in.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Model files a human mail into an archiving bucket | Med | High | Row 5 keeps sole-to-me human mail in code; a reply re-surfaces the thread; rollback is one config key |
| Human mail to me plus a Cc, misfiled, is archived | Med | Med | Outside the guard by design (same line as the draft rule). The Phase 8 eval shows how often. `fyi-work` can stay `archive: false` |
| Vendor cold outreach is sole-to-me and human, so it stays | High | Low | Same as today: it ages on its bucket TTL. Named so nobody expects the guard to remove it |
| A non-automated bulk mail with no To (Bcc blast) is protected and stays | Low | Low | Row 3 fails closed on purpose. It ages on its bucket TTL as today |
| D5 changes aging on the live work config | High | Low | Stated in Rollout. Direction is longer holds for bucket mail the config already asked to hold; archive takes that mail out of the inbox anyway |
| A bot's approval task (Workday) is archived by row 4 | Med | Low | Scott's stated rule for the bucket. The mail keeps its `llm/needs-reply` label for 7 days and the source tool still holds the task |
| Triage down longer than a bucket TTL: a reply on an archived thread ages out unclassified | Low | Med | Existing risk, sharper now. The reply shows in INBOX the whole time |
| Personal first run trips Gmail quota | Low | Low | Limiter exists; `run` retries; the cache bounds later runs |
| `deny_unknown_fields` breaks the live config on upgrade | Low | Med | Phase 8 runs `config validate` before reinstall |
| Personal bankruptcy sends something wanted to Oblivion | Med | Med | Personal-category mail never gets a stage label (D5, criterion 5). Updates-category mail (bank, DocuSign notices) does, within minutes of the first run for old mail: do not bulk-trash Oblivion on this account without a look |

## Open Questions

None. Two panel rounds closed with no dispute left, and Scott ruled on the four decisions that were his (Resolved Decisions).

## Addendum: parked, not rejected

### Jev stage in triage
- What it is: TypeSafe System One ahead of Haiku. Code computes header facts, Jev answers atomic yes/no questions per thread, Haiku takes the uncertain band.
- Known:
  - one `state` per call, so one call per thread; 117 questions measured at 0.14 s
  - client to harvest in `second-brain/distillers/src/tags.rs`. It is blocking `ureq`; here it would be ported onto the hyper stack the way `HttpSlackPoster` was (`src/slack/mod.rs:63`)
  - pin the version, fail closed on a non-`jev` model
- What gates it:
  - no measurement of Jev on mail exists. The 2026-10-16 tag trial found context labels (`work` 0 of 6) invisible to it
  - it ends the keyless property in `docs/design/2026-07-06-llm-triage.md:73-77`: a key in the triage unit, mail text to a second vendor
  - hourly Haiku removes the backlog problem Jev's speed would solve
- Revisit condition: archive has run for two weeks. The archive decisions plus Scott's stars are then a labeled set to score Jev against, with no hand labeling. It is a biased set: an archived mistake gets no feedback unless Scott goes looking.

### Server-side Gmail filters from config
- What it is: compile sender-plus-subject message filters into Gmail's own filters so the mail never lands in INBOX.
- Known: `google-gmail1` already has `settings_filters_create`; both accounts already carry hand-made filters (work 2, personal 12).
- What gates it:
  - does the `https://mail.google.com/` grant cover filter create? The crate defaults to `gmail.settings.basic`
  - Gmail criteria are not globs
  - filters have no name, so engine-owned ones need an ownership scheme
  - a guard against `forward`
- Revisit condition: after G1 ships, mail visible for up to 5 min before a message filter acts is still a complaint.

### Per-arrival triage inside `run`
- What gates it: the run unit's PATH lacks `~/.local/bin`; `run` is parallel across accounts and assumes no LLM call; `claude` cold start for a one-thread batch is unmeasured.
- Revisit condition: hourly is not fast enough.

## References

- `docs/design/2026-07-06-llm-triage.md` (triage design, keyless property)
- `docs/design/2026-10-06-per-thread-error-isolation.md` (skip ledger)
- `docs/plan/2026-11-01-thread-history-cache.md` (active-set cost)
- `docs/eval/llm-triage-eval.md` (the 50-thread dry-run gate)
- Jev field guide: https://marquee.internal.tatari.dev/p/eng/typesafe-jev/
