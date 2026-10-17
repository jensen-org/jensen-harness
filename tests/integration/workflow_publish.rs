use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use dagrs::MemoryRunStore;
use jensen_harness::engine::{ConfiguredModels, ModelSource};
use jensen_harness_core::budget::{BudgetSettings, Caps};
use jensen_harness_core::providers::Provider;
use jensen_harness_core::providers::config::ProviderRegistry;
use jensen_harness_core::secrets::{SecretError, SecretSource};
use jensen_harness_core::store::{EventKind, Store};
use jensen_harness_core::toolset::{Access, ToolSet};
use jensen_harness_promote::forge::{ForgeKind, ForgeTarget, Secret, parse_remote};
use jensen_harness_promote::{Isolation, PromoteError};
use jensen_harness_workflow::nodes::Values;
use jensen_harness_workflow::{
    Host, RoleBinding, RunError, RunReport, RunStatus, Runner, StartRequest, load,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::common::{Scripted, calls_tool, says, tool_set};
use crate::promote_worktree::{Fixture, fixture, git};

const TOKEN: &str = "tok-0123456789-secret";

struct ForgeHost {
    provider: Arc<dyn Provider>,
    repo: PathBuf,
    isolation: Isolation,
    target: ForgeTarget,
    budget: BudgetSettings,
}

impl Host for ForgeHost {
    fn bind(&self, _role: &str) -> Result<RoleBinding, RunError> {
        Ok(RoleBinding {
            provider: Arc::clone(&self.provider),
            model_address: "scripted/m".to_owned(),
        })
    }

    fn tools(&self, _access: Access, root: &std::path::Path) -> Result<ToolSet, RunError> {
        Ok(tool_set(root))
    }

    fn worktree(&self) -> &std::path::Path {
        &self.repo
    }

    fn isolation(&self) -> Option<&Isolation> {
        Some(&self.isolation)
    }

    fn forge(&self, _remote_url: &str) -> Result<ForgeTarget, RunError> {
        Ok(self.target.clone())
    }

    fn budget(&self) -> &BudgetSettings {
        &self.budget
    }
}

const FLOW: &str = r#"
version: 1
name: publish-test
channels:
  task: { schema: { type: string }, init: "" }
  checked: { schema: { type: boolean }, verified: true }
  promoted: { schema: { type: object } }
  published: { schema: { type: object } }
nodes:
  write: { kind: agent, role: implement, prompt: edit the file, reads: [task], tools: writer, max_turns: 5 }
  check: { kind: verifier, run: "true", pass_when: exit_code == 0, passes: checked }
  promote: { kind: promote, from: write, requires: [checked], output: promoted }
  publish: { kind: publish, from: promote, title: Add alpha plus, body: Automated change, output: published }
edges:
  - { from: write, to: check }
  - { from: check, to: promote }
  - { from: promote, to: publish }
"#;

struct Origin {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

fn with_origin(fixture: &Fixture) -> Origin {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("origin.git");
    git(dir.path(), &["init", "-q", "--bare", "origin.git"]);
    git(
        &fixture.repo,
        &["remote", "add", "origin", path.to_str().unwrap()],
    );
    Origin { _dir: dir, path }
}

async fn run_publish(
    fixture: &Fixture,
    kind: ForgeKind,
    api: &MockServer,
    run_id: &str,
) -> (RunReport, Store) {
    let store = Store::in_memory().unwrap();
    let provider: Arc<dyn Provider> = Arc::new(Scripted::of(vec![
        calls_tool(
            "t1",
            "edit",
            json!({"path": "a.txt", "find": "alpha", "replace": "alpha+"}),
        ),
        says("done"),
    ]));
    let runner = Runner {
        store: store.clone(),
        host: Arc::new(ForgeHost {
            provider,
            repo: fixture.repo.clone(),
            isolation: fixture.isolation.clone(),
            target: ForgeTarget {
                kind,
                api_base: Some(api.uri()),
                project: "acme/app".to_owned(),
                token: Secret::new(TOKEN),
            },
            budget: BudgetSettings::default(),
        }),
        checkpoints: Arc::new(MemoryRunStore::new()),
    };
    let report = runner
        .start(
            load(FLOW).unwrap(),
            StartRequest {
                run_id: run_id.to_owned(),
                inputs: Values::new(),
                caps: Caps::default(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    (report, store)
}

fn pull(number: u64) -> serde_json::Value {
    json!({
        "url": format!("https://api.example.invalid/repos/acme/app/pulls/{number}"),
        "id": number,
        "number": number,
        "state": "open",
        "title": "Add alpha plus",
        "html_url": format!("https://github.example.invalid/acme/app/pull/{number}"),
        "head": {"label": "acme:jensen", "ref": "jensen", "sha": "0000000000000000000000000000000000000000"},
        "base": {"label": "acme:main", "ref": "main", "sha": "0000000000000000000000000000000000000000"},
    })
}

async fn github_api(existing: bool) -> MockServer {
    let server = MockServer::start().await;
    let listed = if existing {
        json!([pull(4)])
    } else {
        json!([])
    };
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listed))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/app/pulls"))
        .respond_with(ResponseTemplate::new(201).set_body_json(pull(7)))
        .mount(&server)
        .await;
    server
}

async fn gitlab_api(existing: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v4/user"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 1, "username": "bot", "name": "Bot"})),
        )
        .mount(&server)
        .await;
    let mr =
        json!({"iid": 3, "web_url": "https://gitlab.example.invalid/acme/app/-/merge_requests/3"});
    let listed = if existing {
        json!([mr.clone()])
    } else {
        json!([])
    };
    Mock::given(method("GET"))
        .and(path_regex(r"^/api/v4/projects/.*/merge_requests$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(listed))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/api/v4/projects/.*/merge_requests$"))
        .respond_with(ResponseTemplate::new(201).set_body_json(mr))
        .mount(&server)
        .await;
    server
}

async fn posts(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .count()
}

#[tokio::test]
async fn a_promoted_run_is_pushed_and_opened_as_a_github_pull_request_once() {
    let fixture = fixture();
    let origin = with_origin(&fixture);
    let api = github_api(false).await;

    let (report, store) = run_publish(&fixture, ForgeKind::GitHub, &api, "f1").await;

    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.status, RunStatus::Succeeded);
    let published = report.channels.get("published").unwrap();
    assert_eq!(published["forge"], "github");
    assert_eq!(published["number"], 7);
    assert_eq!(published["existing"], false);
    assert_eq!(posts(&api).await, 1);
    assert_eq!(
        git(&origin.path, &["rev-parse", "refs/heads/jensen/f1"]),
        report.head.unwrap()
    );
    let events = store.events("wf:f1").await.unwrap();
    let kinds: Vec<EventKind> = events.iter().map(|event| event.kind).collect();
    assert!(
        kinds.contains(&EventKind::PublishStarted) && kinds.contains(&EventKind::PublishFinished)
    );
    assert!(
        !format!("{events:?}").contains(TOKEN),
        "the token reached the journal"
    );
    let request = api.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&request.last().unwrap().body).into_owned();
    assert!(
        body.contains("jensen/f1") && body.contains("main"),
        "{body}"
    );
}

