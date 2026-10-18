use std::path::{Path, PathBuf};
use std::process::Command;

use jensen_harness_promote::{Isolation, PromoteError, Provenance};

pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub(crate) struct Fixture {
    _dir: tempfile::TempDir,
    pub(crate) repo: PathBuf,
    pub(crate) workspaces: PathBuf,
    pub(crate) isolation: Isolation,
}

pub(crate) fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(repo.join("src/b.rs"), "fn b() {}\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let workspaces = root.join("state.workspaces");
    let isolation = Isolation::open(&repo, &workspaces).unwrap();
    Fixture {
        _dir: dir,
        repo,
        workspaces,
        isolation,
    }
}

pub(crate) fn provenance(run: &str, node: &str) -> Provenance {
    Provenance {
        run: run.to_owned(),
        node: node.to_owned(),
        session: "session-1".to_owned(),
        model: "fake/model".to_owned(),
        spec: "specHash".to_owned(),
    }
}

#[test]
fn a_run_gets_an_integration_branch_at_head_and_a_node_a_worktree_on_its_own_branch() {
    let fixture = fixture();
    let head = git(&fixture.repo, &["rev-parse", "HEAD"]);
    assert_eq!(fixture.isolation.begin_run("r1").unwrap(), head);
    assert_eq!(fixture.isolation.begin_run("r1").unwrap(), head);

    let node = fixture.isolation.prepare_node("r1", "write").unwrap();

    assert_eq!(node.path, fixture.workspaces.join("r1").join("write"));
    assert_eq!(
        std::fs::read_to_string(node.path.join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert!(node.path.join(".git").is_file());
    assert_eq!(
        git(
            &fixture.repo,
            &["rev-parse", "refs/heads/jensen/r1/node/write"]
        ),
        head
    );
    let again = fixture.isolation.prepare_node("r1", "write").unwrap();
    assert_eq!(again.path, node.path);
    assert_eq!(git(&fixture.repo, &["status", "--porcelain"]), "");
    assert_eq!(
        git(&fixture.repo, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
}

#[test]
fn a_snapshot_commits_the_files_with_provenance_and_is_idempotent() {
    let fixture = fixture();
    let head = fixture.isolation.begin_run("r2").unwrap();
    let node = fixture.isolation.prepare_node("r2", "write").unwrap();
    std::fs::write(node.path.join("a.txt"), "alpha changed\n").unwrap();
    std::fs::write(node.path.join("new.sh"), "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            node.path.join("new.sh"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    std::fs::create_dir_all(node.path.join("target")).unwrap();
    std::fs::write(node.path.join("target/out.bin"), "build output").unwrap();

    let first = fixture
        .isolation
        .snapshot(&node, &provenance("r2", "write"))
        .unwrap();
    let second = fixture
        .isolation
        .snapshot(&node, &provenance("r2", "write"))
        .unwrap();

    assert!(first.created);
    assert_eq!(first.parent, head);
    assert!(!second.created);
    assert_eq!(second.commit, first.commit);
    let message = git(&fixture.repo, &["log", "-1", "--format=%B", &first.commit]);
    for trailer in [
        "Jensen-Run: r2",
        "Jensen-Node: write",
        "Jensen-Session: session-1",
        "Jensen-Model: fake/model",
        "Jensen-Spec: specHash",
    ] {
        assert!(
            message.contains(trailer),
            "{trailer} missing from {message}"
        );
    }
    let listing = git(&fixture.repo, &["ls-tree", "-r", &first.commit]);
    assert!(listing.contains("100755"), "{listing}");
    assert!(listing.contains("new.sh"));
    assert!(
        !listing.contains("target/out.bin"),
        "build output must stay out"
    );
    assert!(listing.lines().all(|line| !line.ends_with("\t.git")));
    assert_eq!(
        git(&fixture.repo, &["show", &format!("{}:a.txt", first.commit)]),
        "alpha changed"
    );
    assert_eq!(git(&fixture.repo, &["rev-parse", "main"]), head);
    assert_eq!(
        fixture.isolation.integration_head("r2").unwrap(),
        head,
        "a snapshot never moves the integration branch"
    );
}

#[test]
fn an_unchanged_worktree_makes_no_commit() {
    let fixture = fixture();
    let head = fixture.isolation.begin_run("r3").unwrap();
    let node = fixture.isolation.prepare_node("r3", "write").unwrap();
    let snapshot = fixture
        .isolation
        .snapshot(&node, &provenance("r3", "write"))
        .unwrap();
    assert!(!snapshot.created);
    assert_eq!(snapshot.commit, head);
}

#[test]
fn a_rewritten_git_pointer_is_refused_before_anything_is_read() {
    let fixture = fixture();
    fixture.isolation.begin_run("r4").unwrap();
    let node = fixture.isolation.prepare_node("r4", "write").unwrap();
    let evil = fixture.workspaces.join("evil-gitdir");
    std::fs::create_dir_all(&evil).unwrap();
    std::fs::write(
        evil.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tfsmonitor = touch /tmp/jensen-pwned\n",
    )
    .unwrap();
    std::fs::write(
        node.path.join(".git"),
        format!("gitdir: {}\n", evil.display()),
    )
    .unwrap();

    let refused = fixture
        .isolation
        .snapshot(&node, &provenance("r4", "write"))
        .unwrap_err();
    assert!(
        matches!(refused, PromoteError::TamperedWorktree { .. }),
        "{refused}"
    );
    let again = fixture.isolation.prepare_node("r4", "write").unwrap_err();
    assert!(matches!(again, PromoteError::TamperedWorktree { .. }));
    assert!(!Path::new("/tmp/jensen-pwned").exists());
}

#[test]
fn a_hostile_private_config_is_never_read_so_its_command_never_runs() {
    let fixture = fixture();
    fixture.isolation.begin_run("r5").unwrap();
    let node = fixture.isolation.prepare_node("r5", "write").unwrap();
    let marker = fixture.workspaces.join("ran");
    let private = fixture.repo.join(".git/worktrees/write");
    std::fs::write(
        private.join("config.worktree"),
        format!("[core]\n\tfsmonitor = touch {}\n", marker.display()),
    )
    .unwrap();
    std::fs::write(
        fixture.repo.join(".git/config"),
        std::fs::read_to_string(fixture.repo.join(".git/config")).unwrap()
            + "[extensions]\n\tworktreeConfig = true\n",
    )
    .unwrap();
    std::fs::write(node.path.join("x.txt"), "x").unwrap();

    let snapshot = fixture
        .isolation
        .snapshot(&node, &provenance("r5", "write"))
        .unwrap();

    assert!(snapshot.created);
    assert!(!marker.exists(), "the planted fsmonitor command ran");
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_worktree_is_refused_and_one_inside_is_kept() {
    let fixture = fixture();
    fixture.isolation.begin_run("r6").unwrap();
    let node = fixture.isolation.prepare_node("r6", "write").unwrap();
    std::os::unix::fs::symlink("a.txt", node.path.join("inside")).unwrap();
    let kept = fixture
        .isolation
        .snapshot(&node, &provenance("r6", "write"))
        .unwrap();
    assert!(git(&fixture.repo, &["ls-tree", &kept.commit]).contains("120000"));

    std::os::unix::fs::symlink("../../../etc/passwd", node.path.join("out")).unwrap();
    let refused = fixture
        .isolation
        .snapshot(&node, &provenance("r6", "write"))
        .unwrap_err();
    assert!(
        matches!(refused, PromoteError::EscapingSymlink { .. }),
        "{refused}"
    );
    std::fs::remove_file(node.path.join("out")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", node.path.join("abs")).unwrap();
    let refused = fixture
        .isolation
        .snapshot(&node, &provenance("r6", "write"))
        .unwrap_err();
    assert!(matches!(refused, PromoteError::EscapingSymlink { .. }));
}

#[test]
fn a_plain_directory_and_unsafe_names_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let refused = Isolation::open(dir.path(), &dir.path().join("w")).unwrap_err();
    assert!(matches!(refused, PromoteError::NotARepository { .. }));

    let fixture = fixture();
    for name in ["", ".hidden", "a/b", "x..y", "n.lock", "sp ace"] {
        assert!(
            matches!(
                fixture.isolation.begin_run(name),
                Err(PromoteError::InvalidName(_))
            ),
            "{name:?}"
        );
    }
}

#[test]
fn removing_a_node_takes_its_worktree_away() {
    let fixture = fixture();
    fixture.isolation.begin_run("r7").unwrap();
    let node = fixture.isolation.prepare_node("r7", "write").unwrap();
    fixture.isolation.remove_node(&node).unwrap();
    assert!(!node.path.exists());
    assert_eq!(
        git(&fixture.repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

fn node_with(fixture: &Fixture, run: &str, node: &str, file: &str, text: &str) -> String {
    let handle = fixture.isolation.prepare_node(run, node).unwrap();
    std::fs::write(handle.path.join(file), text).unwrap();
    fixture
        .isolation
        .snapshot(&handle, &provenance(run, node))
        .unwrap()
        .commit
}

fn cleared(fixture: &Fixture, run: &str, commit: &str) -> jensen_harness_promote::Cleared {
    let gates =
        jensen_harness_promote::Gates::new(jensen_harness_promote::GateConfig::default()).unwrap();
    let review = gates.review(&fixture.isolation, run, commit, &[]).unwrap();
    review
        .cleared
        .unwrap_or_else(|| panic!("blocked: {:?}", review.report.blocks))
}

fn gates() -> jensen_harness_promote::Gates {
    jensen_harness_promote::Gates::new(jensen_harness_promote::GateConfig::default()).unwrap()
}

#[test]
fn promotion_squashes_onto_the_tip_is_idempotent_and_merges_disjoint_work() {
    let fixture = fixture();
    let base = fixture.isolation.begin_run("m1").unwrap();
    let first = node_with(&fixture, "m1", "one", "one.txt", "1\n");
    let second = node_with(&fixture, "m1", "two", "two.txt", "2\n");

    let ticket = cleared(&fixture, "m1", &first);
    let moved = fixture
        .isolation
        .merge_into_integration("m1", &ticket, &gates(), &provenance("m1", "one"))
        .unwrap();
    assert_eq!(moved.how, jensen_harness_promote::Promotion::Squashed);
    assert_eq!(
        git(
            &fixture.repo,
            &["rev-parse", &format!("{}^{{tree}}", moved.head)]
        ),
        git(&fixture.repo, &["rev-parse", &format!("{first}^{{tree}}")])
    );
    assert_eq!(
        git(&fixture.repo, &["rev-parse", &format!("{}^", moved.head)]),
        base
    );
    let message = git(&fixture.repo, &["log", "-1", "--format=%B", &moved.head]);
    assert!(message.contains("Jensen-Node: one"), "{message}");
    let again = fixture
        .isolation
        .merge_into_integration("m1", &ticket, &gates(), &provenance("m1", "one"))
        .unwrap();
    assert_eq!(again.how, jensen_harness_promote::Promotion::Already);
    assert_eq!(again.head, moved.head);

    let merged = fixture
        .isolation
        .merge_into_integration(
            "m1",
            &cleared(&fixture, "m1", &second),
            &gates(),
            &provenance("m1", "two"),
        )
        .unwrap();
    assert_eq!(merged.how, jensen_harness_promote::Promotion::Merged);
    let listing = git(
        &fixture.repo,
        &["ls-tree", "-r", "--name-only", &merged.head],
    );
    assert!(listing.contains("one.txt") && listing.contains("two.txt"));
    let parents = git(&fixture.repo, &["log", "-1", "--format=%P", &merged.head]);
    assert_eq!(parents.split_whitespace().count(), 2);
    assert_eq!(git(&fixture.repo, &["rev-parse", "main"]), base);
    assert_eq!(
        fixture.isolation.integration_head("m1").unwrap(),
        merged.head
    );
}

#[test]
fn promotion_that_conflicts_is_a_typed_error_and_moves_nothing() {
    let fixture = fixture();
    fixture.isolation.begin_run("m2").unwrap();
    let first = node_with(&fixture, "m2", "one", "a.txt", "from one\n");
    let second = node_with(&fixture, "m2", "two", "a.txt", "from two\n");
    let moved = fixture
        .isolation
        .merge_into_integration(
            "m2",
            &cleared(&fixture, "m2", &first),
            &gates(),
            &provenance("m2", "one"),
        )
        .unwrap();

    let refused = fixture
        .isolation
        .merge_into_integration(
            "m2",
            &cleared(&fixture, "m2", &second),
            &gates(),
            &provenance("m2", "two"),
        )
        .unwrap_err();

    assert!(
        matches!(&refused, PromoteError::PromotionConflict { files, .. } if files.contains("a.txt")),
        "{refused}"
    );
    assert_eq!(
        fixture.isolation.integration_head("m2").unwrap(),
        moved.head
    );
}

#[test]
fn a_review_cannot_be_used_for_another_run_or_after_the_branch_moved() {
    let fixture = fixture();
    fixture.isolation.begin_run("m3").unwrap();
    fixture.isolation.begin_run("m4").unwrap();
    let first = node_with(&fixture, "m3", "one", "one.txt", "1\n");
    let second = node_with(&fixture, "m3", "two", "two.txt", "2\n");
    let ticket = cleared(&fixture, "m3", &first);
    let stale = cleared(&fixture, "m3", &second);

    let wrong = fixture
        .isolation
        .merge_into_integration("m4", &ticket, &gates(), &provenance("m4", "one"))
        .unwrap_err();
    assert!(matches!(wrong, PromoteError::WrongRun { .. }));

    fixture
        .isolation
        .merge_into_integration("m3", &ticket, &gates(), &provenance("m3", "one"))
        .unwrap();
    let late = fixture
        .isolation
        .merge_into_integration("m3", &stale, &gates(), &provenance("m3", "two"))
        .unwrap_err();
    assert!(matches!(late, PromoteError::StaleReview { .. }), "{late}");
}

#[test]
fn work_that_fails_a_gate_is_never_cleared() {
    let fixture = fixture();
    fixture.isolation.begin_run("m5").unwrap();
    let bad = node_with(&fixture, "m5", "one", ".gitlab-ci.yml", "stages: []\n");
    let review = gates().review(&fixture.isolation, "m5", &bad, &[]).unwrap();
    assert!(review.cleared.is_none());
    let unmet = gates()
        .review(
            &fixture.isolation,
            "m5",
            &fixture.isolation.integration_head("m5").unwrap(),
            &["checked".to_owned()],
        )
        .unwrap();
    assert!(unmet.cleared.is_none());
    assert!(unmet.report.blocks.iter().any(|f| f.gate == "verify"));
}
