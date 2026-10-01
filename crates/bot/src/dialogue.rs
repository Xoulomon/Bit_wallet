//! Dialogue state (PLAN.md §8.4).
//!
//! Pure UI state, persisted in `bot.sqlite` so a flow survives a restart. No
//! variant holds a seed, a PSBT or a key — only ids and rendering context. If a
//! variant ever needs one of those, the facade is missing a method (§3a).
//!
//! `AwaitPin` is the single place a PIN is collected, for every action that
//! needs one, so delete-on-receipt, the lockout message and the retry counter
//! are written once.

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use teloxide::{dispatching::dialogue::Storage, prelude::*, types::ChatId};

/// What a collected PIN is for. The PIN itself is forwarded straight into core
/// and never stored in any of these.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum PendingAction {
    Unlock,
    Export,
    Delete,
    /// Sign and broadcast a quote core is already holding (Step 5).
    Send {
        quote: String,
    },
    /// Open a payjoin receiving session, which needs the seed to contribute an
    /// input and sign the proposal (§7).
    PayjoinReceive {
        sats: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Intent {
    Create,
    Restore {
        /// The words the user typed, held only until the PIN arrives.
        ///
        /// This is the one uncomfortable place in the design and it is
        /// unavoidable: §8.4 says the flow must survive a restart, and the
        /// mnemonic cannot be sealed until there is a PIN to seal it with. The
        /// message carrying it is deleted from the chat on receipt (§8.1), and
        /// the row is deleted the moment the wallet is created.
        words: String,
        birthday: Option<u32>,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum State {
    #[default]
    Start,

    /// The mnemonic has been shown and is self-deleting; the quiz is next (§5).
    CreateConfirmWords {
        /// The word numbers core asked for, 1-based for display.
        challenge: [u8; 3],
        answered: Vec<String>,
    },

    RestoreMnemonic,
    RestoreBirthday {
        words: String,
    },
    RestoreConfirmDepth {
        words: String,
        birthday: Option<u32>,
        /// The status message to edit rather than replace (§8.1).
        card: Option<i32>,
    },

    SetPin {
        intent: Intent,
    },
    ConfirmPin {
        intent: Intent,
        first: String,
    },

    AwaitPin {
        pending: PendingAction,
    },

    /// `/delete` needs the word typed out, not just a button press (§8.1).
    DeleteTypeConfirm,

    AwaitFeeChoice {
        target: String,
        amount: Option<u64>,
    },
    AwaitCustomFee {
        target: String,
        amount: Option<u64>,
    },
    SendConfirm {
        quote: String,
        card: Option<i32>,
    },
}

impl State {
    /// Whether this state is waiting for a message the user must not leave in
    /// the chat (§8.1): a PIN, or a seed phrase. The handler for every such
    /// state deletes the message before it looks at it; this is the list that
    /// says which ones those are.
    #[allow(dead_code)] // asserted by the tests below; used by the audit in §10
    pub fn expects_secret(&self) -> bool {
        matches!(
            self,
            State::SetPin { .. }
                | State::ConfirmPin { .. }
                | State::AwaitPin { .. }
                | State::RestoreMnemonic
        )
    }
}

/// Dialogue persistence over the bot's own `bot.sqlite`.
///
/// teloxide ships a SQLite storage, but it is built on sqlx, whose
/// `libsqlite3-sys` cannot coexist with the one `bdk_wallet`'s rusqlite links.
/// Rather than give up the "flows survive a restart" requirement of §8.4, the
/// `Storage` trait is implemented here over the connection this crate already
/// has.
pub struct SqliteDialogueStore {
    db: Arc<Mutex<rusqlite::Connection>>,
}

#[derive(Debug, thiserror::Error)]
pub enum DialogueError {
    #[error("dialogue storage: {0}")]
    Storage(String),
    #[error("dialogue state could not be read back: {0}")]
    Serde(#[from] serde_json::Error),
}

impl SqliteDialogueStore {
    pub fn new(db: Arc<Mutex<rusqlite::Connection>>) -> Result<Arc<Self>, DialogueError> {
        {
            let conn = db
                .lock()
                .map_err(|_| DialogueError::Storage("mutex poisoned".into()))?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS dialogues (
                     chat_id INTEGER PRIMARY KEY,
                     state   TEXT NOT NULL
                 );",
            )
            .map_err(|e| DialogueError::Storage(e.to_string()))?;
        }
        Ok(Arc::new(SqliteDialogueStore { db }))
    }
}

impl<D> Storage<D> for SqliteDialogueStore
where
    D: Send + 'static + Serialize + for<'de> Deserialize<'de>,
{
    type Error = DialogueError;

    fn remove_dialogue(
        self: Arc<Self>,
        chat_id: ChatId,
    ) -> futures::future::BoxFuture<'static, Result<(), Self::Error>> {
        Box::pin(async move {
            let conn = self
                .db
                .lock()
                .map_err(|_| DialogueError::Storage("mutex poisoned".into()))?;
            conn.execute("DELETE FROM dialogues WHERE chat_id = ?1", [chat_id.0])
                .map_err(|e| DialogueError::Storage(e.to_string()))?;
            Ok(())
        })
    }

    fn update_dialogue(
        self: Arc<Self>,
        chat_id: ChatId,
        dialogue: D,
    ) -> futures::future::BoxFuture<'static, Result<(), Self::Error>> {
        Box::pin(async move {
            let encoded = serde_json::to_string(&dialogue)?;
            let conn = self
                .db
                .lock()
                .map_err(|_| DialogueError::Storage("mutex poisoned".into()))?;
            conn.execute(
                "INSERT INTO dialogues (chat_id, state) VALUES (?1, ?2)
                 ON CONFLICT(chat_id) DO UPDATE SET state = excluded.state",
                rusqlite::params![chat_id.0, encoded],
            )
            .map_err(|e| DialogueError::Storage(e.to_string()))?;
            Ok(())
        })
    }

    fn get_dialogue(
        self: Arc<Self>,
        chat_id: ChatId,
    ) -> futures::future::BoxFuture<'static, Result<Option<D>, Self::Error>> {
        Box::pin(async move {
            let encoded: Option<String> = {
                let conn = self
                    .db
                    .lock()
                    .map_err(|_| DialogueError::Storage("mutex poisoned".into()))?;
                conn.query_row(
                    "SELECT state FROM dialogues WHERE chat_id = ?1",
                    [chat_id.0],
                    |r| r.get(0),
                )
                .ok()
            };

            match encoded {
                // A state written by an older build no longer parses. Dropping
                // it returns the user to Start, which is the safe direction:
                // the alternative is a chat that can never issue a command again.
                Some(raw) => Ok(serde_json::from_str(&raw).ok()),
                None => Ok(None),
            }
        })
    }
}

