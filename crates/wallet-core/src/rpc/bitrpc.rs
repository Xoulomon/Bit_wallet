//! Mainnet: BitRPC's hosted Core behind `X-API-Key` (PLAN.md §4b).
//!
//! Neither `jsonrpc::simple_http` nor `minreq_http` can set an arbitrary header
//! — both only ever emit `Authorization` basic auth — and BitRPC authenticates
//! with `X-API-Key`. So the transport is hand-written here and wrapped by
//! `Client::from_jsonrpc`, which yields a genuine `RpcApi` that
//! `bdk_bitcoind_rpc::Emitter` consumes unchanged (§2).
//!
//! The HTTP client is `ureq`, not `reqwest::blocking`: the emitter runs inside
//! `spawn_blocking`, and a blocking `reqwest` there would nest one Tokio
//! runtime inside another.

use crate::{
    config::BitrpcConfig,
    error::{BackendError, CoreError, Result},
};
use bitcoincore_rpc::{Client, jsonrpc};
use governor::{
    Quota, RateLimiter,
    clock::DefaultClock,
    state::{InMemoryState, NotKeyed},
};
use std::{
    fmt,
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// How long to wait before giving up on one call. BitRPC is an HTTP hop in
/// front of a shared node, so a stuck request must not hold the emitter's
/// blocking thread forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Which allowance a call draws from. Interactive calls get the full budget;
/// the block emitter is capped, so a sync backlog can never starve a `/send`
/// (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Interactive,
    Sync,
}

/// The shared 100 req/min budget (§4).
///
/// One instance per process. Both lanes draw from `shared`, because the limit
/// BitRPC enforces is per *key*, not per caller; the sync lane additionally
/// draws from its own smaller allowance, which is what keeps it from consuming
/// the whole key.
pub struct CallBudget {
    shared: Limiter,
    sync: Limiter,
    limit_per_min: u32,
    /// Calls made in the current minute window, for `/status` (§8.2).
    used: AtomicU32,
    window_start_secs: AtomicU64,
}

impl CallBudget {
    pub fn new(limit_per_min: u32, sync_per_min: u32) -> Self {
        let quota = |n: u32| {
            let n = NonZeroU32::new(n.max(1)).unwrap_or(NonZeroU32::MIN);
            Quota::per_minute(n)
        };
        CallBudget {
            shared: RateLimiter::direct(quota(limit_per_min)),
            sync: RateLimiter::direct(quota(sync_per_min)),
            limit_per_min,
            used: AtomicU32::new(0),
            window_start_secs: AtomicU64::new(now_secs()),
        }
    }

    /// Block until a call in this lane may proceed. Used on the emitter's
    /// blocking thread, where there is no runtime to await on.
    pub fn acquire_blocking(&self, lane: Lane) {
        if lane == Lane::Sync {
            Self::wait_blocking(&self.sync);
        }
        Self::wait_blocking(&self.shared);
        self.record();
    }

    /// Await a slot from async code.
    pub async fn acquire(&self, lane: Lane) {
        if lane == Lane::Sync {
            self.sync.until_ready().await;
        }
        self.shared.until_ready().await;
        self.record();
    }

    fn wait_blocking(limiter: &Limiter) {
        while let Err(not_until) = limiter.check() {
            let wait = not_until.wait_time_from(DefaultClock::default().now());
            // A floor keeps a zero-length wait from spinning the CPU.
            std::thread::sleep(wait.max(Duration::from_millis(5)));
        }
    }

    /// Calls spent in the current minute, for `/status`.
    pub fn used(&self) -> u32 {
        self.roll_window();
        self.used.load(Ordering::Relaxed)
    }

    pub fn limit(&self) -> u32 {
        self.limit_per_min
    }

    fn record(&self) {
        self.roll_window();
        self.used.fetch_add(1, Ordering::Relaxed);
    }

