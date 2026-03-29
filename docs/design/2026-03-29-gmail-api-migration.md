# Design Document: Eratosthenes - Gmail API-Native Inbox Zero Engine

**Author:** Scott Idler
**Date:** 2026-03-29
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Eratosthenes is a rewrite of `imap-filter-rs-v2` that replaces the IMAP transport layer with the Gmail REST API (`google-gmail1` + `yup-oauth2` + `tokio`). The YAML configuration DSL and its Rust deserialization layer are preserved wholesale. The IMAP protocol code, RFC822 header parsing, and BFS thread-graph construction are deleted and replaced by stateless HTTP/JSON calls to Google's servers.

## Problem Statement

### Background

`imap-filter-rs-v2` is a production email filtering tool that provides capabilities Gmail's native filters lack: "only me" sole-recipient detection, TTL-based message expiry, thread-aware state transitions, and a multi-stage purgatory workflow. It connects to Gmail via IMAP, downloads raw RFC822 headers, parses them locally, builds thread graphs from `Message-ID`/`In-Reply-To`/`References` headers, and applies IMAP `STORE`/`MOVE` commands.

### Problem

The IMAP approach has three fundamental problems:

1. **Parsing fragility** - Gmail's `X-GM-THRID` attribute crashes the `rust-imap` crate's Nom parser, forcing regex extraction from raw FETCH responses. Any malformed header in any email can crash the entire execution loop.

2. **Inaccurate threading** - Standard email headers (`In-Reply-To`, `References`) diverge from Gmail's internal threading. BFS graph construction produces different thread groupings than what users see in the Gmail UI, making "age-off" logic inaccurate.

3. **Stateful connection brittleness** - IMAP holds a stateful TCP session. Disconnects, rate limits, and server-side timeouts require reconnection logic. A single malformed email can take down the session.

### Goals

- Replace IMAP transport with Gmail REST API (`google-gmail1`)
- Preserve the existing YAML config DSL and Rust deserialization (`cfg/` module)
- Preserve the two-phase filter architecture (MessageFilters then StateFilters)
- Achieve accurate thread age-off using Gmail's native `Thread` objects
- Implement the "only me" filter reliably via hybrid server-side query + local validation
- Provide a slick OAuth2 browser-based authentication flow
- Build rate-limit-aware API access with exponential backoff

### Non-Goals

- Supporting non-Gmail IMAP servers (the old codebase supported generic IMAP - we are going Gmail-only)
- Changing the config file format or filter semantics
- Building a daemon/service mode (this remains a run-once CLI tool)
- Gmail push notifications or watch/sync (we poll on demand)
- Email body content matching (we match on headers and metadata only, same as before)

## Proposed Solution

### Overview

The system has three layers:

1. **Config layer** (`cfg/`) - Ported from `imap-filter-rs-v2` with minimal changes. Deserializes `eratosthenes.yml` into `MessageFilter` and `StateFilter` structs.

2. **Gmail transport layer** (`gmail/`) - New module wrapping `google-gmail1`. Handles OAuth2 auth, API calls, rate limiting, and query compilation.

3. **Engine layer** (`engine.rs`) - Async two-phase execution loop replacing `imap_filter.rs`. Orchestrates query filters (Phase 1) and state/age-off filters (Phase 2).

### Architecture

```
eratosthenes.yml
      |
      v
 ┌─────────┐     ┌──────────────────────────────────────┐
 │ cfg/     │     │ gmail/                                │
 │          │     │                                       │
 │ config   │     │ auth ─── yup-oauth2                   │
 │ filter   │────>│ client ─ google-gmail1 Hub wrapper    │
 │ state    │     │ query ── MessageFilter -> q: string   │
 │ label    │     │ rate ─── quota-aware rate limiter      │
 └─────────┘     └──────────────────────────────────────┘
      |                          |
      v                          v
 ┌─────────────────────────────────┐
 │ engine                           │
 │                                  │
 │ Phase 1: Query + local validate  │
 │ Phase 2: Thread age-off          │
 └─────────────────────────────────┘
```

