//! The typed error surface of `wallet-core` (PLAN.md §3a).
//!
//! Every variant is data, never prose: a front end renders it (`ui::render_error`
//! in the bot is a `match` over this type), so no user-facing sentence, emoji or
//! HTML is ever written here. Secrets — the mnemonic and `BITRPC_API_KEY` — must
//! never reach a variant's fields, because `Display` output is logged.

use bdk_wallet::bitcoin::{Amount, FeeRate, Network};
use std::time::{Duration, SystemTime};

/// Everything a `WalletService` call can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CoreError {
    // --- configuration ---
    /// A required setting is absent from the environment.
    #[error("missing configuration: {0}")]
    MissingConfig(&'static str),

    /// A setting is present but cannot be parsed into the type it must have.
    #[error("invalid configuration for {key}: {reason}")]
    InvalidConfig { key: &'static str, reason: String },

    /// `NETWORK=bitcoin` was requested without the acknowledgement in §4.
    #[error("mainnet requires MAINNET_I_UNDERSTAND_RISK=true")]
    MainnetNotAcknowledged,

    /// The backend reports a different chain than `NETWORK` asks for (§4).
    #[error("network mismatch: configured {configured}, backend reports {backend}")]
    NetworkMismatch {
        configured: Network,
        backend: String,
    },

    // --- backend ---
    /// A chain-source failure, already classified by `rpc::bitrpc` (§4b).
    #[error(transparent)]
    Backend(#[from] BackendError),

    // --- wallet lifecycle ---
    /// The user has no wallet in this network's namespace.
    #[error("no wallet for this user")]
    NoWallet,

    /// The user already has a wallet; create/restore would overwrite a seed.
    #[error("a wallet already exists for this user")]
    WalletExists,

    /// The supplied words are not a valid BIP39 mnemonic.
    #[error("invalid mnemonic")]
    InvalidMnemonic,

    /// The three words read back did not match the challenge core issued (§5).
    #[error("the words read back do not match")]
    BackupCheckFailed,

    /// The PIN does not satisfy the policy in §5 (6–8 digits).
    #[error("PIN must be {min}-{max} digits")]
    InvalidPin { min: usize, max: usize },

    /// The PIN was wrong. `remaining` counts attempts before a lockout.
    #[error("incorrect PIN, {remaining} attempt(s) remaining")]
    WrongPin { remaining: u32 },

    /// Too many wrong PINs; locked out until the given instant (§5).
    #[error("locked out after repeated failures")]
    PinLocked { until: SystemTime },

    /// No unlocked session and no PIN supplied.
    #[error("wallet is locked")]
    Locked,

    /// The requested rescan is deeper than `MAINNET_MAX_RESCAN_BLOCKS` (§6).
    #[error("restore would rescan {depth} blocks, over the {max} limit")]
    RestoreTooDeep { depth: u32, max: u32, eta: Duration },

    // --- spending ---
    /// The input is neither an address nor a BIP21 URI for this network.
    #[error("not a valid address or BIP21 URI for {network}")]
    InvalidPaymentTarget { network: Network },

    /// Coin selection could not cover amount plus fee.
    #[error("insufficient funds: need {needed}, have {available}")]
    InsufficientFunds { needed: Amount, available: Amount },

    /// The rate is under `mempoolminfee` (or the configured floor) (§6).
    #[error("fee rate below the floor")]
    FeeBelowFloor { given: FeeRate, floor: FeeRate },

    /// The amount is over the configured `MAX_SEND_SATS` safety cap (§4).
    #[error("amount {amount} exceeds the configured cap {cap}")]
    OverSendCap { amount: Amount, cap: Amount },

    /// The quote expired, or never belonged to this user (§3a).
    #[error("this quote is no longer valid")]
    QuoteExpired,

    /// `/bumpfee` cannot apply here, for a reason the user can act on (§6).
    #[error("cannot bump this fee: {reason}")]
    CannotBumpFee { reason: FeeBumpRefusal },

    /// The node refused the transaction; `reason` is its own words (§6 step 5).
    #[error("broadcast rejected: {reason}")]
    BroadcastRejected { reason: String },

    // --- payjoin ---
    /// A payjoin session failed a typestate check or its endpoint (§7).
    #[error("payjoin: {0}")]
    Payjoin(String),

    /// No such payjoin session for this user.
    #[error("no such payjoin session")]
    NoSuchSession,

    // --- policy ---
    /// The call is not available on the active network (e.g. `/mine`).
    #[error("not supported on {network}")]
    UnsupportedOnNetwork { network: Network },

    // --- infrastructure ---
    /// Persistence failed.
    #[error("storage: {0}")]
    Storage(String),

    /// A wallet-level failure from BDK that has no more specific variant.
    #[error("wallet: {0}")]
    Wallet(String),

    /// Cryptographic failure in the vault (§5) — never carries key material.
    #[error("vault: {0}")]
    Crypto(&'static str),
}

/// Why a fee bump is impossible.
///
/// These are ordinary outcomes, not faults: a confirmed transaction simply
/// cannot be replaced. Lumping them under a generic wallet error told the user
/// "something went wrong on this server", which is both wrong and unactionable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeeBumpRefusal {
    /// Already in a block. Replace-by-fee only works before that.
    AlreadyConfirmed,
    /// This wallet has never seen that transaction.
    NotFound,
    /// Its inputs did not signal replaceability, so no node will accept a
    /// replacement.
    NotReplaceable,

    /// The rate asked for does not beat the original by enough.
    ///
    /// A replacement must pay the original's rate plus the incremental relay
    /// fee, so on a chain with one flat preset the obvious choice — the same
    /// rate as last time — is always refused. `required` is the lowest rate
    /// that will be accepted, and drafting *at* it succeeds.
    RateTooLow { required: FeeRate },

    /// The replacement's absolute fee does not beat the original's.
    ///
    /// Distinct from `RateTooLow`: a replacement can be smaller than what it
    /// replaces, and then a higher rate still buys a lower total.
    AbsoluteFeeTooLow { required: Amount },
}

impl std::fmt::Display for FeeBumpRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FeeBumpRefusal::AlreadyConfirmed => f.write_str("already confirmed"),
            FeeBumpRefusal::NotFound => f.write_str("not found in this wallet"),
            FeeBumpRefusal::NotReplaceable => f.write_str("does not signal replace-by-fee"),
            FeeBumpRefusal::RateTooLow { required } => {
                write!(f, "needs at least {} sat/vB", required.to_sat_per_vb_ceil())
            }
            FeeBumpRefusal::AbsoluteFeeTooLow { required } => {
                write!(f, "needs at least {} sat in fees", required.to_sat())
            }
        }
    }
}

/// Chain-source failures, kept separate so the bot can retry only where retrying
/// helps: 429 and 502 are transient, 401 and 403 are not (§4b).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackendError {
    /// HTTP 401 — no `X-API-Key` was sent.
    #[error("backend rejected the request: no API key")]
    MissingApiKey,

    /// HTTP 403 — the key is invalid, or the method is not on the allowlist (§4b).
    #[error("backend refused: invalid API key, or method not permitted")]
    Forbidden { method: String },

    /// HTTP 429 — the 100 req/min budget is spent.
    #[error("backend rate limit reached")]
    RateLimited { retry_after: Option<Duration> },

    /// HTTP 502 — the upstream node is unavailable.
    #[error("backend node unavailable")]
    NodeUnavailable,

    /// The node answered with a JSON-RPC error object.
    #[error("rpc error {code}: {message}")]
    Rpc { code: i32, message: String },

    /// Transport-level failure (DNS, TLS, timeout). Never contains the API key.
    #[error("transport: {0}")]
    Transport(String),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, CoreError>;

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        CoreError::Storage(e.to_string())
    }
}
