# Design Document: Per-Thread History Cache for State Filters

**Author:** Scott Idler
**Date:** 2026-11-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Phase 2 (state filters, thread age-off) calls `threads.get` on every active thread on every run: ~3775 threads x 10 units = ~37.7k quota units, ~5 min, back to back, all day. Cache each thread's aging inputs on disk keyed by thread id, stamped with the thread's `historyId`. Each run lists the active set (which already returns `historyId` per thread) and fetches only threads whose stamp changed. Everything else evaluates from the cache.

## Problem Statement

### Background

- Eratosthenes runs on ripr as a systemd `--user` oneshot, `eratosthenes.timer`, `OnUnitActiveSec=5min`
- A run is Phase 0 (stage sanitize) -> Phase 1 (message filters) -> Phase 2 (state filters)
- Phase 2: `execute_state_filters` (`src/engine.rs`) runs `list_threads(active_query)`, then `get_thread(id)` (format=metadata) for EVERY id, then `evaluate_thread`
- `build_active_threads_query` ORs `in:inbox`, every state filter label, and every `Move` destination. The `llm/*` bucket labels stay on threads forever by design, so Purgatory and Oblivion threads stay in the active set for good
- The 2026-03-29 migration doc budgeted Phase 2 at 500 threads ~= 5k units. The 2026-09-03 act-once doc parked this exact problem: "revisit if run duration exceeds the timer interval"

### Problem

- 2026-11-01: Gmail returned `429 User-rate limit exceeded. Retry after <now+15m>` at 14:12 and 17:28 PST. First time this week; a week of logs had zero of these before today
- Measured on 2026-11-01 (`~/.local/share/eratosthenes/logs/tatari.log`, journal):
  - `[state] 3775 active threads to evaluate`
  - one `threads.get` per thread = ~37.7k units per run
  - run length ~5 min, timer 5 min -> runs chain back to back (`Finished` and `Starting` in the same second), ~125 units/sec sustained, half of Gmail's 250/sec per-user ceiling
  - 1439 of those threads in one run were Oblivion no-ops (`refusing backward move ... from 'Oblivion'`)
- The active set grew 3539 (Oct 26) -> 3775 (Nov 1). Nothing shrinks it
- Almost every fetch is waste: aging reads only labels, newest message date, read state, and the clock. For an untouched thread, only the clock moved

### Goals

- A run fetches a thread with `threads.get` only when it is new to the cache or its `historyId` changed since the last run
- Phase 2 decisions are identical to today's for the same mailbox state
- Decisions are correct as of the moment the active list was read. A lost, corrupt, or stale cache costs fetches, never a decision made on facts older than that list
- Hit/fetch/drop counts visible in the log on every run

### Freshness window (accepted)

- Today a thread is fetched right before it is judged. With the cache, a hit's facts are proven current when the list is read, then judged later in the same run
- A star, reply, or relabel that lands between the list read and the judgment can be acted on from the pre-change facts
- Today already has the same window between `threads.get` and the write. The cache stretches it from one call to the rest of the run, and a cached run is short
- Accepted. Not mitigated further: the next run sees the new `historyId` and corrects course the same way a manual relabel is corrected today

### Non-Goals

- **Honoring `Retry-After` on a 15-minute 429.** Parked. Revisit: if 429s recur after this ships. `with_retry` tops out at 60s backoff (`src/gmail/rate.rs`)
- **Dropping `label:oblivion` from the active query.** Parked. Revisit: if cache hits on Oblivion threads still show up as a cost. With the cache, an Oblivion thread costs nothing after its first fetch
- **Timer change to `OnUnitInactiveSec`.** Parked. Revisit: if a cached run still takes longer than the 5 min interval
- **`users.history.list` mailbox cursor.** Excluded: see Alternatives
- **Caching Phase 1's pin-suppression `get_thread`** (`fetch_thread_labels`). Excluded: it fetches only the threads Phase 1 matched, a handful per run
- **Caching message bodies / .eml.** Excluded: aging reads none of it (discussed in session)

## Proposed Solution

### Overview

Per run, inside Phase 2:

