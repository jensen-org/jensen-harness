use crate::common::{
    Scripted, calls_tool, costing, default_budget, path_input, says, session_config, session_over,
    tool_set,
};
use jensen_harness_core::agent::{AgentError, PromptStop};
use jensen_harness_core::budget::{Caps, Price, SessionBudget};
use jensen_harness_core::providers::ProviderError;
use jensen_harness_core::store::{EventKind, NewScope, ScopeKind, Store};
use std::fs;
use tempfile::tempdir;

fn budget(caps: Caps, soft_percent: u8, price: Option<Price>) -> SessionBudget {
    SessionBudget {
        caps,
        soft_percent,
        price,
    }
}

fn tokens_cap(max: u64) -> Caps {
    Caps {
        max_tokens: Some(max),
        max_cost_micro_usd: None,
    }
}

fn a_read() -> jensen_harness_core::providers::CompletionResponse {
    calls_tool("t", "read", path_input("a.txt"))
}

#[tokio::test]
async fn a_hard_cap_stops_the_prompt_before_the_call_that_would_cross_it() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    let provider = Scripted::repeating(costing(6_000, a_read()));
    let store = Store::in_memory().unwrap();
    let mut session = session_over(
        store.clone(),
        tool_set(dir.path()),
        session_config(20, budget(tokens_cap(12_000), 80, None)),
    )
    .await;

    let outcome = session.prompt(&provider, "go").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::BudgetExhausted);
    assert_eq!(outcome.model_calls, 2);
    assert_eq!(provider.calls(), 2);
    let events = store.events(session.session_id()).await.unwrap();
    let exhausted = events
        .iter()
        .find(|event| event.kind == EventKind::BudgetExhausted)
        .unwrap();
    assert_eq!(exhausted.payload["limit"], "Tokens");
    assert_eq!(exhausted.payload["cap"], 12_000);
    let usage = store.usage(session.session_id()).await.unwrap();
    assert_eq!((usage[0].spent_tokens, usage[0].held_tokens), (12_000, 0));
}

#[tokio::test]
async fn a_request_larger_than_the_cap_never_reaches_the_provider() {
    let dir = tempdir().unwrap();
    let provider = Scripted::of(vec![says("never")]);
    let mut session = session_over(
        Store::in_memory().unwrap(),
        tool_set(dir.path()),
        session_config(20, budget(tokens_cap(100), 80, None)),
    )
    .await;

    let outcome = session.prompt(&provider, "go").await.unwrap();

    assert_eq!(
        (outcome.stop, outcome.model_calls),
        (PromptStop::BudgetExhausted, 0)
    );
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn a_cost_cap_refuses_on_the_estimated_cost() {
    let dir = tempdir().unwrap();
    let price = Price {
        input: 3_000_000,
        output: 15_000_000,
        cache_read: 300_000,
        cache_write: 3_750_000,
    };
    let caps = Caps {
        max_tokens: None,
        max_cost_micro_usd: Some(100),
    };
    let provider = Scripted::of(vec![says("never")]);
    let mut session = session_over(
        Store::in_memory().unwrap(),
        tool_set(dir.path()),
        session_config(20, budget(caps, 80, Some(price))),
    )
    .await;

    let outcome = session.prompt(&provider, "go").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::BudgetExhausted);
    assert_eq!(provider.calls(), 0);
}

#[tokio::test]
async fn the_cost_of_each_call_is_priced_journaled_and_summed() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    let price = Price {
        input: 1_000_000,
        output: 0,
        cache_read: 0,
        cache_write: 0,
    };
    let provider = Scripted::of(vec![costing(1_500, a_read()), costing(500, says("done"))]);
    let store = Store::in_memory().unwrap();
    let mut session = session_over(
        store.clone(),
        tool_set(dir.path()),
        session_config(20, budget(tokens_cap(1_000_000), 80, Some(price))),
    )
    .await;

    let outcome = session.prompt(&provider, "go").await.unwrap();

    assert_eq!(outcome.cost_micro_usd, 2_000);
    let events = store.events(session.session_id()).await.unwrap();
    let responded: Vec<_> = events
        .iter()
        .filter(|event| event.kind == EventKind::ModelResponded)
        .collect();
    assert_eq!(responded.len(), 2);
    let first = &responded[0].payload;
    assert_eq!(first["cost_micro_usd"], 1_500);
    assert_eq!(first["model"], "scripted/m");
    assert_eq!(first["price_version"], "test");
    assert_eq!(first["usage"]["input_tokens"], 1_500);
    assert_eq!(first["stop_reason"], "tool_use");
    assert!(first["latency_ms"].is_u64());
    assert!(first["content"].is_array());
    let usage = store.usage(session.session_id()).await.unwrap();
    assert_eq!(usage[0].spent_tokens, 2_000);
    assert_eq!(usage[0].spent_cost_micro_usd, 2_000);
}

