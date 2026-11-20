use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use solascope_config::EmbeddingConfig;
use solascope_core::{Error, Result};
use tokio::{sync::Notify, time::Instant};

const WINDOW: Duration = Duration::from_secs(60);

pub(crate) struct RateLimiter {
    state: Mutex<State>,
    changed: Notify,
    max_concurrency: usize,
    base_cooldown_ms: u64,
    retry_base_ms: u64,
    retry_max_ms: u64,
    request_interval: Option<Duration>,
    tokens_per_minute: Option<u64>,
}

struct State {
    active: usize,
    concurrency: usize,
    epoch: u64,
    recovery_successes: usize,
    stable_successes: usize,
    next_cooldown_ms: u64,
    cooldown: Cooldown,
    next_request: Instant,
    tokens: u64,
    usage: VecDeque<(Instant, u64)>,
    next_ticket: u64,
    queue: VecDeque<u64>,
}

#[derive(Clone, Copy)]
enum Cooldown {
    Ready,
    Until(Instant),
}

impl Cooldown {
    fn ready(self, now: Instant) -> bool {
        matches!(self, Self::Ready) || matches!(self, Self::Until(until) if until <= now)
    }

    fn extend(&mut self, now: Instant, delay: Duration) -> Result<()> {
        let until = now.checked_add(delay).ok_or_else(|| {
            Error::ModelUnavailable("model Retry-After exceeds the supported time range".into())
        })?;
        match self {
            Self::Until(current) => *current = (*current).max(until),
            Self::Ready => *self = Self::Until(until),
        }
        Ok(())
    }

    fn remaining(self, now: Instant) -> Duration {
        match self {
            Self::Ready => Duration::ZERO,
            Self::Until(until) => until.saturating_duration_since(now),
        }
    }
}

impl RateLimiter {
    pub(crate) fn new(config: &EmbeddingConfig) -> Result<Arc<Self>> {
        config.validate_rate_limits()?;
        let now = Instant::now();
        Ok(Arc::new(Self {
            state: Mutex::new(State {
                active: 0,
                concurrency: config.concurrency,
                epoch: 0,
                recovery_successes: 0,
                stable_successes: 0,
                next_cooldown_ms: config.rate_limit_cooldown_ms,
                cooldown: Cooldown::Ready,
                next_request: now,
                tokens: 0,
                usage: VecDeque::new(),
                next_ticket: 0,
                queue: VecDeque::new(),
            }),
            changed: Notify::new(),
            max_concurrency: config.concurrency,
            base_cooldown_ms: config.rate_limit_cooldown_ms,
            retry_base_ms: config.retry_base_delay_ms,
            retry_max_ms: config.retry_max_delay_ms,
            request_interval: config
                .requests_per_minute
                .map(|rpm| Duration::from_nanos(60_000_000_000_u64.div_ceil(u64::from(rpm)))),
            tokens_per_minute: config.tokens_per_minute,
        }))
    }

    pub(crate) fn token_limit(&self) -> Option<u64> {
        self.tokens_per_minute
    }

    pub(crate) fn retry_delay(&self, attempt: usize, hint: Option<Duration>) -> Duration {
        let base = self
            .retry_base_ms
            .saturating_mul(1_u64 << attempt.min(5))
            .min(self.retry_max_ms);
        jittered(base, self.retry_max_ms).max(hint.unwrap_or_default())
    }

