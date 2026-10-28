# Design Document: Address Filters Match Every Address

**Author:** Scott Idler
**Date:** 2026-10-28
**Status:** Implemented
**Review Passes Completed:** 5/5 (Draft; Correctness: fixed doc line refs, the third `bail!` site, a wrong performance claim, the ripr install path; Clarity; Edge Cases: added the parser-junk risk, made AC1 fail on `0 passed`; Excellence)

## Summary

`to: ['scott.idler@tatari.tv']` is supposed to mean "sent only to me". The matcher reads it as "I am one of the recipients", so `only-me-ttv` flagged a 39-recipient meeting invite as IMPORTANT, and `only-me-vip` starred a two-recipient forward. Fix: an address filter matches only when EVERY address in the header matches one of its patterns, and an empty header never satisfies a non-empty pattern list. Apply it to `to`, `cc` and `from` alike.

## Problem Statement

### Background

- Message filters live under `message-filters:` in `~/.config/eratosthenes/tatari.yml`. Every condition must match (AND), first match wins, and the winner claims the message.
- Address fields (`to`, `cc`, `from`) deserialize in `deserialize_opt_address_filter` (`src/cfg/filter.rs:180-206`):
  - absent -> `None`, field unconstrained
  - `[]` -> header must be empty (`cc: []` == "no CC")
  - a string or a list -> glob patterns
- `MessageFilter::matches` (`src/cfg/filter.rs:86-176`) handles `[]` itself, then delegates non-empty lists to `AddressFilter::matches` (`src/cfg/filter.rs:63-74`). The `to`, `cc` and `from` blocks (`:101-129`) are three copies of the same logic.
- The Gmail query from `compile_query` (`src/gmail/query.rs:18-65`) is a prefilter only. `match_message_filters` (`src/engine.rs:731-797`) re-checks every candidate and is authoritative.
- Intent is on record in this repo's own designs:
  - `docs/design/2026-03-29-gmail-api-migration.md`: "only me" sole-recipient detection is "the #1 feature requirement" (lines 16, 34, 281-283, 573)
  - `docs/gemini-architecture.md:16`: "ensure lists are empty or exactly size 1"
  - the live rules' comments: `only-me-vip` and `only-me-ttv` both say "direct to me, no CC"
- The same repo already encodes "only me" correctly once: `TriageThread::is_solely_to` (`src/triage/thread.rs:127-137`) requires `to.len() == 1` and empty Cc, and fails closed.

### Problem

`AddressFilter::matches` is a double `any`: true when any pattern matches any address.

```rust
for pat in &self.patterns {
    let matcher = Glob::new(pat).expect("invalid glob").compile_matcher();
    for email in emails {
        if matcher.is_match(email) { return true; }
    }
}
false
```

- It came over unchanged from imap-filter-rs-v2 (`src/cfg/message_filter.rs:67-80`, doc comment "any of the emails matches any glob") in scaffold commit a666169. So the code never met the design. This is not a regression.
- No test caught it: every `to` test (`test_message_filter_matches_to` `:354`, `test_message_filter_combined_only_me` `:398`) uses a one-address header.
- Measured damage, from logs `tatari.log.7.gz` through `tatari.log` (2026-10-21 07:21 -> 2026-10-28 21:05 UTC; older rotations are gone) joined to Gmail by timestamp and the `Triaged` marker:

| Write (UTC) | Filter | Message | To | Verdict | Pin today |
|---|---|---|---|---|---|
| 10-22 13:33 | only-me-ttv | 1a163cf35ade9d30 | 1 | correct | no |
| 10-23 18:20 | only-me-ttv | 1a169fca93964cba | 39 | **wrong** | IMPORTANT |
| 10-23 21:46 | only-me-ttv | 1a16ab6a9196b7ac | 1 | correct | IMPORTANT |
| 10-26 13:35 | only-me-ttv | 1a17867dcc481720 | 1 | correct | no |
| 10-26 21:47 | only-me-ttv | 1a17a2a947d28667 | 2 | **wrong** | no (already removed) |
| 10-27 14:23 | only-me-vip | 1a17dbe10e40e04d | 2 | **wrong** | STARRED |
| 10-27 16:57 | only-me-ttv | 1a17e46fbf6c3f6e | 1 | correct | IMPORTANT |

