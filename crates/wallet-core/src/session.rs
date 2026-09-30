//! The unlocked-session cache (PLAN.md §5, §3a rule 5).
//!
//! This module is in `wallet-core` and not in the bot, and that placement is
//! the whole point: a front end must not be able to hold a decrypted seed,
//! because a *second* front end would then need its own copy of this logic and
//! its own chance to leak one. A front end may ask whether a session is open
//! and how long is left; it can never see what is in it.
//!
//! Expiry is evaluated on read rather than by a timer, so a process that was
//! suspended for an hour wakes up with every session already expired instead of
//! with a queue of timers that have not fired.

use crate::service::{
    events::CoreEvent,
    types::{SessionInfo, UserId},
};
use bdk_wallet::keys::bip39::Mnemonic;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::broadcast;

struct Entry {
    mnemonic: Mnemonic,
    /// Monotonic, so changing the system clock cannot extend a session.
    last_used: Instant,
}

pub struct Sessions {
    entries: Mutex<HashMap<UserId, Entry>>,
    idle_timeout: Duration,
    events: broadcast::Sender<CoreEvent>,
}

impl Sessions {
    pub fn new(idle_timeout: Duration, events: broadcast::Sender<CoreEvent>) -> Self {
        Sessions {
            entries: Mutex::new(HashMap::new()),
            idle_timeout,
            events,
        }
    }

    /// Cache a decrypted mnemonic and start the idle clock.
    pub fn unlock(&self, user: UserId, mnemonic: Mnemonic) -> SessionInfo {
        let now = Instant::now();
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                user,
                Entry {
                    mnemonic,
                    last_used: now,
                },
            );
        }
        self.info_at(user, now)
    }

    /// Whether a session is open and how long is left — never its contents.
    pub fn info(&self, user: UserId) -> Option<SessionInfo> {
        self.sweep();
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(&user)?;
        Some(self.info_at(user, entry.last_used))
    }

    /// Run `f` over the cached mnemonic and refresh the idle clock.
    ///
    /// The seed is handed to a closure rather than returned, so there is no
    /// call on this type that yields a secret to a caller. That is what makes
    /// rule 5 structural instead of a convention.
    pub fn with_mnemonic<T>(&self, user: UserId, f: impl FnOnce(&Mnemonic) -> T) -> Option<T> {
        self.sweep();
        let mut entries = self.entries.lock().ok()?;
        let entry = entries.get_mut(&user)?;
        entry.last_used = Instant::now();
        Some(f(&entry.mnemonic))
    }

    pub fn is_unlocked(&self, user: UserId) -> bool {
        self.info(user).is_some()
    }

    /// Drop a session now. Idempotent: locking an already-locked wallet is a
    /// reasonable thing for a nervous user to do twice.
    pub fn lock(&self, user: UserId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.remove(&user);
        }
    }

    /// Drop every expired session, announcing each one (§8.6).
    pub fn sweep(&self) {
        let now = Instant::now();
        let expired: Vec<UserId> = match self.entries.lock() {
            Ok(mut entries) => {
                let gone: Vec<UserId> = entries
                    .iter()
                    .filter(|(_, e)| now.duration_since(e.last_used) >= self.idle_timeout)
                    .map(|(u, _)| *u)
                    .collect();
                for user in &gone {
                    entries.remove(user);
                }
                gone
            }
            Err(_) => return,
        };

        for user in expired {
            // A failed send means nobody is subscribed, which is not an error.
            let _ = self.events.send(CoreEvent::SessionExpired { user });
        }
    }

    fn info_at(&self, user: UserId, last_used: Instant) -> SessionInfo {
        let remaining = self
            .idle_timeout
            .saturating_sub(Instant::now().duration_since(last_used));
        SessionInfo {
            user,
            expires_at: SystemTime::now() + remaining,
            remaining,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn sessions(timeout: Duration) -> (Sessions, broadcast::Receiver<CoreEvent>) {
        let (tx, rx) = broadcast::channel(16);
        (Sessions::new(timeout, tx), rx)
    }

    fn mnemonic() -> Mnemonic {
        keys::parse(MNEMONIC).expect("the test vector parses")
    }

    #[test]
    fn an_unlocked_session_reports_time_remaining() {
        let (s, _rx) = sessions(Duration::from_secs(600));
        let user = UserId::new();
        assert!(s.info(user).is_none());

        let info = s.unlock(user, mnemonic());
        assert_eq!(info.user, user);
        assert!(info.remaining <= Duration::from_secs(600));
        assert!(s.is_unlocked(user));
    }

    #[test]
    fn locking_drops_the_session_immediately() {
        let (s, _rx) = sessions(Duration::from_secs(600));
        let user = UserId::new();
        s.unlock(user, mnemonic());
        s.lock(user);
        assert!(!s.is_unlocked(user));
        // Idempotent.
        s.lock(user);
    }

    #[test]
    fn an_idle_session_expires_and_announces_itself() {
        let (s, mut rx) = sessions(Duration::ZERO);
        let user = UserId::new();
        s.unlock(user, mnemonic());

        // A zero idle timeout means the next read finds it expired.
        assert!(s.info(user).is_none());
        assert!(!s.is_unlocked(user));

        match rx.try_recv() {
            Ok(CoreEvent::SessionExpired { user: u }) => assert_eq!(u, user),
            other => panic!("expected SessionExpired, got {other:?}"),
        }
    }

    #[test]
    fn one_users_session_is_not_another_users() {
        let (s, _rx) = sessions(Duration::from_secs(600));
        let a = UserId::new();
        let b = UserId::new();
        s.unlock(a, mnemonic());
        assert!(s.is_unlocked(a));
        assert!(!s.is_unlocked(b));
        assert!(s.with_mnemonic(b, |_| ()).is_none());
    }

    #[test]
    fn the_seed_is_reachable_only_through_a_closure() {
        let (s, _rx) = sessions(Duration::from_secs(600));
        let user = UserId::new();
        s.unlock(user, mnemonic());

        // What a signing path does: borrow it, use it, never keep it.
        let word_count = s
            .with_mnemonic(user, |m| m.words().count())
            .expect("the session is open");
        assert_eq!(word_count, 12);
    }

    #[test]
    fn using_a_session_refreshes_its_idle_clock() {
        let (s, _rx) = sessions(Duration::from_millis(120));
        let user = UserId::new();
        s.unlock(user, mnemonic());

        std::thread::sleep(Duration::from_millis(80));
        assert!(
            s.with_mnemonic(user, |_| ()).is_some(),
            "still inside the window"
        );

        std::thread::sleep(Duration::from_millis(80));
        // Without the refresh above this would already be gone.
        assert!(s.is_unlocked(user), "the clock restarted on use");

        std::thread::sleep(Duration::from_millis(160));
        assert!(!s.is_unlocked(user), "idle past the timeout");
    }
}
