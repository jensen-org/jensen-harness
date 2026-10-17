use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dagrs::MemoryRunStore;
use jensen_harness_core::budget::{BudgetSettings, Caps};
use jensen_harness_core::providers::Provider;
use jensen_harness_core::toolset::{Access, ToolSet};
use jensen_harness_promote::Isolation;
use jensen_harness_workflow::nodes::Values;
use jensen_harness_workflow::{
    Host, RoleBinding, RunError, RunReport, RunStatus, Runner, StartRequest, load,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::common::{Scripted, calls_tool, says, tool_set};
use crate::promote_worktree::{Fixture, fixture, git};

struct RepoHost {
    provider: Arc<dyn Provider>,
    repo: PathBuf,
    isolation: Isolation,
    budget: BudgetSettings,
}

impl Host for RepoHost {
    fn bind(&self, _role: &str) -> Result<RoleBinding, RunError> {
        Ok(RoleBinding {
            provider: Arc::clone(&self.provider),
            model_address: "scripted/m".to_owned(),
        })
    }

    fn tools(&self, access: Access, root: &std::path::Path) -> Result<ToolSet, RunError> {
        let tools = tool_set(root);
        Ok(match access {
            Access::ReadOnly => tools.read_only(),
            Access::ReadWrite => tools,
        })
    }

    fn worktree(&self) -> &std::path::Path {
        &self.repo
    }

    fn isolation(&self) -> Option<&Isolation> {
        Some(&self.isolation)
    }

    fn forge(
        &self,
        remote_url: &str,
    ) -> Result<jensen_harness_promote::forge::ForgeTarget, RunError> {
        Err(jensen_harness_promote::PromoteError::UnknownForge(remote_url.to_owned()).into())
    }

    fn budget(&self) -> &BudgetSettings {
        &self.budget
    }
}

fn runner(fixture: &Fixture, provider: Arc<dyn Provider>) -> Runner {
    Runner {
        store: jensen_harness_core::store::Store::in_memory().unwrap(),
        host: Arc::new(RepoHost {
            provider,
            repo: fixture.repo.clone(),
            isolation: fixture.isolation.clone(),
            budget: BudgetSettings::default(),
        }),
        checkpoints: Arc::new(MemoryRunStore::new()),
    }
}

async fn run(runner: &Runner, yaml: &str, run_id: &str) -> RunReport {
    runner
        .start(
            load(yaml).unwrap(),
            StartRequest {
                run_id: run_id.to_owned(),
                inputs: Values::new(),
                caps: Caps::default(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

fn flow(verify: &str) -> String {
    format!(
        r#"
version: 1
name: promote-test
channels:
  task: {{ schema: {{ type: string }}, init: "" }}
  checked: {{ schema: {{ type: boolean }}, verified: true }}
  promoted: {{ schema: {{ type: object }} }}
nodes:
  write: {{ kind: agent, role: implement, prompt: edit the file, reads: [task], tools: writer, max_turns: 5 }}
  check: {{ kind: verifier, run: "{verify}", pass_when: exit_code == 0, passes: checked }}
  promote: {{ kind: promote, from: write, requires: [checked], output: promoted }}
edges:
  - {{ from: write, to: check }}
  - {{ from: check, to: promote }}
"#
    )
}

fn edit(replace: &str) -> jensen_harness_core::providers::CompletionResponse {
    calls_tool(
        "t1",
        "edit",
        json!({"path": "a.txt", "find": "alpha", "replace": replace}),
    )
}

fn commits(fixture: &Fixture, branch: &str) -> usize {
    git(
        &fixture.repo,
        &["rev-list", "--count", &format!("main..{branch}")],
    )
    .parse()
    .unwrap()
}

#[tokio::test]
async fn a_verified_writer_is_promoted_as_one_commit_with_provenance() {
    let fixture = fixture();
    let provider: Arc<dyn Provider> = Arc::new(Scripted::of(vec![edit("alpha+"), says("done")]));
    let report = run(
        &runner(&fixture, provider),
        &flow("grep -q 'alpha+' a.txt"),
        "p1",
    )
    .await;

    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(
        report.outcomes.get("promote").map(String::as_str),
        Some("ok")
    );
    let branch = report.integration_branch.clone().unwrap();
    assert_eq!(commits(&fixture, &branch), 1);
    assert_eq!(
        report.head.clone().unwrap(),
        git(&fixture.repo, &["rev-parse", &branch])
    );
    let message = git(&fixture.repo, &["log", "-1", "--format=%B", &branch]);
    assert!(
        message.contains("Jensen-Node: write") && message.contains("Jensen-Model: scripted/m"),
        "{message}"
    );
    assert_eq!(
        git(&fixture.repo, &["show", &format!("{branch}:a.txt")]),
        "alpha+"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        git(&fixture.repo, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert!(!fixture.workspaces.join("p1").join("write").exists());
    let promoted = report.channels.get("promoted").unwrap();
    assert_eq!(
        promoted.get("commit"),
        Some(&Value::String(report.head.clone().unwrap()))
    );
}

#[tokio::test]
async fn a_failed_verifier_stops_promotion_and_leaves_the_branch_alone() {
    let fixture = fixture();
    let provider: Arc<dyn Provider> = Arc::new(Scripted::of(vec![edit("alpha+"), says("done")]));
    let report = run(
        &runner(&fixture, provider),
        &flow("grep -q nothing-matches a.txt"),
        "p2",
    )
    .await;

    assert_eq!(
        report.outcomes.get("promote").map(String::as_str),
        Some("blocked")
    );
    let branch = report.integration_branch.clone().unwrap();
    assert_eq!(commits(&fixture, &branch), 0);
    let blocked = report
        .channels
        .get("promoted")
        .unwrap()
        .get("blocked")
        .unwrap()
        .to_string();
    assert!(blocked.contains("verify"), "{blocked}");
    assert!(
        fixture.workspaces.join("p2").join("write").exists(),
        "kept for inspection"
    );
}

#[tokio::test]
async fn a_secret_is_blocked_and_never_quoted() {
    let fixture = fixture();
    let key = ["AKIA", "IOSFODNN7", "EXAMPLE"].concat();
    let provider: Arc<dyn Provider> = Arc::new(Scripted::of(vec![edit(&key), says("done")]));
    let report = run(&runner(&fixture, provider), &flow("true"), "p3").await;

    assert_eq!(
        report.outcomes.get("promote").map(String::as_str),
        Some("blocked")
    );
    let branch = report.integration_branch.clone().unwrap();
    assert_eq!(commits(&fixture, &branch), 0);
    let everything = format!("{report:?}");
    assert!(everything.contains("aws_access_key"));
    assert!(!everything.contains(&key));
}

#[tokio::test]
async fn a_protected_path_is_blocked() {
    let fixture = fixture();
    let provider: Arc<dyn Provider> = Arc::new(Scripted::of(vec![
        calls_tool(
            "t1",
            "shell",
            json!({"command": "printf 'stages: []' > .gitlab-ci.yml"}),
        ),
        says("done"),
    ]));
    let report = run(&runner(&fixture, provider), &flow("true"), "p4").await;
    assert_eq!(
        report.outcomes.get("promote").map(String::as_str),
        Some("blocked")
    );
    let blocked = report.channels.get("promoted").unwrap().to_string();
    assert!(blocked.contains(".gitlab-ci.yml"), "{blocked}");
}

#[tokio::test]
async fn a_read_only_agent_cannot_change_a_file() {
    let fixture = fixture();
    let provider: Arc<dyn Provider> = Arc::new(Scripted::of(vec![edit("changed"), says("done")]));
    let yaml = r#"
version: 1
name: reader
channels:
  x: { schema: { type: string }, init: "" }
nodes:
  look: { kind: agent, role: r, prompt: p, reads: [x], max_turns: 4 }
"#;
    let report = run(&runner(&fixture, provider), yaml, "p5").await;
    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(
        std::fs::read_to_string(fixture.repo.join("a.txt")).unwrap(),
        "alpha\n"
    );
}

#[tokio::test]
async fn a_run_dropped_after_promotion_resumes_without_a_second_commit() {
    let fixture = fixture();
    let checkpoints = tempfile::tempdir().unwrap();
    let go = fixture.workspaces.join("go");
    let after = format!(
        "  after: {{ kind: command, run: 'touch after.started; while [ ! -f {} ]; do sleep 0.05; done; cat a.txt', output: seen, idempotent: true, timeout_secs: 30 }}\nedges:",
        go.display()
    );
    let yaml = flow("true")
        .replacen(
            "channels:",
            "channels:\n  seen: { schema: { type: object } }",
            1,
        )
        .replacen("edges:", &after, 1)
        + "  - { from: promote, to: after }\n";
    let provider: Arc<dyn Provider> = Arc::new(Scripted::of(vec![edit("alpha+"), says("done")]));
    let store = jensen_harness_core::store::Store::in_memory().unwrap();
    let build = |provider: Arc<dyn Provider>| Runner {
        store: store.clone(),
        host: Arc::new(RepoHost {
            provider,
            repo: fixture.repo.clone(),
            isolation: fixture.isolation.clone(),
            budget: BudgetSettings::default(),
        }),
        checkpoints: Arc::new(dagrs::FileRunStore::new(checkpoints.path()).unwrap()),
    };
    let first = build(provider);
    let running = tokio::spawn({
        let yaml = yaml.clone();
        async move {
            first
                .start(
                    load(&yaml).unwrap(),
                    StartRequest {
                        run_id: "p6".to_owned(),
                        inputs: Values::new(),
                        caps: Caps::default(),
                    },
                    CancellationToken::new(),
                )
                .await
        }
    });
    let marker = fixture
        .workspaces
        .join("p6")
        .join("after.read")
        .join("after.started");
    let waited = std::time::Instant::now();
    while !marker.exists() {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            waited.elapsed() < Duration::from_secs(20),
            "the last node never started"
        );
    }
    running.abort();
    let _ = running.await;
    std::fs::write(&go, "").unwrap();
    let branch = Isolation::integration_ref("p6");
    let before = git(&fixture.repo, &["rev-parse", &branch]);

    let report = build(Arc::new(Scripted::of(vec![])))
        .resume(load(&yaml).unwrap(), "p6", CancellationToken::new())
        .await
        .unwrap();

    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(git(&fixture.repo, &["rev-parse", &branch]), before);
    assert_eq!(commits(&fixture, &branch), 1);
    assert_eq!(
        report.channels["seen"]["stdout"], "alpha+\n",
        "the check saw the promoted work"
    );
    assert_eq!(
        git(&fixture.repo, &["status", "--porcelain"]),
        "",
        "the user's checkout is untouched"
    );
    assert!(!fixture.workspaces.join("p6").join("after.read").exists());
}
