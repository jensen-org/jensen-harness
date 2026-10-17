use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Component;

use cap_std::fs::{MetadataExt, OpenOptions};

use super::{ToolError, Worktree, io_error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    pub replacements: usize,
    pub first_line: usize,
}

/// Replaces `find` with `replace` in `relative`. Refuses a `find` that
/// matches more than once unless `replace_all` is set, since nobody checks
/// which match was meant. Also refuses a target that resolves under the
/// worktree's own `.git`, and any file with more than one hard link: a
/// hard link is a second path to the same inode that no path check can see,
/// and a legitimate file in a git worktree is essentially never linked.
///
/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes the worktree,
/// resolves under `.git`, or is hard linked; [`ToolError::Ambiguous`] for a
/// repeated match without `replace_all`; [`ToolError::Io`] on a read or
/// write failure, an empty `find`, or a `find` that is not in the file.
pub fn run(
    worktree: &Worktree,
    relative: &str,
    find: &str,
    replace: &str,
    replace_all: bool,
) -> Result<EditOutcome, ToolError> {
    if find.is_empty() {
        return Err(ToolError::Io("find must not be empty".to_string()));
    }
    let resolved = worktree.canonical(relative)?;
    if resolved.components().next() == Some(Component::Normal(".git".as_ref())) {
        return Err(ToolError::OutsideWorktree {
            path: relative.to_string(),
        });
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    let mut file = worktree
        .dir
        .open_with(&resolved, &options)
        .map_err(|error| io_error(&error, relative))?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error(&error, relative))?;
    if metadata.nlink() > 1 {
        return Err(ToolError::OutsideWorktree {
            path: relative.to_string(),
        });
    }
    let mut original = String::new();
    file.read_to_string(&mut original)
        .map_err(|error| io_error(&error, relative))?;
    let count = original.matches(find).count();
    if count == 0 {
        return Err(ToolError::Io(format!(
            "{find:?} was not found in {relative}"
        )));
    }
    if count > 1 && !replace_all {
        return Err(ToolError::Ambiguous {
            path: relative.to_string(),
            find: find.to_string(),
            count,
        });
    }
    let first_line = original.find(find).map_or(1, |at| {
        original
            .get(..at)
            .map_or(0, |head| head.matches('\n').count())
            + 1
    });
    let updated = if replace_all {
        original.replace(find, replace)
    } else {
        original.replacen(find, replace, 1)
    };
    file.set_len(0)
        .map_err(|error| io_error(&error, relative))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error(&error, relative))?;
    file.write_all(updated.as_bytes())
        .map_err(|error| io_error(&error, relative))?;
    Ok(EditOutcome {
        replacements: if replace_all { count } else { 1 },
        first_line,
    })
}
