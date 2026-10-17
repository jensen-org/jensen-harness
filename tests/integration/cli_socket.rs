use jensen_harness::socket::{current_uid, remove_stale_socket};

#[test]
fn current_uid_matches_the_owner_of_a_file_this_process_just_created() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let owner = std::fs::metadata(file.path()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(current_uid(), owner.uid());
    }
}

#[test]
fn remove_stale_socket_leaves_an_ordinary_file_alone() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path().to_path_buf();

    let error = remove_stale_socket(&path).unwrap_err();

    assert!(error.contains("not a socket"), "got: {error}");
    assert!(path.exists(), "an ordinary file must never be removed");
}

#[test]
fn remove_stale_socket_is_a_no_op_when_nothing_is_there() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("does-not-exist.sock");

    remove_stale_socket(&path).unwrap();
}

#[cfg(unix)]
#[test]
fn remove_stale_socket_removes_a_real_socket_this_user_owns() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("jensen-harness.sock");
    std::os::unix::net::UnixListener::bind(&path).unwrap();

    remove_stale_socket(&path).unwrap();

    assert!(!path.exists());
}
