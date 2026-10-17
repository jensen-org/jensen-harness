use std::path::{Path, PathBuf};
use std::process::Command;

use jensen_harness_promote::{Isolation, PromoteError, Provenance};

fn git(dir: &Path, args: &[&str]) -> String {
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

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    workspaces: PathBuf,
    isolation: Isolation,
}

fn fixture() -> Fixture {
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

fn provenance(run: &str, node: &str) -> Provenance {
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
    let private = fixture.repo.join(".git/worktrees/r5.write");
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

#[test]
fn libgit2_is_used_by_one_module_only() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates");
    let mut users = Vec::new();
    let mut stack = vec![crates];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && std::fs::read_to_string(&path).unwrap().contains("git2::")
            {
                users.push(
                    path.strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .unwrap()
                        .to_owned(),
                );
            }
        }
    }
    assert_eq!(users, [PathBuf::from("crates/promote/src/worktree.rs")]);
}
