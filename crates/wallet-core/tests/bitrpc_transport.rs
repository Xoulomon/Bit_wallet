//! The BitRPC transport, against a local mock HTTP server (PLAN.md §10).
//!
//! These are the tests that prove §4b's contract holds at the wire: the header
//! is on every request, each documented status maps to its own `CoreError`
//! variant, and the key never appears in what we log or return.

use bdk_wallet::bitcoin::FeeRate;
use bitcoincore_rpc::RpcApi;
use httpmock::prelude::*;
use std::sync::Arc;
use wallet_core::rpc::bitrpc::{CallBudget, Lane, client_for};
use wallet_core::rpc::map_rpc_error;
use wallet_core::{BackendError, CoreError, config::BitrpcConfig};

const API_KEY: &str = "smoke-test-key-never-real";

fn config(base: String) -> BitrpcConfig {
    BitrpcConfig {
        url: base,
        api_key: zeroize::Zeroizing::new(API_KEY.to_string()),
        rate_limit_per_min: 600,
        sync_budget_per_min: 600,
        max_rescan_blocks: 10_000,
        min_fee: FeeRate::from_sat_per_vb(1).expect("a valid rate"),
        fee_api: "https://example.invalid".into(),
        payjoin_directory: "https://example.invalid".into(),
        ohttp_relay: "https://example.invalid".into(),
    }
}

fn client(server: &MockServer) -> bitcoincore_rpc::Client {
    let budget = Arc::new(CallBudget::new(600, 600));
    client_for(&config(server.base_url()), budget, Lane::Interactive).expect("client builds")
}

#[test]
fn the_api_key_header_is_set_on_every_request() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST)
            .path("/bitcoin")
            .header("X-API-Key", API_KEY);
        then.status(200)
            .header("content-type", "application/json")
            .body(r#"{"result":901234,"error":null,"id":"1"}"#);
    });

    let rpc = client(&server);
    let height = rpc.get_block_count().expect("the call succeeds");

    assert_eq!(height, 901_234);
    // Asserted by the matcher above: without the header, no mock would match.
    mock.assert();
}

/// The statuses of §4b, each to its own variant, so the front end can retry
/// only where retrying helps.
#[test]
fn each_documented_status_maps_to_its_own_error() {
    for (status, check) in [
        (401u16, "missing_key"),
        (403, "forbidden"),
        (429, "rate_limited"),
        (502, "node_unavailable"),
    ] {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/bitcoin");
            then.status(status).body("{}");
        });

        let rpc = client(&server);
        let err = rpc
            .get_block_count()
            .map_err(map_rpc_error("getblockcount"))
            .expect_err("a failing status must not look like success");

        match (check, &err) {
            ("missing_key", CoreError::Backend(BackendError::MissingApiKey)) => {}
            ("forbidden", CoreError::Backend(BackendError::Forbidden { method })) => {
                assert_eq!(method, "getblockcount", "a 403 must name the refused call");
            }
            ("rate_limited", CoreError::Backend(BackendError::RateLimited { .. })) => {}
            ("node_unavailable", CoreError::Backend(BackendError::NodeUnavailable)) => {}
            _ => panic!("HTTP {status} mapped to the wrong variant: {err:?}"),
        }

        // §3a rule 3: nothing that reaches a log line may carry the key.
        let rendered = format!("{err} {err:?}");
        assert!(!rendered.contains(API_KEY), "API key leaked into an error");
    }
}

/// A JSON-RPC error object is a different thing from an HTTP failure, and the
/// node's own words are what make a broadcast rejection useful (§6).
#[test]
fn a_json_rpc_error_object_keeps_the_nodes_own_reason() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/bitcoin");
        then.status(200).body(
            r#"{"result":null,"error":{"code":-26,"message":"min relay fee not met"},"id":"1"}"#,
        );
    });

    let rpc = client(&server);
    let err = rpc
        .get_block_count()
        .map_err(map_rpc_error("getblockcount"))
        .expect_err("an error object is an error");

    match err {
        CoreError::Backend(BackendError::Rpc { code, message }) => {
            assert_eq!(code, -26);
            assert_eq!(message, "min relay fee not met");
        }
        other => panic!("expected an Rpc variant, got {other:?}"),
    }
}

/// The limiter exists so we never *reach* BitRPC's own 429 (§4).
#[test]
fn the_shared_budget_serialises_calls_past_the_quota() {
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/bitcoin");
        then.status(200)
            .body(r#"{"result":1,"error":null,"id":"1"}"#);
    });

    // Two calls a minute, both of which must be drawn from the budget.
    let budget = Arc::new(CallBudget::new(2, 2));
    let rpc = client_for(
        &config(server.base_url()),
        Arc::clone(&budget),
        Lane::Interactive,
    )
    .expect("client builds");

    rpc.get_block_count().expect("first call");
    rpc.get_block_count().expect("second call");

    // Every call went through the budget — which is the only reason we never
    // reach BitRPC's own 429. (That the budget then blocks is asserted in the
    // unit test; waiting a real minute here would buy nothing.)
    assert_eq!(budget.used(), 2);
}
