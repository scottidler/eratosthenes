# Design Document: Per-Thread Error Isolation

**Author:** Scott Idler
**Date:** 2026-10-06
**Status:** Implemented
**Review Passes Completed:** 3/5 (Draft, Correctness, Edge Cases; Clarity and Excellence not run as separate passes). Review panel: round 1 folded in (see Resolved Decisions).

## Summary

One Gmail call failing on one thread aborts the whole account run, exits 1, and fires the ntfy alert. Gmail answers a small fraction of thread calls with a transient `400 FAILED_PRECONDITION` that clears by the next run, so the unit fails on a healthy mailbox. Fix: classify per-thread errors as thread-scoped or account-scoped, skip and count thread-scoped ones at the per-thread boundary with no further writes to that thread, keep failing loudly on account-scoped ones and on a skip ceiling, and stop the retry ladder from discarding the cause.

## Problem Statement

### Background

- `eratosthenes.service` runs every 5 min (`Type=oneshot`, `OnUnitActiveSec=5min`); Phase 2 does one `threads.get` per active thread, about 2,689 per run.
- The ntfy alert is new: dotfiles d947a42 (2026-10-05 06:21 PDT) added a global `~/.config/systemd/user/service.d/10-onfailure.conf` -> `notify-failure@%n.service`. The failures go back to 2026-09-04; they were invisible until then.
- Skip-and-log was already decided engine-wide and never built in the engine: `docs/design/2026-03-29-gmail-api-migration.md:586` ("skip and log on permanent errors"), `docs/design/2026-06-06-slack-digest.md:330`. The digest and triage candidate paths already do it (`src/lib.rs:205-209`, `src/triage/mod.rs:503-509`).

### Problem

Two failure classes, measured from the journal (2026-09-04 -> 2026-10-06, 6,821 runs) and the app log (`~/.local/share/eratosthenes/logs/tatari.log*`):

- **Class A: `400 FAILED_PRECONDITION` on one thread** (35 occurrences: 20 `threads.get`, 15 `threads.modify`)
  - Every one on a different thread id; no id ever failed twice.
  - All 15 `threads.modify` events predate the v0.7.4 install (last 2026-10-05 17:24 PDT), most on flip-flopping threads. Both events since are `threads.get`.
  - `threads.get` events are transient, verified: today's 19fa97828f656eb1 (12:16:24) and 19fd98c8db9ecfc1 (12:21:02) were fetched cleanly by the 12:24 run, and `gws` probes at `format=metadata` with the same headers succeed on all five sampled never-written threads. Transience of `threads.modify` events is not separately proven; the skip makes it moot (next run re-plans the write).
  - Not concurrency (oneshot, strict start -> finish ordering; triage runs 06:30 only, no overlap with any failure). Post-v0.7.4 write volume is 57 `modify_thread` calls today, so it does not explain the two recent gets.
  - 22 of 35 fell 12:00-15:00 PDT, zero 00:00-08:00. Google-side load is the likely driver; not proven, and the fix does not depend on it.
  - Path: `is_retryable` (`src/gmail/rate.rs:161`) treats a 400 as permanent -> `with_retry` returns it on attempt 1 -> `?` at `src/engine.rs:844` -> `src/main.rs:91` bails -> exit 1.
- **Class B: `<op> failed after 5 retries`** (about 150 occurrences)
  - Underlying errors (app-log WARNs from `rate.rs:222`): 88x `429 rateLimitExceeded "Retry after <now+15m>"`, 66x `429 Too many concurrent requests for user`, a few `503 backendError`, 2 transport timeouts, 2 DNS bursts.
  - Trigger: the Purgatory <-> Oblivion flip-flop, about 130k `modify_thread` calls per day, fixed in v0.7.4 (285cf0f, binary installed 2026-10-05 17:36:40 PDT). Zero 429s since 2026-10-06 00:56 UTC.
  - Residual defects that outlive the trigger:
    - `rate.rs:238` bails with a fresh error, dropping the cause; the journal only ever says "after 5 retries"
    - `rate.rs:57` logs "Rate limited" on every backoff, DNS included (the per-attempt WARN at `:222` already carries the cause; this is a wording fix)
- **Measured on main:** `journalctl --user -u eratosthenes.service --since '7 days ago' -g 'Failed with result'` -> 101 lines.
- The comment at `src/engine.rs:979-983` credits the flip-flop for the 400. Half wrong: 400s hit quiescent threads before and after v0.7.4.

