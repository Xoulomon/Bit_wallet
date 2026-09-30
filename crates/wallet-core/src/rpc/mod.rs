//! The chain source (PLAN.md §3, §4, §4b).
//!
//! Both networks are reached through one `bitcoincore_rpc::Client`, so
//! everything above this module — the wallet, the emitter, broadcast — is
//! written once. Regtest gets the stock basic-auth client; mainnet gets a
//! hand-written `X-API-Key` transport wrapped by `Client::from_jsonrpc`
//! (`bitrpc.rs`, Step 2), which is the only way to reach BitRPC while still
//! satisfying the `RpcApi` trait `bdk_bitcoind_rpc` requires (§2).

pub mod bitrpc;
pub mod polar;

use crate::{
    config::{AppConfig, BackendConfig},
    error::{BackendError, CoreError, Result},
};
use bdk_wallet::bitcoin::Network;
use bitcoincore_rpc::{Client, RpcApi};
use std::{sync::Arc, time::Instant};

/// A chain source, plus what the rest of core needs to know about its limits.
pub struct ChainSource {
    client: Arc<Client>,
    network: Network,
    /// Present only on the BitRPC path; the shared 100 req/min budget (§4).
    budget: Option<Arc<bitrpc::CallBudget>>,
    capabilities: Capabilities,
}

/// What this backend can actually do. Mainnet's answers are the allowlist of
/// §4b, expressed as data so the layers above branch on a capability rather
/// than on a network name — and so the front end can *say* what is missing
/// instead of quietly returning an empty result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// `getrawmempool`: without it `Emitter::mempool()` cannot run, so
    /// unconfirmed *incoming* transactions are invisible (§4b, §6).
    pub mempool: bool,
    /// `estimatesmartfee`: without it fee estimation moves to an external API
    /// and every rate is floored at `mempoolminfee` (§4b, §6).
    pub fee_estimation: bool,
    /// `testmempoolaccept`: without it payjoin's broadcast-suitability check is
    /// best-effort rather than a real dry run (§4b, §7).
    pub test_mempool_accept: bool,
    /// `generatetoaddress`: regtest only, and it always was.
    pub mining: bool,
}

impl Capabilities {
    pub const POLAR: Capabilities = Capabilities {
        mempool: true,
        fee_estimation: true,
        test_mempool_accept: true,
        mining: true,
    };

    /// Exactly what §4b's allowlist leaves us.
    pub const BITRPC: Capabilities = Capabilities {
        mempool: false,
        fee_estimation: false,
        test_mempool_accept: false,
        mining: false,
    };
}

impl ChainSource {
    /// Build the client for the configured backend. Does not talk to the
    /// network; call `health_check` for that.
    pub fn connect(cfg: &AppConfig) -> Result<Self> {
        match &cfg.backend {
            BackendConfig::Regtest(r) => Ok(ChainSource {
                client: Arc::new(polar::client(r)?),
                network: Network::Regtest,
                budget: None,
                capabilities: Capabilities::POLAR,
            }),
            BackendConfig::Bitrpc(b) => {
                let budget = Arc::new(bitrpc::CallBudget::new(
                    b.rate_limit_per_min,
                    b.sync_budget_per_min,
                ));
                Ok(ChainSource {
                    client: Arc::new(bitrpc::client(b, Arc::clone(&budget))?),
                    network: Network::Bitcoin,
                    budget: Some(budget),
                    capabilities: Capabilities::BITRPC,
                })
            }
        }
    }

    pub fn client(&self) -> Arc<Client> {
        Arc::clone(&self.client)
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    pub fn budget(&self) -> Option<&Arc<bitrpc::CallBudget>> {
        self.budget.as_ref()
    }

    /// §4: the startup check. Confirms the backend is reachable and is serving
    /// the chain `NETWORK` asks for; a mismatch is fatal, because mixing regtest
    /// and mainnet state is the one mistake that cannot be undone.
    pub fn health_check(&self) -> Result<Health> {
        let started = Instant::now();
        let info = self
            .client
            .get_blockchain_info()
            .map_err(map_rpc_error("getblockchaininfo"))?;
        let latency = started.elapsed();

        if info.chain != self.network {
            return Err(CoreError::NetworkMismatch {
                configured: self.network,
                backend: chain_name(info.chain).to_string(),
            });
        }

        Ok(Health {
            tip_height: u32::try_from(info.blocks).unwrap_or(u32::MAX),
            tip_hash: info.best_block_hash,
            latency,
        })
    }
}

/// The result of a reachability check.
#[derive(Debug, Clone)]
pub struct Health {
    pub tip_height: u32,
    pub tip_hash: bdk_wallet::bitcoin::BlockHash,
    pub latency: std::time::Duration,
}

/// `getblockchaininfo.chain` as Core spells it (BIP70 names), for the mismatch
/// message. `Network`'s own `Display` says "bitcoin", Core says "main".
pub fn chain_name(n: Network) -> &'static str {
    match n {
        Network::Bitcoin => "main",
        Network::Testnet => "test",
        Network::Signet => "signet",
        Network::Regtest => "regtest",
        _ => "unknown",
    }
}

/// Turn a `bitcoincore_rpc` failure into a typed `BackendError`.
///
/// The BitRPC transport has already classified its own HTTP statuses (§4b) and
/// smuggles the verdict through `jsonrpc::Error::Transport`; anything else is a
/// genuine JSON-RPC error object or a transport fault. The method name is
/// carried so a 403 can say *which* call the allowlist refused.
pub fn map_rpc_error(method: &'static str) -> impl Fn(bitcoincore_rpc::Error) -> CoreError {
    move |e| {
        use bitcoincore_rpc::Error as E;
        use bitcoincore_rpc::jsonrpc::Error as JE;

        let backend = match e {
            E::JsonRpc(JE::Rpc(rpc)) => BackendError::Rpc {
                code: rpc.code,
                message: rpc.message,
            },
            E::JsonRpc(JE::Transport(t)) => {
                // `bitrpc::TransportError` round-trips through this box.
                match t.downcast::<bitrpc::TransportError>() {
                    Ok(classified) => classified.into_backend(method),
                    // Never interpolate the request: it carries the API key.
                    Err(other) => BackendError::Transport(other.to_string()),
                }
            }
            other => BackendError::Transport(other.to_string()),
        };
        CoreError::Backend(backend)
    }
}
