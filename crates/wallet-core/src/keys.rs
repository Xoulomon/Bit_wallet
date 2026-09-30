//! Mnemonics and BIP84 descriptors (PLAN.md §5, §6; MVP rows 1–3).
//!
//! Two descriptors per wallet: `wpkh(xprv/84'/{0|1}'/0'/0/*)` for receiving and
//! `/1/*` for change, coin type 0 on mainnet and 1 on regtest. The public pair
//! is what gets persisted, so syncing, balances, addresses and history all work
//! without the PIN; the private pair is built only for the moment of signing
//! and dropped immediately (§5).

use crate::error::{CoreError, Result};
use bdk_wallet::{
    KeychainKind,
    bitcoin::{Network, bip32::Xpriv},
    descriptor::template::Bip84,
    keys::{
        DerivableKey, ExtendedKey,
        bip39::{Language, Mnemonic, WordCount},
    },
    miniscript::Segwitv0,
    template::DescriptorTemplate,
};
use zeroize::Zeroizing;

/// The BIP39 word count this wallet issues (§5). Restoring accepts any valid
/// length; only generation is fixed.
const GENERATED_WORDS: WordCount = WordCount::Words12;

/// A descriptor pair. The private form never leaves the call that builds it.
pub struct Descriptors {
    pub external: String,
    pub internal: String,
}

/// Generate a fresh 12-word mnemonic (§5).
pub fn generate() -> Result<Zeroizing<String>> {
    let generated = <Mnemonic as bdk_wallet::keys::GeneratableKey<Segwitv0>>::generate((
        GENERATED_WORDS,
        Language::English,
    ))
    .map_err(|_| CoreError::Crypto("mnemonic generation failed"))?;
    Ok(Zeroizing::new(generated.to_string()))
}

/// Parse and validate a mnemonic, including its checksum (§5).
///
/// The words arrive as whatever the user typed, so they are normalised here:
/// case folded and re-joined on single spaces. Anything that is not a valid
/// BIP39 phrase is one error, `InvalidMnemonic` — being more specific would
/// tell an onlooker which word was wrong.
pub fn parse(words: &str) -> Result<Mnemonic> {
    let normalised = Zeroizing::new(
        words
            .split_whitespace()
            .map(str::to_lowercase)
            .collect::<Vec<_>>()
            .join(" "),
    );
    // `Mnemonic` is `ZeroizeOnDrop` (the bip39 `zeroize` feature, enabled in
    // Cargo.toml for exactly this reason), so it wipes itself and does not need
    // wrapping in `Zeroizing`.
    Mnemonic::parse_in(Language::English, normalised.as_str())
        .map_err(|_| CoreError::InvalidMnemonic)
}

/// The three word indices a new user must read back (§5, §3a).
///
/// Core's rule, not the front end's invention — which is what stops a second
/// front end from quietly skipping the backup check.
pub fn backup_challenge(word_count: usize) -> [u8; 3] {
    use rand::RngExt as _;
    let mut rng = rand::rng();
    let mut picks = [0u8; 3];
    let mut chosen = 0usize;
    while chosen < 3 {
        let candidate = rng.random_range(0..word_count) as u8;
        if !picks[..chosen].contains(&candidate) {
            picks[chosen] = candidate;
            chosen += 1;
        }
    }
    picks.sort_unstable();
    picks
}

/// Check the answers to a `backup_challenge`, case- and space-insensitively.
pub fn check_backup(mnemonic: &Mnemonic, challenge: [u8; 3], answers: &[String; 3]) -> bool {
    let words: Vec<&'static str> = mnemonic.words().collect();
    challenge.iter().zip(answers.iter()).all(|(idx, answer)| {
        words
            .get(*idx as usize)
            .is_some_and(|expected| expected.eq_ignore_ascii_case(answer.trim()))
    })
}

/// The master key for a mnemonic on this network.
/// `Xpriv` is not `Zeroize`, but the `SecretKey` inside it is erased by
/// `secp256k1` on drop, and this value never outlives the call that builds a
/// descriptor from it.
fn master(mnemonic: &Mnemonic, network: Network) -> Result<Xpriv> {
    let key: ExtendedKey<Segwitv0> = mnemonic
        .clone()
        .into_extended_key()
        .map_err(|_| CoreError::Crypto("deriving the master key failed"))?;
    key.into_xprv(network.into())
        .ok_or(CoreError::Crypto("no private key in the derived master"))
}