#[tokio::test]
async fn the_soft_cap_tells_the_model_once_and_only_once() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    let mut script: Vec<_> = (0..11).map(|_| costing(1_000, a_read())).collect();
    script.push(says("done"));
    let provider = Scripted::of(script);
    let mut session = session_over(
        Store::in_memory().unwrap(),
        tool_set(dir.path()),
        session_config(30, budget(tokens_cap(50_000), 20, None)),
    )
    .await;

    let outcome = session.prompt(&provider, "go").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::EndTurn);
    let notes: Vec<String> = session
        .turns()
        .iter()
        .flat_map(|turn| &turn.content)
        .filter_map(|block| match block {
            jensen_harness_core::providers::ContentBlock::Text(text)
                if text.starts_with("[budget]") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("20% of the token budget"), "{notes:?}");
    assert!(
        provider
            .request(9)
            .turns
            .iter()
            .all(|turn| turn.plain_text().is_empty() || !turn.plain_text().contains("[budget]")),
        "the note must not appear before the cap is crossed"
    );
    assert!(
        provider
            .request(10)
            .turns
            .iter()
            .any(|turn| turn.plain_text().contains("[budget]"))
    );
}

#[tokio::test]
async fn two_sessions_in_one_run_share_the_run_cap() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    let store = Store::in_memory().unwrap();
    store
        .open_scope(NewScope {
            id: "shared-run".to_string(),
            parent_id: None,
            kind: ScopeKind::Run,
            caps: tokens_cap(12_000),
            soft_percent: 80,
        })
        .await
        .unwrap();
    let in_run = |store: &Store| {
        let mut config = session_config(20, budget(tokens_cap(1_000_000), 80, None));
        config.run_id = Some("shared-run".to_string());
        session_over(store.clone(), tool_set(dir.path()), config)
    };
    let mut first = in_run(&store).await;
    let mut second = in_run(&store).await;
    let provider = Scripted::repeating(costing(6_000, says("ok")));
    first.prompt(&provider, "one").await.unwrap();
    second.prompt(&provider, "two").await.unwrap();

    let outcome = first.prompt(&provider, "three").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::BudgetExhausted);
    let exhausted = store
        .events(first.session_id())
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.kind == EventKind::BudgetExhausted)
        .unwrap();
    assert_eq!(exhausted.payload["scope_id"], "shared-run");
    assert_eq!(
        store.usage("shared-run").await.unwrap()[0].spent_tokens,
        12_000
    );
}

#[tokio::test]
async fn a_session_cannot_join_a_run_that_does_not_exist() {
    let dir = tempdir().unwrap();
    let mut config = session_config(20, default_budget());
    config.run_id = Some("no-such-run".to_string());
    let error = jensen_harness_core::agent::Session::create(
        Store::in_memory().unwrap(),
        config,
        tool_set(dir.path()),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(error, AgentError::Store(_)), "{error:?}");
}

#[tokio::test]
async fn a_provider_error_releases_its_hold_and_journals_the_failure() {
    let dir = tempdir().unwrap();
    let provider = Scripted::new(vec![Err(ProviderError::Upstream {
        provider: "scripted".to_string(),
        message: "503".to_string(),
    })]);
    let store = Store::in_memory().unwrap();
    let mut session = session_over(
        store.clone(),
        tool_set(dir.path()),
        session_config(20, default_budget()),
    )
    .await;

    let error = session.prompt(&provider, "go").await.unwrap_err();

    assert!(matches!(error, AgentError::Provider(_)));
    let usage = store.usage(session.session_id()).await.unwrap();
    assert_eq!((usage[0].held_tokens, usage[0].spent_tokens), (0, 0));
    assert!(
        store
            .open_reservations(session.session_id())
            .await
            .unwrap()
            .is_empty()
    );
    let events = store.events(session.session_id()).await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.kind == EventKind::ModelFailed)
    );
}

#[tokio::test]
async fn with_no_budget_configured_the_built_in_cap_is_on_the_session_scope() {
    let dir = tempdir().unwrap();
    let store = Store::in_memory().unwrap();
    let session = session_over(
        store.clone(),
        tool_set(dir.path()),
        session_config(20, default_budget()),
    )
    .await;

    let usage = store.usage(session.session_id()).await.unwrap();

    assert_eq!(usage[0].caps.max_tokens, Some(5_000_000));
    assert_eq!(usage.len(), 2, "a private run sits above the session");
    assert_eq!(usage[1].caps, Caps::default());
    let created = &store.events(session.session_id()).await.unwrap()[0];
    assert_eq!(created.kind, EventKind::SessionCreated);
    assert_eq!(created.payload["model_address"], "scripted/m");
    assert!(created.payload["worktree"].is_string());
}
