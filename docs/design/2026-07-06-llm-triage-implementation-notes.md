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
