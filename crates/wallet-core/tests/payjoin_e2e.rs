//! Payjoin end to end: one wallet pays another with payjoin (PLAN.md §7, §10).
//!
//! Both sides run in this process against a real directory and relay, so the
//! whole BIP77 v2 exchange is exercised: the receiver opens a session and
//! polls, the sender posts the Original PSBT, the receiver walks the typestate
//! checks and contributes an input, the sender signs the proposal and
//! broadcasts it.
//!
//! **Why this needs the network, and is therefore `#[ignore]`d.** §10 asks for
//! `payjoin-test-utils`, which runs an in-process directory and relay. Its only
//! published version, 0.0.1, depends on `payjoin` 0.24 while this project uses
//! 1.1, so adding it puts two incompatible copies of the crate in one tree. A
//! fully local pair is no use either: OHTTP requires the relay and directory to
//! be different operators, and `payjoin-mailroom` 0.1.2 cannot point its relay
//! at a local gateway (see docs/payjoin-setup.md). So the honest options were a
//! network test or none, and none is worse.
//!
//! Run it with:
//!
//!     cargo test -p wallet-core --test payjoin_e2e -- --ignored --nocapture

use bdk_wallet::bitcoin::{Amount, FeeRate};
use corepc_node::Node;
use std::time::Duration;
use wallet_core::{
    AppConfig, NetworkChoice, WalletService,
    config::{BackendConfig, RegtestConfig},
    service::types::{Auth, Pin, SendAmount, SendRequest, UserId},
};
use zeroize::Zeroizing;

/// Public infrastructure, as `.env.example` uses. The directory only stores and
/// forwards PSBTs, so a regtest session through it moves nothing real.
const DIRECTORY: &str = "https://payjo.in";
const RELAY: &str = "https://pj.benalleng.com";

/// Without a subscriber, core's own account of what the exchange did goes
/// nowhere — and that account is most of the value of this test when it fails.
fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("wallet_core=debug")),
        )
        .with_test_writer()
        .try_init();
}

fn harness() -> (Node, AppConfig, tempfile::TempDir) {
    init_logging();
    let node = Node::from_downloaded().expect("a regtest bitcoind starts");
    let dir = tempfile::tempdir().expect("temp dir");
    let (user, pass) = match node.params.get_cookie_values().expect("cookie readable") {
        Some(v) => (v.user, v.password),
        None => ("__cookie__".to_string(), String::new()),
    };

    let cfg = AppConfig {
        network: NetworkChoice::Regtest,
        backend: BackendConfig::Regtest(RegtestConfig {
            rpc_url: node.rpc_url(),
            rpc_user: user,
            rpc_pass: Zeroizing::new(pass),
            payjoin_directory: DIRECTORY.into(),
            ohttp_relay: RELAY.into(),
            fallback_fee: FeeRate::from_sat_per_vb(2).expect("valid"),
        }),
        data_dir: dir.path().to_path_buf(),
        session_idle_timeout: Duration::from_secs(1800),
        max_send: None,
        fee_cache: Duration::from_secs(60),
    };

    (node, cfg, dir)
}

