use crate::common::{
    Echo, Scripted, UnavailableSandbox, calls_tool, default_budget, new_session, path_input, reply,
    says, session_config, session_over, tool_set_with, tool_use,
};
use jensen_harness_core::agent::{AgentError, PromptStop};
use jensen_harness_core::providers::{
    ContentBlock, ProviderError, Role, StopReason, ToolSpec, Usage,
};
use jensen_harness_core::store::Store;
use jensen_harness_core::toolset::{ToolSet, ToolSetError};
use serde_json::json;
use std::fs;
use tempfile::tempdir;

fn result_of(turn: &jensen_harness_core::providers::Turn, index: usize) -> (&str, &str, bool) {
    match turn.content.get(index) {
        Some(ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        }) => (tool_use_id, content, *is_error),
        other => panic!("expected a tool result, got {other:?}"),
    }
}

#[tokio::test]
async fn a_plain_answer_appends_both_turns_in_order() {
    let dir = tempdir().unwrap();
    let mut session = new_session(dir.path(), 5).await;
    let outcome = session.prompt(&Echo, "hello").await.unwrap();
    assert_eq!(outcome.stop, PromptStop::EndTurn);
    assert_eq!(outcome.model_calls, 1);
    let [first, second] = session.turns() else {
        panic!("expected exactly two turns, got {}", session.turns().len());
    };
    assert_eq!(first.role, Role::User);
    assert_eq!(first.plain_text(), "hello");
    assert_eq!(second.role, Role::Assistant);
    assert_eq!(second.plain_text(), "echo: hello");
}

#[tokio::test]
async fn a_tool_call_runs_and_its_result_goes_back_before_the_final_answer() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello world").unwrap();
    let provider = Scripted::of(vec![
        calls_tool(
            "t1",
            "edit",
            json!({"path": "a.txt", "find": "world", "replace": "there"}),
        ),
        says("done"),
    ]);
    let mut session = new_session(dir.path(), 5).await;

    let outcome = session.prompt(&provider, "fix it").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::EndTurn);
    assert_eq!(outcome.model_calls, 2);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "hello there"
    );
    let turns = session.turns();
    assert_eq!(turns.len(), 4);
    let (id, content, is_error) = result_of(turns.get(2).unwrap(), 0);
    assert_eq!((id, is_error), ("t1", false));
    assert_eq!(content, "edited a.txt: 1 replacement, first at line 1");
    assert_eq!(turns.get(3).unwrap().plain_text(), "done");
    assert_eq!(
        outcome.usage,
        Usage {
            input_tokens: 20,
            output_tokens: 10,
            cache_read_tokens: 200,
            cache_write_tokens: 2,
        }
    );
}

#[tokio::test]
async fn every_result_for_one_reply_comes_back_in_one_user_turn_in_call_order() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "alpha").unwrap();
    fs::write(dir.path().join("b.txt"), "beta").unwrap();
    let provider = Scripted::of(vec![
        reply(
            vec![
                tool_use("one", "read", path_input("a.txt")),
                tool_use("two", "read", path_input("b.txt")),
            ],
            StopReason::ToolUse,
        ),
        says("ok"),
    ]);
    let mut session = new_session(dir.path(), 5).await;

    session.prompt(&provider, "read both").await.unwrap();

    let results = session.turns().get(2).unwrap();
    assert_eq!(results.role, Role::User);
    assert_eq!(results.content.len(), 2);
    assert_eq!(result_of(results, 0), ("one", "alpha", false));
    assert_eq!(result_of(results, 1), ("two", "beta", false));
    let second_request = provider.request(1);
    assert_eq!(second_request.turns.len(), 3);
    assert_eq!(second_request.tools, ToolSet::specs());
}

#[tokio::test]
async fn what_the_model_got_wrong_comes_back_as_an_error_it_can_read_and_the_loop_continues() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    let provider = Scripted::of(vec![
        reply(
            vec![
                tool_use("a", "nope", json!({})),
                tool_use("b", "read", json!({"path": "a.txt", "bogus": 1})),
                tool_use("c", "read", path_input("../outside.txt")),
                tool_use(
                    "d",
                    "edit",
                    json!({"path": "a.txt", "find": "absent", "replace": "y"}),
                ),
                tool_use("e", "expand", json!({"handle": "r99"})),
            ],
            StopReason::ToolUse,
        ),
        says("understood"),
    ]);
    let mut session = new_session(dir.path(), 5).await;

    let outcome = session.prompt(&provider, "go").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::EndTurn);
    let results = session.turns().get(2).unwrap();
    for (index, needle) in [
        "unknown tool",
        "invalid input",
        "outside the worktree",
        "was not found",
        "not a handle",
    ]
    .iter()
    .enumerate()
    {
        let (_, content, is_error) = result_of(results, index);
        assert!(is_error, "{content}");
        assert!(content.contains(needle), "{content}");
    }
    assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "x");
}

#[tokio::test]
async fn a_model_that_never_stops_calling_tools_is_ended_at_the_turn_cap() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "x").unwrap();
    let provider = Scripted::repeating(calls_tool("t", "read", path_input("a.txt")));
    let mut session = new_session(dir.path(), 3).await;

    let outcome = session.prompt(&provider, "loop").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::TurnLimit);
    assert_eq!(outcome.model_calls, 3);
    assert_eq!(provider.calls(), 3);
}

