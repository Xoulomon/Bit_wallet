//! `WalletService` — the only entry point a front end may call (PLAN.md §3a).
//!
//! This *is* the MVP: if a capability is not a method here, no front end can
//! offer it. Nothing below (`onchain`, `keys`, `payjoin`, `rpc`, `session`) is
//! reachable from `crates/bot/`, which is what makes "a second front end" true
//! rather than aspirational — and what §10's boundary tests enforce.
//!
//! Two shapes carry the design:
//!
//! * Core never waits for a human (rule 4). Anything needing a decision is a
//!   pure `quote_*` the UI renders, then a `confirm_*` carrying an id. There are
//!   no callbacks into the UI and no blocking prompts.
//! * Core never sees a user's real identity (rule 3). Every method takes an
//!   opaque `UserId`; the bot owns the `tg_id` map and translates on every call.

pub mod events;
pub mod types;

use crate::{
    config::{AppConfig, BackendConfig},
    error::{CoreError, Result},
    keys,
    rpc::ChainSource,
    session::Sessions,
    storage::Storage,
};
use bdk_wallet::bitcoin::{Address, Amount, BlockHash, FeeRate, Network, Txid};
use events::{CoreEvent, EVENT_CHANNEL_CAPACITY};
use std::{sync::Arc, time::Duration};
use tokio::sync::broadcast;
use types::*;
use zeroize::Zeroizing;

pub struct WalletService {
    cfg: AppConfig,
    chain: ChainSource,
    events: broadcast::Sender<CoreEvent>,
    storage: Arc<Storage>,
    sessions: Sessions,
    /// Stops the sync task. Step 7 flushes persisters on the way out.
    shutdown: tokio::sync::watch::Sender<bool>,
    fees: Arc<crate::rpc::fees::FeePolicy>,
    /// Drafted payments awaiting a human (§3a rule 4).
    quotes: crate::onchain::quotes::Quotes,
    payjoin: Arc<crate::payjoin::persist::SessionStore>,
}

/// A fee bump keeps the original recipient; find it among the outputs that are
/// not ours.
fn wallet_recipient(draft: &crate::onchain::wallet::Draft, network: Network) -> Result<Address> {
    draft
        .psbt
        .unsigned_tx
        .output
        .iter()
        .find_map(|o| Address::from_script(&o.script_pubkey, network).ok())
        .ok_or_else(|| CoreError::Wallet("the replacement has no recognisable output".into()))
}

impl WalletService {
    /// Connect to the configured backend and verify it is serving the chain
    /// `NETWORK` asks for (§4). Fails fast: a mismatch here is cheaper than a
    /// wallet written into the wrong namespace.
    pub async fn new(cfg: AppConfig) -> Result<Arc<Self>> {
        let chain = ChainSource::connect(&cfg)?;

        // The RPC client is blocking, so the startup probe goes to a blocking
        // thread rather than stalling the runtime.
        let health = {
            let probe = ChainSource::connect(&cfg)?;
            tokio::task::spawn_blocking(move || probe.health_check())
                .await
                .map_err(|e| crate::error::CoreError::Wallet(e.to_string()))??
        };

        tracing::info!(
            network = %cfg.network.namespace(),
            tip = health.tip_height,
            latency_ms = health.latency.as_millis(),
            "backend reachable"
        );

        std::fs::create_dir_all(cfg.network_dir())
            .map_err(|e| crate::error::CoreError::Storage(e.to_string()))?;

        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let storage = Arc::new(Storage::open(&cfg.app_db())?);
        let sessions = Sessions::new(cfg.session_idle_timeout, events.clone());
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let cfg_for_fees = cfg.clone();
        let payjoin_db = cfg.network_dir().join("payjoin.sqlite");

        // One follower for every wallet: a block is fetched once, however many
        // users there are (§6).
        let syncer = crate::onchain::sync::ChainService::new(
            cfg.clone(),
            events.clone(),
            Arc::clone(&storage),
        );
        tokio::spawn(syncer.run(shutdown_rx));

        Ok(Arc::new(WalletService {
            cfg,
            chain,
            events,
            fees: Arc::new(crate::rpc::fees::FeePolicy::new(&cfg_for_fees)),
            storage,
            sessions,
            shutdown: shutdown_tx,
            quotes: crate::onchain::quotes::Quotes::new(),
            payjoin: Arc::new(crate::payjoin::persist::SessionStore::open(&payjoin_db)?),
        }))
    }

