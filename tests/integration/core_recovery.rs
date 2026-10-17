use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::common::{
    Scripted, calls_tool, default_budget, new_session, path_input, reply, says, session_config,
    session_over, tool_set, tool_use,
};
use async_trait::async_trait;
use jensen_harness_core::agent::{AgentError, PromptStop, Recovered, Session};
use jensen_harness_core::providers::{
    CompletionRequest, CompletionResponse, ContentBlock, Provider, ProviderError, StopReason, Turn,
};
use jensen_harness_core::store::{Event, EventKind, NewEvent, NewScope, ScopeKind, Store};
use serde_json::json;
use std::fs;
use tempfile::tempdir;

struct Finished {
    store: Store,
    session_id: String,
    events: Vec<Event>,
    turns: Vec<Turn>,
}

async fn run_whole(dir: &Path, script: Vec<CompletionResponse>) -> (Finished, Scripted) {
    let store = Store::in_memory().unwrap();
    let mut session = session_over(
        store.clone(),
        tool_set(dir),
        session_config(20, default_budget()),
    )
    .await;
    let provider = Scripted::of(script);
    let outcome = session.prompt(&provider, "do the work").await.unwrap();
    assert_eq!(outcome.stop, PromptStop::EndTurn);
    let session_id = session.session_id().to_string();
    let events = store.events(&session_id).await.unwrap();
    (
        Finished {
            store,
            session_id,
            events,
            turns: session.turns().to_vec(),
        },
        provider,
    )
}

/// A fresh database holding only the first `count` journal events of a
/// finished run: the journal as a crash at that point would have left it.
async fn journal_prefix(finished: &Finished, count: usize) -> Store {
    let store = Store::in_memory().unwrap();
    let chain = finished.store.usage(&finished.session_id).await.unwrap();
    let (session_scope, run_scope) = (&chain[0], &chain[1]);
    store
        .open_scope(NewScope {
            id: run_scope.scope_id.clone(),
            parent_id: None,
            kind: ScopeKind::Run,
            caps: run_scope.caps,
            soft_percent: run_scope.soft_percent,
        })
        .await
        .unwrap();
    store
        .open_scope(NewScope {
            id: session_scope.scope_id.clone(),
            parent_id: Some(run_scope.scope_id.clone()),
            kind: ScopeKind::Session,
            caps: session_scope.caps,
            soft_percent: session_scope.soft_percent,
        })
        .await
        .unwrap();
    for event in finished.events.iter().take(count) {
        store
            .append(NewEvent {
                session_id: event.session_id.clone(),
                parent_id: event.parent_id.clone(),
                kind: event.kind,
                payload: event.payload.clone(),
            })
            .await
            .unwrap();
    }
    store
}

fn responded(events: &[Event], count: usize) -> usize {
    events
        .iter()
        .take(count)
        .filter(|event| event.kind == EventKind::ModelResponded)
        .count()
}

fn reads_script() -> Vec<CompletionResponse> {
    vec![
        calls_tool("t1", "read", path_input("a.txt")),
        reply(
            vec![
                tool_use("t2", "read", path_input("b.txt")),
                tool_use("t3", "grep", json!({"pattern": "alpha"})),
            ],
            StopReason::ToolUse,
        ),
        says("done"),
    ]
}

#[tokio::test]
async fn resuming_from_every_journal_boundary_gives_the_model_the_same_transcript() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "alpha one").unwrap();
    fs::write(dir.path().join("b.txt"), "beta two").unwrap();
    let (finished, original) = run_whole(dir.path(), reads_script()).await;
    let total = finished.events.len();
    assert!(
        total >= 12,
        "a realistic run journals many boundaries, got {total}"
    );

    for count in 2..total {
        let already = responded(&finished.events, count);
        let store = journal_prefix(&finished, count).await;
        let recovered = Recovered::load(&store, &finished.session_id).await.unwrap();
        assert!(recovered.has_open_prompt() || count < 2, "boundary {count}");
        let mut session = Session::restore(store.clone(), recovered, tool_set(dir.path()))
            .await
            .unwrap();
        let provider = Scripted::of(reads_script().into_iter().skip(already).collect());

        let outcome = session.resume(&provider).await.unwrap();

        assert_eq!(outcome.stop, PromptStop::EndTurn, "boundary {count}");
        assert_eq!(outcome.model_calls, 3, "boundary {count}");
        assert_eq!(
            session.turns(),
            finished.turns.as_slice(),
            "boundary {count}"
        );
        assert_eq!(provider.calls(), 3 - already, "boundary {count}");
        if provider.calls() > 0 {
            assert_eq!(
                provider.request(0).turns,
                original.request(already).turns,
                "the first resumed call must see exactly what the original saw, boundary {count}"
            );
        }
        let events = store.events(&finished.session_id).await.unwrap();
        assert_eq!(
            events.last().unwrap().kind,
            EventKind::PromptFinished,
            "boundary {count}"
        );
    }
}

