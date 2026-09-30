//! Configuration and network switching (PLAN.md §4).
//!
//! `NETWORK` selects one of two backend blocks and nothing else in the process
//! ever branches on an env var again. Loading fails fast: a missing setting is a
//! startup error, not a `None` that surfaces three layers later.

use crate::error::{CoreError, Result};
use bdk_wallet::bitcoin::{Amount, FeeRate, Network};
use std::{path::PathBuf, str::FromStr, time::Duration};
use zeroize::Zeroizing;

/// Which chain this instance is bound to. All state is namespaced by this (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkChoice {
    Regtest,
    Mainnet,
}

impl NetworkChoice {
    pub fn network(self) -> Network {
        match self {
            NetworkChoice::Regtest => Network::Regtest,
            NetworkChoice::Mainnet => Network::Bitcoin,
        }
    }

    /// The directory segment that keeps regtest and mainnet state apart (§4).
    pub fn namespace(self) -> &'static str {
        match self {
            NetworkChoice::Regtest => "regtest",
            NetworkChoice::Mainnet => "bitcoin",
        }
    }

    /// What `getblockchaininfo.chain` must report for this choice.
    pub fn expected_chain(self) -> &'static str {
        match self {
            NetworkChoice::Regtest => "regtest",
            NetworkChoice::Mainnet => "main",
        }
    }
}

impl FromStr for NetworkChoice {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "regtest" => Ok(NetworkChoice::Regtest),
            "bitcoin" | "mainnet" | "main" => Ok(NetworkChoice::Mainnet),
            other => Err(CoreError::InvalidConfig {
                key: "NETWORK",
                reason: format!("expected `regtest` or `bitcoin`, got `{other}`"),
            }),
        }
    }
}

/// Polar's bitcoind over stock basic auth (§4).
#[derive(Debug, Clone)]
pub struct RegtestConfig {
    pub rpc_url: String,
    pub rpc_user: String,
    pub rpc_pass: Zeroizing<String>,
    pub payjoin_directory: String,
    pub ohttp_relay: String,
    /// Used when `estimatesmartfee` errors on a fresh regtest chain (§6).
    pub fallback_fee: FeeRate,
}

/// BitRPC's hosted Core over `X-API-Key` (§4b).
///
/// `Debug` is hand-written: the API key must never reach a log line (§3a rule 3).
#[derive(Clone)]
pub struct BitrpcConfig {
    pub url: String,
    pub api_key: Zeroizing<String>,
    /// Headroom under the hard 100 req/min, shared by every user.
    pub rate_limit_per_min: u32,
    /// The slice of that budget the block emitter may spend, so a sync backlog
    /// can never starve an interactive `/send` (§4).
    pub sync_budget_per_min: u32,
    pub max_rescan_blocks: u32,
    /// Hard floor when `getmempoolinfo` is unavailable (§6).
    pub min_fee: FeeRate,
    /// External estimator, because BitRPC blocks `estimatesmartfee` (§4b).
    pub fee_api: String,
    pub payjoin_directory: String,
    pub ohttp_relay: String,
}

impl std::fmt::Debug for BitrpcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitrpcConfig")
            .field("url", &self.url)
            .field("api_key", &"<redacted>")
            .field("rate_limit_per_min", &self.rate_limit_per_min)
            .field("sync_budget_per_min", &self.sync_budget_per_min)
            .field("max_rescan_blocks", &self.max_rescan_blocks)
            .field("min_fee", &self.min_fee)
            .field("fee_api", &self.fee_api)
            .field("payjoin_directory", &self.payjoin_directory)
            .field("ohttp_relay", &self.ohttp_relay)
            .finish()
    }
}

/// The active backend: exactly one, chosen by `NETWORK`.
#[derive(Debug, Clone)]
pub enum BackendConfig {
    Regtest(RegtestConfig),
    Bitrpc(BitrpcConfig),
}

/// Everything `wallet-core` needs to run. Note what is absent: no bot token, no
/// admin ids, no allowlist — those are the front end's configuration (§3a).
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub network: NetworkChoice,
    pub backend: BackendConfig,
    /// Root of the per-network state tree: `{data_dir}/{namespace}/…` (§4).
    pub data_dir: PathBuf,
    pub session_idle_timeout: Duration,
    /// Optional safety cap on a single `/send` (§4).
    pub max_send: Option<Amount>,
    pub fee_cache: Duration,
}