1. `lib::run` loads `<cache-dir>/eratosthenes/<account>.json` into a `ThreadCache` (missing/corrupt/wrong version -> empty + WARN)
2. `list_thread_refs(active_query)` returns `ThreadRef { id, history_id }` for every active thread (500 per page)
3. Diff against the cache:
   - same `history_id` -> **hit**, evaluate from the cache entry
   - different `history_id`, or not cached, or list returned no `history_id` -> **fetch** with `threads.get`, store the entry, evaluate
   - cached id absent from the list -> **drop** the entry
   - the drop runs only after the WHOLE list succeeded. A list that fails mid-pagination errors out (`?` on every page, account-scoped, as today) and leaves the cache untouched
   - limit: a list that silently comes back short can't be detected (`resultSizeEstimate` is an estimate). Cost: extra drops, then re-fetches next run. Never a wrong move
   - list items with no `id` are skipped as today, but counted and logged at DEBUG
4. Evaluate every thread through one path that takes a `ThreadState` (cached or freshly fetched, same struct)
5. A thread that gets written (`threads.modify` / `threads.trash`) or skipped -> drop its entry. Next run sees it changed or missing and fetches it
   - drop only on a REAL write: `apply_state_action` returns `Ok(true)` in dry run too, so the drop checks `!dry_run`
   - on skip, drop BEFORE `skipped.skip_thread(...)?`, since `SkipLedger` can return early
   - with Phase 0 proven, our own write moves the thread's `historyId` anyway. The drop is a backstop, not the correctness mechanism
6. `lib::run` saves the cache atomically after `engine::execute` returns, success OR error

### Architecture

- `src/cache.rs` (new): `ThreadCache` + `CachedThread`, load/save, nothing else. No Gmail, no engine knowledge
- `src/gmail/client.rs`: new `list_thread_refs`, mirroring the existing `search_message_refs` / `MessageRef` pattern. `get_thread` keeps the `history_id` the API already returns
- `src/cfg/config.rs`: `xdg_cache_dir()` next to `xdg_config_dir()` / `xdg_data_dir()`, same reasoning (honors `XDG_CACHE_HOME`, falls back to `~/.cache`, avoids the `dirs` macOS paths)
- `src/lib.rs` `run`: owns the cache file lifecycle, since it knows the account name. Passes `&mut ThreadCache` into `engine::execute`. The engine does no cache I/O, so tests hand it an in-memory cache
- `src/engine.rs`: `execute_state_filters` does list -> diff -> fetch-misses -> evaluate. `evaluate_thread` / `apply_state_action` take `&ThreadState` instead of `&GmailThread`
- `GmailClient` holds no cache state

### Data Model

```rust
// src/cache.rs
#[derive(Serialize, Deserialize)]
pub struct ThreadCache {
    version: u32,                              // CACHE_VERSION = 1; mismatch -> empty + WARN
    threads: HashMap<String, CachedThread>,    // key: thread id
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CachedThread {
    pub history_id: u64,
    pub label_ids: Vec<String>,           // RAW Gmail ids, resolved at evaluation time
    pub last_activity: DateTime<Utc>,     // last message internal_date
    pub is_read: bool,                    // last message lacks UNREAD
}
```

```rust
// src/engine.rs: the one evaluation input
pub struct ThreadState<'a> {
    pub id: &'a str,
    pub label_ids: &'a [String],
    pub last_activity: DateTime<Utc>,
    pub is_read: bool,
}
```

