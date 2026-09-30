//! The seed vault (PLAN.md §5).
//!
//! `seed_ct = XChaCha20Poly1305(key = Argon2id(PIN, salt, m=64MiB, t=3), nonce,
//! mnemonic)`. A 6–8 digit PIN is a tiny search space, so the only thing
//! standing between a stolen database and a stolen seed is how expensive one
//! guess is: 64 MiB and three passes per attempt is the whole defence, together
//! with the lockout counted in `storage.rs`.
//!
//! Plaintext never exists outside a `Zeroizing`, and no value here has a
//! `Debug` impl that could print one.

use crate::error::{CoreError, Result};
use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit},
};
use rand::Rng as _;
use zeroize::{Zeroize, Zeroizing};

/// §5: m=64 MiB, t=3. Argon2's `m_cost` is in KiB.
const M_COST_KIB: u32 = 64 * 1024;
const T_COST: u32 = 3;
const P_COST: u32 = 1;

const KEY_LEN: usize = 32;
pub const SALT_LEN: usize = 16;
pub const NONCE_LEN: usize = 24;

/// A sealed seed, exactly as it is stored: salt, nonce, ciphertext. Nothing
/// here is secret on its own, which is why it is the only shape that reaches
/// SQLite.
#[derive(Clone, PartialEq, Eq)]
pub struct Vault {
    pub salt: [u8; SALT_LEN],
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ciphertext is not a secret, but printing it invites someone to
        // paste it somewhere it becomes one alongside a PIN.
        f.debug_struct("Vault")
            .field("salt", &"<opaque>")
            .field("nonce", &"<opaque>")
            .field("ciphertext_len", &self.ciphertext.len())
            .finish()
    }
}

/// Seal a mnemonic under a PIN.
pub fn seal(pin: &str, plaintext: &Zeroizing<String>) -> Result<Vault> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut salt);
    rand::rng().fill_bytes(&mut nonce);

    let key = derive_key(pin, &salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
        .map_err(|_| CoreError::Crypto("key length"))?;

    let ciphertext = cipher
        .encrypt(&XNonce::from(nonce), plaintext.as_bytes())
        .map_err(|_| CoreError::Crypto("encryption failed"))?;

    Ok(Vault {
        salt,
        nonce,
        ciphertext,
    })
}

/// Open a vault with a PIN.
///
/// A wrong PIN is indistinguishable from a corrupt vault here, and deliberately
/// so: both are `Crypto("decryption failed")`, and it is `storage.rs` — which
/// knows whose vault this is — that turns a failure into `WrongPin` and counts
/// it towards the lockout.
pub fn open(pin: &str, vault: &Vault) -> Result<Zeroizing<String>> {
    let key = derive_key(pin, &vault.salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(key.as_slice())
        .map_err(|_| CoreError::Crypto("key length"))?;

    let mut plaintext = cipher
        .decrypt(&XNonce::from(vault.nonce), vault.ciphertext.as_ref())
        .map_err(|_| CoreError::Crypto("decryption failed"))?;

    let recovered = String::from_utf8(plaintext.clone())
        .map_err(|_| CoreError::Crypto("vault does not hold text"))?;
    plaintext.zeroize();

    Ok(Zeroizing::new(recovered))
}

fn derive_key(pin: &str, salt: &[u8; SALT_LEN]) -> Result<Zeroizing<[u8; KEY_LEN]>> {
    let params = Params::new(M_COST_KIB, T_COST, P_COST, Some(KEY_LEN))
        .map_err(|_| CoreError::Crypto("argon2 parameters"))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(pin.as_bytes(), salt, key.as_mut_slice())
        .map_err(|_| CoreError::Crypto("key derivation failed"))?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    #[test]
    fn a_sealed_vault_opens_with_the_right_pin() {
        let secret = Zeroizing::new(MNEMONIC.to_string());
        let vault = seal("864213", &secret).expect("seals");
        let recovered = open("864213", &vault).expect("opens");
        assert_eq!(*recovered, *secret);
    }

    #[test]
    fn a_wrong_pin_fails_and_says_nothing_about_why() {
        let vault = seal("864213", &Zeroizing::new(MNEMONIC.to_string())).expect("seals");
        let err = open("864214", &vault).expect_err("a wrong PIN must not open the vault");
        assert!(matches!(err, CoreError::Crypto(_)));
        // The message must not hint at how close the guess was.
        assert!(!err.to_string().contains("864214"));
    }

    #[test]
    fn two_seals_of_the_same_secret_differ() {
        // A shared salt or nonce would let an attacker with two vaults learn
        // that two users chose the same PIN.
        let secret = Zeroizing::new(MNEMONIC.to_string());
        let a = seal("864213", &secret).expect("seals");
        let b = seal("864213", &secret).expect("seals");
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn a_tampered_ciphertext_is_rejected_rather_than_decrypted() {
        let mut vault = seal("864213", &Zeroizing::new(MNEMONIC.to_string())).expect("seals");
        if let Some(byte) = vault.ciphertext.first_mut() {
            *byte ^= 0xff;
        }
        assert!(open("864213", &vault).is_err(), "AEAD must catch tampering");
    }

    #[test]
    fn the_vault_never_prints_its_contents() {
        let vault = seal("864213", &Zeroizing::new(MNEMONIC.to_string())).expect("seals");
        let rendered = format!("{vault:?}");
        assert!(rendered.contains("<opaque>"));
        assert!(!rendered.contains(&format!("{:?}", vault.salt)));
    }
}
