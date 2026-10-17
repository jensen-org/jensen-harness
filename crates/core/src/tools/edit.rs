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
/// it.
///
/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes `worktree` or
/// resolves under `.git`, [`ToolError::Io`] on a read or write failure, and
/// [`ToolError::Io`] if `find` is not present in the file.
pub fn run(worktree: &Path, relative: &str, find: &str, replace: &str) -> Result<(), ToolError> {
    let path = resolve_within(worktree, relative)?;
    let canonical_worktree =
        std::fs::canonicalize(worktree).map_err(|error| ToolError::Io(error.to_string()))?;
    if path.starts_with(canonical_worktree.join(".git")) {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::run;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn replaces_the_first_match() {
        let worktree = tempdir().unwrap();
        fs::write(worktree.path().join("a.txt"), "hello world").unwrap();
        run(worktree.path(), "a.txt", "world", "there").unwrap();
        assert_eq!(
            fs::read_to_string(worktree.path().join("a.txt")).unwrap(),
            "hello there"
        );
    }

    #[test]
    fn refuses_to_touch_the_worktrees_own_git_pointer() {
        let worktree = tempdir().unwrap();
        fs::write(worktree.path().join(".git"), "gitdir: /real").unwrap();
        assert!(run(worktree.path(), ".git", "real", "fake").is_err());
    }

    #[test]
    fn refuses_a_git_pointer_edit_disguised_by_a_traversal_prefix() {
        let worktree = tempdir().unwrap();
        fs::write(worktree.path().join(".git"), "gitdir: /real").unwrap();
        fs::create_dir(worktree.path().join("src")).unwrap();
        // The raw string does not start with ".git", but resolves there.
        assert!(run(worktree.path(), "src/../.git", "real", "fake").is_err());
    }

    #[test]
    fn refuses_a_path_outside_the_worktree() {
        let worktree = tempdir().unwrap();
        assert!(run(worktree.path(), "../a.txt", "x", "y").is_err());
    }
}
