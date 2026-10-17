use std::path::Path;

use super::{ToolError, resolve_within};

/// Replaces the first occurrence of `find` with `replace` in `relative` and
/// writes the result back. Also refuses an edit whose target, after
/// resolving, is the worktree's own `.git` pointer, so this tool never
/// becomes a second way around the sandbox's own denial of that path.
///
/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes `worktree` or names
/// `.git`, [`ToolError::Io`] on a read or write failure, and
/// [`ToolError::Io`] if `find` is not present in the file.
pub fn run(worktree: &Path, relative: &str, find: &str, replace: &str) -> Result<(), ToolError> {
    if Path::new(relative)
        .components()
        .next()
        .is_some_and(|first| first.as_os_str() == ".git")
    {
        return Err(ToolError::OutsideWorktree {
            path: relative.to_string(),
        });
    }
    let path = resolve_within(worktree, relative)?;
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
    fn refuses_a_path_outside_the_worktree() {
        let worktree = tempdir().unwrap();
        assert!(run(worktree.path(), "../a.txt", "x", "y").is_err());
    }
}
