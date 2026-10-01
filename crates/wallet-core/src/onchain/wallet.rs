//! The per-user BDK wallet (PLAN.md §6; MVP rows 4–7, 10–11).
//!
//! One SQLite file per user under `data/{network}/wallets/`, persisted with the
//! **public** descriptors, so everything in this module works without the PIN
//! (§5). Signing attaches a transient private descriptor and is Step 5's job.
//!
//! Loading is deliberately explicit rather than cached: a wallet is opened,
//! read, and dropped. The shared thing is the chain data, not the handles.

use crate::service::types::SendAmount;
use crate::{
    error::{CoreError, Result},
    keys,
    service::types::{
        AddressInfo, BalanceView, Page, Paged, TxDetail, TxDirection, TxStatus, TxSummary,
    },
};
use bdk_wallet::{
    KeychainKind, PersistedWallet, Wallet,
    bitcoin::{Address, Amount, Network, Txid},
    chain::ChainPosition,
    keys::bip39::Mnemonic,
    rusqlite::Connection,
};
use bdk_wallet::{
    SignOptions,
    bitcoin::{FeeRate, Psbt},
    descriptor::IntoWalletDescriptor,
    signer::SignersContainer,
};

use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How far past the last revealed index BDK watches. A payment to an address
/// the user asked for but never used still has to be noticed, and on mainnet a
/// missed one is invisible until a rescan.
const LOOKAHEAD: u32 = 50;

/// An open wallet and the connection it persists to. They travel together
/// because BDK's `persist` needs both, and separating them is how a change ends
/// up staged and never written.
pub struct OpenWallet {
    pub wallet: PersistedWallet<Connection>,
    pub db: Connection,
    network: Network,
}

