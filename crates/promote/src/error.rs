use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PromoteError {
    #[error("{path} is not a usable git repository: {reason}")]
    NotARepository { path: PathBuf, reason: String },
    #[error("git failed: {0}")]
    Git(String),
    #[error("`{0}` is not a safe name for a run or node")]
    InvalidName(String),
    #[error("the worktree of node `{node}` was tampered with: {reason}")]
    TamperedWorktree { node: String, reason: String },
    #[error("symlink `{path}` in node `{node}` points outside its worktree")]
    EscapingSymlink { node: String, path: String },
    #[error("promoting `{node}` conflicts with the integration branch in: {files}")]
    PromotionConflict { node: String, files: String },
    #[error("path {0:?} in a worktree is not valid UTF-8")]
    NonUtf8Path(PathBuf),
    #[error("{0}")]
    Io(String),
}

pub(crate) fn git(error: impl std::fmt::Display) -> PromoteError {
    PromoteError::Git(error.to_string())
}

pub(crate) fn io(path: &std::path::Path, error: impl std::fmt::Display) -> PromoteError {
    PromoteError::Io(format!("{}: {error}", path.display()))
}