### Goals

- A thread-scoped error on one thread never fails the run; the thread is skipped, logged at WARN with id, op and full error, gets no further writes this run, and is counted in the `Done:` line.
- Account-scoped errors (429, auth, `threads.list`, `labels.list`, retries exhausted, transport) still fail the run and alert.
- A systemic thread-scoped failure (many distinct threads in one run) still alerts, without first burning the whole run's calls.
- Retry exhaustion carries the underlying error into the journal.

### Non-Goals

- **Excluded:** silencing the alert or the OnFailure drop-in. The alert did its job.
- **Excluded:** an in-call retry of `FAILED_PRECONDITION`. Unproven to help within seconds; proven to clear by the next run, which the skip already gets for free.
- **Excluded:** changing triage candidate discovery, which already swallows every `get_message` error including account-scoped ones (`src/triage/mod.rs:500-510`). Pre-existing, retained as is.
- **Parked:** narrowing `build_active_threads_query` (`src/engine.rs:1117-1123`) to drop Move destinations no filter scopes on. Oblivion holds 2,471 of about 2,689 fetched threads, so this cuts about 92% of per-run reads. Not requested. Revisit if Class A skips show up more than once a day after this ships, or on a quota problem.
- **Parked:** Retry-After handling: both a fail-fast `LockedOut` on a 429 whose Retry-After is past the ladder, and persisting a lockout across runs (borg `blocklist.rs:172` shape). No requester, and the write storm that caused lockouts is gone (zero 429s since v0.7.4). Revisit if a 429 lockout recurs after v0.7.4.

## Proposed Solution

### Overview

1. `rate.rs`: keep the cause on retry exhaustion; fix the backoff wording.
2. One classifier, `error_scope(&eyre::Report) -> ErrorScope::{Thread, Account}`, reading the structured body, never the variant name or rendered text.
3. Catch once per unit of work at the per-thread loop boundary, never inside an action, so a failed write cannot fall through to a later filter.
4. A skipped-thread set feeds the pure planners, so a skipped thread gets no further writes.
5. A per-account-run skip ceiling from config, checked at each skip.
6. Tests run the production `GmailClient` against a wiremock server via `Hub::base_url`, so they exercise `google_gmail1`'s own error decoding.

### Architecture

```
per-thread unit (get_thread + evaluate_thread | fetch_thread_labels | get_message | sanitize modify)
   Err ─> error_scope ─Thread──> warn!(id, op, err); skipped.insert(id);
                     │          skipped.len() > max-skipped-threads ? Err(SkipCeiling) : continue
                     └─Account─> propagate -> main.rs:91 -> exit 1 -> ntfy
```

### Classification

- `google_gmail1` maps every non-2xx response with a JSON body to `Error::BadRequest` (`api.rs:27479-27481`), so the variant says nothing about status. Read the body.
- `Thread` when, and only when:
  - body `error.code == 400` and `error.status == "FAILED_PRECONDITION"`, or
  - body `error.code == 404` and some `error.errors[].reason == "notFound"`.
- `Account` for everything else, including any report carrying `RetryExhausted`. Check `report.downcast_ref::<RetryExhausted>()` first: a `report.chain()` walk does not find a `wrap_err` context type.
- `error_scope` is only called at per-thread call sites. `threads.list`, `labels.list`, `messages.list` and `batch_modify` never reach it and keep propagating.

### Per-thread boundary (Phase 2 state filters)

- Wrap `get_thread(thread_id)` + `evaluate_thread(...)` (`engine.rs:844`) in one match. Any error from either, `Thread` scope -> the thread is `Skipped`; no later filter in `evaluate_thread` runs for it.
- `apply_state_action` (`engine.rs:1038-1091`) keeps `?` on `modify_thread` / `trash_thread`. Turning a failed Move into `Ok(false)` would make `evaluate_thread` (`engine.rs:863-935`) try later filters, and a later Delete could trash the thread.
- `sanitize_stages` (`engine.rs:241-243`): the per-tid `modify_thread` gets the same match; a `Thread`-scope failure skips that tid.

### Drop-writes seam (Phase 1 message filters)

