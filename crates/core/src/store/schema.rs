use std::sync::LazyLock;

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

use super::StoreError;

const SCHEMA_V1: &str = "
CREATE TABLE events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    parent_id TEXT,
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX events_by_session ON events (session_id, seq);

CREATE TABLE scopes (
    id TEXT PRIMARY KEY,
    parent_id TEXT REFERENCES scopes (id),
    kind TEXT NOT NULL CHECK (kind IN ('run', 'session')),
    max_tokens INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
    max_cost_micro_usd INTEGER CHECK (max_cost_micro_usd IS NULL OR max_cost_micro_usd > 0),
    soft_percent INTEGER NOT NULL CHECK (soft_percent BETWEEN 1 AND 99),
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE ledger (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    scope_id TEXT NOT NULL REFERENCES scopes (id),
    kind TEXT NOT NULL CHECK (kind IN ('reserve', 'settle', 'release', 'adjust')),
    call_id TEXT NOT NULL,
    tokens INTEGER NOT NULL,
    cost_micro_usd INTEGER NOT NULL,
    expires_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    CHECK (kind = 'adjust' OR (tokens >= 0 AND cost_micro_usd >= 0))
);
CREATE INDEX ledger_by_scope ON ledger (scope_id);
CREATE INDEX ledger_by_call ON ledger (call_id);
CREATE UNIQUE INDEX ledger_one_reserve_per_call ON ledger (call_id) WHERE kind = 'reserve';
CREATE UNIQUE INDEX ledger_one_end_per_call ON ledger (call_id) WHERE kind IN ('settle', 'release');

CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'events are append only'); END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'events are append only'); END;
CREATE TRIGGER scopes_no_update BEFORE UPDATE ON scopes
BEGIN SELECT RAISE(ABORT, 'scopes are append only'); END;
CREATE TRIGGER scopes_no_delete BEFORE DELETE ON scopes
BEGIN SELECT RAISE(ABORT, 'scopes are append only'); END;
CREATE TRIGGER ledger_no_update BEFORE UPDATE ON ledger
BEGIN SELECT RAISE(ABORT, 'the ledger is append only'); END;
CREATE TRIGGER ledger_no_delete BEFORE DELETE ON ledger
BEGIN SELECT RAISE(ABORT, 'the ledger is append only'); END;
";

const SCHEMA_V2: &str = "
CREATE TABLE scopes_v2 (
    id TEXT PRIMARY KEY,
    parent_id TEXT REFERENCES scopes_v2 (id),
    kind TEXT NOT NULL CHECK (kind IN ('run', 'node', 'session')),
    max_tokens INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
    max_cost_micro_usd INTEGER CHECK (max_cost_micro_usd IS NULL OR max_cost_micro_usd > 0),
    soft_percent INTEGER NOT NULL CHECK (soft_percent BETWEEN 1 AND 99),
    created_at_ms INTEGER NOT NULL
);
INSERT INTO scopes_v2 SELECT id, parent_id, kind, max_tokens, max_cost_micro_usd, soft_percent, created_at_ms FROM scopes;
DROP TABLE scopes;
ALTER TABLE scopes_v2 RENAME TO scopes;
CREATE TRIGGER scopes_no_update BEFORE UPDATE ON scopes
BEGIN SELECT RAISE(ABORT, 'scopes are append only'); END;
CREATE TRIGGER scopes_no_delete BEFORE DELETE ON scopes
BEGIN SELECT RAISE(ABORT, 'scopes are append only'); END;

CREATE TABLE workflow_runs (
    run_id TEXT PRIMARY KEY,
    spec_hash TEXT NOT NULL,
    spec TEXT NOT NULL,
    inputs TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE artifacts (
    hash TEXT PRIMARY KEY,
    bytes BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE node_results (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES workflow_runs (run_id),
    node_id TEXT NOT NULL,
    memo_key TEXT NOT NULL,
    outputs TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX node_results_by_memo ON node_results (memo_key, seq);
CREATE INDEX node_results_by_run ON node_results (run_id, seq);

CREATE TRIGGER workflow_runs_no_update BEFORE UPDATE ON workflow_runs
BEGIN SELECT RAISE(ABORT, 'workflow runs are append only'); END;
CREATE TRIGGER workflow_runs_no_delete BEFORE DELETE ON workflow_runs
BEGIN SELECT RAISE(ABORT, 'workflow runs are append only'); END;
CREATE TRIGGER artifacts_no_update BEFORE UPDATE ON artifacts
BEGIN SELECT RAISE(ABORT, 'artifacts are append only'); END;
CREATE TRIGGER artifacts_no_delete BEFORE DELETE ON artifacts
BEGIN SELECT RAISE(ABORT, 'artifacts are append only'); END;
CREATE TRIGGER node_results_no_update BEFORE UPDATE ON node_results
BEGIN SELECT RAISE(ABORT, 'node results are append only'); END;
CREATE TRIGGER node_results_no_delete BEFORE DELETE ON node_results
BEGIN SELECT RAISE(ABORT, 'node results are append only'); END;
";

static MIGRATIONS: LazyLock<Migrations<'static>> =
    LazyLock::new(|| Migrations::new(vec![M::up(SCHEMA_V1), M::up(SCHEMA_V2).foreign_key_check()]));

/// Sets the connection up and brings the schema to the latest version. A
/// database written by a newer version is refused, not guessed at.
pub(super) fn prepare(conn: &mut Connection) -> Result<(), StoreError> {
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    // Rebuilding a referenced table needs foreign keys off, and the pragma
    // cannot change inside a transaction; the migration checks them itself.
    conn.pragma_update(None, "foreign_keys", false)?;
    MIGRATIONS
        .to_latest(conn)
        .map_err(|error| StoreError::Migration(error.to_string()))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}
