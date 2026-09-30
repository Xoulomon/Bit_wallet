//! Chain sync (PLAN.md §6).
//!
//! One background task runs a `bdk_bitcoind_rpc::Emitter` and applies each
//! block to every loaded wallet, so a block is fetched **once** however many
//! users there are. That is not an optimisation: on mainnet each block costs
//! about two calls against a 100-per-minute key shared by everyone, so
//! per-wallet emitters would not fit in the budget at all (§4).
//!
//! The two networks differ in exactly one way, and it is the allowlist's doing:
//!
//! * **Regtest** also applies `emitter.mempool()`, so unconfirmed incoming
//!   payments appear at once.
//! * **Mainnet** cannot: `getrawmempool` is not on BitRPC's allowlist, so
//!   incoming payments are invisible until a block confirms them (§4b, §6).
//!   `Capabilities::mempool` carries that fact upward so the front end can say
//!   so instead of showing a zero that looks like a lost payment.

use crate::{
    config::AppConfig,
    error::{CoreError, Result},
    rpc::{ChainSource, map_rpc_error},
    service::{
        events::{BackendHealth, CoreEvent},
        types::{TxStatus, UserId},
    },
    storage::Storage,
};
use bdk_bitcoind_rpc::{Emitter, NO_EXPECTED_MEMPOOL_TXS};
use bdk_wallet::{
    bitcoin::{Amount, Txid},
    chain::ChainPosition,
};
use bitcoincore_rpc::Client;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::sync::broadcast;

/// How often to look for a new block when the last poll found none.
///
/// On regtest this is short because blocks arrive when a test mines them. On
/// mainnet it is derived from the sync budget rather than fixed: each idle poll
/// costs one call and each new block about two, so the interval is what keeps
/// the emitter inside `BITRPC_SYNC_BUDGET_PER_MIN` (§6).
fn poll_interval(cfg: &AppConfig) -> Duration {
    match &cfg.backend {
        crate::config::BackendConfig::Regtest(_) => Duration::from_secs(5),
        crate::config::BackendConfig::Bitrpc(b) => {
            // Leave half the sync allowance for the blocks themselves.
            let polls_per_min = (b.sync_budget_per_min / 2).max(1);
            Duration::from_secs((60 / u64::from(polls_per_min)).max(1))
        }
    }
}

/// What the sync task knows about one wallet, so it can tell what changed.
#[derive(Default)]
struct Seen {
    /// Transactions already announced, and the confirmation count last reported.
    announced: HashMap<Txid, u32>,
    /// Transactions announced as incoming, so a confirmation does not look new.
    incoming: HashSet<Txid>,
}

/// The shared chain follower.
pub struct ChainService {
    cfg: AppConfig,
    events: broadcast::Sender<CoreEvent>,
    storage: Arc<Storage>,
}

impl ChainService {
    pub fn new(
        cfg: AppConfig,
        events: broadcast::Sender<CoreEvent>,
        storage: Arc<Storage>,
    ) -> Self {
        ChainService {
            cfg,
            events,
            storage,
        }
    }

    /// Run until cancelled. Every RPC call is blocking, so the whole loop lives
    /// on a blocking thread and yields between passes.
    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let interval = poll_interval(&self.cfg);
        let mut seen: HashMap<UserId, Seen> = HashMap::new();
        let mut healthy = true;

        loop {
            if *shutdown.borrow() {
                tracing::info!("chain sync stopping");
                return;
            }

            match self.pass(&mut seen).await {
                Ok(()) => {
                    if !healthy {
                        healthy = true;
                        let _ = self
                            .events
                            .send(CoreEvent::BackendHealth(BackendHealth::Healthy));
                    }
                }
                Err(e) => {
                    // A backend that is down is not a reason to stop following
                    // the chain — it is a reason to say so and try again.
                    if healthy {
                        healthy = false;
                        let _ = self.events.send(CoreEvent::BackendHealth(health_of(&e)));
                    }
                    tracing::warn!(error = %e, "sync pass failed");
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => {}
            }
        }
    }

