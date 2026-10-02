//! On-chain integration tests against a real bitcoind (PLAN.md §10).
//!
//! `corepc-node` spawns a throwaway regtest node, so these need neither Polar
//! nor BitRPC and run unchanged in CI. They exercise the path a reviewer most
//! wants proven: a wallet that is created, funded, synced, and still correct
//! after a restart.
//!
//! The bitcoind binary is downloaded by the `download` feature on first run.

use bdk_wallet::bitcoin::{Amount, Network};
use corepc_node::Node;
use std::{sync::Arc, time::Duration};
use wallet_core::{
    AppConfig, NetworkChoice,
    config::{BackendConfig, RegtestConfig},
    service::types::{Page, TxStatus, UserId},
    storage::Storage,
};
use zeroize::Zeroizing;

const MNEMONIC: &str =
    "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

/// A second phrase, so two wallets in one test are genuinely different.
const OTHER_MNEMONIC: &str =
    "legal winner thank year wave sausage worth useful legal winner thank yellow";

struct Harness {
    node: Node,
    cfg: AppConfig,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let node = Node::from_downloaded().expect("a regtest bitcoind starts");
    let dir = tempfile::tempdir().expect("temp dir");
    let auth = node.params.get_cookie_values().expect("cookie is readable");

    let (user, pass) = match auth {
        Some(values) => (values.user, values.password),
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
            fallback_fee: bdk_wallet::bitcoin::FeeRate::from_sat_per_vb(2).expect("valid"),
        }),
        data_dir: dir.path().to_path_buf(),
        session_idle_timeout: Duration::from_secs(600),
        max_send: None,
        fee_cache: Duration::from_secs(60),
        price_api: "https://mempool.space/api".into(),
    };

    Harness {
        node,
        cfg,
        _dir: dir,
    }
}

/// Create a user's wallet on disk the way the service does.
fn make_wallet(cfg: &AppConfig, user: UserId, words: &str) -> wallet_core::onchain::OpenWallet {
    let mnemonic = wallet_core::keys::parse(words).expect("the vector parses");
    wallet_core::onchain::OpenWallet::create(&cfg.wallet_db(&user), &mnemonic, Network::Regtest)
        .expect("wallet is created")
}

/// MVP 5 + 6: fund a wallet and see the balance after a sync.
#[tokio::test(flavor = "multi_thread")]
async fn a_funded_wallet_reports_its_balance_after_syncing() {
    let h = harness();
    let user = UserId::new();

    let address = {
        let mut w = make_wallet(&h.cfg, user, MNEMONIC);
        w.next_address().expect("reveals").address
    };

    // Coinbase outputs need 100 blocks to mature, so mine past that and then
    // pay ourselves from a matured one.
    let miner = h.node.client.new_address().expect("node address");
    h.node
        .client
        .generate_to_address(101, &miner)
        .expect("mines");
    h.node
        .client
        .send_to_address(&address, Amount::from_sat(150_000))
        .expect("funds the wallet");
    h.node
        .client
        .generate_to_address(1, &miner)
        .expect("confirms");

    wallet_core::onchain::sync::sync_now(&h.cfg, user)
        .await
        .expect("syncs");

    let w = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&user), Network::Regtest)
        .expect("loads");
    let balance = w.balance(true);

    assert_eq!(
        balance.confirmed,
        Amount::from_sat(150_000),
        "the confirmed balance is what was sent"
    );
    assert!(
        balance.unconfirmed_incoming_visible,
        "regtest has a mempool"
    );
}

/// MVP 11: the whole point of persistence — reload and nothing is lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_reloaded_wallet_keeps_its_balance_and_address_index() {
    let h = harness();
    let user = UserId::new();

    let (address, index) = {
        let mut w = make_wallet(&h.cfg, user, MNEMONIC);
        let info = w.next_address().expect("reveals");
        (info.address, info.index)
    };

    let miner = h.node.client.new_address().expect("node address");
    h.node
        .client
        .generate_to_address(101, &miner)
        .expect("mines");
    h.node
        .client
        .send_to_address(&address, Amount::from_sat(75_000))
        .expect("funds");
    h.node
        .client
        .generate_to_address(1, &miner)
        .expect("confirms");

    wallet_core::onchain::sync::sync_now(&h.cfg, user)
        .await
        .expect("syncs");

    // Drop every handle and come back from disk alone.
    let mut reloaded =
        wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&user), Network::Regtest)
            .expect("reloads");

    assert_eq!(reloaded.balance(true).confirmed, Amount::from_sat(75_000));

    // The funded address is now used, so the next one is a different address.
    let next = reloaded.next_address().expect("reveals");
    assert_ne!(next.index, index, "a used address is not offered again");

    let listed = reloaded.addresses(Page::new(0)).expect("lists");
    let funded = listed
        .items
        .iter()
        .find(|a| a.index == index)
        .expect("the funded address is still listed");
    assert!(funded.used, "the funded address is marked used");
    assert_eq!(funded.received, Amount::from_sat(75_000));
}