/// §10: "receiver and sender wallets complete a v2 payjoin, the final tx has
/// inputs from both".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the public payjoin directory; run with --ignored"]
async fn two_wallets_complete_a_v2_payjoin() {
    let (node, cfg, _dir) = harness();
    let core = WalletService::new(cfg.clone())
        .await
        .expect("service starts");

    let receiver = UserId::new();
    let sender = UserId::new();
    let pin = Pin::new("864213");

    core.create_wallet(receiver, &pin)
        .await
        .expect("receiver wallet");
    core.create_wallet(sender, &pin)
        .await
        .expect("sender wallet");

    // Both sides need coins: the sender to pay, and the receiver to have a UTXO
    // worth contributing — a payjoin where only one party has inputs is just a
    // transaction.
    let miner = node.client.new_address().expect("node address");
    node.client.generate_to_address(101, &miner).expect("mines");

    for (who, sats) in [(receiver, 400_000u64), (sender, 600_000)] {
        let address = core.next_address(who).await.expect("address").address;
        node.client
            .send_to_address(&address, Amount::from_sat(sats))
            .expect("funds");
    }
    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");
    core.sync_now(receiver).await.expect("syncs receiver");
    core.sync_now(sender).await.expect("syncs sender");

    let receiver_before = core.balance(receiver).await.expect("balance").confirmed;
    let sender_before = core.balance(sender).await.expect("balance").confirmed;
    assert_eq!(receiver_before, Amount::from_sat(400_000));
    assert_eq!(sender_before, Amount::from_sat(600_000));

    // --- the receiver asks to be paid -------------------------------------
    let amount = Amount::from_sat(120_000);
    let receipt = core
        .payjoin_receive(receiver, amount)
        .await
        .expect("a payjoin session opens");

    eprintln!("receiver URI: {}", receipt.bip21);
    assert!(
        receipt.bip21.to_lowercase().contains("pj="),
        "the URI offers payjoin"
    );

    // Wait out at least one silent long poll before the sender appears.
    //
    // This is the realistic case and it used to break everything: a receiver
    // whose first poll timed out treated that as fatal and closed the session,
    // so a payjoin only worked if the sender was already standing there. A
    // human takes longer than that to scan a QR.
    tokio::time::sleep(Duration::from_secs(40)).await;

    // --- the sender pays it ------------------------------------------------
    let target = core
        .parse_payment(&receipt.bip21)
        .expect("the receiver's URI parses");
    assert!(
        target.payjoin_endpoint.is_some(),
        "parse_payment must see the pj= endpoint, or the ordinary path is taken"
    );

    let quote = core
        .quote_send(
            sender,
            SendRequest {
                target,
                raw: receipt.bip21.clone(),
                amount: SendAmount::Exact(amount),
                fee_rate: FeeRate::from_sat_per_vb(2).expect("valid"),
            },
        )
        .await
        .expect("quotes");

    assert!(quote.is_payjoin, "the quote must know this is a payjoin");

    let broadcast = core
        .confirm_send(sender, quote.id, Auth::Session)
        .await
        .expect("the payjoin completes or falls back, but must not error");

    eprintln!(
        "broadcast {} (payjoin: {})",
        broadcast.txid, broadcast.payjoin
    );

    // --- what actually landed ---------------------------------------------
    let raw = node
        .client
        .get_raw_transaction(broadcast.txid)
        .expect("the node has the transaction we broadcast");
    let tx = raw.transaction().expect("decodes");

    assert!(
        broadcast.payjoin,
        "the receiver was online and willing, so this should have been a payjoin, \
         not a fallback"
    );

    // §10: inputs from both wallets. That is the whole privacy claim — the
    // common-input-ownership heuristic now points at two owners, not one.
    assert!(
        tx.input.len() >= 2,
        "a payjoin has inputs from both parties, found {}",
        tx.input.len()
    );

    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");
    core.sync_now(receiver).await.expect("syncs receiver");
    core.sync_now(sender).await.expect("syncs sender");

    let receiver_after = core.balance(receiver).await.expect("balance").confirmed;
    let sender_after = core.balance(sender).await.expect("balance").confirmed;
    eprintln!(
        "receiver {} -> {}   sender {} -> {}",
        receiver_before.to_sat(),
        receiver_after.to_sat(),
        sender_before.to_sat(),
        sender_after.to_sat()
    );

    assert!(
        receiver_after > receiver_before,
        "the receiver ended up with more than they started with"
    );
    assert!(sender_after < sender_before, "the sender paid");

    core.shutdown().await;
}

/// §7: a payjoin that does not complete must still pay. With no receiver
/// listening, the sender falls back to the Original PSBT and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the public payjoin directory; run with --ignored"]
async fn an_unanswered_payjoin_falls_back_to_an_ordinary_payment() {
    let (node, mut cfg, _dir) = harness();

    // A short deadline so the test does not sit through the real one.
    cfg.session_idle_timeout = Duration::from_secs(1800);
    let core = WalletService::new(cfg.clone())
        .await
        .expect("service starts");

    let receiver = UserId::new();
    let sender = UserId::new();
    let pin = Pin::new("864213");
    core.create_wallet(receiver, &pin).await.expect("receiver");
    core.create_wallet(sender, &pin).await.expect("sender");

    let miner = node.client.new_address().expect("node address");
    node.client.generate_to_address(101, &miner).expect("mines");
    let sender_address = core.next_address(sender).await.expect("address").address;
    node.client
        .send_to_address(&sender_address, Amount::from_sat(500_000))
        .expect("funds the sender");
    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");
    core.sync_now(sender).await.expect("syncs");

    // A session the receiver opens and then abandons: cancel it so nobody is
    // polling, which is what a sender meets when the other side has gone away.
    let receipt = core
        .payjoin_receive(receiver, Amount::from_sat(50_000))
        .await
        .expect("session opens");
    core.payjoin_cancel(receiver, receipt.session_id)
        .await
        .expect("cancels");

    let target = core.parse_payment(&receipt.bip21).expect("parses");
    let quote = core
        .quote_send(
            sender,
            SendRequest {
                target,
                raw: receipt.bip21.clone(),
                amount: SendAmount::Exact(Amount::from_sat(50_000)),
                fee_rate: FeeRate::from_sat_per_vb(2).expect("valid"),
            },
        )
        .await
        .expect("quotes");

    let broadcast = core
        .confirm_send(sender, quote.id, Auth::Session)
        .await
        .expect("a payjoin that cannot complete must still pay");

    assert!(
        !broadcast.payjoin,
        "with nobody listening this must be reported as a fallback, not a payjoin"
    );

    // And the money really moved: a fallback is a payment, not an error.
    node.client
        .get_raw_transaction(broadcast.txid)
        .expect("the fallback transaction was broadcast");

    core.shutdown().await;
}