#[tokio::test]
async fn a_gitlab_remote_gets_a_merge_request() {
    let fixture = fixture();
    let origin = with_origin(&fixture);
    let api = gitlab_api(false).await;

    let (report, _) = run_publish(&fixture, ForgeKind::GitLab, &api, "f2").await;

    let seen: Vec<String> = api
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| format!("{} {}", r.method, r.url))
        .collect();
    assert!(report.failure.is_none(), "{:?} {seen:?}", report.failure);
    let published = report.channels.get("published").unwrap();
    assert_eq!(published["forge"], "gitlab");
    assert_eq!(published["number"], 3);
    assert_eq!(posts(&api).await, 1);
    assert_eq!(
        git(&origin.path, &["rev-parse", "refs/heads/jensen/f2"]),
        report.head.unwrap()
    );
}

#[tokio::test]
async fn an_open_request_for_the_branch_is_reused_on_both_forges() {
    let fixture = fixture();
    let _origin = with_origin(&fixture);
    let github = github_api(true).await;
    let (report, _) = run_publish(&fixture, ForgeKind::GitHub, &github, "f3").await;
    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.channels["published"]["existing"], true);
    assert_eq!(report.channels["published"]["number"], 4);
    assert_eq!(posts(&github).await, 0);

    let fixture = crate::promote_worktree::fixture();
    let _origin = with_origin(&fixture);
    let gitlab = gitlab_api(true).await;
    let (report, _) = run_publish(&fixture, ForgeKind::GitLab, &gitlab, "f4").await;
    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.channels["published"]["existing"], true);
    assert_eq!(posts(&gitlab).await, 0);
}

