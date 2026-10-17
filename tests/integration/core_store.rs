use std::path::PathBuf;
use std::sync::Arc;

use jensen_harness_core::budget::Caps;
use jensen_harness_core::store::{
    EventKind, LedgerError, LimitKind, NewEvent, NewScope, ReserveOutcome, ReserveRequest,
    ScopeKind, Store, StoreError,
};
use serde_json::json;
use tempfile::TempDir;

fn on_disk() -> (TempDir, PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state").join("state.sqlite");
    let store = Store::open(&path).unwrap();
    (dir, path, store)
}

fn scope(id: &str, parent: Option<&str>, kind: ScopeKind, max_tokens: Option<u64>) -> NewScope {
    NewScope {
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        kind,
        caps: Caps {
            max_tokens,
            max_cost_micro_usd: None,
        },
        soft_percent: 80,
    }
}

fn event(session: &str, kind: EventKind, payload: serde_json::Value) -> NewEvent {
    NewEvent {
        session_id: session.to_string(),
        parent_id: None,
        kind,
        payload,
    }
}

fn reserve(scope: &str, call: &str, tokens: u64) -> ReserveRequest {
    ReserveRequest {
        scope_id: scope.to_string(),
        call_id: call.to_string(),
        tokens,
        cost_micro_usd: tokens * 2,
        ttl_ms: 60_000,
    }
}

async fn granted(store: &Store, request: ReserveRequest) {
    let outcome = store.reserve(request).await.unwrap();
    assert!(matches!(outcome, ReserveOutcome::Granted(_)), "{outcome:?}");
}

#[tokio::test]
async fn events_come_back_in_order_and_only_for_their_session() {
    let store = Store::in_memory().unwrap();
    let first = store
        .append(event(
            "s1",
            EventKind::PromptStarted,
            json!({"message": "hi"}),
        ))
        .await
        .unwrap();
    store
        .append(event(
            "s2",
            EventKind::PromptStarted,
            json!({"message": "other"}),
        ))
        .await
        .unwrap();
    store
        .append(NewEvent {
            parent_id: Some(first.id.clone()),
            ..event("s1", EventKind::ModelResponded, json!({"n": 1}))
        })
        .await
        .unwrap();

    let events = store.events("s1").await.unwrap();

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, EventKind::PromptStarted);
    assert_eq!(events[0].payload, json!({"message": "hi"}));
    assert_eq!(events[1].parent_id.as_deref(), Some(first.id.as_str()));
    assert!(events[0].seq < events[1].seq);
    assert_eq!(store.session_ids().await.unwrap(), ["s1", "s2"]);
}

#[tokio::test]
async fn the_database_itself_refuses_to_change_or_remove_history() {
    let (_dir, path, store) = on_disk();
    store
        .append(event("s", EventKind::PromptStarted, json!({})))
        .await
        .unwrap();
    store
        .open_scope(scope("run", None, ScopeKind::Run, Some(10)))
        .await
        .unwrap();
    granted(&store, reserve("run", "c1", 5)).await;

    let raw = rusqlite::Connection::open(&path).unwrap();
    for statement in [
        "UPDATE events SET payload = '{}'",
        "DELETE FROM events",
        "UPDATE scopes SET max_tokens = 999999",
        "DELETE FROM scopes",
        "UPDATE ledger SET tokens = 0",
        "DELETE FROM ledger",
    ] {
        let error = raw.execute(statement, []).unwrap_err().to_string();
        assert!(error.contains("append only"), "{statement}: {error}");
    }
}

#[tokio::test]
async fn an_event_kind_this_version_does_not_know_is_corrupt_not_skipped() {
    let (_dir, path, store) = on_disk();
    store
        .append(event("s", EventKind::PromptStarted, json!({})))
        .await
        .unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute(
        "INSERT INTO events (id, session_id, kind, payload, created_at_ms)
         VALUES ('x', 's', 'from.the.future', '{}', 0)",
        [],
    )
    .unwrap();

    let error = store.events("s").await.unwrap_err();

    assert!(matches!(error, StoreError::Corrupt(_)), "{error:?}");
}

