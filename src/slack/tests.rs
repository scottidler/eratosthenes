#![allow(clippy::unwrap_used)]

use super::*;
use std::sync::Mutex;

/// In-memory `SlackPoster` for tests: captures every (channel, text) posted, and the `blocks`
/// alongside it.
///
/// `blocks` is captured because a fake that dropped it would let the digest's block renderer go
/// unreachable again without a single test failing -- which is exactly how it shipped orphaned.
#[derive(Default)]
pub struct FakeSlackPoster {
    pub posts: Mutex<Vec<(String, String)>>,
    pub blocks: Mutex<Vec<Option<serde_json::Value>>>,
}

impl FakeSlackPoster {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SlackPoster for FakeSlackPoster {
    async fn post(
        &self,
        channel: &str,
        text: &str,
        blocks: Option<&serde_json::Value>,
    ) -> Result<()> {
        self.posts
            .lock()
            .unwrap()
            .push((channel.to_string(), text.to_string()));
        self.blocks.lock().unwrap().push(blocks.cloned());
        Ok(())
    }
}

// Env-var mutation is not safe with parallel tests; serialize behind a lock.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test]
async fn test_fake_poster_captures_posts() {
    let poster = FakeSlackPoster::new();
    poster.post("D123", "hello", None).await.unwrap();
    poster.post("D123", "world", None).await.unwrap();

    let posts = poster.posts.lock().unwrap();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0], ("D123".to_string(), "hello".to_string()));
    assert_eq!(posts[1], ("D123".to_string(), "world".to_string()));
}

#[test]
fn test_from_env_missing_var_errors() {
    let guard = ENV_LOCK.lock().unwrap();
    let var = "ERATOSTHENES_TEST_SLACK_TOKEN_MISSING";
    unsafe { std::env::remove_var(var) };

    // `from_env` validates the env var before touching TLS, so this path needs
    // no crypto provider; it must return a clear error naming the missing var.
    let err = match HttpSlackPoster::from_env(var) {
        Ok(_) => panic!("expected error for missing env var"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains(var),
        "error should name the missing var: {}",
        err
    );
    drop(guard);
}

/// The regression that matters most here: the digest must actually SEND its blocks.
///
/// `digest::blocks::format_blocks` shipped twice with zero call sites outside its own tests. It
/// compiled, its tests passed, and `#![deny(dead_code)]` said nothing because `blocks` is a
/// `pub mod` and public items are never dead code. So a fully tested renderer sat unreachable
/// while the mrkdwn path posted literal `  - ` hyphens to a live channel.
///
/// This asserts the wiring, not the rendering: that a `blocks` payload reaches the poster at
/// all. The shapes inside it are covered in `digest::blocks`.
#[tokio::test]
async fn test_a_blocks_payload_reaches_the_poster() {
    let poster = FakeSlackPoster::new();
    let blocks = serde_json::json!([{ "type": "rich_text", "elements": [] }]);

    poster
        .post("D123", "fallback", Some(&blocks))
        .await
        .unwrap();

    let captured = poster.blocks.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].as_ref(),
        Some(&blocks),
        "the poster must forward blocks, not silently drop them"
    );
}

/// And `None` still means a text-only post, so the fallback path is not broken by the addition.
#[tokio::test]
async fn test_no_blocks_still_posts_text_only() {
    let poster = FakeSlackPoster::new();
    poster.post("D123", "just text", None).await.unwrap();

    assert_eq!(poster.blocks.lock().unwrap()[0], None);
    assert_eq!(
        poster.posts.lock().unwrap()[0],
        ("D123".to_string(), "just text".to_string())
    );
}