impl OpenWallet {
    /// Create the persisted wallet for a user from their mnemonic (§6).
    pub fn create(path: &Path, mnemonic: &Mnemonic, network: Network) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::Storage(e.to_string()))?;
        }
        let descriptors = keys::public_descriptors(mnemonic, network)?;
        let mut db = Connection::open(path).map_err(|e| CoreError::Storage(e.to_string()))?;

        let wallet = Wallet::create(descriptors.external, descriptors.internal)
            .network(network)
            .lookahead(LOOKAHEAD)
            .create_wallet(&mut db)
            .map_err(|e| CoreError::Wallet(e.to_string()))?;

        Ok(OpenWallet {
            wallet,
            db,
            network,
        })
    }

    /// Load an existing wallet. Watch-only: no key material is involved, which
    /// is why `/balance` and `/receive` never ask for a PIN (§5).
    pub fn load(path: &Path, network: Network) -> Result<Self> {
        if !path.exists() {
            return Err(CoreError::NoWallet);
        }
        let mut db = Connection::open(path).map_err(|e| CoreError::Storage(e.to_string()))?;

        let wallet = Wallet::load()
            .check_network(network)
            .lookahead(LOOKAHEAD)
            .load_wallet(&mut db)
            .map_err(|e| CoreError::Wallet(e.to_string()))?
            .ok_or(CoreError::NoWallet)?;

        Ok(OpenWallet {
            wallet,
            db,
            network,
        })
    }

    /// Load if it exists, create from the mnemonic if it does not.
    pub fn load_or_create(path: &Path, mnemonic: &Mnemonic, network: Network) -> Result<Self> {
        if path.exists() {
            Self::load(path, network)
        } else {
            Self::create(path, mnemonic, network)
        }
    }

    /// Flush staged changes. Every mutating call below ends here, because a
    /// revealed address that was not persisted is an address the user may be
    /// paid at and we will not recognise (MVP 11).
    pub fn flush(&mut self) -> Result<()> {
        self.wallet
            .persist(&mut self.db)
            .map_err(|e| CoreError::Storage(e.to_string()))?;
        Ok(())
    }

    /// MVP 4. The next address that has not been paid to yet (§6).
    pub fn next_address(&mut self) -> Result<AddressInfo> {
        let info = self.wallet.next_unused_address(KeychainKind::External);
        self.flush()?;
        Ok(self.describe_address(info.index, info.address.clone(), true))
    }

    /// MVP 4. Revealed addresses with used/unused status, newest first.
    pub fn addresses(&self, page: Page) -> Result<Paged<AddressInfo>> {
        let index = self.wallet.spk_index();
        let last = index
            .last_revealed_index(KeychainKind::External)
            .unwrap_or(0);

        let all: Vec<AddressInfo> = (0..=last)
            .rev()
            .map(|i| {
                let address = self.wallet.peek_address(KeychainKind::External, i).address;
                self.describe_address(i, address, false)
            })
            .collect();

        let total = all.len();
        let items = all
            .into_iter()
            .skip(page.offset())
            .take(page.size as usize)
            .collect();

        Ok(Paged { items, page, total })
    }

    /// MVP 6. `wallet.balance()` flattened, with the honest note about what is
    /// invisible on this backend (§4b, §6).
    pub fn balance(&self, unconfirmed_incoming_visible: bool) -> BalanceView {
        let b = self.wallet.balance();
        BalanceView {
            confirmed: b.confirmed,
            trusted_pending: b.trusted_pending,
            untrusted_pending: b.untrusted_pending,
            immature: b.immature,
            total: b.total(),
            unconfirmed_incoming_visible,
        }
    }

    /// MVP 7. History, newest first (§6).
    pub fn history(&self, tip: u32, page: Page) -> Result<Paged<TxSummary>> {
        let mut all: Vec<TxSummary> = self
            .wallet
            .transactions()
            .map(|tx| self.summarise(&tx, tip))
            .collect();

        // Newest first: confirmed by descending height, unconfirmed above all
        // of them, because that is the order a user scans for "did it arrive".
        all.sort_by_key(|tx| std::cmp::Reverse(rank(&tx.status)));

        let total = all.len();
        let items = all
            .into_iter()
            .skip(page.offset())
            .take(page.size as usize)
            .collect();

        Ok(Paged { items, page, total })
    }

    /// MVP 10. One transaction in detail (§6).
    ///
    /// Confirmations come from the wallet's own `ChainPosition`, not from
    /// `getrawtransaction`, so nothing depends on `txindex` at the shared node.
    pub fn tx(&self, txid: Txid, tip: u32) -> Result<TxDetail> {
        let tx = self
            .wallet
            .transactions()
            .find(|t| t.tx_node.txid == txid)
            .ok_or(CoreError::NoWallet)?;

        let summary = self.summarise(&tx, tip);
        let raw = tx.tx_node.tx.as_ref();
        let vsize = raw.vsize() as u64;
        // sat/vB expressed in BDK's sat/kwu: 1 vB is 4 wu, so sat/vB * 250
        // is sat/kwu.
        let fee_rate = summary.fee.map(|f| {
            bdk_wallet::bitcoin::FeeRate::from_sat_per_kwu(
                f.to_sat().saturating_mul(250) / vsize.max(1),
            )
        });

        Ok(TxDetail {
            summary,
            fee_rate,
            inputs: raw.input.len(),
            outputs: raw.output.len(),
            vsize,
        })
    }

    pub fn is_mine(&self, address: &Address) -> bool {
        self.wallet.is_mine(address.script_pubkey())
    }

    pub fn network(&self) -> Network {
        self.network
    }

    fn describe_address(
        &self,
        index: u32,
        address: Address,
        freshly_revealed: bool,
    ) -> AddressInfo {
        let spk = address.script_pubkey();
        let used = !freshly_revealed
            && self
                .wallet
                .spk_index()
                .is_used(KeychainKind::External, index);

        // What this address has ever received, which is what a user means by
        // "did that one get paid".
        let received: Amount = self
            .wallet
            .transactions()
            .flat_map(|tx| tx.tx_node.tx.output.clone())
            .filter(|out| out.script_pubkey == spk)
            .map(|out| out.value)
            .sum();

        AddressInfo {
            bip21: format!("bitcoin:{address}"),
            address,
            index,
            used,
            received,
        }
    }

    fn summarise(&self, tx: &bdk_wallet::WalletTx<'_>, tip: u32) -> TxSummary {
        let (sent, received) = self.wallet.sent_and_received(&tx.tx_node.tx);
        let fee = self.wallet.calculate_fee(&tx.tx_node.tx).ok();

        let direction = if sent > Amount::ZERO && received > Amount::ZERO {
            // Change coming back to us: outgoing unless nothing left at all.
            if received >= sent {
                TxDirection::Internal
            } else {
                TxDirection::Outgoing
            }
        } else if sent > Amount::ZERO {
            TxDirection::Outgoing
        } else {
            TxDirection::Incoming
        };

        // The number a user cares about: what this transaction did to them.
        let amount = match direction {
            TxDirection::Incoming => received,
            TxDirection::Internal => received.checked_sub(sent).unwrap_or(Amount::ZERO),
            TxDirection::Outgoing => sent
                .checked_sub(received)
                .unwrap_or(Amount::ZERO)
                .checked_sub(fee.unwrap_or(Amount::ZERO))
                .unwrap_or(Amount::ZERO),
        };

        let (status, timestamp) = match &tx.chain_position {
            ChainPosition::Confirmed { anchor, .. } => {
                let height = anchor.block_id.height;
                (
                    TxStatus::Confirmed {
                        height,
                        confirmations: tip.saturating_sub(height).saturating_add(1),
                    },
                    Some(UNIX_EPOCH + Duration::from_secs(anchor.confirmation_time)),
                )
            }
            ChainPosition::Unconfirmed { first_seen, .. } => (
                TxStatus::Unconfirmed,
                first_seen.map(|s| UNIX_EPOCH + Duration::from_secs(s)),
            ),
        };

        TxSummary {
            txid: tx.tx_node.txid,
            direction,
            amount,
            fee,
            status,
            timestamp,
        }
    }
}

