## Phase 2: State filter ownership (D5)
### Design decisions
- The filter walk is a pure planner: `plan_thread` returns `ThreadVerdict { Move, Delete, Held, Unowned }` (src/engine.rs:plan_thread). Ownership is testable without a Gmail client; `evaluate_thread` only logs and applies the verdict (modify or trash).
- `apply_state_action` is gone (src/engine.rs). Its planning moved into `plan_thread`, its write into `evaluate_thread`: one place decides, one place writes.
- The first matching filter owns the thread: unexpired TTL -> `Held`, expired -> its action (src/engine.rs:plan_thread). A `Move` whose `plan_state_move` is a no-op (thread at or past the destination, backward-move guard included) is skipped before the TTL is read, so it falls through before and after expiry. That is the panel round 2 exception.
- `Keep` still owns unconditionally, ahead of the no-op check (src/engine.rs:plan_thread). A Keep filter's destination is the empty default, so a planned move for it means nothing. Unchanged behavior.
- An unexpired `Delete` filter owns its thread like a `Move` (src/engine.rs:plan_thread). A delete is never a no-op; same rule for every non-Keep action.
- `eratosthenes.example.yml` bucket comment rewritten: the first matching filter owns the thread while its TTL runs, tighter or looser than `Cull`, and steps aside once the thread is at or past its destination. The keep-needs-reply block is untouched (Phase 5).
- README "State filter ownership" (written in Phase 1) already matches the implementation. Not edited.
- Tests in src/engine.rs (`cargo test engine`): the four named criteria cases, plus `expired_bucket_filter_moves_its_thread_to_purgatory`, `unlabeled_inbox_thread_is_still_culled`, `keep_filter_still_holds_a_starred_thread`, `bucket_filter_does_not_own_an_oblivion_thread`, `unexpired_delete_filter_holds_its_thread`. Fixture ladders: `work_filters` (tatari.yml shape) and `home_filters` (the doc's `home.yml`, same TTLs, `personal` with no action).
- `personal_category_thread_never_gets_a_stage_label` runs the planner at 4d (held by `personal`) and 8d (move, add `[]`, remove `[INBOX]`), applies the move to the labels, asserts no Purgatory or Oblivion, then runs a third pass over the archived thread: `Unowned`, since the empty-destination move is now a no-op and nothing else matches.
- Break-to-prove: replaced `(None, _) => ThreadVerdict::Held { .. }` with `(None, _) => continue` (the old fall-through). Result: 46 passed, 3 failed: `unexpired_bucket_filter_owns_thread_ahead_of_cull`, `personal_category_thread_never_gets_a_stage_label`, `unexpired_delete_filter_holds_its_thread`. The first failed with `left: None, right: Some("age-receipts")` and verdict `Move { filter: "Cull", dest: "Purgatory", planned: PlannedMove { add: ["Label_7"], remove: ["INBOX"] } }`, which is D5 itself: `Cull` taking a 4-day read receipts thread. Fix restored, 49 passed.
### Deviations
- Ownership tests drive the pure `plan_thread` seam, not `evaluate_thread` through a mocked Gmail client. Same effect, correct seam: the decision is what changed, and the write path (`modify_thread` | `trash_thread`) is unchanged and covered by tests/state_isolation.rs.
- No existing test encoded the old fall-through. `evaluate_thread` had no unit tests; tests/state_isolation.rs and tests/state_cache.rs use expired TTLs or one filter. Every existing test passes unchanged.
### Tradeoffs
- `plan_state_move` runs for every matching non-Keep filter, expired or not, vs only on expiry. Needed for the no-op exception before expiry; it is pure label arithmetic, no Gmail call.
- The no-op fall-through logs at DEBUG (the old `apply_state_action` level) vs TRACE. Kept the prior level; it fires only for parked or backward-matched threads.
### Open questions
- Phase 1 did not create this notes file, so it starts at Phase 2. Phase 1's notes may want back-filling.

## Phase 1: Docs and config plumbing (back-filled by the orchestrator after Phase 2)

The Phase 1 worker reported this notes file as committed in 6185f5e. It was not: 6185f5e touches README.md, src/cfg/label.rs, src/cfg/triage.rs, src/triage/classify.rs and tests/triage_config.rs only. This section was written from the 6185f5e diff and an independent re-run of the criteria. It sits after Phase 2 because the file is append-only.

### Design decisions
- `archive` and `awaits_reply` are `#[serde(default)] bool` fields on `TriageBucket`, kebab-case in YAML (src/cfg/triage.rs).
- The two config errors live in `TriageConfig::validate`, which config load already calls. Each names the bucket (src/cfg/triage.rs:TriageConfig::validate).
- `Label::new` maps any name that uppercases to a `CATEGORY_` prefix onto `Label::Custom` with the uppercased name, so its Gmail id is the system category id (src/cfg/label.rs:Label::new).
- Criteria re-run by the orchestrator: `cargo test --test triage_config` 7 passed, including `archive_with_draft_is_rejected`, `awaits_reply_with_archive_is_rejected`, `unknown_bucket_key_is_rejected`; `test_label_category_is_uppercased` passed; `rg -c 'Needs Reply' README.md` printed nothing (rg exit 1). `archive_with_draft_is_rejected` runs the binary's `config validate` and asserts a non-zero exit; the worker reported exit 1 with `triage bucket 'gated': archive: true with draft: true`.

### Deviations
None.

### Tradeoffs
None.

### Open questions
None.

## Phase 3: Header facts and the archive decision
### Design decisions
- `List-Id`, `List-Unsubscribe`, `Auto-Submitted`, `Precedence` added to `TRIAGE_HEADERS` (src/triage/thread.rs). `PayloadMessage` is unchanged; `test_build_payload_omits_the_automated_sender_headers` (src/triage/classify.rs) asserts the four values are kept on the message and absent from the payload, whose message keys are exactly `body, date, from, from_account_owner`.
- `normalize_address` (src/triage/thread.rs): trim, lowercase, split on the last `@`, drop everything from the first `+` in the local part, drop dots in the local part for `gmail.com` | `googlemail.com`. A string with no `@` is only lowercased. `is_from_self` and `is_solely_to` both compare through it on both sides.
- Row 3's fail-closed addressing is a separate private method, `TriageMessage::is_solely_to_or_has_no_to` (src/triage/thread.rs): To missing or yielding no address counts as addressed to me, otherwise `is_solely_to`. `is_solely_to` itself keeps its drafting semantics (missing To = not sole); `non_automated_message_without_to_protects_thread` asserts both in one test.
- `is_automated` (src/triage/thread.rs) reads exactly the five header facts. `Auto-Submitted` and `Precedence` compare trimmed and case-insensitively. The From fact reads the first parsed From address's local part with `-` `_` `.` removed; a `noreply` in the domain does not count (`bob@noreply.example.com` is a pinned negative case).
- `inbound` excludes messages labeled `DRAFT` and messages `is_from_self`. A message with no From counts as inbound, which leans toward protection.
- `protection` checks rows 1 to 3 in order over the whole thread: any message `STARRED`/`IMPORTANT` (drafts and my own messages included), then no inbound, then any inbound message that is non-automated and `is_solely_to_or_has_no_to`.
- `plan_archive` (src/triage/mod.rs) = `protection` -> `StayProtected`, else `awaits_reply` and the newest inbound message is automated -> `ArchiveAutomated`, else `!archive` -> `Stay`, else `Archive`. One DEBUG line per call with the decision. Not wired into the write path (Phase 4).
- No dead-code allow was needed: `triage` is `pub mod` in the lib crate, so the new pub items are reachable API.
- Tests: `cargo test triage::thread` -> `test result: ok. 41 passed; 0 failed; 0 ignored; 0 measured; 368 filtered out`. Positive and negative per automated fact (List-Id, List-Unsubscribe, Auto-Submitted incl. `no`, Precedence bulk|list|junk vs first-class, From local part noreply/donotreply/mailerdaemon vs `reply`, `mailer-daemon-ops`, noreply-in-domain) and per `Protection` variant; measured senders `greenhouse_no_reply_is_automated`, `drive_shares_noreply_is_automated`, `colleague_without_list_headers_is_not_automated`; normalize_address plus-tag, gmail dots, googlemail dots, non-gmail dots kept, case, no-`@`.
- `cargo test plan_archive_row` observed: `running 6 tests` / `test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 403 filtered out` (`plan_archive_row_1` .. `plan_archive_row_6`, src/triage/tests.rs). Row 5 uses an `awaits-reply`, `archive: false` bucket with a Cc'd human, so it also proves row 4 does not fire on a human.
- Break-to-prove: (1) row 3 reading only the newest inbound message -> `older_sole_human_message_protects_thread_under_newer_bot_message` FAILED (171 passed, 1 failed). (2) removing the missing-To arm of `is_solely_to_or_has_no_to` -> `non_automated_message_without_to_protects_thread` and `non_automated_message_with_unparseable_to_protects_thread` FAILED (170 passed, 2 failed). Both restored.
- `otto ci` exit 0.
### Deviations
- `inbound` signature is `fn inbound<'a>(&'a self, self_address: &'a str) -> impl Iterator<Item = &'a TriageMessage> + 'a` rather than the doc's elided form. Same effect, correct seam: the iterator captures `self_address` for the `is_from_self` filter, so both borrows must outlive it.
- `Protection` and `ArchiveDecision` derive `Debug, Clone, Copy, PartialEq, Eq` (not stated in the doc), for test assertions and the Phase 4 dry-run column.
### Tradeoffs
- A To header that is present but yields no address (`undisclosed-recipients:;`) is treated like a missing To, i.e. protects a human message, vs only a literally absent header. The doc says "no parseable To"; this reads it literally and fails closed.
- `normalize_address` keeps the domain as written (`googlemail.com` is not rewritten to `gmail.com`), vs canonicalizing both to one domain. The doc lists exactly three steps; a cross-domain match would be a fourth.
### Open questions
- `scott.a.idler@googlemail.com` and `scott.a.idler@gmail.com` are the same mailbox but normalize to different strings. Neither account receives mail at a googlemail.com address as far as this phase can see; say so if a domain fold is wanted.

## Phase 4: Archive write path
### Design decisions
- One header-list builder, `engine::metadata_headers(&[MessageFilter])` (src/engine.rs), lifted verbatim from `lib::run`. `lib::run` calls it; `triage::execute` calls it before the filter pass. Unit tests: `metadata_headers_adds_every_guarded_header_once_after_the_standard_set`, `metadata_headers_without_header_guards_is_the_standard_set`.
- `engine::ensure_labels` and `engine::execute_message_filters` are `pub(crate)`. Live triage runs them through `run_message_filters` (src/triage/mod.rs): `ensure_labels`, record `filter_pass_start` (T0), then the filter pass with `dry_run: false, mark_only: false`. It returns T0. A dry run skips the function and uses `Utc::now()` as its cutoff.
- The skip ledger is created before the filter pass and shared with the classify pass. `SkipLedger::contains_message` is new (src/skip.rs, test `test_contains_message_sees_message_skips_only`). `classify_and_label` drops a thread skipped by the filter pass before fetching it, and drops a fetched thread when any of its messages is in the ledger. Same rule the draft pass follows.
- T0 is applied at two levels. `candidates_before` (discovery) drops candidate messages dated at or after T0, so a thread whose only new mail is that fresh is never fetched. `drop_messages_from` (after the fetch) trims messages dated at or after T0 out of a candidate thread's snapshot. Those messages are never classified, marked seen, or archived, so they remain candidates for the next run.
- `plan_assignments` (src/triage/mod.rs) pairs each classification with its thread and bucket and holds the only `plan_archive` call. The dry-run column and the live write path both read its `Assignment`s, so they cannot disagree. `plan_writes` now takes `&[Assignment]`. `ThreadWrite` carries `archive: ArchiveDecision`, and `plan_write` takes it as a parameter. The thread-level write never removes INBOX (`plan_writes_carries_the_archive_decision_but_never_removes_inbox`).
- `plan_seen_batches` splits the classified message ids of landed writes into `SeenBatches { stay, archive }`. The live pass then sends two `batch_modify` calls: stay adds `llm/seen`, and archive adds `llm/seen` and removes `INBOX` (`INBOX_LABEL`). `batch_modify` returns early on an empty id list, so an empty set sends no request. UNREAD is never in either remove list.
- The dry-run column is `impl Display for ArchiveDecision` via `Formatter::pad`, printed as `{:<22}` between the bucket and the subject.
- Summary line: a private `Tally` struct feeds `summary_line`. Format: `Triage: N threads classified, N labeled, N archived, N protected, N skipped`. `archived` and `protected` count LANDED writes: archive-set threads, and threads with `StayProtected`. A dry run lands nothing, so both read 0 there, the same as `labeled`. The INFO log line now reuses `summary_line`.
- tests/triage_archive.rs (new, 7 tests): `archiving_bucket_removes_inbox_from_the_classified_ids_only`, `sole_to_me_human_thread_classified_noise_is_not_archived`, `no_reply_thread_in_an_awaits_reply_bucket_is_archived`, `list_mail_is_not_starred_by_a_list_id_guarded_filter_in_triage` (M2), `message_dated_after_filter_pass_start_is_not_classified` (M4), `dry_run_issues_zero_non_get_requests`, and `thread_with_a_message_the_filter_pass_skipped_is_not_classified`. In the M2 mock, `messages.get` returns `List-Id` only when the request carries `metadataHeaders=List-Id`, the way Gmail's metadata format behaves. The dry-run test has every label missing and a Star filter that would match, and asserts zero non-GET requests after classification ran.
- `cargo test --test triage_archive` observed: `test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.04s`, exit 0 (overall Acceptance Criterion 1).
- Break-to-prove, each restored afterwards:
  - Deleted the `plan_archive` call in `plan_assignments` and put `ArchiveDecision::Stay` in its place. 2 failed: `archiving_bucket_removes_inbox_from_the_classified_ids_only` (`left: [Batch { ids: ["t1-m1", "t2-m1"], add: ["Label_S"], remove: [] }]`, expected a separate `t1-m1` batch with `remove: ["INBOX"]`) and `no_reply_thread_in_an_awaits_reply_bucket_is_archived` (`left: [Batch { ids: ["t1-m1"], add: ["Label_S"], remove: [] }]`).
  - Removed `set_metadata_headers` from `triage::execute`. `list_mail_is_not_starred_by_a_list_id_guarded_filter_in_triage` failed with `left: ["f1", "f2"]`, `right: ["f2"]`: the list mail was starred.
  - Removed the `candidates_before` cutoff. `message_dated_after_filter_pass_start_is_not_classified` failed: the fresh thread was fetched (`left: 1, right: 0`).
  - Removed the `drop_messages_from` trim. `archiving_bucket_removes_inbox_from_the_classified_ids_only` failed: `t1-late` was marked seen.
  - Replaced `thread.protection(..)` in `plan_archive` with `None`. `sole_to_me_human_thread_classified_noise_is_not_archived` failed.
  - Ran the filter pass in a dry run too. `dry_run_issues_zero_non_get_requests` failed.
  - Neutralized the ledger message check. `thread_with_a_message_the_filter_pass_skipped_is_not_classified` failed.
- `otto ci` exit 0.
### Deviations
- `lib::triage` does not call the header-list function. `triage::execute` does, right before the filter pass that needs it. Same effect, correct seam: `lib::triage` is a thin wrapper around `triage::execute`, and only at this seam does the wiremock M2 test exercise the wiring. A call in `lib::triage` would sit behind OAuth and go untested.
- T0 is also recorded on a dry run, as `Utc::now()` with no filter pass, so discovery has the same cutoff in both modes. The doc says a dry run skips "all three". No Gmail mutation is involved. The only difference is that mail landing mid-dry-run is not listed.
- The T0 rule also trims messages dated at or after T0 out of a fetched candidate thread. The doc states the rule for discovery only. This applies the doc's intent ("classifies only mail that existed when that pass started") to a reply that lands in an already-selected thread during the pass.
- tests/triage_isolation.rs fixtures gained the `Triaged` marker label. Live triage now calls `engine::ensure_labels`, which would otherwise `POST` the default marker into a mock with no labels.create route. No assertion changed.
### Tradeoffs
- `archived` / `protected` count landed writes, so a dry run reports 0, vs counting planned decisions. Chosen so the dry-run line never claims an archive that did not happen. The per-thread dry-run column carries the planned decision.
- Stay batch first, then archive batch, vs one combined call with per-id removes (Gmail has none). If the archive batch fails after the stay batch lands, the run errors. The archive-set messages stay unseen and in INBOX, and the next run reclassifies them. That is the doc's failure mode.
- T0 comes from the local clock, while Gmail's `internalDate` comes from the server. Skew of a few seconds can let a message from that window through on either side of the cutoff. Accepted: the filter-pass race it guards is minutes wide, and the alternative (a Gmail-side timestamp) costs a call per run.
### Open questions
- The filter pass inside triage runs `execute_message_filters` with the full live behavior, including Move filters. The doc's "the same pass `run` does" reads that way. If only Star/Flag filters were meant, say so.

## Phase 5: Answered sweep
### Design decisions
- `sweep_answered` (src/triage/mod.rs) runs in `triage::execute` after `classify_and_label` (whose seen-marker batches end the classify pass) and before `refresh_drafts`, so the `draft_buckets.is_empty()` return can no longer starve it (D1). It runs on every invocation, dry run included.
- Scope is `awaits-reply` buckets only. A config with none logs at DEBUG and returns before any Gmail call, so a config that has not opted in sees no change.
- One answered rule, shared: `TriageThread::is_answered` (src/triage/thread.rs) = the newest non-draft message is from me (`is_from_self`, so the Phase 3 normalization applies). `draft::plan_refresh` now calls it for its `Answered` arm, so the sweep and the draft pass cannot disagree. Lifted, not redefined: same order of checks, `Empty` still wins for a thread of drafts.
- Metadata fetch: new `GmailClient::get_thread_metadata(id, headers)` (src/gmail/client.rs), `threads.get` at `format=metadata` with only `From`, returning the raw API thread so `TriageThread::from_api` parses it. Not `get_thread`, whose header list is the client-wide message-filter set and whose return type is the engine's `GmailThread`.
- `plan_answered_targets` (src/triage/mod.rs) groups `(bucket, label id, thread ids)` per thread in first-seen order. A thread under two `awaits-reply` buckets is fetched once and loses both labels in one `threads.modify`.
- The write removes only the bucket label(s). INBOX stays, per Resolved Decisions: the thread ages under `Cull`.
- Errors follow the classify pass, not the draft pass: a failed fetch or modify goes through `SkipLedger::skip_thread`, so a thread-scoped error skips that thread and an account-scoped one (or the ceiling) fails the run. A thread already in the ledger this run is left alone.
- Dry run: the sweep issues its GETs (`threads.list` by label ids, metadata `threads.get`) and prints `[dry-run] thread <id> is answered; would remove [<buckets>]`. Zero non-GET requests. Same contract as the draft pass's existing answered arm. On a fresh mailbox the bucket labels do not exist and the sweep skips that bucket (`resolve_name` is `None`), which is the path `dry_run_issues_zero_non_get_requests` exercises.
- Summary line, printed and logged: `Triage: N answered threads cleared, N skipped` (+ ` (dry run)`).
- `NEEDS_REPLY_BUCKET` removed (no references).
- `eratosthenes.example.yml`: the `keep-needs-reply` (`ttl: Keep`) block is replaced by `age-needs-reply` (read 3d | unread 7d -> Purgatory); the commented triage buckets carry the doc's Data Model keys (needs-reply `draft: false`, `archive: false`, `awaits-reply: true`; the other four `archive: true`); the bucket comment documents the three keys; the state-filter comment states the answered rule as the sweep now runs it. `refresh_drafts`' doc comment and README's `awaits-reply` bullet updated to say the sweep runs whatever `draft` says.
- Tests:
  - tests/triage_isolation.rs: `answered_awaits_reply_thread_loses_label_with_draft_false` (exactly one `threads.modify`, `removeLabelIds == [llm/needs-reply id]`, no add, INBOX not removed, the fetch carried `format=metadata`), `answered_thread_in_plain_bucket_keeps_label` (no modify and no `threads.get` for t1), `answered_sweep_dry_run_writes_nothing` (zero non-GET, one read), `unanswered_awaits_reply_thread_keeps_label`. `cargo test --test triage_isolation`: `test result: ok. 9 passed; 0 failed`.
  - unit: `thread_whose_newest_message_is_mine_is_answered`, `thread_whose_newest_message_is_inbound_is_not_answered`, `my_own_draft_is_not_an_answer`, `thread_of_drafts_only_is_not_answered` (src/triage/thread.rs); `test_plan_answered_targets_groups_a_thread_across_buckets`, `test_plan_answered_targets_on_an_empty_mailbox_plans_nothing` (src/triage/tests.rs).
- Criteria observed:
  - `rg -c 'keep-needs-reply' eratosthenes.example.yml`: no output, rg exit 1.
  - `cargo run -q -- -c eratosthenes.example.yml config validate`: exit 0, `Account 'eratosthenes.example' config is valid.`, 9 state filters including `age-needs-reply`, `Triage: not configured` (the example's triage block is commented out). A scratch copy with the triage block uncommented also validates, exit 0, `Buckets: 5 defined`, so the new keys parse under `deny_unknown_fields`.
  - `cargo test --test triage_archive`: 7 passed, including `dry_run_issues_zero_non_get_requests`.
- Break-to-prove, each restored afterwards:
  - Put the D1 early return back ahead of the sweep in `execute` (`if !triage.buckets.iter().any(|b| b.draft) { return Ok(()); }`). 2 failed: `answered_awaits_reply_thread_loses_label_with_draft_false` (`exactly one threads.modify: []`, `left: 0, right: 1`) and `answered_sweep_dry_run_writes_nothing` (`the dry run still reads the candidate`, `left: 0, right: 1`).
  - Swept every bucket (`b.awaits_reply || true`). `answered_thread_in_plain_bucket_keeps_label` failed (`a plain bucket keeps its label`, `left: 1, right: 0`), plus two older isolation tests whose plain bucket then hit an unmocked `threads.list`.
- `otto ci` exit 0.
### Deviations
- The sweep is called from `triage::execute`, not from inside `refresh_drafts` ahead of its early return. Same effect, correct seam: the sweep does not depend on the draft pass, and calling it from `execute` keeps `refresh_drafts` about drafts.
- tests/triage_archive.rs `mount_mailbox` gained a `GET threads` (list) mock returning nothing. Its config has an `awaits-reply` bucket, so live triage now lists it after the classify pass. No assertion changed.
- README's `awaits-reply` bullet was edited (not in the Phase 5 bullet list) so it states when the label is cleared. It said "the label is cleared", which was not true with `draft: false` until this phase.
### Tradeoffs
- No `max-threads` cap on the sweep, vs reusing the draft pass's cap. Each target costs one metadata `threads.get` and no LLM call; the doc's Performance section counts 6 such threads on 2026-11-11. A cap is a capacity feature for a problem not observed.
- A bucket with both `draft: true` and `awaits-reply: true` (allowed by config) is swept first, so the draft pass no longer sees an answered thread live. On a dry run nothing is removed, so the draft pass also reports it as answered: one thread can print twice in a dry run. Accepted: no bucket in the doc's config sets both.
- Sweep skips go into the shared ledger and count toward `max-skipped-threads`, vs a sweep-local count. Same ceiling the rest of triage uses.
### Open questions
- The example config's triage schedule is still `Mon..Fri 06:30:00`; the doc's Data Model shows hourly (`*-*-* *:30:00`). Phase 5 lists only the needs-reply TTL, the keys, and the comment, so it was left. Say if the example should go hourly now or with Phase 8.

## Phase 6: Timer fix and category clause
### Design decisions
- Deleted `Persistent=true` from the run timer only (src/service.rs:generate_timer), the one cause of the D2 stall (shape d). The digest and triage OnCalendar timers keep it.
- `test_generate_timer` assertion replaced in place: it now asserts the exact `[Timer]` section (`OnBootSec=2min`, `OnUnitActiveSec=5min`, then the blank line and `[Install]`) and that no `Persistent=` appears anywhere in the output.
- Category skip in src/engine.rs:build_active_threads_query keys on `to_gmail_id().starts_with("CATEGORY_")`. No `category:` clause is emitted either.
- New test `test_build_active_threads_query_skips_category_labels` (src/engine.rs): a `CATEGORY_PROMOTIONS` filter adds no `label:category_` or `category:` clause, and `in:inbox` is still present.
### Deviations
- None.
### Tradeoffs
- Skip by id prefix vs adding a `Label::Category` variant: the prefix is the smaller change and Phase 1 already normalizes the name; a variant touches every match on `Label`.
### Open questions
- None.

### Break-to-prove (observed, each restored)
- Put `Persistent=true` back after `OnUnitActiveSec={interval}`: `cargo test service` failed `service::tests::test_generate_timer` (`assertion failed: output.contains("[Timer]\nOnBootSec=2min\nOnUnitActiveSec=5min\n\n[Install]")`).
- Replaced the category skip condition with `if false`: `cargo test engine` failed `test_build_active_threads_query_skips_category_labels`; the query was `in:inbox OR label:category_promotions`.
- `otto ci` exit 0 after restoring both.

## Phase 7: senders
### Design decisions
- `eratosthenes senders [accounts...] [--days N] [--limit N] [--format yaml|json]` (src/cli.rs:Command::Senders, src/main.rs:cmd_senders). Same per-account pattern as digest/triage: sequential, failures aggregated, non-zero exit at the end.
- Format: `Format` ValueEnum with `ignore_case = true`. `Format::resolve(explicit, stdout_is_terminal)` is pure and unit-tested (yaml on a TTY, json when piped). One document for all accounts: a list of `{account, days, senders}`.
- Aggregation is the pure `senders::aggregate` (src/senders.rs): messages desc, threads desc, address asc, then truncate. `senders::collect` does the I/O: one `messages.list` (`in:inbox newer_than:<days>d`) plus one metadata `messages.get` per hit. GETs only.
- `automated` reuses the guard's predicate: new `TriageMessage::from_metadata(&GmailMessage)` (src/triage/thread.rs) builds a body-less TriageMessage so `is_automated` is called, not copied. A sender is automated when ANY of its messages in the window is.
- Fetch headers are `TRIAGE_HEADERS`, a superset of what `is_automated` reads.
- `unread` counts messages carrying UNREAD. `threads` is distinct thread ids per sender. Bucket split counts each of a sender's threads once per config bucket name whose label any of that sender's messages in the window carries. Empty without a triage block.
- Sender address is the first From address, lowercased only. Not `normalize_address`: dot/plus folding answers "is this me" and would merge distinct gmail senders here.
### Deviations
- `GmailClient::new` prints "Connecting to Gmail..." with `eprintln!` instead of `println!` (src/gmail/client.rs). Same effect, correct seam: piped `senders` json has to be the only thing on stdout. It moves for every subcommand; no test or doc asserts it on stdout.
### Tradeoffs
- Thread bucket labels come from the messages inside the window, vs a `threads.get` per thread: the doc's Performance section budgets one `messages.get` per inbox message and nothing more. A reply that arrived after the bucket write carries no label, but earlier messages of its thread in the window usually do.
- `any` automated vs `all`: the guard fires on any one automated message, so the report shows the same fact.
### Open questions
- None.

### Success criteria
- wiremock `tests/senders.rs`: zero non-GET requests and the ranking (Greenhouse 3 msgs / 3 threads / 2 unread / automated / noise 2; Nick 2 msgs / 1 thread / 1 unread / not automated; `Nick@x.com` and `nick@x.com` fold to one sender): PASS.
- `cargo run -q -- senders --help; echo $?`: exit 0 (PASS; exit 2 on main).
- Break-to-prove: ranking comparator changed to `a.messages.cmp(&a.messages)` made `senders_is_read_only_and_ranks_the_fixture` fail; restored.
- `otto ci` exit 0.

## Audit fold-in (implementation audit round 1)

Findings fixed: F1 (must-fix), F2 (cheap-win), F5 (doc fix). Source: review-panel run jxnw01H2, synthesis and probes.

### Findings and fixes
- **F1: a config that fails to load dropped its account silently.** `discover_accounts` (src/cfg/account.rs) turned any `load_config` error into `warn!("Skipping ...")`, which runs before `logging::setup` and so printed nothing. With Phase 1's `deny_unknown_fields`, a typo in `tatari.yml` made the work account vanish from `config validate`, `run`, `triage`, `digest` and `service install` while `home` kept going (probe 8). With one account it surfaced as the wrong error, `No config files found` (probe 9).
  - Fix: `discover_accounts` now returns the first load error, wrapped as `Failed to load account config <path>` with the parse error (key, line, column) as the cause. Candidate `*.yml` paths are sorted first, so the file reported is deterministic. Every caller propagates it: `resolve_accounts` (all subcommands, including `config validate`) and `service::install` (`discover_accounts()?`).
  - `discovered_account_names` (dynamic `--help` text) now returns `Result`. src/main.rs renders ``Discovered accounts: (a config failed to load, see `eratosthenes config validate`): <error>`` instead of the old `(none)`, which would have been a lie.
  - Observed after the fix, two-account XDG dir with `archvie: true` in tatari.yml: `config validate` exits 1, stderr `Error: Failed to load account config .../eratosthenes/tatari.yml` / `Caused by: Failed to parse YAML: triage: triage bucket 'n': unknown field 'archvie', expected one of ...` (backticks around the key in the real output).
  - Tests: new `tests/discovery.rs`, which points discovery at a temp dir through `XDG_CONFIG_HOME` with no `--config` (no existing test did this; `xdg_config_dir` honors an absolute `XDG_CONFIG_HOME`). `discovered_account_with_unknown_triage_key_fails_validate` asserts non-zero exit and that stderr names `tatari.yml` and `archvie`. `valid_discovered_accounts_validate` asserts both accounts validate.
  - Break-to-prove: restored the `match ... Err(e) => { warn!("Skipping ..."); continue; }` arm. `discovered_account_with_unknown_triage_key_fails_validate` FAILED at the non-zero-exit assert (tests/discovery.rs:57); the valid-dir test stayed green. Restored, both pass.
- **F2: an RFC 5322 comment defeated the owner match.** `parse_address_header` (src/gmail/message.rs; the audit named src/triage/thread.rs, which calls it) kept the comment, so `To: scott.idler@tatari.tv (Scott)` parsed as `scott.idler@tatari.tv (scott)`, failed `is_solely_to`, and row 3 did not protect that human mail.
  - Fix: new private `strip_comments` runs on the whole header before the `,` split. It drops parenthesized text, nestable, honoring `\` escapes, and leaves quoted strings alone. Stripping before the split also stops a comma inside a comment from splitting an address. An unclosed `(` runs to the end of the header, so the address after it is dropped rather than misread; for row 3 an empty To fails closed to "protect".
  - Tests: `human_message_to_me_with_address_comment_protects_thread` (src/triage/thread.rs, SoleHuman for a non-automated sender); in src/gmail/message.rs `test_parse_address_header_strips_trailing_comment`, `..._strips_comments_around_angle_addr` (`Scott (work) <scott.idler@tatari.tv> (Scott)`), `..._comment_with_comma_does_not_split` (including a nested comment), `..._keeps_parens_inside_quoted_name`, `..._unclosed_comment_drops_rest`.
  - Break-to-prove: made `strip_comments` return its input unchanged. Four tests FAILED: the trailing-comment, comma-in-comment and unclosed-comment parser tests, and `human_message_to_me_with_address_comment_protects_thread`. The angle-addr and quoted-name tests stayed green, as expected: the `rfind('<')` branch already handled those forms, so they are regression guards, not proofs of the fix. Restored, all pass. (A first attempt that bypassed the call failed to compile on the crate's `#![deny(dead_code)]`, so the break went into the helper body.)
- **F5: the `home.yml` starter's relative `creds-path` moves with the file.** Verified in code: `load_config` (src/cfg/config.rs) joins a relative `creds-path` onto `config_path.parent()` and leaves absolute and `~/` paths alone; `AuthConfig.creds_path` goes through `deserialize_tilde_pathbuf`, so `~/` is expanded at load and `gmail/auth.rs` gets an absolute path. `config show` on a scratch copy: `creds-path: home` -> `<scratch dir>/home`; `creds-path: ~/.config/eratosthenes/home` -> `/home/saidler/.config/eratosthenes/home`. `tatari.yml` (dotfiles) uses relative `creds-path: tatari`, read through its symlink at `~/.config/eratosthenes/tatari.yml`, so it resolves to `~/.config/eratosthenes/tatari` (holds `client-secret.json` and `tokencache.json`).
  - Fix (doc only): the starter now uses `creds-path: ~/.config/eratosthenes/home` with a bullet saying why; Phase 0b places the client secret there and notes where the token lands; Phase 9 notes nothing is copied; Resolved Decisions has the dated `2026-11-12 (implementation audit F5)` line. Status line untouched.

### Design decisions
- Fail on the first bad file, not after collecting all of them (src/cfg/account.rs:discover_accounts). `?` with a sorted path list is the smallest change that is loud and deterministic; a second bad file shows after the first is fixed.
- Help text reports the load error rather than `(none)` (src/main.rs:main, src/cfg/account.rs:discovered_account_names). `--help` must still render with a broken config, and `(none)` would say something false.
- Strip comments on the whole header before splitting (src/gmail/message.rs:strip_comments). It fixes the owner match and the comma-in-comment split in one pass; quoted strings are left intact.

### Deviations
- F2's fix lives in src/gmail/message.rs, not src/triage/thread.rs as the brief named it: `parse_address_header` is defined there and thread.rs only calls it. Same effect, correct seam. It also changes parsing for the other callers (draft reply-to, `GmailMessage` to/cc/from, `senders`), all toward the bare address.
- `discovered_account_names` changed signature (`Vec<String>` -> `Result<Vec<String>>`). Not named in the brief; it is a `discover_accounts` caller and had to stop swallowing the error.

### Tradeoffs
- Hard error on any `*.yml` in the config dir that fails to load vs skipping files with no `auth:` section: the old doc comment said non-eratosthenes yml would be skipped, but a skip cannot tell "not ours" from "ours with a typo". The live dir holds only `tatari.yml` (plus `digest.env` and the `tatari/` creds dir, neither of which is `*.yml`), so nothing there is affected.
- Hand-rolled comment stripper vs a mail-address parsing crate: the stripper is about 25 lines with tests; a crate would change every caller's parsing at once, which is past the finding's scope.

### Open questions
- None.

### Accepted, not fixed (panel reasoning)
- F3 (defer): a message that fails `TriageMessage::from_api` (missing id or internal_date, which Gmail always sends) is dropped with a warn, so protection never sees it and it keeps its own INBOX (it is not in `classified_message_ids`). The parser predates this work and the input is theoretical; if it is ever seen, skip the whole thread instead.
- F4 (defer): `senders` looks up `From` with exact case while `from_metadata` lowercases, so a sender whose header is named `from` reports address "". Report-only and read-only; Gmail normally returns `From`.
- README dry-run sample (no change): the "Connecting to Gmail..." block predates the archive column and the new summary format, but it is labeled as a 2026-09-07 capture, so it is historical, not wrong.
- The panel's five questions, all closed:
  1. Equivalent: `lib::triage` only builds the client and calls `triage::execute`, which sets metadata headers before `ensure_triage_labels` and the filter pass; no other caller exists.
  2. Sound, no hole: `classified_message_ids` covers the trimmed snapshot, so a late message keeps INBOX and no `llm/seen`, comes back next run, and is evaluated with the whole thread. A late SENT reply never carries INBOX. Residual, already disclosed: local T0 vs server internalDate skew.
  3. No behavioral difference: `sweep_answered` runs after classify_and_label (including the nothing-to-classify return) and before refresh_drafts, over INBOX plus the awaits-reply labels only; the draft arm keeps the same `is_answered`. Disclosed: no cap, and a double dry-run print when a bucket sets both draft and awaits-reply.
  4. Acceptable: no consumer of eratosthenes stdout exists in the repo or dotfiles, systemd captures both streams, and the move to stderr is required for piped `senders` json.
  5. Does not matter today: the personal account, the only gmail.com one, has no triage block, so rows 2/3 and the sweep never run there, and `senders` does not use `normalize_address`. It becomes a real limitation only if personal triage is turned on.

### CI
- `otto ci` exit 0 with all three fixes in.
