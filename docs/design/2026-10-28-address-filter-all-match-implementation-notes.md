## Phase 1: Green the baseline
### Design decisions
- `cargo update -p eyre` (0.6.12 -> 0.6.14), lockfile only: Cargo.lock. Baseline CI needed the newer eyre; no manifest change.
### Deviations
- None.
### Tradeoffs
- Targeted `-p eyre` update vs full `cargo update`: keeps the diff to the one package the phase names.
### Open questions
- None.

## Phase 2: All-match semantics and regression tests
### Design decisions
- `AddressFilter::matches` follows the doc's code block (empty patterns: header must be empty; empty header fails closed; globs compiled up front; all addresses vs any pattern). File: src/cfg/filter.rs:AddressFilter::matches.
- The three to/cc/from blocks in `MessageFilter::matches` are one `af.matches(..)` call each, written as let-chains. File: src/cfg/filter.rs:MessageFilter::matches. Why: clippy `collapsible_if` is denied under `-D warnings`, so the doc's nested `if` fails `otto ci`.
- Test helpers `addrs`, `addr_filter`, `address_only_filter` live in `mod tests`.
- `test_message_filter_only_me_rejects_group_send` builds the 39-address To as 38 synthetic addresses plus `scott`, with `to: [scott]`, `cc: []`, `from: [*@x]` (the `only-me-ttv` shape).
- Example yml comment now says each address field matches when every address in the header matches one of the globs.
### Deviations
- Let-chain form instead of the doc's nested `if`: same effect, clippy-clean.
- The three message-level tests also assert a positive case, so they bite both ways.
### Tradeoffs
- Helpers in tests vs repeating the full `MessageFilter` literal nine times: helpers. Old tests keep their literal style.
### Bite checks (not committed)
- (a) `.all(|e| matchers...` -> `.any(`: `cargo test --lib cfg::filter` gave `13 passed; 5 failed`.
  - `test_address_filter_all_addresses_must_match ... FAILED` (`assertion failed: !filter.matches(&addrs(&["me@x", "other@y"]))`)
  - `test_message_filter_only_me_rejects_group_send ... FAILED` (`assertion failed: !filter.matches(&group, &empty, &from, ...)`)
  - also failed: `test_address_filter_glob_all_match`, `test_message_filter_cc_patterns_all_match`, `test_message_filter_from_all_match`
- (b) empty-header guard removed: `17 passed; 1 failed`; `test_address_filter_empty_header_fails ... FAILED` (`assertion failed: !filter.matches(&[])`).
- Code restored after each; `otto ci` on the restored code exits 0.
### Open questions
- None.

## Finalization: implementation audit fold-in

### Design decisions
- Phase 3 runs entirely on ripr: the checkout and the live service are on the same host, so `bump release` installs straight into the path the systemd units exec. The doc had assumed talos for the release and the probe.

### Deviations
- Probe success check changed from `[filter:only-me-ttv] 0 matched` to "no `MATCH: <unique probe subject>` line": the count covers every candidate, so an unrelated sole-recipient mail could fail a correct build. Doc defect, not a code change.
- Every `gws` call in Phase 3 now carries its `--params`/`--json` body; a bare `get` returns 400 (missing `userId`).

### Tradeoffs
- Probe inserted after the install, not before: the timer still running v0.7.5 would pin it for real.

### Open questions
- None.
