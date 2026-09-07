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

## Phase 4: Triage engine

Implemented 2026-09-06 against v0.3.0 on top of Phase 3 (`ecaacf6`). Scope set
by the orchestrator: code and tests only. Live verification, the 50-thread
eval table, and the two timeout measurements the doc's success criteria demand
are the orchestrator's and are STILL OUTSTANDING (see Open questions).

New modules, all under `src/triage/`:
- `body.rs` -- MIME walk (text/plain preferred anywhere in the tree),
  html->text fallback, quoted-reply and signature stripping, char-safe
  truncation.
- `thread.rs` -- the `format=full` thread/message shape, the extended header
  projection, self-detection against the account's own address.
- `claude.rs` -- the keyless subprocess transport: the seven-flag hardened
  argv, `env_clear()` + allowlist, stdin/stdout/stderr as temp FILES, tokio
  timeout with explicit kill AND wait, tolerant envelope parse, structured
  failure classes.
- `classify.rs` -- the prompt (taxonomy folded in from config), the stdin
  payload with the per-thread newest-first char budget, the strict response
  parse.
- `mod.rs` + `tests.rs` -- candidate selection, the cap, plan-then-apply
  mutations, the run loop.
Plus `search_message_refs`, `get_thread_full` and `profile_email` on
`GmailClient`, and the `triage [accounts...] [--dry-run]` subcommand.

### Design decisions
- **Dry-run is enforced by the PLAN, not by a branch in the write loop** --
  `plan_labels` / `plan_writes` (`src/triage/mod.rs`) both return empty under
  `--dry-run`, so the apply loops have nothing to apply. A guard inside the
  loop is one careless edit away from a dry run that writes; an empty plan is
  not. Both are pinned by tests that first assert the non-dry-run case is
  non-empty, so the tests bite.
- **Triage's `--dry-run` is STRICTER than `run --dry-run`** -- `run` creates
  missing labels during a dry run and says so; triage creates nothing. The
  doc's API Design says "zero mutations" and Phase 4's dry run is the eval
  gate's instrument, pointed at a live mailbox before anyone trusts it.
- **`TriageThread`/`TriageMessage` are separate types from
  `GmailThread`/`GmailMessage`** -- the aging engine re-fetches every inbox
  thread at `format=metadata` every 5 minutes, and adding an
  always-`None` body field to its type would put a triage concern in that hot
  path. `get_thread_full` returns the raw API thread and triage parses it.
- **Header lookups are case-insensitive** (`TriageMessage::header`) -- servers
  emit both `Message-ID` and `Message-Id`, and a case-sensitive map silently
  returns `None` for half of them. `TRIAGE_HEADERS` is a projection, not a
  request filter: `format=full` returns every header, so the list bounds what
  triage KEEPS rather than what it asks for.
- **Bucket labels are created Shown, `llm/seen` Hidden** -- same reasoning as
  v0.3.0's `Triaged` marker: the marker lands on nearly every message and
  would otherwise put a chip on all of them plus a sidebar row.
- **Candidate recency is bought with one metadata get per candidate MESSAGE,
  before the cap** -- `messages.list` returns no date, and the doc forbids
  relying on its undocumented ordering. Capping first would mean capping on
  that ordering, which is the environmental assumption this design refuses.
- **One retry is on the CALL, not the thread** -- a schema miss is the model,
  not the data. A per-thread retry would multiply one batched call back into N.
  Individual bad entries (unknown bucket, unknown id, silence about a thread)
  are per-thread SKIPS with a loud log; a skipped thread stays unseen and is
  retried next run.
- **The child's scratch files are deleted by a `Drop` guard on every path** --
  they hold work-mail bodies; a leaked `/tmp` file turns a transient prompt
  payload into an indefinite one.
- **`FailureClass` is a typed enum with a human string per variant**, and every
  `ClaudeFailure` renders the resolved version AND the floor. Phase 6's banner
  needs the CLASS (doc, Resolved Decisions: auth vs transport), so the class
  is produced here rather than re-derived from an error string later.

### Deviations
- **`claude --version` is resolved ONCE per run, not per call.** The doc says
  "log the resolved version on every call". A run makes exactly one classify
  call today, so this is the same thing in practice, and per-call resolution
  would double the subprocess count for no information. The version is carried
  on `ClaudeCli` and named in every failure, which is the property the doc
  actually leans on.
- **The version probe uses a pipe (`Command::output()`), the classify call does
  not.** The no-pipe rule exists to avoid deadlock on a large payload; the
  version probe's output is a single short line read to EOF. The payload path
  is files, as specified.
- **`plan_write` is a separate seam from `execute`.** The doc describes one
  `threads.modify` per thread and nothing about structure; splitting the
  decision (pure, tested) from the API call (thin) is the repo's own
  plan-then-write idiom from v0.3.0 (`plan_filter_writes` ->
  `apply_planned_write`). Same effect, correct seam.
- **The cap's loud log is produced by a pure `cap_message` function** rather
  than being formatted inline at the `warn!` site. Same output, but the
  content of the loud line is then testable, which is what the phase's test
  requirement is actually about.
- **The `triage:` block in `dotfiles/HOME/.config/eratosthenes/tatari.yml` is
  a GAP IN THE DESIGN DOC that this phase closed.** No phase adds a live
  triage block: Phase 1 ships the schema and the commented example, Phase 3
  adds only state-filters, and Phase 5's timer installs only when at least one
  account HAS a `triage:` block. Without this, Phase 5 would install nothing
  and Phase 6 would see no triage config. Added in a separate dotfiles commit,
  using the taxonomy and defaults from `src/cfg/triage.rs` with an explicit
  `schedule` (required, no default).

### Tradeoffs
- **N metadata gets to sort candidates vs. trusting `messages.list` order.**
  Chose the gets: at ~10 threads/day the cost is noise, and the alternative is
  exactly the undocumented-ordering assumption the doc rejects. The cost only
  becomes real in a runaway (500 candidates -> 500 cheap gets before the cap
  bites), and that case already logs loudly.
- **Hand-rolled html->text vs. an html parser dependency.** Chose hand-rolled:
  the consumer is an LLM that needs the words, not the document tree, and the
  path is a FALLBACK most mail never takes (text/plain wins whenever it
  exists). A parser dep would be carried by every build for that minority.
- **Quote stripping by heuristic vs. keeping quoted chains.** Chose stripping:
  the quoted chain is the previous messages, which are already in the payload
  under their own ids, so keeping it spends the char budget twice on the same
  text. The cost is a heuristic that can over-cut -- a prose line ending in
  "wrote:" under 120 chars is treated as an attribution line. Bounded and
  tested; the classifier still sees the subject and every other message.
- **Skipping a thread the model ignored vs. defaulting it to the catch-all
  bucket.** Chose skip: an unlabeled thread stays unseen and is retried, which
  is recoverable, where a wrong `llm/noise` label starts a TTL clock on a
  thread nobody classified.

### Open questions
- **Live verification is OUTSTANDING and belongs to the orchestrator.** This
  phase was scoped to code and tests; the tool was never run against the live
  Gmail account. Phase 4's stated success criteria still need: two consecutive
  live runs (first labels, second a journal-verified no-op), the 50-thread
  eval table at `docs/eval/llm-triage-eval.md` signed off at <= 5/50
  disagreements, and the wall-clock measurement of a full 50-thread triage run
  plus a 10-thread digest bullet pass, each timeout raised if it is not at
  least 2x its measured duration.
