//! Drafted payments awaiting a human (PLAN.md §3a rule 4).
//!
//! Core never blocks on a person. `quote_send` prices a payment and parks the
//! PSBT here behind a `QuoteId`; `confirm_send` looks it up, re-validates it,
//! and signs. The PSBT itself never crosses the boundary — the front end holds
//! an opaque id and nothing else (§8.5), which is what makes a replayed button
//! harmless: it can only reference a quote core will re-check or reject.
//!
//! Expiry is the second half of that guarantee. A confirm card left open for an
//! hour quotes a feerate that was true an hour ago, and signing it would pay a
//! stale fee (§8.1).

use crate::{
    error::{CoreError, Result},
    service::types::{QuoteId, SendQuote, UserId},
};
use bdk_wallet::bitcoin::Psbt;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

/// §8.1: five minutes.
pub const QUOTE_TTL: Duration = Duration::from_secs(5 * 60);

struct Parked {
    owner: UserId,
    quote: SendQuote,
    psbt: Psbt,
    created: Instant,
}

#[derive(Default)]
pub struct Quotes {
    parked: Mutex<HashMap<QuoteId, Parked>>,
}

impl Quotes {
    pub fn new() -> Self {
        Quotes::default()
    }

    pub fn park(&self, owner: UserId, quote: SendQuote, psbt: Psbt) {
        if let Ok(mut parked) = self.parked.lock() {
            parked.insert(
                quote.id,
                Parked {
                    owner,
                    quote,
                    psbt,
                    created: Instant::now(),
                },
            );
        }
    }

    /// Take a quote for signing, checking both things that make an id safe to
    /// put in callback data: that it belongs to this user, and that it is still
    /// fresh.
    ///
    /// Taking rather than borrowing means a double-tapped Confirm button
    /// cannot broadcast twice.
    pub fn take(&self, owner: UserId, id: QuoteId) -> Result<(SendQuote, Psbt)> {
        self.sweep();

        let mut parked = self
            .parked
            .lock()
            .map_err(|_| CoreError::Storage("quote store poisoned".into()))?;

        match parked.get(&id) {
            // A quote belonging to someone else is reported exactly as an
            // expired one: an attacker guessing ids learns nothing either way.
            Some(entry) if entry.owner != owner => Err(CoreError::QuoteExpired),
            Some(entry) if entry.created.elapsed() >= QUOTE_TTL => {
                parked.remove(&id);
                Err(CoreError::QuoteExpired)
            }
            Some(_) => {
                let entry = parked.remove(&id).ok_or(CoreError::QuoteExpired)?;
                Ok((entry.quote, entry.psbt))
            }
            None => Err(CoreError::QuoteExpired),
        }
    }

    /// Check that a quote is takeable, without consuming it.
    ///
    /// `confirm_send` calls this *before* it verifies the PIN. Two things fall
    /// out, and both matter now that every signature costs a freshly typed PIN
    /// rather than only the ones made while locked:
    ///
    /// * a user who mistypes still has their quote, so the retry is a retry and
    ///   not a whole new `/send`;
    /// * a confirm card that expired while they were typing says so, instead of
    ///   spending one of their attempts against the lockout counter first.
    ///
    /// The answer is deliberately the same for a foreign, unknown and stale id,
    /// exactly as in [`Quotes::take`]. There is a window between `peek` and
    /// `take`, and it is harmless: `take` re-checks and stays the only consumer.
    pub fn peek(&self, owner: UserId, id: QuoteId) -> Result<()> {
        self.sweep();

        let parked = self
            .parked
            .lock()
            .map_err(|_| CoreError::Storage("quote store poisoned".into()))?;

        match parked.get(&id) {
            Some(entry) if entry.owner == owner && entry.created.elapsed() < QUOTE_TTL => Ok(()),
            _ => Err(CoreError::QuoteExpired),
        }
    }

    /// Drop a quote the user cancelled, releasing the inputs it reserved.
    pub fn cancel(&self, owner: UserId, id: QuoteId) {
        if let Ok(mut parked) = self.parked.lock()
            && parked.get(&id).is_some_and(|e| e.owner == owner)
        {
            parked.remove(&id);
        }
    }

    /// Drop everything past its expiry. Called on every access, so an abandoned
    /// quote cannot keep a PSBT alive for the life of the process.
    pub fn sweep(&self) {
        if let Ok(mut parked) = self.parked.lock() {
            parked.retain(|_, e| e.created.elapsed() < QUOTE_TTL);
        }
    }

