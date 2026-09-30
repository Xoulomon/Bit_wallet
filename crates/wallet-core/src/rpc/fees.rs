//! Fee policy (PLAN.md §6, §4b).
//!
//! The MVP requires the wallet to *estimate fees*. BitRPC does not expose
//! `estimatesmartfee`, so on mainnet estimation moves to an external source
//! rather than disappearing — and whatever it yields is flattened into one
//! `FeeOptions`, so the front end's fee keyboard is the same code on both
//! networks: it draws the presets it is handed and nothing more (§3a).
//!
//! Two rules hold everywhere:
//!
//! * every rate, estimated or typed, is floored at `mempoolminfee` (or the
//!   configured hard floor when that call fails) and rejected below it;
//! * if the estimator is unreachable the policy says so and degrades to the
//!   manual prompt. It never silently guesses.

use crate::{
    config::{AppConfig, BackendConfig},
    error::{CoreError, Result},
    service::types::{FeeLabel, FeeOptions, FeeSource},
};
use bdk_wallet::bitcoin::FeeRate;
use bitcoincore_rpc::RpcApi;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// What an estimator returns, before a floor is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estimates {
    pub fast: FeeRate,
    pub normal: FeeRate,
    pub slow: FeeRate,
}

/// A swappable source of fee estimates.
///
/// A trait rather than a function so the mainnet API is mockable in tests and
/// replaceable in production without touching the policy around it (§6).
pub trait FeeEstimator: Send + Sync {
    fn name(&self) -> &str;
    fn estimate(&self) -> Result<Estimates>;
}

/// mempool.space's `/v1/fees/recommended` (§6).
pub struct MempoolSpace {
    base: String,
    agent: ureq::Agent,
}

impl MempoolSpace {
    pub fn new(base: impl Into<String>) -> Self {
        MempoolSpace {
            base: base.into(),
            agent: ureq::Agent::new_with_config(
                ureq::Agent::config_builder()
                    .timeout_global(Some(Duration::from_secs(10)))
                    .build(),
            ),
        }
    }
}

#[derive(serde::Deserialize)]
struct Recommended {
    #[serde(rename = "fastestFee")]
    fastest_fee: u64,
    #[serde(rename = "halfHourFee")]
    half_hour_fee: u64,
    #[serde(rename = "hourFee")]
    hour_fee: u64,
}

impl FeeEstimator for MempoolSpace {
    fn name(&self) -> &str {
        "mempool.space"
    }

    fn estimate(&self) -> Result<Estimates> {
        let url = format!("{}/v1/fees/recommended", self.base.trim_end_matches('/'));
        let body: Recommended = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| CoreError::Wallet(e.to_string()))?
            .into_body()
            .read_json()
            .map_err(|e| CoreError::Wallet(e.to_string()))?;

        Ok(Estimates {
            fast: rate(body.fastest_fee)?,
            normal: rate(body.half_hour_fee)?,
            slow: rate(body.hour_fee)?,
        })
    }
}

fn rate(sat_vb: u64) -> Result<FeeRate> {
    FeeRate::from_sat_per_vb(sat_vb.max(1)).ok_or(CoreError::Crypto("fee rate out of range"))
}

/// The policy: estimates plus a floor, cached, per network.
pub struct FeePolicy {
    cfg: AppConfig,
    estimator: Option<Box<dyn FeeEstimator>>,
    cache: Mutex<Option<(Instant, FeeOptions)>>,
}

impl FeePolicy {
    pub fn new(cfg: &AppConfig) -> Self {
        let estimator: Option<Box<dyn FeeEstimator>> = match &cfg.backend {
            BackendConfig::Bitrpc(b) => Some(Box::new(MempoolSpace::new(b.fee_api.clone()))),
            // Regtest asks its own node; there is nothing external to call.
            BackendConfig::Regtest(_) => None,
        };
        FeePolicy {
            cfg: cfg.clone(),
            estimator,
            cache: Mutex::new(None),
        }
    }

