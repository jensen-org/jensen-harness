use rusqlite::{Connection, params};
use serde_json::Value;

use super::{Store, StoreError, now_ms, to_u64};

/// What a journal entry records. A kind the database holds but this version
/// does not know is an error on read, never skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    SessionCreated,
    PromptStarted,
    ModelResponded,
    ModelFailed,
    ToolStarted,
    ToolFinished,
    TurnResults,
    PromptFinished,
    BudgetExhausted,
    CacheLow,
}

impl EventKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionCreated => "session.created",
            Self::PromptStarted => "prompt.started",
            Self::ModelResponded => "model.responded",
            Self::ModelFailed => "model.failed",
            Self::ToolStarted => "tool.started",
            Self::ToolFinished => "tool.finished",
            Self::TurnResults => "turn.results",
            Self::PromptFinished => "prompt.finished",
            Self::BudgetExhausted => "budget.exhausted",
            Self::CacheLow => "alert.cache_low",
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "session.created" => Self::SessionCreated,
            "prompt.started" => Self::PromptStarted,
            "model.responded" => Self::ModelResponded,
            "model.failed" => Self::ModelFailed,
            "tool.started" => Self::ToolStarted,
            "tool.finished" => Self::ToolFinished,
            "turn.results" => Self::TurnResults,
            "prompt.finished" => Self::PromptFinished,
            "budget.exhausted" => Self::BudgetExhausted,
            "alert.cache_low" => Self::CacheLow,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub session_id: String,
    pub parent_id: Option<String>,
    pub kind: EventKind,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub seq: u64,
    pub id: String,
    pub parent_id: Option<String>,
    pub session_id: String,
    pub kind: EventKind,
    pub payload: Value,
    pub created_at_ms: u64,
}

pub(crate) fn insert_event(conn: &Connection, event: &NewEvent) -> Result<Event, StoreError> {
    let id = ulid::Ulid::generate().to_string();
    let created_at_ms = now_ms()?;
    let payload = serde_json::to_string(&event.payload)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    conn.execute(
        "INSERT INTO events (id, parent_id, session_id, kind, payload, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            event.parent_id,
            event.session_id,
            event.kind.as_str(),
            payload,
            created_at_ms
        ],
    )?;
    Ok(Event {
        seq: to_u64(conn.last_insert_rowid())?,
        id,
        parent_id: event.parent_id.clone(),
        session_id: event.session_id.clone(),
        kind: event.kind,
        payload: event.payload.clone(),
        created_at_ms: to_u64(created_at_ms)?,
    })
}

fn read_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, Event)> {
    let kind: String = row.get("kind")?;
    let payload: String = row.get("payload")?;
    let seq: i64 = row.get("seq")?;
    let created: i64 = row.get("created_at_ms")?;
    let conversion = |error: StoreError| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    };
    let kind = EventKind::parse(&kind)
        .ok_or_else(|| conversion(StoreError::Corrupt(format!("event kind {kind:?}"))))?;
    let payload = serde_json::from_str(&payload)
        .map_err(|error| conversion(StoreError::Corrupt(error.to_string())))?;
    Ok((
        seq,
        Event {
            seq: to_u64(seq).map_err(conversion)?,
            id: row.get("id")?,
            parent_id: row.get("parent_id")?,
            session_id: row.get("session_id")?,
            kind,
            payload,
            created_at_ms: to_u64(created).map_err(conversion)?,
        },
    ))
}

impl Store {
    /// Appends one event and returns it as stored. Durable when it returns.
    ///
    /// # Errors
    /// [`StoreError`] when the write fails.
    pub async fn append(&self, event: NewEvent) -> Result<Event, StoreError> {
        self.with(move |conn| insert_event(conn, &event)).await
    }

    /// Every event of a session, oldest first.
    ///
    /// # Errors
    /// [`StoreError::Corrupt`] for an event this version cannot read.
    pub async fn events(&self, session_id: &str) -> Result<Vec<Event>, StoreError> {
        let session_id = session_id.to_string();
        self.with(move |conn| {
            let mut statement = conn.prepare(
                "SELECT seq, id, parent_id, session_id, kind, payload, created_at_ms
                 FROM events WHERE session_id = ?1 ORDER BY seq",
            )?;
            let rows = statement.query_map(params![session_id], read_event)?;
            rows.map(|row| row.map(|(_, event)| event).map_err(read_error))
                .collect()
        })
        .await
    }

    /// The sessions the journal knows, in the order they first appear.
    ///
    /// # Errors
    /// [`StoreError`] when the read fails.
    pub async fn session_ids(&self) -> Result<Vec<String>, StoreError> {
        self.with(|conn| {
            let mut statement = conn
                .prepare("SELECT session_id FROM events GROUP BY session_id ORDER BY MIN(seq)")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.map(|row| row.map_err(StoreError::from)).collect()
        })
        .await
    }
}

fn read_error(error: rusqlite::Error) -> StoreError {
    match error {
        rusqlite::Error::FromSqlConversionFailure(_, _, source) => {
            StoreError::Corrupt(source.to_string())
        }
        other => StoreError::from(other),
    }
}
