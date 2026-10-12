//! Payjoin session persistence (PLAN.md §7).
//!
//! The `payjoin` crate models a session as an **event log**: each typestate
//! transition appends an event, and replaying the log reconstructs the state.
//! That is what makes resuming cheap — there is no serialised state machine to
//! version, only a list of events to replay.
//!
//! One SQLite table holds every session's events, keyed by our own `SessionId`,
//! so a restart can enumerate the open ones and replay each. `open_sessions`
//! is that enumeration, and `WalletService::resume_payjoin_sessions` is its
//! caller — it had none for a long time, which meant this module described a
//! resumption that never happened. What a restart can do depends on what the
//! session still needs:
//!
//! * a receiver past its proposal is only waiting to see which transaction
//!   settled it, which is a chain lookup, so it resumes immediately;
//! * a receiver short of that has to select an input and sign, and nobody is
//!   unlocked a moment after a restart, so it resumes on the owner's next
//!   `unlock`;
//! * a sender's Original is signed and in the log, so it goes back on the wire
//!   and the payment lands as an ordinary one.
//!
//! Rows are therefore kept past their expiry and removed only by `closed`:
//! `expires_at` is when the *directory* session ends, not when there is
//! nothing left to find out.

use crate::{
    error::{CoreError, Result},
    service::types::{PayjoinRole, SessionId, UserId},
};
use bdk_wallet::bitcoin::{Amount, Txid};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    marker::PhantomData,
    path::Path,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// How long an outpoint stays in `pj_seen_inputs`.
///
/// `check_no_inputs_seen_before` is recorded *before* the session is known to
/// succeed, so without a TTL a single failed attempt blacklists that sender's
/// coins for good — and a sender whose first try failed for a reason on our
/// side could never retry with the same UTXOs. A week is long enough to spoil
/// probing, which is a burst, and short enough to forgive a bad afternoon.
const SEEN_INPUT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The row the front end sees, before the event log is replayed.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub id: SessionId,
    pub user: UserId,
    pub role: PayjoinRole,
    pub created_at: SystemTime,
    pub expires_at: SystemTime,
    pub closed: bool,
    /// The last state we recorded, so `/pj_sessions` can render without
    /// replaying every log (§8.2).
    pub state: String,
    /// The transaction that settled this session, once one has.
    ///
    /// Recorded rather than derived, because the only moment it is knowable is
    /// when the payjoin crate hands us a `SessionOutcome` — and a front end
    /// that reads a closed row later still has to be able to show it.
    pub txid: Option<Txid>,
    /// What the receiver asked for, so `/pj_sessions` can say so (§8.2).
    pub amount: Option<Amount>,
}

/// Every column `read_row` expects, in order. One constant so a new SELECT
/// cannot drift from the reader and shift every column by one.
const ROW_COLUMNS: &str =
    "id, user_id, role, created_at, expires_at, closed, state, txid, amount_sat";