/// MVP 7 + 10: history and confirmation counting, read from the wallet's own
/// chain position rather than from `getrawtransaction` (§6).
#[tokio::test(flavor = "multi_thread")]
async fn confirmations_are_tracked_as_blocks_land() {
    let h = harness();
    let user = UserId::new();

    let address = {
        let mut w = make_wallet(&h.cfg, user, MNEMONIC);
        w.next_address().expect("reveals").address
    };

    let miner = h.node.client.new_address().expect("node address");
    h.node
        .client
        .generate_to_address(101, &miner)
        .expect("mines");
    h.node
        .client
        .send_to_address(&address, Amount::from_sat(40_000))
        .expect("funds");
    h.node
        .client
        .generate_to_address(1, &miner)
        .expect("confirms");

    let tip = wallet_core::onchain::sync::sync_now(&h.cfg, user)
        .await
        .expect("syncs");

    let w = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&user), Network::Regtest)
        .expect("loads");
    let history = w.history(tip, Page::new(0)).expect("lists");

    assert_eq!(history.total, 1, "one incoming transaction");
    let first = &history.items[0];
    match first.status {
        TxStatus::Confirmed { confirmations, .. } => {
            assert_eq!(confirmations, 1, "one block deep");
        }
        other => panic!("expected a confirmed transaction, got {other:?}"),
    }

    // Five more blocks, and the same transaction is six deep.
    h.node.client.generate_to_address(5, &miner).expect("mines");
    let tip = wallet_core::onchain::sync::sync_now(&h.cfg, user)
        .await
        .expect("syncs");

    let w = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&user), Network::Regtest)
        .expect("loads");
    let detail = w.tx(first.txid, tip).expect("the tx is known");
    match detail.summary.status {
        TxStatus::Confirmed { confirmations, .. } => assert_eq!(confirmations, 6),
        other => panic!("expected a confirmed transaction, got {other:?}"),
    }
    assert!(detail.vsize > 0);
}

/// §6: on regtest the mempool is visible, so a payment shows before it confirms.
#[tokio::test(flavor = "multi_thread")]
async fn an_unconfirmed_payment_is_visible_on_regtest() {
    let h = harness();
    let user = UserId::new();

    let address = {
        let mut w = make_wallet(&h.cfg, user, MNEMONIC);
        w.next_address().expect("reveals").address
    };

    let miner = h.node.client.new_address().expect("node address");
    h.node
        .client
        .generate_to_address(101, &miner)
        .expect("mines");
    h.node
        .client
        .send_to_address(&address, Amount::from_sat(30_000))
        .expect("funds");
    // Deliberately no block.

    wallet_core::onchain::sync::sync_now(&h.cfg, user)
        .await
        .expect("syncs");

    let w = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&user), Network::Regtest)
        .expect("loads");
    let balance = w.balance(true);

    assert_eq!(balance.confirmed, Amount::ZERO);
    assert_eq!(
        balance.untrusted_pending,
        Amount::from_sat(30_000),
        "an unconfirmed payment from outside shows as untrusted pending"
    );
    assert_eq!(balance.total, Amount::from_sat(30_000));
}

/// §4: two users are two wallets, and neither can see the other's coins.
#[tokio::test(flavor = "multi_thread")]
async fn two_users_hold_separate_wallets() {
    let h = harness();
    let alice = UserId::new();
    let bob = UserId::new();

    let alice_address = {
        let mut w = make_wallet(&h.cfg, alice, MNEMONIC);
        w.next_address().expect("reveals").address
    };
    let bob_address = {
        let mut w = make_wallet(&h.cfg, bob, OTHER_MNEMONIC);
        w.next_address().expect("reveals").address
    };
    assert_ne!(alice_address, bob_address);

    let miner = h.node.client.new_address().expect("node address");
    h.node
        .client
        .generate_to_address(101, &miner)
        .expect("mines");
    h.node
        .client
        .send_to_address(&alice_address, Amount::from_sat(60_000))
        .expect("funds alice");
    h.node
        .client
        .generate_to_address(1, &miner)
        .expect("confirms");

    wallet_core::onchain::sync::sync_now(&h.cfg, alice)
        .await
        .expect("syncs alice");
    wallet_core::onchain::sync::sync_now(&h.cfg, bob)
        .await
        .expect("syncs bob");

    let a = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&alice), Network::Regtest)
        .expect("loads");
    let b = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&bob), Network::Regtest)
        .expect("loads");

    assert_eq!(a.balance(true).confirmed, Amount::from_sat(60_000));
    assert_eq!(b.balance(true).confirmed, Amount::ZERO);
}