- That is 3 wrong pins out of 7 in one week.
- Message ids are not in the logs (`batch_modify` logs counts only; the per-message `checking ... to=` line at `src/engine.rs:767-770` is TRACE and the service runs at debug). The table is a timestamp join, confirmed against Gmail headers read with `gws` on 2026-10-28.
- Pre-window: a read-only pass over all 84 `label:triaged to:scott.idler@tatari.tv from:tatari.tv` messages finds no other message that fails all-match, escaped `leadership`, and still carries IMPORTANT or STARRED.

### Goals

- `to: [x]` means every To address matches `x`. `to: ['scott.idler@tatari.tv']` == sent only to Scott.
- Same rule for `cc` and `from`: sibling fields behave identically.
- An empty header never satisfies a non-empty pattern list (fail closed). `undisclosed-recipients:;` parses to an empty To list (`src/gmail/message.rs:106-123` drops segments with no `@`), and it must not become a vacuous "every address matched".
- `[]` keeps meaning "header must be empty".
- Tests that bite: a multi-recipient header fails `to: [me]`, and the tests fail if the matcher reverts to `any`.
- Undo the two mis-pins still standing.

### Non-Goals

- **Excluded:** changing `compile_query`. All-match on a non-empty list is a strict subset of any-match, so the existing prefilter still returns a superset of what the matcher accepts.
- **Excluded:** an "unpin what the new matcher rejects" backfill mode. eratosthenes' IMPORTANT and Gmail's own IMPORTANT are the same system label id, so a sweep cannot tell them apart, and the digest treats Important as a human pin (`src/digest/mod.rs:60-69`). Two messages need the undo; they are named and handled by hand in Phase 3.
- **Excluded:** implicit aliases for the account owner. `to: [me]` matches only the listed patterns. A group or alias that delivers to Scott fails it, which is correct for "only me". If Scott wants an alias to count, he adds it as a pattern.
- **Parked:** routing `is_solely_to` through `AddressFilter`. Two encodings of "only me" is the class this bug lives in, but triage is not broken and nobody asked. Revisit if either encoding changes again.
- **Parked:** case-insensitive patterns. Addresses are lowercased (`src/gmail/message.rs:115,117`), patterns are not, so a pattern with a capital letter never matches. No live pattern has one. Revisit when a config needs one.
- **Parked:** compiling globs once at config load. Today a bad glob panics at match time (`.expect("invalid glob")`, `src/cfg/filter.rs:65`). Not part of this bug. Revisit on the first bad-glob panic.
- **Parked:** logging message ids on pin writes. It would have made the blast radius a log grep instead of a timestamp join. Revisit if another mis-pin investigation needs it.

## Proposed Solution

### Overview

1. Bump `eyre` 0.6.12 -> 0.6.14 so `otto ci` is green on the current toolchain (prerequisite, see Phase 1).
2. Make `AddressFilter::matches` the single owner of address semantics: empty-list rule plus all-match.
3. Collapse the three copies in `MessageFilter::matches` to one call per field.
4. Regression tests, positive and negative, proven to bite.
5. Correct the example config's semantics comment.
6. Ship to ripr, prove the live matcher, undo the two mis-pins.

### Matching semantics

| Patterns | Header | Result |
|---|---|---|
| `[]` | empty | match |
| `[]` | non-empty | no match |
| non-empty | empty | **no match** |
| non-empty | every address matches some pattern | match |
| non-empty | any address matches no pattern | **no match** |

```rust
impl AddressFilter {
    /// Every address in the header must match at least one pattern; an empty pattern
    /// list means the header must be empty. An empty header never satisfies a
    /// non-empty pattern list, so a header that parsed to no addresses fails closed.
    /// Applies to parsed addresses only: a segment the parser drops is never checked.
    pub fn matches(&self, emails: &[String]) -> bool {
        if self.patterns.is_empty() {
            return emails.is_empty();
        }
        if emails.is_empty() {
            return false;
        }
        let matchers: Vec<_> = self.patterns.iter()
            .map(|p| Glob::new(p).expect("invalid glob").compile_matcher())
            .collect();
        emails.iter().all(|e| matchers.iter().any(|m| m.is_match(e)))
    }
}
```

