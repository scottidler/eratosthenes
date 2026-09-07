# Implementation Notes: LLM Triage Layer

Running, append-only record of how the implementation diverges from or
interprets `docs/design/2026-07-06-llm-triage.md`. Append per phase; never
rewrite history. A later decision that overrides an earlier one is a NEW entry
that supersedes it.

## Phase 0: Prove the environmental assumptions -- zero code

Executed inline (not delegated) on 2026-09-06 against v0.3.0, host desk.lan.
Zero code, so no commit and no `otto ci` for this phase. Full observed output
is recorded in the design doc under "Phase 0 results".

### Design decisions
- Ran Phase 0 inline rather than via `phase-implementer` -- the agent's
  contract is code + tests + CI + commit, and this phase produces none of
  those. Its whole output is judgment about the host environment that the
  later phases read out of the doc.
- Probe (c) forces binary resolution INSIDE the systemd unit
  (`/bin/sh -c 'command -v claude'`) rather than letting `systemd-run` resolve
  the executable. This is the only form that tests what `Command::new("claude")`
  will do at runtime. Recorded in the doc because the naive form passes
  spuriously and would have certified a broken assumption.
- Probe (c) approximated the planned `env_clear()` + allowlist with `env -i`
  plus HOME, USER, PATH, `NO_UPDATE_NOTIFIER=1`. Sufficient to prove the
  keyless transport authenticates with no `ANTHROPIC*` var present.

### Deviations
- **Phase 0(a) was not run.** The `drafts.create` probe against the live work
  inbox was refused by the local agent permission layer before any Gmail API
  call was made. This is a BLOCK, not a failure: nothing was learned about
  threading either way. It gates Phase 7 only.

### Tradeoffs
- Used an AWS notification thread (`1a0797a6bf20de2e`) as the probe target for
  (a) and (b) rather than a human correspondence thread -- a stray draft on a
  no-reply notification is the lowest-consequence place to prove threading.
- Recorded the single-call latency as a BASELINE ONLY and explicitly declined
  to let it confirm either timeout, per the phase's own success criteria. One
  call bears on neither a 50-thread triage run nor a 10-thread digest pass.

### Open questions
- **Phase 0(a) must be run before Phase 7 begins.** It needs permission to
  create and then delete one draft in the live work inbox.

### Doc amendments made in this phase (with evidence)
- **Acceptance criteria 1, 2, 3 and 5 had no `Observed on main:` line**, so the
  doc had skipped its own ready-to-build gate (only criterion 4 carried one).
  All four were executed against `main` before Phase 1 and their output
  recorded. All four FAIL pre-build, which is correct and expected. None was a
  doc defect: `eratosthenes triage --dry-run` exits 2 with
  `error: unrecognized subcommand 'triage'`, and the `triage` subcommand plus
  its `--dry-run` flag are specified in the doc's API Design and built in
  Phase 4. No criterion was amended to match the code.
- **Phase 7's guard-pattern note was factually wrong and is corrected.** It
  said a grep for `messages_send`/`drafts_send` "may never match the crate's
  actual builder surface". Measured against `google-gmail1 7.0.0+20251215`,
  both DO match, at `api.rs:2130` and `api.rs:1733`. The real hazard the doc
  had not named is that `drafts_send` is a second sender, and it is precisely
  the call that would send the drafts Phase 7 creates.
- **Phase 5's PATH work is now load-bearing, not precautionary.** Phase 0c
  confirmed `claude` does not resolve on the generated unit PATH.

## Phase 1: Config schema + validation

Implemented on 2026-09-06 against v0.3.0, host desk.lan. `otto ci` green.

### Design decisions
- New file `src/cfg/triage.rs` holds `TriageConfig` and `TriageBucket`,
  matching the one-struct(-family)-per-file convention already in `src/cfg/`
  (`filter.rs`, `state.rs`, `label.rs`). Wired into `Config` as
  `pub triage: Option<TriageConfig>` (`src/cfg/config.rs`), mirroring the
  existing `Option<SlackConfig>` pattern exactly.
- `schedule: String` carries NO `#[serde(default)]`, so a `triage:` block with
  no `schedule` fails to deserialize with serde's own "missing field
  `schedule`" — the identical mechanism already proven for
  `slack.schedule` (`src/cfg/config.rs:75`, `test_slack_block_requires_schedule`).
  No custom validator was needed for this rule; the absence of a default IS
  the enforcement, deliberately, consistent with the design doc's own framing
  ("no default, deliberately").
