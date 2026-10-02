//! The data types that cross the boundary (PLAN.md §3a).
//!
//! Everything here is *data*: numbers, ids, typed enums. Not one field is a
//! formatted string, an emoji, a network badge, a QR image or a table — those
//! belong to whichever front end is rendering (§3a rule 2). If you are tempted
//! to add a `pub display: String`, add the fields it would be built from instead.

use bdk_wallet::bitcoin::address::NetworkUnchecked;
use bdk_wallet::bitcoin::{Address, Amount, BlockHash, FeeRate, Network, Txid};
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

/// An opaque wallet user. Core has never heard of a Telegram id (§3a rule 3):
/// the bot owns the `tg_id -> UserId` map and translates on every call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(pub uuid::Uuid);

impl UserId {
    pub fn new() -> Self {
        UserId(uuid::Uuid::new_v4())
    }
}

impl Default for UserId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for UserId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(UserId(uuid::Uuid::parse_str(s)?))
    }
}

macro_rules! opaque_ulid {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $name(pub ulid::Ulid);

        impl $name {
            pub fn new() -> Self {
                $name(ulid::Ulid::generate())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = ulid::DecodeError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok($name(s.parse()?))
            }
        }
    };
}

opaque_ulid! {
    /// Handle on a drafted payment. The PSBT itself never leaves core (§3a); the
    /// front end holds only this id, which is short enough for Telegram's 64-byte
    /// callback data (§8.5) and which core re-validates on every use.
    QuoteId
}

opaque_ulid! {
    /// Handle on a payjoin session (§7).
    SessionId
}

/// A PIN on its way into core and nowhere else. The bot forwards what the user
/// typed and never learns whether it decrypted anything (§3a rule 5).
#[derive(Clone)]
pub struct Pin(Zeroizing<String>);

impl Pin {
    pub fn new(pin: impl Into<String>) -> Self {
        Pin(Zeroizing::new(pin.into()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    /// §5: 6–8 digits.
    pub fn is_well_formed(&self) -> bool {
        let n = self.0.chars().count();
        (6..=8).contains(&n) && self.0.chars().all(|c| c.is_ascii_digit())
    }
}

impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pin(<redacted>)")
    }
}

/// Backend health for `/status` (§8.2).
#[derive(Debug, Clone)]
pub struct BackendStatus {
    pub network: Network,
    pub tip_height: u32,
    pub tip_hash: BlockHash,
    pub latency: Duration,
    /// Calls spent against the shared per-minute budget; `None` on regtest,
    /// where there is no budget to spend (§4).
    pub calls_used: Option<u32>,
    pub call_budget: Option<u32>,
    /// True once the backend is refusing or lagging (§4b: 429 / 502).
    pub degraded: bool,
}

/// A freshly created wallet. The mnemonic is shown once and the challenge makes
/// the "confirm 3 words" step core's rule rather than the front end's invention
/// (§3a, §5).
pub struct NewWallet {
    pub user: UserId,
    pub mnemonic: Zeroizing<String>,
    /// Zero-based indices of the words the user must read back.
    pub confirm_challenge: [u8; 3],
    pub birthday: u32,
}

impl std::fmt::Debug for NewWallet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewWallet")
            .field("user", &self.user)
            .field("mnemonic", &"<redacted>")
            .field("confirm_challenge", &self.confirm_challenge)
            .field("birthday", &self.birthday)
            .finish()
    }
}

/// What a restore would cost, and whether core will allow it (§6). The depth cap
/// is core policy; the front end renders the verdict and nothing more.
#[derive(Debug, Clone)]
pub struct RestorePlan {
    pub birthday: u32,
    pub tip: u32,
    pub depth: u32,
    pub eta: Duration,
    pub verdict: RestoreVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreVerdict {
    /// Shallow enough to run without saying anything.
    Proceed,
    /// Allowed, but the user should see the ETA first.
    Warn,
    /// Over `MAINNET_MAX_RESCAN_BLOCKS` (§6).
    Refuse { max: u32 },
}

/// An open session (§5). The front end can see *that* one is open and how long
/// is left; it can never see what is in it.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub user: UserId,
    pub expires_at: SystemTime,
    pub remaining: Duration,
}

/// A revealed address (§6). `index` and `used` come from BDK's own SPK index.
#[derive(Debug, Clone)]
pub struct AddressInfo {
    pub address: Address,
    pub index: u32,
    pub used: bool,
    pub received: Amount,
    /// BIP21 for this address, built by core because it is protocol, not prose.
    pub bip21: String,
}

