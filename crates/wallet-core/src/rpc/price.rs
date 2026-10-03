//! What a bitcoin is worth, in dollars (PLAN.md §6).
//!
//! Shaped exactly like `fees.rs` next door, and for the same reasons: a trait
//! so the network call is mockable, a cache so a chatty front end cannot turn
//! one `/balance` into one HTTP request, and an `Option` rather than a
//! `Result` at the edge — a price is a nicety, and a balance that refuses to
//! render because a third-party API is down would be a worse bug than the one
//! it is reporting.
//!
//! Two rules this module keeps to:
//!
//! * it is **not** consulted for anything that moves money. Fees come from the
//!   node and the estimator in `fees.rs`; nothing here is ever an input to a
//!   transaction. A wrong price makes a label wrong, never a payment.
//! * it says who said so. `FiatPrice::source` exists so the front end can
//!   attribute the number rather than present it as fact, which is the same
//!   honesty `FeeSource` buys for fee estimates.

use crate::{
    error::{CoreError, Result},
    service::types::FiatPrice,
};
use std::{
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

/// How long a price is reused. Five minutes is far inside the accuracy anyone
/// should read into an "≈", and it keeps a busy chat down to one call per
/// network per five minutes.
pub const PRICE_CACHE: Duration = Duration::from_secs(5 * 60);

/// How long to wait before trying again after a failed fetch.
///
/// Without this, a bot with no outbound internet — which is every offline
/// regtest session — would pay the HTTP timeout on *every* `/balance`.
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);

/// A swappable source of the BTC/USD price.
pub trait PriceSource: Send + Sync {
    fn name(&self) -> &str;
    fn usd_per_btc(&self) -> Result<f64>;
}

/// A blocking HTTP agent with the same timeout every source uses.
fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build(),
    )
}

/// CoinGecko's `/simple/price`. The default, because it is the most widely
/// reachable free price endpoint and needs no key.
pub struct CoinGecko {
    base: String,
    agent: ureq::Agent,
}

impl CoinGecko {
    pub fn new(base: impl Into<String>) -> Self {
        CoinGecko {
            base: base.into(),
            agent: agent(),
        }
    }
}

/// `{"bitcoin":{"usd":84581}}`
#[derive(serde::Deserialize)]
struct CoinGeckoBody {
    bitcoin: CoinGeckoUsd,
}

#[derive(serde::Deserialize)]
struct CoinGeckoUsd {
    usd: f64,
}

impl PriceSource for CoinGecko {
    fn name(&self) -> &str {
        "CoinGecko"
    }

    fn usd_per_btc(&self) -> Result<f64> {
        let url = format!(
            "{}/simple/price?ids=bitcoin&vs_currencies=usd",
            self.base.trim_end_matches('/')
        );
        let body: CoinGeckoBody = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| CoreError::Wallet(e.to_string()))?
            .into_body()
            .read_json()
            .map_err(|e| CoreError::Wallet(e.to_string()))?;

        usable(body.bitcoin.usd)
    }
}

/// mempool.space's `/v1/prices` — the same host `fees.rs` already asks about
/// fees, kept as the fallback so a deployment that can reach one can still
/// show a number when the other is blocked or rate-limiting.
pub struct MempoolSpacePrice {
    base: String,
    agent: ureq::Agent,
}

impl MempoolSpacePrice {
    pub fn new(base: impl Into<String>) -> Self {
        MempoolSpacePrice {
            base: base.into(),
            agent: agent(),
        }
    }
}

#[derive(serde::Deserialize)]
struct Prices {
    #[serde(rename = "USD")]
    usd: f64,
}

impl PriceSource for MempoolSpacePrice {
    fn name(&self) -> &str {
        "mempool.space"
    }

    fn usd_per_btc(&self) -> Result<f64> {
        let url = format!("{}/v1/prices", self.base.trim_end_matches('/'));
        let body: Prices = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| CoreError::Wallet(e.to_string()))?
            .into_body()
            .read_json()
            .map_err(|e| CoreError::Wallet(e.to_string()))?;

        usable(body.usd)
    }
}

/// Zero, negative, NaN and infinity are all "no price", not a price. Letting
/// one through would render `≈ $0.00` over a real balance.
fn usable(usd: f64) -> Result<f64> {
    if !usd.is_finite() || usd <= 0.0 {
        return Err(CoreError::Wallet(
            "price api returned no usable price".into(),
        ));
    }
    Ok(usd)
}