/// Sort key: unconfirmed above every confirmed transaction, then by height.
fn rank(status: &TxStatus) -> (u8, u32) {
    match status {
        TxStatus::Unconfirmed => (1, u32::MAX),
        TxStatus::Confirmed { height, .. } => (0, *height),
    }
}

/// The wall-clock helper the summariser needs; kept here so tests can reason
/// about it without a system clock.
pub fn now() -> SystemTime {
    SystemTime::now()
}

// ------------------------------------------------------------------- spending
// MVP 8 and 9 (PLAN.md §6 steps 3–6). The two halves are deliberately apart:
// `draft` is pure and needs no PIN, `sign` needs the seed and nothing else.

/// What a drafted payment costs, before anyone has agreed to it.
pub struct Draft {
    pub psbt: Psbt,
    pub amount: Amount,
    pub fee: Amount,
    pub change: Amount,
}

impl OpenWallet {
    /// Build and price a payment (§6 step 3).
    ///
    /// Default BnB coin selection, RBF enabled (the default since BDK 1.x) and
    /// `drain_wallet` for `max`. Nothing is signed and nothing is persisted:
    /// a quote the user abandons must leave no trace.
    pub fn draft(
        &mut self,
        recipient: &Address,
        amount: SendAmount,
        fee_rate: FeeRate,
    ) -> Result<Draft> {
        let script = recipient.script_pubkey();

        let psbt = {
            let mut builder = self.wallet.build_tx();
            builder.fee_rate(fee_rate);

            match amount {
                SendAmount::Exact(value) => {
                    builder.add_recipient(script.clone(), value);
                }
                // `max`: every coin goes to the recipient and there is no change.
                SendAmount::Max => {
                    builder.drain_wallet();
                    builder.drain_to(script.clone());
                }
            }

            builder.finish().map_err(map_build_error)?
        };

        let fee = psbt.fee().map_err(|e| CoreError::Wallet(e.to_string()))?;

        let unsigned = &psbt.unsigned_tx;
        let to_recipient: Amount = unsigned
            .output
            .iter()
            .filter(|o| o.script_pubkey == script)
            .map(|o| o.value)
            .sum();
        let change: Amount = unsigned
            .output
            .iter()
            .filter(|o| o.script_pubkey != script)
            .map(|o| o.value)
            .sum();

        Ok(Draft {
            psbt,
            amount: to_recipient,
            fee,
            change,
        })
    }

    /// Raise the fee on a stuck transaction (§6 stretch).
    ///
    /// This matters more on mainnet than the plan first assumed, because there
    /// is no fee estimator to get the first attempt right.
    pub fn draft_fee_bump(&mut self, txid: Txid, fee_rate: FeeRate) -> Result<Draft> {
        let psbt = {
            let mut builder = self
                .wallet
                .build_fee_bump(txid)
                .map_err(|e| CoreError::Wallet(e.to_string()))?;
            builder.fee_rate(fee_rate);
            builder.finish().map_err(map_build_error)?
        };

        let fee = psbt.fee().map_err(|e| CoreError::Wallet(e.to_string()))?;
        let change: Amount = psbt
            .unsigned_tx
            .output
            .iter()
            .filter(|o| self.wallet.is_mine(o.script_pubkey.clone()))
            .map(|o| o.value)
            .sum();
        let amount: Amount = psbt
            .unsigned_tx
            .output
            .iter()
            .filter(|o| !self.wallet.is_mine(o.script_pubkey.clone()))
            .map(|o| o.value)
            .sum();

        Ok(Draft {
            psbt,
            amount,
            fee,
            change,
        })
    }

    /// Sign a drafted PSBT with signers built from the seed (§5, §6 step 5).
    ///
    /// The keys are derived, used and dropped inside this call. BDK 3.x is
    /// explicit that the `Wallet` is no longer a key store, which suits the
    /// design here exactly: the persisted wallet stays watch-only and the
    /// signers are owned by this stack frame and nothing else.
    pub fn sign(&self, psbt: &mut Psbt, mnemonic: &Mnemonic) -> Result<()> {
        self.sign_with(psbt, mnemonic, SignOptions::default())
    }

