//! What a front end subscribes to (PLAN.md §3a rule 6).
//!
//! Core pushes events; it does not send messages. Every event carries the
//! `UserId` it concerns and nothing about how it should read — the bot's
//! `notify.rs` decides which chat it goes to and what words it gets, and a CLI
//! front end prints the same values. Core has no idea either exists.

use super::types::{PayjoinState, SessionId, TxStatus, UserId};
use bdk_wallet::bitcoin::{Amount, Txid};

/// The broadcast channel's depth. A slow subscriber lags rather than blocking
/// the chain sync: `broadcast::Receiver` reports `Lagged`, and the front end can
/// resynchronise by calling `balance()`.
pub const EVENT_CHANNEL_CAPACITY: usize = 512;

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum CoreEvent {
    /// Funds arrived. On mainnet this only ever fires with a confirmed status:
    /// BitRPC has no `getrawmempool`, so unconfirmed incoming is invisible (§4b).
    IncomingTx {
        user: UserId,
        txid: Txid,
        amount: Amount,
        status: TxStatus,
    },

    /// A transaction this wallet knows about gained confirmations (§6).
    TxConfirmed {
        user: UserId,
        txid: Txid,
        confirmations: u32,
    },

    /// Rescan progress, for the single status message a front end edits in place
    /// rather than spamming (§8.1).
    SyncProgress { user: UserId, height: u32, tip: u32 },

    /// The idle timer ran out and the seed was dropped (§5).
    SessionExpired { user: UserId },

    /// A payjoin session changed state (§7).
    Payjoin {
        user: UserId,
        session: SessionId,
        state: PayjoinState,
    },

    /// The chain source became slow, rate-limited or unavailable — not about any
    /// one user, so a front end broadcasts or suppresses it as it sees fit (§4b).
    BackendHealth(BackendHealth),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendHealth {
    Healthy,
    /// Slow, or answering errors that retrying may fix.
    Degraded {
        reason: String,
    },
    /// The shared 100 req/min budget is spent; commands will lag (§4).
    RateLimited,
}

impl CoreEvent {
    /// The user this event concerns, if any. `BackendHealth` concerns everyone,
    /// which is exactly why it returns `None`.
    pub fn user(&self) -> Option<UserId> {
        match self {
            CoreEvent::IncomingTx { user, .. }
            | CoreEvent::TxConfirmed { user, .. }
            | CoreEvent::SyncProgress { user, .. }
            | CoreEvent::SessionExpired { user }
            | CoreEvent::Payjoin { user, .. } => Some(*user),
            CoreEvent::BackendHealth(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_user_events_carry_their_user_and_health_does_not() {
        let u = UserId::new();
        let ev = CoreEvent::SessionExpired { user: u };
        assert_eq!(ev.user(), Some(u));
        assert_eq!(
            CoreEvent::BackendHealth(BackendHealth::RateLimited).user(),
            None
        );
    }
}
