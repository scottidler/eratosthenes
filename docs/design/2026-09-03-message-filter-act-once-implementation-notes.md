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

## Phase 4: Marker label and act-once

### Design decisions
- `marker-label` lives on the account root as `Config::marker_label`
  (`src/cfg/config.rs`), `#[serde(default = "default_marker_label")]` -> `Triaged`,
  picked up by the struct's existing `rename_all = "kebab-case"`. No dotfiles change.
- Validation is `Config::validate` (`src/cfg/config.rs`), two named checks:
  `validate_marker_label` (no state-filter label, no state-filter destination,
  compared case-INSENSITIVELY and also rejecting an empty/whitespace marker) and
  `validate_move_position` (count `FilterAction::Move`; reject count > 1; reject
  count == 1 whose index is not `len - 1`). Both errors name the offending filter
  and which clause failed.
- New `pub fn parse_config(&str) -> Result<Config>` (`src/cfg/config.rs`) is
  parse-THEN-validate; `load_config` is that plus file IO and creds-path
  resolution. That makes "FAILS to load" assertable in a unit test against the
  real load path instead of a bare `serde_yaml::from_str`, which would bypass
  validation entirely.
- `LabelVisibility { Shown, Hidden }` (`src/gmail/label.rs`) replaces the
  hardcoded `labelShow`/`show` pair. Chosen over two `&str` parameters so the two
  strings can never be mismatched at a call site. Destinations are `Shown`; the
  marker is `Hidden`.
- `ensure_labels` (`src/engine.rs`) registers the marker LAST, after the
  destinations, with `LabelVisibility::Hidden`. Its `FilterAction` arm became an
  exhaustive `match` rather than `if let Move(..)`, so a future action variant that
  needs a label cannot be silently skipped; `Tag`'s label is registered too.
- `FilterAction::Tag(String)` (`src/cfg/filter.rs`) plus `parse_action`, which
  handles ONE action for both the scalar and sequence forms. Bare `Tag` is a loud
  error naming the mapping form rather than a `Move("Tag")`; bare `Move` likewise.
  `{Tag: <label>}` and `{Move: <label>}` mappings parse.
- The write plan is DATA: `PlannedWrite { action, ids, add, remove }` and the pure
  `plan_filter_writes(filter, matched_ids, messages, thread_labels, resolver,
  marker) -> Vec<PlannedWrite>` (`src/engine.rs`). `apply_planned_write` is the
  thin shell that issues one `batch_modify` and logs it. This is what makes "no
  STARRED add is issued" / "exactly one STARRED add" assertable with no Gmail:
  every Phase 4 success criterion is asserted against planned writes.
- Per-action scoping lives in `plan_pin_ids` (`src/engine.rs`), used ONLY by the
  `Star`/`Flag` arm: skip a thread whose label union already carries the pin, else
  emit exactly the newest matched message id in that thread (ties keep the first in
  matched order, so the plan is deterministic). `Move` and user `Tag` take
  `matched_ids.to_vec()` unchanged.
- Suppression input is `HashMap<String, HashSet<String>>` thread-id -> label union,
  filled by `fetch_thread_labels` with one `client.get_thread` per DISTINCT matched
  thread, cached across filters for the whole run, and only for filters where
  `pins(filter)` is true.
- `record_planned_pins` (`src/engine.rs`) folds each filter's planned pins back
  into that cache before the next filter plans. Not in the doc, and a real hole
  without it: claiming is per MESSAGE, so two different filters can match two
  different messages in one thread, and each would have seen a union with no
  `STARRED` and added its own star -- manufacturing exactly the multi-star threads
  this phase exists to stop. Runs under `--dry-run` too, so the preview equals what
  a real run would write.
- Marker enforcement is doubled: `-label:<marker>` appended in `compile_query`
  (lowercased, matching the existing `label:` convention) AND
  `MessageFilter::matches` rejects a message carrying `Label::new(marker)` before
  any other criterion. `matches` grew a `marker: &str` parameter; `""` disables it
  for the pre-existing tests that predate the marker.
- `--dry-run` moved from the top-level `Cli` struct into `Command::Run`
  (`src/cli.rs`), so clap REJECTS it for every other subcommand instead of
  accepting and ignoring it. Both user-facing strings reworded to "no message or
  thread changes; missing labels may be created", which is true given
  `ensure_labels` runs ahead of the dry-run guard.
- Both misleading `is:unread` comments corrected in place
  (`src/gmail/query.rs`, and the read-skip inside `match_message_filters` in
  `src/engine.rs`): it is a scope constraint, not an idempotency guard.

