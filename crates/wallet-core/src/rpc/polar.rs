//! Regtest: Polar's bitcoind over stock basic auth (PLAN.md §3, §4).
//!
//! Nothing clever is needed here — which is the point of keeping it beside
//! `bitrpc.rs`: the difference between the two networks is one constructor.

use crate::{
    config::RegtestConfig,
    error::{BackendError, CoreError, Result},
};
use bitcoincore_rpc::{Auth, Client};

pub fn client(cfg: &RegtestConfig) -> Result<Client> {
    Client::new(
        &cfg.rpc_url,
        Auth::UserPass(cfg.rpc_user.clone(), cfg.rpc_pass.to_string()),
    )
    .map_err(|e| CoreError::Backend(BackendError::Transport(e.to_string())))
}