#[tokio::test]
async fn a_finished_prompt_has_nothing_to_resume_and_the_session_takes_a_new_prompt() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "alpha").unwrap();
    let (finished, _) = run_whole(
        dir.path(),
        vec![calls_tool("t", "read", path_input("a.txt")), says("done")],
    )
    .await;
    let recovered = Recovered::load(&finished.store, &finished.session_id)
        .await
        .unwrap();
    assert!(!recovered.has_open_prompt());
    let mut session = Session::restore(finished.store.clone(), recovered, tool_set(dir.path()))
        .await
        .unwrap();
    let provider = Scripted::of(vec![says("again")]);

    let nothing = session.resume(&provider).await.unwrap_err();
    assert!(matches!(nothing, AgentError::NothingToResume));
    let outcome = session.prompt(&provider, "second prompt").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::EndTurn);
    assert_eq!(session.turns().len(), finished.turns.len() + 2);
}

fn edit_script() -> Vec<CompletionResponse> {
    vec![
        calls_tool(
            "e1",
            "edit",
            json!({"path": "a.txt", "find": "hello", "replace": "bye"}),
        ),
        says("done"),
    ]
}

#[tokio::test]
async fn an_edit_that_was_started_and_never_finished_is_reported_not_repeated() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello world").unwrap();
    let (finished, _) = run_whole(dir.path(), edit_script()).await;
    let started_at = finished
        .events
        .iter()
        .position(|event| event.kind == EventKind::ToolStarted)
        .unwrap()
        + 1;
    fs::write(dir.path().join("a.txt"), "hello world").unwrap();
    let store = journal_prefix(&finished, started_at).await;
    let recovered = Recovered::load(&store, &finished.session_id).await.unwrap();
    let mut session = Session::restore(store, recovered, tool_set(dir.path()))
        .await
        .unwrap();
    let provider = Scripted::of(vec![says("I checked the file")]);

    let outcome = session.resume(&provider).await.unwrap();

    assert_eq!(outcome.stop, PromptStop::EndTurn);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "hello world",
        "the edit must not run again"
    );
    let results = &provider.request(0).turns[2];
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &results.content[0]
    else {
        panic!("expected a tool result");
    };
    assert!(*is_error);
    assert!(content.contains("interrupted by a restart"), "{content}");
}

#[tokio::test]
async fn an_edit_that_finished_before_the_crash_is_not_run_again_and_its_result_is_kept() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello world").unwrap();
    let (finished, _) = run_whole(dir.path(), edit_script()).await;
    let finished_at = finished
        .events
        .iter()
        .position(|event| event.kind == EventKind::ToolFinished)
        .unwrap()
        + 1;
    let store = journal_prefix(&finished, finished_at).await;
    let recovered = Recovered::load(&store, &finished.session_id).await.unwrap();
    let mut session = Session::restore(store, recovered, tool_set(dir.path()))
        .await
        .unwrap();
    let provider = Scripted::of(vec![says("done")]);

    session.resume(&provider).await.unwrap();

    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "bye world"
    );
    assert_eq!(session.turns(), finished.turns.as_slice());
}