/// The sync task walks every user in storage, so a wallet nobody has opened
/// still catches up.
#[tokio::test(flavor = "multi_thread")]
async fn storage_lists_every_user_for_the_sync_task() {
    let storage = Storage::in_memory().expect("opens");
    let users: Vec<UserId> = (0..3).map(|_| UserId::new()).collect();

    for user in &users {
        let vault = wallet_core::crypto::seal("864213", &Zeroizing::new(MNEMONIC.to_string()))
            .expect("seals");
        storage
            .insert_wallet(*user, &vault, 0, None)
            .expect("inserts");
    }

    let listed = Arc::new(storage).all_users().expect("lists");
    assert_eq!(listed.len(), 3);
    for user in users {
        assert!(listed.contains(&user));
    }
}

// ------------------------------------------------------------------- spending
// MVP 8 and 9 (PLAN.md §10): a real transaction, built, signed and broadcast
// against a real node.

use wallet_core::service::types::SendAmount;

/// Fund a wallet and return its first address plus the miner's, so a test can
/// keep mining.
async fn funded(
    h: &Harness,
    user: UserId,
    words: &str,
    sats: u64,
) -> wallet_core::bitcoin::Address {
    let address = {
        let mut w = make_wallet(&h.cfg, user, words);
        w.next_address().expect("reveals").address
    };
    let miner = h.node.client.new_address().expect("node address");
    h.node
        .client
        .generate_to_address(101, &miner)
        .expect("mines");
    h.node
        .client
        .send_to_address(&address, Amount::from_sat(sats))
        .expect("funds");
    h.node
        .client
        .generate_to_address(1, &miner)
        .expect("confirms");
    wallet_core::onchain::sync::sync_now(&h.cfg, user)
        .await
        .expect("syncs");
    miner
}

/// MVP 8 + 9: build, sign and broadcast, then see it confirm.
#[tokio::test(flavor = "multi_thread")]
async fn a_payment_between_two_wallets_is_signed_broadcast_and_confirmed() {
    let h = harness();
    let alice = UserId::new();
    let bob = UserId::new();

    let miner = funded(&h, alice, MNEMONIC, 500_000).await;

    let bob_address = {
        let mut w = make_wallet(&h.cfg, bob, OTHER_MNEMONIC);
        w.next_address().expect("reveals").address
    };

    let rate = wallet_core::bitcoin::FeeRate::from_sat_per_vb(2).expect("valid");
    let mnemonic = wallet_core::keys::parse(MNEMONIC).expect("parses");

    let (txid, quoted_amount) = {
        let mut w =
            wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&alice), Network::Regtest)
                .expect("loads");

        let draft = w
            .draft(
                &bob_address,
                SendAmount::Exact(Amount::from_sat(120_000)),
                rate,
            )
            .expect("drafts");

        assert_eq!(draft.amount, Amount::from_sat(120_000));
        assert!(draft.fee > Amount::ZERO, "a real transaction pays a fee");
        assert!(draft.change > Amount::ZERO, "the rest comes back as change");

        let mut psbt = draft.psbt;
        w.sign(&mut psbt, &mnemonic).expect("signs");

        let tx = psbt.extract_tx().expect("the PSBT is final");
        let txid = h
            .node
            .client
            .send_raw_transaction(&tx)
            .expect("the node accepts it");

        w.record_broadcast(tx).expect("records it as pending");
        (txid.txid().expect("a txid"), draft.amount)
    };

    // Before a block, Alice sees it as pending; the money has left.
    {
        let w = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&alice), Network::Regtest)
            .expect("loads");
        let history = w.history(0, Page::new(0)).expect("lists");
        assert!(
            history.items.iter().any(|t| t.txid == txid),
            "the broadcast transaction shows at once"
        );
    }

    h.node
        .client
        .generate_to_address(1, &miner)
        .expect("confirms");

    let tip = wallet_core::onchain::sync::sync_now(&h.cfg, alice)
        .await
        .expect("syncs alice");
    wallet_core::onchain::sync::sync_now(&h.cfg, bob)
        .await
        .expect("syncs bob");

    // Bob has the money.
    let b = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&bob), Network::Regtest)
        .expect("loads");
    assert_eq!(b.balance(true).confirmed, quoted_amount);

    // And Alice's copy is confirmed, with the fee accounted for.
    let a = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&alice), Network::Regtest)
        .expect("loads");
    let detail = a.tx(txid, tip).expect("alice knows the transaction");
    match detail.summary.status {
        TxStatus::Confirmed { confirmations, .. } => assert_eq!(confirmations, 1),
        other => panic!("expected a confirmation, got {other:?}"),
    }
    assert!(
        detail.summary.fee.is_some(),
        "the fee is known to the sender"
    );
    assert!(
        a.balance(true).confirmed < Amount::from_sat(500_000 - 120_000),
        "the fee came out of Alice's balance too"
    );
}

