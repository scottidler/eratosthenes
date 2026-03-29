# Design Document: Unread Gating and Stage Sanitization

**Author:** Scott Idler
**Date:** 2026-03-29
**Status:** Implemented
**Review Passes Completed:** 3/3

## Summary

Two changes to the eratosthenes engine: (1) Phase 1 message filters only apply Star/Important to unread messages, preventing re-labeling of read emails the user has already processed. (2) A new Phase 0 sanitization pass that detects and resolves conflicting stage labels (INBOX/Purgatory/Oblivion) on threads, handling the case where a new reply arrives on an aged-off thread.

## Problem Statement

### Background

Eratosthenes runs a two-phase pipeline: Phase 1 applies Star/Important labels based on message filters, Phase 2 ages threads through INBOX -> Purgatory -> Oblivion based on state filters. Star and Important state filters with `ttl: Keep` protect labeled threads from age-off indefinitely.

### Problem

**Re-labeling loop:** When a user reads a starred VIP email and un-stars it to let it age off, the next run's Phase 1 re-matches the message and re-stars it. The user can never dismiss a message.

**Conflicting stages:** When a new reply arrives on a thread that was moved to Purgatory, Gmail adds INBOX to the new message. The thread now has both INBOX and Purgatory labels. Cull matches on INBOX and tries to re-add Purgatory. Purge matches on Purgatory and evaluates TTL against the new (fresh) message. The thread is in a dirty state.

### Goals

- Phase 1 message filters only match unread INBOX messages
- Read messages retain their Star/Important from initial application but are never re-labeled
- Users control lifecycle: read + un-star/un-important = message enters normal age-off
- Threads with conflicting stage labels are automatically cleaned up before filter evaluation
- Stage progression (INBOX -> Purgatory -> Oblivion) is derived from config, not hardcoded

### Non-Goals

- Changing the state filter semantics or TTL evaluation logic
- Automatic removal of Star/Important from read messages (user controls this)
- Supporting non-linear stage progressions (e.g., Oblivion -> INBOX)

## Proposed Solution

### Change 1: Unread Gating on Phase 1

Message filters only match unread messages. Two enforcement points:

**Server-side:** Add `is:unread` to the compiled Gmail query string. This reduces the candidate set - the API only returns unread messages.

```
# Before
to:scott.idler@tatari.tv from:(*@tatari.tv) label:inbox

# After
to:scott.idler@tatari.tv from:(*@tatari.tv) label:inbox is:unread
```

**Local validation:** Check that the message has the `UNREAD` label before running `MessageFilter::matches()`. Belt and suspenders.

**Impact on lifecycle:**

```
Email arrives (unread) -> Phase 1 stars/importants it
User reads it          -> Star/Important stay, but next run skips it (read)
User un-stars it       -> No protection from Starred state filter
Cull ages it off       -> 7d read -> Purgatory -> 3d -> Oblivion
```

If user never un-stars: thread stays forever (Starred + Keep). That's the user's choice.

### Change 2: Stage Sanitization (Phase 0)

A built-in engine pass that runs before Phase 1 and Phase 2. Not user-configured - derived automatically from the state filter definitions.

**Stage derivation:** The engine reads the state filter config and extracts the stage progression:

```yaml
state-filters:
  - Cull:
      label: INBOX        # source stage
      action: Purgatory    # destination stage
  - Purge:
      label: Purgatory     # source stage
      action: Oblivion     # destination stage
```

This produces the ordered stages: `[INBOX, Purgatory, Oblivion]`

**Conflict resolution rule:** If a thread has labels from multiple stages, keep only the earliest (most active) stage. A new message pulling a thread back to INBOX is a promotion - it overrides Purgatory/Oblivion.

| Conflict | Resolution | Reasoning |
|---|---|---|
| INBOX + Purgatory | Keep INBOX, remove Purgatory | New activity resets the thread |
| INBOX + Oblivion | Keep INBOX, remove Oblivion | New activity resets the thread |
| Purgatory + Oblivion | Keep Purgatory, remove Oblivion | Should not happen, but safe fallback |

**Implementation:**

```rust
async fn sanitize_stages(client: &GmailClient, state_filters: &[StateFilter]) -> Result<()> {
    let stages = derive_stages(state_filters);
    // stages = ["INBOX", "Purgatory", "Oblivion"]

    // For each pair of stages, find threads with both labels
    for i in 0..stages.len() {
        for j in (i + 1)..stages.len() {
            let query = format!("label:{} label:{}", stages[i], stages[j]);
            let thread_ids = client.list_threads(&query).await?;

            if !thread_ids.is_empty() {
                // Keep earlier stage (i), remove later stage (j)
                // Collect all message IDs and batch_modify
            }
        }
    }
}

fn derive_stages(state_filters: &[StateFilter]) -> Vec<String> {
    let mut stages = vec!["INBOX".to_string()];
    for filter in state_filters {
        if let StateAction::Move(dest) = &filter.action {
            if !dest.is_empty() && !stages.contains(dest) {
                stages.push(dest.clone());
            }
        }
    }
    stages
}
```

**Execution order:**

```
Phase 0: Sanitize conflicting stage labels
Phase 1: Message filters (unread only) - Star/Important
Phase 2: State filters - age-off through stages
```

### Config Changes

Normalize the state filter actions to consistent bare-string syntax:

```yaml
state-filters:
  - Starred:
      labels: [Starred]
      ttl: Keep

  - Important:
      labels: [Important]
      ttl: Keep

  - Cull:
      label: INBOX
      ttl:
        read: 7d
        unread: 21d
      action: Purgatory

  - Purge:
      label: Purgatory
      ttl: 3d
      action: Oblivion
```

### Implementation Plan

#### Phase A: Unread Gating

- Add `is:unread` to `compile_query()` output
- Add UNREAD label check before `filter.matches()` in engine
- Tests for query compilation with unread

#### Phase B: Stage Sanitization

- Add `derive_stages()` function
- Add `sanitize_stages()` async function to engine
- Call sanitize before Phase 1 in `execute()`
- Tests for stage derivation and conflict resolution logic

## Alternatives Considered

### Alternative 1: Auto-remove Star/Important from Read Messages

- **Description:** A de-escalation pass that strips Star/Important from any read message automatically.
- **Pros:** User doesn't have to manually un-star. Fully automated lifecycle.
- **Cons:** User loses the ability to keep something starred permanently. Removes user agency. What if you starred something and want it to stay starred even after reading?
- **Why not chosen:** User explicitly wants to control when Star/Important are removed. The system should not override manual choices.

### Alternative 2: Hardcode Stage Names

- **Description:** Hardcode INBOX/Purgatory/Oblivion in the sanitization pass.
- **Pros:** Simpler code.
- **Cons:** Breaks if user renames stages in config. Not composable.
- **Why not chosen:** Deriving from config is barely more code and respects the design principle of config-driven behavior.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Sanitization query returns too many threads on first run | Medium | Low | Batch modify handles up to 1000. Paginate if needed. One-time cleanup. |
| Stage derivation produces wrong order | Low | Medium | Stages are derived in config declaration order, which matches the intended progression. Add a test. |
| Unread gating means already-read unstarred emails in INBOX never get Star/Important | N/A | N/A | This is the intended behavior. Those emails already had their chance. |

## Open Questions

- [x] Should sanitization run in dry-run mode? **Yes, but only report - don't modify.** Same as other phases.