#[derive(Debug)]
struct Gate {
    release: tokio::sync::Notify,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl Provider for Gate {
    fn id(&self) -> &str {
        "gate"
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(says("never delivered"))
    }
}

#[tokio::test]
async fn a_model_call_in_flight_at_the_crash_is_released_not_charged_and_not_double_counted() {
    let dir = tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let session = session_over(
        store.clone(),
        tool_set(dir.path()),
        session_config(20, default_budget()),
    )
    .await;
    let session_id = session.session_id().to_string();
    let gate = Arc::new(Gate {
        release: tokio::sync::Notify::new(),
        entered: tokio::sync::Notify::new(),
    });
    let running = {
        let gate = Arc::clone(&gate);
        let mut session = session;
        tokio::spawn(async move { session.prompt(gate.as_ref(), "go").await })
    };
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(store.open_reservations(&session_id).await.unwrap().len(), 1);

    running.abort();
    let _ = running.await;
    assert_eq!(
        store.open_reservations(&session_id).await.unwrap().len(),
        1,
        "a crash leaves the hold"
    );

    let recovered = Recovered::load(&store, &session_id).await.unwrap();
    assert!(recovered.has_open_prompt());
    let mut restored = Session::restore(store.clone(), recovered, tool_set(dir.path()))
        .await
        .unwrap();
    assert!(
        store
            .open_reservations(&session_id)
            .await
            .unwrap()
            .is_empty()
    );
    let provider = Scripted::of(vec![says("finished after the restart")]);
    let outcome = restored.resume(&provider).await.unwrap();

    assert_eq!(
        (outcome.stop, outcome.model_calls),
        (PromptStop::EndTurn, 1)
    );
    let usage = store.usage(&session_id).await.unwrap();
    assert_eq!(
        usage[0].spent_tokens, 116,
        "only the call that completed is charged"
    );
    assert_eq!(usage[0].held_tokens, 0);
    let events = store.events(&session_id).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::ModelResponded)
            .count(),
        1
    );
    assert!(
        events.iter().any(|e| e.kind == EventKind::ModelFailed
            && e.payload["error"] == "interrupted by a restart")
    );
}

#[tokio::test]
async fn killing_the_loop_while_a_shell_command_runs_leaves_it_reported_as_interrupted() {
    let dir = tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let mut session = session_over(
        store.clone(),
        tool_set(dir.path()),
        session_config(20, default_budget()),
    )
    .await;
    let session_id = session.session_id().to_string();
    let running = tokio::spawn(async move {
        let provider = Scripted::of(vec![calls_tool(
            "s1",
            "shell",
            json!({"command": "sleep 3"}),
        )]);
        session.prompt(&provider, "go").await
    });
    let mut waited = 0;
    while !store
        .events(&session_id)
        .await
        .unwrap()
        .iter()
        .any(|e| e.kind == EventKind::ToolStarted)
    {
        tokio::time::sleep(Duration::from_millis(20)).await;
        waited += 1;
        assert!(waited < 250, "the shell tool never started");
    }
    running.abort();
    let _ = running.await;

    let recovered = Recovered::load(&store, &session_id).await.unwrap();
    let mut restored = Session::restore(store.clone(), recovered, tool_set(dir.path()))
        .await
        .unwrap();
    let provider = Scripted::of(vec![says("noted")]);
    restored.resume(&provider).await.unwrap();

    let results = &provider.request(0).turns[2];
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &results.content[0]
    else {
        panic!("expected a tool result");
    };
    assert!(
        *is_error && content.contains("interrupted by a restart"),
        "{content}"
    );
}

#[tokio::test]
async fn a_new_prompt_is_refused_while_an_interrupted_one_is_open() {
    let dir = tempdir().unwrap();
    let mut session = new_session(dir.path(), 5).await;
    let failing = Scripted::new(vec![Err(ProviderError::Upstream {
        provider: "scripted".into(),
        message: "503".into(),
    })]);
    session.prompt(&failing, "first").await.unwrap_err();
    assert!(session.has_open_prompt());

    let blocked = session
        .prompt(&Scripted::of(vec![says("x")]), "second")
        .await
        .unwrap_err();
    assert!(matches!(blocked, AgentError::PromptOpen));

    let resumed = session
        .resume(&Scripted::of(vec![says("ok")]))
        .await
        .unwrap();
    assert_eq!(resumed.stop, PromptStop::EndTurn);
    assert!(!session.has_open_prompt());
}