- The cache holds mailbox FACTS, never decisions. Editing `state-filters` in config changes what gets decided from those facts, so a config edit never invalidates the cache
- Raw label ids, not names: a label rename resolves fresh every run, never stale in the file
- File name is the `account` string `lib::run` already receives (`tatari`, the same name as `tatari.yml` and `tatari.log`), one file per account. Accounts run as separate tokio tasks and never share a file
- `CachedThread` is built from a fetched `GmailThread` with exactly today's definitions (`label_ids()`, `last_activity()`, `is_read()` in `src/gmail/message.rs`), so a hit and a fetch can't disagree
- A fetched thread with no messages (today's `warn!` + skip path) is not cached
- A fetched thread where `get_thread` skipped a malformed message is evaluated as today but NOT cached. `GmailThread` gains `skipped_messages: usize` so `from_thread` can tell
- `CACHE_VERSION` bumps whenever the definition of a cached field changes (label union, `last_activity`, `is_read`), not only when a field is added. Old files then load empty
- Size: ~3.8k entries of ids, a few label ids, a timestamp and a bool. Small JSON

### API Design

```rust
// src/gmail/client.rs
pub struct ThreadRef { pub id: String, pub history_id: Option<u64> }
pub async fn list_thread_refs(&self, query: &str) -> Result<Vec<ThreadRef>>;   // max_results(500)

// GmailThread gains: pub history_id: Option<u64>   (populated by get_thread)

// src/cache.rs
impl ThreadCache {
    pub fn load(path: &Path) -> ThreadCache;            // never errors: WARN + empty on any failure
    pub fn save(&self, path: &Path) -> Result<()>;      // <path>.<pid>.tmp, sync_all, rename
    pub fn get(&self, id: &str) -> Option<&CachedThread>;
    pub fn insert(&mut self, id: String, entry: CachedThread);
    pub fn remove(&mut self, id: &str);
    pub fn retain_ids(&mut self, live: &HashSet<&str>) -> usize;   // returns dropped count
}

// src/engine.rs
pub async fn execute(client, config, prefix, dry_run, mark_only, cache: &mut ThreadCache) -> Result<..>;
```

- Log line per run, INFO: `[state] active=N hits=H fetched=F dropped=D pages=P`
  - a run whose active list fails has no counters yet; it logs WARN `[state] active list failed, cache untouched: <err>` instead (implementation audit S1)
  - `active`: thread refs the list returned
  - `hits`: refs evaluated from the cache with no `threads.get`
  - `fetched`: `threads.get` calls made (new, changed, or `history_id` missing). `hits + fetched = active` minus threads already in the SkipLedger
  - `dropped`: cache entries removed this run (absent from the list + written + skipped)
  - `pages`: list pages, so the wire `maxResults` cap is visible
- One DEBUG line per fetch: `[state] fetch id=<id> reason=new|changed|no-history-id`. Bounded by changed threads, so it stays small
  - repeated save failure shows up as `hits=0` run after run; no extra alerting
- Save failure: WARN, run result unchanged. Next run loads stale-or-empty and self-heals through the `historyId` diff
- `dry_run`: reads populate the cache (they are real reads), "would write" does NOT drop the entry (nothing changed in Gmail)
- `mark_only`: returns before Phase 2 today; the cache is loaded and saved untouched

### Implementation Plan

#### Phase 0: Prove `historyId` moves on label-only changes
**Model:** sonnet (operator-driven, zero code)
- Wait until the account is out of the 429 window; timer stays stopped
- With `gws` on the work account, pick one throwaway thread:
  - `threads.list` with a `q` that matches it, record `historyId`
  - add a user label -> list again
  - remove it -> list again
  - mark read (removes `UNREAD`) -> list again; mark unread -> list again
  - `threads.get` format=metadata, compare its `historyId` to the list's
- **Success criteria:**
  - every label-only step strictly increases the thread's `historyId` in `threads.list`
  - `threads.list` and `threads.get` report the same `historyId` for the same state
  - if either fails: STOP, the design pivots (Alternatives: history cursor) and the doc is revised before Phase 1
- **Result (2026-11-01, PASSED):** run with `gws` on `scott.idler@tatari.tv`, thread `1a19a44593937c4b` (a single-message Google Cloud receipt), listed via `q=rfc822msgid:<its Message-ID>`, temp label `erato-probe` (`Label_70`, deleted after). Every step restored the thread to its baseline labels
  ```
  0 baseline              list=43850065   get=43850065   labels=["CATEGORY_FORUMS","INBOX","UNREAD"]
  1 add user label        list=43880866   get=43880866   labels=["CATEGORY_FORUMS","INBOX","Label_70","UNREAD"]
  2 remove user label     list=43880873   get=43880873   labels=["CATEGORY_FORUMS","INBOX","UNREAD"]
  3 mark read             list=43880882   get=43880882   labels=["CATEGORY_FORUMS","INBOX"]
  4 mark unread (restore) list=43880903   get=43880903   labels=["CATEGORY_FORUMS","INBOX","UNREAD"]
  5 star                  list=43880924   get=43880924   labels=["CATEGORY_FORUMS","INBOX","STARRED","UNREAD"]
  6 unstar (restore)      list=43880936   get=43880936   labels=["CATEGORY_FORUMS","INBOX","UNREAD"]
  ```
  - every label-only change (user label, `UNREAD`, `STARRED`) strictly increased `historyId`
  - `threads.list` and `threads.get` agreed at every step
  - not exercised: a new message arriving in the thread (needs mail sent into it). Phase 4's live check covers it: a reply on any cached thread must show as `fetched`, not a hit

#### Phase 1: Client plumbing
**Model:** sonnet
- `ThreadRef` + `list_thread_refs` (with `max_results(500)`), on the `MessageRef` / `search_message_refs` pattern
- `GmailThread.history_id`, populated in `get_thread`
- `xdg_cache_dir()` in `src/cfg/config.rs`
- `tests/common/mod.rs`: `thread_body_with_labels` gains a `historyId`; add a GET-only `threads.get` request counter (today's `thread_get_requests` also counts `POST .../modify`)
- **Success criteria:**
  - wiremock test: `list_thread_refs` returns the mocked `historyId` per thread and the request carries `maxResults=500`
  - wiremock test: `get_thread` returns the mocked `historyId`
  - `otto ci` green. Existing tests change only mechanically: fixture helpers, and `GmailThread` struct literals gaining `history_id` / `skipped_messages` (`src/engine.rs` tests, `src/gmail/message.rs` tests, `src/digest/tests.rs`)

#### Phase 2: Cache module
**Model:** sonnet
- `src/cache.rs`: `ThreadCache`, `CachedThread`, `CACHE_VERSION`, load/save/get/insert/remove/retain_ids
- Atomic save: `<path>.<pid>.tmp`, `sync_all`, `rename`; create the parent dir. Copy the shape of `tatari-tv/pagerduty-cli/src/cache.rs`, except its fixed `.tmp` name: two concurrent saves would truncate the same temp file
- `CachedThread::from_thread(&GmailThread) -> Option<CachedThread>` (None for an empty thread, missing `history_id`, or `skipped_messages > 0`)
- **Success criteria:**
  - unit tests (tempfile): round-trip equal; missing file -> empty; garbage bytes -> empty; `version: 0` -> empty; no `.tmp` left after save
  - each negative test fails when its guard is deleted (break-to-prove)

#### Phase 3: Wire the cache into Phase 2
**Model:** opus
- `lib::run`: resolve `<xdg_cache_dir>/eratosthenes/<account>.json`, `ThreadCache::load`, pass `&mut` into `engine::execute`, `save` after it returns regardless of result
- `engine::execute` signature gains `cache`; update callers (`src/lib.rs`, `tests/message_isolation.rs`, `tests/state_isolation.rs`)
- `execute_state_filters`: `list_thread_refs` -> `retain_ids` -> per thread hit-or-fetch -> `ThreadState` -> `evaluate_thread`
- `evaluate_thread` / `apply_state_action` take `&ThreadState`. Labels resolved via `client.resolver` at evaluation, as today
- Drop the entry on: successful write, skip (SkipLedger), fetch error. Keep it on: no-op, Keep, dry-run "would write"
- INFO line `[state] active=N hits=H fetched=F dropped=D`
- **Success criteria** (new `tests/state_cache.rs`, wiremock):
  - two identical runs sharing one cache: run 2 makes 0 GET `threads/{id}` requests
  - change one thread's `historyId` in the list mock: run 2 makes exactly 1 GET
  - a thread Moved in run 1 is fetched again in run 2; a thread absent from run 2's list is gone from the cache
  - `dry_run`: 0 writes, and the would-move thread is still cached after the run
  - parity: the same facts, config and fixed clock, once cached and once fetched, produce identical modify/trash requests
  - error path: a drop made before an account-scoped error (mocked 429 exhaustion) is absent from the file `lib::run` saved
  - break the diff (always treat as hit) and the "changed historyId" test fails

#### Phase 4: Ship and prove it live
**Model:** sonnet (operator)
- Release, install on ripr, start `eratosthenes.timer`
- Run 1 after install rebuilds the cache (full fetch, same cost as today, one time)
- Reply to (or receive mail on) one thread that is already cached, and confirm the next run fetches it rather than counting it as a hit (closes the one Phase 0 gap)
- Read the INFO line and journal for the next hour (AC3, AC4), then the next full calendar day's rotated log (AC5)
- **Success criteria:** see Acceptance Criteria 3-5

## Acceptance Criteria

- [ ] **1.** `cargo test --test state_cache` passes, including "run 2 makes 0 GET threads/{id} requests"
  - Observed on main: not runnable, `tests/state_cache.rs` does not exist (Phase 3 creates it)
- [ ] **2.** `rg -c 'max_results\(500\)' src/gmail/client.rs` returns >= 1
  - Observed on main: no output (0 matching lines)
- [ ] **3.** After install on ripr, `ls ~/.cache/eratosthenes/tatari.json` exists, and the second run's `[state] ... hits=H fetched=F` line in `tatari.log` has `F < H`
  - Observed on main: `ls: cannot access '/home/saidler/.cache/eratosthenes': No such file or directory`; no `hits=` line exists
- [ ] **4.** Over one hour after install, `journalctl --user -u eratosthenes.service` shows >= 10 runs ending in `Finished` (none `Failed`), and no run whose `Starting` lands in the same second as the previous `Finished` (runs no longer chain). The run count keeps a stopped service from passing trivially
  - Observed on main (2026-11-01 15:00-17:25, awk over `-o short-iso`): `21 of 28 starts landed in the same second as the previous Finished`, e.g. `17:13:54 Finished` -> `17:13:54 Starting`
- [ ] **5.** For the first full calendar day after install, that day's rotated log (`zgrep -ac '"code":429' ~/.local/share/eratosthenes/logs/tatari.log.1.gz`, run the following day) returns 0
  - Observed on main: `grep -ac '"code":429' tatari.log` for 2026-11-01 (not yet rotated) returns `60`. The prior seven rotated days returned 0,0,0,0,1,0,0: this criterion guards against regression, the headroom claim is criterion 3

## Resolved Decisions

- **2026-11-01, Scott:** per-thread `historyId` stamps, not a mailbox cursor, not a last-run timestamp (session)
- **2026-11-01, Scott:** cache Gmail metadata, not .eml bodies (session)
- **2026-11-01, Scott:** trashed all Oblivion threads in the Gmail UI. Not part of this doc's work
- **2026-11-01, author:** on-disk JSON, not in-memory only: each run is a new process
- **2026-11-01, author:** engine never touches the cache file; `lib::run` loads/saves. Testability, and the engine stays free of XDG
- **2026-11-01, author:** save failure is a WARN, not a run failure. The cache is an optimization; a failed save costs the next run fetches, nothing else
- **2026-11-01, author:** no `--no-cache` flag. Forced rebuild = remove the cache file. A wrong cache can't cause a wrong move, so a kill switch guards nothing
- **2026-11-01, author:** new code follows the house rule, single-word `src/cache.rs`. The repo's existing `mod.rs` files stay as they are
- **2026-11-01, panel round 1 (Architect + Staff Engineer):** all 11 findings folded in: freshness window stated and accepted (M1); no caching of partly parsed threads (C1); per-pid temp file (C2); cached-vs-fetched parity test (C3); Phase 1 criterion allows mechanical struct-literal edits (C4); `CACHE_VERSION` bumps on definition changes (C5); error-path save test (C6); rollback line (D1); `hits=0` as the save-failure signal (D2); AC4 run count (D3); log counters defined (D4)
- **2026-11-01, panel round 1 pushback, closed:** Architect claimed `threads.list` does not return `historyId`. Evidence against: a live `gws` `threads.list` response in this repo (`gws-isimportant.ndjson`) returns `{"historyId":"9467038","id":"1a06257e2b6384f1",...}` per thread, and the `google-gmail1` crate's `Thread` deserializes `historyId`. Phase 0 still proves it on the work account before code
- **2026-11-01, panel round 1 pushback, closed:** Architect's `maxResults=500` cap concern affects list cost only; `pages=P` added to the INFO line to make it visible

## Alternatives Considered

### Alternative 1: Mailbox cursor (`users.history.list`)
- **Description:** store one mailbox `historyId`; each run asks Gmail for changes since it
- **Pros:** cheaper than listing the active set
- **Cons:** history expires; an old cursor 404s and needs a full-resync path. Change records are per message, so they must be mapped back to threads and to "still in the active set"
- **Why not chosen:** per-thread stamps are simpler and self-healing; the list is a few pages. Layer on later if list pages ever matter

### Alternative 2: Last-run timestamp
- **Description:** fetch only threads with activity after the last run
- **Why not chosen:** Gmail's `after:` filters by message date, not label-change time. Starred, read, or relabeled threads with no new mail would be missed

### Alternative 3: Long-running daemon holding threads in memory
- **Why not chosen:** a restart empties memory, so it needs the disk cache anyway, plus self-scheduling and crash handling

### Alternative 4: Store .eml files
- **Why not chosen:** labels, read state and `historyId` are Gmail metadata, not in the message. Raw bodies are more bandwidth and quota, and put work mail unencrypted on ripr

## Technical Considerations

### Dependencies
- None new. `serde`, `serde_json`, `chrono` direct; `tempfile` dev
- `google-gmail1` already deserializes `Thread.history_id: Option<u64>` from both `threads.list` and `threads.get`

### Performance
- Today: list (~38 pages at the default 100) + 3775 gets = ~38k units, ~5 min
- Steady state: list (8 pages at 500, ~80 units) + 10 units per changed thread
- First run after install, or after a lost cache: same as today, once

### Security
- The cache holds thread ids, label ids, a date and a bool. No subjects, senders or bodies
- File lives under the user's `~/.cache`, created with default user perms, same as the logs

### Testing Strategy
- Unit tests in `src/cache.rs` (tempfile)
- wiremock end-to-end in `tests/state_cache.rs`, on the `tests/state_isolation.rs` template, counting GET-only `threads.get` requests
- Break-to-prove for the diff and each load guard

### Rollout Plan
- Phase 0 by hand before any code
- Phases 1-3 on one branch, one commit each, `otto ci` green
- Ship via `bump`, install on ripr, re-enable the timer, run the Acceptance Criteria 3-5 probes
- Rollback: reinstall the previous binary. It never reads the cache file, so the file can stay

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `historyId` doesn't move on label-only changes | Low | High: missed transitions | Phase 0 proves it before any code; pivot if not |
| Manual run overlaps the timer and both save | Low | Low | The unit is `Type=oneshot`, so systemd never overlaps itself; only a hand run can. Per-pid temp file + rename: last writer wins, neither corrupts the other. A stale stamp mismatches the live list and forces a fetch |
| `maxResults=500` capped lower on the wire | Low | Low | List cost only, never correctness. `pages=P` in the INFO line shows it |
| Cache file corrupt or from an older version | Low | Low | Load returns empty + WARN; next run rebuilds |
| `list` omits `historyId` for some thread | Low | Low | `None` always fetches, so a list with no stamp is never a hit. The fetched thread is still cached under `threads.get`'s own stamp |
| Engine signature change misses a caller | Low | Low | Compiler; `otto ci` |

## Open Questions

(none)

## References

- `src/engine.rs`: `execute_state_filters`, `evaluate_thread`, `apply_state_action`, `plan_state_move`, `build_active_threads_query`
- `src/gmail/client.rs`: `list_threads`, `search_message_refs`, `get_thread`, `modify_thread`, `trash_thread`
- `src/gmail/message.rs`: `GmailThread`, `label_ids`, `last_activity`, `is_read`
- `src/gmail/rate.rs`: `RateLimiter`, `with_retry`
- `tests/common/mod.rs`, `tests/state_isolation.rs`
- `docs/design/2026-03-29-gmail-api-migration.md` (Phase 2 quota budget)
- `docs/design/2026-09-03-message-filter-act-once.md` (parked run-duration revisit)
- `docs/design/2026-10-06-per-thread-error-isolation.md` (prior 429 storm)
- `tatari-tv/pagerduty-cli/src/cache.rs` (atomic-save precedent)
