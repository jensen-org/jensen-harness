use super::{ToolError, Worktree, io_error};

/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes the worktree,
/// [`ToolError::Io`] if the file cannot be read.
pub fn run(worktree: &Worktree, relative: &str) -> Result<String, ToolError> {
    worktree
        .dir
        .read_to_string(relative)
        .map_err(|error| io_error(&error, relative))
}