impl AppConfig {
    /// Load `.env` (if present) and the process environment.
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        Self::from_source(&EnvSource)
    }

    /// The testable core of `from_env`: everything reads through `Source`, so a
    /// test can supply a map instead of mutating the process environment.
    pub fn from_source(src: &dyn Source) -> Result<Self> {
        let network: NetworkChoice = req(src, "NETWORK")?.parse()?;

        let backend = match network {
            NetworkChoice::Regtest => BackendConfig::Regtest(RegtestConfig {
                rpc_url: opt(src, "REGTEST_RPC_URL")
                    .unwrap_or_else(|| "http://127.0.0.1:18443".into()),
                rpc_user: req(src, "REGTEST_RPC_USER")?,
                rpc_pass: Zeroizing::new(req(src, "REGTEST_RPC_PASS")?),
                payjoin_directory: opt(src, "REGTEST_PAYJOIN_DIRECTORY")
                    .unwrap_or_else(|| "http://localhost:8080".into()),
                ohttp_relay: opt(src, "REGTEST_OHTTP_RELAY")
                    .unwrap_or_else(|| "http://localhost:3000".into()),
                fallback_fee: fee_rate(src, "REGTEST_FALLBACK_FEE_SAT_VB", 2)?,
            }),
            NetworkChoice::Mainnet => {
                // §4: refuse to start on mainnet without the explicit acknowledgement.
                if !flag(src, "MAINNET_I_UNDERSTAND_RISK")? {
                    return Err(CoreError::MainnetNotAcknowledged);
                }
                let api_key = req(src, "BITRPC_API_KEY")?;
                // §4: an empty key on mainnet is the single most common misconfiguration.
                if api_key.trim().is_empty() {
                    return Err(CoreError::MissingConfig("BITRPC_API_KEY"));
                }
                BackendConfig::Bitrpc(BitrpcConfig {
                    url: opt(src, "BITRPC_URL")
                        .unwrap_or_else(|| "https://bitrpc.thebuidl.xyz".into()),
                    api_key: Zeroizing::new(api_key),
                    rate_limit_per_min: num(src, "BITRPC_RATE_LIMIT_PER_MIN", 90)?,
                    sync_budget_per_min: num(src, "BITRPC_SYNC_BUDGET_PER_MIN", 60)?,
                    max_rescan_blocks: num(src, "MAINNET_MAX_RESCAN_BLOCKS", 10_000)?,
                    min_fee: fee_rate(src, "MAINNET_MIN_FEE_SAT_VB", 1)?,
                    fee_api: opt(src, "MAINNET_FEE_API")
                        .unwrap_or_else(|| "https://mempool.space/api".into()),
                    payjoin_directory: opt(src, "MAINNET_PAYJOIN_DIRECTORY")
                        .unwrap_or_else(|| "https://payjo.in".into()),
                    ohttp_relay: opt(src, "MAINNET_OHTTP_RELAY")
                        .unwrap_or_else(|| "https://pj.bobspacebind.com".into()),
                })
            }
        };

        Ok(AppConfig {
            network,
            backend,
            data_dir: PathBuf::from(opt(src, "DATA_DIR").unwrap_or_else(|| "./data".into())),
            session_idle_timeout: Duration::from_secs(num::<u64>(
                src,
                "SESSION_IDLE_TIMEOUT_SECS",
                600,
            )?),
            max_send: match opt(src, "MAX_SEND_SATS") {
                Some(v) if !v.trim().is_empty() => {
                    Some(Amount::from_sat(v.trim().parse().map_err(|_| {
                        CoreError::InvalidConfig {
                            key: "MAX_SEND_SATS",
                            reason: "expected a whole number of satoshis".into(),
                        }
                    })?))
                }
                _ => None,
            },
            fee_cache: Duration::from_secs(num::<u64>(src, "FEE_CACHE_SECS", 60)?),
        })
    }

    /// `{data_dir}/{regtest|bitcoin}` — the root of this network's state (§4).
    pub fn network_dir(&self) -> PathBuf {
        self.data_dir.join(self.network.namespace())
    }

    /// `{network_dir}/app.sqlite` — users, vaults, quotes, payjoin sessions.
    pub fn app_db(&self) -> PathBuf {
        self.network_dir().join("app.sqlite")
    }

    /// `{network_dir}/wallets/{user}.sqlite` — one BDK wallet per user (§4).
    pub fn wallet_db(&self, user: &crate::service::types::UserId) -> PathBuf {
        self.network_dir()
            .join("wallets")
            .join(format!("{user}.sqlite"))
    }

    pub fn is_mainnet(&self) -> bool {
        self.network == NetworkChoice::Mainnet
    }
}

/// Where settings come from. Injectable so §10's config tests never have to
/// mutate the process environment (which is global, and racy under a test
/// harness that runs threads).
pub trait Source {
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
pub struct EnvSource;

impl Source for EnvSource {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl Source for std::collections::HashMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        // Fully qualified: the trait method and the inherent method share a name.
        std::collections::HashMap::get(self, key).cloned()
    }
}

