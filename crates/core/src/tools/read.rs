use std::path::Path;

use super::{ToolError, resolve_within};

/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes `worktree`,
/// [`ToolError::Io`] if the file cannot be read.
pub fn run(worktree: &Path, relative: &str) -> Result<String, ToolError> {
    let path = resolve_within(worktree, relative)?;
    std::fs::read_to_string(&path).map_err(|error| ToolError::Io(error.to_string()))
}