    /// Which chain this instance is bound to. Every front end reads this to
    /// stamp its network badge (§8.1) — core supplies the fact, not the badge.
    pub fn network(&self) -> Network {
        self.cfg.network.network()
    }

    /// Read-only view of the configuration, for front ends that must render a
    /// limit they did not set (the send cap, the idle timeout).
    pub fn config(&self) -> &AppConfig {
        &self.cfg
    }

    /// What this backend cannot do, as data rather than as a caveat in prose
    /// (§4b). A front end renders these into the limitations it shows the user.
    pub fn capabilities(&self) -> crate::rpc::Capabilities {
        self.chain.capabilities()
    }

    /// Tip, latency and the remaining call budget (§8.2 `/status`).
    pub async fn status(&self) -> Result<BackendStatus> {
        let chain = ChainSource::connect(&self.cfg)?;
        let health = tokio::task::spawn_blocking(move || chain.health_check())
            .await
            .map_err(|e| crate::error::CoreError::Wallet(e.to_string()))??;

        let (calls_used, call_budget) = match self.chain.budget() {
            Some(b) => (Some(b.used()), Some(b.limit())),
            None => (None, None),
        };

        Ok(BackendStatus {
            network: self.network(),
            tip_height: health.tip_height,
            tip_hash: health.tip_hash,
            latency: health.latency,
            calls_used,
            call_budget,
            degraded: false,
        })
    }

    /// Stop the background sync task and let it finish its current pass.
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// Bring this user's wallet to the tip now, rather than at the next pass.
    /// The Refresh button of §8.2, and what the CLI calls before it prints.
    pub async fn sync_now(&self, u: UserId) -> Result<u32> {
        crate::onchain::sync::sync_now(&self.cfg, u).await
    }

    /// Subscribe to `CoreEvent`s (§3a rule 6). Core pushes; the front end
    /// decides where each event goes and how it reads.
    pub fn subscribe(&self) -> broadcast::Receiver<CoreEvent> {
        self.events.subscribe()
    }

    /// Publish an event. Failure means nobody is listening, which is not an
    /// error — a headless run has no subscribers.
    // Wired up by `ChainService` in Step 4; defined here so the channel has
    // exactly one writer.
    #[allow(dead_code)]
    pub(crate) fn emit(&self, event: CoreEvent) {
        let _ = self.events.send(event);
    }

    // ---------------------------------------------------------------- lifecycle

    /// MVP 1. Generates a mnemonic, seals it under the PIN, and returns it once
    /// together with the three word indices the user must read back (§5).
    ///
    /// The birthday is the current tip, so creating a wallet on mainnet is
    /// instant: there is nothing before it to scan (§6).
    pub async fn create_wallet(&self, u: UserId, pin: &Pin) -> Result<NewWallet> {
        self.check_pin_policy(pin)?;
        if self.storage.wallet_exists(u)? {
            return Err(CoreError::WalletExists);
        }

        let mnemonic = keys::generate()?;
        let birthday = self.tip_height().await?;
        let parsed = keys::parse(&mnemonic)?;
        let challenge = keys::backup_challenge(parsed.words().count());

        let vault = crate::crypto::seal(pin.expose(), &mnemonic)?;
        self.storage
            .insert_wallet(u, &vault, birthday, Some(challenge))?;

        // Open the session straight away: the user has just proved they hold
        // the PIN, and making them type it twice teaches nothing.
        self.sessions.unlock(u, parsed);

        tracing::info!(user = %u, birthday, "wallet created");

        Ok(NewWallet {
            user: u,
            mnemonic,
            confirm_challenge: challenge,
            birthday,
        })
    }

