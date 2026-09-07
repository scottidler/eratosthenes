# LLM Triage: Phase 8 Shakedown Report

Design doc: `docs/design/2026-07-06-llm-triage.md`
Implementation notes: `docs/design/2026-07-06-llm-triage-implementation-notes.md`
Date: 2026-09-07. Prior phase commit: `11f28ef` (Phase 7: Reply drafts).

Scope: exercise the new CLI surface (`triage`, and the `config`/`service`
output Phases 1 and 5 changed) with real commands against the live `tatari`
account, true up README + `eratosthenes.example.yml` against shipped
behavior, and enumerate real operator steps. Per the orchestrator's explicit
constraints, this phase did NOT run live labeling (`triage` without
`--dry-run`), did NOT run `service install`/`reinstall`, and did NOT post to
Slack.

## Commands run (all real, all output pasted verbatim below or in README)

```
$ cargo build --release                    # clean, 0 warnings
$ eratosthenes --help
$ eratosthenes triage --help
$ eratosthenes config --help
$ eratosthenes config validate
$ eratosthenes config show
$ eratosthenes service status              # required sandbox off: D-Bus denied inside it
$ eratosthenes triage --dry-run            # live tatari account, 2:33 wall time, one LLM call
$ readlink -f ~/.config/eratosthenes/tatari.yml
$ loginctl show-user saidler --property=Linger
$ otto ci
```

Full `triage --dry-run` output (50 threads, real subjects/ids) is in
`README.md` under **Triage** (excerpted) and was captured in full during this
session; the excerpt in the README is a representative subset of the same
run, not invented output. Key lines, verbatim:

```
Connecting to Gmail...
[dry-run] label 'llm/seen' does not exist yet
max-threads cap HIT: 402 candidate threads, classifying the newest 50, 352 left unseen for the next run (raise max-threads if this repeats)
...
Triage: 50 threads classified, 0 labeled, 0 skipped (dry run)
```
Exit code 0. Zero Gmail mutations (dry-run gate held).

## Findings

### Fixed (this phase)

1. **README's Commands list never mentioned `triage`, and the `service` line
   said "(run + digest)" with no mention of the triage timer.**
   `README.md:11-24`. Fixed: added a `triage` entry (with `--dry-run`) and
   corrected the `service` line to name all three timer pairs and their
   opt-in conditions.
2. **`eratosthenes.example.yml`'s `buckets:` comment promises "a matching
   state-filters entry below (needs-reply -> Keep, etc.)" that didn't
   exist** -- the example's `state-filters:` block only ever showed the
   generic `Starred`/`Important`/`Cull`/`Purge` filters, never an `llm/*`
   one. Fixed: added the five real `llm/*` state-filters (modeled on the
   live `tatari.yml`, ordering comment included: bucket filters must precede
   `Cull`) to `eratosthenes.example.yml:166-201`.
3. **No README section documented `triage` at all**: no config example, no
   CLI usage, no drafts behavior, no mention of the no-send guard. Fixed:
   added a `### Triage` section with a real config excerpt, real
   `--dry-run` output (actually run, 2026-09-07), and real `config validate`
   output.