    /// Sign a payjoin PSBT, where some inputs are the counterparty's (§7).
    ///
    /// `trust_witness_utxo` has to be on here, and it is worth being explicit
    /// about why rather than leaving a surprising flag in the code. BDK refuses
    /// by default to sign a segwit input that carries only `witness_utxo`,
    /// because without the parent transaction it cannot verify the amount it is
    /// committing to — the lever behind fee-inflation attacks. In a payjoin we
    /// *cannot* hold the other party's parent transactions: they contributed
    /// inputs from a wallet we know nothing about.
    ///
    /// What makes that acceptable is the typestate we are inside. By the time
    /// this is called the payjoin crate has run `check_broadcast_suitability`,
    /// `identify_receiver_outputs`, `commit_outputs` and `apply_fee_range`, so
    /// our own outputs and the fee range are already verified against values we
    /// chose. We trust the witness UTXOs of inputs we are not paying for.
    pub fn sign_payjoin(&self, psbt: &mut Psbt, mnemonic: &Mnemonic) -> Result<()> {
        self.sign_with(
            psbt,
            mnemonic,
            SignOptions {
                trust_witness_utxo: true,
                ..SignOptions::default()
            },
        )
    }

    fn sign_with(&self, psbt: &mut Psbt, mnemonic: &Mnemonic, options: SignOptions) -> Result<()> {
        let secret = keys::private_descriptors(mnemonic, self.network)?;
        let secp = self.wallet.secp_ctx();

        let container = |descriptor: &str| -> Result<SignersContainer> {
            let (parsed, keymap) = descriptor
                .to_string()
                .into_wallet_descriptor(secp, self.network.into())
                .map_err(|e| CoreError::Wallet(e.to_string()))?;
            Ok(SignersContainer::build(keymap, &parsed, secp))
        };

        let external = container(&secret.external)?;
        let internal = container(&secret.internal)?;

        let finalized = self
            .wallet
            .sign_with_signers(psbt, &[&external, &internal], options)
            .map_err(|e| CoreError::Wallet(e.to_string()))?;

        // A payjoin PSBT is *expected* to come back unfinalised: the other
        // party's inputs are not ours to complete. Only our own transactions
        // must be finalised here, and `sign` is the path those take.
        if !finalized {
            return Err(CoreError::Wallet(
                "the transaction could not be fully signed".into(),
            ));
        }
        Ok(())
    }

    /// Sign our inputs in a shared PSBT without requiring the whole thing to
    /// finalise — the other party still has theirs to add (§7).
    pub fn sign_payjoin_partial(&self, psbt: &mut Psbt, mnemonic: &Mnemonic) -> Result<()> {
        let secret = keys::private_descriptors(mnemonic, self.network)?;
        let secp = self.wallet.secp_ctx();

        let container = |descriptor: &str| -> Result<SignersContainer> {
            let (parsed, keymap) = descriptor
                .to_string()
                .into_wallet_descriptor(secp, self.network.into())
                .map_err(|e| CoreError::Wallet(e.to_string()))?;
            Ok(SignersContainer::build(keymap, &parsed, secp))
        };

        let external = container(&secret.external)?;
        let internal = container(&secret.internal)?;

        // The return value is "is the whole PSBT final", which for a shared
        // transaction is not what we are asking, so it is deliberately ignored.
        self.wallet
            .sign_with_signers(
                psbt,
                &[&external, &internal],
                SignOptions {
                    trust_witness_utxo: true,
                    ..SignOptions::default()
                },
            )
            .map_err(|e| CoreError::Wallet(e.to_string()))?;

        Ok(())
    }

    /// Record a broadcast transaction as unconfirmed, so it shows as pending at
    /// once — on mainnet this is the *only* way an outgoing payment appears
    /// before a block, since there is no mempool to read (§4b, §6).
    pub fn record_broadcast(&mut self, tx: bdk_wallet::bitcoin::Transaction) -> Result<()> {
        let seen = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.wallet.apply_unconfirmed_txs([(tx, seen)]);
        self.flush()
    }
}

