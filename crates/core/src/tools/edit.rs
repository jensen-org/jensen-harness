use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, File, Metadata, MetadataExt, OpenOptions};

use super::{ToolError, Worktree, io_error};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditOutcome {
    pub replacements: usize,
    pub first_line: usize,
}

type Identity = (u64, u64);

#[cfg(unix)]
fn identity(metadata: &Metadata) -> Option<Identity> {
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn identity(_metadata: &Metadata) -> Option<Identity> {
    None
}

fn refuse(relative: &str) -> ToolError {
    ToolError::OutsideWorktree {
        path: relative.to_string(),
    }
}

/// Opens a path that has already been resolved, one component at a time,
/// refusing every symlink and anything that is the worktree's own `.git`.
///
/// The `.git` test compares the device and inode of each directory and of
/// the file as opened, not the spelling of the path: a case insensitive
/// filesystem makes `.GIT` the same directory as `.git`, and a component
/// swapped for a symlink after resolution fails the nofollow open instead of
/// slipping past a string check made earlier. Where the platform has no inode
/// identity, an ASCII case insensitive match on the first component stands in.
///
/// # Errors
/// [`ToolError::OutsideWorktree`] for a `.git` target or a path that is not
/// plain components; [`ToolError::Io`] when a component is missing or is a
/// symlink.
pub fn open_nofollow(worktree: &Worktree, resolved: &Path) -> Result<File, ToolError> {
    let shown = resolved.to_string_lossy().into_owned();
    let names: Vec<&std::ffi::OsStr> = resolved
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name),
            _other => Err(refuse(&shown)),
        })
        .collect::<Result<_, _>>()?;
    if names
        .first()
        .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
    {
        return Err(refuse(&shown));
    }
    let git = worktree
        .dir
        .metadata(".git")
        .ok()
        .and_then(|metadata| identity(&metadata));
    let is_git = |metadata: &Metadata| git.is_some() && identity(metadata) == git;
    let mut current: Dir = worktree
        .dir
        .try_clone()
        .map_err(|error| io_error(&error, &shown))?;
    let Some((leaf, parents)) = names.split_last() else {
        return Err(ToolError::Io(format!("{shown} is not a file")));
    };
    for name in parents {
        current = current
            .open_dir_nofollow(name)
            .map_err(|error| io_error(&error, &shown))?;
        let metadata = current
            .dir_metadata()
            .map_err(|error| io_error(&error, &shown))?;
        if is_git(&metadata) {
            return Err(refuse(&shown));
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    let file = current
        .open_with(leaf, &options)
        .map_err(|error| io_error(&error, &shown))?;
    let metadata = file.metadata().map_err(|error| io_error(&error, &shown))?;
    if is_git(&metadata) {
        return Err(refuse(&shown));
    }
    Ok(file)
}

/// Replaces `find` with `replace` in `relative`. Refuses a `find` that
/// matches more than once unless `replace_all` is set, since nobody checks
/// which match was meant. Also refuses a target that is under the worktree's
/// own `.git` (see [`open_nofollow`]), and any file with more than one hard
/// link: a hard link is a second path to the same inode that no path check
/// can see, and a legitimate file in a git worktree is essentially never
/// linked.
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
    let mut file = open_nofollow(worktree, &resolved)?;
    let metadata = file
        .metadata()
        .map_err(|error| io_error(&error, relative))?;
    if metadata.nlink() > 1 {
        return Err(refuse(relative));
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
