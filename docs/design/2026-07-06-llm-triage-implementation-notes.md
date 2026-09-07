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