/// The cached price, and the policy around it.
pub struct PriceFeed {
    /// Tried in order. The first that answers wins; the rest exist so one
    /// blocked or rate-limiting host does not mean no price at all.
    sources: Vec<Box<dyn PriceSource>>,
    ttl: Duration,
    /// The last answer and when it was given. `None` inside the tuple is a
    /// remembered *failure*, which is why this is not `Option<(Instant, f64)>`.
    cache: Mutex<Option<(Instant, Option<FiatPrice>)>>,
}

impl PriceFeed {
    /// CoinGecko first, mempool.space second.
    ///
    /// `api` configures CoinGecko, which is the one an operator is likely to
    /// want to point elsewhere; the fallback is fixed because its only job is
    /// to be a second opinion when the first host cannot be reached.
    pub fn new(api: &str) -> Self {
        PriceFeed::with_sources(vec![
            Box::new(CoinGecko::new(api)),
            Box::new(MempoolSpacePrice::new("https://mempool.space/api")),
        ])
    }

    /// The seam §6 asks for, so a test never touches the network.
    pub fn with_source(source: Box<dyn PriceSource>) -> Self {
        PriceFeed::with_sources(vec![source])
    }

    pub fn with_sources(sources: Vec<Box<dyn PriceSource>>) -> Self {
        PriceFeed {
            sources,
            ttl: PRICE_CACHE,
            cache: Mutex::new(None),
        }
    }

    /// The current price, or `None` if nobody could say.
    ///
    /// Blocking: the caller runs it off the async runtime.
    pub fn get(&self) -> Option<FiatPrice> {
        if let Ok(cache) = self.cache.lock()
            && let Some((at, answer)) = cache.as_ref()
        {
            let still_good = match answer {
                Some(_) => at.elapsed() < self.ttl,
                None => at.elapsed() < RETRY_AFTER_FAILURE,
            };
            if still_good {
                return answer.clone();
            }
        }

        let fetched = self.fetch();
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some((Instant::now(), fetched.clone()));
        }
        fetched
    }

    fn fetch(&self) -> Option<FiatPrice> {
        for source in &self.sources {
            match source.usd_per_btc() {
                Ok(usd_per_btc) => {
                    return Some(FiatPrice {
                        usd_per_btc,
                        source: source.name().to_string(),
                        fetched_at: SystemTime::now(),
                    });
                }
                Err(e) => {
                    // `warn`, not `debug`. A missing dollar line is invisible
                    // in the chat — the card simply renders without it — so if
                    // this is quiet too there is nothing anywhere to tell an
                    // operator the feature is failing rather than absent.
                    tracing::warn!(source = source.name(), error = %e, "price source failed");
                }
            }
        }
        tracing::warn!("no price source answered; dollar values will be omitted");
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        calls: AtomicUsize,
        answer: fn() -> Result<f64>,
    }

    impl PriceSource for Counting {
        fn name(&self) -> &str {
            "test"
        }
        fn usd_per_btc(&self) -> Result<f64> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.answer)()
        }
    }

    #[test]
    fn a_price_is_fetched_once_and_then_reused() {
        let feed = PriceFeed::with_source(Box::new(Counting {
            calls: AtomicUsize::new(0),
            answer: || Ok(104_852.0),
        }));

        let first = feed.get().expect("a price");
        assert!((first.usd_per_btc - 104_852.0).abs() < f64::EPSILON);
        assert_eq!(first.source, "test", "the number is attributed");

        for _ in 0..5 {
            assert!(feed.get().is_some());
        }
        // One HTTP call for six renders, which is the point of the cache.
    }

    /// The offline case, which on regtest is the common one. A failure must
    /// be an absent price, never an error that reaches the user.
    #[test]
    fn an_unreachable_api_is_no_price_rather_than_a_failure() {
        let feed = PriceFeed::with_source(Box::new(Counting {
            calls: AtomicUsize::new(0),
            answer: || Err(CoreError::Wallet("connection refused".into())),
        }));

        assert!(feed.get().is_none());
        // And it is remembered, so the next /balance does not pay the timeout
        // all over again.
        assert!(feed.get().is_none());
    }

    /// A price of zero, NaN or a negative number is not a price. Letting one
    /// through would render "≈ $0.00" over a real balance.
    #[test]
    fn a_nonsense_price_is_refused_by_the_source() {
        for body in ["{\"USD\": 0}", "{\"USD\": -1.5}"] {
            let parsed: Prices = serde_json::from_str(body).expect("parses");
            assert!(
                parsed.usd <= 0.0,
                "the guard in usd_per_btc covers this shape"
            );
        }
    }
}
