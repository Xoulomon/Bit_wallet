//! The price sources, against the real APIs (PLAN.md §10).
//!
//! `#[ignore]`d for the same reason `payjoin_e2e` is: it needs the public
//! internet, so it must never be what makes `cargo test` red on a laptop in a
//! tunnel. Run it when a dollar figure stops appearing, or to check an API
//! still answers the shape we parse:
//!
//! ```bash
//! cargo test -p wallet-core --test price_live -- --ignored --nocapture
//! ```
//!
//! This exists because the failure it diagnoses is *invisible* in the chat:
//! a card with no price renders exactly like a card from a build that never
//! had the feature.

use wallet_core::rpc::price::{CoinGecko, MempoolSpacePrice, PriceFeed, PriceSource};

/// A sanity band rather than a fixed number, so the test does not need
/// rewriting every bull run. It is here to catch a parse that silently
/// produced 0, or a field read in the wrong unit.
fn plausible(usd: f64) -> bool {
    (1_000.0..10_000_000.0).contains(&usd)
}

#[test]
#[ignore]
fn coingecko_answers_the_shape_we_parse() {
    let src = CoinGecko::new("https://api.coingecko.com/api/v3");
    match src.usd_per_btc() {
        Ok(usd) => {
            println!("CoinGecko: 1 BTC = ${usd}");
            assert!(plausible(usd), "${usd} is not a plausible BTC price");
        }
        Err(e) => panic!("CoinGecko did not answer: {e}"),
    }
}

#[test]
#[ignore]
fn mempool_space_answers_the_shape_we_parse() {
    let src = MempoolSpacePrice::new("https://mempool.space/api");
    match src.usd_per_btc() {
        Ok(usd) => {
            println!("mempool.space: 1 BTC = ${usd}");
            assert!(plausible(usd), "${usd} is not a plausible BTC price");
        }
        Err(e) => panic!("mempool.space did not answer: {e}"),
    }
}

/// The whole feed, as the bot uses it — including which source won.
#[test]
#[ignore]
fn the_feed_produces_a_price() {
    let feed = PriceFeed::new("https://api.coingecko.com/api/v3");
    match feed.get() {
        Some(p) => {
            println!("feed: 1 BTC = ${} (from {})", p.usd_per_btc, p.source);
            assert!(plausible(p.usd_per_btc));
        }
        None => panic!("no source answered — dollar values would be hidden"),
    }
}

/// The fallback is the reason CoinGecko being unreachable is not the end of
/// it, so prove the chain actually falls through rather than giving up.
#[test]
#[ignore]
fn an_unreachable_primary_falls_through_to_the_fallback() {
    let feed = PriceFeed::with_sources(vec![
        Box::new(CoinGecko::new("https://price.invalid")),
        Box::new(MempoolSpacePrice::new("https://mempool.space/api")),
    ]);
    let p = feed.get().expect("the fallback should have answered");
    assert_eq!(p.source, "mempool.space");
    println!("fell through to {} at ${}", p.source, p.usd_per_btc);
}
