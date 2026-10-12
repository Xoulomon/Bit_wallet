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
    config::{BackendConfig, CoreRpcConfig},
    service::types::{Pin, PayjoinState, SendAmount, SendRequest, SessionId, UserId},
};
use zeroize::Zeroizing;

/// Wait for the receiver's own session to reach a verdict (§7 step 4).
///
/// This is the assertion the test used to be missing entirely, and its absence
/// is what let a broken receive path look healthy: everything here only ever
/// checked the *sender's* `Broadcast`, so a receiver that polled for an hour
/// and then reported `Expired` passed just as well as one that completed.
async fn settled(
    core: &WalletService,
    user: UserId,
    session: SessionId,
    patience: Duration,
) -> PayjoinState {
    let deadline = std::time::Instant::now() + patience;

    loop {
        let found = core
            .payjoin_sessions(user)
            .await
            .expect("sessions list")
            .into_iter()
            .find(|s| s.id == session)
            .expect("the session we opened is listed");

        let settled = matches!(
            found.state,
            PayjoinState::Completed { .. }
                | PayjoinState::FellBack { .. }
                | PayjoinState::Expired
                | PayjoinState::Cancelled
                | PayjoinState::Failed { .. }
        );

        if settled || std::time::Instant::now() > deadline {
            return found.state;
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

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

    // `-txindex` is for the test's own benefit, not the wallet's: checking that
    // one of the final transaction's inputs is the *receiver's* means looking
    // up each input's parent, and without an index `getrawtransaction` only
    // answers for transactions still in the mempool. Nothing in `wallet-core`
    // depends on an index — see `onchain::wallet` on why.
    let mut conf = corepc_node::Conf::default();
    conf.args.push("-txindex");
    let node =
        Node::from_downloaded_with_conf(&conf).expect("a regtest bitcoind starts with an index");
    let dir = tempfile::tempdir().expect("temp dir");
    let (user, pass) = match node.params.get_cookie_values().expect("cookie readable") {
        Some(v) => (v.user, v.password),
        None => ("__cookie__".to_string(), String::new()),
    };

    let cfg = AppConfig {
        network: NetworkChoice::Regtest,
        backend: BackendConfig::Core(CoreRpcConfig {
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
        price_api: "https://api.coingecko.com/api/v3".into(),
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
        .confirm_send(sender, quote.id, &pin)
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

    // §6 step 6: the sender's wallet knows about it *before* a block. On a
    // backend with no mempool this is the only way an outgoing payment shows
    // at all until it confirms, and the payjoin path used to return without
    // recording it — so a payjoin send was invisible in /balance and
    // /history, and /bumpfee could not find it.
    let pending = core
        .history(sender, wallet_core::service::types::Page::new(0))
        .await
        .expect("history");
    assert!(
        pending.items.iter().any(|t| t.txid == broadcast.txid),
        "the payjoin must appear in the sender's history before a block lands"
    );

    // §10: inputs from both wallets. That is the whole privacy claim — the
    // common-input-ownership heuristic now points at two owners, not one.
    //
    // Checking the *count* is not enough: a sender spending two of its own
    // coins satisfies that on its own. What has to be true is that one of the
    // inputs belongs to the receiver.
    assert!(
        tx.input.len() >= 2,
        "a payjoin has inputs from both parties, found {}",
        tx.input.len()
    );

    let mut receiver_scripts = Vec::new();
    for page in 0..5 {
        let listed = core
            .addresses(receiver, wallet_core::service::types::Page::new(page))
            .await
            .expect("addresses");
        if listed.items.is_empty() {
            break;
        }
        receiver_scripts.extend(listed.items.iter().map(|a| a.address.script_pubkey()));
    }
    assert!(
        !receiver_scripts.is_empty(),
        "the receiver has revealed addresses to compare against"
    );
    let contributed = tx.input.iter().any(|input| {
        node.client
            .get_raw_transaction(input.previous_output.txid)
            .ok()
            .and_then(|raw| raw.transaction().ok())
            .and_then(|parent| {
                parent
                    .output
                    .get(input.previous_output.vout as usize)
                    .cloned()
            })
            .is_some_and(|out| receiver_scripts.contains(&out.script_pubkey))
    });
    assert!(
        contributed,
        "one of the inputs has to be the receiver's, or no heuristic was broken"
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

    // --- and the receiver agrees -------------------------------------------
    //
    // §7 step 4. The receiver only learns which transaction settled its
    // session by driving the payjoin crate's `Monitor` typestate, and nothing
    // used to drive it: the session polled until its hour was up and reported
    // `Expired` — for a payjoin that had completed perfectly. Everything above
    // this line passed throughout.
    let state = settled(&core, receiver, receipt.session_id, Duration::from_secs(180)).await;
    eprintln!("receiver session ended as {state:?}");

    assert_eq!(
        state,
        PayjoinState::Completed {
            txid: broadcast.txid
        },
        "the receiver must report the payjoin it actually took part in, naming \
         the transaction that settled it"
    );

    core.shutdown().await;
}

/// §7: a receiver that is cancelled stops, rather than carrying on for its
/// full hour and then overwriting the user's `Cancelled` with `Expired`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the public payjoin directory; run with --ignored"]
async fn a_cancelled_receive_session_stops_and_stays_cancelled() {
    let (node, cfg, _dir) = harness();
    let core = WalletService::new(cfg).await.expect("service starts");

    let receiver = UserId::new();
    let pin = Pin::new("864213");
    core.create_wallet(receiver, &pin).await.expect("receiver");

    let miner = node.client.new_address().expect("node address");
    node.client.generate_to_address(101, &miner).expect("mines");

    let receipt = core
        .payjoin_receive(receiver, Amount::from_sat(50_000))
        .await
        .expect("session opens");

    // Let the polling task get going, so cancellation has something to stop.
    tokio::time::sleep(Duration::from_secs(10)).await;

    core.payjoin_cancel(receiver, receipt.session_id)
        .await
        .expect("cancels");

    // Long enough for a task that ignored the cancel to come back round and
    // overwrite the state, which is exactly what used to happen.
    tokio::time::sleep(Duration::from_secs(30)).await;

    let state = core
        .payjoin_sessions(receiver)
        .await
        .expect("sessions")
        .into_iter()
        .find(|s| s.id == receipt.session_id)
        .expect("listed")
        .state;

    assert_eq!(
        state,
        PayjoinState::Cancelled,
        "a cancelled session must stay cancelled"
    );

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
        .confirm_send(sender, quote.id, &pin)
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

    // §6 step 6 again: a fallback is a payment, so it has to be visible as
    // one straight away.
    let pending = core
        .history(sender, wallet_core::service::types::Page::new(0))
        .await
        .expect("history");
    assert!(
        pending.items.iter().any(|t| t.txid == broadcast.txid),
        "the fallback must appear in the sender's history before a block lands"
    );

    // The sender's own session has to say `FellBack`, with the transaction
    // that actually went out — not `Completed` with an invented txid.
    let state = settled(
        &core,
        sender,
        sender_session(&core, sender).await,
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        state,
        PayjoinState::FellBack {
            txid: broadcast.txid
        },
        "a fallback is a payjoin that did not happen, and must not be reported \
         as one that did"
    );

    core.shutdown().await;
}

/// The sender's own session id, which `confirm_send` does not hand back.
async fn sender_session(core: &WalletService, user: UserId) -> SessionId {
    core.payjoin_sessions(user)
        .await
        .expect("sessions")
        .into_iter()
        .find(|s| s.role == wallet_core::service::types::PayjoinRole::Sender)
        .expect("the send attempt opened a session")
        .id
}