- Today `plan_pin_ids` treats a thread missing from `thread_labels` as unpinned (`engine.rs:579-594`), so it stars it; Tag, Move and marker writes use the full `matched_ids` (`engine.rs:512-569`). A skipped label fetch would give a duplicate star plus a marker stamp that freezes the message unhandled forever.
- Fix: `fetch_thread_labels` (`engine.rs:454`) returns `(labels, skipped_thread_ids)`. Before `plan_filter_writes`, `matched_ids` drops every message whose `thread_id` is in the skipped set. The planner stays pure; the skipped set is an input.
- Scope of the guarantee: no further writes to a skipped thread after discovery. Writes from earlier filters in the same run (`engine.rs:338-391` interleaves fetches and writes per pinning filter) are not undone, and correctness does not need them to be.
- `get_message` (`engine.rs:320`) failure, `Thread` scope: skip that one message. It never enters `messages`, so it never matches. Star suppression reads the full thread via `threads.get`, so this does not affect star correctness for siblings.

### Data Model

- `enum ErrorScope { Thread, Account }` and `struct RetryExhausted { op: String, attempts: u32 }` in `src/gmail/rate.rs`.
- Config key `max-skipped-threads` (per account, `src/cfg/config.rs`, kebab-case via serde), shipped `eratosthenes.example.yml` entry: 10, with a comment.
  - Rationale: 35 Class A events in 6,821 runs, about 1 per 195 runs. Today a run can never observe a second (the first aborts it), so per-run max is censored; but 11 independent events in one run is not plausible. More than 10 is systemic.
- Skip accounting: one `HashSet<String>` of skipped ids per account run (thread ids, plus message ids for `get_message` skips), shared across Phase 0/1/2. The ceiling counts distinct ids, not failed calls.
- Ceiling checked at each insert, not at end of run: an account-wide `FAILED_PRECONDITION` fails after 11 skips instead of 2,689 calls, and the `--mark-only` early return (`engine.rs:31-56`) cannot bypass it.
- Ceiling error text: `skipped {n} distinct threads/messages, over max-skipped-threads {m}`. It does not embed the per-thread causes (those are WARNs), so a ceiling failure never matches `FAILED: .*FAILED_PRECONDITION`.

### API Design

```rust
pub enum ErrorScope { Thread, Account }
pub fn error_scope(report: &eyre::Report) -> ErrorScope;                 // rate.rs
pub struct RetryExhausted { pub op: String, pub attempts: u32 }          // wrap_err context on the last error
```

- `with_retry` on exhaustion: `Err(last_err.wrap_err(RetryExhausted { .. }))`.
- Skip WARN: `skipping thread {id}: {op} failed: {err:#}` (message skips: `skipping message {id}: ...`).
- `Done:` lines (both the normal and `--mark-only` forms) append `, {n} skipped`.

### Implementation Plan

#### Phase 0: Prove the wiremock seam
**Model:** sonnet
- Zero production code. A throwaway test: build a `Hub` with `base_url` pointed at a `wiremock` server and a no-op token, call `GmailClient::get_thread`, return a canned `400 FAILED_PRECONDITION` body.
- `cargo add --dev wiremock` (in-house precedent: `qai`, `mermaid-rs`).
- **Success criteria:**
  - the test sees an error whose body has `error.status == "FAILED_PRECONDITION"`, after exactly 1 HTTP request (not retried)
  - a canned 429 is retried (wiremock records > 1 request) under the paused tokio clock

#### Phase 1: Retry hygiene
**Model:** sonnet
- `rate.rs:238`: `Err(last_err.wrap_err(RetryExhausted { .. }))` instead of `eyre::bail!`.
- `rate.rs:57`: `[retry] backing off {n}s before attempt {k}`, no "Rate limited".
- Ladder and `MAX_RETRIES` untouched, so `worst_case_call_duration`, `TimeoutStartSec` and `service.rs` stay put.
- **Success criteria:**
  - test: every attempt returns a DNS error -> final `{:#}` contains `dns error` and `downcast_ref::<RetryExhausted>()` is `Some`
  - `rg -n 'failed after \{\} retries' src/gmail/rate.rs` returns no match

