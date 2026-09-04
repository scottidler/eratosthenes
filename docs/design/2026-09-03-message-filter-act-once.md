# Design Document: Message-Filter Act-Once Semantics

**Author:** Scott Idler
**Date:** 2026-09-03
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

eratosthenes re-applies `STARRED` and `IMPORTANT` to the same messages on every
run (34-37 messages, 254 runs on 2026-09-02, never 0), so unstarring a message is
undone inside one timer tick. Fix by recording filter HANDLING as a marker label in Gmail
(Gmail is the ledger), isolating each filter's candidate set to its own query,
and compiling multi-pattern `from:`/`to:` to real Gmail OR.

## Problem Statement

### Background

- The engine runs three stages per run, in this order: **stage sanitization**
  (`sanitize_stages`, `src/engine.rs:36`, which the code logs as its own "Phase 0" and is
  unrelated to this doc's implementation Phase 0), then the **message-filter** stage, then
  the **state-filter** stage.
- The `message-filter` stage applies `Star` | `Flag` | `Move` to matching inbox mail.
- The `state-filter` phase ages threads out to Purgatory | Oblivion by TTL, and
  `Starred: ttl: Keep` | `Important: ttl: Keep` exempt pinned threads from culling.
- A Slack digest posts the pinned set Mon,Thu 07:00 to `D01G4Q7AWLV`.
- Config lives in dotfiles: `HOME/.config/eratosthenes/tatari.yml` (unchanged since 4fe94e0, 2026-06-27).
- **This bug was already found and already "fixed" once.**
  `docs/design/2026-03-29-unread-gating-and-sanitization.md:20` states it verbatim:
  "When a user reads a starred VIP email and un-stars it to let it age off, the next
  run's Phase 1 re-matches the message and re-stars it. The user can never dismiss a
  message." The shipped fix was the `is:unread` gate. It rests on the premise that
  dismissing means READING then un-starring. Un-starring from the thread list does not
  mark a message read, so the message stays UNREAD and stays eligible. The premise is
  false, so the gate never engaged. This doc fixes that forward; it does not revert it.

### Problem

Reported: "it should not only grow. me removing a star or important should mean
its gone. NOT REPORTED ON"

Four defects, one symptom.

**1. There is no record of user intent.** The only re-application guard is
read-state (the 2026-03-29 gate above), in two places:

- `src/gmail/query.rs:38-41` appends `is:unread` to every compiled query, commented
  "Only match unread messages - prevents re-labeling read emails".
- `src/engine.rs:265-269` repeats it in Rust: `if msg.is_read() { continue }`,
  commented "belt and suspenders with is:unread in query".

Read-state is the wrong predicate. Unstar an unread message that still matches
its filter's address criteria and the next run re-stars it. Measured: `[filter:only-me-ttv]
flagging 8 messages` every run == `is:important in:inbox is:unread` == 8, same set.
Across 254 runs on 2026-09-02 the matched count ranged 34-37 and was NEVER 0
(`grep -c "Done: 0 messages matched filters"` -> 0).

**2. Candidate pooling destroys filter scope.** `execute_message_filters`
(`src/engine.rs:204-226`) accumulates every filter's query hits into one `all_ids`,
then the match loop (`src/engine.rs:250-291`) walks `for filter in filters` over
`for id in &all_ids`. Each filter is matched against the union of all queries.

A filter's effective scope is therefore emergent, not declared. Live proof
(`~/.local/share/eratosthenes/logs/tatari.log:4694-4722`):

```
[filter:leadership]   query returned 0 candidates
[filter:leadership]   19 matched (total claimed: 19)
[filter:leadership]   starring 19 messages
```

Its own query found nothing. It acted on 19 messages that `only-me-ttv`'s broad
query fetched.

This cuts both ways. `leadership`'s declared scope is 20 messages; its effective
scope is the 19 that also fall inside `only-me-ttv`'s query:

| query | count |
|---|---|
| `from:{philip@tatari.tv mark.weiler@tatari.tv} label:inbox is:unread` (declared) | 20 |
| `to:scott.idler@tatari.tv from:{philip mark.weiler} label:inbox is:unread` (effective) | 19 |

So pooling causes over-application AND a miss.

**3. Multi-pattern `from:`/`to:` compiles to a Gmail AND.** `compile_query`
(`src/gmail/query.rs:13-24`) joins patterns with a space INSIDE `from:(...)`.
Gmail reads that as AND, which is unsatisfiable:

| query | count |
|---|---|
| `from:(philip@tatari.tv mark.weiler@tatari.tv) label:inbox is:unread` | 0 |
| `from:{philip@tatari.tv mark.weiler@tatari.tv} label:inbox is:unread` | 20 |
| `from:(philip@tatari.tv OR mark.weiler@tatari.tv) label:inbox is:unread` | 20 |

Defect 2 masks defect 3: the Rust matcher performs the OR the query failed to,
using another filter's pool.

**4. Custom labels never match in message-filters.** `GmailMessage::labels()`
(`src/gmail/message.rs:50-52`) maps raw Gmail label ids through `Label::new(id)`
with no resolver, so a custom label arrives as `Label::Custom("Label_60")`, not
`Label::Custom("Oblivion")`. The message-filter match loop uses that unresolved
form (`src/engine.rs:266`). Any `labels.included` | `labels.excluded` naming a
CUSTOM label is therefore a silent no-op.

This is the same bug 67946c5 fixed for STATE filters ("Thread labels come back
from Gmail as raw IDs (e.g. Label_61), but state filters match against names").
State filters got `labels_resolved(&client.resolver)` (`src/engine.rs:448`);
message filters never did. `GmailThread` has `labels_resolved`
(`src/gmail/message.rs:83-87`); `GmailMessage` has no equivalent.

It matters here because the marker design excludes a custom label. Left unfixed,
the marker exclusion silently never fires and the re-application loop continues.
The Phase 0 spike below only worked because it used `STARRED` | `IMPORTANT`,
system labels whose ids equal their names.

### The compounding effect

- The message-filter phase re-pins 34-37 messages per run.
- `state-filters` `Keep` then exempts those threads from the `Cull` TTL
  (`read: 3d` / `unread: 21d`, `tatari.yml:78-83`; the config's own inline comment
  says 7d and is stale).
- The pinned pile is monotonic by construction: 34 of 38 starred threads now sit
  at inbox positions 382-417 of 417, dated 2026-02-18 -> 2026-08-17 (measured 2026-09-03).
- The digest faithfully reports the pile. The digest is NOT wrong: 38 starred /
  9 important matches `is:starred in:inbox` / `is:important in:inbox` exactly.

### Goals

- Removing a star or an important flag is permanent. eratosthenes never re-applies it.
- The digest reports only what is pinned right now (already true, must not regress).
- Each filter acts on exactly the messages its own criteria declare.
- Multi-pattern `from`/`to` means OR, identically in the Gmail query and in Rust.
- A custom label named in a message-filter's `labels` block actually matches.

### Non-Goals

- **Excluded:** redesigning `state-filters`, TTL semantics, or Purgatory | Oblivion.
- **Excluded:** digest format, schedule, or channel.
- **Excluded:** Gmail's own importance classifier, and the two Gmail-side filters
  (one marks `*@superhuman.com` important, one removes `IMPORTANT` from mail
  delivered to me). The second is defeated by eratosthenes re-adding `IMPORTANT`
  ~254x/day. It stops being defeated when this doc lands. No Gmail-side change.
- **Excluded:** whether filters should act on already-read mail. `is:unread` stays
  as a scope constraint, not an idempotency guard. Changing it is unrequested scope.
- **Parked, and it is cb472b9's COMMENT that is wrong, not its code.** Found while
  justifying this doc's own query inventory. `sanitize_stages` is called ONLY from
  `engine::execute` (`src/engine.rs:36`), reached only by `run` (`src/lib.rs:77`); the
  digest is a SEPARATE subcommand (`src/main.rs:278`, entry `src/lib.rs:84`,
  `ExecStart={binary} digest` at `src/service.rs:138`) with no sanitize anywhere in its
  path. So if a reply lands between the last `run` and the digest, the thread is MIXED
  ("new msg = INBOX, old msgs = Purgatory", `src/engine.rs:138-140`) and
  `-label:purgatory` does NOT exclude it, because the new message lacks the label.
  cb472b9's comment names exactly that case as one it wants excluded: "a later reply that
  re-added INBOX without clearing the stage label" (`src/lib.rs:98-102`).
  **The code's behavior is the better behavior.** A mixed thread is one where a reply
  arrived: it is pinned AND genuinely back in the inbox, so reporting it is right and
  excluding it would hide live pinned mail. So the negation is safe here by DIRECTION,
  not by uniformity. Actionable item is therefore to correct the comment, NOT to add
  post-fetch stage filtering in `digest::build`, which would suppress the threads you
  most want to see. Out of scope, the digest is a Non-Goal, and this doc does not make it
  worse. Established from code and that comment, NOT from a live observation: no mixed
  thread exists right now. Measured with the existential AND that row 5 describes, which
  is exactly the right probe for a mixed thread: `label:Oblivion in:inbox` = 0 and
  `label:Purgatory in:inbox` = 0 threads, with `Purgatory` holding 0 threads total and
  `Oblivion` 1503 (`labels.get`, authoritative; a `threads.list` search caps out and
  under-reports it). Revisit condition: a
  digest reports a thread carrying a stage label AND that thread has no new reply.
- **Parked, revisit if run duration exceeds the timer interval:** the state-filter
  phase re-fetches 1932 threads via sequential `get_thread` every run, mostly to
  transition nothing (218 of 254 runs on 2026-09-02: `0 threads transitioned`).
  Runs take 3m29s-4m05s; observed timer spacing is 5-6min (`OnUnitActiveSec=5min`).
  It is latency-bound, not rate-limited: 231s for 1932 threads is ~120ms/thread, so
  the 5min budget is exhausted around ~2,500 active threads and today's 1,932 leaves
  ~570 of headroom. Parked rather than excluded because the pin loop is what GROWS
  that number: fixing it shrinks the active set, so this doc is self-stabilizing
  against the cliff rather than indifferent to it.

## Proposed Solution

### Overview

Gmail is the ledger. When a filter HANDLES a message, the engine stamps that message
with a marker label, whether or not the action changed anything. Every message-filter excludes the marker. Unstar
and the marker stays, so the message is never re-acted.

### Architecture

**The `Move` invariant. Read this before changing any rule in this document.**

> **`Move` is the only action that removes the labels its own filter scopes on. Therefore
> any rule quantified over "actions" is WRONG for `Move` unless `Move` is checked
> separately.**

That is not documentation hygiene, it is the root cause of five separate defects found in
review, all the same shape:

| round | the rule stated over "actions" | why `Move` broke it |
|---|---|---|
| 3 | thread-scoped suppression | skipping a `Move` strands the message in the inbox forever |
| 5 | "a failed stamp retries next run" | a moved message is archived and read, so there IS no next run |
| 6 | fold the marker per action | `[Star, Bots]` commits the marker, then `Move` fails and never retries |
| 7 | (my own sweep) API Design and Phase 5 restatements | same rule, unupdated |
| 8 | fold into the last APPLIED action | `[Bots, Star]` archives, then fails, leaving it unmarked forever |

Stating the invariant is what stops the class. The grep below only catches it afterwards.

Maintenance note, secondary: **do not trust a section list, grep the whole document.** An
earlier version of this note listed "Architecture, Overview, Resolved Decisions and Risks".
Review showed that missed at least thirteen more sections that restate operational rules,
including API Design, Data Model, Phases 1 to 5, Acceptance Criteria, Performance, Edge
cases, Testing Strategy, Rollout and Rollback. Any list short enough to be useful is wrong,
and a complete list is just the document. So when a rule changes, grep the entire file for
the new phrasing AND the phrasing being replaced, then re-read every hit.

```
message-filter phase, per filter:
  candidates = gmail_query(filter.criteria + -label:<marker> + is:unread)
  matched    = candidates.filter(|m| !claimed && filter.matches(m))   # matches() enforces the marker via labels.excluded
  filter has a Move (validated last): marker folds into the Move write;
  filter has no Move: pin writes, then one marker-only write over all matched
  claimed |= matched          # first-match-wins, unchanged
```

Four changes, in dependency order:

1. `compile_query` emits Gmail brace-OR for multi-pattern address fields.
2. The match loop iterates each filter's OWN candidate ids, not the pooled union.
   Message FETCHES stay deduped across filters; only the SCOPE is per-filter.
3. Message labels resolve custom ids to names, so a marker exclusion can match at all.
4. A single global marker label, excluded by every filter, written LAST: folded into the
   `Move` write where the filter has one (validated to be its last action), otherwise a
   single marker-only write after the pins.

### Why a single global marker, not per-filter label exclusions

`labels.excluded` already exists and works today: parsed at
`src/cfg/filter.rs:191-256` (the `Value::Mapping` arm, keys `included` | `excluded`,
unknown keys rejected via `de::Error::unknown_field`) and enforced at
`src/cfg/filter.rs:131-135`. It is enforced ONLY in the Rust matcher;
`compile_query` never emits `-label:` for it.

So a zero-code config fix looks available: add `excluded: [STARRED]` to the Star
filters and `excluded: [IMPORTANT]` to the Flag filter. It is worse than the bug.
Measured by dry-run against the live mailbox (Phase 0 below):

| filter | baseline matched | with per-filter `excluded` |
|---|---|---|
| `leadership` (Star) | 19 | 0 |
| `only-me-vip` (Star) | 8 | 0 |
| `only-me-ttv` (Flag) | 8 | **21** |
| run total (`Done:` line) | 35 | 21 |

The run total DROPS while the harm goes UP: 21 messages newly acquire `IMPORTANT`
where 8 did before.

`leadership` and `only-me-vip` stop matching, so they stop CLAIMING, so their 27
messages fall through to `only-me-ttv`, which excludes only `IMPORTANT`. Result:
`[filter:only-me-ttv] flagging 21 messages as important`. The config-only fix
would newly flag 21 starred messages as Important.

The defect is heterogeneous exclusions interacting with first-match-wins claiming.
One marker excluded uniformly by every filter has no fall-through: a message
skipped by `leadership` for carrying the marker is skipped by `only-me-ttv` for
the same reason.

### Data Model

New config key under the account root, defaulted so no dotfiles change is required:

```yaml
marker-label: Triaged   # default; override to rename
```

Flat name, no `/` nesting, matching the existing convention (Oblivion | Purgatory
| Bots). Nesting would raise a Gmail query-escaping question for `-label:parent/child`
that a flat name removes outright rather than tests.

- Created on startup via the existing `create_label_if_missing`
  (`src/gmail/label.rs:62-93`), with `labelListVisibility: labelHide` and
  `messageListVisibility: hide` so it does not clutter the Gmail sidebar.
- Applied per MESSAGE, matching where actions are applied (`batch_modify` takes
  message ids). Digest and `state-filters` are thread-level and read live
  `is:starred` | `is:important`, so neither needs marker awareness.
- `Triaged` is a new KIND of label in this mailbox: Bots | Purgatory | Oblivion are
  destinations, this is a marker. That is deliberate and worth knowing when reading
  the label list.
- The marker is NOT a state-filter label, so it never enters
  `build_active_threads_query` (today: `in:inbox OR is:starred OR is:important OR
  label:purgatory OR label:oblivion`).

Semantics: the marker means **"a message-filter HANDLED this message"**, stamped on
every matched message whether or not the action changed anything. It does NOT mean
"the action was applied to this message": under the thread-level suppression rule
below, most matched messages are handled without being starred.

That distinction is load-bearing. If only the acted-on message were stamped, its older
matching unread siblings would stay unmarked and eligible, and the next run would star
them: the same loop, one message along. Messages that no filter matched stay unmarked,
so a later config change still reaches old mail.

### API Design

New `FilterAction` variant. `FilterAction::Move` (`src/engine.rs:358-380`) adds
the destination AND removes `INBOX` + `UNREAD`; a marker must add a label and
remove nothing.

```rust
// src/cfg/filter.rs
pub enum FilterAction {
    Star,
    Flag,
    Move(String),
    Tag(String),   // NEW: add label, remove nothing
}
```

`Tag` is the mechanism the engine uses to stamp the marker. It is not required in
user config: the engine folds the marker into the `Move` write where the filter has one,
or issues one marker-only write after the pins where it does not. Exposing `Tag` in config costs nothing and cannot be
misconfigured into the marker's role, since the marker name comes from
`marker-label`.

### Implementation Plan

Land with the timer stopped (`systemctl --user stop eratosthenes.timer`), because
it mutates the live mailbox every 5-6 minutes. Restart after Phase 5.

#### Phase 0: Prove Gmail OR syntax and disprove the config-only fix
**Model:** sonnet
- Zero code. `gws` queries for the three OR forms; `eratosthenes --dry-run --config`
  against a copy of the real config with per-filter `excluded` added.
- **Success criteria:** `from:{a b}` returns > 0 while `from:(a b)` returns 0;
  the per-filter-`excluded` dry-run shows a filter's matched count INCREASE.
- **EXECUTED 2026-09-03.** `from:(a b)` = 0, `from:{a b}` = 20, `from:(a OR b)` = 20.
  Dry-run with per-filter `excluded`: `only-me-ttv` 8 -> 21, logged at 15:21:31 as
  `[filter:only-me-ttv] flagging 21 messages as important`, run total
  `Done: 21 messages matched filters ... (dry run)` at 15:24:52 vs the baseline
  dry-run's `Done: 35 ...` at 15:21:22. Both proven. Neither dry-run wrote anything, but note WHY: `--dry-run` is not
  fully read-only (see the rollout note below), and it wrote nothing here only because
  every label those configs reference already existed.

#### Phase 1: Compile multi-pattern address fields to Gmail OR
**Model:** sonnet
- `compile_query` (`src/gmail/query.rs:5-24`): multi-pattern `from`/`to` -> `from:{a b}`.
  Single pattern keeps `from:(a)`.
- No existing test pins the space-joined multi-pattern output, so nothing breaks here:
  all four tests in `src/gmail/query.rs` use single-pattern filters
  (`:77` `from:(*@example.com)`, `:100` and `:117` exact-match). Add the missing
  multi-pattern coverage.
- `to:` has the SAME defect by a different code path: `src/gmail/query.rs:6-10` pushes
  a separate `to:{pat}` term per pattern, ANDed at top level. Latent today only because
  every `to` in the config is single-pattern. Fix both fields, not just `from`.
- **Success criteria:** a unit test asserts a 2-pattern `to` compiles to exactly one
  `to:{a b}` term, not two `to:` terms; a debug run logs
  `[filter:leadership] query returned N candidates`
  with N > 0 and N equal to the live count of
  `from:{philip@tatari.tv mark.weiler@tatari.tv} label:inbox is:unread` at run time
  (21 on 2026-09-03, drifts with new mail); a unit test asserts `from:{philip@tatari.tv mark.weiler@tatari.tv}` for the 2-pattern case.

#### Phase 2: Per-filter candidate scope
**Model:** opus
- Keep a per-filter `Vec<String>` of ids from that filter's own query. Keep the
  single deduped fetch pass into `messages: HashMap`.
- Match loop walks the filter's own ids; `claimed` still spans filters (first-match-wins
  is declared behavior, `src/engine.rs:248`).
- Document in a code comment that the Gmail query is an intentional PREFILTER and the
  Rust matcher is authoritative: Gmail cannot express `cc: []`, `headers.List-Id: []`,
  or globset semantics (`from:(*@tatari.tv)` returns 193 unread inbox messages, and
  Gmail treats the `*` as noise).
- **Success criteria:** for every filter in a debug run, `matched <= query returned`;
  a test with two filters whose queries return disjoint id sets asserts neither
  matches the other's ids.

Phase 2 WIDENS real scope, measured 2026-09-03 (counts drift as mail arrives):

| filter | matched today (pooled) | its own query post-Phase-1 |
|---|---|---|
| `bots` | 0 | 0 |
| `leadership` | 19 | 21 |
| `only-me-vip` | 8 | 35 |
| `only-me-ttv` | 8 | 70 |

Candidate counts, not match counts: the Rust matcher still enforces `cc: []` and
`headers.List-Id: []`, which Gmail cannot express. But `only-me-vip` going from an
8-message pooled slice to a 35-message own query is a large widening, and it is
why the rollout backfills markers BEFORE the timer restarts.

#### Phase 3: Resolve custom labels in message-filters
**Model:** sonnet
- Add `GmailMessage::labels_resolved(&LabelResolver)` mirroring
  `GmailThread::labels_resolved` (`src/gmail/message.rs:83-87`).
- `src/engine.rs:266` uses it instead of `msg.labels()`. `client.resolver` is
  already in scope.
- Ships BEFORE Phase 4: the marker exclusion is a custom label and is a silent
  no-op without this.
- **Success criteria:** (1) a test with a message carrying custom label id `Label_60`,
  a resolver mapping `Label_60 -> Oblivion`, and a filter with
  `labels: {excluded: [Oblivion]}` asserts no match; that test fails on current `main`.
  (2) `rg -n 'msg\.labels\(\)' src/engine.rs` returns nothing.
  **Observed on main:** `266:            let labels = msg.labels();`
  The two are not redundant: (1) proves the one call site at `src/engine.rs:266` is fixed,
  (2) proves no OTHER call site reintroduces the unresolved form.

#### Phase 4: Marker label and act-once
**Model:** opus
- Add `marker-label` config with default `Triaged`.
- **Validate `marker-label` at config load: it must match NO state-filter label and NO
  state-filter destination.** Not a Risks row, actual Phase 4 work. Load-bearing, not
  decorative: if the marker could be set to `Purgatory` or `Oblivion`, it WOULD reach
  state filtering, since `derive_stages` harvests state `Move` destinations
  (`src/engine.rs:101-112`) and `ensure_labels` also harvests custom names from state
  filters' `labels` blocks (`src/engine.rs:79-84`), and `sanitize_stages` would then
  strip it. Fail loudly at load, not at runtime.
- **Register it in `ensure_labels` (`src/engine.rs:63-72`), which today collects only
  `FilterAction::Move` destinations.** Ordering is already correct and needs no change:
  `ensure_labels` is the FIRST thing `execute` does (`src/engine.rs:33`), before stage
  sanitization and before the message-filter stage, so the marker id is in the resolver
  before any action folds it into an add-list. Unregistered, the label is never created,
  `resolver.resolve_name` returns `None`, and the `unwrap_or(dest.as_str())` pattern
  (`src/engine.rs:360-364`) sends a label NAME where Gmail wants an ID: a 400, not a
  clean error.
- **Parameterize `create_label_if_missing` visibility.** It hardcodes
  `label_list_visibility: "labelShow"` and `message_list_visibility: "show"`
  (`src/gmail/label.rs:76-77`). The marker needs `labelHide`/`hide` or it puts a chip
  on nearly every message.
- Add `FilterAction::Tag(String)`: `batch_modify(ids, &[resolved_id], &[])`,
  identical in shape to `Star` but with a resolved custom label id. **Add an explicit
  arm in `deserialize_actions`**: the `other => FilterAction::Move(other.to_string())`
  catch-all (`src/cfg/filter.rs:271` and `:281`) will otherwise silently parse `Tag`
  as a Move to a label called "Tag".
- **The scoping rule is PER ACTION, not per "action".** `Star` and `Flag` are pins:
  every consumer reads them at thread level, so a second one in a thread carries no
  information and only costs another gesture. `Move` is a relocation: it must apply to
  every matched message or messages get stranded.

  | action | scope | rule |
  |---|---|---|
  | `Star`, `Flag` | thread | skip entirely if the thread's label union already carries the label; else apply to exactly ONE message, the newest matched in that thread |
  | `Move` | message | apply to the FULL matched set, unchanged from today. Never suppressed, never limited to one per thread |

  **Why `Move` must be excluded, explicitly, because this is the trap:** `Move` is
  `batch_modify(ids, [dest], ["INBOX","UNREAD"])` over all ids (`src/engine.rs:359-379`),
  and `bots` in the live config is `action: Bots` (`tatari.yml:19`), parsed to
  `Move("Bots")` by the catch-all at `src/cfg/filter.rs:267`. Apply the thread rule to it
  and a new bot message in a thread whose older message already carries `Bots` has its
  move SKIPPED and its marker STAMPED: stranded in the inbox permanently and never
  eligible again. That is worse than the bug this doc fixes, and it partly undoes 26fa4ee.
- **The suppression check needs a `get_thread`, which this phase does not currently do.**
  The check is on the thread's label UNION, and the residual star may sit on a sibling
  that was never fetched because it is read or does not match. The phase only builds
  `HashMap<String, GmailMessage>` from `get_message` (`src/engine.rs:235`), and
  `GmailMessage` carries only its own `thread_id` and `label_ids`
  (`src/gmail/message.rs:8-18`). So: one `client.get_thread` (`src/gmail/client.rs:224`)
  per DISTINCT matched `thread_id`, cached for the run, for `Star` | `Flag` filters only.
- **Multi-action filters:** `deserialize_actions` accepts a sequence
  (`src/cfg/filter.rs:275-284`), so `action: [Star, Bots]` is legal, and actions apply in
  declared order over the SAME matched set (`src/engine.rs:307-310`). The scoping table
  applies independently per action: the `Star` half is thread-scoped and suppressible, the
  `Move` half still takes the full matched set. No live filter uses more than one action
  today, but the config allows it, which is why the `Move`-must-be-last constraint below
  exists rather than being left to convention.
- Suppression also stops a new reply from adding another star to a thread that is still
  pinned, and removes any dependence on which message Gmail's thread-list star toggle
  happens to clear.
- Append `-label:<marker>` to every compiled message-filter query; ALSO enforce the
  marker in `MessageFilter::matches`, because the query is only a prefilter.
- **Constrain the config so the marker's carrier is STATIC: at most one `Move` per
  filter, and if a `Move` is present it MUST be the last action.** Fail loudly at config
  load with a named error. This is what makes the rest implementable; see the rejected
  alternative below.
- **The marker goes on the filter's LAST write, which the constraint above makes knowable
  from `filter.actions` alone, with no per-message lookahead:**
  - filter HAS a `Move`: fold the marker into the `Move` write, which is both last and
    applied to every matched message. `batch_modify(ids, [dest_id, marker_id],
    ["INBOX","UNREAD"])`. No separate marker write.
  - filter has NO `Move`: issue the pin writes, then ONE marker-only `batch_modify`
    covering every matched message.

  Invariant in both cases: **the marker is the last write, and where an action destroys
  future eligibility it is atomic with that action.**

  Partial-failure convergence, worked rather than asserted. `[Star, Bots]` where `Star`
  succeeds and the `Move`+marker write fails: the message is left starred, unread, in the
  inbox, unmarked. Next run, `Star` suppresses because the thread now carries `STARRED`,
  `Move` applies, the marker rides it. Converged, and the transient state is visible in the
  inbox rather than silent. For a pin-only filter, no failure removes `INBOX` or `UNREAD`,
  so every failure path leaves the message eligible and retries. `Move` is the only action that
  destroys eligibility (it strips `INBOX` and `UNREAD`, `src/engine.rs:359-378`, and every
  query carries `is:unread` plus, for `bots`, `label:inbox`). Pins do not, so a failed
  marker-only write leaves the message unread and in the inbox and the next run retries.

  **Rejected: "fold into the last APPLIED action", which an earlier draft of this doc
  specified.** Review killed it on two counts, both correct:
  1. It is mechanically impossible in the current engine without a restructure the doc
     never specified. The loop streams `for action in &filter.actions { apply(..).await? }`
     (`src/engine.rs:307-311`), so at `Move` time it cannot know whether a later `Star`
     will apply or be suppressed for a given message. Knowing the last APPLIED action per
     message requires evaluating every action's suppression before issuing any write, i.e.
     a plan-then-write restructure.
  2. It reproduced the very orphan it existed to prevent, in the reverse action order. For
     `action: [Bots, Star]` with both applying, `Star` is the last applied write and would
     carry the marker; `Move` succeeds, `Star` fails, `?` aborts (`src/engine.rs:311`), and
     the message is now archived and read, so no future run sees it. Marker lost forever.

  **Rejected: allow any action order and add a transaction strategy.** There is no
  cross-call transaction in the Gmail API, and `[Bots, Star]` is incoherent anyway: it
  stars a message it has just archived, which the digest cannot show because the digest
  queries `in:inbox is:starred` (`src/lib.rs:114-121`). Forbidding the order costs nothing
  real and makes the orphan unrepresentable rather than handled.
- **Exactly what the validator accepts and rejects**, including the degenerate cases a
  validator typically gets wrong:

  | `action:` | verdict | why |
  |---|---|---|
  | `Bots` (bare string) | ACCEPT | one `Move`, trivially in final position: it is the only action |
  | `Star` | ACCEPT | no `Move` |
  | `[Star, Bots]` | ACCEPT | one `Move`, last |
  | `[Star, Flag]` | ACCEPT | no `Move`, so no constraint applies |
  | `[Bots, Star]` | REJECT | `Move` is not last, which is the orphan case |
  | `[Bots, Bots]` | REJECT | two `Move`s. "Last applied" is technically well-defined here but the config is a mistake, and one message cannot coherently land in two destinations |
  | `[Bots, Purgatory]` | REJECT | same rule, two `Move`s to different destinations |

  Rule as implemented: count `FilterAction::Move` in `filter.actions`; reject if the count
  exceeds 1; reject if the count is 1 and its index is not `len - 1`. The error names the
  filter and which clause failed.
- **Success criteria for the constraint:** `[Bots, Star]` and `[Bots, Bots]` each FAIL to
  load with a named error; bare `Bots`, `[Star, Bots]` and `[Star, Flag]` each load.

  **Why atomic and not "after", which is what an earlier draft of this doc said:**
  "stamp after, and a failed stamp just retries next run" is FALSE for `Move`. A
  successful `Move` removes both `INBOX` and `UNREAD`, and every compiled query carries
  `is:unread` (`src/gmail/query.rs:39-41`) plus, for `bots`, `label:inbox`
  (`tatari.yml:19`), and the Rust loop independently skips read messages
  (`src/engine.rs:261`). The moved message is therefore excluded from its own filter's
  query twice over: **there is no next run for it.** The action is not lost, it already
  succeeded, but the MARKER is lost forever, which silently breaks the promise that
  pulling a `Bots`-moved message back into the inbox will not get it re-moved. One
  `batch_modify` cannot half-apply, so folding removes the failure mode instead of
  documenting it. Write counts are in Performance below: it removes the extra write for
  ACTED messages, and for fully suppressed ones the marker-only write REPLACES a pin
  write rather than adding to it.
- Correct the two misleading COMMENTS: `src/gmail/query.rs:38` and `src/engine.rs:265`
  say `is:unread` prevents re-labeling. It is a scope constraint, not an idempotency
  guard.
- **Make `--dry-run` `run`-only, and fix its two user-facing strings.** This is a real
  safety defect, not wording, and it is in scope because this doc's own rollout step 3
  instructs the operator to run `--dry-run`:
  - `--dry-run` is declared `global = true` (`src/cli.rs:28-30`) so every subcommand
    ACCEPTS it, but `cli.dry_run` is read at exactly one place, `src/main.rs:57`, inside
    `cmd_run`. Every other subcommand silently ignores it. So
    **`eratosthenes --dry-run digest` posts a live digest to Slack** (`src/lib.rs:151-154`,
    and the live config has a `slack` block at `tatari.yml:6-10`), and
    `--dry-run service install` writes unit files and calls `systemctl`
    (`src/service.rs:307-320,353-361`). A silently-ignored safety flag is the
    fail-loudly-fail-closed rule inverted.
  - Fix: **MOVE `dry_run` out of the top-level `Cli` struct and into the `Run` variant of
    `Command`** (`src/cli.rs:34-40`). Dropping `global = true` is NOT sufficient and an
    earlier draft of this doc said it was: `dry_run` sits on the top-level struct
    (`src/cli.rs:28-30`), so `eratosthenes --dry-run digest` parses the flag as a
    top-level argument and is accepted no matter what `global` says. Verified on the
    installed binary: `--dry-run digest` and `digest --dry-run` are BOTH accepted today.
    `global = true` only adds the second position. Only moving the field into the `Run`
    variant makes clap reject the flag for other subcommands.
  - Accepted cost of that fix: `run` is the default subcommand when none is given
    (`src/main.rs:276`), so bare `eratosthenes --dry-run` stops working and becomes
    `eratosthenes run --dry-run`. That is a deliberate ergonomics loss in exchange for a
    safety flag that cannot be silently ignored. Every invocation in this doc already
    names `run` explicitly.
  - Rejected the alternative of threading `dry_run` into every subcommand: more surface,
    and a dry `auth login` or `service install` has no sensible meaning.
  - **SECOND, INDEPENDENT change, also required: reword the strings.** Moving the field
    does NOT make `run --dry-run` read-only. `ensure_labels` stays unguarded
    (`src/engine.rs:33` calling `src/gmail/label.rs:83`), so a dry run still creates
    missing labels. `src/engine.rs:30` prints `=== DRY RUN - no changes will be made ===`
    and `src/cli.rs:28` reads `Dry run - show what would be done without making changes`.
    Both are false even for `run`, and stay false after the field moves. Accurate wording:
    **"no message or thread changes; missing labels may be created"**. Do not treat the
    two changes as one: the move fixes WHICH subcommands honor the flag, the reword fixes
    what the flag CLAIMS. Shipping either alone leaves a false statement in front of the
    operator.
  - Gmail write audit, for the record, so nobody re-derives it: `batch_modify`
    (`src/engine.rs:342,354,372`), `modify_thread` (`:173,546`) and `trash_thread` (`:556`)
    are all correctly behind `if !dry_run`. `labels_create` via `create_label_if_missing`
    (`src/gmail/label.rs:62-93`) is the ONLY unguarded Gmail write. `modify_message`
    (`src/gmail/client.rs:263-292`) is write-capable but unused.
- **Success criteria:** a test with a message carrying the marker and NOT carrying
  `STARRED` (the unstarred-by-user case) asserts no `STARRED` add is issued; a test
  with two matching messages in one thread asserts exactly one `STARRED` add; a test
  where the thread's existing `STARRED` sits on a NON-MATCHED sibling (read, or failing
  the address criteria) asserts the action is suppressed, which is what proves the
  per-thread `get_thread` is required rather than an optimization; a config with
  `marker-label: Oblivion` FAILS to load with a named error; and the IMPORTANT twin
  of the first test, a marked message NOT carrying `IMPORTANT` against a `Flag` filter,
  asserts no `IMPORTANT` add. `Star` coverage does not imply `Flag` coverage: separate
  arm (`src/engine.rs:347`), separate suppression path.
  (The live `Done: 0` assertion belongs to Phase 5: this phase's own plan says it
  causes one final re-pin wave, so it cannot report 0 until markers are backfilled.)
- Note: appending `-label:<marker>` breaks the two exact-match query tests
  (`src/gmail/query.rs:100`, `:117`). Update them; they are correct to fail.

#### Phase 5: Marker backfill mode, example config, README
**Model:** sonnet
- `eratosthenes run --mark-only [accounts]`: stamp the marker on the set a normal run
  would HANDLE (post-Rust-match, post-claim, deduped by message id), apply NO actions.
  NOT the set each filter's query returns: queries deliberately overfetch
  (`only-me-ttv` returns 70 candidates for 8 matches), so stamping query hits would
  freeze mail no filter would ever touch. A MODE of `run`, not a new
  subcommand: it reuses account discovery and the whole matching path, and a
  one-shot migration should not leave a permanent verb behind. Without this, the
  first run after Phase 4 re-pins the existing 35-message set one last time,
  including anything already deliberately unstarred.
- Update `eratosthenes.example.yml` with an annotated `marker-label`; update README.
- `--mark-only` stamps exactly the set a normal run would HANDLE: the unique
  post-match, post-claim message ids. Not query candidates, and NOT the smaller set a
  run would have ACTED on (under suppression a run handles every matched message but
  stars at most one per thread, so the acted set is strictly smaller than the stamp set).
- It LOGS every message it stamps at INFO: id, date, from, subject. The stamp is
  irreversible in effect (a stamped message is never handled again), so the operator
  needs a list to undo a wrong freeze by hand.
- **Success criteria:** `run --mark-only` issues ZERO `STARRED` | `IMPORTANT` | `Move`
  modifications (assert on the fake's recorded calls) and stamps N messages while
  logging exactly N lines; a normal run afterwards reports
  `Done: 0 messages matched filters`; `eratosthenes.example.yml` documents `marker-label`.

Phases 4 and 5 ship in the same release. Phase 4 alone causes one final re-pin wave.
Phase 3 must precede Phase 4 or the marker exclusion silently never fires.

## Acceptance Criteria

These five are what the implementation audit verifies end to end. Per-phase asserts live
with their phases above and are not repeated here.

- [ ] For every filter in a `--log-level debug` run, `matched` <= `query returned`.
      **Observed on main:** FAILS. Run at 2026-09-03 18:25:55 UTC:
      `[filter:leadership] query returned 0 candidates` then
      `[filter:leadership] 20 matched (total claimed: 20)`. Cite by timestamp, not line
      number: `tatari.log` rotates daily.
- [ ] Removing a pin is permanent, tested on BOTH arms: unstarring a message that
      matches a `Star` filter, AND removing IMPORTANT from one that matches a `Flag`
      filter, each survive two timer intervals. Both arms are required because the Goal
      says "a star OR an important flag" and `Flag` is a separate action arm
      (`src/engine.rs:347`) with its own suppression path, so `Star` coverage does not
      imply it.
      **Observed on main:** FAILS on both, by PROXY not by direct test. The direct test
      mutates the live mailbox, so it was not run. The proxy:
      `[filter:only-me-ttv] flagging 8 messages as important` on every run against
      `is:important in:inbox is:unread` = 8, and `[filter:leadership] starring 20`
      against a stable starred set, the same sets re-labeled indefinitely. Whoever
      implements this SHOULD run both direct tests once at rollout step 6.
- [ ] Two consecutive runs with no new mail: the second reports
      `Done: 0 messages matched filters`.
      **Observed on main:** FAILS. `grep -c "Done: 0 messages matched filters"` returns
      `0` across all 125 runs logged on 2026-09-03; the only values seen are 35, 36, 37
      (and 21 from this doc's own Phase 0 dry-run).
- [ ] `Move` is NEITHER suppressed NOR limited to one message per thread: a test with
      two matched bot messages in one thread asserts BOTH leave the inbox.
      **Observed on main:** PASSES today by default (no suppression exists). This is a
      regression guard for the rule added in Phase 4, not a defect on `main`.
- [ ] Digest counts equal the digest's OWN query, which since cb472b9 is
      `in:inbox is:starred -label:purgatory -label:oblivion` (`src/lib.rs:113-121`),
      not bare `in:inbox is:starred` (non-regression).
      **Observed on main:** PASSES. Digest reported "38 starred, 9 important";
      `is:starred in:inbox` = 38 threads and `is:important in:inbox` = 9 threads.
      The two queries agree today only because nothing pinned is staged. Verified with
      POSITIVE checks, since this doc also establishes that Gmail negation is unreliable
      at thread level: `is:starred label:Purgatory` = 0, `is:important label:Purgatory` = 0,
      `is:starred label:Oblivion` = 0, `is:important label:Oblivion` = 0. Re-measure if
      any of those stops being 0.

## Resolved Decisions

- **2026-09-03: per-filter `labels.excluded` rejected.** Measured to newly flag 21
  messages Important via fall-through (Phase 0 dry-run). One global marker instead.
- **2026-09-03: Gmail as the ledger, not a local store.** No durable store exists
  today: `Cargo.toml` has no rusqlite/jsonl/taskstore, `xdg_data_dir()`
  (`src/cfg/config.rs:29`) is used only for logs (`src/logging.rs:95`), and
  `~/.local/share/eratosthenes/` holds only `logs/`. A marker label adds zero
  dependencies, travels with the mailbox rather than the machine (tokens are
  per-machine, the mailbox is not), and is inspectable and clearable in the Gmail UI.
- **2026-09-03: markers are per-message, not per-thread.** Actions apply per message
  via `batch_modify`; digest and state-filters read live label state at thread level
  and need no marker awareness.
- **2026-09-03: marker means "HANDLED", not "acted on" and not "considered".** Stamped
  on every matched message, including ones whose action was suppressed. Stamping only
  the acted-on message leaves its older matching siblings eligible and the loop returns
  one message along; stamping everything considered would freeze mail no filter matched.
  Unmatched messages stay unmarked so a later config change still reaches old mail.
  **Do not "restore" this to "acted on": that reintroduces the loop.**
- **2026-09-03: no run lock needed.** `eratosthenes.timer` uses `OnUnitActiveSec=5min`
  on a `Type=oneshot` unit, so systemd will not run two timer-triggered instances
  concurrently; observed spacing is 5-6min (07:10:50, 07:16:50, 07:22:50 ... and
  15:00:17, 15:05:17, 15:10:18) against 3m29s-4m05s runs, and there are 0 `ERROR`
  and 0 `429` lines in the current log. A MANUAL run alongside the timer DOES
  overlap (observed: 15:15:18 timer and 15:15:30 manual), and under the marker
  design that is idempotent: a double stamp is a no-op.
- **2026-09-03: `is:unread` stays.** It is a scope constraint. Its comment is wrong
  and gets corrected; its behavior does not change.
- **2026-09-03: marker label name is flat (`Triaged`), not nested.** A `/` in the
  name would raise a Gmail query-escaping question for `-label:parent/child`.
  A flat name removes the question instead of testing it, and matches the existing
  Oblivion | Purgatory | Bots convention.
- **2026-09-03: the marker is the filter's LAST write, and `Move` is validated to be the
  filter's last action. Supersedes BOTH earlier decisions on this.** History, because both
  rejected forms look reasonable:
  1. "Stamp after actions succeed" is FALSE for `Move`. A moved message loses `INBOX` and
     `UNREAD` and is excluded from its own query twice over, so there is no next run and
     the marker is lost.
  2. "Fold into the last APPLIED action" is worse: unimplementable in the streaming action
     loop (`src/engine.rs:307-311`) without a plan-then-write restructure, AND it
     reproduces the same orphan for `action: [Bots, Star]` where `Move` succeeds and
     `Star` fails.
  Current rule: at most one `Move` per filter, validated LAST; the marker folds into the
  `Move` write when there is one, otherwise a single marker-only write follows the pins.
  The carrier is then knowable from `filter.actions` alone. Do NOT restore either
  rejected form.
- **2026-09-03: marker exclusion suppresses the CLAIM, not just the action, and that is
  safe.** A marked message fails `matches()`, so it is never claimed and falls through
  to later filters. That is exactly the mechanism that made per-filter `excluded`
  harmful. It is safe here for one reason: the exclusion is UNIFORM, so every later
  filter rejects the message for the same reason. Any future per-filter exclusion
  reintroduces the hazard, which is why the fall-through regression test exists.
- **2026-09-03: Gmail queries stay deliberately broad; globs are matched only in Rust.**
  Gmail does not glob: `from:(*@tatari.tv)` returns 193 unread inbox messages by
  treating the `*` as noise. The query is a prefilter, `AddressFilter::matches`
  (`src/cfg/filter.rs:58-70`, globset) is authoritative. Per-filter isolation does NOT
  mean trusting the query to be precise.
- **2026-09-03: `Cull` is correct. Audited exhaustively, not sampled.** Review called
  the first version of this decision an overreach and was right: it rested on 8 threads
  found via a negated query, and `GmailThread::is_read()` is the LAST message
  (`src/gmail/message.rs:90-92`), not any message, so the query did not mean what I
  thought. Redone properly: all **418** inbox threads fetched, and Cull's exact predicate
  computed per thread from per-message `labelIds` with no negation
  (union carries STARRED or IMPORTANT -> Keep; else ttl = last-message-read ? 3d : 21d;
  compare `age >= ttl` per `src/cfg/state.rs:144`). Result: **47 protected, 368 within
  TTL, 3 at exactly 21.0d having just crossed the boundary, 0 wrongly retained.**
  Corroborated live: the last five runs transitioned 2, 2, 1, 4 and 3 threads. Cull is
  working; the old mail at the bottom of the inbox is the starred pile this doc is about.
- **2026-09-03: how message-level predicates behave on thread-returning APIs.** The
  rule, stated at the right altitude: **a message-level predicate projected into a
  thread-returning API is EXISTENTIAL. It matches the thread if ANY message matches,
  positive or negative. It proves universal thread state only if the predicate is
  uniform across the thread.** This is not specific to labels or to negation: the two
  measurements it cost me used `is:read` and `older_than:`, neither of which is a label.

  Full inventory of this codebase's Gmail query projections:

  | query | API | semantics and verdict |
  |---|---|---|
  | `-is:starred`, `is:read`, `older_than:` | `list_threads` (`src/gmail/client.rs:134`, `threads.list`) | **EXISTENTIAL, unsafe as proof of universal thread state.** Measured: `in:inbox is:read older_than:3d -is:starred` returned 8 threads, 7 of them starred. Both of this doc's wrong measurements. |
  | positive `is:starred`, `is:important`, and also `is:unread` \| `in:trash` \| `in:spam` if a state filter ever declares those labels (`src/engine.rs:571-580`; today's config declares only Starred, Important, INBOX, Purgatory, so those three are unreachable) | `list_threads`: digest (`src/lib.rs:115,119`) and `build_active_threads_query` (`src/engine.rs:565`) | **EXISTENTIAL, and that is exactly what is wanted:** "the thread has at least one pinned message". Load-bearing here: it is why a thread keeps being reported while ANY residual star remains, which is what makes "one gesture per residual star" true rather than a hope. |
  | `-label:<stage>` (purgatory, oblivion) | `list_threads`: digest `stage_exclusions` (`src/lib.rs:103,115,119`) | **CONDITIONALLY safe.** `modify_thread` (`src/gmail/client.rs:341`, `threads.modify`) labels every message, so a staged thread is normally uniform and the negation excludes it. But a fresh reply transiently breaks uniformity ("new msg = INBOX, old msgs = Purgatory", `src/engine.rs:138`), and `sanitize_stages` is what restores it. Uniform when staged, broken by a later reply, repaired by `sanitize_stages` on the next `run` (`src/engine.rs:36`). The digest does NOT sanitize, so it can observe the interim mixed state. Safe by DIRECTION, not by uniformity: see the parked finding. |
  | `-label:<marker>` | `search_messages` -> `messages_list` (`src/gmail/client.rs:69-82`, `users.messages.list`) | **SAFE.** Predicate and returned unit are both message-level, so there is no projection at all. This is what makes Phase 4's marker exclusion sound. |
  | `add_label_ids` (not text `q`) | `list_threads_by_label_ids` (`src/gmail/client.rs:180`, `threads.list`) | **Thread-level ALL-label matching across DIFFERENT messages:** "a thread matches if any message has label A AND any message has label B" (`src/engine.rs:138-140`). Load-bearing for `sanitize_stages`, untouched by this design. Listed so the inventory is complete. |

  **Operational rule, unconditional and not subject to the table:** for any claim about
  protection or culling, fetch and inspect per-message `labelIds`. A query negation is a
  prefilter, never proof. The table is for reasoning about the code's own queries; it is
  NOT a licence to shortcut verification, because judging whether the units agree is
  exactly where this doc went wrong twice.
- **2026-09-03: the marker is stamped per message for INTENT granularity, not for query
  mechanics.** Review split on this and the architect is right: a per-thread stamp would
  also work with a message-level `-label:<marker>` query, since it would simply exclude
  every message in the thread. So query safety is not a reason. The actual reason is that
  stamping only HANDLED messages leaves unmatched siblings unmarked and therefore still
  reachable by a later config change; stamping a whole thread would freeze siblings no
  filter ever matched.
- **2026-09-03: the `--mark-only` freeze trade is accepted, with a log.** Mail arriving
  while the timer is stopped gets stamped and never starred. Rejected the alternative
  (drop mark-only, accept "a minor UI annoyance of 35 messages") because after Phase 2
  the re-pin set is the WIDENED one, up to 70 for `only-me-ttv` alone: that is the
  reported bug reproduced once more at larger scale. Mitigation is a log of every
  stamped id, not a smaller freeze.
- **2026-09-03: `Star` | `Flag` apply to one message per thread.** Raised by review as
  a blocker on the marker design: a marker is message-scoped, the pin is thread-scoped,
  so a residual sibling star keeps a thread pinned and reported. Measured: 6 of 38
  starred inbox threads carry >1 star. Disposition: not a blocker, but the tool created
  it, so the tool stops creating it. One star per thread makes one gesture sufficient.
- **2026-09-03: no Gmail-UI spike needed for the star toggle.** Review wanted a spike on
  which message the thread-list star toggle clears. One-star-per-thread makes the answer
  irrelevant for all new mail, and `run --mark-only` freezes the 6 existing multi-star
  threads. Removing the dependency beats testing it.
- **2026-09-03: retroactivity loss accepted.** A marked message is never reconsidered,
  so adding a new VIP to the config no longer reaches existing unread inbox mail. That
  is the direct cost of act-once and it is the point. The escape hatch already exists
  and needs no code: delete the `Triaged` label in the Gmail UI to reset.
- **2026-09-03: custom-label resolution is in scope.** It is normally out of scope
  for a bug about re-starring, but the marker IS a custom label, so the fix does
  not work without it. Traceable to this design, not to a separate request.

## Alternatives Considered

### Alternative 1: per-filter `labels.excluded` (config only, zero code)
- **Description:** `excluded: [STARRED]` on Star filters, `excluded: [IMPORTANT]` on Flag.
- **Pros:** zero code, ships immediately, uses machinery that already works.
- **Cons:** only makes the WRITE idempotent; it does not record intent. Unstar a
  matching message and the exclusion no longer applies, so it is re-starred.
- **Why not chosen:** measured harmful. Excluded filters stop claiming, so their
  messages fall through to the broader filter: `only-me-ttv` went 8 -> 21 matched.

### Alternative 2: local ledger (sqlite or JSONL under `xdg_data_dir()`)
- **Description:** persist `(message_id, label, applied_at)` and consult it before acting.
- **Pros:** exact intent tracking, no new Gmail labels, no mailbox pollution.
- **Cons:** new dependency and new failure modes (schema, corruption, migration).
  Machine-local: a new machine re-authenticates and starts with an empty ledger,
  so the whole pinned pile gets re-applied once.
- **Why not chosen:** new infrastructure for something the mailbox can hold. Revisit
  only if a marker label proves insufficient.

### Alternative 3: mark messages read after acting
- **Description:** lean on the existing `is:unread` guard by marking acted messages read.
- **Pros:** no new label, no new config.
- **Cons:** destroys the unread signal, which is load-bearing (`Cull` uses 3d read /
  21d unread) and is how the inbox is actually read. Conflates read-state with
  processed-state, which is the original defect.
- **Why not chosen:** it is the current bug, formalized.

### Alternative 4: only act on mail newer than the last run
- **Description:** store a high-water timestamp; act only on newer messages.
- **Pros:** tiny state, bounded work per run.
- **Cons:** any missed window is missed permanently; clock and delivery-order
  sensitivity; a re-delivered or re-inboxed old message is never triaged.
- **Why not chosen:** trades a correctness bug for a silent-miss bug.

## Technical Considerations

### Dependencies

- No new crates. `create_label_if_missing` (`src/gmail/label.rs:62-93`) and
  `batch_modify` already exist.
- Gmail scope: label create + message modify, both already granted (the tool
  creates Purgatory | Oblivion | Bots today).

### Performance

- Message-filter fetch volume drops sharply: `-label:<marker>` narrows every query,
  and after backfill every currently-matching message is marked. Today `only-me-ttv`
  returns 68-70 candidates and fetches all of them every run; steady state after
  backfill is genuinely new mail only.
- The state-filter phase is untouched: still 1932 sequential `get_thread` calls
  per run. Parked, see Non-Goals.
- Write counts per matched message, exactly, since an earlier draft of this doc got this
  wrong in both directions:

  | filter shape | writes |
  |---|---|
  | has a `Move` (marker folded into the `Move` write) | 1 |
  | pins only, any suppression state | 1 marker-only write, PLUS 1 pin write if any pin actually applied |
  | `[Star, Bots]` | 2: the `Star` write, then the `Move` write carrying the marker |

  A fully suppressed pin-only filter is therefore 1 write, the marker-only one, which
  REPLACES the harmful pin write rather than adding to it.

  So the fold removes the extra write for acted messages and is write-neutral or better
  for suppressed ones. Steady state after backfill is zero writes, because the matched set
  is empty.
- NEW cost, not free: one `get_thread` per distinct matched `thread_id` for `Star` |
  `Flag` filters, to read the thread label union for suppression. Steady state is still
  near zero because the matched set is near zero after backfill, but `--mark-only` and
  any run with real matches now carry that round trip. Budgeted here so it is not a
  surprise in a phase otherwise advertised as getting cheaper.

### Security

- No credential surface change. No new secrets.
- The marker label is visible to anyone with mailbox access, which is already the
  owner only. It records that automation touched a message: appropriate, not sensitive.

### Edge cases and intended behavior

- **A new reply lands in a thread you unstarred.** The reply is a NEW message id,
  unmarked and unread, so it is starred and marked, and the thread returns to the
  digest. Intended: that is new mail from a leadership sender, not a resurrection.
- **Gmail stars per MESSAGE, every consumer reads per THREAD.** `GmailThread::label_ids()`
  unions labels across messages (`src/gmail/message.rs:69-74`), and both `evaluate_thread`
  (`src/engine.rs:448`) and the digest (`src/lib.rs:114-121`) read that union. One
  residual star keeps a thread pinned and reported. Measured across the 38 starred
  inbox threads: 17 are single-message (one gesture dismisses), 21 are multi-message,
  and **6 carry more than one star** (`1a063e5cbf4ecb5d` 3 of 4 msgs, `19fab38ffce098a1`
  2 of 4, `19f8ae254a9dd36a` 2 of 2, `19f1e55f57e7fd4b` 2 of 3, `19ef514b411c7438` 2 of 3,
  `19d275b340329799` 2 of 5). For those 6, clearing one star leaves the thread reported.
  eratosthenes MANUFACTURED that: it stars every matching message. Phase 4 stops it
  (one star per thread, plus suppression when the thread is already pinned). The existing
  ones need **one gesture per residual star, once**: `1a063e5cbf4ecb5d` carries 3 stars
  so it needs 3, the other five carry 2 each. After that, never again, because
  suppression stops a new reply from adding another and the markers stop re-starring.
- **You delete the `Triaged` label in the Gmail UI.** `create_label_if_missing`
  recreates it empty and every message becomes eligible again: one re-pin wave.
  This is the flip side of the ledger being inspectable and hand-clearable. It is
  the intended manual reset.
- **`--mark-only` freezes mail that arrives during the rollout window.** The timer is
  stopped from step 1 to step 6. Leadership mail landing inside that window is stamped
  by step 4 and never starred. This is a deliberate trade: freezing is what stops the
  widened Phase 2 scope from re-pinning up to 70 messages. The mark-only log (id, date,
  from, subject) is how a wrong freeze gets undone. Keep the window short.
- **A matching message that is already READ is never stamped by `--mark-only`**, because
  `is:unread` scopes the query. Marking it unread later makes it eligible and it gets
  starred once. Correct under the retained `is:unread` scope rule, and worth knowing.
- **You star something by hand that also matches a `Star` filter.** The filter's
  star is a no-op and it stamps the marker. Unstarring later then sticks.
- **You pull a `Bots`-moved message back into the inbox.** It carries the marker, so
  `bots` will not move it out again. Precisely: the marker is NOT load-bearing for a
  message in its normal moved state, which is already excluded twice over by the query's
  `is:unread` plus configured `label:inbox` (`src/gmail/query.rs:33-41`, `tatari.yml:16-19`)
  and by the Rust read guard (`src/engine.rs:261-266`). It becomes load-bearing ONLY here,
  where the message regains `INBOX` and `UNREAD` and would otherwise re-match. That is the
  one promise the `Move` fold protects. Today it would be re-moved on the next run,
  which is the same defect class.

### Testing Strategy

Tests must bite: break the code and prove the test fails.

Starting point: `execute_message_filters` and `apply_filter_action` have **zero test
coverage today**. Every defect in this doc shipped untested. The three engine tests
(`src/engine.rs:609,646,679`) cover only `build_active_threads_query` and `derive_stages`.

- `compile_query`: 2-pattern `from` -> `from:{a b}`; 1-pattern -> `from:(a)`;
  marker exclusion present in every compiled query.
- Scope isolation: two filters with disjoint query results; assert neither matches
  the other's ids. This test fails on current `main`.
- Act-once: a message carrying the marker and NOT carrying `STARRED` (the
  unstarred-by-user case) yields no `STARRED` add. This is the reported bug,
  pinned as a regression test.
- `labels.excluded` parse: the `Value::Mapping` arm, plus the unknown-key rejection.
- Custom-label resolution: a message carrying `Label_60`, a resolver mapping
  `Label_60 -> Oblivion`, and a filter excluding `Oblivion` must NOT match. Fails on `main`.
- Fall-through regression: the exact Phase 0 shape (heterogeneous exclusions) must
  not be reachable, since exclusion is now uniform.

### Rollout Plan

Order matters: the timer stays STOPPED until markers are backfilled, because
Phase 2 widens each filter's scope (see the table above) and an unguarded run
would star the wider set.

1. `systemctl --user stop eratosthenes.timer`.
2. Land phases 1 -> 5, one commit each, `otto ci` green per phase.
3. `eratosthenes --dry-run --log-level debug run`; read the per-filter counts and diff
   them against the table above. **Type `run`.** Until the Phase 4 flag fix lands,
   `--dry-run` is accepted by every subcommand and honored by none but `run`, so
   `--dry-run digest` would post a live digest to Slack. **`--dry-run` is not fully read-only:** `ensure_labels`
   runs unconditionally at `src/engine.rs:33`, ahead of the dry-run guard, and neither it
   nor `create_label_if_missing` takes a `dry_run` argument. So this step CREATES the
   `Triaged` label as a side effect. That is harmless (an empty hidden label) and in fact
   convenient, since the marker must be resolvable before the first `Move` folds it into
   an add-list. No message is modified.
4. `eratosthenes run --mark-only`. This FREEZES today's mailbox. It stamps the
   ACTIONABLE set, not raw query hits: post-Rust-match, post-claim, deduped by message
   id. Gmail queries deliberately overfetch (`only-me-ttv` returns 70 candidates for 8
   actual matches), so stamping query hits would freeze mail no filter would ever have
   touched. The widened scope then applies only to genuinely new mail.
5. Two live runs; assert the second reports `Done: 0 messages matched filters`.
6. Restart the timer. Unstar one message by hand; confirm it is still unstarred
   after two intervals.
7. Confirm the next Mon | Thu digest count dropped by whatever was unstarred.

### Rollback

**Phases 2 and 4 revert together or not at all.** Phase 2 permanently widens each
filter's scope (`only-me-vip` 8 -> 35 candidates, `only-me-ttv` 8 -> 70) and Phase 4's
marker is the only thing holding that back. Reverting Phase 4 alone leaves the widened
scope running unguarded every 5-6 minutes: strictly worse than today's bug.

| symptom after ship | revert | note |
|---|---|---|
| unstarring still undone | 4 AND 2 together | never 4 alone |
| wrong mail got frozen by `--mark-only` | nothing: hand-clear `Triaged` from the ids in the mark-only log | that log exists for this |
| everything re-pinned at once | someone deleted the `Triaged` label; re-run `run --mark-only` | expected reset behavior |
| marker label wrong or unwanted | stop the timer, delete `Triaged`, revert 4 AND 2 | |

**Phase 1 reverts with them.** Phase 1 widens what Gmail returns
(`only-me-vip` 0 -> 35, `leadership` 0 -> 21). Under reverted Phase 2 those hits go back
into the shared `all_ids` pool (`src/engine.rs:204`) that every filter walks
(`src/engine.rs:251`), so keeping Phase 1 while reverting 2 and 4 leaves MORE unguarded
candidates than `main` has today. The revert unit is **1, 2 and 4 together**.

Per-phase independence, stated exactly:

| phase | safe to revert alone? |
|---|---|
| 1 | NO, and it fails in BOTH directions. Revert 1 while Phase 2 is live and `from:(a b)` returns to 0 hits, so with each filter confined to its own query `leadership` and `only-me-vip` collapse to zero candidates and silently stop working. Revert 1 and 2 together and its corrected queries feed the restored `all_ids` pool unguarded. |
| 2 | Only while Phase 4 is still live. Restores pooled scope, still guarded. |
| 3 | Yes, standalone. NO while Phase 4 is live, but not because it breaks: `-label:<marker>` on `users.messages.list` IS reliable per the negation rule below, so the exclusion still works. Reverting it removes the Rust-side guard and leaves the exclusion single-sourced on the query, and `matches` goes back to comparing against `Label_60`. Keep it. |
| 4 | NO, never alone. |
| 5 | NO, not "just docs": it adds a mutating `run --mark-only` mode. Time-dependent: AFTER a successful backfill, reverting is inert because the stamps are already in Gmail. BEFORE one, reverting removes the only thing preventing the final re-pin wave. |

Full reset at any point: stop the timer, delete the `Triaged` label in Gmail, revert
phases 1, 2 and 4.

Blast radius is one repo: `scottidler/eratosthenes`. No dotfiles change is required
because `marker-label` defaults. `scottidler/dotfiles` is touched only to override
the default. No cross-repo ship order is forced.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| First post-Phase-4 run re-pins the current 35-message set | High | Med | `run --mark-only` ships with Phase 4 in the same release |
| Marker label clutters the Gmail sidebar | Med | Low | Create with `labelListVisibility: labelHide`, `messageListVisibility: hide` |
| Per-filter scope changes which filter claims a message | High | Med | Intended: it makes scope match the config. Dry-run diff per-filter counts before and after Phase 2 |
| `leadership` starts acting on the message pooling missed | High | Low | Intended: declared scope is its own query, not another filter's |
| Marker leaks into `state-filters` and blocks culling | Low | High | Marker is never a state-filter label; assert absent from `build_active_threads_query` in a test |
| Marker exclusion silently no-ops because custom labels are unresolved | High | High | Phase 3 ships first, with a test that fails on `main` |
| Marker label never registered in `ensure_labels`, so a label NAME is sent as an ID (Gmail 400) | High | Med | Phase 4 adds it to `ensure_labels`; the two-messages-one-thread test exercises the real path |
| `Tag` silently parsed as `Move("Tag")` by the `deserialize_actions` catch-all | High | Med | Explicit arm before the catch-all, plus a config-parse test |
| Marker name collides with a state-filter stage and gets stripped by `sanitize_stages` | Low | Med | Validate at config load that `marker-label` matches NO state-filter label AND no state-filter destination, not just `Move` destinations |
| Unstarring stops working again after a future refactor | Med | High | The act-once regression test is the guard, not discipline |
| Marker stamped but action failed, so the message is skipped forever | Low | Med | The marker is the filter's LAST write, and where an action destroys eligibility (`Move`, validated last) it is folded into that same `batch_modify`, which cannot half-apply. Pins do not destroy eligibility, so a failed marker-only write leaves the message unread and in the inbox and the next run retries |

## Open Questions

- None.

## References

- Live evidence: `~/.local/share/eratosthenes/logs/tatari.log:4694-4722` (pooling),
  `:4951-5004` (Phase 0 baseline), `15:21:22-15:21:31` (Phase 0 config-only dry-run).
- Config: `scottidler/dotfiles` `HOME/.config/eratosthenes/tatari.yml`, last changed 4fe94e0.
- Prior digest design: `docs/design/2026-06-06-slack-digest.md`.
- Prior attempt at THIS bug: `docs/design/2026-03-29-unread-gating-and-sanitization.md:20`.
- Related commits: 26fa4ee (make `Move` actually archive), cb472b9 (exclude staged
  threads from digest), de3705a (log flood).
- Gmail search operators: https://support.google.com/mail/answer/7190