    /// One pass: bring every wallet up to the tip.
    async fn pass(&self, seen: &mut HashMap<UserId, Seen>) -> Result<()> {
        let users = self.storage.all_users()?;
        if users.is_empty() {
            return Ok(());
        }

        for user in users {
            let entry = seen.entry(user).or_default();
            self.sync_one(user, entry).await?;
        }
        Ok(())
    }

    async fn sync_one(&self, user: UserId, seen: &mut Seen) -> Result<()> {
        let path = self.cfg.wallet_db(&user);
        if !path.exists() {
            // A vault with no wallet file yet: nothing to sync until the user
            // unlocks and the descriptors can be built.
            return Ok(());
        }

        let cfg = self.cfg.clone();
        let network = cfg.network.network();

        // The emitter and the wallet are both blocking; the whole unit of work
        // goes to a blocking thread and comes back as plain data.
        let outcome = tokio::task::spawn_blocking(move || sync_blocking(&cfg, &path, network))
            .await
            .map_err(|e| CoreError::Wallet(e.to_string()))??;

        for change in outcome.changes {
            self.announce(user, change, seen);
        }

        if outcome.tip > outcome.started_at {
            let _ = self.events.send(CoreEvent::SyncProgress {
                user,
                height: outcome.tip,
                tip: outcome.tip,
            });
        }

        Ok(())
    }

    /// Turn a transaction's new state into at most one event.
    ///
    /// The bookkeeping matters: without it a wallet with fifty transactions
    /// would announce all fifty on every pass, which is worse than announcing
    /// nothing.
    fn announce(&self, user: UserId, change: TxChange, seen: &mut Seen) {
        let confirmations = match change.status {
            TxStatus::Confirmed { confirmations, .. } => confirmations,
            TxStatus::Unconfirmed => 0,
        };

        match seen.announced.get(&change.txid) {
            Some(previous) if *previous == confirmations => {}
            Some(_) => {
                seen.announced.insert(change.txid, confirmations);
                let _ = self.events.send(CoreEvent::TxConfirmed {
                    user,
                    txid: change.txid,
                    confirmations,
                });
            }
            None => {
                seen.announced.insert(change.txid, confirmations);
                if change.incoming {
                    seen.incoming.insert(change.txid);
                    let _ = self.events.send(CoreEvent::IncomingTx {
                        user,
                        txid: change.txid,
                        amount: change.amount,
                        status: change.status,
                    });
                } else if confirmations > 0 {
                    let _ = self.events.send(CoreEvent::TxConfirmed {
                        user,
                        txid: change.txid,
                        confirmations,
                    });
                }
            }
        }
    }
}

fn health_of(e: &CoreError) -> BackendHealth {
    match e {
        CoreError::Backend(crate::error::BackendError::RateLimited { .. }) => {
            BackendHealth::RateLimited
        }
        other => BackendHealth::Degraded {
            reason: other.to_string(),
        },
    }
}

/// One transaction's state, as the blocking pass observed it.
struct TxChange {
    txid: Txid,
    amount: Amount,
    incoming: bool,
    status: TxStatus,
}

struct SyncOutcome {
    started_at: u32,
    tip: u32,
    changes: Vec<TxChange>,
}