#### Phase 2: Classifier, state-filter boundary, ceiling
**Model:** opus
- `ErrorScope` + `error_scope` per Classification above; unit tests on wiremock-produced errors: 400 FAILED_PRECONDITION, 400 other, 404 notFound, 429, 503, transport, `RetryExhausted`.
- Per-thread boundary at `engine.rs:844` and `sanitize_stages` per Per-thread boundary above.
- Skipped-id set, ceiling check at insert, `max-skipped-threads` config + example entry, `Done:` suffix on both forms.
- Correct the comment at `engine.rs:979-983`.
- **Success criteria:**
  - test: wiremock fails `threads.get` for 1 of N threads with FAILED_PRECONDITION -> `Ok`, N-1 evaluated, 1 skipped; reverting the match arm makes it fail (bite)
  - test: a thread whose first matching Move fails with FAILED_PRECONDITION and whose later filter is a Delete -> no `threads.trash` request reaches wiremock
  - test: 10 distinct skips with `max-skipped-threads: 10` -> `Ok`; 11 -> `Err` before the 12th `threads.get`; a 429 on one thread -> `Err`

#### Phase 3: Message-filter drop-writes
**Model:** opus
- `fetch_thread_labels` returns the skipped set; `matched_ids` filtered by it before `plan_filter_writes` per Drop-writes seam above.
- `get_message` `Thread`-scope failure skips the message.
- **Success criteria:**
  - test: a thread whose `threads.get` fails in `fetch_thread_labels` gets no star, Tag, Move or marker write (no `batchModify` id for its messages reaches wiremock); other threads' writes unchanged
  - test: a failed `get_message` produces no write for that id and 1 skip

#### Phase 4: Triage parity
**Model:** sonnet
- `src/triage/mod.rs:329-332` (`get_thread_full`) and `:376-380` (bucket write) skip `Thread`-scope errors. The ordering at `:362-372` already guarantees no marker without a landed bucket. Provenance: migration doc `:586` is engine-wide.
- **Success criteria:**
  - test: one failing `get_thread_full` -> the rest are classified, 1 skipped

## Acceptance Criteria

- [ ] No per-thread Gmail call in the engine propagates with a bare `?` outside the boundary: `rg -nU '(get_thread|get_message|modify_thread|trash_thread)\([^;]*?\)\s*\.await\?' src/engine.rs` returns only the two `apply_state_action` writes (`modify_thread`, `trash_thread`), which the Phase 2 boundary catches by design.
  - Observed on main: 6 call sites (`:242`, `:320`, `:454`, `:844`, `:1078`, `:1089`); rg prints 8 lines because `-U` emits every line of a multi-line match.
- [ ] `rg -n 'failed after \{\} retries' src/gmail/rate.rs` returns no match.
  - Observed on main: 1 match (`:238`).
- [ ] `otto ci` exits 0 with the Phase 2 and Phase 3 tests present, and fails when the `Thread` match arm at the `engine.rs:844` boundary is reverted.
  - Observed on main: cannot run; the tests are added in Phases 2-3.
- [ ] Over 7 days after install, `journalctl --user -u eratosthenes.service --since "$(date -r ~/.cargo/bin/eratosthenes '+%F %T')" -g 'FAILED: .*FAILED_PRECONDITION'` returns zero lines. (A ceiling failure prints `over max-skipped-threads`, not the per-thread cause, so this stays consistent with the ceiling.)
  - Observed on main: the `--since` expression resolves to `2026-10-05 17:36:40` (v0.7.4 install); `--since '7 days ago'` returns 15 lines.
- [ ] Every Class A event after install is a WARN in the app log: `rg -z 'skipping (thread|message) .*FAILED_PRECONDITION' ~/.local/share/eratosthenes/logs/` prints one line per event, each with id and op.
  - Observed on main: no match (exit 1); the line does not exist yet.

## Resolved Decisions

- 2026-10-06 (author): a run with skips at or under the ceiling exits 0. One transient thread is noise; the ceiling keeps a systemic failure loud.
- 2026-10-06 (author): no in-call retry for `FAILED_PRECONDITION`; next-run recovery is proven, same-call recovery is not.
- 2026-10-06 (author): query narrowing parked (see Non-Goals) as unrequested scope.
- 2026-10-06 (panel r1, both seats, folded): drop-writes seam named (skipped set filters `matched_ids` before the pure planner), tested for star, Tag, Move and marker.
- 2026-10-06 (panel r1, staff, folded): catch at the per-thread boundary, never inside `apply_state_action`; Move-then-Delete fall-through test added.
- 2026-10-06 (panel r1, staff, folded): test seam is wiremock + `Hub::base_url` against the production `GmailClient`, proven in Phase 0. Chosen over a trait over `GmailClient`: the engine also reaches `client.hub()` and `client.resolver` directly, and the HTTP seam exercises the `BadRequest` body decoding the classifier depends on.
- 2026-10-06 (panel r1, staff, folded): classifier reads body `error.code` + `status`/`reason`; `RetryExhausted` via `downcast_ref` first.
- 2026-10-06 (panel r1, staff, folded): ceiling counts distinct ids, checks at insert (covers `--mark-only`), rationale restated as rate-based since per-run max was censored.
- 2026-10-06 (panel r1, both seats, folded): Retry-After `LockedOut` parsing removed from Phase 1 and parked with lockout persistence; no requester.
- 2026-10-06 (panel r1, folded): Class A op split (20 get / 15 modify) stated; `rate.rs:57` reframed as wording; drop-writes guarantee scoped to "after discovery"; failed `get_message` skips only that message; AC commands fixed (`--since` resolvable, `rg -z` over rotations, ceiling-consistent).

