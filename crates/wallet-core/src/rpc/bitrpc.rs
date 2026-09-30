//! Mainnet: BitRPC's hosted Core behind `X-API-Key` (PLAN.md §4b).
//!
//! Neither `jsonrpc::simple_http` nor `minreq_http` can set an arbitrary header
//! — both only ever emit `Authorization` basic auth — so the transport is
//! hand-written and wrapped by `Client::from_jsonrpc`, which yields a genuine
//! `RpcApi` that `bdk_bitcoind_rpc::Emitter` consumes unchanged (§2).
//!
//! Step 2 fills these bodies in. The signatures are fixed now so the layers
//! above are written against the finished shape.

use crate::{
    config::BitrpcConfig,
    error::{BackendError, Result},
};
use bitcoincore_rpc::Client;
use std::{sync::Arc, time::Duration};

/// The shared 100 req/min budget (§4).
///
/// One instance per process, held by `ChainSource` and consulted by every
/// mainnet call. The emitter draws from a second, smaller allowance so a sync
/// backlog can never starve an interactive `/send`.
pub struct CallBudget {
    _limit_per_min: u32,
    _sync_per_min: u32,
}

/// Which allowance a call draws from. Interactive calls get the full budget;
/// the block emitter is capped (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Interactive,
    Sync,
}

impl CallBudget {
    pub fn new(limit_per_min: u32, sync_per_min: u32) -> Self {
        CallBudget {
            _limit_per_min: limit_per_min,
            _sync_per_min: sync_per_min,
        }
    }

    /// Block until a call in this lane may proceed. Used on the emitter's
    /// blocking thread.
    pub fn acquire_blocking(&self, _lane: Lane) {
        todo!("Step 2: governor rate limiter, blocking wait")
    }

    /// Await a slot from async code.
    pub async fn acquire(&self, _lane: Lane) {
        todo!("Step 2: governor rate limiter, async wait")
    }

    /// Calls spent in the current minute, for `/status` (§8.2).
    pub fn used(&self) -> u32 {
        todo!("Step 2: budget accounting")
    }

    pub fn limit(&self) -> u32 {
        self._limit_per_min
    }
}

/// An HTTP status BitRPC returned, classified before it reaches the layers
/// above (§4b). Carried through `jsonrpc::Error::Transport` as a boxed error and
/// downcast by `rpc::map_rpc_error`, which is what keeps the mapping in one
/// place. It never contains the API key.
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
}

/// Build an `RpcApi` client that speaks to BitRPC (§2, §4b).
pub fn client(_cfg: &BitrpcConfig, _budget: Arc<CallBudget>) -> Result<Client> {
    todo!("Step 2: X-API-Key jsonrpc::Transport wrapped by Client::from_jsonrpc")
}
