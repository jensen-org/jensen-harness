use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jensen_harness_core::budget::BudgetSettings;
use jensen_harness_core::providers::{ContentBlock, Provider};
use jensen_harness_core::sandbox::NetworkPolicy;
use jensen_harness_core::toolset::{Replay, ToolProvider};
use jensen_harness_extensions::mcp::connect;
use jensen_harness_extensions::{
    Extensions, McpDirectory, McpError, McpServerSpec, McpTransportSpec, Secret,
};
use jensen_harness_workflow::{RunError, RunStatus, load};
use serde_json::json;

use crate::common::{Scripted, calls_tool, says, testkit_mcp_path, tool_set};
use crate::ext_host::{ExtHost, run};

fn stdio(env: Vec<(String, String)>) -> McpServerSpec {
    McpServerSpec {
        name: "fx".to_owned(),
        transport: McpTransportSpec::Stdio {
            command: testkit_mcp_path().display().to_string(),
            args: Vec::new(),
            env,
        },
        network: NetworkPolicy::None,
        writable: false,
        tools: Vec::new(),
        timeout: Duration::from_secs(20),
    }
}

struct Directory(McpServerSpec);

impl McpDirectory for Directory {
    fn server(&self, name: &str) -> Result<McpServerSpec, McpError> {
        if name == self.0.name {
            Ok(self.0.clone())
        } else {
            Err(McpError::Unknown(name.to_owned()))
        }
    }
}

fn host(provider: Arc<dyn Provider>, dir: &Path, spec: McpServerSpec) -> ExtHost {
    ExtHost {
        provider,
        dir: dir.to_path_buf(),
        extensions: Extensions {
            mcp: Some(Arc::new(Directory(spec))),
            ..Extensions::default()
        },
        budget: BudgetSettings::default(),
    }
}

const FLOW: &str = r#"
version: 1
name: mcp
channels:
  x: { schema: { type: string }, init: "" }
nodes:
  drawer: { kind: agent, role: design, prompt: p, reads: [x], max_turns: 5, mcp: [fx] }
"#;

fn tool_result(
    request: &jensen_harness_core::providers::CompletionRequest,
    turn: usize,
) -> (String, bool) {
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &request.turns[turn].content[0]
    else {
        panic!("expected a tool result");
    };
    (content.clone(), *is_error)
}

#[tokio::test]
async fn server_tools_reach_the_model_and_a_call_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let scripted = Arc::new(Scripted::of(vec![
        calls_tool("t1", "mcp__fx__echo", json!({"text": "hello"})),
        says("done"),
    ]));

    let report = run(
        host(scripted.clone(), dir.path(), stdio(Vec::new())),
        load(FLOW).unwrap(),
        "m1",
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Succeeded, "{:?}", report.failure);
    let names: Vec<String> = scripted
        .request(0)
        .tools
        .iter()
        .map(|t| t.name.clone())
        .collect();
    let extra: Vec<&str> = names
        .iter()
        .filter(|n| n.starts_with("mcp__"))
        .map(String::as_str)
        .collect();
    assert_eq!(
        extra,
        [
            "mcp__fx__big",
            "mcp__fx__draw",
            "mcp__fx__echo",
            "mcp__fx__env",
            "mcp__fx__slow",
            "mcp__fx__write_file"
        ]
    );
    assert_eq!(
        tool_result(&scripted.request(1), 2),
        ("hello".to_owned(), false)
    );
}

#[tokio::test]
async fn the_operator_and_the_node_allowlists_both_narrow_the_tools() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tool_set(dir.path());
    let mut spec = stdio(Vec::new());
    spec.tools = vec!["echo".to_owned(), "big".to_owned()];
    let both = connect(&spec, &["echo".to_owned(), "draw".to_owned()], &tools)
        .await
        .unwrap();
    let names: Vec<String> = both.specs().iter().map(|s| s.name.clone()).collect();
    assert_eq!(names, ["mcp__fx__echo"]);
    let operator_only = connect(&spec, &[], &tools).await.unwrap();
    assert_eq!(operator_only.specs().len(), 2);
}

#[tokio::test]
async fn only_the_allowed_environment_reaches_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tool_set(dir.path());
    let provider = connect(
        &stdio(vec![("TESTKIT_PASSED".to_owned(), "yes".to_owned())]),
        &[],
        &tools,
    )
    .await
    .unwrap();
    let passed = provider
        .invoke("mcp__fx__env", &json!({"text": "TESTKIT_PASSED"}))
        .await
        .unwrap();
    assert_eq!(passed.text, "yes");
    let leaked = provider
        .invoke("mcp__fx__env", &json!({"text": "USER"}))
        .await
        .unwrap();
    assert_eq!(
        leaked.text, "unset",
        "the host environment must not be inherited"
    );
}

