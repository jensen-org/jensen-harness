use globset::GlobBuilder;
use ignore::WalkBuilder;

use super::{ToolError, Worktree};

pub(crate) fn walker(root: &std::path::Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(false)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .sort_by_file_path(Ord::cmp)
        .filter_entry(|entry| entry.file_name() != ".git");
    builder
}

/// Lists the files under the worktree matching `pattern`, relative to it and
/// sorted. `*` stays within one directory and `**` crosses them. The walk
/// honors `.gitignore`, never follows a symlink, and drops any symlink that
/// does not resolve to somewhere inside the worktree.
///
/// # Errors
/// [`ToolError::Io`] if `pattern` is not a valid glob or the walk fails.
pub fn run(worktree: &Worktree, pattern: &str) -> Result<Vec<String>, ToolError> {
    let matcher = GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| ToolError::Io(error.to_string()))?
        .compile_matcher();
    let mut matches = Vec::new();
    for entry in walker(worktree.root()).build() {
        let entry = entry.map_err(|error| ToolError::Io(error.to_string()))?;
        let Ok(relative) = entry.path().strip_prefix(worktree.root()) else {
            continue;
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() || !matcher.is_match(relative) {
            continue;
        }
        if file_type.is_symlink() && worktree.dir.canonicalize(relative).is_err() {
            continue;
        }
        matches.push(relative.to_string_lossy().into_owned());
    }
    Ok(matches)
}
