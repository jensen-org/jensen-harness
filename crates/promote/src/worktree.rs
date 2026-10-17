use std::path::Path;
use std::sync::atomic::AtomicBool;

use gix::progress::Discard;
use gix::worktree::add::Head;
use gix::worktree::remove::Force;

use crate::error::{PromoteError, git, io};
use crate::isolation::NodeWorktree;

pub(crate) fn create(
    repo: &gix::Repository,
    workspaces: &Path,
    handle: &NodeWorktree,
) -> Result<(), PromoteError> {
    let parent = handle.path.parent().unwrap_or(workspaces);
    std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    restrict(workspaces)?;
    let branch = gix::refs::FullName::try_from(handle.branch.as_str()).map_err(git)?;
    repo.add_worktree(
        &handle.path,
        Head::Attached(branch),
        Discard,
        &AtomicBool::new(false),
    )
    .map_err(git)?;
    Ok(())
}

pub(crate) fn verify(repo: &Path, handle: &NodeWorktree) -> Result<(), PromoteError> {
    let tampered = |reason: String| PromoteError::TamperedWorktree {
        node: handle.node.clone(),
        reason,
    };
    let pointer = handle.path.join(".git");
    let metadata = std::fs::symlink_metadata(&pointer)
        .map_err(|error| tampered(format!("the .git pointer is missing: {error}")))?;
    if !metadata.is_file() {
        return Err(tampered("the .git pointer is not a plain file".to_owned()));
    }
    let text = std::fs::read_to_string(&pointer)
        .map_err(|error| tampered(format!("the .git pointer is unreadable: {error}")))?;
    let recorded = text
        .strip_prefix("gitdir:")
        .map(str::trim)
        .ok_or_else(|| tampered("the .git pointer is not a gitdir line".to_owned()))?;
    let admin = Path::new(recorded).canonicalize().map_err(|error| {
        tampered(format!(
            "the .git pointer names a missing directory: {error}"
        ))
    })?;
    let registry = repo
        .join(".git")
        .join("worktrees")
        .canonicalize()
        .map_err(|error| tampered(format!("the repository has no worktree registry: {error}")))?;
    if admin.parent() != Some(registry.as_path()) {
        return Err(tampered(format!(
            "the .git pointer names {recorded}, outside the repository's worktrees"
        )));
    }
    let back = std::fs::read_to_string(admin.join("gitdir"))
        .map_err(|error| tampered(format!("the worktree is not registered: {error}")))?;
    let registered = Path::new(back.trim()).canonicalize().ok();
    if registered != pointer.canonicalize().ok() {
        return Err(tampered(
            "the registration points at a different worktree".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn remove(repo: &gix::Repository, handle: &NodeWorktree) -> Result<(), PromoteError> {
    repo.remove_worktree(&handle.path, Force::DiscardChanges, Discard)
        .map_err(git)
}

#[cfg(unix)]
fn restrict(path: &Path) -> Result<(), PromoteError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| io(path, error))
}

#[cfg(not(unix))]
fn restrict(_path: &Path) -> Result<(), PromoteError> {
    Ok(())
}
