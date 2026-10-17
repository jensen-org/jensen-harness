use std::process::Command;

use crate::common::binary_path;

const TRIAGE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/workflows/examples/triage.yaml"
);

fn run(args: &[&str]) -> std::process::Output {
    Command::new(binary_path()).args(args).output().unwrap()
}

#[test]
fn validate_accepts_the_example_and_prints_its_hash() {
    let output = run(&["workflow", "validate", TRIAGE]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("ok triage "), "{stdout}");
    assert!(stdout.contains("(3 waves)"), "{stdout}");
}

#[test]
fn validate_refuses_a_broken_workflow_naming_the_node() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("broken.yaml");
    std::fs::write(
        &file,
        "version: 1\nname: broken\nnodes:\n  a: {kind: agent, role: r, prompt: p}\n",
    )
    .unwrap();
    let output = run(&["workflow", "validate", file.to_str().unwrap()]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("agent `a`"), "{stderr}");
}

#[test]
fn dot_draws_every_node() {
    let output = run(&["workflow", "dot", TRIAGE]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("digraph"), "{stdout}");
    for node in ["inspect_code", "inspect_docs", "check", "route"] {
        assert!(stdout.contains(node), "{node} missing from {stdout}");
    }
}
