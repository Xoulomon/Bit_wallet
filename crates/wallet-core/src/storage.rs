//! `app.sqlite`: users, vaults, lockouts (PLAN.md §3, §5).
//!
//! One database per network, because `AppConfig::app_db` puts it under
//! `data/{regtest|bitcoin}/` — regtest and mainnet state can never mix (§4).
//!
//! The lockout counter lives here rather than in a front end, so every front
//! end shares one count: five wrong PINs through the CLI and five more through
//! Telegram is still five (§5).

use crate::{
    crypto::{NONCE_LEN, SALT_LEN, Vault},
    error::{CoreError, Result},
    service::types::UserId,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// §5: five failures, then a lockout that doubles each time.
const MAX_ATTEMPTS: u32 = 5;
const BASE_LOCKOUT: Duration = Duration::from_secs(60);
const MAX_LOCKOUT: Duration = Duration::from_secs(60 * 60 * 24);

pub struct Storage {
    db: Mutex<Connection>,
}

/// A user's wallet record, without its secret.
#[derive(Debug, Clone)]
pub struct WalletRecord {
    pub user: UserId,
    pub birthday: u32,
    pub created_at: SystemTime,
    pub backup_confirmed: bool,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::Storage(e.to_string()))?;
        }
        let db = Connection::open(path)?;
        let storage = Storage { db: Mutex::new(db) };
        storage.migrate()?;
        Ok(storage)
    }

    /// In-memory, for tests. Same schema, same migrations.
    pub fn in_memory() -> Result<Self> {
        let storage = Storage {
            db: Mutex::new(Connection::open_in_memory()?),
        };
        storage.migrate()?;
        Ok(storage)
    }

    /// Migrations are keyed on `user_version`, so opening an old database
    /// upgrades it rather than failing or, worse, silently ignoring columns.
    fn migrate(&self) -> Result<()> {
        let db = self.lock()?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

        let version: u32 = db
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);

        if version < 1 {
            db.execute_batch(
                "BEGIN;
                 CREATE TABLE wallets (
                     user_id          TEXT PRIMARY KEY,
                     salt             BLOB NOT NULL,
                     nonce            BLOB NOT NULL,
                     seed_ct          BLOB NOT NULL,
                     birthday_height  INTEGER NOT NULL,
                     created_at       INTEGER NOT NULL,
                     backup_confirmed INTEGER NOT NULL DEFAULT 0,
                     failed_attempts  INTEGER NOT NULL DEFAULT 0,
                     locked_until     INTEGER
                 );
                 CREATE TABLE labels (
                     user_id TEXT NOT NULL,
                     txid    TEXT NOT NULL,
                     label   TEXT NOT NULL,
                     PRIMARY KEY (user_id, txid)
                 );
                 PRAGMA user_version = 1;
                 COMMIT;",
            )?;
        }

        if version < 2 {
            // The backup challenge is core's rule (§3a), so core has to
            // remember which words it asked for — a front end that supplied
            // the challenge back could choose three it had just shown.
            db.execute_batch(
                "BEGIN;
                 ALTER TABLE wallets ADD COLUMN backup_challenge BLOB;
                 PRAGMA user_version = 2;
                 COMMIT;",
            )?;
        }

        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| CoreError::Storage("storage mutex poisoned".into()))
    }

    pub fn wallet_exists(&self, user: UserId) -> Result<bool> {
        let db = self.lock()?;
        let found: Option<u32> = db
            .query_row(
                "SELECT 1 FROM wallets WHERE user_id = ?1",
                params![user.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn record(&self, user: UserId) -> Result<WalletRecord> {
        let db = self.lock()?;
        db.query_row(
            "SELECT birthday_height, created_at, backup_confirmed
             FROM wallets WHERE user_id = ?1",
            params![user.to_string()],
            |r| {
                Ok(WalletRecord {
                    user,
                    birthday: r.get::<_, i64>(0)? as u32,
                    created_at: UNIX_EPOCH + Duration::from_secs(r.get::<_, i64>(1)? as u64),
                    backup_confirmed: r.get::<_, i64>(2)? != 0,
                })
            },
        )
        .optional()?
        .ok_or(CoreError::NoWallet)
    }

    /// Store a sealed seed. Refuses to overwrite: a `/create` that silently
    /// replaced an existing vault would destroy funds (§5).
    pub fn insert_wallet(
        &self,
        user: UserId,
        vault: &Vault,
        birthday: u32,
        challenge: Option<[u8; 3]>,
    ) -> Result<()> {
        let db = self.lock()?;
        let affected = db.execute(
            "INSERT OR IGNORE INTO wallets
                 (user_id, salt, nonce, seed_ct, birthday_height, created_at, backup_challenge)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                user.to_string(),
                vault.salt.to_vec(),
                vault.nonce.to_vec(),
                vault.ciphertext,
                i64::from(birthday),
                now_secs(),
                challenge.map(|c| c.to_vec()),
            ],
        )?;

        if affected == 0 {
            return Err(CoreError::WalletExists);
        }
        Ok(())
    }

    /// The word indices this user was asked to read back, if the quiz is still
    /// outstanding.
    pub fn backup_challenge(&self, user: UserId) -> Result<Option<[u8; 3]>> {
        let db = self.lock()?;
        let raw: Option<Vec<u8>> = db
            .query_row(
                "SELECT backup_challenge FROM wallets WHERE user_id = ?1",
                params![user.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        Ok(raw.and_then(|v| <[u8; 3]>::try_from(v.as_slice()).ok()))
    }

    pub fn mark_backup_confirmed(&self, user: UserId) -> Result<()> {
        let db = self.lock()?;
        db.execute(
            "UPDATE wallets SET backup_confirmed = 1, backup_challenge = NULL
             WHERE user_id = ?1",
            params![user.to_string()],
        )?;
        Ok(())
    }

    pub fn delete_wallet(&self, user: UserId) -> Result<()> {
        let db = self.lock()?;
        db.execute(
            "DELETE FROM wallets WHERE user_id = ?1",
            params![user.to_string()],
        )?;
        db.execute(
            "DELETE FROM labels WHERE user_id = ?1",
            params![user.to_string()],
        )?;
        Ok(())
    }

    /// Open a user's vault, counting the attempt (§5).
    ///
    /// The counting is the point: `crypto::open` cannot tell a wrong PIN from a
    /// corrupt vault, and it has no idea whose vault it is. Here we know both,
    /// so this is where a failure becomes `WrongPin` and moves the user towards
    /// a lockout, and where a success clears the count.
    pub fn unseal(&self, user: UserId, pin: &str) -> Result<zeroize::Zeroizing<String>> {
        if let Some(until) = self.locked_until(user)? {
            return Err(CoreError::PinLocked { until });
        }

        let vault = self.vault(user)?;

        match crate::crypto::open(pin, &vault) {
            Ok(secret) => {
                self.clear_failures(user)?;
                Ok(secret)
            }
            Err(_) => Err(self.record_failure(user)?),
        }
    }

    fn vault(&self, user: UserId) -> Result<Vault> {
        let db = self.lock()?;
        let row = db
            .query_row(
                "SELECT salt, nonce, seed_ct FROM wallets WHERE user_id = ?1",
                params![user.to_string()],
                |r| {
                    Ok((
                        r.get::<_, Vec<u8>>(0)?,
                        r.get::<_, Vec<u8>>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or(CoreError::NoWallet)?;

        let salt: [u8; SALT_LEN] = row
            .0
            .try_into()
            .map_err(|_| CoreError::Crypto("stored salt has the wrong length"))?;
        let nonce: [u8; NONCE_LEN] = row
            .1
            .try_into()
            .map_err(|_| CoreError::Crypto("stored nonce has the wrong length"))?;

        Ok(Vault {
            salt,
            nonce,
            ciphertext: row.2,
        })
    }

    /// The lockout instant, or `None` if the user may try now.
    pub fn locked_until(&self, user: UserId) -> Result<Option<SystemTime>> {
        let db = self.lock()?;
        let until: Option<i64> = db
            .query_row(
                "SELECT locked_until FROM wallets WHERE user_id = ?1",
                params![user.to_string()],
                |r| r.get(0),
            )
            .optional()?
            .flatten();

        Ok(match until {
            Some(secs) if secs > now_secs() => Some(UNIX_EPOCH + Duration::from_secs(secs as u64)),
            _ => None,
        })
    }

    /// Count one failure and decide what the user is told (§5).
    fn record_failure(&self, user: UserId) -> Result<CoreError> {
        let db = self.lock()?;
        db.execute(
            "UPDATE wallets SET failed_attempts = failed_attempts + 1 WHERE user_id = ?1",
            params![user.to_string()],
        )?;

        let attempts: u32 = db.query_row(
            "SELECT failed_attempts FROM wallets WHERE user_id = ?1",
            params![user.to_string()],
            |r| r.get::<_, i64>(0).map(|v| v as u32),
        )?;

        if attempts < MAX_ATTEMPTS {
            return Ok(CoreError::WrongPin {
                remaining: MAX_ATTEMPTS - attempts,
            });
        }

        // Exponential backoff past the threshold, capped so a wallet is never
        // bricked outright: 1 min, 2, 4 … up to a day.
        let over = attempts - MAX_ATTEMPTS;
        let lockout = BASE_LOCKOUT
            .checked_mul(1u32 << over.min(20))
            .unwrap_or(MAX_LOCKOUT)
            .min(MAX_LOCKOUT);
        let until_secs = now_secs() + lockout.as_secs() as i64;

        db.execute(
            "UPDATE wallets SET locked_until = ?2 WHERE user_id = ?1",
            params![user.to_string(), until_secs],
        )?;

        Ok(CoreError::PinLocked {
            until: UNIX_EPOCH + Duration::from_secs(until_secs as u64),
        })
    }

    fn clear_failures(&self, user: UserId) -> Result<()> {
        let db = self.lock()?;
        db.execute(
            "UPDATE wallets SET failed_attempts = 0, locked_until = NULL WHERE user_id = ?1",
            params![user.to_string()],
        )?;
        Ok(())
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    const MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";

    fn seeded() -> (Storage, UserId) {
        let storage = Storage::in_memory().expect("opens");
        let user = UserId::new();
        let vault =
            crate::crypto::seal("864213", &Zeroizing::new(MNEMONIC.to_string())).expect("seals");
        storage
            .insert_wallet(user, &vault, 101, Some([0, 5, 11]))
            .expect("inserts");
        (storage, user)
    }

    #[test]
    fn a_wallet_round_trips_through_the_vault() {
        let (storage, user) = seeded();
        assert!(storage.wallet_exists(user).expect("queries"));
        let recovered = storage.unseal(user, "864213").expect("opens");
        assert_eq!(*recovered, MNEMONIC);
        assert_eq!(storage.record(user).expect("reads").birthday, 101);
    }

    #[test]
    fn a_second_create_cannot_overwrite_a_seed() {
        let (storage, user) = seeded();
        let other =
            crate::crypto::seal("111111", &Zeroizing::new(MNEMONIC.to_string())).expect("seals");
        assert!(matches!(
            storage.insert_wallet(user, &other, 0, None),
            Err(CoreError::WalletExists)
        ));
        // And the original still opens with the original PIN.
        assert!(storage.unseal(user, "864213").is_ok());
    }

    #[test]
    fn an_unknown_user_has_no_wallet() {
        let storage = Storage::in_memory().expect("opens");
        let stranger = UserId::new();
        assert!(!storage.wallet_exists(stranger).expect("queries"));
        assert!(matches!(storage.record(stranger), Err(CoreError::NoWallet)));
        assert!(matches!(
            storage.unseal(stranger, "864213"),
            Err(CoreError::NoWallet)
        ));
    }

    /// §5: the count is shared, so the lockout cannot be reset by switching
    /// front ends.
    #[test]
    fn five_wrong_pins_lock_the_wallet() {
        let (storage, user) = seeded();

        for expected_remaining in (1..=4).rev() {
            match storage.unseal(user, "000000") {
                Err(CoreError::WrongPin { remaining }) => {
                    assert_eq!(remaining, expected_remaining);
                }
                other => panic!("expected WrongPin, got {other:?}"),
            }
        }

        assert!(matches!(
            storage.unseal(user, "000000"),
            Err(CoreError::PinLocked { .. })
        ));

        // While locked, even the right PIN is refused — otherwise the lockout
        // would only slow down an attacker who is wrong.
        assert!(matches!(
            storage.unseal(user, "864213"),
            Err(CoreError::PinLocked { .. })
        ));
    }

    #[test]
    fn a_correct_pin_clears_the_failure_count() {
        let (storage, user) = seeded();
        let _ = storage.unseal(user, "000000");
        let _ = storage.unseal(user, "000000");
        storage.unseal(user, "864213").expect("the right PIN opens");

        // Back to a full five attempts.
        match storage.unseal(user, "000000") {
            Err(CoreError::WrongPin { remaining }) => assert_eq!(remaining, 4),
            other => panic!("expected a reset counter, got {other:?}"),
        }
    }

    #[test]
    fn deleting_a_wallet_removes_the_vault() {
        let (storage, user) = seeded();
        storage.delete_wallet(user).expect("deletes");
        assert!(!storage.wallet_exists(user).expect("queries"));
        assert!(matches!(
            storage.unseal(user, "864213"),
            Err(CoreError::NoWallet)
        ));
    }

    #[test]
    fn the_backup_flag_starts_false_and_can_be_set() {
        let (storage, user) = seeded();
        assert!(!storage.record(user).expect("reads").backup_confirmed);
        storage.mark_backup_confirmed(user).expect("marks");
        assert!(storage.record(user).expect("reads").backup_confirmed);
    }

    /// §3a: core issues the challenge and core remembers it, so a front end
    /// cannot hand back three indices of its own choosing.
    #[test]
    fn the_backup_challenge_is_remembered_by_core_and_cleared_when_answered() {
        let (storage, user) = seeded();
        assert_eq!(
            storage.backup_challenge(user).expect("reads"),
            Some([0, 5, 11])
        );
        storage.mark_backup_confirmed(user).expect("marks");
        assert_eq!(storage.backup_challenge(user).expect("reads"), None);
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("app.sqlite");

        let user = {
            let first = Storage::open(&path).expect("opens");
            let user = UserId::new();
            let vault = crate::crypto::seal("864213", &Zeroizing::new(MNEMONIC.to_string()))
                .expect("seals");
            first.insert_wallet(user, &vault, 7, None).expect("inserts");
            user
        };

        // Reopening runs migrate() again; the data must survive.
        let second = Storage::open(&path).expect("reopens");
        assert_eq!(second.record(user).expect("reads").birthday, 7);
    }
}