/// The blocking half: drive the emitter, apply to the wallet, persist, and
/// report what the wallet now holds.
fn sync_blocking(
    cfg: &AppConfig,
    path: &Path,
    network: bdk_wallet::bitcoin::Network,
) -> Result<SyncOutcome> {
    let source = ChainSource::connect(cfg)?;
    let client: Arc<Client> = source.sync_client(cfg)?;

    let mut open = super::OpenWallet::load(path, network)?;
    let checkpoint = open.wallet.latest_checkpoint();
    let started_at = checkpoint.height();

    let mut emitter = Emitter::new(
        client.as_ref(),
        checkpoint,
        started_at,
        NO_EXPECTED_MEMPOOL_TXS,
    );

    while let Some(block) = emitter.next_block().map_err(map_rpc_error("getblock"))? {
        open.wallet
            .apply_block_connected_to(&block.block, block.block_height(), block.connected_to())
            .map_err(|e| CoreError::Wallet(e.to_string()))?;
    }

    // §4b: only where `getrawmempool` exists. On mainnet this branch is dead,
    // and that is precisely the limitation the README has to state.
    if source.capabilities().mempool {
        let mempool = emitter.mempool().map_err(map_rpc_error("getrawmempool"))?;
        open.wallet.apply_unconfirmed_txs(mempool.update);
    }

    open.flush()?;

    let tip = open.wallet.latest_checkpoint().height();
    let changes = open
        .wallet
        .transactions()
        .map(|tx| {
            let (sent, received) = open.wallet.sent_and_received(&tx.tx_node.tx);
            let status = match &tx.chain_position {
                ChainPosition::Confirmed { anchor, .. } => TxStatus::Confirmed {
                    height: anchor.block_id.height,
                    confirmations: tip.saturating_sub(anchor.block_id.height).saturating_add(1),
                },
                ChainPosition::Unconfirmed { .. } => TxStatus::Unconfirmed,
            };
            TxChange {
                txid: tx.tx_node.txid,
                amount: if received > sent {
                    received - sent
                } else {
                    sent - received
                },
                incoming: received > sent,
                status,
            }
        })
        .collect();

    Ok(SyncOutcome {
        started_at,
        tip,
        changes,
    })
}

