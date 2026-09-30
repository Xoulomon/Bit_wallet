//! Payjoin session persistence (PLAN.md §7).
//!
//! The `payjoin` crate models a session as an **event log**: each typestate
//! transition appends an event, and replaying the log reconstructs the state.
//! That is what makes "a bot restart resumes polling" cheap — there is no
//! serialised state machine to version, only a list of events to replay.
//!
//! One SQLite table holds every session's events, keyed by our own `SessionId`,
//! so a restart can enumerate the open ones and replay each.

use crate::{
    error::{CoreError, Result},
    service::types::{PayjoinRole, SessionId, UserId},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    marker::PhantomData,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
}

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
            "SELECT id, user_id, role, created_at, expires_at, closed, state
             FROM pj_sessions WHERE id = ?1",
            params![id.to_string()],
            read_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Every session belonging to one user, newest first (§8.2).
    pub fn for_user(&self, user: UserId) -> Result<Vec<SessionRow>> {
        let db = self.lock()?;
        let mut stmt = db.prepare(
            "SELECT id, user_id, role, created_at, expires_at, closed, state
             FROM pj_sessions WHERE user_id = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![user.to_string()], read_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Sessions a restart must resume polling (§7).
    pub fn open_sessions(&self) -> Result<Vec<SessionRow>> {
        let db = self.lock()?;
        let mut stmt = db.prepare(
            "SELECT id, user_id, role, created_at, expires_at, closed, state
             FROM pj_sessions WHERE closed = 0 AND expires_at > ?1",
        )?;
        let rows = stmt.query_map(params![now_secs()], read_row)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// §7: remember an outpoint the receiver has contributed against, so the
    /// same input cannot be replayed to probe which coins are ours.
    pub fn remember_input(&self, outpoint: &str) -> Result<bool> {
        let db = self.lock()?;
        let inserted = db.execute(
            "INSERT OR IGNORE INTO pj_seen_inputs (outpoint, seen_at) VALUES (?1, ?2)",
            params![outpoint, now_secs()],
        )?;
        Ok(inserted == 1)
    }

    pub fn has_seen_input(&self, outpoint: &str) -> Result<bool> {
        let db = self.lock()?;
        let found: Option<i64> = db
            .query_row(
                "SELECT 1 FROM pj_seen_inputs WHERE outpoint = ?1",
                params![outpoint],
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
    #[test]
    fn only_open_unexpired_sessions_are_resumed() {
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
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, live);
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