    /// Use a different estimator — the seam §6 asks for, so the API is
    /// mockable in tests.
    pub fn with_estimator(mut self, estimator: Box<dyn FeeEstimator>) -> Self {
        self.estimator = Some(estimator);
        self
    }

    /// The presets, floor and source for this network (§3a `FeeOptions`).
    pub fn options(&self) -> Result<FeeOptions> {
        if let Ok(cache) = self.cache.lock()
            && let Some((at, cached)) = cache.as_ref()
            && at.elapsed() < self.cfg.fee_cache
        {
            return Ok(cached.clone());
        }

        let floor = self.floor();
        let options = match &self.cfg.backend {
            BackendConfig::Regtest(r) => self.regtest_options(r, floor),
            BackendConfig::Bitrpc(_) => self.mainnet_options(floor),
        };

        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some((Instant::now(), options.clone()));
        }
        Ok(options)
    }

    /// Regtest: `estimatesmartfee` at our own node, falling back to the
    /// configured rate — which is every fresh regtest chain, because a chain
    /// with no fee history has nothing to estimate from (§6).
    fn regtest_options(&self, r: &crate::config::RegtestConfig, floor: FeeRate) -> FeeOptions {
        let estimated = self.estimatesmartfee();

        let (presets, source) = match estimated {
            Some(e) => (
                vec![
                    (FeeLabel::Fast, e.fast),
                    (FeeLabel::Normal, e.normal),
                    (FeeLabel::Slow, e.slow),
                ],
                FeeSource::Node,
            ),
            None => (
                vec![(FeeLabel::Normal, r.fallback_fee)],
                FeeSource::Unavailable,
            ),
        };

        FeeOptions {
            presets: floored(presets, floor),
            floor,
            source,
            allows_custom: true,
        }
    }

    /// Mainnet: the external estimator, or an honest "unavailable" (§6).
    fn mainnet_options(&self, floor: FeeRate) -> FeeOptions {
        let estimated = self
            .estimator
            .as_ref()
            .and_then(|e| e.estimate().ok().map(|est| (e.name().to_string(), est)));

        match estimated {
            Some((name, e)) => FeeOptions {
                presets: floored(
                    vec![
                        (FeeLabel::Fast, e.fast),
                        (FeeLabel::Normal, e.normal),
                        (FeeLabel::Slow, e.slow),
                    ],
                    floor,
                ),
                floor,
                source: FeeSource::External { name },
                allows_custom: true,
            },
            // No presets and `Unavailable`: the front end prompts for a rate
            // and says why, rather than offering a number nobody stands behind.
            None => FeeOptions {
                presets: Vec::new(),
                floor,
                source: FeeSource::Unavailable,
                allows_custom: true,
            },
        }
    }

    /// `getmempoolinfo.mempoolminfee`, or the configured hard floor (§6).
    pub fn floor(&self) -> FeeRate {
        let configured = match &self.cfg.backend {
            BackendConfig::Bitrpc(b) => b.min_fee,
            BackendConfig::Regtest(_) => {
                FeeRate::from_sat_per_vb(1).unwrap_or(FeeRate::BROADCAST_MIN)
            }
        };

        match self.mempool_min_fee() {
            Some(node) if node > configured => node,
            Some(node) => node.max(configured),
            None => configured,
        }
    }

    fn mempool_min_fee(&self) -> Option<FeeRate> {
        let source = crate::rpc::ChainSource::connect(&self.cfg).ok()?;
        let info = source.client().get_mempool_info().ok()?;
        // Core reports BTC/kvB; sat/vB is the same number times 100 000 / 1 000.
        let sat_per_kvb = info.mempool_min_fee.to_sat();
        FeeRate::from_sat_per_vb(sat_per_kvb.div_ceil(1_000).max(1))
    }

    fn estimatesmartfee(&self) -> Option<Estimates> {
        let source = crate::rpc::ChainSource::connect(&self.cfg).ok()?;
        if !source.capabilities().fee_estimation {
            return None;
        }
        let client = source.client();

        // Targets 1/6/144 as Fast/Normal/Slow (§6).
        let at = |blocks: u16| -> Option<FeeRate> {
            let result = client.estimate_smart_fee(blocks, None).ok()?;
            let per_kvb = result.fee_rate?;
            FeeRate::from_sat_per_vb(per_kvb.to_sat().div_ceil(1_000).max(1))
        };

        Some(Estimates {
            fast: at(1)?,
            normal: at(6)?,
            slow: at(144)?,
        })
    }
}

