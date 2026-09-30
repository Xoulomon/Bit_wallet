//! `tg_id` ↔ `UserId` (PLAN.md §3a rule 3).
//!
//! This map is the whole reason core has never heard of Telegram. It lives in
//! the bot's own `bot.sqlite`, beside the dialogue state — core's database has
//! no column for it and no way to learn one. A user met for the first time is
//! minted a fresh `UserId` here (§8.7).

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::{path::Path, sync::Mutex};
use wallet_core::types::UserId;

pub struct UserStore {
    db: Mutex<Connection>,
}

impl UserStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS telegram_users (
                 tg_id      INTEGER PRIMARY KEY,
                 user_id    TEXT NOT NULL UNIQUE,
                 is_admin   INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL
             );",
        )
        .context("creating telegram_users")?;
        Ok(UserStore { db: Mutex::new(db) })
    }

    /// The `UserId` for this Telegram id, minting one on first contact (§8.7).
    pub fn resolve(&self, tg_id: i64) -> Result<UserId> {
        let db = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("user store mutex poisoned"))?;

        let existing: Option<String> = db
            .query_row(
                "SELECT user_id FROM telegram_users WHERE tg_id = ?1",
                [tg_id],
                |r| r.get(0),
            )
            .optional()
            .context("reading telegram_users")?;

        if let Some(raw) = existing {
            return raw.parse().context("stored user_id is not a UUID");
        }

        let fresh = UserId::new();
        db.execute(
            "INSERT INTO telegram_users (tg_id, user_id, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                tg_id,
                fresh.to_string(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            ],
        )
        .context("inserting a new telegram user")?;

        Ok(fresh)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_telegram_id_always_maps_to_the_same_user() {
        let dir = std::env::temp_dir().join(format!("bot-users-{}", std::process::id()));
        let path = dir.join("bot.sqlite");
        let _ = std::fs::remove_file(&path);
        let store = UserStore::open(&path).expect("opens");

        let first = store.resolve(4242).expect("mints");
        let again = store.resolve(4242).expect("resolves");
        assert_eq!(first, again);

        let other = store.resolve(9999).expect("mints another");
        assert_ne!(first, other);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
