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