`MessageFilter::matches` then reads, per field:

```rust
if let Some(ref af) = self.to {
    if !af.matches(to) { return false; }
}
```

### Effect on live rules

| Rule | Address fields | Intent | Under all-match |
|---|---|---|---|
| `bots` | `from: machine@tatari.tv` | sender | unchanged (From holds one address) |
| `leadership` | `from: [philip, mark.weiler]` | sender, any recipients | unchanged; still claims multi-recipient leadership mail first |
| `only-me-vip` | `to: [scott]`, `cc: []`, 16 `from` | only me, no CC | fixed |
| `only-me-ttv` | `to: [scott]`, `cc: []`, `from: *@tatari.tv` | only me, no CC | fixed |
| example `only-me-star` | `to: [you]`, `cc: []` | only me, no CC | fixed |
| example `only-me` | `to: [you]`, no `cc` | To is only me, CC free | correct |

No rule in either config means "I am among the recipients", so nothing loses intended behavior.

### Implementation Plan

#### Phase 1: Green the baseline
**Model:** sonnet
- `cargo update -p eyre` (0.6.12 -> 0.6.14). Lockfile only.
- Why: `otto ci` on main exits 1. rustc/clippy 1.99.1 (installed 2026-10-16, after v0.7.5 went green on 2026-10-06) reports `trailing semicolon in macro used in expression position` inside eyre 0.6.12's `bail!` at `src/gmail/rate.rs:246`, `src/triage/classify.rs:253` and `src/digest/bullets.rs:207`, and `-D warnings` turns it into an error. eyre 0.6.14 clears it (verified in a scratch clone).
- **Success criteria:** `otto ci` exits 0 with `Cargo.lock` the only changed file.

#### Phase 2: All-match semantics and regression tests
**Model:** sonnet
- `AddressFilter::matches` per the code above, with the doc comment.
- Collapse the three blocks in `MessageFilter::matches` (`src/cfg/filter.rs:101-129`) to one `af.matches(...)` call each.
- New tests in `src/cfg/filter.rs`:
  - `test_address_filter_all_addresses_must_match`: `[me@x]` vs `[me@x, other@y]` -> false
  - `test_address_filter_glob_all_match`: `*@x` vs `[a@x, b@x]` -> true; vs `[a@x, b@y]` -> false
  - `test_address_filter_multi_pattern`: `[a@x, b@x]` vs `[b@x, a@x]` -> true
  - `test_address_filter_empty_header_fails`: `[me@x]` vs `[]` -> false
  - `test_address_filter_empty_patterns`: `[]` vs `[]` -> true; `[]` vs `[a@x]` -> false
  - `test_message_filter_only_me_rejects_group_send`: the `only-me-ttv` shape against a 39-address To -> false; against To = `[scott]` -> true
  - `test_message_filter_cc_patterns_all_match`: `cc: ['*@x']` vs `[a@x, b@y]` -> false
  - `test_address_filter_duplicates_match`: `[me@x]` vs `[me@x, me@x]` -> true
  - `test_message_filter_from_all_match`: the `bots` shape (`from: [machine@x]`) vs From `[machine@x, amit@x]` -> false. Pins the first-match reroute (see Security): a parsed two-address From no longer claims `bots`.
- `eratosthenes.example.yml:139-142`: say each address field matches when every address in the header matches one of its globs, so `to: [me]` means only me.
- Bite check (not committed): revert `all` to `any` and confirm the multi-recipient tests fail; drop the empty-header guard and confirm `test_address_filter_empty_header_fails` fails. Record both in the implementation notes.
- **Success criteria:**
  - `cargo test --lib cfg::filter` passes, including the nine new tests.
  - With `.all(` reverted to `.any(`, `test_address_filter_all_addresses_must_match` and `test_message_filter_only_me_rejects_group_send` fail.
  - With the empty-header guard removed, `test_address_filter_empty_header_fails` fails.
  - `otto ci` exits 0.

