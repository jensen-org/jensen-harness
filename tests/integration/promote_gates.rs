use jensen_harness_promote::{GateConfig, Gates, Report};

use crate::promote_worktree::{Fixture, fixture, provenance};

fn changed(fixture: &Fixture, run: &str, edit: impl FnOnce(&std::path::Path)) -> (String, String) {
    let base = fixture.isolation.begin_run(run).unwrap();
    let node = fixture.isolation.prepare_node(run, "write").unwrap();
    edit(&node.path);
    let snapshot = fixture
        .isolation
        .snapshot(&node, &provenance(run, "write"))
        .unwrap();
    (base, snapshot.commit)
}

fn evaluate(
    fixture: &Fixture,
    config: GateConfig,
    run: &str,
    edit: impl FnOnce(&std::path::Path),
) -> Report {
    let (base, head) = changed(fixture, run, edit);
    Gates::new(config)
        .unwrap()
        .evaluate(&fixture.isolation, &base, &head)
        .unwrap()
}

fn reasons(report: &Report) -> Vec<String> {
    report
        .blocks
        .iter()
        .map(|finding| format!("{}:{}:{}", finding.gate, finding.path, finding.reason))
        .collect()
}

#[test]
fn a_clean_diff_passes() {
    let fixture = fixture();
    let report = evaluate(&fixture, GateConfig::default(), "g1", |dir| {
        std::fs::write(dir.join("a.txt"), "alpha and more\n").unwrap();
        std::fs::write(dir.join("src/new.rs"), "pub fn new() {}\n").unwrap();
    });
    assert!(report.passed(), "{:?}", report.blocks);
    assert!(report.flags.is_empty(), "{:?}", report.flags);
    assert_eq!(report.files, 2);
}

#[test]
fn planted_secrets_block_and_are_never_quoted() {
    let fixture = fixture();
    let aws = [".AKIA", "IOSFODNN7", "EXAMPLE"].concat()[1..].to_owned();
    let pem = format!(
        "-----{} RSA PRIVATE KEY-----\nMIIEow\n-----{} RSA PRIVATE KEY-----\n",
        "BEGIN", "END"
    );
    let gitlab = ["glpat", "-abcdefghij", "klmnopqrstuvwxyz"].concat();
    let url = "https://deploy:hunter2secret@example.invalid/repo.git";
    let report = evaluate(&fixture, GateConfig::default(), "g2", |dir| {
        std::fs::write(dir.join("aws.env"), format!("KEY={aws}\n")).unwrap();
        std::fs::write(dir.join("id_rsa"), &pem).unwrap();
        std::fs::write(dir.join("ci.txt"), format!("token {gitlab}\n")).unwrap();
        std::fs::write(dir.join("remote.txt"), format!("{url}\n")).unwrap();
    });
    let blocked = reasons(&report);
    for (path, rule) in [
        ("aws.env", "aws_access_key"),
        ("id_rsa", "private_key"),
        ("ci.txt", "gitlab_token"),
        ("remote.txt", "credentialed_url"),
    ] {
        assert!(
            blocked
                .iter()
                .any(|line| line.contains(path) && line.contains(rule)),
            "{path} {rule} not in {blocked:?}"
        );
    }
    let everything = format!("{report:?}");
    for secret in [aws.as_str(), "MIIEow", gitlab.as_str(), "hunter2secret"] {
        assert!(
            !everything.contains(secret),
            "{secret} leaked into the report"
        );
    }
}

#[test]
fn protected_paths_and_ci_files_block() {
    let fixture = fixture();
    let config = GateConfig {
        protected: vec!["secrets/**".to_owned()],
        ..GateConfig::default()
    };
    let report = evaluate(&fixture, config, "g3", |dir| {
        std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
        std::fs::write(dir.join(".github/workflows/ci.yml"), "on: push\n").unwrap();
        std::fs::write(dir.join(".gitlab-ci.yml"), "stages: []\n").unwrap();
        std::fs::create_dir_all(dir.join("secrets")).unwrap();
        std::fs::write(dir.join("secrets/note.txt"), "x\n").unwrap();
        std::fs::write(dir.join(".gitmodules"), "[submodule]\n").unwrap();
    });
    let blocked = reasons(&report);
    for path in [
        ".github/workflows/ci.yml",
        ".gitlab-ci.yml",
        "secrets/note.txt",
        ".gitmodules",
    ] {
        assert!(
            blocked
                .iter()
                .any(|line| line.starts_with("path:") && line.contains(path)),
            "{path} not blocked in {blocked:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_new_symlink_blocks() {
    let fixture = fixture();
    let report = evaluate(&fixture, GateConfig::default(), "g4", |dir| {
        std::os::unix::fs::symlink("a.txt", dir.join("link")).unwrap();
    });
    assert!(
        reasons(&report)
            .iter()
            .any(|line| line.contains("link") && line.contains("symlink"))
    );
}

#[test]
fn size_and_binary_limits_block() {
    let fixture = fixture();
    let config = GateConfig {
        max_changed_files: 2,
        max_added_bytes: 100,
        ..GateConfig::default()
    };
    let report = evaluate(&fixture, config, "g5", |dir| {
        std::fs::write(dir.join("one.txt"), "x".repeat(200)).unwrap();
        std::fs::write(dir.join("two.txt"), "y").unwrap();
        std::fs::write(dir.join("three.txt"), "z").unwrap();
        std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    });
    let blocked = reasons(&report);
    assert!(
        blocked.iter().any(|line| line.contains("files changed")),
        "{blocked:?}"
    );
    assert!(
        blocked.iter().any(|line| line.contains("bytes added")),
        "{blocked:?}"
    );
    assert!(
        blocked
            .iter()
            .any(|line| line.contains("blob.bin") && line.contains("binary")),
        "{blocked:?}"
    );

    let allowed = evaluate(
        &fixture,
        GateConfig {
            allow_binary: true,
            ..GateConfig::default()
        },
        "g6",
        |dir| std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 3]).unwrap(),
    );
    assert!(allowed.passed(), "{:?}", allowed.blocks);
}