    /// Verify the "confirm 3 words" challenge from `NewWallet` (§5).
    ///
    /// Core issued the challenge and core remembers it: the answers are checked
    /// against the words core asked for, not against three the front end chose.
    /// Requires an open session, because the check needs the mnemonic and the
    /// user unlocked one moments ago by creating the wallet.
    pub async fn confirm_backup(&self, u: UserId, answers: [String; 3]) -> Result<()> {
        let Some(challenge) = self.storage.backup_challenge(u)? else {
            // Nothing outstanding — either already confirmed, or restored.
            return Ok(());
        };

        let correct = self
            .sessions
            .with_mnemonic(u, |m| keys::check_backup(m, challenge, &answers))
            .ok_or(CoreError::Locked)?;

        if !correct {
            return Err(CoreError::BackupCheckFailed);
        }

        self.storage.mark_backup_confirmed(u)?;
        Ok(())
    }

    /// MVP 2, first half: what a restore from this birthday would cost, and
    /// whether core will allow it at all (§6).
    pub async fn restore_preflight(&self, birthday: Option<u32>) -> Result<RestorePlan> {
        let tip = self.tip_height().await?;
        Ok(self.plan_restore(tip, birthday))
    }

    /// MVP 2. The words are deleted from the chat by the front end the moment
    /// they arrive; they reach core as `Zeroizing` and are never logged.
    pub async fn restore_wallet(
        &self,
        u: UserId,
        words: Zeroizing<String>,
        birthday: Option<u32>,
        pin: &Pin,
    ) -> Result<()> {
        self.check_pin_policy(pin)?;
        if self.storage.wallet_exists(u)? {
            return Err(CoreError::WalletExists);
        }

        // Validate before anything is written, so a typo leaves no half-made
        // wallet behind.
        let mnemonic = keys::parse(&words)?;

        let tip = self.tip_height().await?;
        let plan = self.plan_restore(tip, birthday);
        if let RestoreVerdict::Refuse { max } = plan.verdict {
            return Err(CoreError::RestoreTooDeep {
                depth: plan.depth,
                max,
                eta: plan.eta,
            });
        }

        let normalised = Zeroizing::new(mnemonic.to_string());
        let vault = crate::crypto::seal(pin.expose(), &normalised)?;
        self.storage.insert_wallet(u, &vault, plan.birthday, None)?;
        // A restored wallet needs no backup quiz: the user already has the words.
        self.storage.mark_backup_confirmed(u)?;
        self.sessions.unlock(u, mnemonic);

        tracing::info!(user = %u, birthday = plan.birthday, depth = plan.depth, "wallet restored");
        Ok(())
    }

    /// `/export` (§8.2): reshow the mnemonic, PIN-gated.
    pub async fn export_mnemonic(&self, u: UserId, pin: &Pin) -> Result<Zeroizing<String>> {
        self.storage.unseal(u, pin.expose())
    }

    /// `/delete` (§8.2). Destroys the vault and this user's wallet state.
    ///
    /// The PIN is required even though the record is about to be destroyed:
    /// otherwise anyone who reached the chat could wipe a wallet whose owner
    /// had not written the words down.
    pub async fn delete_wallet(&self, u: UserId, pin: &Pin) -> Result<()> {
        let _ = self.storage.unseal(u, pin.expose())?;
        self.sessions.lock(u);
        self.storage.delete_wallet(u)?;

        let wallet_db = self.cfg.wallet_db(&u);
        if wallet_db.exists() {
            std::fs::remove_file(&wallet_db).map_err(|e| CoreError::Storage(e.to_string()))?;
        }

        tracing::info!(user = %u, "wallet deleted");
        Ok(())
    }

    pub fn wallet_exists(&self, u: UserId) -> Result<bool> {
        self.storage.wallet_exists(u)
    }

    // ------------------------------------------------------------------ session

    /// Open a session (§5). The seed stays in core until the idle timer expires
    /// or `lock` is called; the caller learns only that it worked.
    pub async fn unlock(&self, u: UserId, pin: &Pin) -> Result<SessionInfo> {
        let words = self.storage.unseal(u, pin.expose())?;
        let mnemonic = keys::parse(&words)?;
        Ok(self.sessions.unlock(u, mnemonic))
    }

    pub fn lock(&self, u: UserId) {
        self.sessions.lock(u);
    }

    /// Whether a session is open and how long is left — never its contents.
    pub fn session(&self, u: UserId) -> Option<SessionInfo> {
        self.sessions.info(u)
    }

    // ------------------------------------------------------------------ helpers

