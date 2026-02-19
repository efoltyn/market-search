pub use eli_finance_types::*;

use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::sleep;
use tokio::time::Duration as TokioDuration;

/// Simple adaptive rate limiter for API pagination.
pub struct RateLimiter {
    base_delay_ms: u64,
    max_delay_ms: u64,
    current_delay_ms: AtomicU64,
}

impl RateLimiter {
    pub fn new(base_delay_ms: u64, max_delay_ms: u64) -> Self {
        let base = base_delay_ms.max(0);
        let max = max_delay_ms.max(base);
        Self {
            base_delay_ms: base,
            max_delay_ms: max,
            current_delay_ms: AtomicU64::new(base),
        }
    }

    pub async fn wait(&self) {
        let delay_ms = self.current_delay_ms.load(Ordering::Relaxed);
        if delay_ms > 0 {
            sleep(TokioDuration::from_millis(delay_ms)).await;
        }
    }

    pub fn on_rate_limited(&self) {
        let mut cur = self.current_delay_ms.load(Ordering::Relaxed);
        loop {
            let next = cur
                .saturating_mul(2)
                .clamp(self.base_delay_ms, self.max_delay_ms);
            match self.current_delay_ms.compare_exchange(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    pub fn on_success(&self) {
        let mut cur = self.current_delay_ms.load(Ordering::Relaxed);
        loop {
            if cur <= self.base_delay_ms {
                break;
            }
            let decayed = (cur as f64 * 0.9).round() as u64;
            let next = decayed.max(self.base_delay_ms);
            match self.current_delay_ms.compare_exchange(
                cur,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }
}