### Data Model

#### Preserved from imap-filter-rs-v2

These structs are ported with minimal changes:

```rust
// cfg/label.rs - unchanged
pub enum Label {
    Inbox, Important, Starred, Sent, Draft, Trash, Spam,
    Custom(String),
}

// cfg/filter.rs - unchanged
pub struct MessageFilter {
    pub name: String,
    pub to: Option<AddressFilter>,
    pub cc: Option<AddressFilter>,
    pub from: Option<AddressFilter>,
    pub subject: Vec<String>,
    pub labels: LabelsFilter,
    pub headers: HashMap<String, Vec<String>>,
    pub actions: Vec<FilterAction>,
}

pub enum FilterAction { Star, Flag, Move(String) }

// cfg/state.rs - unchanged
pub struct StateFilter {
    pub name: String,
    pub labels: Vec<Label>,
    pub ttl: Ttl,
    pub action: StateAction,
}

pub enum Ttl {
    Keep,
    Days(chrono::Duration),
    Detailed { read: chrono::Duration, unread: chrono::Duration },
}

pub enum StateAction { Move(String), Delete }
```

#### New: Gmail Message Representation

Replaces the old `Message` struct that parsed RFC822 headers:

```rust
// gmail/message.rs
pub struct GmailMessage {
    pub id: String,
    pub thread_id: String,
    pub label_ids: Vec<String>,
    pub internal_date: DateTime<Utc>,  // from epoch millis
    pub headers: HashMap<String, String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub from: Vec<String>,
    pub subject: String,
}
```

Constructed from the Gmail API `Message` struct (fetched with `format=METADATA`, `metadata_headers=["To","Cc","From","Subject","List-Id"]`):

```rust
impl GmailMessage {
    pub fn from_api(msg: google_gmail1::api::Message, resolver: &LabelResolver) -> Result<Self> {
        let headers: HashMap<String, String> = msg.payload
            .and_then(|p| p.headers)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|h| Some((h.name?, h.value?)))
            .collect();

        Ok(Self {
            id: msg.id.ok_or_else(|| eyre!("message missing id"))?,
            thread_id: msg.thread_id.ok_or_else(|| eyre!("message missing thread_id"))?,
            label_ids: msg.label_ids.unwrap_or_default(),
            internal_date: DateTime::from_timestamp_millis(
                msg.internal_date.ok_or_else(|| eyre!("missing internal_date"))?
            ).ok_or_else(|| eyre!("invalid timestamp"))?,
            to: parse_address_header(headers.get("To")),
            cc: parse_address_header(headers.get("Cc")),
            from: parse_address_header(headers.get("From")),
            subject: headers.get("Subject").cloned().unwrap_or_default(),
            headers,
        })
    }
}
```

No RFC822 body parsing - just structured header extraction from JSON.

#### New: Gmail Thread Representation

```rust
pub struct GmailThread {
    pub id: String,
    pub messages: Vec<GmailMessage>,
}

impl GmailThread {
    /// Last message's internal_date - the canonical "last activity" time.
    pub fn last_activity(&self) -> Option<DateTime<Utc>> {
        self.messages.last().map(|m| m.internal_date)
    }

    /// Union of all label IDs across all messages in the thread.
    pub fn label_ids(&self) -> HashSet<String> {
        self.messages.iter()
            .flat_map(|m| m.label_ids.iter().cloned())
            .collect()
    }

    /// True if the last message in the thread has been read (lacks UNREAD label).
    /// For TTL evaluation, only the last message's read state matters.
    pub fn is_read(&self) -> bool {
        self.messages.last()
            .map(|m| !m.label_ids.contains(&"UNREAD".to_string()))
            .unwrap_or(false)
    }
}
```

#### Config Changes

The top-level `Config` struct changes to replace IMAP fields with auth fields:

```yaml
# eratosthenes.yml
auth:
  client-secret-path: "~/.config/eratosthenes/client-secret.json"
  token-cache-path: "~/.config/eratosthenes/tokencache.json"
  # callback-port: 13131  # optional, default 13131

message-filters:
  - only-me-star:
      to: ['scott.idler@tatari.tv']
      cc: []
      from: '*@tatari.tv'
      label: INBOX
      action: Star

  - only-me:
      to: ['scott.idler@tatari.tv']
      from: '*@tatari.tv'
      label: INBOX
      action: Flag

state-filters:
  - Starred:
      labels: [Important, Starred]
      ttl: Keep

  - Important:
      label: Important
      ttl: Keep

  - Cull:
      ttl:
        read: 7d
        unread: 21d
      action: Purgatory

  - Purge:
      label: Purgatory
      ttl: 3d
      action:
        Move: Oblivion
```

The `Config` struct becomes:

```rust
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Config {
    pub auth: AuthConfig,

    #[serde(rename = "message-filters")]
    #[serde(deserialize_with = "deserialize_named_filters")]
    pub message_filters: Vec<MessageFilter>,

    #[serde(rename = "state-filters")]
    #[serde(deserialize_with = "deserialize_named_states")]
    pub state_filters: Vec<StateFilter>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AuthConfig {
    pub client_secret_path: PathBuf,
    pub token_cache_path: PathBuf,
    #[serde(default = "default_callback_port")]
    pub callback_port: u16,  // default: 13131
}
```

### The "Only Me" Hybrid Strategy

Gmail search operators cannot reliably detect sole-recipient messages. Specifically:

- No wildcard negation: `-cc:*` is not supported
- No recipient count assertion: can't express "To has exactly 1 address"
- `bcc:` only applies to messages you sent, not received

**Solution: Two-pass approach.**

**Pass 1 (server-side narrowing):** Compile the `MessageFilter` into a best-effort Gmail query string to reduce the result set. For the "only-me-star" filter above:

```
to:scott.idler@tatari.tv from:(*@tatari.tv) label:inbox
```

This might return 200 messages instead of 10,000.

**Pass 2 (local validation):** For each returned message, fetch headers and run the existing `MessageFilter::matches()` logic. The `cc: []` (require empty CC) is enforced locally by checking the parsed `Cc` header is empty.

The query compilation module (`gmail/query.rs`) translates structured filter fields to Gmail search syntax:

| Filter field | Gmail query |
|---|---|
| `to: ['addr']` | `to:addr` |
| `from: '*@domain'` | `from:(*@domain)` |
| `label: INBOX` | `label:inbox` |
| `subject: ['*urgent*']` | `subject:(urgent)` |
| `cc: []` | *(cannot express - skip, enforce locally)* |
| `headers: { "List-Id": [] }` | *(cannot express - skip, enforce locally)* |

### OAuth2 Authentication Flow

Uses `yup-oauth2::InstalledFlowAuthenticator` which provides the same pattern as `okta-auth-rs`:

1. Read `client-secret.json` from Google Cloud Console
2. Build authenticator with `HTTPPortRedirect(13131)` - opens browser, runs local callback server
3. Token cached to disk at `token-cache-path`
4. On subsequent runs: cache hit -> use token; expired -> auto-refresh; refresh fails -> re-open browser

```rust
// gmail/auth.rs
pub async fn build_auth(config: &AuthConfig) -> Result<Authenticator<HttpsConnector<HttpConnector>>> {
    let secret = read_application_secret(&config.client_secret_path).await?;
    let auth = InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPPortRedirect(13131))
        .persist_tokens_to_disk(&config.token_cache_path)
        .build()
        .await?;
    Ok(auth)
}
```

Required OAuth2 scope: `https://www.googleapis.com/auth/gmail.modify`

### Gmail Client Wrapper

Thin shim around `google-gmail1::Gmail` Hub to provide ergonomic methods and handle Option-heavy API types:

```rust
// gmail/client.rs
pub struct GmailClient {
    hub: Gmail<HttpsConnector<HttpConnector>>,
    limiter: RateLimiter,
}

impl GmailClient {
    /// List message IDs matching a query string.
    pub async fn search_messages(&self, query: &str) -> Result<Vec<String>>;

    /// Fetch a single message with headers (metadata format).
    pub async fn get_message(&self, id: &str) -> Result<GmailMessage>;

    /// List thread IDs matching a query string.
    pub async fn list_threads(&self, query: &str) -> Result<Vec<String>>;

    /// Fetch a full thread with all messages (metadata format).
    pub async fn get_thread(&self, id: &str) -> Result<GmailThread>;

    /// Add/remove labels on a single message.
    pub async fn modify_message(&self, id: &str, add: &[&str], remove: &[&str]) -> Result<()>;

    /// Batch modify labels on up to 1000 messages.
    pub async fn batch_modify(&self, ids: &[String], add: &[&str], remove: &[&str]) -> Result<()>;

    /// Trash a thread (all messages).
    pub async fn trash_thread(&self, id: &str) -> Result<()>;
}
```

All methods go through the `RateLimiter` before issuing API calls. All list methods handle pagination internally (iterating `next_page_token` until exhausted).

### Label Resolution

Gmail API uses opaque label IDs for custom labels (e.g., `Label_123` for "Purgatory"). System labels use well-known IDs (`INBOX`, `STARRED`, `IMPORTANT`, `UNREAD`, `TRASH`, `SPAM`).

At startup, the client fetches `labels.list` and builds a bidirectional map:

```rust
pub struct LabelResolver {
    name_to_id: HashMap<String, String>,
    id_to_name: HashMap<String, String>,
}
```

This maps `Label::Custom("Purgatory")` to its Gmail ID for API calls, and maps API response label IDs back to names for `MessageFilter::matches()`. System labels are hardcoded. Custom labels are resolved dynamically. Missing custom labels are auto-created via `labels.create`.

### Rate Limiter

Gmail API quota: 15,000 units/min/user (moving average, allows bursts).

```rust
// gmail/rate.rs
pub struct RateLimiter {
    tokens: AtomicU32,         // remaining quota units
    refill_rate: u32,          // units per second (250)
    last_refill: Mutex<Instant>,
}

impl RateLimiter {
    /// Wait until `cost` units are available, then consume them.
    pub async fn acquire(&self, cost: u32);

    /// Called on 429/503 response - back off exponentially.
    pub async fn backoff(&self, attempt: u32);
}
```

Quota costs:

| Operation | Units | When used |
|---|---|---|
| `messages.list` | 5 | Phase 1: search |
| `messages.get` (metadata) | 5 | Phase 1: fetch headers for local validation |
| `threads.list` | 10 | Phase 2: list active threads |
| `threads.get` | 10 | Phase 2: fetch thread for age-off eval |
| `messages.modify` | 5 | Phase 1: star/flag individual messages |
| `messages.batchModify` | 50 | Phase 2: bulk label changes (up to 1000 msgs) |

Backoff strategy: exponential with jitter, starting at 1s, max 60s, on HTTP 429 or 503.

### Execution Engine