    /// Open this user's persisted wallet.
    ///
    /// Watch-only, and therefore PIN-free: the descriptors on disk are public
    /// (§5). A user who has a vault but no wallet file yet — the window between
    /// restore and the first sync — gets one built from their unlocked session.
    fn open_wallet(&self, u: UserId) -> Result<crate::onchain::OpenWallet> {
        let path = self.cfg.wallet_db(&u);
        let network = self.network();

        if path.exists() {
            return crate::onchain::OpenWallet::load(&path, network);
        }

        if !self.storage.wallet_exists(u)? {
            return Err(CoreError::NoWallet);
        }

        // The vault exists but the BDK file does not. Building it needs the
        // descriptors, which need the mnemonic — so this is the one read path
        // that wants an open session, and only once per wallet.
        self.sessions
            .with_mnemonic(u, |m| crate::onchain::OpenWallet::create(&path, m, network))
            .ok_or(CoreError::Locked)?
    }

    fn check_pin_policy(&self, pin: &Pin) -> Result<()> {
        if pin.is_well_formed() {
            Ok(())
        } else {
            Err(CoreError::InvalidPin { min: 6, max: 8 })
        }
    }

    /// The current tip, through the active backend.
    async fn tip_height(&self) -> Result<u32> {
        let chain = ChainSource::connect(&self.cfg)?;
        let health = tokio::task::spawn_blocking(move || chain.health_check())
            .await
            .map_err(|e| CoreError::Wallet(e.to_string()))??;
        Ok(health.tip_height)
    }

    fn plan_restore(&self, tip: u32, birthday: Option<u32>) -> RestorePlan {
        plan_restore(&self.cfg.backend, tip, birthday)
    }
}

/// The restore-depth policy of §6, as a free function so §10 can test the
/// arithmetic and the refusal without standing up a backend.
///
/// Regtest has no cap: there is nothing to scan, and a birthday of 0 on a
/// hundred-block chain costs nothing.
pub fn plan_restore(backend: &BackendConfig, tip: u32, birthday: Option<u32>) -> RestorePlan {
    {
        let birthday = birthday.unwrap_or(0).min(tip);
        let depth = tip.saturating_sub(birthday);

        // §6: ~2 calls per block at the sync budget, so roughly half the
        // budget in blocks per minute.
        let blocks_per_min = match backend {
            BackendConfig::Bitrpc(b) => (b.sync_budget_per_min / 2).max(1),
            BackendConfig::Regtest(_) => 600,
        };
        let eta = Duration::from_secs(u64::from(depth) * 60 / u64::from(blocks_per_min));

        let verdict = match backend {
            BackendConfig::Regtest(_) => RestoreVerdict::Proceed,
            BackendConfig::Bitrpc(b) if depth > b.max_rescan_blocks => RestoreVerdict::Refuse {
                max: b.max_rescan_blocks,
            },
            // Under ~2 000 blocks it proceeds silently; beyond that the user
            // should see the ETA before committing to it.
            BackendConfig::Bitrpc(_) if depth > 2_000 => RestoreVerdict::Warn,
            BackendConfig::Bitrpc(_) => RestoreVerdict::Proceed,
        };

        RestorePlan {
            birthday,
            tip,
            depth,
            eta,
            verdict,
        }
    }
}

impl WalletService {
    // -------------------------------------------------- watch-only (no PIN)

    /// MVP 4. `/receive`: the next unused external address (§6).
    pub async fn next_address(&self, u: UserId) -> Result<AddressInfo> {
        let mut wallet = self.open_wallet(u)?;
        let info = wallet.next_address()?;
        Ok(info)
    }

    /// MVP 4. `/addresses`: revealed addresses with used/unused status (§6).
    pub async fn addresses(&self, u: UserId, page: Page) -> Result<Paged<AddressInfo>> {
        self.open_wallet(u)?.addresses(page)
    }

    /// MVP 6. Confirmed, pending and immature (§6).
    ///
    /// `unconfirmed_incoming_visible` carries §4b's consequence as data: on
    /// mainnet there is no `getrawmempool`, so a payment that has not made it
    /// into a block is not merely zero — it is unseen, and the front end has to
    /// be able to say which.
    pub async fn balance(&self, u: UserId) -> Result<BalanceView> {
        let visible = self.chain.capabilities().mempool;
        Ok(self.open_wallet(u)?.balance(visible))
    }

