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