/// The store behind every session's event log.
pub struct SessionStore {
    db: Arc<Mutex<Connection>>,
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::Storage(e.to_string()))?;
        }
        let db = Connection::open(path)?;
        let store = SessionStore {
            db: Arc::new(Mutex::new(db)),
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn in_memory() -> Result<Self> {
        let store = SessionStore {
            db: Arc::new(Mutex::new(Connection::open_in_memory()?)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let db = self.lock()?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS pj_sessions (
                 id         TEXT PRIMARY KEY,
                 user_id    TEXT NOT NULL,
                 role       TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL,
                 closed     INTEGER NOT NULL DEFAULT 0,
                 state      TEXT NOT NULL DEFAULT 'Waiting',
                 amount_sat INTEGER
             );
             CREATE TABLE IF NOT EXISTS pj_events (
                 session_id TEXT NOT NULL,
                 seq        INTEGER NOT NULL,
                 payload    TEXT NOT NULL,
                 PRIMARY KEY (session_id, seq)
             );
             -- §7: outpoints this receiver has already seen, so a sender
             -- cannot probe us by replaying the same input.
             CREATE TABLE IF NOT EXISTS pj_seen_inputs (
                 outpoint TEXT PRIMARY KEY,
                 seen_at  INTEGER NOT NULL
             );",
        )?;

        // `CREATE TABLE IF NOT EXISTS` cannot add a column to a table that
        // already exists, so the settlement txid arrives as its own migration.
        // SQLite has no `ADD COLUMN IF NOT EXISTS`; a duplicate column is the
        // ordinary second-run case and the only error worth swallowing here.
        match db.execute("ALTER TABLE pj_sessions ADD COLUMN txid TEXT", []) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e.into()),
        }

        // Forget outpoints older than the TTL on the way in, so the sweep costs
        // one statement per process rather than a background task.
        db.execute(
            "DELETE FROM pj_seen_inputs WHERE seen_at < ?1",
            params![now_secs() - SEEN_INPUT_TTL.as_secs() as i64],
        )?;

        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| CoreError::Storage("payjoin session store poisoned".into()))
    }

    /// Register a new session before its first event.
    pub fn create(
        &self,
        id: SessionId,
        user: UserId,
        role: PayjoinRole,
        expires_in: Duration,
        amount_sat: Option<u64>,
    ) -> Result<()> {
        let db = self.lock()?;
        let now = now_secs();
        db.execute(
            "INSERT INTO pj_sessions (id, user_id, role, created_at, expires_at, amount_sat)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id.to_string(),
                user.to_string(),
                role_name(role),
                now,
                now + expires_in.as_secs() as i64,
                amount_sat.map(|a| a as i64),
            ],
        )?;
        Ok(())
    }

    /// Record the state a front end should render (§8.2). Cheap to read, and
    /// authoritative only for display — the event log is the real state.
    pub fn set_state(&self, id: SessionId, state: &str) -> Result<()> {
        let db = self.lock()?;
        db.execute(
            "UPDATE pj_sessions SET state = ?2 WHERE id = ?1",
            params![id.to_string(), state],
        )?;
        Ok(())
    }

    /// Record how a session ended, together with the transaction that settled
    /// it (§7 step 4).
    ///
    /// The txid is the whole point: without it a closed session can say only
    /// *that* it finished, and `/pj_sessions` had to invent an all-zeros txid
    /// to fill the gap. `None` is for the outcomes that genuinely have no
    /// transaction — a cancelled session, or a sender whose non-SegWit inputs
    /// mean the proposal's txid was never knowable.
    pub fn set_outcome(&self, id: SessionId, state: &str, txid: Option<Txid>) -> Result<()> {
        let db = self.lock()?;
        db.execute(
            "UPDATE pj_sessions SET state = ?2, txid = ?3 WHERE id = ?1",
            params![id.to_string(), state, txid.map(|t| t.to_string())],
        )?;
        Ok(())
    }

    pub fn close(&self, id: SessionId) -> Result<()> {
        let db = self.lock()?;
        db.execute(
            "UPDATE pj_sessions SET closed = 1 WHERE id = ?1",
            params![id.to_string()],
        )?;
        Ok(())
    }

    pub fn row(&self, id: SessionId) -> Result<Option<SessionRow>> {
        let db = self.lock()?;
        db.query_row(
            &format!("SELECT {ROW_COLUMNS} FROM pj_sessions WHERE id = ?1"),
            params![id.to_string()],
            read_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every session belonging to one user, newest first (§8.2).
    pub fn for_user(&self, user: UserId) -> Result<Vec<SessionRow>> {
        let db = self.lock()?;
        let mut stmt = db.prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM pj_sessions
             WHERE user_id = ?1 ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map(params![user.to_string()], read_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Sessions a restart must resume (§7).
    ///
    /// Deliberately *not* filtered by `expires_at`: a receiver that has already
    /// posted its proposal is waiting on a transaction to settle, which is a
    /// chain question and outlives the directory session. Filtering those out
    /// here is what made a restart lose exactly the sessions worth resuming.
    /// The caller decides what each role and state still has to do.
    pub fn open_sessions(&self) -> Result<Vec<SessionRow>> {
        let db = self.lock()?;
        let mut stmt = db.prepare(&format!(
            "SELECT {ROW_COLUMNS} FROM pj_sessions WHERE closed = 0"
        ))?;
        let rows = stmt.query_map([], read_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// §7: remember an outpoint the receiver has contributed against, so the
    /// same input cannot be replayed to probe which coins are ours.
    ///
    /// Returns whether this is the first sighting. A row past `SEEN_INPUT_TTL`
    /// does not count as one: it is dropped and the outpoint is recorded
    /// afresh. The sweep in `migrate` only runs when the store is opened, and
    /// a bot that stays up for a month would otherwise keep enforcing
    /// sightings it had already decided to forget.
    pub fn remember_input(&self, outpoint: &str) -> Result<bool> {
        let db = self.lock()?;
        let now = now_secs();

        db.execute(
            "DELETE FROM pj_seen_inputs WHERE outpoint = ?1 AND seen_at < ?2",
            params![outpoint, now - SEEN_INPUT_TTL.as_secs() as i64],
        )?;

        let inserted = db.execute(
            "INSERT OR IGNORE INTO pj_seen_inputs (outpoint, seen_at) VALUES (?1, ?2)",
            params![outpoint, now],
        )?;
        Ok(inserted == 1)
    }

    /// Whether this outpoint counts as seen *now* — a sighting older than the
    /// TTL does not.
    pub fn has_seen_input(&self, outpoint: &str) -> Result<bool> {
        let db = self.lock()?;
        let found: Option<i64> = db
            .query_row(
                "SELECT 1 FROM pj_seen_inputs WHERE outpoint = ?1 AND seen_at >= ?2",
                params![
                    outpoint,
                    now_secs() - SEEN_INPUT_TTL.as_secs() as i64
                ],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// The `SessionPersister` the payjoin crate drives for one session.
    pub fn persister<E>(&self, id: SessionId) -> EventLog<E> {
        EventLog {
            db: Arc::clone(&self.db),
            id,
            _event: PhantomData,
        }
    }
}

fn read_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r
            .get::<_, String>(0)?
            .parse()
            .unwrap_or_else(|_| SessionId::new()),
        user: r
            .get::<_, String>(1)?
            .parse()
            .unwrap_or_else(|_| UserId::new()),
        role: match r.get::<_, String>(2)?.as_str() {
            "sender" => PayjoinRole::Sender,
            _ => PayjoinRole::Receiver,
        },
        created_at: UNIX_EPOCH + Duration::from_secs(r.get::<_, i64>(3)?.max(0) as u64),
        expires_at: UNIX_EPOCH + Duration::from_secs(r.get::<_, i64>(4)?.max(0) as u64),
        closed: r.get::<_, i64>(5)? != 0,
        state: r.get(6)?,
        // A txid that cannot be parsed is a corrupt row, not a reason to fail
        // the whole listing: the state beside it is still worth showing.
        txid: r
            .get::<_, Option<String>>(7)?
            .and_then(|raw| Txid::from_str(&raw).ok()),
        amount: r
            .get::<_, Option<i64>>(8)?
            .and_then(|sats| u64::try_from(sats).ok())
            .map(Amount::from_sat),
    })
}

fn role_name(role: PayjoinRole) -> &'static str {
    match role {
        PayjoinRole::Sender => "sender",
        PayjoinRole::Receiver => "receiver",
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// One session's append-only event log.
///
/// Events are stored as JSON rather than a binary encoding: a session that
/// survives a restart also has to survive a *rebuild*, and JSON is the format
/// whose compatibility rules are easiest to reason about when a field is added.
pub struct EventLog<E> {
    db: Arc<Mutex<Connection>>,
    id: SessionId,
    _event: PhantomData<E>,
}

#[derive(Debug, thiserror::Error)]
pub enum EventLogError {
    #[error("payjoin event log: {0}")]
    Storage(String),
    #[error("payjoin event could not be read back: {0}")]
    Encoding(String),
}

impl<E> EventLog<E>
where
    E: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    fn conn(&self) -> std::result::Result<std::sync::MutexGuard<'_, Connection>, EventLogError> {
        self.db
            .lock()
            .map_err(|_| EventLogError::Storage("event log poisoned".into()))
    }

    pub fn append(&self, event: &E) -> std::result::Result<(), EventLogError> {
        let payload =
            serde_json::to_string(event).map_err(|e| EventLogError::Encoding(e.to_string()))?;
        let db = self.conn()?;
        db.execute(
            "INSERT INTO pj_events (session_id, seq, payload)
             VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM pj_events WHERE session_id = ?1), ?2)",
            params![self.id.to_string(), payload],
        )
        .map_err(|e| EventLogError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn events(&self) -> std::result::Result<Vec<E>, EventLogError> {
        let db = self.conn()?;
        let mut stmt = db
            .prepare("SELECT payload FROM pj_events WHERE session_id = ?1 ORDER BY seq ASC")
            .map_err(|e| EventLogError::Storage(e.to_string()))?;

        let rows = stmt
            .query_map(params![self.id.to_string()], |r| r.get::<_, String>(0))
            .map_err(|e| EventLogError::Storage(e.to_string()))?;

        let mut out = Vec::new();
        for row in rows {
            let raw = row.map_err(|e| EventLogError::Storage(e.to_string()))?;
            out.push(
                serde_json::from_str(&raw).map_err(|e| EventLogError::Encoding(e.to_string()))?,
            );
        }
        Ok(out)
    }

    pub fn mark_closed(&self) -> std::result::Result<(), EventLogError> {
        let db = self.conn()?;
        db.execute(
            "UPDATE pj_sessions SET closed = 1 WHERE id = ?1",
            params![self.id.to_string()],
        )
        .map_err(|e| EventLogError::Storage(e.to_string()))?;
        Ok(())
    }
}

/// The payjoin crate's own trait, over the log above.
impl<E> payjoin::persist::SessionPersister for EventLog<E>
where
    E: Serialize + DeserializeOwned + Send + Sync + 'static,
{
    type InternalStorageError = EventLogError;
    type SessionEvent = E;

    fn save_event(&self, event: Self::SessionEvent) -> std::result::Result<(), EventLogError> {
        self.append(&event)
    }

    fn load(
        &self,
    ) -> std::result::Result<Box<dyn Iterator<Item = Self::SessionEvent>>, EventLogError> {
        Ok(Box::new(self.events()?.into_iter()))
    }

    fn close(&self) -> std::result::Result<(), EventLogError> {
        self.mark_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use payjoin::persist::SessionPersister as _;

    #[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
    enum TestEvent {
        Created { at: u64 },
        Advanced(String),
    }

    fn store() -> SessionStore {
        SessionStore::in_memory().expect("opens")
    }

    #[test]
    fn a_session_is_listed_for_its_owner_and_nobody_else() {
        let s = store();
        let alice = UserId::new();
        let bob = UserId::new();
        let id = SessionId::new();

        s.create(
            id,
            alice,
            PayjoinRole::Receiver,
            Duration::from_secs(3600),
            Some(50_000),
        )
        .expect("creates");

        assert_eq!(s.for_user(alice).expect("lists").len(), 1);
        assert!(s.for_user(bob).expect("lists").is_empty());
    }

    #[test]
    fn events_replay_in_the_order_they_were_written() {
        let s = store();
        let id = SessionId::new();
        s.create(
            id,
            UserId::new(),
            PayjoinRole::Receiver,
            Duration::from_secs(3600),
            None,
        )
        .expect("creates");

        let log = s.persister::<TestEvent>(id);
        log.save_event(TestEvent::Created { at: 1 }).expect("saves");
        log.save_event(TestEvent::Advanced("one".into()))
            .expect("saves");
        log.save_event(TestEvent::Advanced("two".into()))
            .expect("saves");

        let replayed: Vec<TestEvent> = log.load().expect("loads").collect();
        assert_eq!(
            replayed,
            vec![
                TestEvent::Created { at: 1 },
                TestEvent::Advanced("one".into()),
                TestEvent::Advanced("two".into()),
            ],
            "ordering is the whole point of an event log"
        );
    }

    /// §7: a restart resumes polling, which means finding the open sessions.
    ///
    /// Expiry is **not** a filter here, and that is the fix: a receiver past
    /// its directory expiry may still be waiting to see which transaction
    /// settled it, and dropping those rows is what made a restart lose exactly
    /// the sessions worth resuming. Only `closed` takes a session off the list.
    #[test]
    fn every_unclosed_session_is_offered_for_resumption() {
        let s = store();
        let user = UserId::new();

        let live = SessionId::new();
        s.create(
            live,
            user,
            PayjoinRole::Receiver,
            Duration::from_secs(3600),
            None,
        )
        .expect("creates");

        let closed = SessionId::new();
        s.create(
            closed,
            user,
            PayjoinRole::Sender,
            Duration::from_secs(3600),
            None,
        )
        .expect("creates");
        s.close(closed).expect("closes");

        let expired = SessionId::new();
        s.create(expired, user, PayjoinRole::Receiver, Duration::ZERO, None)
            .expect("creates");

        let open = s.open_sessions().expect("lists");
        let ids: Vec<_> = open.iter().map(|r| r.id).collect();
        assert!(ids.contains(&live), "a live session resumes");
        assert!(
            ids.contains(&expired),
            "an expired session still has a settlement to watch for"
        );
        assert!(!ids.contains(&closed), "a closed session is finished");
        assert_eq!(open.len(), 2);
    }

    /// §7: the settlement txid is the one fact a closed session has to keep —
    /// without it `/pj_sessions` had to invent an all-zeros txid.
    #[test]
    fn an_outcome_records_the_transaction_that_settled_the_session() {
        let s = store();
        let id = SessionId::new();
        s.create(
            id,
            UserId::new(),
            PayjoinRole::Receiver,
            Duration::from_secs(3600),
            Some(120_000),
        )
        .expect("creates");

        assert!(s.row(id).expect("reads").expect("exists").txid.is_none());

        let txid = Txid::from_str(
            "6a0c1b8f0a2f1a4e5b3c7d9e8f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2",
        )
        .expect("a valid txid");
        s.set_outcome(id, "Completed", Some(txid)).expect("records");

        let row = s.row(id).expect("reads").expect("exists");
        assert_eq!(row.state, "Completed");
        assert_eq!(row.txid, Some(txid));
    }

    /// §8.2: the amount was written from the first commit and never read back,
    /// so `/pj_sessions` could not show what the receiver had asked for.
    #[test]
    fn the_requested_amount_survives_a_round_trip() {
        let s = store();
        let id = SessionId::new();
        s.create(
            id,
            UserId::new(),
            PayjoinRole::Receiver,
            Duration::from_secs(3600),
            Some(120_000),
        )
        .expect("creates");

        assert_eq!(
            s.row(id).expect("reads").expect("exists").amount,
            Some(Amount::from_sat(120_000))
        );
    }

    /// §7: a sender whose first attempt failed on *our* side must be able to
    /// retry with the same coins once the TTL has passed.
    #[test]
    fn a_seen_outpoint_is_forgotten_once_the_ttl_has_passed() {
        let s = store();
        let outpoint = "0000000000000000000000000000000000000000000000000000000000000002:1";
        assert!(s.remember_input(outpoint).expect("remembers"));
        assert!(s.has_seen_input(outpoint).expect("queries"));

        // Age the row past the TTL. No sweep needed: the reads are TTL-aware,
        // which is what makes this true for a bot that never restarts.
        {
            let db = s.lock().expect("locks");
            db.execute(
                "UPDATE pj_seen_inputs SET seen_at = ?1 WHERE outpoint = ?2",
                params![
                    now_secs() - SEEN_INPUT_TTL.as_secs() as i64 - 1,
                    outpoint
                ],
            )
            .expect("ages the row");
        }

        assert!(
            !s.has_seen_input(outpoint).expect("queries"),
            "a stale outpoint must not blacklist a sender's coins for good"
        );
        assert!(
            s.remember_input(outpoint).expect("remembers"),
            "and the sender may contribute it again, which is the point"
        );
    }

    #[test]
    fn closing_through_the_persister_marks_the_session_closed() {
        let s = store();
        let id = SessionId::new();
        s.create(
            id,
            UserId::new(),
            PayjoinRole::Receiver,
            Duration::from_secs(3600),
            None,
        )
        .expect("creates");

        let log = s.persister::<TestEvent>(id);
        payjoin::persist::SessionPersister::close(&log).expect("closes");

        assert!(s.row(id).expect("reads").expect("exists").closed);
        assert!(s.open_sessions().expect("lists").is_empty());
    }

    /// §7: `check_no_inputs_seen_before` — a sender who replays an input is
    /// probing which coins are ours, and must be refused.
    #[test]
    fn a_seen_outpoint_is_remembered_across_sessions() {
        let s = store();
        let outpoint = "0000000000000000000000000000000000000000000000000000000000000001:0";

        assert!(!s.has_seen_input(outpoint).expect("queries"));
        assert!(
            s.remember_input(outpoint).expect("remembers"),
            "first sight"
        );
        assert!(s.has_seen_input(outpoint).expect("queries"));
        assert!(
            !s.remember_input(outpoint).expect("remembers"),
            "a repeat is reported as already seen"
        );
    }

    #[test]
    fn two_sessions_keep_separate_logs() {
        let s = store();
        let user = UserId::new();
        let a = SessionId::new();
        let b = SessionId::new();

        for id in [a, b] {
            s.create(
                id,
                user,
                PayjoinRole::Receiver,
                Duration::from_secs(3600),
                None,
            )
            .expect("creates");
        }

        s.persister::<TestEvent>(a)
            .save_event(TestEvent::Created { at: 1 })
            .expect("saves");
        s.persister::<TestEvent>(b)
            .save_event(TestEvent::Created { at: 2 })
            .expect("saves");
        s.persister::<TestEvent>(b)
            .save_event(TestEvent::Advanced("only b".into()))
            .expect("saves");

        assert_eq!(
            s.persister::<TestEvent>(a).events().expect("loads").len(),
            1
        );
        assert_eq!(
            s.persister::<TestEvent>(b).events().expect("loads").len(),
            2
        );
    }

    #[test]
    fn a_state_label_is_readable_without_replaying_the_log() {
        let s = store();
        let id = SessionId::new();
        s.create(
            id,
            UserId::new(),
            PayjoinRole::Receiver,
            Duration::from_secs(3600),
            None,
        )
        .expect("creates");

        assert_eq!(s.row(id).expect("reads").expect("exists").state, "Waiting");
        s.set_state(id, "ProposalSent").expect("updates");
        assert_eq!(
            s.row(id).expect("reads").expect("exists").state,
            "ProposalSent"
        );
    }
}
