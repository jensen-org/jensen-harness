use std::path::{Path, PathBuf};
use std::sync::Arc;

use jensen_harness_core::budget::BudgetSettings;
use jensen_harness_core::providers::Provider;
use jensen_harness_core::toolset::ToolProvider;
use jensen_harness_extensions::{Extensions, SkillCatalog, SkillError, SkillProvider};
use jensen_harness_workflow::{RunError, RunStatus, load};
use serde_json::json;

use crate::common::{Scripted, says};
use crate::ext_host::{ExtHost, run};

fn skill(root: &Path, name: &str, front: &str, body: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), format!("---\n{front}\n---\n{body}")).unwrap();
    dir
}

fn catalog(root: &Path) -> SkillCatalog {
    SkillCatalog::discover(&[root.to_path_buf()]).unwrap()
}

fn refused(root: &Path) -> String {
    match SkillCatalog::discover(&[root.to_path_buf()]) {
        Err(SkillError::Invalid { path, reason }) => format!("{path}: {reason}"),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn skills_are_discovered_with_their_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    skill(
        dir.path(),
        "design-review",
        "name: design-review\ndescription: Review a design against the system.\nlicense: MIT\nallowed-tools: read grep",
        "# Steps\nLook first.\n",
    );
    let found = catalog(dir.path());
    let loaded = found.get("design-review").unwrap();
    assert_eq!(loaded.description, "Review a design against the system.");
    assert_eq!(loaded.license.as_deref(), Some("MIT"));
    assert_eq!(loaded.allowed_tools, ["read", "grep"]);
    assert!(loaded.body.starts_with("# Steps"));
    assert_eq!(found.names().collect::<Vec<_>>(), ["design-review"]);
}

#[test]
fn a_skill_that_breaks_the_specification_is_refused_naming_the_file() {
    let long = "x".repeat(65);
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "upper",
            "name: Upper\ndescription: d".to_owned(),
            "lowercase",
        ),
        (
            "-lead",
            "name: -lead\ndescription: d".to_owned(),
            "lowercase",
        ),
        (
            "dou--ble",
            "name: dou--ble\ndescription: d".to_owned(),
            "lowercase",
        ),
        (
            "other",
            "name: different\ndescription: d".to_owned(),
            "must match its directory",
        ),
        (&long, format!("name: {long}\ndescription: d"), "lowercase"),
        (
            "empty",
            "name: empty\ndescription: ''".to_owned(),
            "description",
        ),
        (
            "huge",
            format!("name: huge\ndescription: {}", "d".repeat(1025)),
            "description",
        ),
        (
            "compat",
            format!(
                "name: compat\ndescription: d\ncompatibility: {}",
                "c".repeat(501)
            ),
            "compatibility",
        ),
    ];
    for (directory, front, expected) in cases {
        let dir = tempfile::tempdir().unwrap();
        skill(dir.path(), directory, &front, "body");
        let message = refused(dir.path());
        assert!(
            message.contains("SKILL.md") && message.contains(expected),
            "{directory}: {message}"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("SKILL.md"), "no frontmatter").unwrap();
    assert!(refused(dir.path()).contains("frontmatter"));
}

#[test]
fn a_name_in_two_roots_is_an_error_and_unknown_names_are_refused() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    skill(
        first.path(),
        "same",
        "name: same\ndescription: from the first",
        "one",
    );
    skill(
        second.path(),
        "same",
        "name: same\ndescription: from the second",
        "two",
    );
    skill(
        second.path(),
        "extra",
        "name: extra\ndescription: only here",
        "x",
    );
    let roots = [first.path().to_path_buf(), second.path().to_path_buf()];
    assert!(matches!(
        SkillCatalog::discover(&roots),
        Err(SkillError::Shadowed { name, .. }) if name == "same"
    ));
    let found = SkillCatalog::discover(&roots[1..]).unwrap();
    assert!(matches!(
        found.select(&["nope".to_owned()]),
        Err(SkillError::Unknown(name)) if name == "nope"
    ));
}

#[cfg(unix)]
#[test]
fn a_skill_that_is_a_symlink_or_holds_one_out_of_its_root_is_refused() {
    let outside = tempfile::tempdir().unwrap();
    skill(
        outside.path(),
        "evil",
        "name: evil\ndescription: lives elsewhere",
        "x",
    );
    let root = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path().join("evil"), root.path().join("evil")).unwrap();
    assert!(
        SkillCatalog::discover(&[root.path().to_path_buf()]).is_err()
            || SkillCatalog::discover(&[root.path().to_path_buf()])
                .unwrap()
                .get("evil")
                .is_none()
    );

    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("linked");
    std::fs::create_dir_all(&dir).unwrap();
    let target = outside.path().join("SKILL.md");
    std::fs::write(&target, "---\nname: linked\ndescription: d\n---\nbody").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("SKILL.md")).unwrap();
    assert!(refused(root.path()).contains("not a regular file"));
}