    /// MVP 7. `/history`, newest first (§6).
    pub async fn history(&self, u: UserId, page: Page) -> Result<Paged<TxSummary>> {
        let tip = self.tip_height().await?;
        self.open_wallet(u)?.history(tip, page)
    }

    /// MVP 10. `/tx <txid>`; confirmations come from the wallet's own
    /// `ChainPosition`, not from `getrawtransaction`, so nothing depends on
    /// `txindex` at the shared node (§6).
    pub async fn tx(&self, u: UserId, txid: Txid) -> Result<TxDetail> {
        let tip = self.tip_height().await?;
        self.open_wallet(u)?.tx(txid, tip)
    }

    // ----------------------------------------------------------------- spending

    /// Parse an address or a BIP21 URI, including `pj=` and `pjos=0` (§6, §7).
    /// Pure and synchronous: a front end can validate as the user types.
    pub fn parse_payment(&self, input: &str) -> Result<PaymentTarget> {
        crate::onchain::payment::parse(input, self.network())
    }

    /// The fee presets, floor and source for this network, flattened so the
    /// front end's fee keyboard is the same code on both (§6).
    pub async fn fee_options(&self) -> Result<FeeOptions> {
        let fees = Arc::clone(&self.fees);
        tokio::task::spawn_blocking(move || fees.options())
            .await
            .map_err(|e| CoreError::Wallet(e.to_string()))?
    }

    /// MVP 8, first half. Builds and prices a PSBT that stays inside core; the
    /// caller gets plain numbers and an id (§3a rule 4).
    pub async fn quote_send(&self, u: UserId, req: SendRequest) -> Result<SendQuote> {
        let recipient = req
            .target
            .address
            .clone()
            .require_network(self.network())
            .map_err(|_| CoreError::InvalidPaymentTarget {
                network: self.network(),
            })?;

        // Every rate, estimated or typed, meets the floor (§6).
        let fees = Arc::clone(&self.fees);
        let floor = tokio::task::spawn_blocking(move || fees.floor())
            .await
            .map_err(|e| CoreError::Wallet(e.to_string()))?;
        crate::rpc::fees::check_rate(req.fee_rate, floor)?;

        if let (SendAmount::Exact(amount), Some(cap)) = (req.amount, self.cfg.max_send)
            && amount > cap
        {
            return Err(CoreError::OverSendCap { amount, cap });
        }

        let mut wallet = self.open_wallet(u)?;
        let draft = wallet.draft(&recipient, req.amount, req.fee_rate)?;

        // The cap again, now that `max` has a number.
        if let Some(cap) = self.cfg.max_send
            && draft.amount > cap
        {
            return Err(CoreError::OverSendCap {
                amount: draft.amount,
                cap,
            });
        }

        let quote = SendQuote {
            id: QuoteId::new(),
            recipient,
            amount: draft.amount,
            fee: draft.fee,
            fee_rate: req.fee_rate,
            total: draft.amount + draft.fee,
            change: draft.change,
            is_payjoin: req.target.payjoin_endpoint.is_some(),
            payjoin_uri: req
                .target
                .payjoin_endpoint
                .as_ref()
                .map(|_| req.raw.clone()),
            replaces: None,
            expires_at: std::time::SystemTime::now() + crate::onchain::quotes::QUOTE_TTL,
        };

        self.quotes.park(u, quote.clone(), draft.psbt);
        Ok(quote)
    }