#### Phase 3: Ship, prove live, undo the mis-pins
**Model:** sonnet (operator steps, run in the main session)
- Release with `bump release` on talos (`bump --gates`: ungated -> shipit path). It installs on talos only.
- Install on ripr: in `~/repos/scottidler/eratosthenes` (checkout at v0.7.5 today), `git pull`, then `otto install` (`cargo install --path .`). The 5-minute timer picks up the new binary on its next run.
- Prove it on ripr: `eratosthenes --version` reports the release; the next scheduled run logs no errors.
- Prove the matcher live with a probe message. `--version` passes with the old matcher too. The 7 historical messages carry the marker, so they are never re-checked. And on 2026-10-28 every unread `only-me-ttv` candidate had a non-empty Cc, so `cc: []` rejects them under either matcher. Steps:
  - `gws gmail users messages insert` a message into the mailbox (sends nothing) with labels `INBOX`, `UNREAD`: From `scott.idler@tatari.tv`, To `scott.idler@tatari.tv, scott.idler+allmatch@tatari.tv`, no Cc.
  - Run `eratosthenes --log-level trace run --dry-run tatari` on talos with the new binary. Stop it once `~/.local/share/eratosthenes/logs/tatari.log` shows `[filter:only-me-ttv] N matched`; the message filters finish in seconds, and the state pass after them is irrelevant.
  - Read the probe's `checking` line and the `matched` count, then `gws gmail users messages trash` the probe.
- Undo the two standing mis-pins with `gws gmail users messages modify`:
  - remove `IMPORTANT` from 1a169fca93964cba (only eratosthenes set it: `plan_pin_ids` skips threads already IMPORTANT, so Gmail had not marked it)
  - remove `STARRED` from 1a17dbe10e40e04d (set by `only-me-vip` at 10-27 14:23)
- **Success criteria:**
  - `ssh ripr.lan 'eratosthenes --version'` prints the release version.
  - The probe dry run logs the probe's `[filter:only-me-ttv] checking` line, then `[filter:only-me-ttv] 0 matched`.
  - `gws gmail users messages get` for 1a169fca93964cba returns `labelIds` without `IMPORTANT`; for 1a17dbe10e40e04d, without `STARRED`.

## Acceptance Criteria

- [ ] `cargo test --lib test_address_filter_all_addresses_must_match` reports `1 passed; 0 failed` (a `0 passed` result is a FAIL: the filter matched nothing).
  - Observed on main: `test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 328 filtered out`. The test is added in Phase 2.
- [ ] `cargo test --lib cfg::filter` reports `>= 18 passed; 0 failed`, and with `.all(` reverted to `.any(` in `AddressFilter::matches` it reports `>= 2 failed`.
  - Observed on main: `test result: ok. 9 passed; 0 failed; ... 319 filtered out`. The revert half cannot run until Phase 2 adds `.all(`.
- [ ] `otto ci` exits 0.
  - Observed on main: exit 1, `check` task, clippy `trailing semicolon in macro used in expression position` x3 (eyre 0.6.12 `bail!` on rustc 1.99.1). Phase 1 fixes it.
- [ ] `ssh ripr.lan 'eratosthenes --version'` prints the release that carries this change, not `v0.7.5`.
  - Observed on main: `eratosthenes v0.7.5`.
- [ ] The Phase 3 probe dry run logs `[filter:only-me-ttv] checking <probe> to=["scott.idler@tatari.tv", "scott.idler+allmatch@tatari.tv"] cc=[]`, then `[filter:only-me-ttv] 0 matched`.
  - Observed on main (v0.7.5, 2026-10-28 21:33 UTC, probe 1a1846edca454fca, trashed after): `[filter:only-me-ttv] checking 1a1846edca454fca to=["scott.idler@tatari.tv", "scott.idler+allmatch@tatari.tv"] cc=[] from=["scott.idler@tatari.tv"]` then `[filter:only-me-ttv] 1 matched (total claimed: 1)`. Any-match claims it; that is the bug.
- [ ] `gws gmail users messages get` returns `labelIds` without `IMPORTANT` for 1a169fca93964cba and without `STARRED` for 1a17dbe10e40e04d.
  - Observed on main: 1a169fca93964cba `["IMPORTANT","Label_63","Label_64","Label_69","CATEGORY_PERSONAL","INBOX"]`; 1a17dbe10e40e04d `["UNREAD","STARRED","Label_63","CATEGORY_PERSONAL","INBOX"]`.

