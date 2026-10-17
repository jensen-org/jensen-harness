use std::path::Path;

use git2::{Repository, WorktreeAddOptions, WorktreePruneOptions};

use crate::error::{PromoteError, io};
use crate::isolation::NodeWorktree;

fn libgit(error: git2::Error) -> PromoteError {
    PromoteError::Git(error.message().to_owned())
}

pub(crate) fn create(
    repo: &Path,
    workspaces: &Path,
    handle: &NodeWorktree,
) -> Result<(), PromoteError> {
    let parent = handle.path.parent().unwrap_or(workspaces);
    std::fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    restrict(workspaces)?;
    let repository = Repository::open(repo).map_err(libgit)?;
    let reference = repository.find_reference(&handle.branch).map_err(libgit)?;
    let mut options = WorktreeAddOptions::new();
    options.reference(Some(&reference));
    repository
        .worktree(&handle.name, &handle.path, Some(&options))
        .map_err(libgit)?;
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
    let expected = repo.join(".git").join("worktrees").join(&handle.name);
    let same = Path::new(recorded)
        .canonicalize()
        .ok()
        .zip(expected.canonicalize().ok())
        .is_some_and(|(found, wanted)| found == wanted);
    if !same {
        return Err(tampered(format!(
            "the .git pointer names {recorded}, not {}",
            expected.display()
        )));
    }
    Ok(())
}

pub(crate) fn remove(repo: &Path, handle: &NodeWorktree) -> Result<(), PromoteError> {
    let repository = Repository::open(repo).map_err(libgit)?;
    let found = repository.find_worktree(&handle.name).map_err(libgit)?;
    let mut options = WorktreePruneOptions::new();
    options.valid(true).working_tree(true).locked(true);
    found.prune(Some(&mut options)).map_err(libgit)?;
    Ok(())
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