/// Nothing below the floor is ever offered — a preset under `mempoolminfee`
/// would be a button that builds a transaction the network will not relay.
fn floored(presets: Vec<(FeeLabel, FeeRate)>, floor: FeeRate) -> Vec<(FeeLabel, FeeRate)> {
    presets
        .into_iter()
        .map(|(label, rate)| (label, rate.max(floor)))
        .collect()
}

/// Check a rate — estimated or typed — against the floor (§6).
pub fn check_rate(given: FeeRate, floor: FeeRate) -> Result<FeeRate> {
    if given < floor {
        return Err(CoreError::FeeBelowFloor { given, floor });
    }
    Ok(given)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BitrpcConfig, RegtestConfig};
    use zeroize::Zeroizing;

    struct Fixed(Estimates);
    impl FeeEstimator for Fixed {
        fn name(&self) -> &str {
            "test"
        }
        fn estimate(&self) -> Result<Estimates> {
            Ok(self.0.clone())
        }
    }

    struct Broken;
    impl FeeEstimator for Broken {
        fn name(&self) -> &str {
            "broken"
        }
        fn estimate(&self) -> Result<Estimates> {
            Err(CoreError::Wallet("the fee API is down".into()))
        }
    }

    fn vb(n: u64) -> FeeRate {
        FeeRate::from_sat_per_vb(n).expect("a valid rate")
    }

    fn mainnet_cfg() -> AppConfig {
        AppConfig {
            network: crate::NetworkChoice::Mainnet,
            backend: BackendConfig::Bitrpc(BitrpcConfig {
                url: "https://example.invalid".into(),
                api_key: Zeroizing::new("k".into()),
                rate_limit_per_min: 90,
                sync_budget_per_min: 60,
                max_rescan_blocks: 10_000,
                min_fee: vb(1),
                fee_api: "https://example.invalid".into(),
                payjoin_directory: "https://example.invalid".into(),
                ohttp_relay: "https://example.invalid".into(),
            }),
            data_dir: "./data".into(),
            session_idle_timeout: Duration::from_secs(600),
            max_send: None,
            fee_cache: Duration::from_secs(60),
        }
    }

    fn regtest_cfg(fallback: u64) -> AppConfig {
        AppConfig {
            network: crate::NetworkChoice::Regtest,
            backend: BackendConfig::Regtest(RegtestConfig {
                // Unreachable on purpose: estimatesmartfee must fail and the
                // fallback must take over, which is the regtest case in §6.
                rpc_url: "http://127.0.0.1:1".into(),
                rpc_user: "u".into(),
                rpc_pass: Zeroizing::new("p".into()),
                payjoin_directory: "http://localhost:8080".into(),
                ohttp_relay: "http://localhost:3000".into(),
                fallback_fee: vb(fallback),
            }),
            data_dir: "./data".into(),
            session_idle_timeout: Duration::from_secs(600),
            max_send: None,
            fee_cache: Duration::from_secs(60),
        }
    }

    /// §6: a mempool.space payload becomes Fast/Normal/Slow, and the front end
    /// is told where the numbers came from.
    #[test]
    fn a_recorded_mainnet_payload_maps_onto_fast_normal_slow() {
        let policy = FeePolicy::new(&mainnet_cfg()).with_estimator(Box::new(Fixed(Estimates {
            fast: vb(12),
            normal: vb(6),
            slow: vb(2),
        })));

        let options = policy.options().expect("options build");
        assert_eq!(
            options.presets,
            vec![
                (FeeLabel::Fast, vb(12)),
                (FeeLabel::Normal, vb(6)),
                (FeeLabel::Slow, vb(2)),
            ]
        );
        assert_eq!(
            options.source,
            FeeSource::External {
                name: "test".into()
            }
        );
        assert!(options.allows_custom, "mainnet always allows a typed rate");
    }

    /// §6: it never silently guesses.
    #[test]
    fn an_unreachable_fee_api_degrades_to_the_manual_prompt() {
        let policy = FeePolicy::new(&mainnet_cfg()).with_estimator(Box::new(Broken));
        let options = policy.options().expect("options build");

        assert!(options.presets.is_empty(), "no invented presets");
        assert_eq!(options.source, FeeSource::Unavailable);
        assert!(options.allows_custom);
    }

    /// §6: regtest falls back to REGTEST_FALLBACK_FEE_SAT_VB when
    /// estimatesmartfee errors, which is every fresh chain.
    #[test]
    fn regtest_falls_back_to_the_configured_rate() {
        let options = FeePolicy::new(&regtest_cfg(7)).options().expect("builds");
        assert_eq!(options.source, FeeSource::Unavailable);
        assert_eq!(options.presets, vec![(FeeLabel::Normal, vb(7))]);
    }

    #[test]
    fn no_preset_is_ever_offered_below_the_floor() {
        // A preset under mempoolminfee would be a button that builds a
        // transaction the network will not relay.
        let raised = floored(
            vec![(FeeLabel::Slow, vb(1)), (FeeLabel::Fast, vb(20))],
            vb(5),
        );
        assert_eq!(
            raised[0].1,
            vb(5),
            "the slow preset was lifted to the floor"
        );
        assert_eq!(raised[1].1, vb(20), "a rate already above it is untouched");
    }

    #[test]
    fn a_rate_below_the_floor_is_rejected_with_both_numbers() {
        match check_rate(vb(1), vb(3)) {
            Err(CoreError::FeeBelowFloor { given, floor }) => {
                assert_eq!(given, vb(1));
                assert_eq!(floor, vb(3));
            }
            other => panic!("expected FeeBelowFloor, got {other:?}"),
        }
        assert_eq!(
            check_rate(vb(3), vb(3)).expect("the floor itself passes"),
            vb(3)
        );
        assert_eq!(
            check_rate(vb(9), vb(3)).expect("above the floor passes"),
            vb(9)
        );
    }

    #[test]
    fn estimates_are_cached_so_a_burst_of_sends_makes_one_call() {
        struct Counting(std::sync::atomic::AtomicU32);
        impl FeeEstimator for Counting {
            fn name(&self) -> &str {
                "counting"
            }
            fn estimate(&self) -> Result<Estimates> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(Estimates {
                    fast: vb(10),
                    normal: vb(5),
                    slow: vb(2),
                })
            }
        }

        let counter = std::sync::Arc::new(Counting(std::sync::atomic::AtomicU32::new(0)));
        struct Shared(std::sync::Arc<Counting>);
        impl FeeEstimator for Shared {
            fn name(&self) -> &str {
                self.0.name()
            }
            fn estimate(&self) -> Result<Estimates> {
                self.0.estimate()
            }
        }

        let policy = FeePolicy::new(&mainnet_cfg())
            .with_estimator(Box::new(Shared(std::sync::Arc::clone(&counter))));

        policy.options().expect("first");
        policy.options().expect("second");
        policy.options().expect("third");

        assert_eq!(
            counter.0.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "FEE_CACHE_SECS exists so three sends in a minute cost one call"
        );
    }
}
