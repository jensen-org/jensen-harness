//! Local-socket hygiene: a fresh Unix socket is world-accessible by
//! default on most systems, and this one carries live model credentials
//! and runs the agent for whoever connects to it, so it is never trusted
//! on filesystem permissions alone.

use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

/// # Safety
/// `getuid` has no preconditions and cannot fail; the `unsafe` here is only
/// the FFI boundary, not a real hazard.
#[must_use]
pub fn current_uid() -> u32 {
    // SAFETY: see above.
    unsafe { libc::getuid() }
}

/// Removes a stale socket file left at `path` by a crashed previous run,
/// but only when `path` is actually a socket this same user owns: a path
/// that is something else, or owned by someone else, is left alone and
/// reported as an error rather than deleted on the assumption it must be
/// safe to clear.
///
/// # Errors
/// Returns an error when `path` exists but is not a socket owned by this
/// user, or when removing it fails.
pub fn remove_stale_socket(path: &Path) -> Result<(), String> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_socket() && metadata.uid() == current_uid() {
        return std::fs::remove_file(path).map_err(|error| error.to_string());
    }
    Err(format!(
        "refusing to remove {}: it is not a socket this user owns",
        path.display()
    ))
}