/// The **public** descriptor pair, which is what gets persisted (§5).
///
/// BDK never saves the keymap, so a wallet reloaded from SQLite is watch-only
/// until a signer is attached — the property the whole "no PIN to read your
/// balance" design rests on.
pub fn public_descriptors(mnemonic: &Mnemonic, network: Network) -> Result<Descriptors> {
    let (external, internal) = descriptor_pair(mnemonic, network, Secrecy::PublicOnly)?;
    Ok(Descriptors { external, internal })
}

/// The **private** descriptor pair, built only to sign and dropped at once (§5).
pub fn private_descriptors(
    mnemonic: &Mnemonic,
    network: Network,
) -> Result<Zeroizing<Descriptors>> {
    let (external, internal) = descriptor_pair(mnemonic, network, Secrecy::WithKeys)?;
    Ok(Zeroizing::new(Descriptors { external, internal }))
}

/// Whether a rendered descriptor carries its private keys.
///
/// `DescriptorTemplateOut` always hands back a *public* `Descriptor` plus a
/// separate `KeyMap`, so rendering the private form is an explicit act — which
/// is the right default: a descriptor that printed its xprv by accident is how
/// a seed ends up in a log file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Secrecy {
    PublicOnly,
    WithKeys,
}

fn descriptor_pair(
    mnemonic: &Mnemonic,
    network: Network,
    secrecy: Secrecy,
) -> Result<(String, String)> {
    let xprv = master(mnemonic, network)?;
    let build = |keychain: KeychainKind| -> Result<String> {
        let (descriptor, keymap, _networks) = Bip84(xprv, keychain)
            .build(network.into())
            .map_err(|e| CoreError::Wallet(e.to_string()))?;
        Ok(match secrecy {
            Secrecy::PublicOnly => descriptor.to_string(),
            Secrecy::WithKeys => descriptor.to_string_with_secret(&keymap),
        })
    };
    Ok((
        build(KeychainKind::External)?,
        build(KeychainKind::Internal)?,
    ))
}