    /// MVP 8 + 9, second half. Signs the quoted PSBT and broadcasts it.
    ///
    /// The quote is re-validated here — expiry and ownership are checked by the
    /// store — so a replayed button cannot move money (§8.5). A `Pin` opens a
    /// session first, which is why a user who unlocked a minute ago is not
    /// asked again.
    pub async fn confirm_send(&self, u: UserId, q: QuoteId, auth: Auth) -> Result<Broadcast> {
        if let Auth::Pin(pin) = &auth {
            self.unlock(u, pin).await?;
        }

        let (quote, mut psbt) = self.quotes.take(u, q)?;

        let mut wallet = self.open_wallet(u)?;
        self.sessions
            .with_mnemonic(u, |m| wallet.sign(&mut psbt, m))
            .ok_or(CoreError::Locked)??;

        // §7: a `pj=` target goes to the payjoin sender, whose fallback is this
        // very transaction. A payjoin that fails is never a payment that fails.
        if quote.is_payjoin
            && let Some(uri) = quote.payjoin_uri.clone()
        {
            let mnemonic = self
                .sessions
                .with_mnemonic(u, |m| m.clone())
                .ok_or(CoreError::Locked)?;

            let outcome = crate::payjoin::send::attempt(crate::payjoin::send::Attempt {
                cfg: self.cfg.clone(),
                store: Arc::clone(&self.payjoin),
                events: self.events.clone(),
                user: u,
                original: psbt,
                uri,
                fee_rate: quote.fee_rate,
                mnemonic,
            })
            .await?;

            tracing::info!(user = %u, txid = %outcome.txid, payjoin = outcome.payjoin, "broadcast");
            return Ok(Broadcast {
                txid: outcome.txid,
                amount: quote.amount,
                fee: quote.fee,
                payjoin: outcome.payjoin,
            });
        }

        let tx = psbt
            .extract_tx()
            .map_err(|e| CoreError::Wallet(e.to_string()))?;

        self.broadcast(&tx).await?;

        // §6 step 6: insert it as unconfirmed at once. On mainnet this is the
        // only way an outgoing payment shows before a block lands.
        let txid = tx.compute_txid();
        wallet.record_broadcast(tx)?;

        tracing::info!(user = %u, %txid, "broadcast");

        Ok(Broadcast {
            txid,
            amount: quote.amount,
            fee: quote.fee,
            payjoin: false,
        })
    }

    /// Drop a quote the user cancelled, so its PSBT and its inputs are released.
    pub async fn cancel_quote(&self, u: UserId, q: QuoteId) {
        self.quotes.cancel(u, q);
    }

    /// `/bumpfee` (§6). Returns the same `SendQuote` shape, so the confirm card
    /// is the same code — only the header differs.
    pub async fn bump_fee(&self, u: UserId, txid: Txid, rate: FeeRate) -> Result<SendQuote> {
        let fees = Arc::clone(&self.fees);
        let floor = tokio::task::spawn_blocking(move || fees.floor())
            .await
            .map_err(|e| CoreError::Wallet(e.to_string()))?;
        crate::rpc::fees::check_rate(rate, floor)?;

        let mut wallet = self.open_wallet(u)?;
        let draft = wallet.draft_fee_bump(txid, rate)?;

        let recipient = wallet_recipient(&draft, self.network())?;
        let quote = SendQuote {
            id: QuoteId::new(),
            recipient,
            amount: draft.amount,
            fee: draft.fee,
            fee_rate: rate,
            total: draft.amount + draft.fee,
            change: draft.change,
            is_payjoin: false,
            payjoin_uri: None,
            replaces: Some(txid),
            expires_at: std::time::SystemTime::now() + crate::onchain::quotes::QUOTE_TTL,
        };

        self.quotes.park(u, quote.clone(), draft.psbt);
        Ok(quote)
    }

    /// Send a raw transaction, with the dry run where one exists (§6 step 5).
    async fn broadcast(&self, tx: &bdk_wallet::bitcoin::Transaction) -> Result<()> {
        let cfg = self.cfg.clone();
        let raw = bdk_wallet::bitcoin::consensus::encode::serialize_hex(tx);
        let dry_run = self.chain.capabilities().test_mempool_accept;

        tokio::task::spawn_blocking(move || -> Result<()> {
            use bitcoincore_rpc::RpcApi as _;
            let source = ChainSource::connect(&cfg)?;
            let client = source.client();

            // Regtest gets the real dry run; mainnet has no testmempoolaccept
            // on the allowlist, so its rejection arrives from the broadcast
            // itself — which is why that reason is surfaced verbatim (§4b).
            if dry_run {
                let results = client
                    .test_mempool_accept(std::slice::from_ref(&raw))
                    .map_err(crate::rpc::map_rpc_error("testmempoolaccept"))?;
                if let Some(first) = results.first()
                    && !first.allowed
                {
                    return Err(CoreError::BroadcastRejected {
                        reason: first
                            .reject_reason
                            .clone()
                            .unwrap_or_else(|| "rejected by the node".into()),
                    });
                }
            }

            client.send_raw_transaction(raw).map_err(|e| {
                match crate::rpc::map_rpc_error("sendrawtransaction")(e) {
                    // The node's own words are the useful part here (§6).
                    CoreError::Backend(crate::error::BackendError::Rpc { message, .. }) => {
                        CoreError::BroadcastRejected { reason: message }
                    }
                    other => other,
                }
            })?;
            Ok(())
        })
        .await
        .map_err(|e| CoreError::Wallet(e.to_string()))?
    }

