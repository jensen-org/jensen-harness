use jensen_harness_core::sandbox::{
    NetworkPolicy, Sandbox, SandboxError, SandboxPolicy, bubblewrap::Bubblewrap,
};

#[test]
fn confine_binds_the_git_pointer_read_only_after_the_worktree_bind() {
    let policy = SandboxPolicy::confined_to("/repo/task");
    let confined = Bubblewrap
        .confine(&policy, "/bin/sh", &["-c".to_string(), "true".to_string()])
        .unwrap();
    let worktree_bind = confined
        .args
        .iter()
        .position(|arg| arg == "/repo/task")
        .unwrap();
    let git_ro_bind = confined
        .args
        .iter()
        .position(|arg| arg == "/repo/task/.git")
        .unwrap();
    assert!(
        worktree_bind < git_ro_bind,
        "the read-only .git bind must come after the read-write worktree bind so it wins"
    );
    assert_eq!(
        confined.args.get(git_ro_bind - 1),
        Some(&"--ro-bind".to_string())
    );
}

#[test]
fn confine_unshares_the_network_by_default() {
    let policy = SandboxPolicy::confined_to("/repo/task");
    let confined = Bubblewrap.confine(&policy, "/bin/true", &[]).unwrap();
    assert!(confined.args.iter().any(|arg| arg == "--unshare-net"));
    assert_eq!(confined.report.network_tier, "none");
}

#[test]
fn confine_masks_the_usual_socket_directories_and_isolates_pid_and_ipc() {
    let policy = SandboxPolicy::confined_to("/repo/task");
    let confined = Bubblewrap.confine(&policy, "/bin/true", &[]).unwrap();
    for masked in ["/tmp", "/run", "/var/run"] {
        assert!(
            confined.args.iter().any(|arg| arg == masked),
            "{masked} must be masked with a private tmpfs"
        );
    }
    for flag in ["--unshare-pid", "--unshare-ipc", "--new-session"] {
        assert!(
            confined.args.iter().any(|arg| arg == flag),
            "{flag} must be set"
        );
    }
}

#[test]
fn confine_refuses_a_proxy_allowlist_instead_of_granting_full_network() {
    let mut policy = SandboxPolicy::confined_to("/repo/task");
    policy.network = NetworkPolicy::ProxyAllowlist { port: 8080 };

    let error = Bubblewrap.confine(&policy, "/bin/true", &[]).unwrap_err();

    assert!(matches!(error, SandboxError::Policy(_)));
}

#[test]
fn confine_reports_that_full_network_does_not_isolate_abstract_sockets() {
    let mut policy = SandboxPolicy::confined_to("/repo/task");
    policy.network = NetworkPolicy::Full;

    let confined = Bubblewrap.confine(&policy, "/bin/true", &[]).unwrap();

    assert_eq!(confined.report.network_tier, "full");
    assert!(
        !confined.report.unsupported.is_empty(),
        "the Full tier must disclose that it does not isolate the network namespace"
    );
    assert!(!confined.args.iter().any(|arg| arg == "--unshare-net"));
}
