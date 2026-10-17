use std::sync::Arc;

use async_trait::async_trait;
use jensen_harness_core::agent::PromptStop;
use jensen_harness_core::providers::{ContentBlock, ToolSpec};
use jensen_harness_core::toolset::{ProviderReply, Replay, ToolProvider, ToolSet, ToolSetError};
use serde_json::{Value, json};
use tempfile::tempdir;

use crate::common::{
    Scripted, calls_tool, default_budget, says, session_config, session_over, tool_set,
};

struct Fake {
    names: Vec<&'static str>,
    reply: ProviderReply,
    fail: bool,
}

#[async_trait]
impl ToolProvider for Fake {
    fn specs(&self) -> Vec<ToolSpec> {
        self.names
            .iter()
            .map(|name| ToolSpec {
                name: (*name).to_owned(),
                description: format!("the {name} tool"),
                input_schema: json!({"type": "object"}),
            })
            .collect()
    }

    fn replay(&self, _name: &str) -> Replay {
        Replay::Never
    }

    async fn invoke(&self, _name: &str, _input: &Value) -> Result<ProviderReply, ToolSetError> {
        if self.fail {
            return Err(ToolSetError::Infrastructure(
                "the server vanished".to_owned(),
            ));
        }
        Ok(self.reply.clone())
    }
}

fn fake(names: Vec<&'static str>, text: &str, is_error: bool) -> Arc<Fake> {
    Arc::new(Fake {
        names,
        reply: ProviderReply {
            text: text.to_owned(),
            is_error,
        },
        fail: false,
    })
}

#[test]
fn provider_tools_follow_the_built_ins_in_a_stable_order_whatever_the_attach_order() {
    let dir = tempdir().unwrap();
    let first = tool_set(dir.path())
        .with_provider(fake(vec!["mcp__b__two", "mcp__a__one"], "", false))
        .with_provider(fake(vec!["lsp_symbols"], "", false));
    let second = tool_set(dir.path())
        .with_provider(fake(vec!["lsp_symbols"], "", false))
        .with_provider(fake(vec!["mcp__a__one", "mcp__b__two"], "", false));

    let specs = first.specs();

    assert_eq!(specs, second.specs());
    let builtin = ToolSet::builtin_specs();
    assert_eq!(&specs[..builtin.len()], builtin.as_slice());
    let extra: Vec<&str> = specs[builtin.len()..]
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(extra, ["lsp_symbols", "mcp__a__one", "mcp__b__two"]);
}

#[tokio::test]
async fn a_large_extension_result_is_compacted_with_a_handle_that_expands_to_all_of_it() {
    let dir = tempdir().unwrap();
    let big: String = (0..2000).map(|n| format!("line {n}\n")).collect();
    let tools = tool_set(dir.path()).with_provider(fake(vec!["ext_dump"], &big, false));

    let reply = tools.invoke("ext_dump", &json!({})).await.unwrap();

    assert!(!reply.is_error);
    assert!(
        reply.sent_tokens < reply.raw_tokens,
        "{} vs {}",
        reply.sent_tokens,
        reply.raw_tokens
    );
    let handle = reply
        .text
        .rsplit("expand ")
        .next()
        .unwrap()
        .trim_end_matches(']')
        .to_owned();
    let expanded = tools
        .invoke(
            "expand",
            &json!({"handle": handle, "offset": 0, "limit": 5}),
        )
        .await
        .unwrap();
    assert!(expanded.text.contains("line 0"), "{}", expanded.text);
}

#[tokio::test]
async fn an_extension_error_reaches_the_model_and_a_broken_provider_ends_the_run() {
    let dir = tempdir().unwrap();
    let store = jensen_harness_core::store::Store::in_memory().unwrap();
    let tools =
        tool_set(dir.path()).with_provider(fake(vec!["ext_do"], "the file is locked", true));
    let mut session = session_over(store, tools, session_config(5, default_budget())).await;
    let provider = Scripted::of(vec![calls_tool("t1", "ext_do", json!({})), says("noted")]);

    let outcome = session.prompt(&provider, "go").await.unwrap();

    assert_eq!(outcome.stop, PromptStop::EndTurn);
    let results = &provider.request(1).turns[2];
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &results.content[0]
    else {
        panic!("expected a tool result");
    };
    assert!(
        *is_error && content.contains("the file is locked"),
        "{content}"
    );

    let broken = Arc::new(Fake {
        names: vec!["ext_do"],
        reply: ProviderReply {
            text: String::new(),
            is_error: false,
        },
        fail: true,
    });
    let store = jensen_harness_core::store::Store::in_memory().unwrap();
    let mut session = session_over(
        store,
        tool_set(dir.path()).with_provider(broken),
        session_config(5, default_budget()),
    )
    .await;
    let provider = Scripted::of(vec![calls_tool("t1", "ext_do", json!({})), says("never")]);
    let error = session.prompt(&provider, "go").await.unwrap_err();
    assert!(error.to_string().contains("the server vanished"), "{error}");
}

#[tokio::test]
async fn the_request_carries_the_provider_tools() {
    let dir = tempdir().unwrap();
    let store = jensen_harness_core::store::Store::in_memory().unwrap();
    let tools = tool_set(dir.path()).with_provider(fake(vec!["ext_do"], "", false));
    let mut session = session_over(store, tools, session_config(5, default_budget())).await;
    let provider = Scripted::of(vec![says("hi")]);
    session.prompt(&provider, "go").await.unwrap();
    let names: Vec<String> = provider
        .request(0)
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    assert_eq!(names.last().map(String::as_str), Some("ext_do"));
}