/// `/send <addr> max` drains the wallet: everything goes, and no change is left.
#[tokio::test(flavor = "multi_thread")]
async fn a_max_payment_drains_the_wallet() {
    let h = harness();
    let alice = UserId::new();
    let bob = UserId::new();

    let miner = funded(&h, alice, MNEMONIC, 300_000).await;

    let bob_address = {
        let mut w = make_wallet(&h.cfg, bob, OTHER_MNEMONIC);
        w.next_address().expect("reveals").address
    };

    let rate = wallet_core::bitcoin::FeeRate::from_sat_per_vb(2).expect("valid");
    let mnemonic = wallet_core::keys::parse(MNEMONIC).expect("parses");

    {
        let mut w =
            wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&alice), Network::Regtest)
                .expect("loads");
        let draft = w
            .draft(&bob_address, SendAmount::Max, rate)
            .expect("drafts");

        assert_eq!(draft.change, Amount::ZERO, "max leaves no change");
        assert_eq!(
            draft.amount + draft.fee,
            Amount::from_sat(300_000),
            "everything is accounted for"
        );

        let mut psbt = draft.psbt;
        w.sign(&mut psbt, &mnemonic).expect("signs");
        let tx = psbt.extract_tx().expect("final");
        h.node
            .client
            .send_raw_transaction(&tx)
            .expect("the node accepts it");
        w.record_broadcast(tx).expect("records");
    }

    h.node
        .client
        .generate_to_address(1, &miner)
        .expect("confirms");
    wallet_core::onchain::sync::sync_now(&h.cfg, alice)
        .await
        .expect("syncs");

    let a = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&alice), Network::Regtest)
        .expect("loads");
    assert_eq!(
        a.balance(true).total,
        Amount::ZERO,
        "a drained wallet really is empty"
    );
}

/// Spending more than there is must be a typed error, not a panic and not a
/// transaction the node rejects later (§8.1).
#[tokio::test(flavor = "multi_thread")]
async fn spending_more_than_the_balance_is_refused_with_both_numbers() {
    let h = harness();
    let alice = UserId::new();
    let bob = UserId::new();

    funded(&h, alice, MNEMONIC, 50_000).await;

    let bob_address = {
        let mut w = make_wallet(&h.cfg, bob, OTHER_MNEMONIC);
        w.next_address().expect("reveals").address
    };

    let mut w = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&alice), Network::Regtest)
        .expect("loads");

    match w.draft(
        &bob_address,
        SendAmount::Exact(Amount::from_sat(10_000_000)),
        wallet_core::bitcoin::FeeRate::from_sat_per_vb(2).expect("valid"),
    ) {
        Err(wallet_core::CoreError::InsufficientFunds { available, .. }) => {
            assert!(available <= Amount::from_sat(50_000));
        }
        Err(other) => panic!("expected InsufficientFunds, got {other:?}"),
        Ok(_) => panic!("a payment larger than the balance must not draft"),
    }
}

/// A wallet signs only with its own seed: the wrong mnemonic cannot finalise.
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_seed_cannot_sign_this_wallets_transaction() {
    let h = harness();
    let alice = UserId::new();
    let bob = UserId::new();

    funded(&h, alice, MNEMONIC, 200_000).await;

    let bob_address = {
        let mut w = make_wallet(&h.cfg, bob, OTHER_MNEMONIC);
        w.next_address().expect("reveals").address
    };

    let mut w = wallet_core::onchain::OpenWallet::load(&h.cfg.wallet_db(&alice), Network::Regtest)
        .expect("loads");
    let draft = w
        .draft(
            &bob_address,
            SendAmount::Exact(Amount::from_sat(50_000)),
            wallet_core::bitcoin::FeeRate::from_sat_per_vb(2).expect("valid"),
        )
        .expect("drafts");

    let wrong = wallet_core::keys::parse(OTHER_MNEMONIC).expect("parses");
    let mut psbt = draft.psbt;
    assert!(
        w.sign(&mut psbt, &wrong).is_err(),
        "another seed must not finalise this wallet's inputs"
    );
}
