//! The token ledger. Scopes form a tree (a run holds sessions); a call is
//! reserved against its session and every ancestor in one transaction, then
//! settled with what it really cost, or released if it never completed. A
//! call is reserved once and ended once, and the database enforces both.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::journal::{Event, NewEvent, insert_event};
use super::{Store, StoreError, now_ms, to_i64, to_u64};
use crate::budget::Caps;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LedgerError {
    #[error("no scope {0}")]
    UnknownScope(String),
    #[error("scope {0} already exists")]
    ScopeExists(String),
    #[error("call {0} has no reservation")]
    NoReservation(String),
    #[error("call {0} was already settled or released")]
    AlreadyFinal(String),
    #[error("call {0} already holds a reservation")]
    DuplicateReserve(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Run,
    Session,
}

impl ScopeKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewScope {
    pub id: String,
    pub parent_id: Option<String>,
    pub kind: ScopeKind,
    pub caps: Caps,
    pub soft_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReserveRequest {
    pub scope_id: String,
    pub call_id: String,
    pub tokens: u64,
    pub cost_micro_usd: u64,
    pub ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub call_id: String,
    pub tokens: u64,
    pub cost_micro_usd: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Tokens,
    Cost,
}

/// The hard cap that said no, and how close it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub scope_id: String,
    pub limit: LimitKind,
    pub cap: u64,
    pub in_use: u64,
    pub requested: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    Granted(Reservation),
    Refused(Refusal),
}

/// One scope's position against its caps. `held` is reservations that are
/// neither ended nor expired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeUsage {
    pub scope_id: String,
    pub kind: ScopeKind,
    pub caps: Caps,
    pub soft_percent: u8,
    pub spent_tokens: u64,
    pub held_tokens: u64,
    pub spent_cost_micro_usd: u64,
    pub held_cost_micro_usd: u64,
}

impl ScopeUsage {
    #[must_use]
    pub fn used_tokens(&self) -> u64 {
        self.spent_tokens.saturating_add(self.held_tokens)
    }

    #[must_use]
    pub fn used_cost_micro_usd(&self) -> u64 {
        self.spent_cost_micro_usd
            .saturating_add(self.held_cost_micro_usd)
    }
}

struct ScopeRow {
    parent_id: Option<String>,
    kind: ScopeKind,
    caps: Caps,
    soft_percent: u8,
}

fn scope_row(conn: &Connection, id: &str) -> Result<ScopeRow, StoreError> {
    let found = conn
        .query_row(
            "SELECT parent_id, kind, max_tokens, max_cost_micro_usd, soft_percent
             FROM scopes WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((parent_id, kind, max_tokens, max_cost, soft)) = found else {
        return Err(LedgerError::UnknownScope(id.to_string()).into());
    };
    let kind = match kind.as_str() {
        "run" => ScopeKind::Run,
        "session" => ScopeKind::Session,
        other => return Err(StoreError::Corrupt(format!("scope kind {other:?}"))),
    };
    Ok(ScopeRow {
        parent_id,
        kind,
        caps: Caps {
            max_tokens: max_tokens.map(to_u64).transpose()?,
            max_cost_micro_usd: max_cost.map(to_u64).transpose()?,
        },
        soft_percent: u8::try_from(soft).map_err(|_range| StoreError::Overflow)?,
    })
}

const SUBTREE: &str = "WITH RECURSIVE subtree (id) AS (
    SELECT ?1
    UNION ALL
    SELECT scopes.id FROM scopes JOIN subtree ON scopes.parent_id = subtree.id
)";

fn totals(conn: &Connection, scope_id: &str, now: i64) -> Result<[u64; 4], StoreError> {
    let sql = format!(
        "{SUBTREE}
         SELECT
           COALESCE(SUM(CASE WHEN kind IN ('settle', 'adjust') THEN tokens END), 0),
           COALESCE(SUM(CASE WHEN kind IN ('settle', 'adjust') THEN cost_micro_usd END), 0),
           COALESCE(SUM(CASE WHEN kind = 'reserve' AND expires_at_ms > ?2
             AND NOT EXISTS (SELECT 1 FROM ledger ended WHERE ended.call_id = ledger.call_id
                             AND ended.kind IN ('settle', 'release'))
             THEN tokens END), 0),
           COALESCE(SUM(CASE WHEN kind = 'reserve' AND expires_at_ms > ?2
             AND NOT EXISTS (SELECT 1 FROM ledger ended WHERE ended.call_id = ledger.call_id
                             AND ended.kind IN ('settle', 'release'))
             THEN cost_micro_usd END), 0)
         FROM ledger WHERE scope_id IN (SELECT id FROM subtree)"
    );
    let row: (i64, i64, i64, i64) = conn.query_row(&sql, params![scope_id, now], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    })?;
    Ok([
        to_u64(row.0)?,
        to_u64(row.1)?,
        to_u64(row.2)?,
        to_u64(row.3)?,
    ])
}

