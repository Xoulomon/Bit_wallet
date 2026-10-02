//! The boundary, exercised rather than asserted (PLAN.md §9 Step 8, §10).
//!
//! `wallet-cli` is the proof that §3a holds, so this drives the same sequence
//! the CLI does — create, receive, balance, quote, confirm — entirely through
//! `WalletService`, against a real bitcoind. If a step here needed a private
//! module, the facade would be missing a method, which is exactly the leak the
//! second front end exists to catch.

use bdk_wallet::bitcoin::{Amount, FeeRate};
use corepc_node::Node;
use std::time::Duration;
use wallet_core::{
    AppConfig, NetworkChoice, WalletService,
    config::{BackendConfig, RegtestConfig},
    service::types::{Auth, Page, Pin, SendAmount, SendRequest, UserId},
};
use zeroize::Zeroizing;

fn harness() -> (Node, AppConfig, tempfile::TempDir) {
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
            payjoin_directory: "http://localhost:8080".into(),
            ohttp_relay: "http://localhost:3000".into(),
            fallback_fee: FeeRate::from_sat_per_vb(2).expect("valid"),
        }),
        data_dir: dir.path().to_path_buf(),
        session_idle_timeout: Duration::from_secs(600),
        max_send: None,
        fee_cache: Duration::from_secs(60),
    };

    (node, cfg, dir)
}

/// The CLI's own sequence, over the facade and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_front_end_can_run_the_whole_wallet_through_the_facade() {
    let (node, cfg, _dir) = harness();
    let core = WalletService::new(cfg).await.expect("service starts");
    let alice = UserId::new();
    let bob = UserId::new();
    let pin = Pin::new("864213");

    // create
    let wallet = core.create_wallet(alice, &pin).await.expect("creates");
    assert_eq!(wallet.mnemonic.split_whitespace().count(), 12);
    assert!(core.wallet_exists(alice).expect("queries"));

    // status
    let status = core.status().await.expect("status");
    assert_eq!(status.network, bdk_wallet::bitcoin::Network::Regtest);
    assert!(
        status.calls_used.is_none(),
        "regtest has no shared call budget to report"
    );

    // receive
    let address = core.next_address(alice).await.expect("address").address;
    assert!(address.to_string().starts_with("bcrt1"));

    // Fund it.
    let miner = node.client.new_address().expect("node address");
    node.client.generate_to_address(101, &miner).expect("mines");
    node.client
        .send_to_address(&address, Amount::from_sat(400_000))
        .expect("funds");
    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");

    core.sync_now(alice).await.expect("syncs");

    // balance
    let balance = core.balance(alice).await.expect("balance");
    assert_eq!(balance.confirmed, Amount::from_sat(400_000));

    // history
    let history = core.history(alice, Page::new(0)).await.expect("history");
    assert_eq!(history.total, 1);

    // fees
    let fees = core.fee_options().await.expect("fees");
    assert!(fees.allows_custom);

    // send: quote, then confirm, exactly as §3a rule 4 requires.
    let bob_address = {
        let other = core.create_wallet(bob, &pin).await;
        assert!(other.is_ok(), "a second user gets their own wallet");
        core.next_address(bob).await.expect("address").address
    };

    let target = core
        .parse_payment(&bob_address.to_string())
        .expect("parses");

    let quote = core
        .quote_send(
            alice,
            SendRequest {
                target,
                raw: bob_address.to_string(),
                amount: SendAmount::Exact(Amount::from_sat(100_000)),
                fee_rate: FeeRate::from_sat_per_vb(2).expect("valid"),
            },
        )
        .await
        .expect("quotes");

    assert_eq!(quote.amount, Amount::from_sat(100_000));
    assert!(quote.fee > Amount::ZERO);
    assert!(!quote.is_payjoin);

    // Alice's session is open from create_wallet, so no PIN is needed.
    let sent = core
        .confirm_send(alice, quote.id, Auth::Session)
        .await
        .expect("broadcasts");
    assert_eq!(sent.amount, Amount::from_sat(100_000));

    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");
    core.sync_now(bob).await.expect("syncs bob");

    assert_eq!(
        core.balance(bob).await.expect("balance").confirmed,
        Amount::from_sat(100_000),
        "the payment arrived at the second wallet"
    );

    core.shutdown().await;
}