## Alternatives Considered

### Alternative 1: Make `FAILED_PRECONDITION` retryable
- **Description:** add it to `RETRYABLE_REASONS`.
- **Pros:** one-line change.
- **Cons:** unproven to clear within 38s; a persistent precondition failure would burn 5 calls and still kill the run.
- **Why not chosen:** the run still dies on any thread error the ladder does not outlast.

### Alternative 2: Never fail the unit, log everything
- **Description:** `main.rs` exits 0 always.
- **Pros:** no alerts.
- **Cons:** a lockout or auth expiry goes silent.
- **Why not chosen:** turns off the feature's value. Fail loudly on account-scoped errors.

### Alternative 3: Narrow the active query instead
- **Description:** Parked Non-Goal above.
- **Why not chosen:** shrinks exposure about 92% but leaves the structural flaw; one 400 on the remaining threads still kills the run.

### Alternative 4: Trait seam over `GmailClient` for tests
- **Description:** a `GmailOps` trait like `SlackPoster` (`src/slack/mod.rs:38`); engine functions generic over it.
- **Pros:** in-house pattern; no HTTP in tests.
- **Cons:** engine also uses `client.hub()` (`engine.rs:136`) and `client.resolver`; the trait would have to abstract those too. Fakes would hand-build errors and skip `google_gmail1`'s body decoding, the exact part the classifier must get right.
- **Why not chosen:** larger refactor, weaker test.

## Technical Considerations

### Dependencies
- New dev-dependency: `wiremock` via `cargo add --dev`. No new runtime crates.

### Performance
- No added calls. The ceiling at insert bounds a systemic failure at 11 skipped calls.

### Security
- Drop-writes seam (Phase 3) prevents a skipped thread from being re-starred, re-tagged, moved or marker-stamped.

### Testing Strategy
- Production `GmailClient` against wiremock (Phase 0 proves the seam). Fixtures return Gmail-shaped JSON bodies per thread id.
- Every skip test has a bite: revert the match arm, test fails.

### Rollout Plan
- Ship via `bump release`, install, then watch the journal for the 7-day criterion.
- No `eratosthenes service reinstall`: `TimeoutStartSec` is unchanged.
- Add `max-skipped-threads` to the dotfiles-managed config.
- Blast radius: this repo plus one config line in dotfiles. The OnFailure drop-in is unchanged.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| A thread-scoped error that is persistent gets skipped every run | Low | Med | WARN every run with the id; ceiling fails the run if it spreads |
| Classifier miscalls an account error as thread-scoped | Low | High | Allowlist on body code + status/reason; default `Account`; wiremock fixtures per class |
| Skipped thread leaks a duplicate star or marker | Low | High | Skipped set filters `matched_ids` before planning; Phase 3 test covers star, Tag, Move, marker |
| Failed Move falls through to a later Delete | Low | High | Catch at the per-thread boundary only; Phase 2 Move-then-Delete test |
| `--dry-run` hides skips | Low | Low | Reads still happen in dry-run; skip path identical, only writes gated |
| A skipped thread was due a Move/Delete this run | Med | Low | Next run (5 min) re-evaluates it; age-off is not time-critical |

## Open Questions

(none)

## References

- `src/gmail/rate.rs`, `src/gmail/client.rs`, `src/engine.rs`, `src/main.rs`, `src/triage/mod.rs`
- `docs/design/2026-03-29-gmail-api-migration.md:586`, `docs/design/2026-06-06-slack-digest.md:330`
- `docs/shakedown-v0.7.4.md:72-73`
- Review panel round 1: `/tmp/review-panel/5loRYOSZ/synthesis.md`
- dotfiles d947a42 (OnFailure drop-in)