#[tokio::test]
async fn replay_follows_the_servers_own_read_only_hint() {
    let dir = tempfile::tempdir().unwrap();
    let provider = connect(&stdio(Vec::new()), &[], &tool_set(dir.path()))
        .await
        .unwrap();
    assert_eq!(provider.replay("mcp__fx__echo"), Replay::Repeat);
    assert_eq!(provider.replay("mcp__fx__draw"), Replay::Never);
    assert_eq!(provider.replay("mcp__fx__unknown"), Replay::Never);
}

#[tokio::test]
async fn a_call_that_never_answers_times_out_as_an_error_the_model_can_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = stdio(Vec::new());
    spec.timeout = Duration::from_secs(1);
    let scripted = Arc::new(Scripted::of(vec![
        calls_tool("t1", "mcp__fx__slow", json!({})),
        says("moved on"),
    ]));
    let started = std::time::Instant::now();

    let report = run(
        host(scripted.clone(), dir.path(), spec),
        load(FLOW).unwrap(),
        "m2",
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Succeeded, "{:?}", report.failure);
    assert!(started.elapsed() < Duration::from_secs(15));
    let (text, is_error) = tool_result(&scripted.request(1), 2);
    assert!(is_error && text.contains("no answer"), "{text}");
}

#[tokio::test]
async fn a_large_result_is_compacted_with_a_handle() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tool_set(dir.path());
    let provider = Arc::new(connect(&stdio(Vec::new()), &[], &tools).await.unwrap());
    let tools = tools.with_provider(provider);
    let reply = tools
        .invoke("mcp__fx__big", &json!({"lines": 3000}))
        .await
        .unwrap();
    assert!(reply.text.contains("full output: expand"), "{}", reply.text);
    assert!(reply.sent_tokens < reply.raw_tokens);
}

#[tokio::test]
async fn an_unconfigured_or_unreachable_server_is_a_typed_error_naming_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let scripted = Arc::new(Scripted::of(vec![]));
    let mut spec = stdio(Vec::new());
    spec.name = "other".to_owned();
    let error = run(
        host(scripted.clone(), dir.path(), spec),
        load(FLOW).unwrap(),
        "m3",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, RunError::Mcp { node, source: McpError::Unknown(name) } if node == "drawer" && name == "fx"),
        "{error}"
    );
    assert_eq!(scripted.calls(), 0);

    let mut missing = stdio(Vec::new());
    missing.transport = McpTransportSpec::Stdio {
        command: "/no/such/server".to_owned(),
        args: Vec::new(),
        env: Vec::new(),
    };
    let error = connect(&missing, &[], &tool_set(dir.path()))
        .await
        .err()
        .unwrap();
    assert!(matches!(error, McpError::Connect { .. }), "{error}");
}

#[tokio::test]
async fn the_http_token_travels_as_a_header_and_never_into_errors_or_debug_output() {
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};
    const TOKEN: &str = "tok-mcp-secret-123";
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let spec = McpServerSpec {
        name: "remote".to_owned(),
        transport: McpTransportSpec::Http {
            url: format!("{}/mcp", server.uri()),
            token: Some(Secret::new(TOKEN)),
        },
        network: NetworkPolicy::Full,
        writable: false,
        tools: Vec::new(),
        timeout: Duration::from_secs(5),
    };
    let dir = tempfile::tempdir().unwrap();

    let error = connect(&spec, &[], &tool_set(dir.path()))
        .await
        .err()
        .unwrap();

    assert!(!error.to_string().contains(TOKEN));
    assert!(!format!("{spec:?}").contains(TOKEN));
    let seen: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| r.headers.get("authorization"))
        .map(|v| v.to_str().unwrap().to_owned())
        .collect();
    assert!(seen.iter().any(|value| value.contains(TOKEN)), "{seen:?}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn a_stdio_server_cannot_write_outside_its_worktree_under_the_real_sandbox() {
    use jensen_harness_core::toolset::{Access, ToolSet};
    let worktree = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir_in(std::env::var("HOME").unwrap()).unwrap();
    let tools = ToolSet::for_worktree_access(worktree.path(), Access::ReadWrite).unwrap();
    let provider = connect(&stdio(Vec::new()), &[], &tools).await.unwrap();
    let target = outside.path().join("pwned.txt");

    let reply = provider
        .invoke(
            "mcp__fx__write_file",
            &json!({"path": target.display().to_string()}),
        )
        .await
        .unwrap();

    assert!(reply.is_error, "{}", reply.text);
    assert!(
        !target.exists(),
        "the sandbox let the server write outside its worktree"
    );
}