    // ------------------------------------------------------------------ payjoin

    /// `/pj_receive` (§7). Returns the BIP21 string; rendering it as a QR is the
    /// front end's job — core never returns an image (§3a rule 2).
    ///
    /// Needs an open session: the receiver has to contribute an input and sign
    /// the proposal, and both want the seed.
    pub async fn payjoin_receive(&self, u: UserId, amount: Amount) -> Result<PayjoinReceipt> {
        let mnemonic = self
            .sessions
            .with_mnemonic(u, |m| m.clone())
            .ok_or(CoreError::Locked)?;

        let address = self.open_wallet(u)?.next_address()?.address;

        let started = crate::payjoin::receive::start(
            &self.cfg,
            Arc::clone(&self.payjoin),
            u,
            amount,
            address,
        )
        .await?;

        // The polling task lives in core, so any front end — or none — sees the
        // session through to its end (§7 step 3).
        tokio::spawn(crate::payjoin::receive::run(
            crate::payjoin::receive::Context {
                cfg: self.cfg.clone(),
                store: Arc::clone(&self.payjoin),
                events: self.events.clone(),
                user: u,
                session: started.session,
                mnemonic,
            },
        ));

        Ok(PayjoinReceipt {
            session_id: started.session,
            bip21: started.bip21,
            expires_at: started.expires_at,
        })
    }

    pub async fn payjoin_sessions(&self, u: UserId) -> Result<Vec<PayjoinSessionView>> {
        let rows = self.payjoin.for_user(u)?;
        Ok(rows
            .into_iter()
            .map(|row| PayjoinSessionView {
                id: row.id,
                role: row.role,
                state: if row.closed && row.state == "Waiting" {
                    PayjoinState::Cancelled
                } else {
                    crate::payjoin::receive::state_from_label(&row.state)
                },
                amount: None,
                created_at: row.created_at,
                expires_at: row.expires_at,
            })
            .collect())
    }

    pub async fn payjoin_cancel(&self, u: UserId, id: SessionId) -> Result<()> {
        let row = self.payjoin.row(id)?.ok_or(CoreError::NoSuchSession)?;
        // Another user's session id reads exactly like one that does not exist.
        if row.user != u {
            return Err(CoreError::NoSuchSession);
        }

        self.payjoin.set_state(id, "Cancelled")?;
        self.payjoin.close(id)?;
        self.emit(CoreEvent::Payjoin {
            user: u,
            session: id,
            state: PayjoinState::Cancelled,
        });
        Ok(())
    }

    // ------------------------------------------------------------- regtest only