```rust
// engine.rs
pub async fn execute(client: &GmailClient, config: &Config) -> Result<()> {
    // Phase 1: Message Filters (query + local validate)
    for filter in &config.message_filters {
        let query = compile_query(filter);
        let message_ids = client.search_messages(&query).await?;

        let mut matched_ids = Vec::new();
        for id in message_ids {
            let msg = client.get_message(&id).await?;
            if filter.matches(&msg) {
                matched_ids.push(id);
            }
        }

        apply_filter_actions(client, &matched_ids, &filter.actions).await?;
    }

    // Phase 2: State Filters (thread age-off)
    // Query covers all labels that any state filter cares about.
    // For the default config: "in:inbox OR label:purgatory"
    // This fetches threads that are candidates for any state transition.
    let active_query = build_active_threads_query(&config.state_filters);
    let thread_ids = client.list_threads(&active_query).await?;

    for thread_id in thread_ids {
        let thread = client.get_thread(&thread_id).await?;

        for state_filter in &config.state_filters {
            // thread_matches_labels: if filter.labels is empty, match all threads.
            // Otherwise, check if the union of all message labels in the thread
            // contains any of the filter's labels.
            if !thread_matches_labels(&thread, &state_filter.labels) {
                continue;
            }

            match evaluate_thread_ttl(&thread, state_filter)? {
                Some(action) => {
                    apply_state_action(client, &thread, &action).await?;
                    break;  // first matching state filter wins
                }
                None => {
                    if state_filter.ttl == Ttl::Keep {
                        break;  // protected - skip remaining filters
                    }
                }
            }
        }
    }

    Ok(())
}
```

#### Action Translation

`FilterAction` and `StateAction` translate to Gmail API label operations:

| Action | Gmail API operation |
|---|---|
| `Star` | `messages.modify`: add `STARRED` |
| `Flag` | `messages.modify`: add `IMPORTANT` |
| `Move("Purgatory")` | `messages.batchModify`: add `Purgatory`, remove source labels |
| `Move("Oblivion")` | `messages.batchModify`: add `Oblivion`, remove source labels |
| `Delete` | `messages.trash` |

**Source label inference for `Move`:** The state filter's `labels` field defines the source context. When Cull (no explicit labels, matches all) moves to Purgatory, we remove `INBOX` (the default source). When Purge (`label: Purgatory`) moves to Oblivion, we remove `Purgatory`. This is derived from the state filter's label scope, not hardcoded.

Note: Custom labels like "Purgatory" and "Oblivion" are auto-created on startup if they don't exist (`labels.create`).

#### Thread TTL Evaluation

Age-off uses the **last message** in the thread:

```rust
fn evaluate_thread_ttl(thread: &GmailThread, filter: &StateFilter) -> Result<Option<StateAction>> {
    let last_activity = match thread.last_activity() {
        Some(dt) => dt,
        None => {
            log::warn!("Thread {} has no messages, skipping", thread.id);
            return Ok(None);
        }
    };

    let age = Utc::now().signed_duration_since(last_activity);
    let is_read = thread.is_read();

    let ttl_duration = match &filter.ttl {
        Ttl::Keep => return Ok(None),
        Ttl::Days(dur) => *dur,
        Ttl::Detailed { read, unread } => if is_read { *read } else { *unread },
    };

    if age >= ttl_duration {
        Ok(Some(filter.action.clone()))
    } else {
        Ok(None)
    }
}
```

### Implementation Plan

#### Target Module Structure

```
src/
  main.rs          # thin shell: parse args, call lib
  lib.rs           # orchestration: auth -> build client -> execute engine
  cli.rs           # clap structs (Cli -> Config via TryFrom)
  engine.rs        # async two-phase execution loop
  cfg/
    mod.rs         # module exports
    config.rs      # Config, AuthConfig, load_config, named deserializers
    filter.rs      # MessageFilter, AddressFilter, LabelsFilter, FilterAction
    state.rs       # StateFilter, Ttl, StateAction, evaluate_ttl
    label.rs       # Label enum with case-insensitive normalization
  gmail/
    mod.rs         # module exports
    auth.rs        # yup-oauth2 InstalledFlowAuthenticator setup
    client.rs      # GmailClient wrapper around google-gmail1 Hub
    message.rs     # GmailMessage, GmailThread structs + from_api conversion
    query.rs       # compile MessageFilter -> Gmail q: string
    rate.rs        # token-bucket rate limiter with exponential backoff
    label.rs       # LabelResolver: name <-> ID bidirectional map
```

#### Phase 1: Project Scaffold and Config

