//! A non-custodial, multi-user Bitcoin wallet library (PLAN.md §3a).
//!
//! This crate is the wallet. A Telegram bot, a CLI or an HTTP daemon are *front
//! ends* over it, and a reviewer should be able to delete any of them and still
//! have a working, testable wallet. Everything a front end may touch is on
//! [`service::WalletService`]; nothing else here is part of the contract.
//!
//! Three rules shape the public surface, and §10 has tests for each:
//!
//! * no Telegram types below this line — this crate cannot name `teloxide`;
//! * no presentation below this line — it returns `Amount`, `FeeRate`, `Txid`
//!   and typed enums, never a formatted string, an emoji or an image;
//! * no secret above this line — the seed, the vault and the unlocked session
//!   live here, so no front end is in a position to leak one.

#![forbid(unsafe_code)]

pub mod config;
pub mod error;
pub mod rpc;
pub mod service;

pub use config::{AppConfig, NetworkChoice};
pub use error::{BackendError, CoreError};
pub use service::{WalletService, events, types};

/// Re-exported so a front end can name an `Amount` or a `Txid` without taking
/// its own `bitcoin` dependency — and, more to the point, without being tempted
/// to take a `bdk_wallet` one (§10's grep test).
pub use bdk_wallet::bitcoin;
