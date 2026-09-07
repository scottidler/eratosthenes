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