fn usage_of(conn: &Connection, scope_id: &str, now: i64) -> Result<Vec<ScopeUsage>, StoreError> {
    let mut chain = Vec::new();
    let mut next = Some(scope_id.to_string());
    while let Some(id) = next {
        let row = scope_row(conn, &id)?;
        let [spent_tokens, spent_cost, held_tokens, held_cost] = totals(conn, &id, now)?;
        chain.push(ScopeUsage {
            scope_id: id,
            kind: row.kind,
            caps: row.caps,
            soft_percent: row.soft_percent,
            spent_tokens,
            held_tokens,
            spent_cost_micro_usd: spent_cost,
            held_cost_micro_usd: held_cost,
        });
        next = row.parent_id;
    }
    Ok(chain)
}

struct CallState {
    reserved_in: Option<String>,
    ended: bool,
}

fn call_state(conn: &Connection, call_id: &str) -> Result<CallState, StoreError> {
    let reserved_in = conn
        .query_row(
            "SELECT scope_id FROM ledger WHERE call_id = ?1 AND kind = 'reserve'",
            params![call_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let ended: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM ledger WHERE call_id = ?1 AND kind IN ('settle', 'release'))",
        params![call_id],
        |row| row.get(0),
    )?;
    Ok(CallState { reserved_in, ended })
}

fn insert_entry(
    tx: &Transaction<'_>,
    scope_id: &str,
    kind: &str,
    call_id: &str,
    tokens: i64,
    cost: i64,
    expires_at_ms: Option<i64>,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO ledger (id, scope_id, kind, call_id, tokens, cost_micro_usd, expires_at_ms, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            ulid::Ulid::generate().to_string(),
            scope_id,
            kind,
            call_id,
            tokens,
            cost,
            expires_at_ms,
            now_ms()?
        ],
    )?;
    Ok(())
}

fn end_call(
    conn: &mut Connection,
    call_id: &str,
    kind: &str,
    tokens: u64,
    cost: u64,
    event: Option<&NewEvent>,
) -> Result<Option<Event>, StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let state = call_state(&tx, call_id)?;
    let Some(scope_id) = state.reserved_in else {
        return Err(LedgerError::NoReservation(call_id.to_string()).into());
    };
    if state.ended {
        return Err(LedgerError::AlreadyFinal(call_id.to_string()).into());
    }
    insert_entry(
        &tx,
        &scope_id,
        kind,
        call_id,
        to_i64(tokens)?,
        to_i64(cost)?,
        None,
    )?;
    let stored = event.map(|event| insert_event(&tx, event)).transpose()?;
    tx.commit()?;
    Ok(stored)
}

impl Store {
    /// # Errors
    /// [`LedgerError::ScopeExists`] for a repeated id and
    /// [`LedgerError::UnknownScope`] for a missing parent.
    pub async fn open_scope(&self, scope: NewScope) -> Result<(), StoreError> {
        self.with(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM scopes WHERE id = ?1)",
                params![scope.id],
                |row| row.get(0),
            )?;
            if exists {
                return Err(LedgerError::ScopeExists(scope.id.clone()).into());
            }
            if let Some(parent) = &scope.parent_id {
                scope_row(&tx, parent)?;
            }
            tx.execute(
                "INSERT INTO scopes (id, parent_id, kind, max_tokens, max_cost_micro_usd, soft_percent, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    scope.id,
                    scope.parent_id,
                    scope.kind.as_str(),
                    scope.caps.max_tokens.map(to_i64).transpose()?,
                    scope.caps.max_cost_micro_usd.map(to_i64).transpose()?,
                    i64::from(scope.soft_percent),
                    now_ms()?
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// Holds `tokens` and `cost` against the scope and every ancestor, or
    /// says which hard cap refused. Checked and written in one transaction,
    /// so two callers can never both take the last of a cap.
    ///
    /// # Errors
    /// [`LedgerError`] for an unknown scope or a call that already holds a
    /// reservation.
    pub async fn reserve(&self, request: ReserveRequest) -> Result<ReserveOutcome, StoreError> {
        self.with(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let now = now_ms()?;
            if call_state(&tx, &request.call_id)?.reserved_in.is_some() {
                return Err(LedgerError::DuplicateReserve(request.call_id.clone()).into());
            }
            for scope in usage_of(&tx, &request.scope_id, now)? {
                let checks = [
                    (
                        LimitKind::Tokens,
                        scope.caps.max_tokens,
                        scope.used_tokens(),
                        request.tokens,
                    ),
                    (
                        LimitKind::Cost,
                        scope.caps.max_cost_micro_usd,
                        scope.used_cost_micro_usd(),
                        request.cost_micro_usd,
                    ),
                ];
                for (limit, cap, in_use, requested) in checks {
                    if let Some(cap) = cap
                        && in_use.saturating_add(requested) > cap
                    {
                        return Ok(ReserveOutcome::Refused(Refusal {
                            scope_id: scope.scope_id,
                            limit,
                            cap,
                            in_use,
                            requested,
                        }));
                    }
                }
            }
            let expires = now
                .checked_add(to_i64(request.ttl_ms)?)
                .ok_or(StoreError::Overflow)?;
            insert_entry(
                &tx,
                &request.scope_id,
                "reserve",
                &request.call_id,
                to_i64(request.tokens)?,
                to_i64(request.cost_micro_usd)?,
                Some(expires),
            )?;
            tx.commit()?;
            Ok(ReserveOutcome::Granted(Reservation {
                call_id: request.call_id,
                tokens: request.tokens,
                cost_micro_usd: request.cost_micro_usd,
            }))
        })
        .await
    }

