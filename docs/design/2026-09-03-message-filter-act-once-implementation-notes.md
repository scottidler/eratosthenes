# Implementation Notes: Message-Filter Act-Once Semantics

Design doc: `docs/design/2026-09-03-message-filter-act-once.md`

## Phase 1: Compile multi-pattern address fields to Gmail OR

### Design decisions
- `compile_query` (`src/gmail/query.rs`): `to` now mirrors `from`'s single-vs-multi
  branch structure instead of pushing one `to:pat` term per pattern. Multi-pattern
  compiles to exactly one `to:{a b}` brace-OR term; single-pattern is unchanged
  (`to:pat`, no wrapping).
- `from`'s multi-pattern branch switched from `from:(a b)` (space-joined inside
  parens, which Gmail reads as AND) to `from:{a b}` (brace-OR). Single-pattern
  `from:(a)` is unchanged.
- Added a small private helper `join_patterns(&[String]) -> String` shared by both
  the `to` and `from` multi-pattern branches, to avoid duplicating the
  `iter().map(str).collect::<Vec<_>>().join(" ")` chain — same effect as inlining
  it twice, kept as one function since both branches produce identical joined
  content and only the surrounding brace/field differ.

### Deviations
- Baseline commit `278af89` failed `otto ci`'s clippy gate (`useless_borrows_in_formatting`
  on `src/main.rs:64`, `&name` where `name` suffices), unrelated to this design doc or
  Phase 1's scope. Per team-lead direction, fixed and committed separately as `a1a37c8`
  ("fix(clippy): drop redundant borrow in account start log") BEFORE the Phase 1 commit,
  so the Phase 1 commit's diff maps cleanly to the doc's Phase 1 bullets. Verified
  `otto ci` green at `a1a37c8` before applying the Phase 1 change on top.
