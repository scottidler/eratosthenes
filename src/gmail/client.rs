//! The one module every Gmail call in this binary goes through.
//!
//! Its mutating surface is deliberately narrow -- label modification, trash,
//! and draft CREATION -- and there is no send anywhere in it, because nothing
//! in this design ever sends mail on Scott's behalf: a reply draft is written
//! into Gmail Drafts and a human reviews it. `google-gmail1` exposes two send
//! builders on the very types used here, one of them the sibling of
//! `create_draft` below, so "we did not call it" is not a guarantee anyone can
//! read off this file. The guarantee is `tests/no_send_guard.rs`, which scans
//! all of `src/` for both builders and whose bite is demonstrated, not assumed.
//! Do not add a send path here.

use std::io::Cursor;

use eyre::{Context, Result, eyre};
use google_gmail1::Gmail;
use google_gmail1::api::{
    BatchModifyMessagesRequest, Draft, Message, ModifyMessageRequest, ModifyThreadRequest,
};
use log::{debug, warn};
use mime::Mime;

use crate::gmail::auth::GMAIL_SCOPE;
use crate::gmail::label::LabelResolver;
use crate::gmail::message::{GmailMessage, GmailThread};
use crate::gmail::rate::{RateLimiter, with_retry};

type Hub = Gmail<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>>;

/// A `messages.list` hit: the message id plus the thread it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRef {
    pub id: String,
    pub thread_id: String,
}

pub struct GmailClient {
    hub: Hub,
    /// Public for the same reason `resolver` is: `create_label_if_missing` is a
    /// free function that needs the SHARED bucket while holding `&mut resolver`,
    /// and disjoint field borrows are what make that possible. A private field
    /// behind an accessor would be a second immutable borrow of `self` and would
    /// not compile at either call site.
    pub limiter: RateLimiter,
    pub resolver: LabelResolver,
    metadata_headers: Vec<String>,
}

/// Media type of a draft upload. `drafts.create` accepts `message/*` only.
const RFC822: &str = "message/rfc822";

