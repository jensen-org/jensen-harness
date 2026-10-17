use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jensen_harness_core::budget::BudgetSettings;
use jensen_harness_core::providers::{CompletionRequest, ContentBlock, Provider};
use jensen_harness_core::sandbox::NetworkPolicy;
use jensen_harness_core::store::{EventKind, Store};
use jensen_harness_extensions::{
    ConfiguredLsp, Extensions, LspError, LspServerSpec, LspSpecs, McpDirectory, McpError,
    McpServerSpec, McpTransportSpec, SkillCatalog,
};
use jensen_harness_workflow::{RunError, RunStatus, load};
use serde_json::json;

use crate::common::{Scripted, calls_tool, says, testkit_lsp_path, testkit_mcp_path};
use crate::ext_host::{ExtHost, run};

struct Mcp;

impl McpDirectory for Mcp {
    fn server(&self, name: &str) -> Result<McpServerSpec, McpError> {
        if name != "fx" {
            return Err(McpError::Unknown(name.to_owned()));
        }
        Ok(McpServerSpec {
            name: "fx".to_owned(),
            transport: McpTransportSpec::Stdio {
                command: testkit_mcp_path().display().to_string(),
                args: Vec::new(),
                env: Vec::new(),
            },
            network: NetworkPolicy::None,
            writable: false,
            mutating: false,
            tools: Vec::new(),
            timeout: Duration::from_secs(20),
        })
    }
}

struct Specs {
    command: String,
}

impl LspSpecs for Specs {
    fn server(&self, name: &str) -> Result<LspServerSpec, LspError> {
        if name != "ls" {
            return Err(LspError::Unknown(name.to_owned()));
        }
        Ok(LspServerSpec {
            name: "ls".to_owned(),
            command: self.command.clone(),
            args: Vec::new(),
            env: Vec::new(),
            language: "rust".to_owned(),
            network: NetworkPolicy::None,
            timeout: Duration::from_secs(20),
        })
    }
}

fn extensions(dir: &Path, lsp_command: &str) -> Extensions {
    let skills = dir.join("skills/review");
    std::fs::create_dir_all(&skills).unwrap();
    std::fs::write(
        skills.join("SKILL.md"),
        "---\nname: review\ndescription: How to review.\n---\nCheck the edges first.\n",
    )
    .unwrap();
    Extensions {
        skills: SkillCatalog::discover(&[dir.join("skills")]).unwrap(),
        mcp: Some(Arc::new(Mcp)),
        lsp: Some(Arc::new(ConfiguredLsp::new(Arc::new(Specs {
            command: lsp_command.to_owned(),
        })))),
    }
}

fn lsp_path() -> String {
    testkit_lsp_path().display().to_string()
}

fn project(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/lib.rs"),
        "fn alpha() {}\n// TODO tidy\nfn beta() {}\n",
    )
    .unwrap();
    std::fs::write(dir.join("notes.md"), "remember the brief\n").unwrap();
}

fn host(provider: Arc<dyn Provider>, dir: &Path, extensions: Extensions) -> ExtHost {
    ExtHost {
        provider,
        dir: dir.to_path_buf(),
        extensions,
        budget: BudgetSettings::default(),
    }
}

fn first_prompt(request: &CompletionRequest) -> String {
    let ContentBlock::Text(text) = &request.turns[0].content[0] else {
        panic!("expected text");
    };
    text.clone()
}

const SOURCES: &str = r#"
version: 1
name: sources
channels:
  x: { schema: { type: string }, init: "" }
nodes:
  architect:
    kind: agent
    role: arch
    prompt: Plan the change.
    reads: [x]
    max_turns: 3
    mcp: [fx]
    skills: [review]
    context:
      fetch:
        - { source: files, globs: ["notes.md"] }
        - { source: treesitter, paths: ["src/*.rs"], view: outline }
        - { source: skill, name: review }
        - { source: mcp, server: fx, resource: "note://design" }
        - { source: mcp, server: fx, tool: echo, args: { text: from the tool } }
        - { source: lsp, server: ls, query: symbols, paths: ["src/lib.rs"] }
        - { source: lsp, server: ls, query: diagnostics, paths: ["src/lib.rs"] }
        - { source: lsp, server: ls, query: references, paths: ["src/lib.rs"] }
"#;

#[tokio::test]
async fn every_source_lands_in_the_first_prompt_and_in_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let scripted = Arc::new(Scripted::of(vec![says("planned")]));
    let ext = extensions(dir.path(), &lsp_path());

    let report = run(
        host(scripted.clone(), dir.path(), ext),
        load(SOURCES).unwrap(),
        "c1",
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Succeeded, "{:?}", report.failure);
    let prompt = first_prompt(&scripted.request(0));
    for expected in [
        "remember the brief",
        "fn alpha",
        "Check the edges first.",
        "the design brief",
        "from the tool",
        "src/lib.rs:1 alpha",
        "src/lib.rs:2:1",
        "found a TODO",
        "alpha: 1 reference(s)",
    ] {
        assert!(
            prompt.contains(expected),
            "{expected:?} missing from:\n{prompt}"
        );
    }
    assert!(prompt.contains("<context source=\"files:notes.md\">"));
}