    pub(crate) fn rate_limited(&self, hint: Option<Duration>) -> Result<Duration> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut outcome = Ok(());
        if state.cooldown.ready(now) {
            let delay = jittered(state.next_cooldown_ms, self.retry_max_ms);
            outcome = state.cooldown.extend(now, delay);
            if outcome.is_ok() {
                let previous_concurrency = state.concurrency;
                state.epoch = state.epoch.wrapping_add(1);
                state.concurrency = 1;
                state.recovery_successes = 0;
                state.stable_successes = 0;
                state.next_cooldown_ms = state
                    .next_cooldown_ms
                    .saturating_mul(2)
                    .min(self.retry_max_ms);
                tracing::warn!(
                    cooldown_ms = delay.as_millis() as u64,
                    previous_concurrency,
                    effective_concurrency = 1,
                    "embedding rate limit cooldown"
                );
            }
        }
        // A bad hint must not poison the adapter's shared state. Keep its finite
        // fallback cooldown, fail this call, and allow later calls to recover.
        if outcome.is_ok() {
            if let Some(hint) = hint {
                outcome = state.cooldown.extend(now, hint);
            }
        }
        let delay = state.cooldown.remaining(now);
        drop(state);
        self.changed.notify_waiters();
        outcome.map(|_| delay)
    }

    pub(crate) async fn acquire(self: &Arc<Self>, tokens: u64) -> Result<Permit> {
        let tokens = tokens.max(1);
        if self.tokens_per_minute.is_some_and(|limit| tokens > limit) {
            return Err(Error::InvalidInput(
                "embedding batch exceeds tokens_per_minute; lower batch_size or raise the budget"
                    .into(),
            ));
        }
        let ticket = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let ticket = state.next_ticket;
            state.next_ticket = state.next_ticket.wrapping_add(1);
            state.queue.push_back(ticket);
            ticket
        };
        let mut waiting = Waiting {
            limiter: self.clone(),
            ticket,
            queued: true,
        };
        loop {
            // Create the notification before inspecting state; notify_waiters
            // also wakes an unpolled Notified future, avoiding lost releases.
            let changed = self.changed.notified();
            let wake_at = {
                let now = Instant::now();
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                while state
                    .usage
                    .front()
                    .is_some_and(|(at, _)| *at + WINDOW <= now)
                {
                    let (_, expired) = state.usage.pop_front().unwrap();
                    state.tokens -= expired;
                }
                let mut not_before = state.next_request;
                if let Some(limit) = self.tokens_per_minute {
                    let mut remaining = state.tokens;
                    for (at, used) in &state.usage {
                        if tokens <= limit - remaining {
                            break;
                        }
                        remaining -= used;
                        not_before = not_before.max(*at + WINDOW);
                    }
                }
                if let Cooldown::Until(until) = state.cooldown {
                    not_before = not_before.max(until);
                }
                let first = state.queue.front() == Some(&ticket);
                if first
                    && state.active < state.concurrency
                    && state.cooldown.ready(now)
                    && not_before <= now
                {
                    state.queue.pop_front();
                    waiting.queued = false;
                    state.active += 1;
                    if let Some(interval) = self.request_interval {
                        state.next_request = now + interval;
                    }
                    if self.tokens_per_minute.is_some() {
                        state.tokens += tokens;
                        state.usage.push_back((now, tokens));
                    }
                    let epoch = state.epoch;
                    drop(state);
                    self.changed.notify_waiters();
                    return Ok(Permit {
                        limiter: self.clone(),
                        epoch,
                        success: false,
                    });
                }
                if first && not_before > now {
                    Some(not_before)
                } else {
                    None
                }
            };
            if let Some(wake_at) = wake_at {
                tokio::select! {
                    _ = changed => {},
                    _ = tokio::time::sleep_until(wake_at) => {},
                }
            } else {
                changed.await;
            }
        }
    }
}

struct Waiting {
    limiter: Arc<RateLimiter>,
    ticket: u64,
    queued: bool,
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if self.queued {
            let mut state = self
                .limiter
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.queue.retain(|ticket| *ticket != self.ticket);
            drop(state);
            self.limiter.changed.notify_waiters();
        }
    }
}

pub(crate) struct Permit {
    limiter: Arc<RateLimiter>,
    epoch: u64,
    success: bool,
}

impl Permit {
    pub(crate) fn succeed(mut self) {
        self.success = true;
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self
            .limiter
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active -= 1;
        // In-flight successes from before a 429 must not undo the reduced
        // concurrency or count toward recovery while the group is cooling.
        if self.epoch == state.epoch {
            if self.success && state.cooldown.ready(Instant::now()) {
                state.recovery_successes += 1;
                state.stable_successes += 1;
                if state.recovery_successes >= 3 {
                    state.recovery_successes = 0;
                    if state.concurrency < self.limiter.max_concurrency {
                        state.concurrency += 1;
                        tracing::debug!(
                            effective_concurrency = state.concurrency,
                            max_concurrency = self.limiter.max_concurrency,
                            "embedding concurrency recovered"
                        );
                    }
                }
                if state.stable_successes >= 10 {
                    state.stable_successes = 0;
                    state.next_cooldown_ms =
                        (state.next_cooldown_ms / 2).max(self.limiter.base_cooldown_ms);
                }
            } else if !self.success {
                state.recovery_successes = 0;
                state.stable_successes = 0;
            }
        }
        drop(state);
        self.limiter.changed.notify_waiters();
    }
}