fn opt(src: &dyn Source, key: &str) -> Option<String> {
    src.get(key).filter(|v| !v.trim().is_empty())
}

fn req(src: &dyn Source, key: &'static str) -> Result<String> {
    opt(src, key).ok_or(CoreError::MissingConfig(key))
}

fn num<T: FromStr>(src: &dyn Source, key: &'static str, default: T) -> Result<T> {
    match opt(src, key) {
        None => Ok(default),
        Some(v) => v.trim().parse().map_err(|_| CoreError::InvalidConfig {
            key,
            reason: "expected a number".into(),
        }),
    }
}

fn flag(src: &dyn Source, key: &'static str) -> Result<bool> {
    Ok(matches!(
        opt(src, key).as_deref().map(str::trim),
        Some("true" | "1" | "yes")
    ))
}

fn fee_rate(src: &dyn Source, key: &'static str, default_sat_vb: u64) -> Result<FeeRate> {
    let sat_vb: u64 = num(src, key, default_sat_vb)?;
    FeeRate::from_sat_per_vb(sat_vb).ok_or(CoreError::InvalidConfig {
        key,
        reason: "fee rate out of range".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base(net: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("NETWORK".into(), net.into());
        m.insert("REGTEST_RPC_USER".into(), "polaruser".into());
        m.insert("REGTEST_RPC_PASS".into(), "polarpass".into());
        m
    }

    #[test]
    fn regtest_selects_the_polar_backend() {
        let cfg = AppConfig::from_source(&base("regtest")).expect("regtest config loads");
        assert_eq!(cfg.network, NetworkChoice::Regtest);
        assert!(matches!(cfg.backend, BackendConfig::Regtest(_)));
        assert_eq!(cfg.network_dir(), PathBuf::from("./data/regtest"));
    }

    #[test]
    fn mainnet_selects_the_bitrpc_backend() {
        let mut m = base("bitcoin");
        m.insert("MAINNET_I_UNDERSTAND_RISK".into(), "true".into());
        m.insert("BITRPC_API_KEY".into(), "not-a-real-key".into());
        let cfg = AppConfig::from_source(&m).expect("mainnet config loads");
        assert!(matches!(cfg.backend, BackendConfig::Bitrpc(_)));
        assert_eq!(cfg.network_dir(), PathBuf::from("./data/bitcoin"));
    }

    #[test]
    fn mainnet_with_an_empty_api_key_is_refused() {
        let mut m = base("bitcoin");
        m.insert("MAINNET_I_UNDERSTAND_RISK".into(), "true".into());
        m.insert("BITRPC_API_KEY".into(), "   ".into());
        assert!(matches!(
            AppConfig::from_source(&m),
            Err(CoreError::MissingConfig("BITRPC_API_KEY"))
        ));
    }

    #[test]
    fn mainnet_without_the_acknowledgement_is_refused() {
        let mut m = base("bitcoin");
        m.insert("BITRPC_API_KEY".into(), "not-a-real-key".into());
        assert!(matches!(
            AppConfig::from_source(&m),
            Err(CoreError::MainnetNotAcknowledged)
        ));
    }

    #[test]
    fn an_unknown_network_is_refused() {
        assert!(matches!(
            AppConfig::from_source(&base("signet")),
            Err(CoreError::InvalidConfig { key: "NETWORK", .. })
        ));
    }

    #[test]
    fn a_missing_regtest_credential_is_refused() {
        let mut m = base("regtest");
        m.remove("REGTEST_RPC_PASS");
        assert!(matches!(
            AppConfig::from_source(&m),
            Err(CoreError::MissingConfig("REGTEST_RPC_PASS"))
        ));
    }

    #[test]
    fn the_api_key_is_redacted_from_debug_output() {
        let mut m = base("bitcoin");
        m.insert("MAINNET_I_UNDERSTAND_RISK".into(), "true".into());
        m.insert("BITRPC_API_KEY".into(), "super-secret-key".into());
        let cfg = AppConfig::from_source(&m).expect("config loads");
        let rendered = format!("{cfg:?}");
        assert!(
            !rendered.contains("super-secret-key"),
            "key leaked into Debug"
        );
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn regtest_and_mainnet_state_never_share_a_directory() {
        let regtest = AppConfig::from_source(&base("regtest")).expect("regtest loads");
        let mut m = base("bitcoin");
        m.insert("MAINNET_I_UNDERSTAND_RISK".into(), "true".into());
        m.insert("BITRPC_API_KEY".into(), "k".into());
        let mainnet = AppConfig::from_source(&m).expect("mainnet loads");
        assert_ne!(regtest.app_db(), mainnet.app_db());
    }
}
