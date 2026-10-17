use std::path::Path;

use super::{ToolError, resolve_within};

/// Replaces the first occurrence of `find` with `replace` in `relative` and
/// writes the result back. Also refuses an edit whose target, after fully
/// resolving (symlinks included), is under the worktree's own `.git`, so
/// this tool never becomes a second way around the sandbox's own denial of
/// that path.
///
/// The check is on the resolved path, not the raw `relative` string: a
/// string like `src/../.git/hooks/x` does not start with `.git` lexically,
/// but still resolves there, so checking the input string alone would miss
/// it. On unix, any file with more than one hard link is refused outright,
/// not only one specifically aliasing `.git`: a legitimate file inside a
/// git worktree is essentially never hard linked, so a link count above
/// one is itself the signal, regardless of what the other path happens to
/// be.
///
/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes `worktree`,
/// resolves under `.git`, or is hard linked from elsewhere; [`ToolError::Io`]
/// on a read or write failure, and [`ToolError::Io`] if `find` is not
/// present in the file.
pub fn run(worktree: &Path, relative: &str, find: &str, replace: &str) -> Result<(), ToolError> {
    let path = resolve_within(worktree, relative)?;
    let canonical_worktree =
        std::fs::canonicalize(worktree).map_err(|error| ToolError::Io(error.to_string()))?;
    let git_pointer = canonical_worktree.join(".git");
    if path.starts_with(&git_pointer) {
        return Err(ToolError::OutsideWorktree {
            path: relative.to_string(),
        });
    }
    if is_hard_linked(&path)? {
        return Err(ToolError::OutsideWorktree {
            path: relative.to_string(),
        });
    }
    let original =
        std::fs::read_to_string(&path).map_err(|error| ToolError::Io(error.to_string()))?;
    let Some(at) = original.find(find) else {
        return Err(ToolError::Io(format!(
            "{find:?} was not found in {relative}"
        )));
    };
    let mut updated = String::with_capacity(original.len());
    updated.push_str(&original[..at]);
    updated.push_str(replace);
    updated.push_str(&original[at + find.len()..]);
    std::fs::write(&path, updated).map_err(|error| ToolError::Io(error.to_string()))
}

/// Whether `path` has more than one hard link, which a path-prefix check
/// alone cannot see: a hard link gives a second path to the identical
/// inode, never nested under the first path at all, so it could alias
/// `.git` or anything else outside the worktree without the resolved path
/// ever showing it.
///
/// # Errors
/// [`ToolError::Io`] if `path` cannot be inspected.
#[cfg(unix)]
fn is_hard_linked(path: &Path) -> Result<bool, ToolError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).map_err(|error| ToolError::Io(error.to_string()))?;
    Ok(metadata.nlink() > 1)
}

#[cfg(not(unix))]
fn is_hard_linked(path: &Path) -> Result<bool, ToolError> {
    // No link-count check on this platform yet; the path-prefix check
    // above is what this phase ships here. Still a real read, so a target
    // that cannot be inspected is still reported, not papered over.
    std::fs::metadata(path).map_err(|error| ToolError::Io(error.to_string()))?;
    Ok(false)
}