### Deviations
- The doc specifies the marker exclusion in `MessageFilter::matches`; implemented
  there, but as a new `marker: &str` parameter rather than by reading it off the
  filter, because the marker is account-global config and duplicating it onto every
  `MessageFilter` would create two sources of truth. Same effect, correct seam.
- The doc says `compile_query` appends `-label:<marker>`; implemented with the
  marker as an explicit parameter (`compile_query(filter, marker)`) rather than
  reaching for a `Config`, keeping the function pure like the rest of that module.
- `apply_filter_action` is GONE, replaced by `plan_filter_writes` +
  `apply_planned_write`. The doc's Phase 4 text describes editing the action loop
  in place ("fold the marker into the `Move` write"); the doc's own Testing
  Strategy and success criteria require the write set to be inspectable without a
  live mailbox, and its "Rejected: fold into the last APPLIED action" analysis
  explicitly names plan-then-write as the restructure that would be needed for a
  per-message lookahead. This is that restructure, done for testability, WITHOUT
  adopting the rejected per-message rule: the carrier is still knowable from
  `filter.actions` alone.
- `record_planned_pins` has no counterpart in the doc: see Design decisions for
  why it is required rather than optional.
- The `Tag` action is expressible in config as `{Tag: <label>}`; the doc says only
  that `Tag` must not parse as `Move("Tag")` and that "exposing `Tag` in config
  costs nothing". A bare `Tag` string cannot name a label, so it is a named error
  instead of a silent Move.
- Marker collision is validated against STATE filters only, as the doc specifies.
  A marker colliding with a MESSAGE-filter `Move` destination is not rejected; see
  Open questions.
- The two exact-match query tests were updated as the doc predicted
  (`from:(*@company.com) -label:triaged is:unread`,
  `subject:(urgent) -label:triaged is:unread`).
- Phase 4 does NOT assert the live `Done: 0` criterion, per the doc: this phase
  causes one final re-pin wave until Phase 5 backfills markers.

### Tradeoffs
- Marker written as its RESOLVED Gmail id (`resolve_label_id`, falling back to the
  name) rather than plumbing a hard failure when the marker is unregistered. Kept
  the codebase's existing `unwrap_or(name)` idiom; `ensure_labels` guarantees
  registration before the message-filter stage, and the fallback is correct for
  system labels whose id equals their name. A stricter fail-closed variant would
  have to change the same idiom at four other call sites.
- Thread unions are keyed by RAW label id, not by `Label`, because
  `GmailThread::label_ids` already returns raw ids and the two pins the suppression
  check cares about (`STARRED`, `IMPORTANT`) are system labels whose id equals
  their name. A resolved-`Label` union would be more uniform and buy nothing here.
- Suppression state is read ONCE per thread per run, before any write. Cheaper than
  re-fetching per filter, and `record_planned_pins` keeps it coherent with what
  this run is about to write; it does NOT see a concurrent external change made
  mid-run, which a double stamp makes harmless anyway.
- A filter with matches but NO actions now gets a marker-only write. It claimed the
  message, so under "marker means HANDLED" it did handle it. The alternative
  (stamp nothing) would leave the message eligible forever while still being
  claimed away from later filters every run.
- Bare `eratosthenes --dry-run` is now a parse error and must be typed
  `eratosthenes run --dry-run`. The doc accepts this cost explicitly; the smoke
  test pins both halves (accepted on `run`, rejected on `digest` and
  `service install`).

### Open questions
- `marker-label` is validated against state filters only, per the doc's exact
  wording. A marker equal to a MESSAGE-filter `Move` destination (e.g.
  `marker-label: Bots`) loads today: it would make that filter's add-list carry the
  same id twice (harmless) and would exclude every `Bots`-labeled message from
  every filter (mostly already true via `is:unread`). Worth rejecting for the same
  fail-loudly reason, but it is not in Phase 4's scope and no config does it.

## Phase 5: Marker backfill mode, example config, README

### Design decisions
- `--mark-only` is a new field on `Command::Run` (`src/cli.rs`), alongside `dry_run`,
  threaded through `cmd_run` (`src/main.rs`) -> `eratosthenes::run`
  (`src/lib.rs`) -> `engine::execute` (`src/engine.rs`) as a plain `bool`, mirroring
  the existing `dry_run` plumbing exactly. Bare `eratosthenes` (no subcommand) is
  unaffected: it still calls `cmd_run` with both flags `false`.