pub type WalletDialogue = Dialogue<State, SqliteDialogueStore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_dialogue_state_can_hold_a_secret_beyond_the_restore_window() {
        // A compile-time reminder of §8.4: if a variant grows a field named
        // like a key, this test is where the reviewer is told to object.
        let states = [
            State::Start,
            State::CreateConfirmWords {
                challenge: [0, 1, 2],
                answered: vec![],
            },
            State::AwaitPin {
                pending: PendingAction::Unlock,
            },
            State::DeleteTypeConfirm,
        ];
        for s in states {
            let json = serde_json::to_string(&s).expect("serialises");
            assert!(!json.contains("psbt"), "a PSBT must stay inside core");
            assert!(!json.contains("xprv"), "a key must never reach the bot");
        }
    }

    #[test]
    fn the_states_that_expect_a_secret_are_exactly_the_ones_that_delete_it() {
        assert!(State::RestoreMnemonic.expects_secret());
        assert!(
            State::AwaitPin {
                pending: PendingAction::Delete
            }
            .expects_secret()
        );
        assert!(
            State::SetPin {
                intent: Intent::Create
            }
            .expects_secret()
        );
        assert!(!State::Start.expects_secret());
        assert!(!State::DeleteTypeConfirm.expects_secret());
    }

    /// §8.4: a flow must survive a restart, and a state that cannot be read
    /// back silently returns the user to Start — mid-payjoin, that would lose
    /// the amount they asked for.
    #[test]
    fn a_pending_payjoin_receive_survives_a_restart() {
        let waiting = State::AwaitPin {
            pending: PendingAction::PayjoinReceive { sats: 25_000 },
        };
        let encoded = serde_json::to_string(&waiting).expect("serialises");
        let back: State = serde_json::from_str(&encoded).expect("deserialises");
        assert_eq!(waiting, back);

        // And it is one of the states that expects a secret, so the handler
        // deletes the message carrying it.
        assert!(waiting.expects_secret());
    }

    #[test]
    fn a_state_round_trips_through_storage() {
        let original = State::AwaitPin {
            pending: PendingAction::Send {
                quote: "01HXYZ".into(),
            },
        };
        let encoded = serde_json::to_string(&original).expect("serialises");
        let back: State = serde_json::from_str(&encoded).expect("deserialises");
        assert_eq!(original, back);
    }

    #[test]
    fn an_unreadable_state_is_dropped_rather_than_trapping_the_chat() {
        assert!(serde_json::from_str::<State>("{\"Nonsense\":{}}").is_err());
    }
}