/// §3a rule 4 and §8.5: the quote is re-validated on confirm, so an id that
/// leaked to another user is worth nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_quote_cannot_be_confirmed_by_anyone_but_its_owner() {
    let (node, cfg, _dir) = harness();
    let core = WalletService::new(cfg).await.expect("service starts");
    let alice = UserId::new();
    let mallory = UserId::new();
    let pin = Pin::new("864213");

    core.create_wallet(alice, &pin).await.expect("creates");
    core.create_wallet(mallory, &pin).await.expect("creates");

    let address = core.next_address(alice).await.expect("address").address;
    let miner = node.client.new_address().expect("node address");
    node.client.generate_to_address(101, &miner).expect("mines");
    node.client
        .send_to_address(&address, Amount::from_sat(200_000))
        .expect("funds");
    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");
    core.sync_now(alice).await.expect("syncs");

    let elsewhere = core.next_address(mallory).await.expect("address").address;
    let quote = core
        .quote_send(
            alice,
            SendRequest {
                target: core.parse_payment(&elsewhere.to_string()).expect("parses"),
                raw: elsewhere.to_string(),
                amount: SendAmount::Exact(Amount::from_sat(50_000)),
                fee_rate: FeeRate::from_sat_per_vb(2).expect("valid"),
            },
        )
        .await
        .expect("quotes");

    // Mallory has the id — from a leaked callback, say — and it is useless.
    assert!(matches!(
        core.confirm_send(mallory, quote.id, Auth::Session).await,
        Err(wallet_core::CoreError::QuoteExpired)
    ));

    // And Alice's quote survived the attempt.
    assert!(
        core.confirm_send(alice, quote.id, Auth::Session)
            .await
            .is_ok()
    );

    core.shutdown().await;
}

/// §4: `/mine` exists on regtest and nowhere else — core's decision, not the
/// front end's.
#[tokio::test(flavor = "multi_thread")]
async fn mining_is_available_on_regtest() {
    let (_node, cfg, _dir) = harness();
    let core = WalletService::new(cfg).await.expect("service starts");

    let before = core.status().await.expect("status").tip_height;
    let hashes = core.mine(3, None).await.expect("mines");
    assert_eq!(hashes.len(), 3);

    let after = core.status().await.expect("status").tip_height;
    assert_eq!(after, before + 3);

    core.shutdown().await;
}

/// §5: a locked wallet cannot sign, and the PIN is what opens it.
#[tokio::test(flavor = "multi_thread")]
async fn a_locked_wallet_refuses_to_spend_until_the_pin_arrives() {
    let (node, cfg, _dir) = harness();
    let core = WalletService::new(cfg).await.expect("service starts");
    let alice = UserId::new();
    let pin = Pin::new("864213");

    core.create_wallet(alice, &pin).await.expect("creates");
    let address = core.next_address(alice).await.expect("address").address;

    let miner = node.client.new_address().expect("node address");
    node.client.generate_to_address(101, &miner).expect("mines");
    node.client
        .send_to_address(&address, Amount::from_sat(300_000))
        .expect("funds");
    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");
    core.sync_now(alice).await.expect("syncs");

    let quote = core
        .quote_send(
            alice,
            SendRequest {
                target: core.parse_payment(&address.to_string()).expect("parses"),
                raw: address.to_string(),
                amount: SendAmount::Exact(Amount::from_sat(50_000)),
                fee_rate: FeeRate::from_sat_per_vb(2).expect("valid"),
            },
        )
        .await
        .expect("quotes");

    // Quoting needs no PIN — it is watch-only work. Signing does.
    core.lock(alice);
    assert!(core.session(alice).is_none());

    assert!(matches!(
        core.confirm_send(alice, quote.id, Auth::Session).await,
        Err(wallet_core::CoreError::Locked)
    ));

    core.shutdown().await;
}

