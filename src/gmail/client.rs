use eyre::{Context, Result, eyre};
use google_gmail1::Gmail;
use google_gmail1::api::{BatchModifyMessagesRequest, ModifyMessageRequest, ModifyThreadRequest};
use log::{debug, warn};

use crate::gmail::auth::GMAIL_SCOPE;
use crate::gmail::label::LabelResolver;
use crate::gmail::message::{GmailMessage, GmailThread};
use crate::gmail::rate::{RateLimiter, with_retry};

type Hub = Gmail<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>>;

pub struct GmailClient {
    hub: Hub,
    limiter: RateLimiter,
    pub resolver: LabelResolver,
    metadata_headers: Vec<String>,
}

/// Headers always needed to parse a message (recipients, sender, subject).
/// Header-based filter guards (e.g. List-Id, Precedence) are added on top of
/// these via `set_metadata_headers`, derived from the active config.
fn default_metadata_headers() -> Vec<String> {
    ["To", "Cc", "From", "Subject"].iter().map(|s| s.to_string()).collect()
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
        let mut all_ids = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let result = with_retry(&self.limiter, "messages.list", || async {
                self.limiter.acquire(5).await;
                let mut call = self.hub.users().messages_list("me").q(query).add_scope(GMAIL_SCOPE);
                if let Some(ref token) = page_token {
                    call = call.page_token(token);
                }
                call.doit().await.map(|(_, r)| r).context("messages.list failed")
            })
            .await?;

            if let Some(messages) = result.messages {
                for msg in messages {
                    if let Some(id) = msg.id {
                        all_ids.push(id);
                    }
                }
            }

            page_token = result.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        debug!("search_messages({}) -> {} results", query, all_ids.len());
        Ok(all_ids)
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
                let mut call = self.hub.users().threads_list("me").q(query).add_scope(GMAIL_SCOPE);
                if let Some(ref token) = page_token {
                    call = call.page_token(token);
                }
                call.doit().await.map(|(_, r)| r).context("threads.list failed")
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

    pub async fn modify_message(&self, id: &str, add: &[String], remove: &[String]) -> Result<()> {
        debug!("modify_message: id={}, add={:?}, remove={:?}", id, add, remove);
        with_retry(&self.limiter, "messages.modify", || async {
            self.limiter.acquire(5).await;
            let req = ModifyMessageRequest {
                add_label_ids: if add.is_empty() { None } else { Some(add.to_vec()) },
                remove_label_ids: if remove.is_empty() { None } else { Some(remove.to_vec()) },
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

    pub async fn batch_modify(&self, ids: &[String], add: &[String], remove: &[String]) -> Result<()> {
        debug!("batch_modify: count={}, add={:?}, remove={:?}", ids.len(), add, remove);
        if ids.is_empty() {
            return Ok(());
        }

        for chunk in ids.chunks(1000) {
            with_retry(&self.limiter, "messages.batchModify", || async {
                self.limiter.acquire(50).await;
                let req = BatchModifyMessagesRequest {
                    add_label_ids: if add.is_empty() { None } else { Some(add.to_vec()) },
                    ids: Some(chunk.to_vec()),
                    remove_label_ids: if remove.is_empty() { None } else { Some(remove.to_vec()) },
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
        debug!("modify_thread: id={}, add={:?}, remove={:?}", id, add, remove);
        with_retry(&self.limiter, "threads.modify", || async {
            self.limiter.acquire(10).await;
            let req = ModifyThreadRequest {
                add_label_ids: if add.is_empty() { None } else { Some(add.to_vec()) },
                remove_label_ids: if remove.is_empty() { None } else { Some(remove.to_vec()) },
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