- Scaffold Rust project with `scaffold eratosthenes`
- Add dependencies via `cargo add`
- Port `cfg/` module from `imap-filter-rs-v2` (label, filter, state, config)
- Adapt `Config` struct to replace IMAP fields with `AuthConfig`
- Add CLI (`cli.rs`) with `--config` flag and `--log-level`
- Verify: `otto ci` passes

#### Phase 2: OAuth2 and Gmail Client

- Implement `gmail/auth.rs` with `yup-oauth2` `InstalledFlowAuthenticator`
- Implement `gmail/client.rs` wrapper around `google-gmail1::Gmail`
- Implement `gmail/rate.rs` rate limiter
- Implement `GmailMessage` and `GmailThread` structs
- Verify: can authenticate and list inbox threads

#### Phase 3: Query Compilation and Phase 1 Engine

- Implement `gmail/query.rs` - compile `MessageFilter` to Gmail query string
- Adapt `MessageFilter::matches()` to accept `GmailMessage` directly (same logic, different input struct - both have `to`, `cc`, `from`, `subject`, `labels`, `headers` fields)
- Implement Phase 1 of the engine: query -> fetch -> local validate -> apply actions
- Verify: "only me" filter correctly stars direct messages

#### Phase 4: State Filters and Phase 2 Engine

- Implement Phase 2 of the engine: thread listing, age-off evaluation, action application
- Implement `batch_modify` for bulk label changes
- Implement auto-creation of custom labels (Purgatory, Oblivion)
- Verify: messages age through Inbox -> Purgatory -> Oblivion pipeline

#### Phase 5: Polish

- Structured logging with state transition messages
- Dry-run mode (`--dry-run`)
- Error handling: retry with backoff on transient errors, skip and log on permanent errors
- Comprehensive tests with mock Gmail client

## Alternatives Considered

### Alternative 1: Pure Server-Side Queries (No Local Matching)

- **Description:** Express all filter logic as Gmail search queries. Drop the local `MessageFilter::matches()` engine entirely.
- **Pros:** Simpler code. No need to fetch message headers. Fewer API calls.
- **Cons:** Cannot express "only me" (no CC/BCC negation wildcards). Cannot match custom headers. Cannot enforce exact recipient counts.
- **Why not chosen:** The #1 feature requirement - "only me" filtering - is impossible with Gmail search alone. Local validation is required.

### Alternative 2: Keep IMAP, Fix the Parser

- **Description:** Fork `rust-imap`, fix the Nom parser for X-GM-THRID, add reconnection logic.
- **Pros:** Minimal architectural change. Config and engine stay the same.
- **Cons:** Still parsing raw RFC822 headers (bandwidth, fragility). Still building BFS thread graphs (inaccurate threading). Still fighting stateful TCP sessions. The IMAP crate is on an alpha release (3.0.0-alpha.15) with no clear path to stable.
- **Why not chosen:** We'd be patching symptoms of a fundamentally wrong abstraction. The Gmail API is the correct interface for Gmail.

### Alternative 3: Use google-gmail1 but Keep Synchronous (ureq)

- **Description:** Use the Gmail REST API but with synchronous HTTP calls via `ureq` instead of async `tokio`/`hyper`.
- **Pros:** Simpler code. No async runtime. Matches the old codebase's synchronous style.
- **Cons:** `google-gmail1` requires an async runtime - it's built on `hyper` and `tokio`. Fighting this would require a different HTTP client and manual API calls.
- **Why not chosen:** The `google-gmail1` crate mandates async. Embracing `tokio` is the path of least resistance and enables concurrent API calls for performance.

## Technical Considerations

### Dependencies

| Purpose | Crate |
|---|---|
| Async runtime | `tokio` (full) |
| Gmail API | `google-gmail1` |
| OAuth2 | `yup-oauth2` |
| Config/serde | `serde`, `serde_yaml` |
| CLI | `clap` (derive) |
| Error handling | `eyre` |
| Logging | `log`, `env_logger` |
| Date/time | `chrono` |
| Glob matching | `globset` |
| Colors | `colored` |
| Directories | `dirs` |