/// `wallet.balance()` flattened (§6). On mainnet `untrusted_pending` is always
/// zero: BitRPC has no `getrawmempool`, so unconfirmed *incoming* is invisible
/// until a block lands (§4b). `unconfirmed_incoming_visible` says so, in data,
/// so the front end can warn without knowing why.
#[derive(Debug, Clone, Copy)]
pub struct BalanceView {
    pub confirmed: Amount,
    pub trusted_pending: Amount,
    pub untrusted_pending: Amount,
    pub immature: Amount,
    pub total: Amount,
    pub unconfirmed_incoming_visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxDirection {
    Incoming,
    Outgoing,
    /// A self-transfer, or a payjoin where both sides contributed.
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxStatus {
    Unconfirmed,
    Confirmed { height: u32, confirmations: u32 },
}

/// One row of `/history` (§6, §8.2).
#[derive(Debug, Clone)]
pub struct TxSummary {
    pub txid: Txid,
    pub direction: TxDirection,
    /// Net effect on this wallet, always positive; `direction` carries the sign.
    pub amount: Amount,
    pub fee: Option<Amount>,
    pub status: TxStatus,
    pub timestamp: Option<SystemTime>,
}

/// `/tx <txid>` (§8.2).
#[derive(Debug, Clone)]
pub struct TxDetail {
    pub summary: TxSummary,
    pub fee_rate: Option<FeeRate>,
    pub inputs: usize,
    pub outputs: usize,
    pub vsize: u64,
}

/// A parsed `/send` argument: a bare address, or a BIP21 that may carry an
/// amount, a label and a `pj=` payjoin endpoint (§6 step 1, §7).
#[derive(Debug, Clone)]
pub struct PaymentTarget {
    pub address: Address<NetworkUnchecked>,
    pub amount: Option<Amount>,
    pub label: Option<String>,
    /// Present when the URI carried `pj=` — the sender path of §7.
    pub payjoin_endpoint: Option<String>,
    /// `pjos=0`: the receiver forbids output substitution (§7).
    pub output_substitution_disabled: bool,
}

/// What one bitcoin is worth, and who says so (§6).
///
/// A convenience, never an input: nothing in this crate prices a transaction
/// from it. The front end multiplies and formats — core does not know what a
/// dollar sign looks like (§3a rule 2).
///
/// `source` is here for the same reason `FeeSource` is: a number with no
/// provenance invites more trust than a third-party quote deserves.
#[derive(Debug, Clone, PartialEq)]
pub struct FiatPrice {
    pub usd_per_btc: f64,
    pub source: String,
    pub fetched_at: SystemTime,
}

impl FiatPrice {
    /// What `amount` is worth, in whole cents.
    ///
    /// Cents rather than a float so the front end formats an integer and
    /// cannot print `$0.30000000000000004`. Saturating, because a balance
    /// large enough to overflow a `u64` of cents is not a balance.
    pub fn cents(&self, amount: Amount) -> u64 {
        let dollars = amount.to_btc() * self.usd_per_btc;
        if !dollars.is_finite() || dollars <= 0.0 {
            return 0;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let cents = (dollars * 100.0).round();
        if cents >= u64::MAX as f64 {
            u64::MAX
        } else {
            cents as u64
        }
    }
}

/// Where a fee preset came from, so the front end can say so honestly (§6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeeSource {
    /// `estimatesmartfee` at our own node — regtest only.
    Node,
    /// An external estimator, because BitRPC blocks `estimatesmartfee` (§4b).
    External { name: String },
    /// Nothing answered; the front end must prompt for a manual rate and say why.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeeLabel {
    Fast,
    Normal,
    Slow,
}

/// Flattened so the front end's fee keyboard is the same code on both networks:
/// it draws the presets it is handed and nothing more (§6).
#[derive(Debug, Clone)]
pub struct FeeOptions {
    pub presets: Vec<(FeeLabel, FeeRate)>,
    /// `mempoolminfee`, or the configured hard floor. Nothing below this is
    /// accepted, estimated or typed (§6).
    pub floor: FeeRate,
    pub source: FeeSource,
    pub allows_custom: bool,
}

/// What a replacement for a stuck transaction may pay (§6).
///
/// `fees` is an ordinary [`FeeOptions`] with the replacement minimum already
/// applied — presets below it removed rather than raised, and `floor` set to
/// it — so a front end draws it with the code it already has and cannot offer
/// a rate the network would refuse.
#[derive(Debug, Clone)]
pub struct BumpOptions {
    pub replaces: Txid,
    /// What the stuck transaction paid.
    pub current: FeeRate,
    pub fees: FeeOptions,
}

/// What the user asked for, before core has priced it.
#[derive(Debug, Clone)]
pub struct SendRequest {
    pub target: PaymentTarget,
    /// The URI exactly as the user supplied it, so the payjoin sender can hand
    /// it back to the protocol unchanged (§7).
    pub raw: String,
    pub amount: SendAmount,
    pub fee_rate: FeeRate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendAmount {
    Exact(Amount),
    /// Drain the wallet (`/send <addr> max`).
    Max,
}

/// A priced, drafted payment (§3a). Plain numbers only — the PSBT stays inside
/// core, keyed by `id`. `expires_at` is why an abandoned confirm card cannot be
/// replayed later at a stale feerate (§8.1).
#[derive(Debug, Clone)]
pub struct SendQuote {
    pub id: QuoteId,
    pub recipient: Address,
    pub amount: Amount,
    pub fee: Amount,
    pub fee_rate: FeeRate,
    pub total: Amount,
    pub change: Amount,
    pub is_payjoin: bool,
    /// The BIP21 URI the payjoin sender needs. Kept inside core with the PSBT;
    /// the front end never has to hold it.
    pub payjoin_uri: Option<String>,
    /// Set for `/bumpfee`, so the front end can title the same card differently.
    pub replaces: Option<Txid>,
    pub expires_at: SystemTime,
}

/// The result of `confirm_send` (§6 step 5).
#[derive(Debug, Clone)]
pub struct Broadcast {
    pub txid: Txid,
    pub amount: Amount,
    pub fee: Amount,
    /// True when the broadcast tx was the payjoin, false when it fell back (§7).
    pub payjoin: bool,
}

/// What `/pj_receive` hands back. Core returns the URI string; turning it into a
/// QR is the front end's job (§7 step 2).
#[derive(Debug, Clone)]
pub struct PayjoinReceipt {
    pub session_id: SessionId,
    pub bip21: String,
    pub expires_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayjoinRole {
    Sender,
    Receiver,
}

/// The state machine of §7, as data. Every transition is published as a
/// `CoreEvent::Payjoin` so any front end can follow along.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayjoinState {
    /// Receiver: waiting for an Original PSBT to arrive at the directory.
    Waiting,
    /// An Original PSBT arrived and is being checked.
    ProposalReceived,
    /// Our contribution is committed and the proposal has been posted back.
    ProposalSent,
    Completed {
        txid: Txid,
    },
    /// The payjoin did not finish, so the fallback transaction was broadcast.
    FellBack {
        txid: Txid,
    },
    Expired,
    Cancelled,
    Failed {
        reason: String,
    },
}

/// One row of `/pj_sessions` (§8.2).
#[derive(Debug, Clone)]
pub struct PayjoinSessionView {
    pub id: SessionId,
    pub role: PayjoinRole,
    pub state: PayjoinState,
    pub amount: Option<Amount>,
    pub created_at: SystemTime,
    pub expires_at: SystemTime,
}

/// One page of a paginated read. Pagination is core's, so every front end pages
/// identically and none has to hold a cursor it could get wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    pub index: u32,
    pub size: u32,
}

impl Page {
    pub const DEFAULT_SIZE: u32 = 10;

    pub fn new(index: u32) -> Self {
        Page {
            index,
            size: Self::DEFAULT_SIZE,
        }
    }

    pub fn offset(&self) -> usize {
        (self.index as usize).saturating_mul(self.size as usize)
    }
}

impl Default for Page {
    fn default() -> Self {
        Page::new(0)
    }
}

#[derive(Debug, Clone)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub page: Page,
    pub total: usize,
}

impl<T> Paged<T> {
    pub fn total_pages(&self) -> u32 {
        if self.page.size == 0 {
            return 1;
        }
        self.total.div_ceil(self.page.size as usize).max(1) as u32
    }