- Single-pattern `to:` output kept byte-identical to current `main` (`to:pat`, no
  braces or parens) rather than switching to `to:{pat}` for uniformity with the
  multi-pattern case, since the doc's stated fix is "single pattern keeps `from:(a)`"
  (by analogy, `to`'s existing single-pattern form) and no test in the doc's Testing
  Strategy or Phase 1 success criteria requires changing it.

### Tradeoffs
- Kept the `to`/`from` blocks as separate near-duplicate `if let Some(ref af) = ...`
  bodies rather than merging into one generic `compile_address_field(field, af)`
  helper covering both, because their single-pattern formats differ (`to:pat` vs
  `from:(pat)`) and a shared helper would need a closure or enum parameter for that
  one difference, adding indirection for two call sites.

### Open questions
None.

## Phase 2: Per-filter candidate scope

### Design decisions
- New private struct `FilterCandidates<'a> { filter: &'a MessageFilter, ids: Vec<String> }`
  (`src/engine.rs`) pairs each filter with the ids ITS OWN query returned. Chosen over
  two parallel `Vec`s (`filters` + `Vec<Vec<String>>`) so the filter-to-candidates
  pairing cannot drift.
- `execute_message_filters` still builds the deduped `all_ids` union, but now only as
  the input to the single `get_message` fetch pass. FETCHES stay deduped across filters;
  SCOPE does not. `all_ids` is never iterated by a filter again.
- Match loop extracted into a pure function
  `match_message_filters(&[FilterCandidates], &HashMap<String, GmailMessage>, prefix)
  -> Vec<Vec<String>>`, returning matched ids per filter, parallel to the input. No
  client, no `async`, no writes: the whole match plan is computable and assertable
  before anything is applied, which is what makes the scope-isolation test possible
  (`execute_message_filters` had zero coverage) and what Phases 4-5 need to test
  suppression and `--mark-only` without a live mailbox.
- `claimed` now lives inside that pure function and an id is inserted the MOMENT it
  matches, not in a second pass after each filter. Behavior across filters is unchanged
  (first-match-wins, `src/engine.rs:248` on main); the effect is that the per-filter
  matched sets are provably disjoint, so the caller's running sum equals the claimed-set
  size and the `total claimed:` debug line stays exactly as it was without threading a
  `HashSet` back out of the function.
- The prefilter/authority split is documented on `match_message_filters` itself, at the
  seam where candidates are re-checked, rather than at the search loop: Gmail cannot
  express `cc: []`, `headers.List-Id: []`, or globset semantics, so per-filter scope
  explicitly does NOT mean trusting a filter's query to be precise.
- Both per-filter debug counts are preserved and still interleave per filter
  (`query returned N candidates` from the search loop, `N matched (total claimed: M)`
  from the apply loop), so the rollout's `matched <= query returned` check stays
  readable in `tatari.log`.

### Deviations
- The doc's Phase 2 bullet says "keep a per-filter `Vec<String>` of ids". Implemented as
  a `Vec<FilterCandidates>` holding exactly that `Vec<String>` alongside its filter:
  same data, correct seam, no parallel-index invariant to maintain.
- The doc says "the match loop walks the filter's own ids" in place; the loop was moved
  into a pure function instead of edited in place, per the team-lead's testability
  direction and the repo's "return data, not side effects" rule. Same behavior, and the
  apply loop that follows it is unchanged.

### Tradeoffs
- Matching now completes for ALL filters before the first action is applied, instead of
  match-then-act per filter. Cost: on a mid-run error the engine has computed matches it
  never applies. Benefit: the match pass is pure and testable, and the write plan is
  known before any write. Nothing observable changes for the operator, because the
  `[filter:*]` debug/info line order is preserved.
- `match_message_filters` returns `Vec<Vec<String>>` parallel to its input rather than a
  richer per-filter result struct. Kept minimal because Phase 4 will need to extend this
  return shape anyway (thread suppression, marker set), and inventing that shape now
  would be guessing at it one phase early.

### Open questions
None.

## Phase 3: Resolve custom labels in message-filters

### Design decisions
- Added `GmailMessage::labels_resolved(&LabelResolver) -> Vec<Label>`
  (`src/gmail/message.rs`), byte-for-byte the same shape as
  `GmailThread::labels_resolved`: map each raw `label_ids` entry through
  `resolver.resolve_id`, falling back to the raw id when the resolver has no name for
  it (covers system labels like `STARRED`/`UNREAD`, whose id already equals their name).
- `match_message_filters` (`src/engine.rs`) takes `resolver: &LabelResolver` as a new
  parameter and calls `msg.labels_resolved(resolver)` in place of `msg.labels()` at the
  one call site. `execute_message_filters` passes `&client.resolver` in, so the pure
  matcher stays free of `GmailClient` and any async/Gmail dependency, per the team-lead's
  explicit instruction, while the resolver that's already loaded before the message-
  filter stage runs (`ensure_labels`, `src/engine.rs:33`) reaches the comparison.
- Added `empty_resolver()` test helper (`LabelResolver::from_api_labels(vec![])`) so the
  three pre-existing `match_message_filters` tests, which don't exercise label
  resolution, only need a no-op resolver rather than constructing real label data.

### Deviations
- The doc names the call site as `src/engine.rs:266`; by the time this phase started
  (after Phase 2's extraction into `match_message_filters`) it was at line 323, per the
  team-lead's task message. Same seam, different line number after Phase 2's refactor;
  fixed at its current location, not the stale one.
- The doc says "`client.resolver` is already in scope" at the call site, true on `main`
  but not true after Phase 2: the call site now lives inside the pure
  `match_message_filters`, which has no `client`. Threaded `&LabelResolver` in as an
  explicit parameter from `execute_message_filters` instead, keeping the function pure
  and injectable rather than reaching for a client inside it. Same effect, correct seam.

### Tradeoffs
- Passed `&LabelResolver` rather than the whole `&GmailClient` into
  `match_message_filters`, even though `execute_message_filters` already has a
  `&GmailClient` on hand. Narrower parameter, and keeps the pure matcher's dependency
  surface limited to exactly what it uses, matching Phase 2's stated goal of a testable,
  client-free match plan.

### Open questions
None.