impl zeroize::Zeroize for Descriptors {
    fn zeroize(&mut self) {
        self.external.zeroize();
        self.internal.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk_wallet::bitcoin::Address;
    use bdk_wallet::descriptor::IntoWalletDescriptor;
    use bdk_wallet::miniscript::descriptor::DescriptorPublicKey;
    use std::str::FromStr;

    /// The BIP84 specification's own test vector.
    const BIP84_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const BIP84_FIRST_RECEIVE: &str = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";
    const BIP84_FIRST_CHANGE: &str = "bc1q8c6fshw2dlwun7ekn9qwf37cu2rn755upcp6el";

    fn first_address(descriptor: &str, network: Network) -> String {
        let secp = bdk_wallet::bitcoin::secp256k1::Secp256k1::new();
        let (desc, _) = descriptor
            .to_string()
            .into_wallet_descriptor(&secp, network.into())
            .expect("the descriptor parses");
        desc.at_derivation_index(0)
            .expect("index 0 derives")
            .address(network)
            .expect("a wpkh descriptor always yields an address")
            .to_string()
    }

    #[test]
    fn a_generated_mnemonic_has_twelve_words_and_round_trips() {
        let generated = generate().expect("generates");
        assert_eq!(generated.split_whitespace().count(), 12);
        parse(&generated).expect("what we generate must parse");
    }

    #[test]
    fn two_generated_mnemonics_differ() {
        let a = generate().expect("generates");
        let b = generate().expect("generates");
        assert_ne!(*a, *b);
    }

    /// MVP 3, proven against the BIP84 vectors rather than against ourselves.
    #[test]
    fn the_first_addresses_match_the_bip84_test_vectors() {
        let mnemonic = parse(BIP84_MNEMONIC).expect("the vector parses");
        let descriptors =
            public_descriptors(&mnemonic, Network::Bitcoin).expect("descriptors build");

        assert_eq!(
            first_address(&descriptors.external, Network::Bitcoin),
            BIP84_FIRST_RECEIVE
        );
        assert_eq!(
            first_address(&descriptors.internal, Network::Bitcoin),
            BIP84_FIRST_CHANGE
        );
    }

    #[test]
    fn the_descriptors_use_the_right_coin_type_per_network() {
        let mnemonic = parse(BIP84_MNEMONIC).expect("parses");

        // The account path lives in the key origin, the chain in the suffix.
        let mainnet = public_descriptors(&mnemonic, Network::Bitcoin).expect("builds");
        assert!(
            mainnet.external.contains("/84'/0'/0']"),
            "{}",
            mainnet.external
        );
        assert!(mainnet.external.ends_with("/0/*)#") || mainnet.external.contains("/0/*)"));
        assert!(mainnet.internal.contains("/1/*)"), "{}", mainnet.internal);

        // §6: coin type 1 on regtest, so a regtest wallet can never derive a
        // key that also controls mainnet funds.
        let regtest = public_descriptors(&mnemonic, Network::Regtest).expect("builds");
        assert!(
            regtest.external.contains("/84'/1'/0']"),
            "{}",
            regtest.external
        );
    }

    /// §5: what is persisted is watch-only. A saved xprv would make "no PIN to
    /// read your balance" a synonym for "no PIN to spend".
    #[test]
    fn the_persisted_descriptors_carry_no_private_key() {
        let mnemonic = parse(BIP84_MNEMONIC).expect("parses");
        let public = public_descriptors(&mnemonic, Network::Bitcoin).expect("builds");
        assert!(!public.external.contains("xprv"));
        assert!(!public.internal.contains("xprv"));
        assert!(public.external.contains("xpub"));

        // And the signing pair does carry one, or signing could not work.
        let private = private_descriptors(&mnemonic, Network::Bitcoin).expect("builds");
        assert!(private.external.contains("xprv"));
    }

    #[test]
    fn the_external_and_change_chains_are_different() {
        let mnemonic = parse(BIP84_MNEMONIC).expect("parses");
        let d = public_descriptors(&mnemonic, Network::Bitcoin).expect("builds");
        assert_ne!(d.external, d.internal);
    }

    #[test]
    fn a_mnemonic_is_normalised_before_it_is_parsed() {
        // What a user types: stray case and doubled spaces.
        let messy = "  ABANDON abandon  abandon abandon abandon abandon \
                     abandon abandon abandon abandon abandon ABOUT ";
        let parsed = parse(messy).expect("normalisation handles user input");
        assert_eq!(parsed.to_string(), BIP84_MNEMONIC);
    }

    #[test]
    fn a_bad_checksum_is_rejected() {
        // Twelve valid words, wrong checksum.
        let wrong = "abandon abandon abandon abandon abandon abandon \
                     abandon abandon abandon abandon abandon abandon";
        assert!(matches!(parse(wrong), Err(CoreError::InvalidMnemonic)));
    }

    #[test]
    fn a_word_that_is_not_in_the_list_is_rejected() {
        let wrong = BIP84_MNEMONIC.replace("about", "zzzzzz");
        assert!(matches!(parse(&wrong), Err(CoreError::InvalidMnemonic)));
    }

    #[test]
    fn the_backup_challenge_picks_three_distinct_words_in_range() {
        for _ in 0..50 {
            let c = backup_challenge(12);
            assert!(c.iter().all(|i| *i < 12));
            assert_ne!(c[0], c[1]);
            assert_ne!(c[1], c[2]);
            assert_ne!(c[0], c[2]);
        }
    }

    #[test]
    fn the_backup_check_accepts_the_right_words_and_rejects_the_rest() {
        let mnemonic = parse(BIP84_MNEMONIC).expect("parses");
        let challenge = [0u8, 5, 11];
        let right = [
            "abandon".to_string(),
            "abandon".to_string(),
            "about".to_string(),
        ];
        assert!(check_backup(&mnemonic, challenge, &right));

        let wrong = [
            "abandon".to_string(),
            "abandon".to_string(),
            "abandon".to_string(),
        ];
        assert!(!check_backup(&mnemonic, challenge, &wrong));
    }

    #[test]
    fn the_backup_check_tolerates_case_and_surrounding_space() {
        let mnemonic = parse(BIP84_MNEMONIC).expect("parses");
        let answers = [
            " Abandon ".to_string(),
            "ABANDON".to_string(),
            "About".to_string(),
        ];
        assert!(check_backup(&mnemonic, [0, 5, 11], &answers));
    }

    #[test]
    fn descriptors_parse_as_ranged_public_descriptors() {
        let mnemonic = parse(BIP84_MNEMONIC).expect("parses");
        let d = public_descriptors(&mnemonic, Network::Bitcoin).expect("builds");
        let parsed =
            bdk_wallet::miniscript::Descriptor::<DescriptorPublicKey>::from_str(&d.external)
                .expect("a ranged descriptor");
        assert!(parsed.has_wildcard());

        // And it really is a native SegWit address, not a wrapped one (MVP 3).
        let addr = Address::from_str(&first_address(&d.external, Network::Bitcoin))
            .expect("parses")
            .assume_checked();
        assert!(addr.to_string().starts_with("bc1q"));
    }
}
