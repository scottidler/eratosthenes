use eyre::{Context, Result, eyre};
use google_gmail1::Gmail;
use google_gmail1::api::{BatchModifyMessagesRequest, ModifyMessageRequest};
use log::{debug, warn};

use crate::gmail::auth::GMAIL_SCOPE;
use crate::gmail::label::LabelResolver;
use crate::gmail::message::{GmailMessage, GmailThread};
use crate::gmail::rate::RateLimiter;

type Hub = Gmail<hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>>;

pub struct GmailClient {
    hub: Hub,
    limiter: RateLimiter,
    pub resolver: LabelResolver,
}

impl GmailClient {
    pub async fn new(hub: Hub) -> Result<Self> {
        let limiter = RateLimiter::new();

        println!("Connecting to Gmail...");
        let (_, label_list) = hub
            .users()
            .labels_list("me")
            .add_scope(GMAIL_SCOPE)
            .doit()
            .await
            .context("Failed to list Gmail labels")?;

        let resolver = LabelResolver::from_api_labels(label_list.labels.unwrap_or_default());

        Ok(Self { hub, limiter, resolver })
    }

    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    pub async fn search_messages(&self, query: &str) -> Result<Vec<String>> {
        debug!("search_messages: query={}", query);
        let mut all_ids = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            self.limiter.acquire(5).await;
            let mut call = self.hub.users().messages_list("me").q(query).add_scope(GMAIL_SCOPE);
            if let Some(ref token) = page_token {
                call = call.page_token(token);
            }

            let (_, result) = call.doit().await.context("messages.list failed")?;

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
        self.limiter.acquire(5).await;
        let (_, msg) = self
            .hub
            .users()
            .messages_get("me", id)
            .format("metadata")
            .add_metadata_headers("To")
            .add_metadata_headers("Cc")
            .add_metadata_headers("From")
            .add_metadata_headers("Subject")
            .add_metadata_headers("List-Id")
            .add_scope(GMAIL_SCOPE)
            .doit()
            .await
            .context(format!("messages.get({}) failed", id))?;

        GmailMessage::from_api(msg)
    }

    pub async fn list_threads(&self, query: &str) -> Result<Vec<String>> {
        debug!("list_threads: query={}", query);
        let mut all_ids = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            self.limiter.acquire(10).await;
            let mut call = self.hub.users().threads_list("me").q(query).add_scope(GMAIL_SCOPE);
            if let Some(ref token) = page_token {
                call = call.page_token(token);
            }

            let (_, result) = call.doit().await.context("threads.list failed")?;

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

    pub async fn get_thread(&self, id: &str) -> Result<GmailThread> {
        log::trace!("get_thread: id={}", id);
        self.limiter.acquire(10).await;
        let (_, thread) = self
            .hub
            .users()
            .threads_get("me", id)
            .format("metadata")
            .add_metadata_headers("To")
            .add_metadata_headers("Cc")
            .add_metadata_headers("From")
            .add_metadata_headers("Subject")
            .add_metadata_headers("List-Id")
            .add_scope(GMAIL_SCOPE)
            .doit()
            .await
            .context(format!("threads.get({}) failed", id))?;

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
            .context(format!("messages.modify({}) failed", id))?;

        Ok(())
    }

    pub async fn batch_modify(&self, ids: &[String], add: &[String], remove: &[String]) -> Result<()> {
        debug!("batch_modify: count={}, add={:?}, remove={:?}", ids.len(), add, remove);
        if ids.is_empty() {
            return Ok(());
        }

        for chunk in ids.chunks(1000) {
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
                .context("messages.batchModify failed")?;
        }

        Ok(())
    }

    pub async fn trash_thread(&self, id: &str) -> Result<()> {
        debug!("trash_thread: id={}", id);
        self.limiter.acquire(10).await;
        self.hub
            .users()
            .threads_trash("me", id)
            .add_scope(GMAIL_SCOPE)
            .doit()
            .await
            .context(format!("threads.trash({}) failed", id))?;
        Ok(())
    }
}