#[tokio::test]
async fn a_call_is_settled_once_and_never_after_a_release() {
    let store = Store::in_memory().unwrap();
    store
        .open_scope(scope("s", None, ScopeKind::Session, Some(1_000)))
        .await
        .unwrap();
    granted(&store, reserve("s", "c1", 100)).await;
    granted(&store, reserve("s", "c2", 100)).await;

    store.settle("c1", 80, 160, None).await.unwrap();
    let again = store.settle("c1", 80, 160, None).await.unwrap_err();
    assert!(
        matches!(again, StoreError::Ledger(LedgerError::AlreadyFinal(_))),
        "{again:?}"
    );

    store.release("c2", None).await.unwrap();
    let after_release = store.settle("c2", 50, 100, None).await.unwrap_err();
    assert!(matches!(
        after_release,
        StoreError::Ledger(LedgerError::AlreadyFinal(_))
    ));

    let never_reserved = store.settle("ghost", 1, 1, None).await.unwrap_err();
    assert!(matches!(
        never_reserved,
        StoreError::Ledger(LedgerError::NoReservation(_))
    ));
    let twice = store.reserve(reserve("s", "c1", 5)).await.unwrap_err();
    assert!(matches!(
        twice,
        StoreError::Ledger(LedgerError::DuplicateReserve(_))
    ));

    let usage = store.usage("s").await.unwrap();
    assert_eq!(usage[0].spent_tokens, 80);
    assert_eq!(usage[0].held_tokens, 0);
}

#[tokio::test]
async fn the_response_event_and_its_settlement_land_together_or_not_at_all() {
    let store = Store::in_memory().unwrap();
    store
        .open_scope(scope("s", None, ScopeKind::Session, None))
        .await
        .unwrap();
    granted(&store, reserve("s", "c1", 10)).await;
    let responded = || Some(event("s", EventKind::ModelResponded, json!({"call": "c1"})));

    let stored = store.settle("c1", 7, 14, responded()).await.unwrap();
    assert!(stored.is_some());
    let refused = store.settle("c1", 7, 14, responded()).await;

    assert!(refused.is_err());
    assert_eq!(store.events("s").await.unwrap().len(), 1);
}

#[tokio::test]
async fn holds_spend_and_adjustments_add_up_and_exact_fit_is_allowed() {
    let store = Store::in_memory().unwrap();
    store
        .open_scope(scope("s", None, ScopeKind::Session, Some(1_000)))
        .await
        .unwrap();
    granted(&store, reserve("s", "c1", 400)).await;
    granted(&store, reserve("s", "c2", 600)).await;
    let usage = store.usage("s").await.unwrap();
    assert_eq!((usage[0].held_tokens, usage[0].spent_tokens), (1_000, 0));

    let refused = store.reserve(reserve("s", "c3", 1)).await.unwrap();
    let ReserveOutcome::Refused(refusal) = refused else {
        panic!("expected a refusal");
    };
    assert_eq!(
        (
            refusal.limit,
            refusal.cap,
            refusal.in_use,
            refusal.requested
        ),
        (LimitKind::Tokens, 1_000, 1_000, 1)
    );

    store.settle("c1", 300, 600, None).await.unwrap();
    store.adjust("s", -50, -100).await.unwrap();
    let usage = store.usage("s").await.unwrap();
    assert_eq!((usage[0].spent_tokens, usage[0].held_tokens), (250, 600));
    assert_eq!(usage[0].spent_cost_micro_usd, 500);
}

#[tokio::test]
async fn a_run_cap_is_shared_by_its_sessions() {
    let store = Store::in_memory().unwrap();
    store
        .open_scope(scope("run", None, ScopeKind::Run, Some(1_000)))
        .await
        .unwrap();
    for session in ["a", "b"] {
        store
            .open_scope(scope(session, Some("run"), ScopeKind::Session, Some(900)))
            .await
            .unwrap();
    }
    granted(&store, reserve("a", "ca", 600)).await;

    let outcome = store.reserve(reserve("b", "cb", 500)).await.unwrap();

    let ReserveOutcome::Refused(refusal) = outcome else {
        panic!("expected the run cap to refuse");
    };
    assert_eq!(refusal.scope_id, "run");
    let usage = store.usage("b").await.unwrap();
    assert_eq!(
        usage
            .iter()
            .map(|u| u.scope_id.as_str())
            .collect::<Vec<_>>(),
        ["b", "run"]
    );
    assert_eq!(usage[1].held_tokens, 600);
}