#[tokio::test]
async fn a_forge_failure_is_a_typed_error_without_the_token_and_the_branch_stays_pushed() {
    let fixture = fixture();
    let origin = with_origin(&fixture);
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string(format!("boom {TOKEN}")))
        .mount(&api)
        .await;

    let (report, store) = run_publish(&fixture, ForgeKind::GitHub, &api, "f5").await;

    assert_eq!(report.status, RunStatus::Failed);
    let failure = report.failure.unwrap().to_string();
    assert!(!failure.contains(TOKEN), "{failure}");
    assert_eq!(
        git(&origin.path, &["rev-parse", "refs/heads/jensen/f5"]),
        report.head.unwrap()
    );
    let events = store.events("wf:f5").await.unwrap();
    assert!(
        events
            .iter()
            .any(|event| event.kind == EventKind::PublishStarted)
    );
    assert!(
        !events
            .iter()
            .any(|event| event.kind == EventKind::PublishFinished)
    );
}

#[tokio::test]
async fn pushing_to_an_unreachable_remote_fails_without_leaking_the_token() {
    let fixture = fixture();
    let target = ForgeTarget {
        kind: ForgeKind::GitHub,
        api_base: None,
        project: "acme/app".to_owned(),
        token: Secret::new(TOKEN),
    };
    let error = jensen_harness_promote::push::push(
        &fixture.repo,
        "http://127.0.0.1:1/acme/app.git",
        "refs/heads/main",
        "jensen/x",
        &target,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, PromoteError::Push(_)));
    assert!(!error.to_string().contains(TOKEN));
    assert!(!format!("{target:?}").contains(TOKEN));
}

#[test]
fn remote_urls_in_every_common_form_are_understood() {
    for url in [
        "https://github.com/acme/app.git",
        "https://user@github.com/acme/app",
        "git@github.com:acme/app.git",
        "ssh://git@github.com/acme/app.git",
    ] {
        let remote = parse_remote(url).unwrap();
        assert_eq!(
            (remote.host.as_str(), remote.project.as_str()),
            ("github.com", "acme/app"),
            "{url}"
        );
    }
    let nested = parse_remote("https://gitlab.example.com/group/sub/app.git").unwrap();
    assert_eq!(nested.project, "group/sub/app");
    for bad in [
        "/tmp/origin.git",
        "file:///tmp/origin.git",
        "app",
        "https://github.com/",
    ] {
        assert!(
            matches!(parse_remote(bad), Err(PromoteError::UnknownForge(_))),
            "{bad}"
        );
    }
}

struct EnvSecrets(HashMap<String, String>);

impl SecretSource for EnvSecrets {
    fn env(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }

    fn keychain(&self, _service: &str, _user: &str) -> Result<Option<String>, SecretError> {
        Ok(None)
    }
}

fn configured(secrets: &[(&str, &str)]) -> ConfiguredModels {
    let toml = r#"
[providers.p]
transport = "openai_compatible"
base_url = "http://localhost/v1"
api_key_env = "P_KEY"

[forges.github]
api_key_env = "GH_TOKEN"

[forges.gitlab]
base_url = "https://git.corp.example/api"
api_key_env = "GL_TOKEN"
"#;
    ConfiguredModels {
        registry: ProviderRegistry::from_toml(toml).unwrap(),
        secrets: Arc::new(EnvSecrets(
            secrets
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        )),
        default_model: None,
    }
}

#[test]
fn the_forge_is_chosen_from_the_remote_host_and_the_config() {
    let models = configured(&[("GH_TOKEN", "gh-secret"), ("GL_TOKEN", "gl-secret")]);
    let github = models.forge_target("git@github.com:acme/app.git").unwrap();
    assert_eq!(
        (github.kind, github.project.as_str()),
        (ForgeKind::GitHub, "acme/app")
    );
    assert_eq!(github.token.expose(), "gh-secret");
    let gitlab = models
        .forge_target("https://git.corp.example/team/app.git")
        .unwrap();
    assert_eq!(gitlab.kind, ForgeKind::GitLab);
    assert_eq!(
        gitlab.api_base.as_deref(),
        Some("https://git.corp.example/api")
    );
    assert!(matches!(
        models.forge_target("https://bitbucket.org/acme/app.git"),
        Err(RunError::Promote(PromoteError::UnknownForge(_)))
    ));
    let no_token = configured(&[]);
    assert!(matches!(
        no_token.forge_target("https://github.com/acme/app.git"),
        Err(RunError::Provider(_))
    ));
}
