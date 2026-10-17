use std::path::Path;

use regex::Regex;

use super::{ToolError, resolve_within};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub line_number: usize,
    pub line: String,
}

/// Searches `relative` for lines matching `pattern`.
///
/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes `worktree`,
/// [`ToolError::Io`] if `pattern` is not a valid regex or the file cannot be
/// read.
pub fn run(worktree: &Path, relative: &str, pattern: &str) -> Result<Vec<Hit>, ToolError> {
    let path = resolve_within(worktree, relative)?;
    let regex = Regex::new(pattern).map_err(|error| ToolError::Io(error.to_string()))?;
    let text = std::fs::read_to_string(&path).map_err(|error| ToolError::Io(error.to_string()))?;
    Ok(text
        .lines()
        .enumerate()
        .filter(|(_, line)| regex.is_match(line))
        .map(|(index, line)| Hit {
            line_number: index + 1,
            line: line.to_string(),
        })
        .collect())
}
