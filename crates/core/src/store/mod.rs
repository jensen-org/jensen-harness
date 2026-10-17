//! The state database: an append only event journal and a token ledger in
//! one SQLite file, so a response and the settlement of what it cost land in
//! one transaction. Nothing here is ever updated or deleted; the database
//! itself refuses it.

pub mod journal;
pub mod ledger;
mod schema;
pub mod workflow;

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

pub use journal::{Event, EventKind, NewEvent};
pub use ledger::{
    LedgerError, LimitKind, NewScope, Refusal, Reservation, ReserveOutcome, ReserveRequest,
    ScopeKind, ScopeUsage,
};

#[derive(Debug, Clone, thiserror::Error)]
pub enum StoreError {
    #[error("the state database failed: {0}")]
    Sqlite(String),
    #[error("the state database could not be migrated: {0}")]
    Migration(String),
    #[error("the state file could not be prepared: {0}")]
    Io(String),
    #[error(
        "the state database {0} is in use by another jensen-harness process; stop it, or pass --state with another path"
    )]
    InUse(String),
    #[error("a stored amount does not fit in the type it is read as")]
    Overflow,
    #[error("the state database holds something this version cannot read: {0}")]
    Corrupt(String),
    #[error(transparent)]
    Ledger(#[from] LedgerError),
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error.to_string())
    }
}

pub(crate) fn to_i64(value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_too_large| StoreError::Overflow)
}

pub(crate) fn to_u64(value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_negative| StoreError::Corrupt("a negative amount".to_string()))
}

pub(crate) fn now_ms() -> Result<i64, StoreError> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| StoreError::Io(error.to_string()))?;
    i64::try_from(elapsed.as_millis()).map_err(|_too_large| StoreError::Overflow)
}

/// A handle to the state database. Clones share one connection; every call
/// runs on a blocking thread so the async loop never waits on the disk.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    /// Holds the advisory lock for as long as any clone of the store lives.
    /// The operating system releases it when the process dies, so a crash
    /// never leaves the database locked out.
    _lock: Option<Arc<std::fs::File>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Opens or creates the database at `path`, readable by its owner only.
    ///
    /// # Errors
    /// [`StoreError`] when the file cannot be prepared, the database cannot
    /// be opened, or its schema is newer than this version understands.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| StoreError::Io(error.to_string()))?;
        }
        let mut file = std::fs::OpenOptions::new();
        file.write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            file.mode(0o600);
        }
        file.open(path)
            .map_err(|error| StoreError::Io(format!("{}: {error}", path.display())))?;
        let lock = Self::lock(path, &file)?;
        let mut conn = Connection::open(path)?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))?;
        schema::prepare(&mut conn)?;
        Ok(Self::wrap(conn, Some(lock)))
    }

    /// A database that lives only as long as the process, for tests.
    ///
    /// # Errors
    /// [`StoreError`] when the schema cannot be applied.
    pub fn in_memory() -> Result<Self, StoreError> {
        let mut conn = Connection::open_in_memory()?;
        schema::prepare(&mut conn)?;
        Ok(Self::wrap(conn, None))
    }

    /// Two daemons on one state file would both drive a session that is
    /// open in the journal, so the second one is refused.
    fn lock(path: &Path, options: &std::fs::OpenOptions) -> Result<Arc<std::fs::File>, StoreError> {
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock_path = std::path::PathBuf::from(lock_path);
        let file = options
            .open(&lock_path)
            .map_err(|error| StoreError::Io(format!("{}: {error}", lock_path.display())))?;
        match file.try_lock() {
            Ok(()) => Ok(Arc::new(file)),
            Err(std::fs::TryLockError::WouldBlock) => {
                Err(StoreError::InUse(path.display().to_string()))
            }
            Err(std::fs::TryLockError::Error(error)) => {
                Err(StoreError::Io(format!("{}: {error}", lock_path.display())))
            }
        }
    }

    fn wrap(conn: Connection, lock: Option<Arc<std::fs::File>>) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            _lock: lock,
        }
    }

    pub(crate) async fn with<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().map_err(|_poisoned| {
                StoreError::Sqlite("the connection lock was poisoned".into())
            })?;
            work(&mut guard)
        })
        .await
        .map_err(|error| StoreError::Sqlite(error.to_string()))?
    }
}