- `engine::execute` branches on `mark_only` FIRST, before the dry-run banner: when
  true it logs a distinct `=== MARK-ONLY: ... ===` banner, calls `ensure_labels`
  (the marker must be registered before anything can fold its id into an add-list,
  same ordering requirement as a normal run), calls `execute_message_filters` with
  `mark_only=true`, logs a `Done: N messages marked (mark-only)` line, and
  `return`s -- Phase 0 (`sanitize_stages`) and Phase 2 (state filters) never run.
  Chosen because the doc's own wording is "reuses account discovery and the
  matching path"; sanitize_stages and state-filter age-off are unrelated to the
  marker and mutate the mailbox in ways a one-shot backfill tool has no business
  doing.
- `dry_run` still threads through into mark-only mode (`execute_message_filters`'s
  `dry_run` parameter is orthogonal to `mark_only`), so `run --dry-run --mark-only`
  previews exactly what would be stamped with zero Gmail writes, using the same
  seam the normal dry-run path already uses.
- Implemented at the PLAN level, per the team-lead's direction: `plan_filter_writes`
  (`src/engine.rs`) grew a `mark_only: bool` parameter and, when true, returns
  early with exactly ONE `PlannedWrite { action: Tag(marker), ids: matched_ids
  (the FULL handled set), add: [marker_id], remove: [] }` before any of the
  existing Star/Flag/Move/fold logic runs. No pin write, no `Move` write, and
  critically no `INBOX`/`UNREAD` removal, because in mark-only mode nothing
  destroys eligibility, so the marker must not carry a removal either -- doing so
  would archive the `bots` candidates, which the doc calls out as a serious
  defect. This is what makes "issues ZERO STARRED/IMPORTANT/Move modifications"
  assertable on the plan alone, exactly as Phase 4's tests already do.
- `execute_message_filters` skips `fetch_thread_labels` when `mark_only` (no
  suppression check is ever consulted in mark-only mode, so the `threads.get`
  round trip buys nothing) and calls `plan_filter_writes(..., mark_only)`.
  `record_planned_pins` is still called unconditionally: it is a no-op for
  mark-only's `Tag`-only writes (it only folds `Star`/`Flag` adds), so branching
  around it would add a condition for zero behavior change.
- New `log_mark_only_stamps(ids, messages, prefix) -> usize` (`src/engine.rs`)
  logs exactly one INFO line per id (`[mark-only] stamped <id> date=... from=...
  subject=...`) and returns the count actually logged. Called from
  `execute_message_filters`'s write-application loop, once per planned write,
  before `apply_planned_write` issues it. Returning the count (rather than `()`)
  makes "stamps N messages while logging exactly N lines" a directly assertable
  unit-test property instead of a log-capture exercise: no log-capturing
  infrastructure exists anywhere in this codebase, so per-call-count is the
  testable analog of per-message-count logging.
- `apply_planned_write` grew a `mark_only: bool` parameter whose only effect is to
  suppress the pre-existing aggregate `"[filter:{}] tagging {} messages with
  {}"` INFO line for the `Tag` arm when `mark_only` is true: the per-message
  lines from `log_mark_only_stamps` already logged the same information at
  message granularity, so the aggregate line would be a paired-log
  discrepancy (N lines claiming to be N, plus one more) rather than a clean
  match against the success criterion.
- `eratosthenes.example.yml` gained a `marker-label` block explaining default,
  purpose, the state-filter-collision constraint, and the Gmail-UI reset
  mechanism, placed before `message-filters:` since the marker is what those
  filters exclude. `README.md` gained a "Message filters act once" section:
  what the marker does, how to override/reset it, and the exact 5-command
  rollout sequence (stop timer, dry-run sanity check, `run --mark-only`, a
  normal run, restart timer) lifted from the doc's own Rollout Plan.

### Deviations
- None. The plan-level collapse in `plan_filter_writes` is the "correct seam" the
  team-lead's task message itself specified, not a deviation from it.

### Tradeoffs
- `log_mark_only_stamps` is called from inside the write-application loop
  (per-write, per-filter) rather than once at the end over a globally
  deduplicated set. Since `claimed` already makes each filter's `matched_ids`
  disjoint (Phase 2), and mark-only always emits exactly one write per filter
  covering that filter's own `matched_ids`, this is equivalent to a single
  end-of-run pass with no duplicate ids possible across filters -- simpler to
  call it at the existing per-write seam than to plumb a second accumulator
  through the whole function.
- Suppressing the aggregate `Tag` log line only in `mark_only` mode (not always)
  keeps the normal-run log shape for a user-configured `Tag` action unchanged
  from Phase 4, at the cost of one extra `bool` threaded into
  `apply_planned_write`. The alternative (always suppress it for `Tag`) would
  silently change a normal run's existing log output for a case Phase 5 was not
  asked to touch.

### Open questions
- None.