#[tokio::test]
async fn inherited_channels_and_upstream_answers_arrive_cut_and_labelled() {
    let dir = tempfile::tempdir().unwrap();
    let long = "word ".repeat(400);
    let scripted = Arc::new(Scripted::of(vec![
        says(&format!("{{\"notes\": \"{long}\"}}")),
        says("ok"),
    ]));
    let yaml = r#"
version: 1
name: inherit
channels:
  notes: { schema: { type: string } }
nodes:
  a: { kind: agent, role: r, prompt: write notes, writes: [notes], max_turns: 3 }
  b:
    kind: agent
    role: r
    prompt: use them
    max_turns: 3
    context:
      inherit: [{ channel: notes, mode: summary, max_tokens: 30 }]
      from_nodes: [a]
edges:
  - { from: a, to: b }
"#;

    let report = run(
        host(scripted.clone(), dir.path(), Extensions::default()),
        load(yaml).unwrap(),
        "c2",
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Succeeded, "{:?}", report.failure);
    let prompt = first_prompt(&scripted.request(1));
    assert!(
        prompt.contains("<context source=\"channel notes (summary)\">"),
        "{prompt}"
    );
    assert!(prompt.contains("[cut to 30 tokens]"), "{prompt}");
    assert!(prompt.contains("<context source=\"answer of node a\">"));
    assert!(
        !prompt.contains("Inputs (JSON)"),
        "an inherited channel is not repeated as plain input"
    );
}

#[tokio::test]
async fn sources_over_the_budget_are_dropped_with_a_note() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let scripted = Arc::new(Scripted::of(vec![says("ok")]));
    let yaml = SOURCES.replace(
        "    context:\n",
        "    context:\n      max_context_tokens: 60\n",
    );

    let report = run(
        host(
            scripted.clone(),
            dir.path(),
            extensions(dir.path(), &lsp_path()),
        ),
        load(&yaml).unwrap(),
        "c3",
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Succeeded, "{:?}", report.failure);
    let prompt = first_prompt(&scripted.request(0));
    assert!(
        prompt.contains("context budget of 60 tokens reached; left out:"),
        "{prompt}"
    );
}

#[tokio::test]
async fn lazy_sources_are_offered_as_a_tool_and_not_pasted() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let scripted = Arc::new(Scripted::of(vec![
        calls_tool("t1", "fetch_context", json!({"source": "files:notes.md"})),
        says("done"),
    ]));
    let yaml = r#"
version: 1
name: lazy
channels:
  x: { schema: { type: string }, init: "" }
nodes:
  a:
    kind: agent
    role: r
    prompt: p
    reads: [x]
    max_turns: 4
    context:
      fetch: [{ source: files, globs: ["notes.md"], mode: lazy }]
"#;

    let report = run(
        host(scripted.clone(), dir.path(), Extensions::default()),
        load(yaml).unwrap(),
        "c4",
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Succeeded, "{:?}", report.failure);
    let first = scripted.request(0);
    assert!(!first_prompt(&first).contains("remember the brief"));
    assert!(first.tools.iter().any(|tool| tool.name == "fetch_context"));
    assert!(first.system.contains("files:notes.md"));
    let ContentBlock::ToolResult { content, .. } = &scripted.request(1).turns[2].content[0] else {
        panic!("expected a tool result");
    };
    assert!(content.contains("remember the brief"), "{content}");
}

#[tokio::test]
async fn a_source_that_cannot_be_fetched_is_a_typed_error_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let scripted = Arc::new(Scripted::of(vec![]));

    let report = run(
        host(
            scripted.clone(),
            dir.path(),
            extensions(dir.path(), "/no/such/server"),
        ),
        load(SOURCES).unwrap(),
        "c5",
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Failed);
    assert!(
        matches!(&report.failure, Some(RunError::Context { node, message }) if node == "architect" && message.contains("`ls`")),
        "{:?}",
        report.failure
    );
    assert_eq!(scripted.calls(), 0);
}

#[tokio::test]
async fn the_gathered_context_is_part_of_the_journaled_prompt() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let store = Store::in_memory().unwrap();
    let scripted = Arc::new(Scripted::of(vec![says("ok")]));
    let yaml = r#"
version: 1
name: journal
channels:
  x: { schema: { type: string }, init: "" }
nodes:
  a:
    kind: agent
    role: r
    prompt: p
    reads: [x]
    max_turns: 3
    context:
      fetch: [{ source: files, globs: ["notes.md"] }]
"#;
    let runner = jensen_harness_workflow::Runner {
        store: store.clone(),
        host: Arc::new(host(scripted, dir.path(), Extensions::default())),
        checkpoints: Arc::new(dagrs::MemoryRunStore::new()),
    };
    runner
        .start(
            load(yaml).unwrap(),
            jensen_harness_workflow::StartRequest {
                run_id: "c6".to_owned(),
                inputs: jensen_harness_workflow::nodes::Values::new(),
                caps: jensen_harness_core::budget::Caps::default(),
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

    let events = store.events("c6.a.0").await.unwrap();
    let started = events
        .iter()
        .find(|e| e.kind == EventKind::PromptStarted)
        .unwrap();
    assert!(
        started.payload["message"]
            .as_str()
            .unwrap()
            .contains("remember the brief")
    );
}