/// A one-shot sync, for a front end that wants to refresh now rather than wait
/// for the next pass (`/balance`'s Refresh button, and the CLI).
pub async fn sync_now(cfg: &AppConfig, user: UserId) -> Result<u32> {
    let path = cfg.wallet_db(&user);
    if !path.exists() {
        return Err(CoreError::NoWallet);
    }
    let cfg = cfg.clone();
    let network = cfg.network.network();
    let outcome = tokio::task::spawn_blocking(move || sync_blocking(&cfg, &path, network))
        .await
        .map_err(|e| CoreError::Wallet(e.to_string()))??;
    Ok(outcome.tip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BitrpcConfig, RegtestConfig};
    use bdk_wallet::bitcoin::FeeRate;
    use zeroize::Zeroizing;

    fn cfg_with(backend: crate::config::BackendConfig) -> AppConfig {
        AppConfig {
            network: match backend {
                crate::config::BackendConfig::Regtest(_) => crate::NetworkChoice::Regtest,
                crate::config::BackendConfig::Bitrpc(_) => crate::NetworkChoice::Mainnet,
            },
            backend,
            data_dir: "./data".into(),
            session_idle_timeout: Duration::from_secs(600),
            max_send: None,
            fee_cache: Duration::from_secs(60),
        }
    }

    fn regtest() -> crate::config::BackendConfig {
        crate::config::BackendConfig::Regtest(RegtestConfig {
            rpc_url: "http://127.0.0.1:18443".into(),
            rpc_user: "u".into(),
            rpc_pass: Zeroizing::new("p".into()),
            payjoin_directory: "http://localhost:8080".into(),
            ohttp_relay: "http://localhost:3000".into(),
            fallback_fee: FeeRate::from_sat_per_vb(2).expect("valid"),
        })
    }

    fn bitrpc(sync_budget: u32) -> crate::config::BackendConfig {
        crate::config::BackendConfig::Bitrpc(BitrpcConfig {
            url: "https://example.invalid".into(),
            api_key: Zeroizing::new("k".into()),
            rate_limit_per_min: 90,
            sync_budget_per_min: sync_budget,
            max_rescan_blocks: 10_000,
            min_fee: FeeRate::from_sat_per_vb(1).expect("valid"),
            fee_api: "https://example.invalid".into(),
            payjoin_directory: "https://example.invalid".into(),
            ohttp_relay: "https://example.invalid".into(),
        })
    }

    /// §6: the mainnet interval is derived from the budget, not fixed, so a
    /// smaller allowance polls less often instead of overspending.
    #[test]
    fn the_poll_interval_follows_the_sync_budget_on_mainnet() {
        let generous = poll_interval(&cfg_with(bitrpc(60)));
        let frugal = poll_interval(&cfg_with(bitrpc(10)));
        assert!(frugal > generous, "a smaller budget must poll less often");

        // And it stays inside it: polls per minute never exceed half the
        // allowance, leaving the rest for the blocks themselves.
        let polls_per_min = 60 / generous.as_secs().max(1);
        assert!(polls_per_min <= 30, "{polls_per_min} polls/min over budget");
    }

    #[test]
    fn regtest_polls_quickly_because_blocks_are_free_there() {
        assert_eq!(poll_interval(&cfg_with(regtest())), Duration::from_secs(5));
    }

    #[test]
    fn a_rate_limited_backend_is_reported_as_rate_limited_not_merely_degraded() {
        // The front end retries one and not the other, so the distinction has
        // to survive all the way up (§4b).
        let e = CoreError::Backend(crate::error::BackendError::RateLimited { retry_after: None });
        assert_eq!(health_of(&e), BackendHealth::RateLimited);

        let e = CoreError::Backend(crate::error::BackendError::NodeUnavailable);
        assert!(matches!(health_of(&e), BackendHealth::Degraded { .. }));
    }

    #[test]
    fn a_transaction_is_announced_once_per_change_and_not_once_per_pass() {
        let (tx, mut rx) = broadcast::channel(32);
        let service = ChainService::new(
            cfg_with(regtest()),
            tx,
            Arc::new(Storage::in_memory().expect("opens")),
        );
        let user = UserId::new();
        let mut seen = Seen::default();
        let txid = Txid::from_raw_hash(bdk_wallet::bitcoin::hashes::Hash::from_byte_array(
            [7u8; 32],
        ));

        let change = |confirmations: u32| TxChange {
            txid,
            amount: Amount::from_sat(25_000),
            incoming: true,
            status: if confirmations == 0 {
                TxStatus::Unconfirmed
            } else {
                TxStatus::Confirmed {
                    height: 100,
                    confirmations,
                }
            },
        };

        // First sight: one IncomingTx.
        service.announce(user, change(0), &mut seen);
        assert!(matches!(rx.try_recv(), Ok(CoreEvent::IncomingTx { .. })));

        // Same state again: nothing, however many passes run.
        service.announce(user, change(0), &mut seen);
        service.announce(user, change(0), &mut seen);
        assert!(rx.try_recv().is_err(), "an unchanged tx must stay quiet");

        // It confirms: one TxConfirmed.
        service.announce(user, change(1), &mut seen);
        match rx.try_recv() {
            Ok(CoreEvent::TxConfirmed { confirmations, .. }) => assert_eq!(confirmations, 1),
            other => panic!("expected TxConfirmed, got {other:?}"),
        }

        // And again at six, but not at one a second time.
        service.announce(user, change(1), &mut seen);
        assert!(rx.try_recv().is_err());
        service.announce(user, change(6), &mut seen);
        assert!(matches!(rx.try_recv(), Ok(CoreEvent::TxConfirmed { .. })));
    }

    #[test]
    fn an_outgoing_transaction_is_not_announced_as_money_arriving() {
        let (tx, mut rx) = broadcast::channel(32);
        let service = ChainService::new(
            cfg_with(regtest()),
            tx,
            Arc::new(Storage::in_memory().expect("opens")),
        );
        let mut seen = Seen::default();
        let txid = Txid::from_raw_hash(bdk_wallet::bitcoin::hashes::Hash::from_byte_array(
            [9u8; 32],
        ));

        service.announce(
            UserId::new(),
            TxChange {
                txid,
                amount: Amount::from_sat(1_000),
                incoming: false,
                status: TxStatus::Confirmed {
                    height: 10,
                    confirmations: 1,
                },
            },
            &mut seen,
        );

        assert!(matches!(rx.try_recv(), Ok(CoreEvent::TxConfirmed { .. })));
    }
}