#[tokio::test]
async fn a_cost_cap_refuses_on_cost_even_when_tokens_fit() {
    let store = Store::in_memory().unwrap();
    store
        .open_scope(NewScope {
            caps: Caps {
                max_tokens: None,
                max_cost_micro_usd: Some(100),
            },
            ..scope("s", None, ScopeKind::Session, None)
        })
        .await
        .unwrap();
    let outcome = store.reserve(reserve("s", "c1", 60)).await.unwrap();
    let ReserveOutcome::Refused(refusal) = outcome else {
        panic!("expected a refusal");
    };
    assert_eq!(refusal.limit, LimitKind::Cost);
}

#[tokio::test]
async fn an_expired_hold_stops_counting_and_a_dangling_one_is_listed() {
    let store = Store::in_memory().unwrap();
    store
        .open_scope(scope("s", None, ScopeKind::Session, Some(100)))
        .await
        .unwrap();
    granted(
        &store,
        ReserveRequest {
            ttl_ms: 0,
            ..reserve("s", "stale", 90)
        },
    )
    .await;
    granted(&store, reserve("s", "live", 90)).await;

    let usage = store.usage("s").await.unwrap();
    assert_eq!(usage[0].held_tokens, 90);
    assert_eq!(
        store.open_reservations("s").await.unwrap(),
        ["stale", "live"]
    );

    store.release("stale", None).await.unwrap();
    assert_eq!(store.open_reservations("s").await.unwrap(), ["live"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_callers_racing_for_the_last_of_a_cap_cannot_both_have_it() {
    let store = Arc::new(Store::in_memory().unwrap());
    store
        .open_scope(scope("s", None, ScopeKind::Session, Some(1_000)))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for index in 0..8 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            store
                .reserve(reserve("s", &format!("c{index}"), 300))
                .await
                .unwrap()
        }));
    }
    let mut granted_count = 0;
    for task in tasks {
        if matches!(task.await.unwrap(), ReserveOutcome::Granted(_)) {
            granted_count += 1;
        }
    }
    assert_eq!(granted_count, 3);
}

#[tokio::test]
async fn scopes_must_be_new_and_their_parents_must_exist() {
    let store = Store::in_memory().unwrap();
    store
        .open_scope(scope("s", None, ScopeKind::Session, None))
        .await
        .unwrap();
    let again = store
        .open_scope(scope("s", None, ScopeKind::Session, None))
        .await
        .unwrap_err();
    assert!(matches!(
        again,
        StoreError::Ledger(LedgerError::ScopeExists(_))
    ));
    let orphan = store
        .open_scope(scope("o", Some("nope"), ScopeKind::Session, None))
        .await
        .unwrap_err();
    assert!(matches!(
        orphan,
        StoreError::Ledger(LedgerError::UnknownScope(_))
    ));
    let missing = store.usage("nope").await.unwrap_err();
    assert!(matches!(
        missing,
        StoreError::Ledger(LedgerError::UnknownScope(_))
    ));
}

#[tokio::test]
async fn the_file_is_private_survives_a_reopen_and_a_newer_schema_is_refused() {
    let (_dir, path, store) = on_disk();
    store
        .append(event("s", EventKind::PromptStarted, json!({"n": 1})))
        .await
        .unwrap();
    drop(store);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.events("s").await.unwrap().len(), 1);
    drop(reopened);

    rusqlite::Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", 9_999)
        .unwrap();
    let error = Store::open(&path).unwrap_err();
    assert!(matches!(error, StoreError::Migration(_)), "{error:?}");
}
