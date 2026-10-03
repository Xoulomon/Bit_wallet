//! A Bitcoin Core node we control, over stock basic auth (PLAN.md §3, §4).
//!
//! Nothing clever is needed here — which is the point of keeping it beside
//! `bitrpc.rs`: the difference between a node we run and a hosted allowlisted
//! proxy is one constructor. Which *chain* the node serves is not this
//! module's business; `ChainSource::connect` reads that from the config.

use crate::{
    config::CoreRpcConfig,
    error::{BackendError, CoreError, Result},
};
use bitcoincore_rpc::{Auth, Client};

pub fn client(cfg: &CoreRpcConfig) -> Result<Client> {
    at(cfg, &cfg.rpc_url)
}

/// A client scoped to one of the node's own wallets.
///
/// Needed only by `getnewaddress`, and only when `/mine` is called without an
/// address. Core's wallets are BIP84 descriptors the node has never heard of,
/// so this is the one place we touch a node wallet at all — and Polar loads
/// several, which makes Core refuse a bare wallet call:
///
/// > Multiple wallets are loaded. Please select which wallet to use by
/// > requesting the RPC through the /wallet/<walletname> URI path.
pub fn wallet_client(cfg: &CoreRpcConfig, wallet: &str) -> Result<Client> {
    at(
        cfg,
        &format!("{}/wallet/{}", cfg.rpc_url.trim_end_matches('/'), wallet),
    )
}

fn at(cfg: &CoreRpcConfig, url: &str) -> Result<Client> {
    Client::new(
        url,
        Auth::UserPass(cfg.rpc_user.clone(), cfg.rpc_pass.to_string()),
    )
    .map_err(|e| CoreError::Backend(BackendError::Transport(e.to_string())))
}