/// §5: a wrong PIN counts against the lockout, and the count is core's.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_pin_is_counted_and_the_right_one_clears_it() {
    let (_node, cfg, _dir) = harness();
    let core = WalletService::new(cfg).await.expect("service starts");
    let alice = UserId::new();

    core.create_wallet(alice, &Pin::new("864213"))
        .await
        .expect("creates");

    match core.unlock(alice, &Pin::new("000000")).await {
        Err(wallet_core::CoreError::WrongPin { remaining }) => assert_eq!(remaining, 4),
        other => panic!("expected WrongPin, got {other:?}"),
    }

    core.unlock(alice, &Pin::new("864213"))
        .await
        .expect("the right PIN opens it");

    match core.unlock(alice, &Pin::new("000000")).await {
        Err(wallet_core::CoreError::WrongPin { remaining }) => {
            assert_eq!(remaining, 4, "a success resets the counter")
        }
        other => panic!("expected WrongPin, got {other:?}"),
    }

    core.shutdown().await;
}

/// A front end that does not hold a session between commands — `wallet-cli`,
/// where every command is a new process — must still be able to read.
///
/// Regression: `create_wallet` used to seal the vault and leave the BDK wallet
/// file to be built lazily from the in-memory session, so the file was never
/// created at all for such a front end and every read returned `Locked`. Found
/// by running the CLI against a live node, which is why it is worth having a
/// test that does not keep the service alive across the two halves.
#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_is_readable_by_a_process_that_never_held_the_session() {
    let (_node, cfg, _dir) = harness();
    let alice = UserId::new();

    // First "process": create, then drop the service entirely.
    {
        let core = WalletService::new(cfg.clone()).await.expect("starts");
        core.create_wallet(alice, &Pin::new("864213"))
            .await
            .expect("creates");
        core.shutdown().await;
    }

    // Second "process": a fresh service with no session anywhere.
    let core = WalletService::new(cfg).await.expect("starts");
    assert!(
        core.session(alice).is_none(),
        "no session survived the restart"
    );

    // These are watch-only reads and must not need the PIN (§5).
    let address = core
        .next_address(alice)
        .await
        .expect("an address without a session");
    assert!(address.address.to_string().starts_with("bcrt1"));

    let balance = core
        .balance(alice)
        .await
        .expect("a balance without a session");
    assert_eq!(balance.confirmed, Amount::ZERO);

    core.history(alice, Page::new(0))
        .await
        .expect("history without a session");

    core.shutdown().await;
}

/// `/mine` with no address must work on a node that has several wallets
/// loaded, as Polar's does.
///
/// Regression: core called a bare `getnewaddress`, and Core refuses that with
/// -19 ("Multiple wallets are loaded…") rather than picking one. The shell
/// helper had the same bug and was fixed without the Rust being fixed too,
/// which is how it reached a user.
#[tokio::test(flavor = "multi_thread")]
async fn mining_without_an_address_works_with_several_node_wallets_loaded() {
    let (node, cfg, _dir) = harness();

    // Load a second wallet, so a bare wallet RPC becomes ambiguous.
    node.client
        .create_wallet("second")
        .expect("a second wallet loads");

    let core = WalletService::new(cfg).await.expect("service starts");
    let before = core.status().await.expect("status").tip_height;

    let hashes = core
        .mine(2, None)
        .await
        .expect("mining must not depend on which node wallet is default");
    assert_eq!(hashes.len(), 2);
    assert_eq!(core.status().await.expect("status").tip_height, before + 2);

    core.shutdown().await;
}

/// Mining to a supplied address touches no node wallet at all, which is the
/// path the bot takes.
#[tokio::test(flavor = "multi_thread")]
async fn mining_to_a_given_address_credits_that_wallet() {
    let (node, cfg, _dir) = harness();
    node.client
        .create_wallet("another")
        .expect("a second wallet loads");

    let core = WalletService::new(cfg).await.expect("service starts");
    let alice = UserId::new();
    core.create_wallet(alice, &Pin::new("864213"))
        .await
        .expect("creates");

    let address = core.next_address(alice).await.expect("address").address;
    core.mine(101, Some(address)).await.expect("mines to alice");
    core.sync_now(alice).await.expect("syncs");

    // A coinbase needs 100 confirmations, so the first reward is now spendable
    // and the rest are not. Either way the money is hers, not the node's.
    let balance = core.balance(alice).await.expect("balance");
    assert!(
        balance.total > Amount::ZERO,
        "mining to her address credited her wallet"
    );
    assert!(
        balance.immature > Amount::ZERO,
        "recent coinbase outputs are immature, which the UI has to say"
    );

    core.shutdown().await;
}