4. **No README section for the triage timer**, unlike the existing "Digest
   timer" section. Fixed: added `### Triage timer`, including the measured
   current state (see Accepted #3 below) and a pointer to the INCIDENT entry
   before anyone runs `service reinstall`.

### Accepted (no code/doc change this phase; reason given)

1. **Design doc's Phase 8 bullet lists "dotfiles deploy" as an operator
   step; measured false.** `readlink -f ~/.config/eratosthenes/tatari.yml`
   -> `/home/saidler/repos/scottidler/dotfiles/HOME/.config/eratosthenes/tatari.yml`:
   the config is a symlink into the dotfiles repo. Editing it there is live
   on the next timer fire; there is no deploy step. Accepted: the design doc
   is treated as frozen text (per phase-implementer convention, only
   `Status:` and the parent's finalization touch it); the correct behavior
   is recorded here and is not otherwise operator-facing since README does
   not instruct a deploy step to begin with.
2. **`loginctl enable-linger` for `saidler` is already set.** Measured:
   `loginctl show-user saidler --property=Linger` -> `Linger=yes`. Accepted:
   no action needed; the design doc's Risks table correctly names
   `enable-linger` as the mitigation, and it is already in place.
3. **The triage timer is not currently installed** (`service status`
   real output, sandbox off, shows only `eratosthenes.timer` and
   `eratosthenes-digest.timer`; no `eratosthenes-triage.timer`). Accepted:
   this is the deliberate pre-Phase-5 state the 2026-09-07 INCIDENT entry
   restored to after the credential-destroying reinstall, and installing it
   requires `service reinstall`, which this phase was explicitly told not to
   run pending Scott's eval sign-off (`docs/eval/llm-triage-eval.md`).
4. **`max-threads: 50` cap bites on every run.** Measured twice now: 402
   candidate threads this session (401 the prior session per the design
   doc), 352-plus left unseen after each run. Accepted: this is the doc's
   own documented cost/latency backpressure valve (Risks: "Cost runaway"),
   working as designed; raising the cap is a config value Scott owns, not a
   defect this phase fixes.
5. **`eratosthenes service status` fails inside this session's default Bash
   sandbox** (`Failed to connect to user scope bus via local transport:
   Operation not permitted`) and succeeds identically with the sandbox
   disabled. Accepted: this is the harness's D-Bus restriction, not an
   eratosthenes defect; demonstrated working correctly once the sandbox
   restriction was lifted for that one read-only command.
6. **No label-creation operator step exists, confirmed.** `config validate`
   resolves all five bucket labels from config alone, and
   `triage --dry-run`'s own output (`[dry-run] label 'llm/seen' does not
   exist yet`) shows the engine detecting and would-create the label
   itself. The design doc's Phase 8 bullet is correct on this point;
   nothing to fix.
7. **`SLACK_XOXP_TOKEN` was destroyed this session by a `service reinstall`
   run without the var exported**, and must be re-provided before the
   digest fires Thu 2026-09-10 07:00. Accepted, not ticketed: already fully
   documented with remedy steps in the implementation notes' `## INCIDENT
   2026-09-07` entry, which this phase left untouched (append-only). No
   further action from this phase.

### Ticketed

1. **Threaded reply drafts have never been observed end-to-end against a
   live Gmail thread** (Phase 0(a)'s live probe was blocked; Phase 7 shipped
   the RFC 2822 threading headers against the documented API contract only).
   Ticketed: https://github.com/scottidler/eratosthenes/issues/1

## Acceptance-criteria items NOT verified this phase (by design)

These require live labeling, a real timer fire, or a real Slack post, all of
which this phase was told not to run:

- A timer-fired run leaving every unseen thread with exactly one `llm/*`
  label + `llm/seen`, and an immediate rerun performing zero mutations
  (journal-verified). Gated on eval sign-off.
- `llm/noise` aging off INBOX on TTL while `llm/needs-reply` survives past
  the same window. Gated on eval sign-off (requires elapsed real time after
  a live label, too).
- The Mon/Thu digest's three-section content, bullets, ask-marking, and
  degraded-mode banner. Gated on eval sign-off; also explicitly out of scope
  ("DO NOT post to Slack").
- "Every unanswered needs-reply thread has exactly one threaded reply draft"
  end-to-end in the live Gmail UI. Gated on the same eval sign-off, and the
  specific placement question is the ticketed item above.

The **send-path grep test** (`tests/no_send_guard.rs`) is NOT gated and WAS
verified this phase: 5/5 tests pass under `otto ci`, including
`src_has_no_send_call` and `guard_bites_on_an_injected_send`.

## `otto ci`

Green. `check` (compile + clippy `-D warnings` + `cargo fmt --check`): clean.
`test`: 253 + 19 + 5 + 5 + 4 = 286 tests passed, 0 failed, 0 ignored, 0
warnings anywhere in the log. `lint`: clean.
