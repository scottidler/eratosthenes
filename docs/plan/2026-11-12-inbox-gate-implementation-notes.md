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