    pub fn has_next(&self) -> bool {
        self.page.index + 1 < self.total_pages()
    }

    pub fn has_prev(&self) -> bool {
        self.page.index > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_must_be_six_to_eight_digits() {
        assert!(Pin::new("123456").is_well_formed());
        assert!(Pin::new("12345678").is_well_formed());
        assert!(!Pin::new("12345").is_well_formed());
        assert!(!Pin::new("123456789").is_well_formed());
        assert!(!Pin::new("12345a").is_well_formed());
    }

    #[test]
    fn a_pin_is_redacted_from_debug_output() {
        let rendered = format!("{:?}", Pin::new("864213"));
        assert!(!rendered.contains("864213"));
    }

    #[test]
    fn quote_ids_round_trip_through_callback_data() {
        let id = QuoteId::new();
        let text = id.to_string();
        // §8.5: the whole `action:subject:arg` payload must fit 64 bytes.
        assert!(text.len() <= 26, "ULID is {} bytes", text.len());
        assert_eq!(text.parse::<QuoteId>().expect("round trips"), id);
    }

    #[test]
    fn pagination_arithmetic_holds() {
        let p = Paged {
            items: vec![0u8; 10],
            page: Page::new(0),
            total: 25,
        };
        assert_eq!(p.total_pages(), 3);
        assert!(p.has_next());
        assert!(!p.has_prev());

        let last = Paged {
            items: vec![0u8; 5],
            page: Page::new(2),
            total: 25,
        };
        assert!(!last.has_next());
        assert!(last.has_prev());
    }

    #[test]
    fn an_empty_result_still_has_one_page() {
        let p: Paged<u8> = Paged {
            items: vec![],
            page: Page::new(0),
            total: 0,
        };
        assert_eq!(p.total_pages(), 1);
        assert!(!p.has_next());
    }
}
