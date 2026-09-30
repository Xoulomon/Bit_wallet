//! Per-user command throttling (PLAN.md §8.7).
//!
//! This is *UI* throttling and it belongs to the front end: it stops one chatty
//! user from filling the dispatcher's queue. The BitRPC call budget is a
//! different concern living in core (§4), and conflating the two would mean a
//! user with a fast thumb could starve everyone else's sync.

use governor::{
    Quota, RateLimiter,
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
};
use std::{
    collections::HashMap,
    num::NonZeroU32,
    sync::Mutex,
    time::{Duration, Instant},
};

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// Generous enough that a human never notices, tight enough that a script does.
const COMMANDS_PER_MINUTE: u32 = 20;

/// How long an idle user's limiter is kept before it is forgotten, so a bot
/// with thousands of one-time visitors does not grow a map forever.
const IDLE_EVICTION: Duration = Duration::from_secs(15 * 60);

struct Entry {
    limiter: Limiter,
    last_seen: Instant,
}

pub struct Throttle {
    entries: Mutex<HashMap<i64, Entry>>,
    quota: Quota,
}

impl Throttle {
    pub fn new() -> Self {
        Throttle::with_quota(COMMANDS_PER_MINUTE)
    }

    pub fn with_quota(per_minute: u32) -> Self {
        let n = NonZeroU32::new(per_minute.max(1)).unwrap_or(NonZeroU32::MIN);
        Throttle {
            entries: Mutex::new(HashMap::new()),
            quota: Quota::per_minute(n),
        }
    }

    /// `true` if this user may act now.
    ///
    /// A poisoned mutex lets the command through rather than locking everyone
    /// out: this is politeness, not security, and failing closed here would
    /// turn a bug into an outage.
    pub fn allow(&self, tg_id: i64) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return true;
        };

        let now = Instant::now();
        entries.retain(|_, e| now.duration_since(e.last_seen) < IDLE_EVICTION);

        let entry = entries.entry(tg_id).or_insert_with(|| Entry {
            limiter: RateLimiter::direct(self.quota),
            last_seen: now,
        });
        entry.last_seen = now;
        entry.limiter.check().is_ok()
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Throttle::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_user_is_allowed_up_to_the_quota_and_then_held_back() {
        let t = Throttle::with_quota(3);
        assert!(t.allow(1));
        assert!(t.allow(1));
        assert!(t.allow(1));
        assert!(!t.allow(1), "the fourth command in a minute waits");
    }

    /// §8.7: throttling is per user, so one chatty account cannot silence
    /// anyone else.
    #[test]
    fn one_users_burst_does_not_affect_another() {
        let t = Throttle::with_quota(2);
        assert!(t.allow(1));
        assert!(t.allow(1));
        assert!(!t.allow(1));

        assert!(t.allow(2), "a different user still has their full quota");
        assert!(t.allow(2));
    }

    #[test]
    fn a_zero_quota_is_treated_as_one_rather_than_locking_everyone_out() {
        let t = Throttle::with_quota(0);
        assert!(t.allow(1), "a misconfiguration must not brick the bot");
    }
}
