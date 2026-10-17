use std::path::Path;

use super::{ToolError, resolve_within};

/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes `worktree`,
/// [`ToolError::Io`] if the file cannot be read.
pub fn run(worktree: &Path, relative: &str) -> Result<String, ToolError> {
    let path = resolve_within(worktree, relative)?;
    std::fs::read_to_string(&path).map_err(|error| ToolError::Io(error.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::run;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn reads_a_file_inside_the_worktree() {
        let worktree = tempdir().unwrap();
        fs::write(worktree.path().join("a.txt"), "hello").unwrap();
        assert_eq!(run(worktree.path(), "a.txt").unwrap(), "hello");
    }

    #[test]
    fn refuses_a_path_outside_the_worktree() {
        let worktree = tempdir().unwrap();
        assert!(run(worktree.path(), "../a.txt").is_err());
    }
}