#[tokio::test]
async fn pause_turn_resumes_and_refusal_and_max_tokens_stop() {
    let dir = tempdir().unwrap();
    let paused = Scripted::of(vec![
        reply(
            vec![ContentBlock::Text("thinking".into())],
            StopReason::PauseTurn,
        ),
        says("done"),
    ]);
    let mut session = new_session(dir.path(), 5).await;
    let outcome = session.prompt(&paused, "go").await.unwrap();
    assert_eq!(
        (outcome.stop, outcome.model_calls),
        (PromptStop::EndTurn, 2)
    );

    for (reason, expected) in [
        (StopReason::Refusal, PromptStop::Refusal),
        (StopReason::MaxTokens, PromptStop::MaxTokens),
        (StopReason::StopSequence, PromptStop::StopSequence),
    ] {
        let provider = Scripted::of(vec![reply(vec![], reason)]);
        let mut session = new_session(dir.path(), 5).await;
        assert_eq!(
            session.prompt(&provider, "go").await.unwrap().stop,
            expected
        );
    }
}

#[tokio::test]
async fn a_provider_error_ends_the_prompt_without_an_assistant_turn() {
    let dir = tempdir().unwrap();
    let provider = Scripted::new(vec![Err(ProviderError::Upstream {
        provider: "scripted".to_string(),
        message: "429".to_string(),
    })]);
    let mut session = new_session(dir.path(), 5).await;

    let error = session.prompt(&provider, "go").await.unwrap_err();

    assert!(matches!(
        error,
        AgentError::Provider(ProviderError::Upstream { .. })
    ));
    assert_eq!(session.turns().len(), 1);
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn a_tool_call_stop_with_no_tool_call_is_an_error_not_a_hang() {
    let dir = tempdir().unwrap();
    let provider = Scripted::of(vec![reply(vec![], StopReason::ToolUse)]);
    let mut session = new_session(dir.path(), 5).await;
    let error = session.prompt(&provider, "go").await.unwrap_err();
    assert!(matches!(
        error,
        AgentError::Provider(ProviderError::Malformed(_))
    ));
}

#[tokio::test]
async fn when_the_harness_cannot_run_a_tool_the_run_ends_and_the_transcript_stays_valid() {
    let dir = tempdir().unwrap();
    let provider = Scripted::of(vec![reply(
        vec![
            tool_use("one", "shell", json!({"command": "true"})),
            tool_use("two", "read", path_input("a.txt")),
        ],
        StopReason::ToolUse,
    )]);
    let tools = tool_set_with(dir.path(), Box::new(UnavailableSandbox));
    let mut session = session_over(
        Store::in_memory().unwrap(),
        tools,
        session_config(5, default_budget()),
    )
    .await;

    let error = session.prompt(&provider, "go").await.unwrap_err();

    assert!(matches!(error, AgentError::Tool(ToolSetError::Sandbox(_))));
    let last = session.turns().last().unwrap();
    assert_eq!(last.role, Role::User);
    assert_eq!(last.content.len(), 2);
    for index in 0..2 {
        let (_, _, is_error) = result_of(last, index);
        assert!(is_error);
    }
    assert_eq!(provider.calls(), 1);
}

#[tokio::test]
async fn reading_a_large_file_costs_less_than_the_file_and_the_savings_are_reported() {
    let dir = tempdir().unwrap();
    let mut source = String::new();
    for i in 0..60 {
        source.push_str(&format!(
            "pub fn handler_{i}(input: &str) -> String {{\n    let mut out = String::new();\n    for part in input.split(',') {{\n        out.push_str(part.trim());\n        out.push(';');\n    }}\n    out\n}}\n\n"
        ));
    }
    fs::write(dir.path().join("big.rs"), &source).unwrap();
    let provider = Scripted::of(vec![
        calls_tool("t", "read", path_input("big.rs")),
        says("ok"),
    ]);
    let mut session = new_session(dir.path(), 5).await;

    let outcome = session.prompt(&provider, "look").await.unwrap();

    assert!(outcome.savings.raw_tokens > 0);
    assert!(
        outcome.savings.sent_tokens * 2 < outcome.savings.raw_tokens,
        "{:?}",
        outcome.savings
    );
    let (_, content, _) = result_of(session.turns().get(2).unwrap(), 0);
    assert!(content.contains("outline only"));
}

#[tokio::test]
async fn a_second_prompt_continues_the_same_transcript() {
    let dir = tempdir().unwrap();
    let provider = Scripted::of(vec![says("one"), says("two")]);
    let mut session = new_session(dir.path(), 5).await;
    session.prompt(&provider, "first").await.unwrap();
    session.prompt(&provider, "second").await.unwrap();
    assert_eq!(session.turns().len(), 4);
    assert_eq!(provider.request(1).turns.len(), 3);
}

#[test]
fn the_tool_list_is_fixed_ordered_and_schema_shaped() {
    let specs: Vec<ToolSpec> = ToolSet::specs();
    let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    assert_eq!(names, ["read", "edit", "glob", "grep", "shell", "expand"]);
    assert_eq!(specs, ToolSet::specs());
    for spec in &specs {
        assert_eq!(spec.input_schema["type"], "object", "{}", spec.name);
        assert!(!spec.description.is_empty());
    }
}