    /// Ends a reservation with what the call really cost, and journals
    /// `event` in the same transaction when one is given.
    ///
    /// # Errors
    /// [`LedgerError::NoReservation`] when the call never reserved and
    /// [`LedgerError::AlreadyFinal`] when it was already settled or released.
    pub async fn settle(
        &self,
        call_id: &str,
        tokens: u64,
        cost_micro_usd: u64,
        event: Option<NewEvent>,
    ) -> Result<Option<Event>, StoreError> {
        let call_id = call_id.to_string();
        self.with(move |conn| {
            end_call(
                conn,
                &call_id,
                "settle",
                tokens,
                cost_micro_usd,
                event.as_ref(),
            )
        })
        .await
    }

    /// Frees a reservation without spending it, journaling `event` with it.
    ///
    /// # Errors
    /// The same as [`Store::settle`].
    pub async fn release(
        &self,
        call_id: &str,
        event: Option<NewEvent>,
    ) -> Result<Option<Event>, StoreError> {
        let call_id = call_id.to_string();
        self.with(move |conn| end_call(conn, &call_id, "release", 0, 0, event.as_ref()))
            .await
    }

    /// A signed correction to what a scope has spent.
    ///
    /// # Errors
    /// [`LedgerError::UnknownScope`] for a missing scope.
    pub async fn adjust(
        &self,
        scope_id: &str,
        tokens: i64,
        cost_micro_usd: i64,
    ) -> Result<(), StoreError> {
        let scope_id = scope_id.to_string();
        self.with(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            scope_row(&tx, &scope_id)?;
            let call_id = format!("adjust-{}", ulid::Ulid::generate());
            insert_entry(
                &tx,
                &scope_id,
                "adjust",
                &call_id,
                tokens,
                cost_micro_usd,
                None,
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    /// A scope and each of its ancestors with their spend and holds, the
    /// scope itself first.
    ///
    /// # Errors
    /// [`LedgerError::UnknownScope`] for a missing scope.
    pub async fn usage(&self, scope_id: &str) -> Result<Vec<ScopeUsage>, StoreError> {
        let scope_id = scope_id.to_string();
        self.with(move |conn| usage_of(conn, &scope_id, now_ms()?))
            .await
    }

    /// Reservations that are still open: the calls a crash left in flight.
    ///
    /// # Errors
    /// [`StoreError`] when the read fails.
    pub async fn open_reservations(&self, scope_id: &str) -> Result<Vec<String>, StoreError> {
        let scope_id = scope_id.to_string();
        self.with(move |conn| {
            let mut statement = conn.prepare(
                "SELECT call_id FROM ledger
                 WHERE scope_id = ?1 AND kind = 'reserve'
                   AND NOT EXISTS (SELECT 1 FROM ledger ended WHERE ended.call_id = ledger.call_id
                                   AND ended.kind IN ('settle', 'release'))
                 ORDER BY seq",
            )?;
            let rows = statement.query_map(params![scope_id], |row| row.get::<_, String>(0))?;
            rows.map(|row| row.map_err(StoreError::from)).collect()
        })
        .await
    }
}