fn jittered(base_ms: u64, max_ms: u64) -> Duration {
    let upper = base_ms.saturating_add(base_ms / 4).min(max_ms);
    Duration::from_millis(rand::random_range(base_ms..=upper))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::poll;
    use tokio::time::advance;

    fn config() -> EmbeddingConfig {
        EmbeddingConfig {
            concurrency: 3,
            retry_base_delay_ms: 40,
            retry_max_delay_ms: 1_000,
            rate_limit_cooldown_ms: 100,
            ..Default::default()
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cooldown_is_shared_and_stale_successes_cannot_restore_concurrency() {
        let limiter = RateLimiter::new(&config()).unwrap();
        let old = [
            limiter.acquire(1).await.unwrap(),
            limiter.acquire(1).await.unwrap(),
            limiter.acquire(1).await.unwrap(),
        ];
        let delay = limiter.rate_limited(None).unwrap();
        assert!((Duration::from_millis(100)..=Duration::from_millis(125)).contains(&delay));
        for permit in old {
            permit.succeed();
        }
        assert_eq!(limiter.state.lock().unwrap().concurrency, 1);
        assert_eq!(limiter.state.lock().unwrap().stable_successes, 0);

        let mut waiting = Box::pin(limiter.acquire(1));
        assert!(poll!(waiting.as_mut()).is_pending());
        advance(delay).await;
        waiting.await.unwrap().succeed();
        for _ in 0..2 {
            limiter.acquire(1).await.unwrap().succeed();
        }
        assert_eq!(limiter.state.lock().unwrap().concurrency, 2);
        for _ in 0..3 {
            limiter.acquire(1).await.unwrap().succeed();
        }
        assert_eq!(limiter.state.lock().unwrap().concurrency, 3);
        for _ in 0..4 {
            limiter.acquire(1).await.unwrap().succeed();
        }
        assert_eq!(limiter.state.lock().unwrap().next_cooldown_ms, 100);
    }

    #[tokio::test(start_paused = true)]
    async fn one_episode_does_not_multiply_backoff_and_later_hints_extend_it() {
        let limiter = RateLimiter::new(&config()).unwrap();
        let first = limiter.rate_limited(None).unwrap();
        assert_eq!(limiter.rate_limited(None).unwrap(), first);
        assert_eq!(limiter.state.lock().unwrap().epoch, 1);
        assert_eq!(limiter.state.lock().unwrap().next_cooldown_ms, 200);
        let extended = limiter.rate_limited(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(extended, Duration::from_secs(2));
        assert_eq!(
            limiter
                .rate_limited(Some(Duration::from_millis(1)))
                .unwrap(),
            extended
        );
        assert_eq!(limiter.state.lock().unwrap().next_cooldown_ms, 200);
        advance(extended).await;
        let second = limiter.rate_limited(None).unwrap();
        assert!((Duration::from_millis(200)..=Duration::from_millis(250)).contains(&second));
        assert_eq!(limiter.state.lock().unwrap().epoch, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_reset_consecutive_recovery_successes() {
        let limiter = RateLimiter::new(&config()).unwrap();
        advance(limiter.rate_limited(None).unwrap()).await;
        for _ in 0..2 {
            limiter.acquire(1).await.unwrap().succeed();
        }
        drop(limiter.acquire(1).await.unwrap());
        limiter.acquire(1).await.unwrap().succeed();
        assert_eq!(limiter.state.lock().unwrap().concurrency, 1);
        assert_eq!(limiter.state.lock().unwrap().recovery_successes, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rpm_is_paced_and_failed_attempts_still_consume_budget() {
        let limiter = RateLimiter::new(&EmbeddingConfig {
            requests_per_minute: Some(60),
            ..config()
        })
        .unwrap();
        drop(limiter.acquire(1).await.unwrap());
        let mut next = Box::pin(limiter.acquire(1));
        assert!(poll!(next.as_mut()).is_pending());
        advance(Duration::from_millis(999)).await;
        assert!(poll!(next.as_mut()).is_pending());
        advance(Duration::from_millis(1)).await;
        next.await.unwrap().succeed();
    }

    #[tokio::test(start_paused = true)]
    async fn tpm_uses_a_sliding_window_and_waits_only_for_enough_expirations() {
        let limiter = RateLimiter::new(&EmbeddingConfig {
            tokens_per_minute: Some(10),
            ..config()
        })
        .unwrap();
        limiter.acquire(3).await.unwrap().succeed();
        advance(Duration::from_secs(10)).await;
        limiter.acquire(4).await.unwrap().succeed();
        let mut next = Box::pin(limiter.acquire(5));
        assert!(poll!(next.as_mut()).is_pending());
        advance(Duration::from_secs(49)).await;
        assert!(poll!(next.as_mut()).is_pending());
        advance(Duration::from_secs(1)).await;
        next.await.unwrap().succeed();
        assert_eq!(limiter.state.lock().unwrap().tokens, 9);
        assert_eq!(limiter.state.lock().unwrap().usage.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn canceled_waiters_do_not_reserve_budget_or_block_fifo_admission() {
        let limiter = RateLimiter::new(&EmbeddingConfig {
            concurrency: 1,
            tokens_per_minute: Some(10),
            ..config()
        })
        .unwrap();
        let active = limiter.acquire(1).await.unwrap();
        let mut canceled = Box::pin(limiter.acquire(4));
        let mut next = Box::pin(limiter.acquire(2));
        assert!(poll!(canceled.as_mut()).is_pending());
        assert!(poll!(next.as_mut()).is_pending());
        assert_eq!(limiter.state.lock().unwrap().tokens, 1);
        drop(canceled);
        active.succeed();
        next.await.unwrap().succeed();
        let state = limiter.state.lock().unwrap();
        assert_eq!(state.active, 0);
        assert!(state.queue.is_empty());
        assert_eq!(state.tokens, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn extreme_hints_fail_without_poisoning_shared_cooldown_or_queue_capacity() {
        let limiter = RateLimiter::new(&config()).unwrap();
        assert!(matches!(
            limiter.rate_limited(Some(Duration::MAX)),
            Err(Error::ModelUnavailable(_))
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(1), limiter.acquire(1))
                .await
                .is_err()
        );
        assert!(limiter.state.lock().unwrap().queue.is_empty());
        assert_eq!(limiter.state.lock().unwrap().active, 0);
        let remaining = limiter
            .state
            .lock()
            .unwrap()
            .cooldown
            .remaining(Instant::now());
        assert!(remaining <= Duration::from_millis(125));
        advance(remaining).await;
        limiter.acquire(1).await.unwrap().succeed();
    }

    #[tokio::test(start_paused = true)]
    async fn retry_delays_have_bounded_jitter_and_respect_provider_minimums() {
        let limiter = RateLimiter::new(&config()).unwrap();
        for attempt in 0..=5 {
            let base = (40_u64 << attempt).min(1_000);
            let upper = (base + base / 4).min(1_000);
            for _ in 0..20 {
                let delay = limiter.retry_delay(attempt, None);
                assert!(
                    (Duration::from_millis(base)..=Duration::from_millis(upper)).contains(&delay)
                );
            }
        }
        assert_eq!(
            limiter.retry_delay(0, Some(Duration::from_secs(5))),
            Duration::from_secs(5)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn oversized_estimates_fail_without_reserving_a_slot() {
        let limiter = RateLimiter::new(&EmbeddingConfig {
            tokens_per_minute: Some(10),
            ..config()
        })
        .unwrap();
        assert!(matches!(
            limiter.acquire(11).await,
            Err(Error::InvalidInput(_))
        ));
        assert!(limiter.state.lock().unwrap().queue.is_empty());
        assert_eq!(limiter.state.lock().unwrap().active, 0);
        limiter.acquire(10).await.unwrap().succeed();
    }
}
