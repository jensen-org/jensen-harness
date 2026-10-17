use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};

use super::glob::walker;
use super::{ToolError, Worktree, io_error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub path: String,
    pub line_number: usize,
    pub line: String,
}

/// Searches `relative` (a file, or a directory searched recursively) for
/// lines matching `pattern`. The walk honors `.gitignore` and never follows
/// a symlink, and every file it finds is reopened through the worktree
/// capability before it is read, so a path that slipped past the walk still
/// cannot leave the worktree. Binary files are skipped.
///
/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes the worktree,
/// [`ToolError::Io`] if `pattern` is not a valid regex or a file cannot be
/// read.
pub fn run(worktree: &Worktree, relative: &str, pattern: &str) -> Result<Vec<Hit>, ToolError> {
    let matcher = RegexMatcher::new(pattern).map_err(|error| ToolError::Io(error.to_string()))?;
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let start = worktree.root().join(worktree.canonical(relative)?);
    let mut hits = Vec::new();
    for entry in walker(&start).build() {
        let entry = entry.map_err(|error| ToolError::Io(error.to_string()))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(file_relative) = entry.path().strip_prefix(worktree.root()) else {
            continue;
        };
        let shown = file_relative.to_string_lossy().into_owned();
        let file = worktree
            .dir
            .open(file_relative)
            .map_err(|error| io_error(&error, &shown))?;
        searcher
            .search_reader(
                &matcher,
                file,
                UTF8(|number, line| {
                    let line_number = usize::try_from(number).map_err(std::io::Error::other)?;
                    hits.push(Hit {
                        path: shown.clone(),
                        line_number,
                        line: line.trim_end_matches(['\n', '\r']).to_string(),
                    });
                    Ok(true)
                }),
            )
            .map_err(|error| ToolError::Io(format!("{shown}: {error}")))?;
    }
    Ok(hits)
}
