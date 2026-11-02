## Phase 1: Client plumbing
### Design decisions
- `list_thread_refs` returns a `ThreadList { refs, pages, skipped_items }` — src/gmail/client.rs — Phase 3's `pages=P` log line needs the page count; the no-id skip count is logged at DEBUG and also exposed. Named fields over a tuple so each number says what it is
- Literal `.max_results(500)` at the call site — src/gmail/client.rs — keeps AC2's `rg 'max_results\(500\)'` probe true
- `GmailThread` gains `history_id: Option<u64>` and `skipped_messages: usize`; `get_thread` counts each malformed message it drops — Phase 2's `from_thread` must refuse to cache a partial view
- `xdg_cache_dir()` mirrors `xdg_data_dir()` — src/cfg/config.rs
- Fixture `thread_body_with_labels` carries `"historyId": "1000"`; new `thread_get_only_requests` counts GET-only `threads/{id}` — tests/common/mod.rs
- New tests in tests/thread_history.rs: list returns per-thread historyId and sends maxResults=500; pagination plus no-id skip; mid-pagination failure errors; get_thread historyId; skipped_messages count
- Break-to-prove done: page size 500 -> 100 fails the maxResults test; `get_thread` setting `history_id: None` fails the get_thread test

### Deviations
- `list_thread_refs` returns `Result<ThreadList>`, not `Result<Vec<ThreadRef>>` — same effect, carries the page count Phase 3 logs
- No test for `xdg_cache_dir()` — it reads process-global env, sibling helpers have none, and mutating env in parallel tests is racy

### Tradeoffs
- Struct return vs tuple — named fields
- Literal 500 vs named const — the const made AC2's grep return 0; one use site, doc comment explains it

### Open questions
- None.

## Phase 2: Cache module
### Design decisions
- Missing cache file logs at DEBUG, not WARN - src/cache.rs:ThreadCache::load - it is the normal first run (and every run after a wipe); unreadable, unparseable, and wrong-version files are WARN
- `CachedThread` serializes with kebab-case keys (`history-id`, `last-activity`, `is-read`) - src/cache.rs - repo convention for serialized structs (cfg/triage.rs); pinned by a test
- `from_thread` sorts `label_ids` - src/cache.rs:CachedThread::from_thread - `label_ids()` returns a HashSet; sorting keeps the file byte-stable across runs. Set contents are exactly the existing definition
- `save` returns `eyre::Result<()>` and removes its pid-suffixed tmp file on failure - src/cache.rs:ThreadCache::save - Phase 3 `lib::run` decides how loud a failed save is; no stray tmp files
- Added `empty`, `Default`, `len`, `is_empty` beyond the spec - src/cache.rs - needed for load fallbacks and Phase 3 log counts; all `pub`, so `deny(dead_code)` is satisfied with no `#[allow(dead_code)]`
### Deviations
- pagerduty-cli's fixed `.tmp` name replaced by `<path>.<pid>.tmp` - as instructed, so two concurrent runs never share a temp file
### Tradeoffs
- `ThreadCache` fields private (version settable only in-module) vs pub fields - callers can't forge a version; tests reach it via the in-module `tests`
### Open questions
- None.