- `buckets` gets a hand-rolled `deserialize_buckets` (`src/cfg/triage.rs`) that
  walks the raw YAML sequence one entry at a time instead of deriving
  `Vec<TriageBucket>` directly. A plain derive would surface only serde_yaml's
  generic "missing field `label`" with no way to tell WHICH bucket failed when
  several are present. The custom deserializer peeks the entry's `name` field
  out of the raw `Value` before attempting the real parse, and folds it into
  the error message (`"triage bucket '{name}': {serde error}"`), which is what
  the success criterion ("naming the offending bucket and field") actually
  requires. A bucket missing `name` itself falls back to `#<position>`
  (1-indexed) so the error never goes anonymous.
- `classify-model`, `draft-model`, `max-threads`, and `body-chars` all default
  per the doc's stated values (`claude-haiku-4-5-20251001`, `claude-sonnet-5`,
  `50`, `4000`). `buckets` itself defaults to the same five-bucket taxonomy
  shown in the doc's Data Model / shipped in `eratosthenes.example.yml`
  (needs-reply, fyi-work, recruiting, receipts, noise), so a bare `triage:`
  block carrying only `schedule` still has a working taxonomy end to end —
  matching the doc's description of those YAML values as "illustrative
  defaults", read literally.
- `config validate` and `config show` (`src/service.rs`) both grow a `Triage:`
  section (configured/not-configured, schedule, bucket list, and — for `show`
  — every other field) even though the pre-existing `slack` block was NOT
  previously surfaced in either command. This phase's own success criteria
  require `config validate|show` to cover the triage block explicitly, so
  triage's coverage is now ahead of slack's rather than matching it; slack's
  gap is pre-existing and out of scope here.

### Deviations
- None. The struct shape, field names, and defaults match the design doc's
  Data Model section exactly; the only addition beyond the doc's literal text
  is the per-bucket error-naming mechanism, which the doc's own Phase 1
  success criterion requires but does not specify an implementation for.

### Tradeoffs
- Named-bucket error enrichment via a custom `deserialize_buckets` vs. a
  post-load `Config::validate()` check (the pattern already used for
  `marker_label` collisions in `config.rs`). Chose the deserializer: `label`
  stays a required, non-`Option` `String` (honest typing — later phases can
  rely on it being present without an `.expect()`), and the error fires at the
  exact point of failure rather than requiring a second pass over an
  already-parsed (but conceptually invalid) struct. The tradeoff is a few more
  lines of `Value`-walking code, matching the same pattern already used by
  `deserialize_named_filters` / `deserialize_named_states` in `config.rs`.
- Bucket list defaulting to the full five-entry taxonomy vs. defaulting to an
  empty list. An empty default would make a bare `triage: {schedule: ...}`
  block load successfully but classify nothing (Phase 4 would batch-classify
  zero buckets against a config with none defined). Defaulting to the shipped
  taxonomy keeps "config validates" and "config does something sensible"
  aligned, at the cost of the default living in two places (this file and
  the example YAML) that must be kept in sync by hand if the doc's bucket set
  ever changes.

### Open questions
- None.

## Phase 2: Aging-engine Move semantics for labeled filters

### Design decisions
- Move label math extracted into a pure planner, `plan_state_move`
  (`src/engine.rs`), returning a `PlannedMove { add, remove }` rather than
  computed inline in `apply_state_action`. This follows the message-filter
  precedent (`plan_filter_writes` -> `PlannedWrite`) already in this file:
  the semantics are assertable as data with no `GmailClient`, which is the
  only way the three filter-shape tests this phase requires can exist at all
  (`apply_state_action` needs a live client and cannot be unit-tested).
- `derive_stages` is computed ONCE per run in `execute_state_filters` and
  threaded down as `stages: &[String]` through `evaluate_thread` into
  `apply_state_action`, rather than recomputed per thread or rebuilt inside
  the action. The stage ladder is a property of the config, not of a thread.
- `evaluate_thread` also passes the `thread_labels` it already resolved into
  `apply_state_action` instead of letting it re-resolve them off the thread.
  One resolution per thread, and the planner takes resolved names as data.
- `INBOX` is removed unconditionally on a Move (unless it IS the destination),
  per the doc. Note it is normally redundant: `derive_stages` puts `INBOX`
  first in the ladder, so a thread carrying `INBOX` sheds it via the stage
  loop anyway. It bites only when the thread does not carry `INBOX`, where
  the removal is a Gmail no-op. Kept because the doc specifies it and because
  it makes the rule readable without knowing `derive_stages`' implicit stage.
- The destination is never in the remove set, so a filter that moves a thread
  to the stage it already occupies is a no-op instead of a self-cancelling
  add+remove pair.