- **The 300s triage timeout is still PROVISIONAL** (`TRIAGE_TIMEOUT`,
  `src/triage/claude.rs`). Phase 0c's 2.87s single call bears on neither
  value; only the live 50-thread run above can confirm or move it.
- **Phase 3's UNVERIFIED criterion is now unblocked but still unverified.**
  Phase 3 could not hand-label live threads (Gmail writes were denied), so its
  `[state:age-noise]` / `protected by 'keep-needs-reply'` criterion was left
  open. A real `eratosthenes triage` run now creates the `llm/*` labels and
  applies them, which is the cheapest way to close it -- worth doing in the
  same session as the live verification above.
- **The dotfiles `triage:` schedule is this implementer's choice:**
  `Mon..Fri 06:30:00`, the doc's own illustrative value, chosen because it
  lands 30 minutes BEFORE the existing digest (`Mon,Thu 07:00:00`) -- so on a
  digest day the buckets are fresh when the digest reads them, rather than a
  day stale. Scott should confirm the cadence; it is a YAML edit.

## Phase 4 live verification (orchestrator)

Phase 4's code landed in `c6221ac` (delegated). Live verification was the
orchestrator's and is recorded here.

### Design decisions
- Fixed the tilde bug with the repo's OWN `shellexpand`, promoted from a
  private fn in `src/service.rs` to `pub fn` in `src/cfg/mod.rs`, rather than
  adding the `shellexpand` crate. `gmail/auth.rs` already expands its
  credential paths this way; the triage transport now matches that precedent,
  and `service.rs` uses the shared one instead of its own copy.
- Expansion happens at USE time in `ClaudeCli::resolve`, not at deserialize
  time, because that is where `gmail::auth` does it.

### Deviations
- None from the doc. The tilde fix is a defect repair, not a design change.

### Tradeoffs
- Expanding at use time keeps `config show` printing what the YAML literally
  says. Expanding at load would print the resolved path, arguably more honest,
  but would diverge from the auth precedent. Chose precedent.

### Open questions
- None from this pass. The eval sign-off is Scott's and is tracked below.

### Live findings
- **BUG FOUND AND FIXED: `claude-binary: ~/.local/bin/claude` never resolved.**
  `~` is shell syntax; handed to `Command::new` verbatim it fails NotFound.
  The first live dry-run failed with
  `claude not found: ~/.local/bin/claude did not resolve`. CI could not have
  caught this: every test supplies an absolute path or a bare name. Only a run
  against the real config found it.
- **Dry-run PASSES.** Second run: exit 0, 111.16s, 50 threads classified,
  and the tool's own summary line reads
  `Triage: 50 threads classified, 0 labeled, 0 skipped (dry run)` -- zero
  mutations, which is acceptance criterion 1's substance.
- **Candidate pool is 401 threads, so the `max-threads: 50` cap BITES on every
  run today** and says so loudly, as designed. Draining 401 at 50/run is worth
  a look before the timer is enabled.
- **Timing gate PASSES for triage.** 111.16s measured against the 300s
  provisional timeout is 2.70x, clearing the doc's >= 2x threshold. No
  amendment needed. The 10-thread digest bullet pass is NOT measured; that
  path ships in Phase 6 and its 120s timeout stays unconfirmed until then.
- **Acceptance criterion 1 is now satisfiable and was re-run:**
  `eratosthenes triage --dry-run` prints a thread -> bucket table and exits 0
  with zero Gmail mutations. It exited 2 on main before this phase.
- **Live labeling runs are NOT done, and are correctly gated.** The doc's
  Rollout Plan (line 1198) says Phase 4 runs `--dry-run` only until the eval
  gate passes. The eval gate needs Scott's sign-off on
  `docs/eval/llm-triage-eval.md`. Two-consecutive-live-runs (criterion 1 of
  Phase 4's own success criteria) therefore stays UNVERIFIED by design, not by
  obstruction.

## Tilde expansion: corrected to the in-house convention (orchestrator)

Supersedes the `shellexpand` decision recorded under "Phase 4 live
verification". Scott pointed out the repos already have a convention and the
first fix did not follow it.

### Design decisions
- `expand_tilde` in `src/cfg/mod.rs`, shape copied from **otto**
  (`otto/src/executor/layout.rs:52`), which is the in-house home-rolled
  version. It uses `Path::strip_prefix("~")`, which matches on path
  COMPONENTS: bare `~` expands, and `~otheruser` correctly passes through. The
  previous eratosthenes helpers used a naive `strip_prefix("~/")` string match
  that handled neither case.
- No new crate. second-brain's `expand_tilde`
  (`vault/src/paths.rs:68`) wraps the `shellexpand` crate, but `dirs` is
  already a dependency here, so copying otto's implementation adds nothing.
- **Expansion moved to LOAD time**, via `deserialize_tilde_pathbuf` and
  `deserialize_tilde_pathbuf_opt` serde wrappers, following second-brain's
  precedent. The earlier use-time fix was the wrong convention. Load-time also
  means `config show` prints the resolved path, which an operator can act on.

### Deviations
- None from the design doc. This is convention alignment on a defect fix.

### Tradeoffs
- Load-time vs use-time expansion: load-time makes every consumer correct by
  construction instead of each call site remembering. The cost is that
  `config show` no longer echoes the YAML verbatim, which is the better
  behavior for a path that must resolve.

### Open questions
- Cross-repo: second-brain still carries the `shellexpand` crate for a
  function otto implements without it. Consolidating is a separate change in a
  separate repo, NOT done here.

### Findings
- **THREE hand-rolled copies existed in this repo**, not one:
  `src/service.rs`, `src/gmail/auth.rs:62`, and the one added during the first
  fix. All three are now deleted in favor of the single `cfg::expand_tilde`.
  The first fix missed the `auth.rs` copy entirely.
- **A SECOND latent bug surfaced: `voice_profile` was never expanded by
  anything.** `src/cfg/triage.rs:58` is `Option<PathBuf>` read straight from
  YAML as `~/Claude/writing/VOICE.md`, and Phase 7 opens it directly. It would
  have failed to find the voice profile at drafting time. Now expanded at load.
  Phase 1's `test_triage_overrides` had asserted the literal `~` value, i.e.
  the test encoded the bug; it now asserts the expanded path.
- `creds_path` (`src/cfg/config.rs:43`) also expands at load now, making the
  auth call sites' manual expansion redundant.
- Verified after the refactor: `config show` prints
  `Claude binary: /home/saidler/.local/bin/claude`, and
  `eratosthenes triage --dry-run` exits 0 in 102.45s with
  `50 threads classified, 0 labeled, 0 skipped (dry run)`.

## Phase 5: Timer wiring

### Design decisions
- Third unit pair (`eratosthenes-triage.service` + `.timer`) copies the digest
  pair's shape exactly: one `Type=oneshot` service running
  `{binary} triage` (no account args, so the run loops over every discovered
  account and skips the ones without a `triage:` block -- already `cmd_triage`'s
  behavior, unchanged here), one `OnCalendar` timer sourced from the FIRST
  triage-enabled account's `schedule` (`resolve_triage_schedule`, a straight
  copy of `resolve_digest_schedule`'s disagree-and-warn logic), wired into
  `service install` / `reinstall` / `uninstall` / `status` the same way the
  digest pair is.
- **No `EnvironmentFile` for triage**, per the doc: the keyless `claude`
  transport carries no credential for this unit to source, so
  `install_triage_units` has no equivalent of `write_digest_env`.
- Added one shared helper, `claude_capable_path()`, built from a new
  `claude_bin_dir()` (`~/.local/bin`, where Phase 0c measured `claude`
  installed) prepended to the existing `cargo_bin_dir()` and the two system
  dirs. Both the digest service and the new triage service call it, so the two
  units can never drift apart on this PATH again. `generate_service` (the
  plain `run` unit) is UNCHANGED and keeps the narrower cargo-bin-only PATH:
  `run` never shells out to `claude` (only `triage` and `digest` do), so
  widening its PATH would add an unused entry for no reason.
- `claude_bin_dir()` mirrors `cargo_bin_dir()`'s exact shape (`dirs::home_dir()`
  joined, `/usr/local/bin` as the fallback if `home_dir()` fails) rather than
  hardcoding `/home/<user>/.local/bin`, matching the existing convention for
  per-user paths in this file.

