#![allow(clippy::unwrap_used)]

use super::*;
use std::sync::Mutex;

/// In-memory `SlackPoster` for tests: captures every (channel, text) posted.
#[derive(Default)]
pub struct FakeSlackPoster {
    pub posts: Mutex<Vec<(String, String)>>,
}

impl FakeSlackPoster {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SlackPoster for FakeSlackPoster {
    async fn post(&self, channel: &str, text: &str) -> Result<()> {
        self.posts
            .lock()
            .unwrap()
            .push((channel.to_string(), text.to_string()));
        Ok(())
    }
}

// Env-var mutation is not safe with parallel tests; serialize behind a lock.
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[tokio::test]
async fn test_fake_poster_captures_posts() {
    let poster = FakeSlackPoster::new();
    poster.post("D123", "hello").await.unwrap();
    poster.post("D123", "world").await.unwrap();

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
