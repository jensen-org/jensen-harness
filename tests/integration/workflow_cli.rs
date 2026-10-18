use std::process::Command;

use crate::common::binary_path;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(binary_path()).args(args).output().unwrap()
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