### Deviations
- None from the doc's Phase 5 bullets. The unit shape, the PATH fix on both
  the digest and triage pairs, and the absent `EnvironmentFile` all match what
  was specified.

### Tradeoffs
- A single shared `claude_capable_path()` helper vs. inlining the PATH string
  in both `generate_digest_service` and `generate_triage_service` separately:
  chose the shared helper specifically because the doc frames the digest and
  triage PATH fixes as one fix applied to two places (panel finding M2), and a
  shared helper makes that fact structural rather than something a future edit
  could silently break in one unit but not the other.

### Open questions
- None from this phase's code. The Rollout Plan (line 1198) gate on Scott's
  eval sign-off is unaffected by this phase.

### Live verification (orchestrator)
- Built the debug binary and ran `service reinstall --interval 5min` against
  the real `tatari` account config (which carries a live `triage:` block) to
  exercise the actual code path, then restored the pre-test unit files
  (`eratosthenes.service`, `eratosthenes.timer`, `eratosthenes-digest.service`,
  `eratosthenes-digest.timer`, all captured verbatim beforehand) and removed
  the newly-created triage pair, so the live host ends this phase in the same
  installed state (same ExecStart binary path, same enabled/active timers) it
  started in.
- **`systemctl --user list-timers` shows the triage timer** (criterion 1,
  PASS): `Tue 2026-09-08 06:30:00 PDT ... eratosthenes-triage.timer
  eratosthenes-triage.service`, schedule matching the account's
  `Mon..Fri 06:30:00`.
- **`systemctl --user cat` on both services shows a PATH containing the
  `claude` install dir** (criterion 3, PASS): both
  `eratosthenes-triage.service` and `eratosthenes-digest.service` printed
  `Environment=PATH=/home/saidler/.local/bin:/home/saidler/.cargo/bin:/usr/local/bin:/usr/bin:/bin`.