#[tokio::test]
async fn a_budget_stop_closes_the_prompt_in_the_journal() {
    let dir = tempdir().unwrap();
    let budget = jensen_harness_core::budget::SessionBudget {
        caps: jensen_harness_core::budget::Caps {
            max_tokens: Some(100),
            max_cost_micro_usd: None,
        },
        soft_percent: 80,
        price: None,
    };
    let store = Store::in_memory().unwrap();
    let mut session = session_over(
        store.clone(),
        tool_set(dir.path()),
        session_config(5, budget),
    )
    .await;
    let outcome = session.prompt(&Scripted::of(vec![]), "go").await.unwrap();
    assert_eq!(outcome.stop, PromptStop::BudgetExhausted);

    let recovered = Recovered::load(&store, session.session_id()).await.unwrap();

    assert!(!recovered.has_open_prompt());
}

#[tokio::test]
async fn a_journal_that_cannot_be_replayed_is_an_error_naming_the_event() {
    let store = Store::in_memory().unwrap();
    let missing = Recovered::load(&store, "nobody").await.err().unwrap();
    assert!(matches!(missing, AgentError::UnknownSession(_)));

    store
        .append(NewEvent {
            session_id: "s".to_string(),
            parent_id: None,
            kind: EventKind::PromptStarted,
            payload: json!({"message": "orphan"}),
        })
        .await
        .unwrap();
    let no_creation = Recovered::load(&store, "s").await.err().unwrap();
    assert!(
        matches!(no_creation, AgentError::Journal(_)),
        "{no_creation:?}"
    );

    store
        .append(NewEvent {
            session_id: "t".to_string(),
            parent_id: None,
            kind: EventKind::SessionCreated,
            payload: json!({"wrong": "shape"}),
        })
        .await
        .unwrap();
    let malformed = Recovered::load(&store, "t").await.err().unwrap();
    let AgentError::Journal(message) = malformed else {
        panic!("expected a journal error");
    };
    assert!(message.contains("session.created"), "{message}");
}

fn handle_in(text: &str) -> String {
    let after = text.split("expand ").nth(1).unwrap();
    after.split(']').next().unwrap().to_string()
}

#[tokio::test]
async fn an_expand_handle_from_before_a_restore_is_refused_and_never_names_new_output() {
    let dir = tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let mut first = session_over(
        store.clone(),
        tool_set(dir.path()),
        session_config(20, default_budget()),
    )
    .await;
    let provider = Scripted::of(vec![
        calls_tool("s1", "shell", json!({"command": "seq 1 500"})),
        says("done"),
    ]);
    first.prompt(&provider, "list numbers").await.unwrap();
    let ContentBlock::ToolResult { content, .. } = &first.turns()[2].content[0] else {
        panic!("expected a tool result");
    };
    let old_handle = handle_in(content);
    let session_id = first.session_id().to_string();

    let recovered = Recovered::load(&store, &session_id).await.unwrap();
    let mut restored = Session::restore(store, recovered, tool_set(dir.path()))
        .await
        .unwrap();
    let provider = Scripted::of(vec![
        reply(
            vec![
                tool_use("s2", "shell", json!({"command": "seq 1000 1400"})),
                tool_use("x1", "expand", json!({"handle": old_handle})),
            ],
            StopReason::ToolUse,
        ),
        says("done"),
    ]);
    restored
        .prompt(&provider, "list more numbers")
        .await
        .unwrap();

    let second_request = provider.request(1);
    let results = &second_request.turns.last().unwrap().content;
    let ContentBlock::ToolResult {
        content: new_output,
        ..
    } = &results[0]
    else {
        panic!("expected the shell result");
    };
    let new_handle = handle_in(new_output);
    assert_ne!(
        new_handle, old_handle,
        "a restored session must not reuse a handle"
    );
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &results[1]
    else {
        panic!("expected the expand result");
    };
    assert!(*is_error, "{content}");
    assert!(
        content.contains("not a handle this session issued"),
        "{content}"
    );
    assert!(
        !content.contains("1400"),
        "the old handle must not read the new output"
    );
}
