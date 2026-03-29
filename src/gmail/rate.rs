use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use tokio::time::{Duration, Instant, sleep};

const MAX_TOKENS: u32 = 15000;
const REFILL_PER_SEC: u32 = 250;
const MAX_BACKOFF_SECS: u64 = 60;

pub struct RateLimiter {
    tokens: AtomicU32,
    last_refill: Mutex<Instant>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            tokens: AtomicU32::new(MAX_TOKENS),
            last_refill: Mutex::new(Instant::now()),
        }
    }

    fn refill(&self) {
        let mut last = self.last_refill.lock().expect("lock poisoned");
        let elapsed = last.elapsed();
        let new_tokens = (elapsed.as_secs_f64() * REFILL_PER_SEC as f64) as u32;
        if new_tokens > 0 {
            let current = self.tokens.load(Ordering::Relaxed);
            let refilled = (current + new_tokens).min(MAX_TOKENS);
            self.tokens.store(refilled, Ordering::Relaxed);
            *last = Instant::now();
        }
    }

    pub async fn acquire(&self, cost: u32) {
        loop {
            self.refill();
            let current = self.tokens.load(Ordering::Relaxed);
            if current >= cost {
                self.tokens.fetch_sub(cost, Ordering::Relaxed);
                return;
            }
            let deficit = cost - current;
            let wait_ms = (deficit as f64 / REFILL_PER_SEC as f64 * 1000.0) as u64;
            sleep(Duration::from_millis(wait_ms.max(10))).await;
        }
    }

    pub async fn backoff(&self, attempt: u32) {
        let base = 1u64 << attempt.min(6);
        let jitter = base / 4;
        let wait = base.min(MAX_BACKOFF_SECS).saturating_add(jitter);
        log::warn!("Rate limited, backing off for {}s (attempt {})", wait, attempt);
        sleep(Duration::from_secs(wait)).await;
    }
}

const MAX_RETRIES: u32 = 5;

pub async fn with_retry<F, Fut, T>(limiter: &RateLimiter, op_name: &str, mut f: F) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = eyre::Result<T>>,
{
    for attempt in 0..MAX_RETRIES {
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("429") || msg.contains("503") || msg.contains("rate") {
                    log::warn!("[retry] {} failed (attempt {}): {}", op_name, attempt + 1, msg);
                    limiter.backoff(attempt).await;
                } else {
                    return Err(e);
                }
            }
        }
    }
    eyre::bail!("{} failed after {} retries", op_name, MAX_RETRIES)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_acquire_within_budget() {
        let limiter = RateLimiter::new();
        limiter.acquire(10).await;
        let remaining = limiter.tokens.load(Ordering::Relaxed);
        assert!(remaining < MAX_TOKENS);
    }

    #[tokio::test]
    async fn test_acquire_multiple() {
        let limiter = RateLimiter::new();
        limiter.acquire(100).await;
        limiter.acquire(100).await;
        let remaining = limiter.tokens.load(Ordering::Relaxed);
        assert!(remaining <= MAX_TOKENS - 200);
    }
}