## Phase 3: Wire the cache into Phase 2
### Design decisions
- `ThreadState<'a> { id, label_ids, last_activity, is_read }` is the one evaluation input - src/engine.rs:ThreadState - `evaluate_thread` / `apply_state_action` take `&ThreadState`; `ThreadState::labels_resolved` resolves raw ids exactly as `GmailThread::labels_resolved` does (unit test pins the two equal)
- Hit test is a named pure fn - src/engine.rs:fresh_entry - hit only when the list carried a stamp AND it equals the cached one; `fetch_reason` names the miss (`new|changed|no-history-id`) for the per-fetch DEBUG line
- A hit's entry is cloned out of the cache before evaluation - src/engine.rs:evaluate_active_threads - the cache is mutated later in the same iteration (insert/drop); entries are a few short strings
- A freshly fetched thread `from_thread` rejects is evaluated from a `ThreadState` built with the same definitions (sorted `label_ids()`, `last_activity()`, `is_read()`) and not cached; any older entry for it is removed, since its stamp is now stale
- Drop points: `retain_ids` after the whole list succeeded; `Ok(true)` only when `!dry_run`; every per-thread error (fetch or action) BEFORE `skip_thread(...)?`; a thread already in the SkipLedger from Phase 0 when Phase 2 reaches it
- INFO `[state] active=N hits=H fetched=F dropped=D pages=P` is logged on the error path too, so a failed run still shows what the saved cache reflects - src/engine.rs:execute_state_filters
- `fetched` counts `threads.get` attempts, including ones that then failed
- The state loop moved into its own fn so the INFO line runs after it whatever it returns - src/engine.rs:evaluate_active_threads - 10 args, under the repo's clippy threshold of 12
- `thread_cache_path(account)` errors when no cache dir resolves (no `$XDG_CACHE_HOME`, no `$HOME`) - src/lib.rs - fail loudly on a broken environment rather than silently running uncached
- Break-to-prove done, each restored after: (1) `fresh_entry` always hits -> `changed_history_id_fetches_exactly_that_thread` fails (`left: 0, right: 1` GETs) and the error-path test fails; (2) no drop on a real write -> `written_thread_is_refetched_and_unlisted_thread_is_dropped` fails; (3) drop on dry-run would-write -> `dry_run_keeps_the_would_move_thread_cached` and the parity test fail; (4) no drop before `skip_thread` -> `drops_before_an_account_scoped_error_reach_the_saved_file` fails; (5) save only on `Ok` -> the same error-path test fails

### Deviations
- Load/execute/save extracted from `lib::run` into `pub async fn execute_with_cache(client, config, prefix, dry_run, mark_only, cache_path)` plus `pub fn thread_cache_path(account)` - src/lib.rs - `lib::run` builds an OAuth client and cannot be driven by wiremock; the error-path, save-failure, and mark-only tests call `execute_with_cache`, which `run` calls unchanged. Same effect, correct seam
- INFO line carries `pages=P` - follows the API Design section; the Phase 3 bullet omitted it
- The clock is not injectable through `engine::execute` (`RealClock` inside `execute_state_filters`, as before). The parity test uses fixtures far from every TTL boundary (2023-11-14 and now minus 1h), the same approach tests/state_isolation.rs takes, so the real clock cannot flip a decision between the cached and fetched runs. Label order: the wire order in the parity fixture is deliberately unsorted; evaluation is order-insensitive (`matches_labels` / `plan_state_move` use `contains`, and write lists are built from stage order)
- Empty fetched thread: the `Thread {} has no messages, skipping` WARN now fires for any empty fetched thread, before filter matching. Before, it fired only when some filter matched its (empty) label set, i.e. a label-less catch-all. Decision unchanged: no write either way
- `evaluate_thread`'s TRACE line lost its `msgs=` count; `ThreadState` carries no messages
- `dropped` also counts the stale-entry removal for a refetched uncacheable thread and the Phase 0 SkipLedger drop; the doc lists absent + written + skipped

### Tradeoffs
- Clone the hit entry vs borrow it across the evaluation - the borrow conflicts with the later insert/drop in the same iteration; cloning is cheap and keeps the loop straight-line
- One `MockServer` per run in tests/state_cache.rs vs one server reset between runs - per-run request counts with no remounting, and the shared `ThreadCache` carries across exactly as the file would
- Parity cache warmed by a dry run vs hand-built entries - the dry run proves reads populate and would-writes keep, and builds entries through the production `from_thread`

### Open questions
- `thread_cache_path` fails the account run when neither `$XDG_CACHE_HOME` nor `$HOME` resolves. The doc treats the cache as an optimization (save failure is a WARN); confirm a missing cache dir should be loud rather than a WARN and an uncached run
