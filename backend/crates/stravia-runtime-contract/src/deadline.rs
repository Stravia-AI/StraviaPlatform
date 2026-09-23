use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A shareable hard deadline. Every clone observes the same instant, so a
/// renewal in one layer extends waiters in all layers. `renew` pushes the
/// deadline out by the TTL it was created with; `fixed` deadlines have a zero
/// TTL and never renew.
#[derive(Clone, Debug)]
pub struct Deadline(Arc<DeadlineState>);

#[derive(Debug)]
struct DeadlineState {
    at: Mutex<Instant>,
    ttl: Duration,
    notify: tokio::sync::Notify,
}

impl Deadline {
    /// Create a renewable deadline `ttl` from now. Each `renew` re-arms it for
    /// another `ttl`, turning the deadline into an idle timeout.
    pub fn from_now(ttl: Duration) -> Self {
        Self(Arc::new(DeadlineState {
            at: Mutex::new(Instant::now() + ttl),
            ttl,
            notify: tokio::sync::Notify::new(),
        }))
    }

    /// An absolute deadline that `renew` never extends.
    pub fn fixed(at: Instant) -> Self {
        Self(Arc::new(DeadlineState {
            at: Mutex::new(at),
            ttl: Duration::ZERO,
            notify: tokio::sync::Notify::new(),
        }))
    }

    /// A deadline that never fires (useful for unit tests / health probes).
    pub fn never() -> Self {
        Self::from_now(Duration::from_secs(86400 * 365 * 100))
    }

    /// The absolute `Instant` the deadline currently fires at.
    pub fn at(&self) -> Instant {
        *self.0.at.lock().unwrap()
    }

    /// Returns `true` if the deadline has already passed.
    pub fn is_exceeded(&self) -> bool {
        Instant::now() > self.at()
    }

    /// How much time remains. Returns zero if already exceeded.
    pub fn remaining(&self) -> Duration {
        self.at().saturating_duration_since(Instant::now())
    }

    /// Extend the deadline to `now + ttl` when that lies beyond the current
    /// instant. No-op for `fixed` deadlines.
    pub fn renew(&self) {
        if self.0.ttl.is_zero() {
            return;
        }
        let mut at = self.0.at.lock().unwrap();
        *at = (*at).max(Instant::now() + self.0.ttl);
        drop(at);
        self.0.notify.notify_waiters();
    }

    /// Move the deadline to an absolute instant. The renewal TTL is kept.
    pub fn reset(&self, at: Instant) {
        *self.0.at.lock().unwrap() = at;
        self.0.notify.notify_waiters();
    }

    /// Resolve once the deadline passes. Waiters re-read the shared instant
    /// after each wake, so a renewal or reset while sleeping re-arms the wait
    /// instead of firing at a stale instant.
    pub async fn wait(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Register the waiter before reading `at`: `notify_waiters` stores
            // no permit, so an unregistered renewal or reset between the read
            // and the first poll would otherwise be lost.
            notified.as_mut().enable();
            let at = self.at();
            if Instant::now() >= at {
                return;
            }
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(at.into()) => {}
                () = notified => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clones_share_the_same_instant() {
        let deadline = Deadline::from_now(Duration::from_secs(60));
        let clone = deadline.clone();
        let target = Instant::now() + Duration::from_secs(5);
        clone.reset(target);
        assert_eq!(deadline.at(), target);
    }

    #[tokio::test]
    async fn renew_extends_by_ttl() {
        let deadline = Deadline::from_now(Duration::from_millis(40));
        tokio::time::sleep(Duration::from_millis(20)).await;
        deadline.renew();
        assert!(deadline.at() >= Instant::now() + Duration::from_millis(35));
    }

    #[tokio::test]
    async fn fixed_deadline_never_renews() {
        let at = Instant::now() + Duration::from_millis(50);
        let deadline = Deadline::fixed(at);
        deadline.renew();
        assert_eq!(deadline.at(), at);
    }

    #[tokio::test]
    async fn wait_resolves_after_silence_expires() {
        let deadline = Deadline::from_now(Duration::from_millis(30));
        tokio::time::timeout(Duration::from_secs(2), deadline.wait())
            .await
            .expect("deadline should fire");
        assert!(deadline.is_exceeded());
        assert_eq!(deadline.remaining(), Duration::ZERO);
    }

    #[tokio::test]
    async fn wait_rearms_on_renewal() {
        let deadline = Deadline::from_now(Duration::from_millis(60));
        let renewer = deadline.clone();
        let renewals = tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(30)).await;
                renewer.renew();
            }
        });
        tokio::time::timeout(Duration::from_secs(2), deadline.wait())
            .await
            .expect("deadline should fire after renewals stop");
        renewals.await.expect("renewal task panicked");
        // Three renewals at 30ms intervals push the firing point past 100ms.
        assert!(deadline.is_exceeded());
    }

    #[tokio::test]
    async fn wait_respects_reset_to_earlier_instant() {
        let deadline = Deadline::from_now(Duration::from_secs(60));
        let shorter = deadline.clone();
        let reset = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            shorter.reset(Instant::now() + Duration::from_millis(20));
        });
        tokio::time::timeout(Duration::from_secs(2), deadline.wait())
            .await
            .expect("reset deadline should fire");
        reset.await.expect("reset task panicked");
        assert!(deadline.is_exceeded());
    }

    #[tokio::test]
    async fn never_does_not_fire() {
        let deadline = Deadline::never();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), deadline.wait())
                .await
                .is_err()
        );
        assert!(!deadline.is_exceeded());
    }
}