mod configured {
    use std::collections::HashMap;
    use std::sync::Arc;

    use jensen_harness::engine::{ConfiguredModels, ModelSource};
    use jensen_harness_core::providers::ProviderError;
    use jensen_harness_core::providers::config::ProviderRegistry;
    use jensen_harness_core::sandbox::NetworkPolicy;
    use jensen_harness_core::secrets::{SecretError, SecretSource};
    use jensen_harness_extensions::{McpError, McpTransportSpec};

    const CONFIG: &str = r#"
[providers.p]
transport = "openai_compatible"
base_url = "http://localhost/v1"
api_key_env = "P_KEY"

[mcp.servers.excalidraw]
command = "excalidraw-mcp"
args = ["--stdio"]
env_allow = ["EXCALI_HOME", "MISSING_ONE"]
network = "none"
tools = ["draw"]
timeout_secs = 90

[mcp.servers.figma]
url = "https://mcp.example.invalid/mcp"
network = "full"
access = "read_write"
api_key_env = "FIGMA_TOKEN"
"#;

    struct Env(HashMap<String, String>);

    impl SecretSource for Env {
        fn env(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }

        fn keychain(&self, _s: &str, _u: &str) -> Result<Option<String>, SecretError> {
            Ok(None)
        }
    }

    fn models(extra: &[(&str, &str)]) -> ConfiguredModels {
        ConfiguredModels {
            registry: ProviderRegistry::from_toml(CONFIG).unwrap(),
            secrets: Arc::new(Env(extra
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect())),
            default_model: None,
        }
    }

    #[test]
    fn servers_come_from_the_config_with_only_the_allowed_environment() {
        let models = models(&[
            ("EXCALI_HOME", "/h"),
            ("SECRET", "no"),
            ("FIGMA_TOKEN", "tok"),
        ]);
        let local = models.mcp_server("excalidraw").unwrap();
        let McpTransportSpec::Stdio { command, args, env } = &local.transport else {
            panic!("expected stdio");
        };
        assert_eq!(
            (command.as_str(), args.as_slice()),
            ("excalidraw-mcp", ["--stdio".to_owned()].as_slice())
        );
        assert_eq!(env, &[("EXCALI_HOME".to_owned(), "/h".to_owned())]);
        assert_eq!(local.network, NetworkPolicy::None);
        assert_eq!(
            (local.tools.clone(), local.timeout.as_secs()),
            (vec!["draw".to_owned()], 90)
        );
        assert!(!local.writable);

        let remote = models.mcp_server("figma").unwrap();
        assert!(matches!(
            &remote.transport,
            McpTransportSpec::Http { token: Some(_), .. }
        ));
        assert_eq!(remote.network, NetworkPolicy::Full);
        assert!(remote.writable);
        assert!(!format!("{remote:?}").contains("figma-secret-77"));
    }

    #[test]
    fn a_missing_token_or_server_is_a_typed_error() {
        let bare = models(&[]);
        assert!(matches!(
            bare.mcp_server("figma"),
            Err(McpError::Config { .. })
        ));
        assert!(matches!(bare.mcp_server("nope"), Err(McpError::Unknown(_))));
    }

    #[test]
    fn a_server_entry_must_be_a_command_or_a_url_and_a_command_takes_no_token() {
        for (extra, why) in [
            ("[mcp.servers.a]\n", "neither"),
            (
                "[mcp.servers.a]\ncommand = \"x\"\nurl = \"https://x\"\n",
                "both",
            ),
            (
                "[mcp.servers.a]\ncommand = \"x\"\napi_key_env = \"T\"\n",
                "token on a command",
            ),
            (
                "[mcp.servers.a]\nurl = \"https://x\"\napi_key_env = \"T\"\napi_key_keychain = {}\n",
                "two token sources",
            ),
        ] {
            let text = format!("{CONFIG}\n{extra}");
            assert!(
                matches!(
                    ProviderRegistry::from_toml(&text),
                    Err(ProviderError::Config(_))
                ),
                "{why}"
            );
        }
    }
}