- An empty Move destination (the `action:`-less `default_action`,
  `StateAction::Move(String::new())`) now plans an EMPTY add list. The old
  code resolved `""` and sent `add: [""]`, which is a Gmail 400. Unreachable
  today for `Ttl::Keep` filters (they never produce an action), but reachable
  for a TTL filter written without `action:`.

### Deviations
- The doc's success criterion "all three filter-shape tests pass and are
  demonstrated to fail against the old remove-own-labels behavior" is only
  satisfiable for TWO of the three shapes, and this was measured, not assumed.
  A temporary side-by-side of the old rule against the new one printed:
  stage-transition `Purgatory -> Oblivion` old `remove=[Purgatory]` vs new
  `remove=[INBOX, Purgatory]` (differs); bucket `llm/noise -> Purgatory` old
  `remove=[llm/noise]` vs new `remove=[INBOX]` (differs); bare catch-all
  `Cull` old `remove=[INBOX]` vs new `remove=[INBOX]` (IDENTICAL). The bare
  catch-all is precisely the shape where old and new agree by construction:
  with no `labels:` the old rule fell back to `INBOX`, which is what the stage
  rule now derives. That test is therefore a pure regression pin, and its bite
  was demonstrated against mutations of the NEW code instead (see below).
- Mutation results, all four mutations applied to `plan_state_move` and
  reverted: (1) drop the unconditional `INBOX` removal -> the stage-transition
  test fails; (2) remove every ladder stage whether or not the thread carries
  it -> 5 of 7 fail; (3) drop the "never remove the destination" guard -> the
  destination test fails; (4) strip every label the thread carries (the old
  rule's spirit) -> 4 of 7 fail, including the bare catch-all. Every test
  bites under at least one mutation.
- `apply_state_action`'s signature gained TWO parameters, not one:
  `thread_labels: &[Label]` and `stages: &[String]`. The doc anticipated
  threading "`derive_stages`' output (or the filters themselves)"; the
  resolved thread labels ride along for the same reason (the planner must be
  pure, and the caller already has them).

### Tradeoffs
- Passing the derived stage list vs. passing the `state_filters` themselves.
  Chose the stage list: `apply_state_action` has no business re-deriving a
  ladder, and passing filters would let a future edit reintroduce
  "remove the filter's own labels" without changing a signature.
- Intersecting the ladder with the thread's CURRENT labels vs. removing every
  ladder stage unconditionally. Unconditional removal would be one line
  shorter and is harmless at the API level (removing an absent label is a
  no-op), but it makes every Move write name labels the thread never had,
  which is noise in `--dry-run` output and in the debug log, and it erases the
  distinction the tests need in order to bite.
- `PlannedMove` is a private struct rather than reusing `PlannedWrite`.
  `PlannedWrite` carries `action: FilterAction` and an `ids` list, neither of
  which a thread-level `modify_thread` has. Reuse would have meant a
  `FilterAction` value that lies about what the write is.

### Open questions
- Empty Move destinations are now silently "archive to nowhere". If that is
  never a legitimate config, it belongs as a load-time error in
  `Config::validate` (alongside `validate_move_position`) rather than as a
  runtime behavior. Not added here: it is config validation, which is Phase 1
  territory, and adding it now would reject configs that load today.

## Phase 3: Bucket labels + state-filters (config, dotfiles repo)

No eratosthenes source changed this phase; the deliverable is
`scottidler/dotfiles` `HOME/.config/eratosthenes/tatari.yml`
(committed separately in that repo).

### Design decisions
- Added five `state-filters` entries, one per `llm/*` bucket in
  `src/cfg/triage.rs`'s default taxonomy: `keep-needs-reply` (`ttl: Keep`),
  `age-fyi-work`, `age-recruiting`, `age-receipts`, `age-noise` (all
  `action: Purgatory`). Names are literal, chosen to match the doc's own
  success-criterion text (`[state:age-noise]`, `protected by
  'keep-needs-reply'`) rather than the existing file's Title-Case style
  (`Starred`, `Cull`), since `evaluate_thread`/`apply_state_action` print
  `state_filter.name` verbatim (`src/engine.rs:916`, `:1007`) and the
  criterion names the filters exactly.
- `keep-needs-reply` placed immediately after `Important`, ahead of every
  TTL/Cull entry, per the doc's ordering constraint (Keep-first, config
  order, first-match-wins in `evaluate_thread`).
