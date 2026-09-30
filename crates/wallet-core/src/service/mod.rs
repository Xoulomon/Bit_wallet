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

use crate::{config::AppConfig, error::Result, rpc::ChainSource};
use bdk_wallet::bitcoin::{Address, Amount, BlockHash, FeeRate, Network, Txid};
use events::{CoreEvent, EVENT_CHANNEL_CAPACITY};
use std::sync::Arc;
use tokio::sync::broadcast;
use types::*;
use zeroize::Zeroizing;

pub struct WalletService {
    cfg: AppConfig,
    chain: ChainSource,
    events: broadcast::Sender<CoreEvent>,
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

        Ok(Arc::new(WalletService { cfg, chain, events }))
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
    pub async fn create_wallet(&self, _u: UserId, _pin: &Pin) -> Result<NewWallet> {
        todo!("Step 3: keys + vault")
    }

    /// Verify the "confirm 3 words" challenge from `NewWallet` (§5). Core's rule,
    /// not the front end's invention.
    pub async fn confirm_backup(&self, _u: UserId, _answers: [String; 3]) -> Result<()> {
        todo!("Step 3: keys + vault")
    }

    /// MVP 2, first half: what a restore from this birthday would cost, and
    /// whether core will allow it at all (§6).
    pub async fn restore_preflight(&self, _birthday: Option<u32>) -> Result<RestorePlan> {
        todo!("Step 3: keys + vault")
    }

    /// MVP 2. The words are deleted from the chat by the front end the moment
    /// they arrive; they reach core as `Zeroizing` and are never logged.
    pub async fn restore_wallet(
        &self,
        _u: UserId,
        _words: Zeroizing<String>,
        _birthday: Option<u32>,
        _pin: &Pin,
    ) -> Result<()> {
        todo!("Step 3: keys + vault")
    }

    /// `/export` (§8.2): reshow the mnemonic, PIN-gated.
    pub async fn export_mnemonic(&self, _u: UserId, _pin: &Pin) -> Result<Zeroizing<String>> {
        todo!("Step 3: keys + vault")
    }

    /// `/delete` (§8.2). Destroys the vault and this user's wallet state.
    pub async fn delete_wallet(&self, _u: UserId, _pin: &Pin) -> Result<()> {
        todo!("Step 3: keys + vault")
    }

    pub fn wallet_exists(&self, _u: UserId) -> Result<bool> {
        todo!("Step 3: keys + vault")
    }

    // ------------------------------------------------------------------ session

    /// Open a session (§5). The seed stays in core until the idle timer expires
    /// or `lock` is called; the caller learns only that it worked.
    pub async fn unlock(&self, _u: UserId, _pin: &Pin) -> Result<SessionInfo> {
        todo!("Step 3: session cache")
    }

    pub fn lock(&self, _u: UserId) {
        todo!("Step 3: session cache")
    }

    /// Whether a session is open and how long is left — never its contents.
    pub fn session(&self, _u: UserId) -> Option<SessionInfo> {
        todo!("Step 3: session cache")
    }

    // -------------------------------------------------- watch-only (no PIN)

    /// MVP 4. `/receive`: the next unused external address (§6).
    pub async fn next_address(&self, _u: UserId) -> Result<AddressInfo> {
        todo!("Step 4: on-chain core")
    }

    /// MVP 4. `/addresses`: revealed addresses with used/unused status (§6).
    pub async fn addresses(&self, _u: UserId, _page: Page) -> Result<Paged<AddressInfo>> {
        todo!("Step 4: on-chain core")
    }

    /// MVP 6. Confirmed, pending and immature (§6).
    pub async fn balance(&self, _u: UserId) -> Result<BalanceView> {
        todo!("Step 4: on-chain core")
    }

    /// MVP 7. `/history`, newest first (§6).
    pub async fn history(&self, _u: UserId, _page: Page) -> Result<Paged<TxSummary>> {
        todo!("Step 4: on-chain core")
    }

    /// MVP 10. `/tx <txid>`; confirmations come from the wallet's own
    /// `ChainPosition`, not from `getrawtransaction`, so nothing depends on
    /// `txindex` at the shared node (§6).
    pub async fn tx(&self, _u: UserId, _txid: Txid) -> Result<TxDetail> {
        todo!("Step 4: on-chain core")
    }

    // ----------------------------------------------------------------- spending

    /// Parse an address or a BIP21 URI, including `pj=` and `pjos=0` (§6, §7).
    /// Pure and synchronous: a front end can validate as the user types.
    pub fn parse_payment(&self, _input: &str) -> Result<PaymentTarget> {
        todo!("Step 5: send")
    }

    /// The fee presets, floor and source for this network, flattened so the
    /// front end's fee keyboard is the same code on both (§6).
    pub async fn fee_options(&self) -> Result<FeeOptions> {
        todo!("Step 5: send")
    }

    /// MVP 8, first half. Builds and prices a PSBT that stays inside core; the
    /// caller gets plain numbers and an id (§3a rule 4).
    pub async fn quote_send(&self, _u: UserId, _req: SendRequest) -> Result<SendQuote> {
        todo!("Step 5: send")
    }

    /// MVP 8 + 9, second half. Signs the quoted PSBT and broadcasts it.
    /// Re-validates the quote: expiry and ownership are checked here, so a
    /// replayed button cannot move money (§8.5).
    pub async fn confirm_send(&self, _u: UserId, _q: QuoteId, _auth: Auth) -> Result<Broadcast> {
        todo!("Step 5: send")
    }

    /// Drop a quote the user cancelled, so its PSBT and its inputs are released.
    pub async fn cancel_quote(&self, _u: UserId, _q: QuoteId) {
        todo!("Step 5: send")
    }

    /// `/bumpfee` (§6). Returns the same `SendQuote` shape, so the confirm card
    /// is the same code — only the header differs.
    pub async fn bump_fee(&self, _u: UserId, _txid: Txid, _rate: FeeRate) -> Result<SendQuote> {
        todo!("Step 5: send")
    }

    // ------------------------------------------------------------------ payjoin

    /// `/pj_receive` (§7). Returns the BIP21 string; rendering it as a QR is the
    /// front end's job — core never returns an image (§3a rule 2).
    pub async fn payjoin_receive(&self, _u: UserId, _amount: Amount) -> Result<PayjoinReceipt> {
        todo!("Step 6: payjoin")
    }

    pub async fn payjoin_sessions(&self, _u: UserId) -> Result<Vec<PayjoinSessionView>> {
        todo!("Step 6: payjoin")
    }

    pub async fn payjoin_cancel(&self, _u: UserId, _id: SessionId) -> Result<()> {
        todo!("Step 6: payjoin")
    }

    // ------------------------------------------------------------- regtest only

    /// `/mine` (§8.2). Core refuses off regtest; the front end decides *who* may
    /// call it, core decides *whether it exists*.
    pub async fn mine(&self, _blocks: u32, _to: Option<Address>) -> Result<Vec<BlockHash>> {
        todo!("Step 5: send (regtest mining helper)")
    }
}