#[test]
fn risk_flags_inform_without_blocking() {
    let fixture = fixture();
    let report = evaluate(&fixture, GateConfig::default(), "g7", |dir| {
        std::fs::write(
            dir.join("src/b.rs"),
            "fn b() { unsafe { core::hint::unreachable_unchecked() } }\nfn run() { let _ = std::process::Command::new(\"ls\"); }\n",
        )
        .unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(
            dir.join("tool.py"),
            "import subprocess\nsubprocess.run(['ls'])\neval('1')\n",
        )
        .unwrap();
    });
    assert!(report.passed(), "{:?}", report.blocks);
    let flags: Vec<String> = report
        .flags
        .iter()
        .map(|finding| format!("{}:{}", finding.path, finding.reason))
        .collect();
    for expected in [
        "src/b.rs:unsafe code",
        "src/b.rs:spawns a process",
        "Cargo.toml:build or dependency manifest changed",
        "tool.py:spawns a process",
        "tool.py:evaluates code",
    ] {
        assert!(
            flags.iter().any(|flag| flag == expected),
            "{expected} not in {flags:?}"
        );
    }
}

#[test]
fn constructs_that_were_already_there_are_not_flagged() {
    let fixture = fixture();
    let existing = fixture.repo.join("src/old.rs");
    std::fs::write(&existing, "fn old() { unsafe { } }\n").unwrap();
    crate::promote_worktree::git(&fixture.repo, &["add", "-A"]);
    crate::promote_worktree::git(&fixture.repo, &["commit", "-q", "-m", "old"]);
    let report = evaluate(&fixture, GateConfig::default(), "g8", |dir| {
        std::fs::write(
            dir.join("src/old.rs"),
            "fn old() { unsafe { } }\nfn other() {}\n",
        )
        .unwrap();
    });
    assert!(report.flags.is_empty(), "{:?}", report.flags);
}

#[test]
fn case_and_normalization_tricks_do_not_slip_past_the_path_guard() {
    let fixture = fixture();
    let report = evaluate(&fixture, GateConfig::default(), "g9", |dir| {
        std::fs::create_dir_all(dir.join(".Github/Workflows")).unwrap();
        std::fs::write(dir.join(".Github/Workflows/ci.yml"), "on: push\n").unwrap();
        std::fs::write(dir.join("GITMOD~1"), "x\n").unwrap();
        std::fs::write(dir.join(".gitmodules."), "x\n").unwrap();
        std::fs::write(dir.join(".git\u{200b}modules"), "x\n").unwrap();
    });
    let blocked = reasons(&report);
    for needle in [
        ".Github/Workflows/ci.yml",
        "GITMOD~1",
        ".gitmodules.",
        ".git\u{200b}modules",
    ] {
        assert!(
            blocked
                .iter()
                .any(|line| line.starts_with("path:") && line.contains(needle)),
            "{needle:?} not blocked in {blocked:?}"
        );
    }
}

#[test]
fn a_binary_file_is_still_scanned_for_secrets() {
    let fixture = fixture();
    let key = ["AKIA", "IOSFODNN7", "EXAMPLE"].concat();
    let config = GateConfig {
        allow_binary: true,
        ..GateConfig::default()
    };
    let report = evaluate(&fixture, config, "g10", |dir| {
        let mut bytes = vec![0u8, 1, 2, 3];
        bytes.extend_from_slice(key.as_bytes());
        std::fs::write(dir.join("data.bin"), bytes).unwrap();
    });
    assert!(
        reasons(&report)
            .iter()
            .any(|line| line.contains("data.bin") && line.contains("aws_access_key")),
        "{:?}",
        report.blocks
    );
}

#[test]
fn a_dot_git_directory_in_any_case_cannot_even_be_snapshotted() {
    let fixture = fixture();
    fixture.isolation.begin_run("g11").unwrap();
    let node = fixture.isolation.prepare_node("g11", "write").unwrap();
    std::fs::create_dir_all(node.path.join("sub/.GIT")).unwrap();
    std::fs::write(node.path.join("sub/.GIT/config"), "x\n").unwrap();
    assert!(
        fixture
            .isolation
            .snapshot(&node, &provenance("g11", "write"))
            .is_err()
    );
}

#[test]
fn operator_patterns_match_nested_paths_and_windows_tricks_are_refused() {
    let fixture = fixture();
    let config = GateConfig {
        protected: vec!["*.pem".to_owned()],
        ..GateConfig::default()
    };
    let report = evaluate(&fixture, config, "g12", |dir| {
        std::fs::create_dir_all(dir.join("deep/er")).unwrap();
        std::fs::write(dir.join("deep/er/cert.PEM"), "x\n").unwrap();
        std::fs::write(dir.join("trailing."), "x\n").unwrap();
        std::fs::write(dir.join("stream:name"), "x\n").unwrap();
        std::fs::write(dir.join(".git\u{206b}modules"), "x\n").unwrap();
    });
    let blocked = reasons(&report);
    for needle in [
        "deep/er/cert.PEM",
        "trailing.",
        "stream:name",
        ".git\u{206b}modules",
    ] {
        assert!(
            blocked
                .iter()
                .any(|line| line.starts_with("path:") && line.contains(needle)),
            "{needle:?} not blocked in {blocked:?}"
        );
    }
}