    /// The counter is a display of the last minute, not the limiter itself —
    /// `governor` holds the real state. Resetting on a whole-minute boundary
    /// keeps `/status` honest without a second timer task.
    fn roll_window(&self) {
        let now = now_secs();
        let start = self.window_start_secs.load(Ordering::Relaxed);
        if now.saturating_sub(start) >= 60
            && self
                .window_start_secs
                .compare_exchange(start, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.used.store(0, Ordering::Relaxed);
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

use governor::clock::Clock as _;

/// An HTTP status BitRPC returned, classified here rather than three layers up
/// (§4b). It travels through `jsonrpc::Error::Transport` as a boxed error and
/// is downcast by `rpc::map_rpc_error`, which keeps the mapping in one place.
/// It never contains the API key.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("no API key")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("rate limited")]
    RateLimited { retry_after: Option<Duration> },
    #[error("node unavailable")]
    NodeUnavailable,
    #[error("{0}")]
    Http(String),
}

impl TransportError {
    /// Pair the status with the method that earned it, so a 403 can say which
    /// call the allowlist refused.
    pub fn into_backend(self, method: &str) -> BackendError {
        match self {
            TransportError::Unauthorized => BackendError::MissingApiKey,
            TransportError::Forbidden => BackendError::Forbidden {
                method: method.to_string(),
            },
            TransportError::RateLimited { retry_after } => {
                BackendError::RateLimited { retry_after }
            }
            TransportError::NodeUnavailable => BackendError::NodeUnavailable,
            TransportError::Http(msg) => BackendError::Transport(msg),
        }
    }

    /// Map one HTTP status to a verdict (§4b): 401 missing key, 403 invalid key
    /// *or* a method the allowlist refuses, 429 over quota, 502 node down.
    pub fn from_status(status: u16, retry_after: Option<Duration>) -> Option<Self> {
        match status {
            200..=299 => None,
            401 => Some(TransportError::Unauthorized),
            403 => Some(TransportError::Forbidden),
            429 => Some(TransportError::RateLimited { retry_after }),
            502..=504 => Some(TransportError::NodeUnavailable),
            other => Some(TransportError::Http(format!("HTTP {other}"))),
        }
    }
}

/// The `jsonrpc::Transport` of §2.
///
/// `Debug` and `fmt_target` print the URL only. The API key is in a header that
/// nothing here ever formats, which is the point: a transport that could print
/// itself in full would leak the key into the first error message that wrapped
/// it (§3a rule 3).
pub struct BitrpcTransport {
    agent: ureq::Agent,
    url: String,
    api_key: zeroize::Zeroizing<String>,
    budget: Arc<CallBudget>,
    lane: Lane,
}

impl fmt::Debug for BitrpcTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BitrpcTransport")
            .field("url", &self.url)
            .field("api_key", &"<redacted>")
            .field("lane", &self.lane)
            .finish()
    }
}

impl BitrpcTransport {
    pub fn new(cfg: &BitrpcConfig, budget: Arc<CallBudget>, lane: Lane) -> Self {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(REQUEST_TIMEOUT))
                // We need to see 401/403/429/502 ourselves to classify them;
                // ureq's default turns them into an opaque error.
                .http_status_as_error(false)
                .build(),
        );

        BitrpcTransport {
            agent,
            url: format!("{}/bitcoin", cfg.url.trim_end_matches('/')),
            api_key: cfg.api_key.clone(),
            budget,
            lane,
        }
    }

    fn post(&self, body: &str) -> std::result::Result<String, jsonrpc::Error> {
        // Never exceed the key's budget on our side: a self-inflicted 429 costs
        // a round trip and, worse, tells us nothing about which call to retry.
        self.budget.acquire_blocking(self.lane);

        let response = self
            .agent
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("X-API-Key", self.api_key.as_str())
            .send(body)
            .map_err(|e| transport_err(TransportError::Http(e.to_string())))?;

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);

        if let Some(classified) = TransportError::from_status(status, retry_after) {
            return Err(transport_err(classified));
        }

        response
            .into_body()
            .read_to_string()
            .map_err(|e| transport_err(TransportError::Http(e.to_string())))
    }
}

fn transport_err(e: TransportError) -> jsonrpc::Error {
    jsonrpc::Error::Transport(Box::new(e))
}

impl jsonrpc::Transport for BitrpcTransport {
    fn send_request(
        &self,
        req: jsonrpc::Request,
    ) -> std::result::Result<jsonrpc::Response, jsonrpc::Error> {
        let body = serde_json::to_string(&req)?;
        let started = Instant::now();
        let raw = self.post(&body)?;
        tracing::trace!(
            method = req.method,
            lane = ?self.lane,
            ms = started.elapsed().as_millis(),
            "bitrpc call"
        );
        Ok(serde_json::from_str(&raw)?)
    }

    fn send_batch(
        &self,
        reqs: &[jsonrpc::Request],
    ) -> std::result::Result<Vec<jsonrpc::Response>, jsonrpc::Error> {
        let body = serde_json::to_string(reqs)?;
        let raw = self.post(&body)?;
        Ok(serde_json::from_str(&raw)?)
    }

    fn fmt_target(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // The URL, and deliberately not the key.
        write!(f, "{}", self.url)
    }
}

/// Build an `RpcApi` client that speaks to BitRPC (§2, §4b).
///
/// `lane` decides which allowance this client's calls draw from: the interactive
/// client serves `/send` and `/status`, and the sync client is handed to the
/// block emitter.
pub fn client_for(cfg: &BitrpcConfig, budget: Arc<CallBudget>, lane: Lane) -> Result<Client> {
    if cfg.api_key.trim().is_empty() {
        return Err(CoreError::MissingConfig("BITRPC_API_KEY"));
    }
    let transport = BitrpcTransport::new(cfg, budget, lane);
    Ok(Client::from_jsonrpc(jsonrpc::Client::with_transport(
        transport,
    )))
}