/// Headers always needed to parse a message (recipients, sender, subject).
/// Header-based filter guards (e.g. List-Id, Precedence) are added on top of
/// these via `set_metadata_headers`, derived from the active config.
fn default_metadata_headers() -> Vec<String> {
    ["To", "Cc", "From", "Subject"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

impl GmailClient {
    pub async fn new(hub: Hub, prefix: &str) -> Result<Self> {
        let limiter = RateLimiter::new();

        println!("{}Connecting to Gmail...", prefix);
        let label_list = with_retry(&limiter, "labels.list", || async {
            limiter.acquire(1).await;
            hub.users()
                .labels_list("me")
                .add_scope(GMAIL_SCOPE)
                .doit()
                .await
                .map(|(_, l)| l)
                .context("Failed to list Gmail labels")
        })
        .await?;

        let resolver = LabelResolver::from_api_labels(label_list.labels.unwrap_or_default());

        Ok(Self {
            hub,
            limiter,
            resolver,
            metadata_headers: default_metadata_headers(),
        })
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    /// Set the full list of message headers to request from the Gmail API.
    /// A header-based filter guard only works if the header is actually fetched;
    /// the caller derives this set from the config's filter `headers` keys.
    pub fn set_metadata_headers(&mut self, headers: Vec<String>) {
        debug!("set_metadata_headers: headers={:?}", headers);
        self.metadata_headers = headers;
    }

    pub async fn search_messages(&self, query: &str) -> Result<Vec<String>> {
        debug!("search_messages: query={}", query);
        let refs = self.search_message_refs(query).await?;
        Ok(refs.into_iter().map(|r| r.id).collect())
    }

    /// `messages.list` keeping the `threadId` each hit carries. Triage groups
    /// candidate MESSAGES into distinct THREADS, and re-deriving that grouping
    /// with a `messages.get` per hit would be a round trip for data the list
    /// response already returned.
    pub async fn search_message_refs(&self, query: &str) -> Result<Vec<MessageRef>> {
        debug!("search_message_refs: query={}", query);
        let mut all_refs: Vec<MessageRef> = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let result = with_retry(&self.limiter, "messages.list", || async {
                self.limiter.acquire(5).await;
                let mut call = self
                    .hub
                    .users()
                    .messages_list("me")
                    .q(query)
                    .add_scope(GMAIL_SCOPE);
                if let Some(ref token) = page_token {
                    call = call.page_token(token);
                }
                call.doit()
                    .await
                    .map(|(_, r)| r)
                    .context("messages.list failed")
            })
            .await?;

            if let Some(messages) = result.messages {
                for msg in messages {
                    if let (Some(id), Some(thread_id)) = (msg.id, msg.thread_id) {
                        all_refs.push(MessageRef { id, thread_id });
                    }
                }
            }

            page_token = result.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        debug!(
            "search_message_refs({}) -> {} results",
            query,
            all_refs.len()
        );
        Ok(all_refs)
    }

    /// The authenticated account's own address, via `users.getProfile`.
    /// Triage's self-detection (is the newest message Scott's own reply?)
    /// compares `From` against it, and hard-coding it in config would be a
    /// second source of truth for something the token already knows.
    pub async fn profile_email(&self) -> Result<String> {
        debug!("profile_email");
        let profile = with_retry(&self.limiter, "users.getProfile", || async {
            self.limiter.acquire(1).await;
            self.hub
                .users()
                .get_profile("me")
                .add_scope(GMAIL_SCOPE)
                .doit()
                .await
                .map(|(_, p)| p)
                .context("users.getProfile failed")
        })
        .await?;

        profile
            .email_address
            .map(|e| e.to_lowercase())
            .ok_or_else(|| eyre!("users.getProfile returned no email address"))
    }

    pub async fn get_message(&self, id: &str) -> Result<GmailMessage> {
        log::trace!("get_message: id={}", id);
        let msg = with_retry(&self.limiter, "messages.get", || async {
            self.limiter.acquire(5).await;
            let mut call = self
                .hub
                .users()
                .messages_get("me", id)
                .format("metadata")
                .add_scope(GMAIL_SCOPE);
            for header in &self.metadata_headers {
                call = call.add_metadata_headers(header.as_str());
            }
            call.doit()
                .await
                .map(|(_, m)| m)
                .context(format!("messages.get({}) failed", id))
        })
        .await?;

        GmailMessage::from_api(msg)
    }

    pub async fn list_threads(&self, query: &str) -> Result<Vec<String>> {
        debug!("list_threads: query={}", query);
        let mut all_ids = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let result = with_retry(&self.limiter, "threads.list", || async {
                self.limiter.acquire(10).await;
                let mut call = self
                    .hub
                    .users()
                    .threads_list("me")
                    .q(query)
                    .add_scope(GMAIL_SCOPE);
                if let Some(ref token) = page_token {
                    call = call.page_token(token);
                }
                call.doit()
                    .await
                    .map(|(_, r)| r)
                    .context("threads.list failed")
            })
            .await?;

            if let Some(threads) = result.threads {
                for thread in threads {
                    if let Some(id) = thread.id {
                        all_ids.push(id);
                    }
                }
            }

            page_token = result.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        debug!("list_threads({}) -> {} results", query, all_ids.len());
        Ok(all_ids)
    }

    /// List threads that have ALL of the given label IDs present across any of their messages.
    /// Unlike `list_threads` (which uses a text query requiring a single message to match all
    /// conditions), `labelIds` is evaluated at the thread level: a thread matches if any message
    /// carries label A and any message carries label B.
    pub async fn list_threads_by_label_ids(&self, label_ids: &[&str]) -> Result<Vec<String>> {
        debug!("list_threads_by_label_ids: label_ids={:?}", label_ids);
        let mut all_ids = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let result = with_retry(&self.limiter, "threads.list (by label IDs)", || async {
                self.limiter.acquire(10).await;
                let mut call = self.hub.users().threads_list("me").add_scope(GMAIL_SCOPE);
                for &id in label_ids {
                    call = call.add_label_ids(id);
                }
                if let Some(ref token) = page_token {
                    call = call.page_token(token);
                }
                call.doit()
                    .await
                    .map(|(_, r)| r)
                    .context("threads.list (by label IDs) failed")
            })
            .await?;

            if let Some(threads) = result.threads {
                for thread in threads {
                    if let Some(id) = thread.id {
                        all_ids.push(id);
                    }
                }
            }

            page_token = result.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        debug!(
            "list_threads_by_label_ids({:?}) -> {} results",
            label_ids,
            all_ids.len()
        );
        Ok(all_ids)
    }

    pub async fn get_thread(&self, id: &str) -> Result<GmailThread> {
        log::trace!("get_thread: id={}", id);
        let thread = with_retry(&self.limiter, "threads.get", || async {
            self.limiter.acquire(10).await;
            let mut call = self
                .hub
                .users()
                .threads_get("me", id)
                .format("metadata")
                .add_scope(GMAIL_SCOPE);
            for header in &self.metadata_headers {
                call = call.add_metadata_headers(header.as_str());
            }
            call.doit()
                .await
                .map(|(_, t)| t)
                .context(format!("threads.get({}) failed", id))
        })
        .await?;

        let messages = thread
            .messages
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| match GmailMessage::from_api(m) {
                Ok(msg) => Some(msg),
                Err(e) => {
                    warn!("Skipping malformed message in thread {}: {}", id, e);
                    None
                }
            })
            .collect();

        Ok(GmailThread {
            id: thread.id.ok_or_else(|| eyre!("thread missing id"))?,
            messages,
        })
    }

    /// `threads.get` at `format=full`, which is the only format that carries
    /// message BODIES. Deliberately separate from `get_thread`: the aging
    /// engine stays on `format=metadata` (cheaper, and its quota cost is paid
    /// on every inbox thread every 5 minutes), and only triage's small capped
    /// candidate set pays for full payloads. Returns the raw API thread because
    /// body extraction is a MIME walk over `MessagePart`, which
    /// `GmailMessage::from_api` deliberately does not model.
    pub async fn get_thread_full(&self, id: &str) -> Result<google_gmail1::api::Thread> {
        log::trace!("get_thread_full: id={}", id);
        with_retry(&self.limiter, "threads.get (full)", || async {
            self.limiter.acquire(10).await;
            self.hub
                .users()
                .threads_get("me", id)
                .format("full")
                .add_scope(GMAIL_SCOPE)
                .doit()
                .await
                .map(|(_, t)| t)
                .context(format!("threads.get({}, full) failed", id))
        })
        .await
    }

    pub async fn modify_message(&self, id: &str, add: &[String], remove: &[String]) -> Result<()> {
        debug!(
            "modify_message: id={}, add={:?}, remove={:?}",
            id, add, remove
        );
        with_retry(&self.limiter, "messages.modify", || async {
            self.limiter.acquire(5).await;
            let req = ModifyMessageRequest {
                add_label_ids: if add.is_empty() {
                    None
                } else {
                    Some(add.to_vec())
                },
                remove_label_ids: if remove.is_empty() {
                    None
                } else {
                    Some(remove.to_vec())
                },
            };
            self.hub
                .users()
                .messages_modify(req, "me", id)
                .add_scope(GMAIL_SCOPE)
                .doit()
                .await
                .map(|_| ())
                .context(format!("messages.modify({}) failed", id))
        })
        .await
    }

    pub async fn batch_modify(
        &self,
        ids: &[String],
        add: &[String],
        remove: &[String],
    ) -> Result<()> {
        debug!(
            "batch_modify: count={}, add={:?}, remove={:?}",
            ids.len(),
            add,
            remove
        );
        if ids.is_empty() {
            return Ok(());
        }

        for chunk in ids.chunks(1000) {
            with_retry(&self.limiter, "messages.batchModify", || async {
                self.limiter.acquire(50).await;
                let req = BatchModifyMessagesRequest {
                    add_label_ids: if add.is_empty() {
                        None
                    } else {
                        Some(add.to_vec())
                    },
                    ids: Some(chunk.to_vec()),
                    remove_label_ids: if remove.is_empty() {
                        None
                    } else {
                        Some(remove.to_vec())
                    },
                };
                self.hub
                    .users()
                    .messages_batch_modify(req, "me")
                    .add_scope(GMAIL_SCOPE)
                    .doit()
                    .await
                    .map(|_| ())
                    .context("messages.batchModify failed")
            })
            .await?;
        }

        Ok(())
    }

    pub async fn modify_thread(&self, id: &str, add: &[String], remove: &[String]) -> Result<()> {
        debug!(
            "modify_thread: id={}, add={:?}, remove={:?}",
            id, add, remove
        );
        with_retry(&self.limiter, "threads.modify", || async {
            self.limiter.acquire(10).await;
            let req = ModifyThreadRequest {
                add_label_ids: if add.is_empty() {
                    None
                } else {
                    Some(add.to_vec())
                },
                remove_label_ids: if remove.is_empty() {
                    None
                } else {
                    Some(remove.to_vec())
                },
            };
            self.hub
                .users()
                .threads_modify(req, "me", id)
                .add_scope(GMAIL_SCOPE)
                .doit()
                .await
                .map(|_| ())
                .context(format!("threads.modify({}) failed", id))
        })
        .await
    }

    /// `drafts.create`: park an RFC822 message in Gmail Drafts, inside
    /// `thread_id`. Returns the new draft's id.
    ///
    /// The ONLY write this binary makes that produces a message, and it
    /// produces an UNSENT one. Threading is Gmail's to honor: the API contract
    /// requires `threadId` on the message, `In-Reply-To`/`References` per RFC
    /// 2822, and a matching `Subject` -- all three are built in
    /// `triage::draft::build_rfc822`.
    ///
    /// The RFC822 rides as MEDIA, not as `Draft.message.raw`. Not a choice:
    /// `google-gmail1` marks `drafts.create` upload-capable and its plain
    /// `doit()` is private, so `upload(stream, "message/rfc822")` is the only
    /// public terminal call. Same request either way -- a multipart POST whose
    /// metadata part carries `threadId` and whose media part carries the
    /// message -- and it skips the base64 inflation the `raw` field would add.
    pub async fn create_draft(&self, thread_id: &str, rfc822: &str) -> Result<String> {
        debug!(
            "create_draft: thread_id={}, rfc822_bytes={}",
            thread_id,
            rfc822.len()
        );

        let draft = with_retry(&self.limiter, "drafts.create", || async {
            self.limiter.acquire(10).await;
            let req = Draft {
                id: None,
                message: Some(Message {
                    thread_id: Some(thread_id.to_string()),
                    ..Default::default()
                }),
            };
            self.hub
                .users()
                .drafts_create(req, "me")
                .add_scope(GMAIL_SCOPE)
                .upload(
                    Cursor::new(rfc822.as_bytes().to_vec()),
                    RFC822
                        .parse::<Mime>()
                        .map_err(|e| eyre!("'{}' is not a parseable mime type: {}", RFC822, e))?,
                )
                .await
                .map(|(_, d)| d)
                .context(format!("drafts.create(thread {}) failed", thread_id))
        })
        .await?;

        draft.id.ok_or_else(|| {
            eyre!(
                "drafts.create returned no draft id for thread {}",
                thread_id
            )
        })
    }

    pub async fn trash_thread(&self, id: &str) -> Result<()> {
        debug!("trash_thread: id={}", id);
        with_retry(&self.limiter, "threads.trash", || async {
            self.limiter.acquire(10).await;
            self.hub
                .users()
                .threads_trash("me", id)
                .add_scope(GMAIL_SCOPE)
                .doit()
                .await
                .map(|_| ())
                .context(format!("threads.trash({}) failed", id))
        })
        .await
    }
}