    /// `/mine` (§8.2). Core refuses off regtest; the front end decides *who* may
    /// call it, core decides *whether it exists*.
    pub async fn mine(&self, blocks: u32, to: Option<Address>) -> Result<Vec<BlockHash>> {
        if !self.chain.capabilities().mining {
            return Err(CoreError::UnsupportedOnNetwork {
                network: self.network(),
            });
        }

        let cfg = self.cfg.clone();
        tokio::task::spawn_blocking(move || -> Result<Vec<BlockHash>> {
            use bitcoincore_rpc::RpcApi as _;
            let source = ChainSource::connect(&cfg)?;
            let client = source.client();

            let address = match to {
                Some(a) => a,
                None => client
                    .get_new_address(None, None)
                    .map_err(crate::rpc::map_rpc_error("getnewaddress"))?
                    .require_network(cfg.network.network())
                    .map_err(|e| CoreError::Wallet(e.to_string()))?,
            };

            client
                .generate_to_address(u64::from(blocks), &address)
                .map_err(crate::rpc::map_rpc_error("generatetoaddress"))
        })
        .await
        .map_err(|e| CoreError::Wallet(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BitrpcConfig, RegtestConfig};

    fn bitrpc(max_rescan: u32) -> BackendConfig {
        BackendConfig::Bitrpc(BitrpcConfig {
            url: "https://example.invalid".into(),
            api_key: Zeroizing::new("k".into()),
            rate_limit_per_min: 90,
            sync_budget_per_min: 60,
            max_rescan_blocks: max_rescan,
            min_fee: FeeRate::from_sat_per_vb(1).expect("a valid rate"),
            fee_api: "https://example.invalid".into(),
            payjoin_directory: "https://example.invalid".into(),
            ohttp_relay: "https://example.invalid".into(),
        })
    }

    fn regtest() -> BackendConfig {
        BackendConfig::Regtest(RegtestConfig {
            rpc_url: "http://127.0.0.1:18443".into(),
            rpc_user: "polaruser".into(),
            rpc_pass: Zeroizing::new("polarpass".into()),
            payjoin_directory: "http://localhost:8080".into(),
            ohttp_relay: "http://localhost:3000".into(),
            fallback_fee: FeeRate::from_sat_per_vb(2).expect("a valid rate"),
        })
    }

    #[test]
    fn a_shallow_restore_proceeds_without_comment() {
        let plan = plan_restore(&bitrpc(10_000), 900_000, Some(899_500));
        assert_eq!(plan.depth, 500);
        assert_eq!(plan.verdict, RestoreVerdict::Proceed);
    }

    #[test]
    fn a_deep_but_allowed_restore_warns_with_an_eta() {
        let plan = plan_restore(&bitrpc(10_000), 900_000, Some(895_000));
        assert_eq!(plan.depth, 5_000);
        assert_eq!(plan.verdict, RestoreVerdict::Warn);
        // §6: ~30 blocks/min at the default sync budget, so 5 000 blocks is
        // about two and a half hours — worth saying before it starts.
        assert!(plan.eta >= Duration::from_secs(2 * 3600));
    }

    /// §6: beyond the cap it refuses, quoting the computed ETA and the maximum.
    #[test]
    fn a_restore_past_the_cap_is_refused_with_the_numbers_that_justify_it() {
        let plan = plan_restore(&bitrpc(10_000), 900_000, Some(400_000));
        assert_eq!(plan.depth, 500_000);
        match plan.verdict {
            RestoreVerdict::Refuse { max } => assert_eq!(max, 10_000),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // A full SegWit-era rescan is hundreds of hours, which is why the cap
        // exists rather than a progress bar.
        assert!(plan.eta > Duration::from_secs(100 * 3600));
    }

    #[test]
    fn the_cap_is_exact_rather_than_approximate() {
        let at_limit = plan_restore(&bitrpc(10_000), 900_000, Some(890_000));
        assert_eq!(at_limit.depth, 10_000);
        assert_eq!(
            at_limit.verdict,
            RestoreVerdict::Warn,
            "the cap itself is allowed"
        );

        let one_over = plan_restore(&bitrpc(10_000), 900_000, Some(889_999));
        assert!(matches!(one_over.verdict, RestoreVerdict::Refuse { .. }));
    }

    #[test]
    fn regtest_has_no_cap() {
        let plan = plan_restore(&regtest(), 900_000, Some(0));
        assert_eq!(plan.depth, 900_000);
        assert_eq!(plan.verdict, RestoreVerdict::Proceed);
    }

    #[test]
    fn a_missing_birthday_means_scan_from_genesis() {
        let plan = plan_restore(&regtest(), 101, None);
        assert_eq!(plan.birthday, 0);
        assert_eq!(plan.depth, 101);
    }

    #[test]
    fn a_birthday_in_the_future_is_clamped_to_the_tip() {
        // A user who mistypes a height must not end up with a negative depth
        // or a wallet that skips its own history.
        let plan = plan_restore(&regtest(), 101, Some(500_000));
        assert_eq!(plan.birthday, 101);
        assert_eq!(plan.depth, 0);
    }
}