/// The interactive client (`ChainSource`'s default).
pub fn client(cfg: &BitrpcConfig, budget: Arc<CallBudget>) -> Result<Client> {
    client_for(cfg, budget, Lane::Interactive)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BitrpcConfig;
    use bdk_wallet::bitcoin::FeeRate;

    fn cfg(url: &str, key: &str) -> BitrpcConfig {
        BitrpcConfig {
            url: url.to_string(),
            api_key: zeroize::Zeroizing::new(key.to_string()),
            rate_limit_per_min: 90,
            sync_budget_per_min: 60,
            max_rescan_blocks: 10_000,
            min_fee: FeeRate::from_sat_per_vb(1).expect("1 sat/vb is a valid rate"),
            fee_api: "https://example.invalid".into(),
            payjoin_directory: "https://example.invalid".into(),
            ohttp_relay: "https://example.invalid".into(),
        }
    }

    #[test]
    fn every_http_status_maps_to_the_documented_verdict() {
        // §4b: 401 missing key, 403 invalid key or method not permitted,
        // 429 rate limited, 502 node unavailable.
        assert!(TransportError::from_status(200, None).is_none());
        assert!(matches!(
            TransportError::from_status(401, None),
            Some(TransportError::Unauthorized)
        ));
        assert!(matches!(
            TransportError::from_status(403, None),
            Some(TransportError::Forbidden)
        ));
        assert!(matches!(
            TransportError::from_status(429, None),
            Some(TransportError::RateLimited { .. })
        ));
        assert!(matches!(
            TransportError::from_status(502, None),
            Some(TransportError::NodeUnavailable)
        ));
    }

    #[test]
    fn a_403_names_the_method_the_allowlist_refused() {
        let backend = TransportError::Forbidden.into_backend("estimatesmartfee");
        match backend {
            BackendError::Forbidden { method } => assert_eq!(method, "estimatesmartfee"),
            other => panic!("expected Forbidden, got {other:?}"),
        }
    }

    #[test]
    fn a_429_carries_retry_after_so_only_retryable_errors_are_retried() {
        let backend = TransportError::from_status(429, Some(Duration::from_secs(7)))
            .expect("429 classifies")
            .into_backend("getblockcount");
        match backend {
            BackendError::RateLimited { retry_after } => {
                assert_eq!(retry_after, Some(Duration::from_secs(7)));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn the_api_key_is_redacted_from_the_transports_debug_output() {
        let budget = Arc::new(CallBudget::new(90, 60));
        let t = BitrpcTransport::new(
            &cfg("https://example.invalid", "super-secret-key"),
            budget,
            Lane::Interactive,
        );
        let rendered = format!("{t:?}");
        assert!(!rendered.contains("super-secret-key"), "API key leaked");
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn the_transports_target_is_the_bitcoin_endpoint_without_the_key() {
        let budget = Arc::new(CallBudget::new(90, 60));
        let t = BitrpcTransport::new(
            &cfg("https://bitrpc.example/", "k"),
            budget,
            Lane::Interactive,
        );
        // A trailing slash in config must not become a double slash in the URL.
        assert_eq!(t.url, "https://bitrpc.example/bitcoin");
    }

    #[test]
    fn an_empty_api_key_is_refused_before_a_request_is_made() {
        let budget = Arc::new(CallBudget::new(90, 60));
        assert!(matches!(
            client(&cfg("https://example.invalid", "  "), budget),
            Err(CoreError::MissingConfig("BITRPC_API_KEY"))
        ));
    }

    #[test]
    fn the_budget_blocks_once_the_quota_is_spent() {
        // A one-call-per-minute budget: the first call passes immediately, the
        // second cannot, which is what keeps us off BitRPC's own 429.
        let budget = CallBudget::new(1, 1);
        budget.acquire_blocking(Lane::Interactive);
        assert_eq!(budget.used(), 1);
        assert!(
            budget.shared.check().is_err(),
            "a spent quota must refuse the next call"
        );
    }

    #[test]
    fn the_sync_lane_cannot_consume_the_whole_key() {
        // §4: the emitter draws from a smaller allowance, so an interactive
        // call still has room after the sync lane has spent its own.
        let budget = CallBudget::new(10, 1);
        budget.acquire_blocking(Lane::Sync);
        assert!(budget.sync.check().is_err(), "the sync allowance is spent");
        assert!(
            budget.shared.check().is_ok(),
            "an interactive call must still have budget left"
        );
    }
}