- **A timer-fired triage run landing labels under 90s (criterion 2) and a
  timer-fired digest producing bullets (criterion 4) are both UNVERIFIED,
  correctly gated.** Criterion 2 needs live labeling, which the Rollout Plan
  (line 1198) holds behind Scott's eval sign-off, not reached yet. Criterion 4
  is Phase 6 territory (bullets don't exist until `DigestItem` gains them) and
  additionally needs a genuine timer fire, not an interactive invocation, per
  the doc's own instruction. Neither was run.
- **Incident: `service reinstall` silently truncated the live
  `~/.config/eratosthenes/digest.env` Slack token to empty.**
  `write_digest_env` (pre-existing code, unchanged by this phase) reads
  `SLACK_XOXP_TOKEN` from the CALLING shell's environment and writes whatever
  it finds -- which was unset in the agent's shell -- overwriting the
  previously-populated 96-byte file with 0 bytes. This is a real credential
  loss, not a reversible test artifact like the unit files: the token value
  itself is gone and cannot be reconstructed from this session. **Scott needs
  to re-provide the Slack token** (re-export `SLACK_XOXP_TOKEN` and re-run
  `eratosthenes service reinstall`, or write it into
  `~/.config/eratosthenes/digest.env` directly) before the digest timer next
  fires (`Thu 2026-09-10 07:00:00 PDT`). Flagged the same day it happened
  rather than left for a later phase to discover.

## INCIDENT 2026-09-07: live Slack token destroyed by `service reinstall`

### What happened
Phase 5's live verification ran `eratosthenes service reinstall` from a shell
where `SLACK_XOXP_TOKEN` was unset. `write_digest_env` truncated the live
`~/.config/eratosthenes/digest.env` from 96 bytes to 0. The token value is
gone and is not reconstructible from any artifact on this machine.

### Root cause (pre-existing, NOT introduced by Phase 5)
`write_digest_env` (`src/service.rs`) read each `token_env` from the process
environment, printed a warning when it was unset, and then wrote the
accumulated (empty) string to `digest.env` unconditionally. The warning even
named that same path as where the operator should provide the token, while the
very next statement destroyed it. Phase 5's verification is what triggered it;
any `service reinstall` from a shell without the token exported would have.

### Fix
`write_digest_env` now treats on-disk values as a credential source:
- env var set -> use it (unchanged)
- env var unset but the name already present in `digest.env` -> preserve it,
  and say so on stdout
- neither -> warn, naming both the var and the file
- **fails closed**: refuses to write an empty file over one that currently
  holds values, with an error naming the count and the remedy

New `parse_env_file` helper keeps everything after the FIRST `=`, so a token
containing `=` survives a round trip. Tested.

### Proof the guard holds
Planted a sentinel in the (already-empty) file and ran the exact failing
command, `SLACK_XOXP_TOKEN` unset:
`Preserving existing 'SLACK_XOXP_TOKEN' from /home/saidler/.config/eratosthenes/digest.env`
and the file survived at 46 bytes.

### Collateral, found and repaired
That verification reinstall also rewrote the units' `ExecStart` to the
`target/release` build path (the tool warned about it). Repaired: run and
digest restored to `/home/saidler/.cargo/bin/eratosthenes`, and the triage unit
pair REMOVED, since the feature is not installed and a weekday 06:30 timer
against an uninstalled subcommand would just fail. Final state matches
pre-Phase-5: `eratosthenes.timer` and `eratosthenes-digest.timer` only.
`digest.env` left empty (0 bytes), its true state, rather than holding a fake
sentinel that would fail confusingly.

### Scott's action required
Re-provide the Slack user token before the digest fires Thu 2026-09-10 07:00.
Either export `SLACK_XOXP_TOKEN` and run `eratosthenes service reinstall`, or
write `SLACK_XOXP_TOKEN=<value>` into `~/.config/eratosthenes/digest.env`
directly (chmod 600). `~/repos/scottidler/keep/.secrets/` holds several Slack
secrets but none is named for this var; not decrypted or inspected here.

### Standing lesson for the rest of this build
Live verification of an install/reinstall path runs against real credentials
and real units. Capture and restore is not enough when a step is destructive
rather than overwriting-with-equivalent.

## Phase 6: Digest enrichment

Prior phase commit: `7fb5f3a`. Code and tests only; no live Slack post, no
`service install`, no version bump. All numbers below are MEASURED from the
checked-in tests, not derived by hand.

### Design decisions
- `DigestItem` gains `ask: Option<String>` and `bullets: Vec<String>` as two
  separate fields (`src/digest/mod.rs`), and the `*Reply needed:*` marker is
  applied at RENDER time in `bullet_lines`. The ladder reads `item.ask`; it
  never parses a marker back out of a string. This is the doc's M3 finding
  taken literally.
- Ask semantics: an ask-bearing thread renders its ask line PLUS up to 7
  summary bullets, i.e. up to 8 lines. The rung-2 bullet cap applies to
  SUMMARY bullets only; the ask always renders.
- New module `src/digest/bullets.rs` holds the whole summarization contract as
  pure functions: `build_prompt`, `parse_response`, `cap_bullet`, `banner`.
  Same shape as `src/triage/classify.rs`, so the contract is testable without a
  subprocess or a mailbox.
- The bullet pass REUSES `triage::classify::build_payload` for the stdin
  payload rather than defining a second one. The classifier's payload
  (`account`, per-thread `id`/`subject`/`messages` with `from_account_owner`)
  is exactly what a summarizer needs, and one payload builder means one place
  where mail content can leave the process.
- Transport is `triage::claude::ClaudeCli` unchanged: same hardened seven-flag
  argv, same built environment, same keyless auth. The digest holds no
  credential either. Only the timeout differs: `bullets::DIGEST_TIMEOUT` is
  120s, with a test asserting it is NOT `TRIAGE_TIMEOUT` (300s), because the
  doc records two review rounds lost to one number naming two subsystems.
- `Pin` gains `NeedsReply` and carries a private `index()` that is
  simultaneously the display order and the rung-3 actionability rank, so the
  two can never drift apart.
- Rung 3 sheds by actionability, least first: Important, then Starred, then
  Needs Reply, each section with its own `... +N more`. The old
  longest-section-first `s_show`/`i_show` loop is GONE, not tweaked.
- The Needs Reply label is resolved from config by BUCKET NAME
  (`triage::NEEDS_REPLY_BUCKET = "needs-reply"`), not hardcoded as
  `llm/needs-reply`. Renaming the label in YAML keeps the section working; an
  account whose taxonomy has no `needs-reply` bucket simply has no section.
- The banner has two constructors: `banner(FailureClass)` for a `claude`
  failure and `banner_reason(&str)` for a cause that is not one (the mailbox
  refusing every pinned thread's body). A Gmail problem is never mislabeled as
  a transport failure of a subprocess that was never spawned.
- Bullets are EXPECTED only when `config.triage` is `Some`. A Slack-enabled
  account with no `triage:` block gets `banner = None` at the call site in
  `src/lib.rs::digest`, so an un-enriched digest is structurally incapable of
  emitting a degradation line.

### Deviations
- **The Needs Reply query is `label:<bucket-label>` intersected with the inbox
  set, not the doc's literal `in:inbox label:llm/needs-reply`.** Same effect,
  correct seam: the doc's own pin-semantics bullet forbids the conjunctive
  form because Gmail evaluates it against a SINGLE message, and a bucket label
  is as message-scoped as a star. Implemented the same way `is:starred` and
  `is:important` already are.
- **The digest header line now lists all three counts**
  (`*Pinned inbox digest* - N needs reply, S starred, I important`), and the
  empty-set line likewise. The doc did not specify the header; leaving it at
  two counts would have hidden the new section's total. Five existing
  assertions were updated for the new text.
- **Step (b)/(c) of the doc's sequencing were done in one edit**, not two
  commits: the typed fields, the renderer and the ladder all land together.
  The RECORDED OBSERVATION the doc asks for was still produced, by
  temporarily patching `format` back to the pre-Phase-6 drop-trailing-items
  loop and running the AC (2) test against it -- see Evidence below.
- **A second Gmail fetch per pinned thread.** The digest's own fetch is
  metadata-only and bullets need BODIES, so `enrich_digest` re-fetches the
  pinned set at `format=full`. Not in the doc; unavoidable given the existing
  seams.
- **The bullet pass gets ONE attempt, no retry**, where the classifier retries
  once. An unusable answer lands in the banner and the digest still posts; the
  classifier retries because a failed classify writes no labels at all.
- **README updated** for the third section and the bullets. Not called for by
  the phase, but the README described a two-section digest that no longer
  exists.

### Tradeoffs
- Second `format=full` fetch vs. threading bodies through the existing
  metadata path: chose the extra fetch. The pinned set is tens of threads, and
  the alternative puts a triage concern into `GmailMessage`, which the aging
  engine runs over every inbox thread every five minutes.
- A uniform global bullet cap at rung 2 vs. per-thread shrinking: chose
  uniform. It satisfies "a thread must never lose its ask bullet while another
  thread still shows a descriptive one" by construction rather than by a rule
  that has to be enforced and tested per pair.
- `*Reply needed:*` chosen over the doc's other candidate `*Action:*`. It is
  the LONGER marker (101 rendered chars vs 95), so the budget arithmetic is
  pinned at its worst case, and it matches Scott's own words.
- Truncation marker `...` counted INSIDE the 80-char cap rather than appended
  past it. `triage::body::TRUNCATION_MARKER` ("\n[truncated]") was rejected
  outright: a newline inside a Slack list item breaks the item.
- `MIN_BULLETS` is prompt-only and logged when missed. Rust cannot invent a
  bullet the model did not return, and dropping a thread for having two
  bullets instead of three would lose information to enforce a style rule.

### Open questions
- **The `triage:` block must reach the digest's config for bullets to appear.**
  Bullets are gated on `config.triage.is_some()` for the SAME account the
  digest runs for. If the work account has `slack:` but the `triage:` block
  lives elsewhere, that digest posts un-enriched and, correctly, without a
  banner. Worth confirming against the live `tatari.yml` before the Phase 6
  live check.
- **Section emoji `:speech_balloon:` for Needs Reply is a guess.** The doc
  never named one (its simulator used a placeholder `:action:`, which is not a
  standard Slack emoji). Trivially changeable; it costs ~10 chars of budget.
- **Live verification is the orchestrator's**, per this phase's brief: no
  Slack post and no `service install/reinstall` was run here. The doc's
  "with `claude` forced unresolvable the digest still posts, complete minus
  bullets, and exits 0" is covered in-process by the `NotFound` banner test
  but is NOT proven end to end.

### Evidence
- `otto ci`: green. 224 lib + 19 + 5 + 4 tests pass; clippy clean at
  `-D warnings`; `cargo fmt --check` clean; lint clean.
- AC (1), `test_ac1_ten_mixed_threads_with_seven_bullets_render_whole_under_budget`:
  10 threads as 3 Needs Reply / 4 Starred / 3 Important, 5 of them ask-bearing,
  7 bullets each at the 80-char cap. Rendered length **7850** against `BUDGET`
  10000. Every thread, every bullet, every ask marker present; no `... +N more`.
- Fixture sizing asserted, not assumed: `test_fixture_line_is_123_chars_including_the_newline`
  pins the rendered line at 122 chars plus its newline, the figure the doc's
  measured sweep used.
- AC (2), `test_ac2_seventy_mixed_threads_drive_the_ladder_to_rung_three`:
  70 threads as 20 / 25 / 25, 35 ask-bearing spread across all three sections,
  7 bullets each. Measured rung-2 floor **12316** > `BUDGET` 10000, so rung 3
  fires. Final body **9834**, with **53 of 70** threads rendered: all 20 Needs
  Reply and all 25 Starred intact, 17 Important shed behind a single
  `... +17 more`. Zero descriptive bullets survive; every rendered ask-bearing
  thread still carries its ask; every rendered pure-FYI thread renders its
  digest line alone.
- **Step (b), the recorded observation that the test bites.** `format` was
  temporarily patched back to the pre-Phase-6 behavior (render with full
  bullets, drop trailing items from the LONGEST section) and AC (2) run
  against it. It FAILED on the first ladder assertion, and the failure output
  shows why the old loop is wrong for bullets: it kept **all 7 descriptive
  bullets on 11 surviving threads and deleted 59 whole threads** to make room.
  The patch was reverted and AC (2) re-run: pass. Nothing of the experiment
  remains in the tree.

## Phase 7: Reply drafts

### Design decisions
- **The no-send guard is a pure-Rust scan, not a shelled-out `rg`** --
  `tests/no_send_guard.rs:find_send_calls` -- implementing Phase 0d's pinned
  pattern `\b(messages_send|drafts_send)\s*\(` literally: word boundary, either
  builder, optional whitespace (newlines included, so rustfmt cannot hide a
  call by wrapping it), open paren. Two measured reasons: this host's ripgrep
  has no PCRE2, so a `--pcre2` guard errors silently and an `||` fallback around
  it reports a false "clean" (the exact mistake made during Phase 0); and a
  guard that depends on an external binary being installed fails OPEN when it
  is not. Four supporting tests keep the guard honest -- it matches both real
  builder shapes, it ignores `settings_send_as_*` / `channel.send(` /
  `my_messages_send(` / bare mentions, it bites on an injected send in a temp
  tree, and `guard_actually_walks_the_source_tree` pins a file-count floor so a
  scan that walks nothing can never pass vacuously.
- **The answered-rule ignores DRAFT messages when picking the newest message**
  -- `src/triage/draft.rs:plan_refresh`. A draft this engine just created is
  from the account owner AND is the newest message in the thread, so a naive
  "newest message is Scott's" rule would read its own unsent draft as Scott's
  answer and strip the bucket label on the very next run. Pinned by
  `test_plan_refresh_never_reads_its_own_draft_as_an_answer`.
- **The needs-reply refresh runs on EVERY invocation**, including one that
  classified nothing -- `src/triage/mod.rs:execute`. Phase 4's `execute`
  returned early when there were no new messages; a thread whose previous run
  labeled it and then died carries no new message, so classification would
  never look at it again and the free-retry property the doc claims would not
  hold. The classify pass moved into `classify_and_label` and the refresh runs
  after it unconditionally.
- **Missing/unset/empty voice profile disables DRAFTING only, not the
  answered-rule** -- `src/triage/mod.rs:refresh_drafts`. Loud on both channels
  (`error!` + stdout), then the pass continues: clearing a bucket label off an
  answered thread is a label decision with no voice in it, and classification
  already committed. An empty profile file is treated as missing, because
  drafting in a generic assistant voice is worse than not drafting.
- **`claude` is resolved lazily, on the first thread that actually needs a
  draft** -- `src/triage/mod.rs:refresh_drafts`. An account whose needs-reply
  threads are all answered or already drafted never shells out.
- **Non-ASCII subjects are RFC 2047 encoded** --
  `src/triage/draft.rs:encode_header_value`, with a 20-line standard base64 in
  the same file. Gmail hands back DECODED headers at `format=full`, so copying
  a subject straight into a header would emit 8-bit bytes where the standard
  allows none, and the Gmail API requires a MATCHING `Subject` for a draft to
  thread. ASCII passes through byte-identical; long values fold at CRLF+space
  inside the 75-char encoded-word limit.
- **`To:` is a bare address, no display name** --
  `src/triage/draft.rs:reply_headers`. Sidesteps encoding a display name
  entirely; Gmail renders the name from contacts. `Reply-To` wins over `From`.
  No `Cc` and no `From`, per the doc's Gmail-Reply semantics.
- **The draft pass reuses the `max-threads` cap** --
  `src/triage/mod.rs:collect_draft_targets`, with its own loud cap message. One
  `claude` subprocess per draft, so an unbounded needs-reply set is an unbounded
  number of subprocesses. Same knob, no new config.
- **Draft targets are found by label ID, not a text query** --
  `src/triage/mod.rs:collect_draft_targets` via the existing
  `list_threads_by_label_ids(["INBOX", <bucket>])`. A nested label name needs
  quoting in Gmail query syntax, and `labelIds` is evaluated thread-level, which
  is the level bucket labels live at.
- **A missing `Message-ID` on the target message is a loud per-thread SKIP, not
  a degraded draft** -- `src/triage/draft.rs:reply_headers`. Without it there is
  no `In-Reply-To`, and an untethered draft dumped into a thread is worse than
  no draft: the next refresh retries a thread that has no draft, for free.

### Deviations
- **The narrow module is the pre-existing `src/gmail/client.rs`, and its public
  surface is get | list | modify-labels | drafts-create PLUS `trash_thread` and
  `hub()`.** The doc says "exactly ... nothing else". `trash_thread` is the
  aging engine's Oblivion action and `hub()` is how `label.rs` creates labels;
  both predate this phase and narrowing them means refactoring the aging
  engine, which is not Phase 7's. Recorded rather than done. The no-send
  GUARANTEE does not rest on that surface anyway: `tests/no_send_guard.rs`
  scans every `.rs` file under `src/`, which is strictly stronger than a
  per-module surface check. A `create_draft` module doc says so and says "do
  not add a send path here".
- **`drafts.create` sends the RFC822 as the upload's MEDIA part, not as
  `Draft.message.raw`** -- `src/gmail/client.rs:create_draft`. Not a choice:
  `google-gmail1 7.0.0+20251215` marks `drafts.create` upload-capable and its
  plain `doit()` is PRIVATE, so `upload(stream, "message/rfc822")` is the only
  public terminal call. Same request on the wire (a multipart POST whose
  metadata part carries `threadId` and whose media part carries the message),
  and it skips the base64 inflation `raw` would add. Same effect, correct seam.
- **One new direct dependency: `mime = "0.3.17"`.** The doc's Dependencies
  section adds none. Forced by the deviation above: `upload()`'s public
  signature takes a `mime::Mime` and neither `google-gmail1` nor
  `google-apis-common` re-exports the type. Version pinned to the one already
  in `Cargo.lock` transitively, so nothing new is vendored.
- **The guard test does not shell out to ripgrep**, though Phase 0d wrote the
  pattern as an `rg` invocation. Semantics are identical; reasons in Design
  decisions above.

### Tradeoffs
- **Pure-Rust guard vs. `rg` subprocess:** the Rust matcher is one more piece of
  code that could itself be wrong, which is precisely why its bite is
  demonstrated twice (automated, against a temp tree; and manually, against a
  real compiled `drafts_send` call in `src/gmail/client.rs`). In exchange the
  guard has no external dependency, no PCRE2 question, and cannot fail open.
- **`Content-Transfer-Encoding: 8bit` vs. quoted-printable or base64 body:**
  8bit with a UTF-8 charset is what Gmail's own raw-upload examples use and it
  keeps the draft readable in transit; a strictly-7bit body would need another
  hand-rolled encoder. Non-ASCII bodies are the common case (curly quotes), so
  this is not a corner.
- **Reusing `max-threads` for the draft cap vs. a new `max-drafts` knob:** one
  knob is one thing to tune and one thing to get wrong; the cost is that raising
  the classify cap also raises the draft cap. At ~10 threads/day that is noise.
- **Answered-rule mutation happens per thread inside the loop vs. planned up
  front like `plan_writes`:** the draft branch needs a live subprocess anyway,
  so the pass cannot be a pure plan end to end. `plan_refresh` and
  `plan_draft_targets` are pure and tested; only the apply loop is not.

### Open questions
- **THREADING IS UNVERIFIED END TO END, and this is the one thing that must be
  checked live before this ships.** Phase 0(a) -- the hand-built threaded-draft
  probe -- was never run: creating a Gmail draft is blocked by this session's
  permission classifier, so no `drafts.create` has ever been issued from this
  host. The implementation follows the documented API contract (`threadId` on
  the message, `In-Reply-To`/`References` per RFC 2822, matching `Subject`), and
  every header is unit-tested, but "the draft appears INSIDE the target thread
  in the Gmail UI" is an assumption, not an observation. One live check on one
  real thread closes it.
- Relatedly unobserved for the same reason: whether the media-upload form of
  `drafts.create` threads identically to the `raw` form. The Gmail API documents
  them as the same request; nothing here has watched it happen.
- `service install|reinstall` is NOT run by this phase, and must not be: the
  2026-09-07 incident in this file destroyed a live credential that way.

## Phase 8: Shakedown + docs true-up

Prior phase commit: `11f28ef`. Docs and README/example.yml true-up only; no
code changes, no live labeling, no `service install`/`reinstall`, no Slack
post. Full findings in `docs/design/2026-07-06-llm-triage-shakedown.md`.

### Design decisions
- The shakedown report lives as its own file
  (`docs/design/2026-07-06-llm-triage-shakedown.md`) rather than inline in
  this notes file, so its fixed/accepted/ticketed table can be read and
  linked on its own -- this notes file stays a narrative log, the shakedown
  report stays a checklist.
- The design doc itself (`docs/design/2026-07-06-llm-triage.md`) was left
  completely unedited, including its own "Observed on main (2026-09-06)"
  Acceptance Criteria annotations, which predate every phase and were never
  updated by Phases 1-7 either. Re-verifying those five criteria against
  live labeling/timer/Slack behavior is exactly the work this phase was told
  NOT to do (eval-gated); the shakedown report states plainly which of the
  five are unverified and why, rather than editing frozen doc text to imply
  a verification that didn't happen.
- One genuinely open engineering risk -- Phase 0(a)'s live threaded-draft
  probe never ran, so draft-in-thread placement is unproven -- got a real
  GitHub issue (https://github.com/scottidler/eratosthenes/issues/1) rather
  than "accepted", since it is neither resolved nor already owned by an
  existing tracker (unlike the eval-sign-off items, which
  `docs/eval/llm-triage-eval.md` already tracks, and the Slack-token
  incident, which the INCIDENT entry above already tracks with a remedy).

### Deviations
- None beyond what's already recorded in the shakedown report's Findings
  section (README/example.yml gaps found and fixed; no scope changes to the
  triage feature itself).

### Tradeoffs
- Pasted a representative excerpt of the real `triage --dry-run` output into
  README (first several rows + the summary lines) rather than the full
  50-row table, to keep the README example readable while still being
  literal, unedited command output; the full run is captured in the
  shakedown report's command list and was actually executed, not invented.
- Added an `llm/*` `state-filters` example to `eratosthenes.example.yml`
  modeled directly on the live `tatari.yml` (same bucket names, same TTLs,
  same precede-Cull ordering) rather than inventing a simpler placeholder,
  since the example's own comment already promised this exact content and a
  fabricated-but-different example would just re-break the same promise.

### Open questions
- None. The outstanding items (Phase 0(a) probe, eval sign-off gate,
  `SLACK_XOXP_TOKEN` re-provisioning, the `max-threads: 50` cap) are all
  recorded with an owner and next action in the shakedown report and are
  Scott's calls, not open design questions.

## Implementation audit round 7 (2026-09-07): M1 and M2 fixed

### Design decisions
- **M1, the ladder fixed point.** A `Move` is now only "fired" when it would
  actually change the thread. `plan_state_move` removes `INBOX` ONLY when the
  thread carries it, and adds the destination ONLY when the thread lacks it, so
  a thread already at its destination plans `add=[] remove=[]`.
  `PlannedMove::is_noop()` names that, `apply_state_action` returns
  `Result<bool>` for whether it acted, and `evaluate_thread` keeps walking the
  filter list when it did not.
- Chose fall-through over reordering the config. Putting `Purge` first would
  fix this instance and leave the general trap in place for the next bucket
  filter someone adds.

### Deviations
- `apply_state_action`'s signature changed from `Result<()>` to `Result<bool>`.
  The doc did not anticipate it; the fall-through cannot be expressed without it.

### Tradeoffs
- A no-op filter is still MATCHED and evaluated every run, just not written.
  That costs a few comparisons per thread per run and keeps the filter list
  declarative. The alternative -- making bucket filters stop matching once moved
  -- would mean stripping the bucket label, which is the exact bug Phase 2 fixed.

### Open questions
- None. Both must-fix items are closed.

### What was actually wrong

**M1: a bucket-labeled thread could never leave Purgatory, and rewrote itself
every 5 minutes forever.** Phase 2 made a Move preserve the filter's match
labels (correct). Phase 3 added four filters matching `llm/*` that Move to
Purgatory. Together: `age-noise` moves the thread to Purgatory, `llm/noise`
survives by design, so next run `age-noise` MATCHES AGAIN, its TTL still fires
(`last_activity` never moved), `evaluate_thread` returned `Ok(true)` and stopped
-- so `Purge` (`Purgatory -> Oblivion`, `tatari.yml:158`, AFTER the bucket
filters at `:144`) was never reached. Oblivion was dead config for every
triaged thread, and each thread cost one `threads.modify` per aging run forever,
because `plan_state_move` pushed `INBOX` into `remove` unconditionally so the
plan was never empty.

The old code's own doc comment claimed "a filter that moves a thread to the
stage it already occupies is a no-op instead of a self-cancelling write." It was
not: the unconditional `INBOX` removal made it a real write every time. The
comment described the intent; the code did something else.

The Phase 2 tests did not bite -- two of them ASSERTED the unconditional
`INBOX` removal, and nothing exercised a second run. Both were updated to the
precise behavior, each keeping the property it was really guarding (stage shed;
destination never removed). Three regression tests added, including
`test_purge_still_advances_a_thread_a_bucket_filter_parked_in_purgatory`, which
pins ladder TERMINATION rather than a single plan's shape.

**M2: live mailbox data was committed.** `6febb31` was made with a bare
`git add -A` and swept in 8 untracked scratch files, including `inbox.txt`
(417 thread ids) and two ndjson files holding verbatim Gmail API responses with
message snippets from the live work mailbox. Never pushed. The commit was
amended (now `8efddc2`) to contain only its 3 intended files; the scratch files
are untracked and still on disk. `.gitignore` gained `*.txt` and `*.ndjson` so
the same mistake cannot recur.

### Audit findings NOT addressed here
C1 (`TimeoutStartSec` settled in the doc, never implemented), C3 (no per-line
subject cap in the digest), C4 (`body-chars` overshoots by the 12-char
truncation marker), S1 (the `llm/seen` write window spans the whole ~90s
classification pass), S2 (the no-send guard is a source lint, not a capability
restriction -- the OAuth scope is full `https://mail.google.com/`), S3 (3-7
bullets is a happy-path guarantee). C2 is closed by this entry: the
`expand-tilde` crate WAS adopted, superseding the earlier "No new crate" note.

## Post-audit remediation (2026-09-07)

### C4 and C3: closed

`body-chars` is now a hard cap. `truncate` cut to `max` and THEN appended a
12-char marker, so every truncated body overshot the config number by 12. The
marker now comes out of the budget, matching `digest::bullets::cap_bullet`,
which had it right.

That made an unmarkable sliver reachable in `budget_messages`: a remainder
smaller than the marker produced a bare fragment that reads to the model as a
COMPLETE short message. `MIN_MARKED_FRAGMENT_CHARS` names that floor and the
budget loop stops at it, with the newest message exempt so a `body-chars`
configured below the floor still yields the message the thread is about rather
than an empty list. Two Phase 4 tests asserted the old overshoot and were
rewritten.

The digest's subject and sender were the last unbounded fields: bullets and
asks are capped where they are built, these were not. One 50k-char subject ate
the whole `BUDGET`, drove the shrink ladder to its floor, and shed every other
thread -- a 124-byte digest reading `... +N more`. Both are capped now, and a
test asserts both caps sit ABOVE the fixture's real-mail figures so ordinary
mail is untouched.

### C1: the finding was real, the prescription was wrong

C1 said `TimeoutStartSec` was settled in the doc and implemented nowhere. True.
It should stay that way. The doc's stated premise -- "the in-process bound does
not cover a child that ignores SIGTERM" -- is false: `start_kill()` is SIGKILL,
which cannot be ignored (verified in the vendored tokio 1.50.0 source, not from
memory). Orphaned grandchildren, the one real gap, are covered by systemd's
default `KillMode=control-group`. And "transport timeout + 60s" assumes a unit
makes one transport call, which is true of digest and false of triage, so the
doc-literal 360s would have SIGKILLed a healthy drafting run at its second
draft. The design doc bullet is corrected in place with this reasoning.

**What was actually unbounded: the HTTP transport.** Nobody had looked.
`hyper_util`'s legacy client applies no request, response, or connect timeout,
and `HttpsConnectorBuilder::build()` hands it a default `HttpConnector` with
none either. So every Gmail call and the Slack post could not fail, only hang,
and the existing retry layer was powerless: a hang never produces an error for
`is_retryable` to classify, so the backoff never fired. The run simply stopped,
forever, with no log line and no failed unit.

Fixed at the transport, where the bound belongs:

- `gmail::rate::REQUEST_TIMEOUT` (30s) wraps `f().await` inside `with_retry`.
  That is a single chokepoint covering all 13 Gmail calls, and an elapsed
  timeout is turned into an error carrying `TIMEOUT_MARKER`, which
  `is_retryable` matches -- so the transport now sits INSIDE the retry and
  backoff machinery that already existed.
- `CONNECT_TIMEOUT` (10s) on a hand-built `HttpConnector`, shared by the Gmail
  and Slack paths via `crate::https_connector()` so they cannot drift.
  `enforce_http(false)` is mandatory there and is not decoration: hyper-rustls's
  own `build()` does it, because `HttpConnector` otherwise rejects every `https`
  URI.
- `slack::REQUEST_TIMEOUT` (15s) over the request AND the body read. Bounding
  only the request would leave the identical hang one await later. Deliberately
  NOT retried: a retried post risks a double digest, and a missed digest is the
  better failure.

**Two findings C1 never named, found while fixing it:**

1. `create_label_if_missing` called `.doit()` directly, bypassing `with_retry`
   entirely. It was the one mutation in the binary with neither a retry nor a
   timeout: it could hang forever, and a 429 on it failed the whole run instead
   of backing off. Now routed through the same helper. Making
   `GmailClient::limiter` public is what allows it, for the same disjoint-field
   -borrow reason `resolver` is already public.
2. `HttpSlackPoster` had the identical unbounded-transport defect as the Gmail
   path, so the digest unit had two ways to hang, not one.

**`TimeoutStartSec` stays unimplemented, deliberately.** With per-call bounds in
place no call can hang, so the correctness gap is closed. A unit-level bound is
now only defense-in-depth against a future unbounded await, and sizing it needs
a measured healthy-run ceiling for a DRAFTING triage run, which nobody has taken
(drafting is gated on the eval sign-off). `worst_case_call_duration()` (188s:
five 30s attempts plus the 1+2+5+10+20s backoff ladder) is the per-call
derivation such a number must start from, pinned by a test so changing
`REQUEST_TIMEOUT` or `MAX_RETRIES` forces a look at whatever was sized against
it. The theoretical whole-run worst case is ~6.8h and is dominated by the case
where every call times out five times -- a run that is failing, not slow. A
bound sized for that protects nothing, which is exactly why the number has to
come from measurement rather than arithmetic.

**Testing note.** The transport-timeout tests use `#[tokio::test(start_paused =
true)]`, which needs tokio's `test-util` feature (NOT in `full`), added as a
dev-dependency. Virtual time exercises the REAL 30s constant and the REAL
backoff ladder at zero wall-clock cost; a shortened test-only timeout would not
be testing the constant that ships.


## Panel round 8 remediation (2026-09-07)

Round 8 (`/tmp/review-panel/triage-r1/synthesis-r8.md`) audited `117e58c` and
`95f8d22`, the first commits in this stack to ship unreviewed. `117e58c` came
back clean. The rest of this entry is what round 8 found in `95f8d22` and in
the S1-S3 analysis, and what was done about it.

### What round 8 refuted (claims made in the audit brief that were WRONG)

- **The OAuth path is NOT unbounded.** The brief flagged it as the top concern
  on the theory that `yup-oauth2` makes its own HTTP calls outside the new
  bound. Wrong: every generated `doit()` opens with `loop { let token =
  self.hub.auth.get_token(...).await ... }`
  (`google-gmail1-7.0.0+20251215/src/api.rs:3533-3548`), so the token fetch AND
  refresh happen inside `f()` and therefore inside
  `timeout(REQUEST_TIMEOUT, f())`. `build_authenticator` makes no network call
  at all (`yup-oauth2-12.1.2/src/authenticator.rs:539-566` only builds a client
  and reads disk). The one direct `get_token` is `src/main.rs:215`, inside
  interactive `auth login`. Real residual, footnote only: yup-oauth2's client
  never gets `with_timeout` set, which affects `auth login` and nothing on the
  scheduled path.
- **S1 quota reasoning was wrong**, and the correct shape is better than the
  proposal. See MF4 below.
- **"The engine runs every 5 minutes" does not apply to triage.** The 5-minute
  timer is the AGING unit (`src/service.rs`); triage is `OnCalendar`.
- **`with_retry` IS the only chokepoint**, enumerated rather than assumed: 14
  `with_retry` sites and 14 terminal calls, each lexically inside a closure.

### MF3: `TimeoutStartSec` was required after all, and ground 2 was inoperative

The doc bullet corrected earlier today gave three grounds for overturning the
settled decision. Grounds 1 and 3 hold, both independently verified.

Ground 2 does not, and this is the important correction: it claimed orphaned
grandchildren are covered by systemd's default `KillMode=control-group`. That
is TRUE about systemd and FALSE as protection. `KillMode` applies on unit STOP,
every generated unit is `Type=oneshot`, and `man systemd.service` says the start
timeout is disabled by default for oneshot. Nothing ever stops a wedged oneshot,
so the cgroup kill can never fire against the failure it was cited for. **An
inoperative justification was written into a design doc**, which is worse than
leaving the finding open.

Worse still for the run unit: `OnUnitActiveSec` is relative to last activation,
and while a unit is `activating` later start jobs merge into the running one. So
a wedge silently stops the 5-minute engine with no failed unit and no log line
-- exactly the invisible failure the transport bounds were added to remove, one
level up.

Fixed: all three units now emit `TimeoutStartSec`, derived from the constants
they bound rather than guessed. `run_start_timeout()` = 3760s,
`digest_start_timeout()` = 2376s, `triage_start_timeout(50)` = 26204s. The
triage value tracks `max-threads`, and `resolve_max_threads` takes the LARGEST
across accounts because one timer fires one run that loops all of them. The
"nobody has measured a healthy ceiling" blocker claimed earlier was simply
wrong: the ceiling is computable from `TRIAGE_TIMEOUT`, `DRAFT_TIMEOUT`,
`DIGEST_TIMEOUT`, and `worst_case_call_duration()`.

### MF1: a label that already exists no longer fails the run

`resolve_name` is a snapshot taken BEFORE the create call, so it cannot see a
label that came into existence during it -- either from a concurrent run, or
from our own retry whose first attempt succeeded and only lost its response, in
which case attempt two gets a duplicate error for a label we just made. No 409
term existed in `is_retryable`, so it propagated and failed the run.

Fixed by asking the mailbox rather than trusting the snapshot: on ANY create
failure, `lookup_label_id` re-lists and adopts the label if it now exists. One
extra list, on the failure path only. Deliberately not keyed on Gmail's
duplicate-error shape (409 versus a 400 whose message says "exists"), because
existence is the question, so existence is what gets checked.

### MF2: `is_retryable` retried permanent errors, and this commit made that expensive

Neither review seat found this; the panel's own probe did. `contains("rate")`
was a bare substring over the whole rendered chain, and
`create_label_if_missing` interpolates the LABEL NAME into that chain. So a
permanent HTTP 400 on a label called `Corporate`, `Separate`, `moderate`,
`generate`, or `accurate` classified as retryable. Pre-existing, but the COST
changed: bounding the transport turned each false positive from a fast failure
into five 30s attempts plus 38s of ladder.

`is_retryable` is now structural: it walks the source chain to the typed
`google_gmail1::Error` and reads `Failure(response).status()`, or
`BadRequest(json)`'s `error.code` / `error.status` / `error.errors[].reason`
from their own fields. The transport timeout stays a text match because it is
the one error this module GENERATES rather than receives. An untyped error
mentioning a status no longer retries, which is what keeps the matcher from
silently falling back to the old sweep. Six label names are asserted as bait in
`test_is_retryable_ignores_a_label_name_that_merely_contains_rate`, so the test
cannot pass by accident.

### MF4 / S1: the write order is a correctness constraint

Root cause confirmed and sharper than round 7's framing. `SEEN_LABEL`'s doc
comment claims message-level semantics; `plan_write` put the marker in a
`ThreadWrite`'s `add` list and `modify_thread` applied it as a `threads.modify`,
labeling every message the thread held at call time. A message that arrived
after the snapshot got marked without ever being classified, and
`-label:llm/seen` then hid it forever. Permanent, not racy.

Round 8 established that **one ordering is strictly worse than the bug**:

- marker first, then a failed bucket write -> every message marked, no bucket
  label, thread can never resurface. PERMANENT LOSS.
- bucket first, then a failed marker write -> messages unmarked, thread
  resurfaces next run, the label write is idempotent, and `refresh_drafts`
  skips already-drafted threads so there is no double-draft. SELF-HEALS at the
  cost of one extra LLM call.

Implemented in that order. `ThreadWrite` now carries `classified_message_ids`
and its `add` holds the bucket only; the marker goes on in ONE trailing
`messages.batchModify` over the ids from writes that actually landed.
`batch_modify` chunks at 1000 ids for 50 quota units, so 50 threads cost 50
units for the whole pass rather than 50 per thread -- the opposite of the
proposal's worry. Being last also enforces the ordering structurally and turns
per-thread partial states into one all-or-nothing marker write. `plan_write`
still validates the marker label even though it no longer applies it, so a
missing marker fails the PLAN rather than leaving buckets written and nothing
recorded.

### Cheap wins taken in the same pass

- **`body-chars` had no minimum validation.** Nothing validated `TriageConfig`
  at all. `body-chars: 5` yielded a bare unmarked 5-char cut, reachable only by
  configuring below the floor, since `budget_messages` exempts the newest
  message. `TriageConfig::validate` now rejects `body_chars <
  MIN_MARKED_FRAGMENT_CHARS` and is wired into `Config::validate`, so the
  exemption is unreachable by construction rather than defended by a comment.
- **`backoff_secs` exceeded its own stated ceiling.** It clamped `base` and
  then added the spread on top, returning 76s at attempt 6 against a
  `MAX_BACKOFF_SECS` of 60. Unreachable at `MAX_RETRIES = 5`, a trap for
  whoever raises it. Clamp is now applied last, asserted over 32 attempts.
  Separately, the variable called "jitter" has no randomness, so parallel
  accounts align their retries perfectly; renamed to `spread` and documented as
  deterministic rather than left claiming something the code does not do.
- `test_truncate_never_exceeds_max` now starts at 0.
- The Slack timeout message says the post MAY have landed, so a human
  re-running `digest` on a bare "returned nothing" does not double-post.

### Brief claims round 8 found OVERSTATED but not wrong

- C3's escaping is "bounded and small next to `BUDGET`": the worst case is
  ~8820 against a 10000 budget, so 88%. Bounded, not small. The ladder still
  terminates.
- `test_line_caps_exceed_the_fixture_figures` proves one fixture sits under the
  caps, not that ordinary mail will not clip.

### Still open after round 8

- **S2**: the scope analysis holds, with no permanent-delete call site anywhere
  in `src/` or `tests/`. Narrowing `mail.google.com` to `gmail.modify` is
  Scott's call because it forces a re-auth through a browser flow.
- **S3**: amending the doc's "3-7 bullets" from a guarantee to a target.
- The yup-oauth2 `with_timeout` footnote, which affects `auth login` only.
- Round 8's three `defer` items.