- The four TTL bucket entries placed AFTER the Keep block but BEFORE `Cull`,
  with a comment explaining why: `Cull` matches every `INBOX`-labeled thread
  regardless of other labels (`StateFilter::matches_labels`,
  `src/cfg/state.rs`), so a bucket entry placed after it would never fire on
  a thread that still carries `INBOX` (which every freshly-classified bucket
  thread does, per the doc's Data Model: classification only adds a bucket
  label, it does not move the thread out of the inbox). This is the exact
  hazard the design doc calls out at line 165-166 ("Cull matches ALL inbox
  threads... so bucket labels MUST get their own state-filter entries or
  they age on the default rail").
- `action: Purgatory` (not a new stage) for all four bucket entries: reuses
  the existing `INBOX -> Purgatory -> Oblivion` ladder from `Cull`/`Purge`
  instead of introducing a parallel one. Keeps `derive_stages` unchanged
  (still `[INBOX, Purgatory, Oblivion]`) and needs no engine change, matching
  this phase's scope (Phase 4 owns the engine).
- Per-bucket TTL values are a config knob the doc explicitly leaves
  unspecified ("Each bucket's TTL is a config knob", doc line 167; "llm/noise
  -> short TTL, etc.", line 224) beyond "needs-reply protected, noise
  shortest". Chose: `fyi-work` 3d read / 7d unread (transient work
  notifications, same read/unread split as `Cull`), `recruiting` 3d flat,
  `receipts` 7d flat (kept a bit longer for reference), `noise` 1d flat
  (shortest, per the doc's own "short TTL" language). These are Scott's to
  retune; flagged below as an open question rather than silently assumed
  correct forever.

### Deviations
- **Label-creation half of this phase was NOT attempted, per explicit
  orchestrator instruction.** The doc's Phase 3 bullet also calls for
  creating the `llm/*` labels in the live work Gmail account via one-off
  `gws` calls. Both `gws gmail users labels create` and `gws gmail users
  drafts create` are denied by this session's permission classifier (writes
  blocked, reads unaffected); no alternate route was attempted, per
  instruction. This is a bootstrap-convenience step only: the design doc
  states the engine ensures `llm/*` labels exist itself from Phase 4 onward
  ("Labels: the engine ensures `llm/*` labels exist at run start
  (`labels.create`, idempotent, fail loudly). No operator label step.", doc
  line 252), so nothing here is blocked on it. Deferred to Phase 4.
- Filter naming (`age-noise`, `keep-needs-reply`, lowercase-kebab) diverges
  from the pre-existing file's Title-Case names (`Starred`, `Cull`, `Purge`).
  Not a spec gap: the doc's own success criterion fixes these exact strings
  as the expected `[state:...]` log-line content, so matching the file's
  prior style would have failed the criterion instead of the doc.

### Tradeoffs
- One `action: Purgatory` ladder shared by `Cull` and all four bucket
  filters vs. a dedicated `Bucket-Purgatory` stage per bucket (or per
  bucket-group). Chose the shared ladder: it needs zero engine changes,
  keeps `derive_stages` a two-hop ladder, and the existing `Purge` entry
  (`Purgatory -> Oblivion`, flat 3d) already generalizes over "how a thread
  got into Purgatory". The cost is that a `llm/receipts` thread and a
  same-age plain-INBOX thread become indistinguishable once both land in
  Purgatory; accepted because nothing downstream currently needs to tell
  them apart.
- Flat TTLs (`age-recruiting`, `age-receipts`, `age-noise`) vs. `read`/
  `unread` splits (`age-fyi-work`, matching `Cull`) for all four. Chose flat
  for three of four: recruiting/receipts/noise are catch-all buckets Scott
  is unlikely to leave "unread but seen" the way an actionable fyi-work
  notification might be, so the read/unread distinction buys little; kept
  it for `fyi-work` specifically because that bucket is the one most likely
  to contain something worth reading before it ages.

### Open questions
- **The four numeric TTL values (3d/7d unread for fyi-work, 3d for
  recruiting, 7d for receipts, 1d for noise) are this implementer's choice,
  not specified anywhere in the design doc.** Scott should confirm or retune
  them; they are easy to change (YAML edit, no code).
- **Success criterion 2 is UNVERIFIED, not passing and not failing.** The
  doc's stated Phase 3 success criteria are (1) `config validate` passes and
  (2) engine `--dry-run` output contains a `[state:age-noise]` line for a
  hand-labeled `llm/noise` test thread and a `protected by
  'keep-needs-reply'` line for a hand-labeled `llm/needs-reply` thread.
  Criterion 1 is verified true (`config validate` output recorded below).
  Criterion 2 requires hand-labeling live Gmail threads with `llm/noise` and
  `llm/needs-reply`, which requires the same blocked Gmail-write path as
  label creation above. Not attempted, not faked, not weakened: reported
  here as UNVERIFIED with this reason, for Phase 4 (or a follow-up manual
  step) to close out once `gws` writes or the engine's own label-ensure are
  available.