### Performance

- **Phase 1** is dominated by `messages.get` calls for local validation. With 200 messages to validate, that's ~1,000 quota units and ~200 HTTP requests. Concurrent fetches (bounded to ~10 parallel) bring wall time under 10 seconds.
- **Phase 2** is dominated by `threads.get` calls. With 500 active threads, that's ~5,000 quota units. Well within the 15,000/min budget.
- `batchModify` handles bulk label changes efficiently: 50 quota units for up to 1,000 messages, vs 5,000 units for 1,000 individual `modify` calls.

### Security

- `client-secret.json` contains OAuth2 client credentials. Must be stored securely (0600 permissions). Should be excluded from version control.
- Token cache contains access and refresh tokens. Same file permission requirements.
- The `gmail.modify` scope grants read/write access to all email and labels but not send. This is the minimum scope for our use case.

### Testing Strategy

- **Unit tests** for `cfg/` module: ported from `imap-filter-rs-v2` (config parsing, filter matching, TTL evaluation)
- **Unit tests** for `gmail/query.rs`: query compilation correctness
- **Unit tests** for engine logic: mock `GmailClient` trait, verify correct API calls for each filter type
- **Integration test**: authenticated test against a real Gmail account (manual, not CI)

### Rollout Plan

1. Build and test locally against Scott's Gmail account
2. Run in dry-run mode to verify filter behavior matches `imap-filter-rs-v2`
3. Run live and compare results
4. Replace `imap-filter-rs-v2` in cron

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Gmail search query doesn't narrow enough (too many results to validate locally) | Medium | Medium | Monitor query selectivity. Add more query terms. Pagination handles large result sets. |
| Rate limiting under heavy use | Low | Medium | Rate limiter with backoff. batchModify for bulk ops. 15K units/min is generous for a single-user tool. |
| yup-oauth2 token refresh fails silently | Low | High | Log all auth events. Detect expired tokens and surface clear error messages. |
| Custom label creation race condition | Low | Low | Check-then-create with error handling for "already exists". |
| google-gmail1 crate API changes | Low | Medium | Pin crate version. Monitor releases. The crate is auto-generated from Google's API spec. |
| Phase 1 stars a message, Phase 2 immediately tries to age it off | None | None | Not a risk - Phase 1 runs first and applies Star/Important. Phase 2 then evaluates threads, and the Starred state filter (ttl: Keep) protects starred threads. The ordering is load-bearing and correct. |

## Open Questions

- [x] Should `FilterAction::Move` translate to "add label + remove INBOX" or just "add label"?
  **Decision:** Add destination label AND remove source label. `Move("Purgatory")` from INBOX = add `Purgatory`, remove `INBOX`. `Move("Oblivion")` from Purgatory = add `Oblivion`, remove `Purgatory`. This matches the old IMAP `MOVE` semantics.
- [x] Should we support `--login` / `--logout` CLI subcommands?
  **Decision:** Yes. `--login` forces re-authentication (clears token cache, opens browser). `--logout` clears cached tokens. Default run auto-authenticates.
- [x] What port for OAuth2 callback?
  **Decision:** Default 13131, configurable via `auth.callback-port` in config.
- [x] Auto-create missing labels?
  **Decision:** Yes. On startup, resolve all labels referenced in config. Create any missing custom labels automatically.

## References

- [Gmail API Reference](https://developers.google.com/gmail/api/reference/rest)
- [Gmail Search Operators](https://support.google.com/mail/answer/7190)
- [Gmail API Quota](https://developers.google.com/workspace/gmail/api/reference/quota)
- [google-gmail1 crate docs](https://docs.rs/google-gmail1/latest/google_gmail1/)
- [yup-oauth2 crate docs](https://docs.rs/yup-oauth2/latest/yup_oauth2/)
- [imap-filter-rs-v2 design doc](../../../imap-filter-rs-v2/docs/design.md)
- [Eratosthenes initial architecture](../gemini-architecture.md)