#[test]
fn an_oversize_skill_or_bundled_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    skill(
        dir.path(),
        "big",
        "name: big\ndescription: d",
        &"x".repeat(300 * 1024),
    );
    assert!(refused(dir.path()).contains("over"));
}

async fn call(provider: &SkillProvider, input: serde_json::Value) -> (String, bool) {
    let reply = provider.invoke("skill", &input).await.unwrap();
    (reply.text, reply.is_error)
}

#[tokio::test]
async fn the_prompt_note_lists_descriptions_and_the_tool_loads_the_body_and_bundled_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = skill(
        dir.path(),
        "audit",
        "name: audit\ndescription: Audit a change.",
        "SECRET BODY",
    );
    std::fs::create_dir_all(home.join("references")).unwrap();
    std::fs::write(home.join("references/checklist.md"), "check one").unwrap();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, "do not read").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, home.join("references/escape")).unwrap();
    let provider = SkillProvider::new(catalog(dir.path()).select(&["audit".to_owned()]).unwrap());

    let note = provider.system_note().unwrap();
    assert!(note.contains("audit: Audit a change."));
    assert!(
        !note.contains("SECRET BODY"),
        "only descriptions are in the prompt"
    );
    assert_eq!(provider.specs()[0].name, "skill");

    assert_eq!(
        call(&provider, json!({"name": "audit"})).await,
        ("SECRET BODY".to_owned(), false)
    );
    assert_eq!(
        call(
            &provider,
            json!({"name": "audit", "file": "references/checklist.md"})
        )
        .await,
        ("check one".to_owned(), false)
    );
    for file in [
        "../outside.txt",
        "/etc/passwd",
        "references/../../outside.txt",
        "references/escape",
    ] {
        let (text, is_error) = call(&provider, json!({"name": "audit", "file": file})).await;
        assert!(is_error && !text.contains("do not read"), "{file}: {text}");
    }
    let (text, is_error) = call(&provider, json!({"name": "other"})).await;
    assert!(is_error && text.contains("no attached skill"));
}

const FLOW: &str = r#"
version: 1
name: skills
channels:
  x: { schema: { type: string }, init: "" }
nodes:
  designer: { kind: agent, role: design, prompt: p, reads: [x], max_turns: 3, skills: [audit] }
"#;

fn host(provider: Arc<dyn Provider>, dir: &Path, extensions: Extensions) -> ExtHost {
    ExtHost {
        provider,
        dir: dir.to_path_buf(),
        extensions,
        budget: BudgetSettings::default(),
    }
}

#[tokio::test]
async fn an_attached_skill_reaches_the_model_as_a_description_and_a_tool() {
    let dir = tempfile::tempdir().unwrap();
    skill(
        dir.path(),
        "audit",
        "name: audit\ndescription: Audit a change.",
        "SECRET BODY",
    );
    let scripted = Arc::new(Scripted::of(vec![says("done")]));
    let extensions = Extensions {
        skills: catalog(dir.path()),
        ..Extensions::default()
    };

    let report = run(
        host(scripted.clone(), dir.path(), extensions),
        load(FLOW).unwrap(),
        "s1",
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Succeeded, "{:?}", report.failure);
    let request = scripted.request(0);
    assert!(request.system.contains("audit: Audit a change."));
    assert!(!request.system.contains("SECRET BODY"));
    assert!(request.tools.iter().any(|tool| tool.name == "skill"));
}

#[tokio::test]
async fn a_node_naming_an_unknown_skill_stops_the_run_before_it_starts() {
    let dir = tempfile::tempdir().unwrap();
    let scripted = Arc::new(Scripted::of(vec![]));

    let error = run(
        host(scripted.clone(), dir.path(), Extensions::default()),
        load(FLOW).unwrap(),
        "s2",
    )
    .await
    .unwrap_err();

    assert!(
        matches!(&error, RunError::Skill { node, source: SkillError::Unknown(name) } if node == "designer" && name == "audit"),
        "{error}"
    );
    assert_eq!(scripted.calls(), 0);
}