    pub fn len(&self) -> usize {
        self.parked.lock().map(|p| p.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk_wallet::bitcoin::{
        Amount, FeeRate, Network, Transaction, absolute::LockTime, transaction::Version,
    };
    use std::time::SystemTime;

    fn psbt() -> Psbt {
        Psbt::from_unsigned_tx(Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![],
            output: vec![],
        })
        .expect("an empty unsigned tx is a valid PSBT")
    }

    fn quote(id: QuoteId) -> SendQuote {
        SendQuote {
            id,
            recipient: "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu"
                .parse::<bdk_wallet::bitcoin::Address<_>>()
                .expect("parses")
                .require_network(Network::Bitcoin)
                .expect("mainnet"),
            amount: Amount::from_sat(50_000),
            fee: Amount::from_sat(1_410),
            fee_rate: FeeRate::from_sat_per_vb(6).expect("valid"),
            total: Amount::from_sat(51_410),
            change: Amount::from_sat(212_590),
            is_payjoin: false,
            payjoin_uri: None,
            replaces: None,
            expires_at: SystemTime::now() + QUOTE_TTL,
        }
    }

    #[test]
    fn a_quote_can_be_taken_once() {
        let quotes = Quotes::new();
        let user = UserId::new();
        let id = QuoteId::new();
        quotes.park(user, quote(id), psbt());

        assert!(quotes.take(user, id).is_ok());
        // §8.1: a double-tapped Confirm must not broadcast twice.
        assert!(matches!(
            quotes.take(user, id),
            Err(CoreError::QuoteExpired)
        ));
    }

    /// `peek` is what lets `confirm_send` check the card before it spends a
    /// PIN attempt on it, so it must not consume what it reports on.
    #[test]
    fn peeking_at_a_quote_leaves_it_takeable() {
        let quotes = Quotes::new();
        let user = UserId::new();
        let id = QuoteId::new();
        quotes.park(user, quote(id), psbt());

        assert!(quotes.peek(user, id).is_ok());
        assert!(quotes.peek(user, id).is_ok(), "peeking is not taking");
        assert_eq!(quotes.len(), 1);

        assert!(quotes.take(user, id).is_ok());
        assert!(matches!(
            quotes.peek(user, id),
            Err(CoreError::QuoteExpired)
        ));
    }

    /// The same answer as `take` for every way of not being allowed to have
    /// it, so `peek` leaks nothing `take` would not have leaked anyway.
    #[test]
    fn peeking_at_someone_elses_quote_says_only_that_it_is_gone() {
        let quotes = Quotes::new();
        let alice = UserId::new();
        let mallory = UserId::new();
        let id = QuoteId::new();
        quotes.park(alice, quote(id), psbt());

        assert!(matches!(
            quotes.peek(mallory, id),
            Err(CoreError::QuoteExpired)
        ));
        assert!(matches!(
            quotes.peek(alice, QuoteId::new()),
            Err(CoreError::QuoteExpired)
        ));
        assert!(quotes.peek(alice, id).is_ok());
    }

    /// §10: a QuoteId belonging to another user is rejected.
    #[test]
    fn another_users_quote_is_not_reachable() {
        let quotes = Quotes::new();
        let alice = UserId::new();
        let mallory = UserId::new();
        let id = QuoteId::new();
        quotes.park(alice, quote(id), psbt());

        assert!(matches!(
            quotes.take(mallory, id),
            Err(CoreError::QuoteExpired)
        ));
        // And Alice's quote survived the attempt.
        assert!(quotes.take(alice, id).is_ok());
    }

    #[test]
    fn an_unknown_id_is_rejected_the_same_way_as_an_expired_one() {
        let quotes = Quotes::new();
        assert!(matches!(
            quotes.take(UserId::new(), QuoteId::new()),
            Err(CoreError::QuoteExpired)
        ));
    }

    #[test]
    fn cancelling_releases_the_quote_and_only_its_owners() {
        let quotes = Quotes::new();
        let alice = UserId::new();
        let mallory = UserId::new();
        let id = QuoteId::new();
        quotes.park(alice, quote(id), psbt());

        // Someone else's cancel does nothing.
        quotes.cancel(mallory, id);
        assert_eq!(quotes.len(), 1);

        quotes.cancel(alice, id);
        assert!(quotes.is_empty());
    }

    #[test]
    fn quotes_from_different_users_do_not_collide() {
        let quotes = Quotes::new();
        let alice = UserId::new();
        let bob = UserId::new();
        let a = QuoteId::new();
        let b = QuoteId::new();
        quotes.park(alice, quote(a), psbt());
        quotes.park(bob, quote(b), psbt());

        assert!(quotes.take(alice, a).is_ok());
        assert!(quotes.take(bob, b).is_ok());
    }
}