## Resolved Decisions

- 2026-10-28: All-match applies to `to`, `cc` and `from`, not `to` alone. Sibling fields behave the same. `from` holds one address in practice, so no observed mail changes. A From that parses to 2+ addresses can now miss `bots`/`leadership` and fall through to a later rule (see Security).
- 2026-10-28: No implicit owner aliases. Patterns are the whole contract.
- 2026-10-28: Remove the STARRED on 1a17dbe10e40e04d. Only-me means ONLY ME, no matter what: a two-recipient forward never earns an only-me pin.

## Alternatives Considered

### Alternative 1: A separate `only:` flag per field
- **Description:** keep any-match as the default and add `to: {patterns: [...], only: true}`.
- **Pros:** an "I am among the recipients" rule stays expressible.
- **Cons:** no rule in either config wants any-match; the designs already define the list as exhaustive; the default would stay the wrong thing for every existing rule.
- **Why not chosen:** a second knob that encodes the same meaning as the list itself. If an "includes" rule is ever needed, add it then.

### Alternative 2: Fix only `to`
- **Description:** all-match for `to`, leave `cc`/`from` any-match.
- **Cons:** three fields with one syntax and two meanings.
- **Why not chosen:** sibling fields must behave identically.

## Technical Considerations

### Dependencies
- `eyre` 0.6.12 -> 0.6.14 (lockfile only). No new crates; `globset` is already direct.

### Performance
- Globs still compile per `matches` call, but now all of them up front (16 for `only-me-vip`) instead of lazily with an early return. That also surfaces a bad later glob the early return used to skip. Compiling at load is parked (Non-Goals).

### Security
- Each rule accepts a subset of what it accepted before. Rule order can still move a message to a later rule: first match wins, so a message an earlier rule now rejects reaches the later ones. Example: From `machine@tatari.tv, amit@tatari.tv` misses `bots` and can land in `only-me-ttv`. Only a multi-address parsed From reaches this; none is observed. `test_message_filter_from_all_match` pins it.

### Testing Strategy
- Unit tests in `src/cfg/filter.rs`, positive and negative, with the bite check above.
- Live proof on ripr after install, plus the two Gmail label reads.

### Rollout Plan
- Single repo, no cross-repo blast radius. Ship order: Phase 1 -> Phase 2 -> release -> install on ripr -> undo the two pins.
- Rollback: reinstall v0.7.5 on ripr. That restores the known mis-pin behavior. The pin undo is two label removals, re-addable by hand.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| A legit "only me" mail stops getting pinned because To carries Scott twice or a display-name quirk | Low | Med | All-match accepts duplicates; `parse_address_header` extracts `<addr>` and drops no-`@` segments; tests cover duplicates |
| An empty-To message (undisclosed recipients, Bcc) gets pinned vacuously | Low | Med | Explicit empty-header guard plus `test_address_filter_empty_header_fails` |
| The pin undo removes a pin Scott added by hand after the bug | Low | Low | Labels re-add in one click; both messages are named, so Scott sees exactly what is touched |
| A To header the naive comma split mangles (`"a@b, c" <d@e>` yields junk `"a@b`) | Low | Low | Under all-match a junk address fails to match, so the result is a missed pin. Parser left as is |
| The parser silently drops a segment with `<` but no `>` (`parse_address_header`, `src/gmail/message.rs`): `me@x, Other <other@y` parses to `[me@x]` and passes `to: [me]`; a malformed Cc can vanish and satisfy `cc: []` | Low | Med | Pre-existing, not a regression; any-match had the same hole. Residual false-positive path, parser out of scope |

## Open Questions

None.

## References

- `docs/design/2026-03-29-gmail-api-migration.md` (sole-recipient requirement)
- `docs/gemini-architecture.md`
- `docs/design/2026-09-03-message-filter-act-once.md` (query compilation for multi-pattern `to`/`from`)
- imap-filter-rs-v2 `src/cfg/message_filter.rs:67-80` (origin of the any-match)
