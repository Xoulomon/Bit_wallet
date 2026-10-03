//! The chain source (PLAN.md §3, §4, §4b).
//!
//! Every network is reached through one `bitcoincore_rpc::Client`, so
//! everything above this module — the wallet, the emitter, broadcast — is
//! written once. A node we run ourselves gets the stock basic-auth client
//! (`corerpc.rs`); mainnet gets a hand-written `X-API-Key` transport wrapped by
//! `Client::from_jsonrpc` (`bitrpc.rs`, Step 2), which is the only way to reach
//! BitRPC while still satisfying the `RpcApi` trait `bdk_bitcoind_rpc`
//! requires (§2).
//!
//! The split is by *transport*, not by chain: `connect` takes the chain from
//! `AppConfig::network`, so one transport can serve more than one chain.

pub mod bitrpc;
pub mod corerpc;
pub mod fees;
pub mod price;
pub mod retry;

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
    /// A Bitcoin Core node we control. Everything works except mining, and
    /// mining is a property of the *chain* rather than of the transport —
    /// `generatetoaddress` exists everywhere but is only useful where blocks
    /// need no real proof of work.
    ///
    /// A constructor rather than one const per chain: a const would leave the
    /// caller to pick the right one *from the network*, which is the same
    /// decision moved somewhere it can be got wrong.
    pub const fn core(network: Network) -> Capabilities {
        Capabilities {
            mempool: true,
            fee_estimation: true,
            test_mempool_accept: true,
            mining: matches!(network, Network::Regtest),
        }
    }

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
        // The chain comes from `cfg.network`, never from the backend variant.
        // One `Core` variant can serve more than one chain, and a chain
        // hardcoded here would make `health_check` reject a perfectly good
        // node and `/status` badge the wrong one — and the failure mode of
        // then "fixing" `health_check` is a wallet writing one chain's state
        // into another chain's directory, which §4 calls unrecoverable.
        let network = cfg.network.network();

        match &cfg.backend {
            BackendConfig::Core(c) => Ok(ChainSource {
                client: Arc::new(corerpc::client(c)?),
                network,
                budget: None,
                capabilities: Capabilities::core(network),
            }),
            BackendConfig::Bitrpc(b) => {
                let budget = Arc::new(bitrpc::CallBudget::new(
                    b.rate_limit_per_min,
                    b.sync_budget_per_min,
                ));
                Ok(ChainSource {
                    client: Arc::new(bitrpc::client(b, Arc::clone(&budget))?),
                    network,
                    budget: Some(budget),
                    capabilities: Capabilities::BITRPC,
                })
            }
        }
    }

    pub fn client(&self) -> Arc<Client> {
        Arc::clone(&self.client)
    }

    /// A second client for the block emitter, drawing from the capped sync
    /// allowance rather than the interactive one (§4). On regtest there is no
    /// budget to divide, so both lanes are the same client.
    pub fn sync_client(&self, cfg: &AppConfig) -> Result<Arc<Client>> {
        match (&cfg.backend, &self.budget) {
            (BackendConfig::Bitrpc(b), Some(budget)) => Ok(Arc::new(bitrpc::client_for(
                b,
                Arc::clone(budget),
                bitrpc::Lane::Sync,
            )?)),
            _ => Ok(Arc::clone(&self.client)),
        }
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
        // A startup probe is exactly the call worth retrying: a 429 or a
        // blinking node at boot would otherwise refuse to start the bot (§4b).
        let info = retry::with_retry(|| {
            self.client
                .get_blockchain_info()
                .map_err(map_rpc_error("getblockchaininfo"))
        })?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkChoice;
    use crate::config::CoreRpcConfig;
    use bdk_wallet::bitcoin::FeeRate;
    use std::time::Duration;
    use zeroize::Zeroizing;

    fn core_cfg(network: NetworkChoice) -> AppConfig {
        AppConfig {
            network,
            backend: BackendConfig::Core(CoreRpcConfig {
                rpc_url: "http://127.0.0.1:1".into(),
                rpc_user: "u".into(),
                rpc_pass: Zeroizing::new("p".into()),
                payjoin_directory: "http://localhost:8080".into(),
                ohttp_relay: "http://localhost:3000".into(),
                fallback_fee: FeeRate::from_sat_per_vb(2).expect("valid"),
            }),
            data_dir: "./data".into(),
            session_idle_timeout: Duration::from_secs(600),
            max_send: None,
            fee_cache: Duration::from_secs(60),
            price_api: "https://example.invalid".into(),
        }
    }

    /// **The guard for the trap.**
    ///
    /// `connect` used to hardcode the chain from the backend *variant*, which
    /// worked only while each variant served exactly one chain. One `Core`
    /// variant serving several makes that wrong in the worst available way:
    /// `health_check` compares `info.chain` against this value, so a hardcoded
    /// chain rejects a perfectly good node — and the tempting "fix" of
    /// relaxing `health_check` instead yields a wallet writing one chain's
    /// state into another chain's directory, which §4 calls unrecoverable.
    ///
    /// `connect` builds clients without talking to anything, so this needs no
    /// node despite the unroutable URL.
    #[test]
    fn the_chain_comes_from_the_config_not_the_backend_variant() {
        // Two different chains over the *same* backend variant: that is the
        // whole property, and it is why the second case uses a pairing
        // `from_source` cannot currently produce. `connect`'s contract is
        // about the config it is handed, not about which pairings the loader
        // happens to build today — and a test that only used Regtest would
        // pass against the hardcoded version it exists to forbid.
        for (choice, expected) in [
            (NetworkChoice::Regtest, Network::Regtest),
            (NetworkChoice::Mainnet, Network::Bitcoin),
        ] {
            let src = ChainSource::connect(&core_cfg(choice)).expect("builds");
            assert_eq!(
                src.network(),
                expected,
                "{choice:?} over a Core backend must report {expected}"
            );
        }

        assert!(
            ChainSource::connect(&core_cfg(NetworkChoice::Regtest))
                .expect("builds")
                .capabilities()
                .mining,
            "regtest is the one chain whose blocks we can mint"
        );
    }

    /// Mining is a property of the chain, not of the transport, so it is
    /// decided in exactly one place: `Capabilities::core`.
    #[test]
    fn only_regtest_can_mine() {
        assert!(Capabilities::core(Network::Regtest).mining);
        for network in [Network::Bitcoin, Network::Testnet, Network::Signet] {
            assert!(
                !Capabilities::core(network).mining,
                "{network} blocks need real proof of work"
            );
        }

        // Everything else a node we run can do, it can do on every chain.
        for network in [Network::Regtest, Network::Bitcoin] {
            let c = Capabilities::core(network);
            assert!(c.mempool && c.fee_estimation && c.test_mempool_accept);
        }
    }

    /// §3a rule 3: `rpc_pass` is a `Zeroizing<String>`, whose own `Debug`
    /// delegates to the inner `String` — so a derived `Debug` on the config
    /// would print the node's password. `BitrpcConfig` hand-writes its `Debug`
    /// for exactly this reason; this is its sibling.
    #[test]
    fn the_node_password_is_redacted_from_debug_output() {
        let cfg = core_cfg(NetworkChoice::Regtest);
        let rendered = format!("{cfg:?}");
        assert!(!rendered.contains("\"p\""), "password leaked: {rendered}");
        assert!(rendered.contains("<redacted>"));
    }
}