/// `/bumpfee` on a confirmed transaction is an ordinary outcome, not a fault.
///
/// Regression: it came back as a generic wallet error, which the bot rendered
/// as "Something went wrong on this server" — wrong, and leaving the user with
/// nothing to do about it. Mining a block between the send and the bump is
/// exactly how a user meets this.
#[tokio::test(flavor = "multi_thread")]
async fn bumping_a_confirmed_transaction_says_so() {
    use wallet_core::error::FeeBumpRefusal;

    let (node, cfg, _dir) = harness();
    let core = WalletService::new(cfg).await.expect("service starts");
    let alice = UserId::new();
    let bob = UserId::new();
    let pin = Pin::new("864213");

    core.create_wallet(alice, &pin).await.expect("creates");
    core.create_wallet(bob, &pin).await.expect("creates");

    let address = core.next_address(alice).await.expect("address").address;
    let miner = node.client.new_address().expect("node address");
    node.client.generate_to_address(101, &miner).expect("mines");
    node.client
        .send_to_address(&address, Amount::from_sat(300_000))
        .expect("funds");
    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");
    core.sync_now(alice).await.expect("syncs");

    let destination = core.next_address(bob).await.expect("address").address;
    let quote = core
        .quote_send(
            alice,
            SendRequest {
                target: core
                    .parse_payment(&destination.to_string())
                    .expect("parses"),
                raw: destination.to_string(),
                amount: SendAmount::Exact(Amount::from_sat(50_000)),
                fee_rate: FeeRate::from_sat_per_vb(1).expect("valid"),
            },
        )
        .await
        .expect("quotes");

    let sent = core
        .confirm_send(alice, quote.id, Auth::Session)
        .await
        .expect("broadcasts");

    // While it is unconfirmed, a bump is possible — BDK enables RBF by default.
    core.bump_fee(
        alice,
        sent.txid,
        FeeRate::from_sat_per_vb(5).expect("valid"),
    )
    .await
    .expect("an unconfirmed, replaceable transaction can be bumped");

    // Now mine it, which is what the user did.
    node.client
        .generate_to_address(1, &miner)
        .expect("confirms");
    core.sync_now(alice).await.expect("syncs");

    match core
        .bump_fee(
            alice,
            sent.txid,
            FeeRate::from_sat_per_vb(10).expect("valid"),
        )
        .await
    {
        Err(wallet_core::CoreError::CannotBumpFee {
            reason: FeeBumpRefusal::AlreadyConfirmed,
        }) => {}
        other => panic!("expected AlreadyConfirmed, got {other:?}"),
    }

    core.shutdown().await;
}

/// A txid this wallet has never seen is its own answer, not a server fault.
#[tokio::test(flavor = "multi_thread")]
async fn bumping_an_unknown_transaction_says_so() {
    use bdk_wallet::bitcoin::Txid;
    use wallet_core::error::FeeBumpRefusal;

    let (_node, cfg, _dir) = harness();
    let core = WalletService::new(cfg).await.expect("service starts");
    let alice = UserId::new();
    core.create_wallet(alice, &Pin::new("864213"))
        .await
        .expect("creates");

    let stranger = Txid::from_raw_hash(bdk_wallet::bitcoin::hashes::Hash::from_byte_array(
        [9u8; 32],
    ));

    match core
        .bump_fee(alice, stranger, FeeRate::from_sat_per_vb(5).expect("valid"))
        .await
    {
        Err(wallet_core::CoreError::CannotBumpFee {
            reason: FeeBumpRefusal::NotFound,
        }) => {}
        other => panic!("expected NotFound, got {other:?}"),
    }

    core.shutdown().await;
}
