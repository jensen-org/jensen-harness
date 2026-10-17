//! The 5 built-in tools. `read`, `edit`, `glob` and `grep` touch the
//! filesystem through a [`Worktree`], a `cap-std` directory capability that
//! resolves every path relative to the worktree and refuses `..`, absolute
//! paths and symlinks that lead out of it, at the syscall rather than by a
//! string check. `shell` runs an arbitrary command, which no static check
//! can vet, so it alone goes through a real [`Sandbox`](crate::sandbox::Sandbox)
//! backend, enforced by the operating system.

pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod shell;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolError {
    #[error("{path} is outside the worktree, so this tool refuses it")]
    OutsideWorktree { path: String },
    #[error("the path could not be read or written: {0}")]
    Io(String),
    #[error("the sandbox refused this command: {0}")]
    Sandbox(String),
    #[error("{find:?} matches {count} times in {path}; make it unique or set replace_all")]
    Ambiguous {
        path: String,
        find: String,
        count: usize,
    },
}

/// A worktree opened as a capability. Holding one is the only way the file
/// tools reach the disk.
#[derive(Debug)]
pub struct Worktree {
    pub(crate) dir: Dir,
    pub(crate) root: PathBuf,
}

impl Worktree {
    /// # Errors
    /// [`ToolError::Io`] if `root` is missing or not a directory.
    pub fn open(root: &Path) -> Result<Self, ToolError> {
        let root = std::fs::canonicalize(root).map_err(|error| ToolError::Io(error.to_string()))?;
        let dir = Dir::open_ambient_dir(&root, ambient_authority())
            .map_err(|error| ToolError::Io(error.to_string()))?;
        Ok(Self { dir, root })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The path of `relative` once symlinks are resolved, relative to the
    /// worktree. A path that leads out of it is refused.
    pub(crate) fn canonical(&self, relative: &str) -> Result<PathBuf, ToolError> {
        self.dir
            .canonicalize(relative)
            .map_err(|error| io_error(&error, relative))
    }
}

/// `cap-std` reports an escape as `PermissionDenied`.
pub(crate) fn io_error(error: &std::io::Error, relative: &str) -> ToolError {
    if error.kind() == ErrorKind::PermissionDenied {
        ToolError::OutsideWorktree {
            path: relative.to_string(),
        }
    } else {
        ToolError::Io(format!("{relative}: {error}"))
    }
}