/// Coin selection failures deserve their own variant: "not enough money" is a
/// different conversation from "something went wrong" (§8.1).
fn map_build_error(e: bdk_wallet::error::CreateTxError) -> CoreError {
    use bdk_wallet::error::CreateTxError;
    match e {
        CreateTxError::CoinSelection(inner) => CoreError::InsufficientFunds {
            needed: Amount::from_sat(inner.needed.to_sat()),
            available: Amount::from_sat(inner.available.to_sat()),
        },
        other => CoreError::Wallet(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn mnemonic() -> Mnemonic {
        keys::parse(MNEMONIC).expect("the vector parses")
    }

    #[test]
    fn a_new_wallet_persists_and_reloads_with_the_same_first_address() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wallet.sqlite");

        let first = {
            let mut w = OpenWallet::create(&path, &mnemonic(), Network::Regtest).expect("creates");
            w.next_address().expect("reveals").address
        };

        // MVP 11: reload and the address index is where we left it.
        let mut reloaded = OpenWallet::load(&path, Network::Regtest).expect("reloads");
        let again = reloaded.next_address().expect("reveals").address;
        assert_eq!(first, again, "an unused address is offered again");
    }

    #[test]
    fn loading_a_wallet_that_does_not_exist_is_not_a_crash() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(matches!(
            OpenWallet::load(&dir.path().join("absent.sqlite"), Network::Regtest),
            Err(CoreError::NoWallet)
        ));
    }

    #[test]
    fn a_wallet_refuses_to_load_as_the_wrong_network() {
        // §4: mixing regtest and mainnet state is the one mistake that cannot
        // be undone, so it fails at the door.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("wallet.sqlite");
        OpenWallet::create(&path, &mnemonic(), Network::Regtest).expect("creates");

        assert!(OpenWallet::load(&path, Network::Bitcoin).is_err());
    }

    #[test]
    fn an_empty_wallet_reports_a_zero_balance_rather_than_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let w = OpenWallet::create(&dir.path().join("w.sqlite"), &mnemonic(), Network::Regtest)
            .expect("creates");

        let b = w.balance(true);
        assert_eq!(b.total, Amount::ZERO);
        assert_eq!(b.confirmed, Amount::ZERO);
        assert!(b.unconfirmed_incoming_visible);
    }

    /// §4b: on mainnet this flag is false, and the front end says so rather
    /// than showing a zero that looks like a lost payment.
    #[test]
    fn the_balance_carries_whether_unconfirmed_incoming_is_even_visible() {
        let dir = tempfile::tempdir().expect("temp dir");
        let w = OpenWallet::create(&dir.path().join("w.sqlite"), &mnemonic(), Network::Regtest)
            .expect("creates");
        assert!(!w.balance(false).unconfirmed_incoming_visible);
    }

    #[test]
    fn a_fresh_wallet_has_one_revealed_address_and_no_history() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut w = OpenWallet::create(&dir.path().join("w.sqlite"), &mnemonic(), Network::Regtest)
            .expect("creates");
        w.next_address().expect("reveals");

        let addresses = w.addresses(Page::new(0)).expect("lists");
        assert_eq!(addresses.total, 1);
        assert!(!addresses.items[0].used, "nothing has been paid to it");
        assert!(addresses.items[0].bip21.starts_with("bitcoin:bcrt1"));

        let history = w.history(0, Page::new(0)).expect("lists");
        assert_eq!(history.total, 0);
    }

    #[test]
    fn addresses_paginate_newest_first() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut w = OpenWallet::create(&dir.path().join("w.sqlite"), &mnemonic(), Network::Regtest)
            .expect("creates");

        // Reveal several without using them.
        for _ in 0..12 {
            let info = w.wallet.reveal_next_address(KeychainKind::External);
            let _ = info;
        }
        w.flush().expect("persists");

        let first = w.addresses(Page::new(0)).expect("lists");
        assert_eq!(first.items.len(), Page::DEFAULT_SIZE as usize);
        assert!(first.has_next());
        assert!(first.items[0].index > first.items[1].index, "newest first");

        let second = w.addresses(Page::new(1)).expect("lists");
        assert!(second.has_prev());
        assert_ne!(first.items[0].index, second.items[0].index);
    }

    #[test]
    fn unconfirmed_history_sorts_above_confirmed() {
        // The ordering rule, tested directly: it is the part a reader has to
        // trust when they cannot easily build a wallet with both.
        let mut rows = [
            TxStatus::Confirmed {
                height: 100,
                confirmations: 3,
            },
            TxStatus::Unconfirmed,
            TxStatus::Confirmed {
                height: 102,
                confirmations: 1,
            },
        ];
        rows.sort_by_key(|s| std::cmp::Reverse(rank(s)));
        assert_eq!(rows[0], TxStatus::Unconfirmed);
        assert_eq!(
            rows[1],
            TxStatus::Confirmed {
                height: 102,
                confirmations: 1
            }
        );
    }
}
