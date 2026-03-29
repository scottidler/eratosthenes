# Eratosthenes: A Gmail API-Native Inbox Zero Engine

Eratosthenes is a modernized rewrite of the IMAP-based `imap-filter-rs-v2` project. By abandoning the brittle, stateful IMAP protocol and migrating to the official Google Gmail REST API (via the `google-gmail1` Rust crate), we eliminate parsing crashes (like `X-GM-THRID`), drastically simplify thread evaluation, and offload complex search queries to Google's servers.

This document outlines the architecture, design decisions, and implementation plan for the new system.

---

## 1. Core Architecture Shifts

### 1.1 From Stateful IMAP to Stateless REST
*   **Old (IMAP):** Connect to `imap.gmail.com`, hold a stateful TCP session, download raw headers (`UID FETCH RFC822.HEADER`), parse them locally, build a BFS graph to guess thread groupings, and apply IMAP `FLAGS`.
*   **New (Gmail API):** Authenticate via OAuth2, issue stateless HTTP/JSON requests. We rely completely on Google's native understanding of a "Message" and a "Thread".

### 1.2 The "Only Me" Problem Solved
*   **Old:** Download all emails, parse `To:` and `Cc:` headers locally, and write custom logic to ensure lists are empty or exactly size 1.
*   **New:** Offload the search entirely. We can query the Gmail API using standard advanced search operators.
    *   *Query:* `to:me -{cc:* bcc:*} from:(*@tatari.tv)`
    *   This guarantees that Google only returns messages where you are the sole recipient, avoiding local regex overhead entirely.

### 1.3 The "Thread Age-Off" Problem Solved
*   **Old:** Rely on standard headers (`Message-ID`, `In-Reply-To`, `References`) to build a graph because `X-GM-THRID` crashes the `rust-imap` Nom parser. This means "age-off" logic is wildly inaccurate when a thread diverges from standard headers.
*   **New:** Fetch by `ThreadID`. The Gmail API returns a `Thread` object containing an ordered list of `Message` objects.
    *   *Age Calculation:* Simply look at the `internalDate` of the *last* message in the `messages` array of the `Thread`. This is the exact, canonical "last updated" time of the conversation.

---

## 2. Dependencies & Stack

This will be a modern, async Rust application.

```toml
[dependencies]
# Async Runtime
tokio = { version = "1", features = ["full"] }

# Google Gmail API & Auth
google-gmail1 = "7.0.0"
yup-oauth2 = "12.0.0"

# Configuration & Serde
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"

# Error Handling & Logging
eyre = "0.6"
log = "0.4"
env_logger = "0.11"
```

---

## 3. Implementation Plan

### Phase 1: Authentication & Client Setup (The Hub)
The foundation of the `google-gmail1` crate is the `Hub`.
1.  **OAuth2:** Replace the IMAP password login with a `ServiceAccountAuthenticator` or an `InstalledFlowAuthenticator` (via `yup-oauth2`). This creates an HTTP connector.
2.  **Hub Initialization:** Instantiate `Gmail::new(hyper_client, authenticator)`.

### Phase 2: Configuration Redesign (`eratosthenes.yml`)
The config should focus on *intent* rather than *IMAP specifics*.

```yaml
# eratosthenes.yml
auth:
  # Path to the Google Client Secrets JSON
  client_secrets_path: "secrets.json"
  token_cache_path: "tokencache.json"

filters:
  # 1. The "Only Me" Priority Rules
  - name: "Priority: Bosses Direct"
    query: "to:me -{cc:* bcc:*} {from:boss1@tatari.tv from:ceo@tatari.tv}"
    actions:
      - AddLabel: "STARRED"
      - AddLabel: "IMPORTANT"

  - name: "Priority: Company Direct"
    query: "to:me -{cc:* bcc:*} from:*@tatari.tv"
    actions:
      - AddLabel: "IMPORTANT"

  # 2. State & Age-Off Rules (Evaluated per THREAD)
  - name: "Protect Starred"
    match_labels: ["STARRED", "IMPORTANT"]
    action: Keep

  - name: "Cull Read Inbox"
    match_labels: ["INBOX"]
    read_state: Read
    max_age_days: 7
    action:
      AddLabel: "Purgatory"
      RemoveLabel: "INBOX"

  - name: "Oblivion"
    match_labels: ["Purgatory"]
    max_age_days: 3
    action: Trash
```

### Phase 3: The Execution Loop

#### Step 1: Execute Query Filters (The "Only Me" pass)
1. Iterate over all query-based filters.
2. Call `hub.users().messages_list("me").q(&filter.query).doit().await`.
3. For each returned `Message.id`, execute the `actions` (e.g., `messages_modify` to add `STARRED`).

#### Step 2: Execute State / Age-Off Filters (The Thread pass)
1. Fetch all active threads: `hub.users().threads_list("me").q("in:inbox OR label:purgatory").doit().await`.
2. For each `Thread.id`, fetch the full thread details: `hub.users().threads_get("me", id).doit().await`.
3. **Age Calculation:** Extract the `internalDate` from the *last* item in the `Thread.messages` vector.
4. **State Evaluation:** Look at the labels applied to the thread (which are the union of the labels on its messages).
5. If the thread matches a cull rule (e.g., in Inbox, Read, older than 7 days based on the *last* message), apply the action to the *entire thread* via `hub.users().threads_modify(...)`.

### Phase 4: Error Handling & Logging
*   Wrap all API calls in a retry backoff (Google APIs occasionally return 503 or 429).
*   Log state transitions explicitly: `[THREAD] msg_123 aged out (Last updated 8 days ago) -> Moved to Purgatory`.

---

## 4. Key Advantages of Eratosthenes

1.  **Zero Parsing:** We never download `RFC822` email headers or bodies. We only download JSON metadata (IDs, Labels, Dates). Bandwidth and memory usage will drop by 99%.
2.  **Perfect Threading:** "Age-off" is now perfectly aligned with how the Gmail UI actually groups and dates conversations. No more orphaned messages because of a missing `In-Reply-To` header.
3.  **No More IMAP Disconnects:** Stateless REST means a single malformed email cannot crash the execution loop. If one thread fails, log it and move to the next.