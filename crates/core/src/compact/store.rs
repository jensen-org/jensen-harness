use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::CompactError;

/// Raw tool output kept on disk under a handle, so a result that was cut
/// for the model can still be read in full, a slice at a time.
///
/// The directory must be one the sandboxed shell cannot write: a command
/// that could plant a symlink at the next handle would turn the harness's
/// own unsandboxed write, or a later read, into a write or read anywhere.
/// Handles are also checked against the ones actually issued, and a file is
/// only ever created fresh.
#[derive(Debug)]
pub struct RawStore {
    dir: PathBuf,
    next: AtomicU64,
}

impl RawStore {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            next: AtomicU64::new(1),
        }
    }

    /// # Errors
    /// [`CompactError::Store`] when the directory or file cannot be written.
    pub fn tee(&self, raw: &str) -> Result<String, CompactError> {
        let store_error = |error: std::io::Error| CompactError::Store(error.to_string());
        std::fs::create_dir_all(&self.dir).map_err(store_error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
                .map_err(store_error)?;
        }
        let handle = format!("r{}", self.next.fetch_add(1, Ordering::Relaxed));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.path(&handle))
            .map_err(store_error)?;
        file.write_all(raw.as_bytes()).map_err(store_error)?;
        Ok(handle)
    }

    /// Lines `offset..offset + limit` of a stored output, exactly as stored.
    ///
    /// # Errors
    /// [`CompactError::UnknownHandle`] for a handle this store did not issue.
    pub fn slice(&self, handle: &str, offset: usize, limit: usize) -> Result<String, CompactError> {
        let issued = handle
            .strip_prefix('r')
            .filter(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
            .and_then(|digits| digits.parse::<u64>().ok())
            .is_some_and(|number| number >= 1 && number < self.next.load(Ordering::Relaxed));
        if !issued {
            return Err(CompactError::UnknownHandle(handle.to_string()));
        }
        let text = std::fs::read_to_string(self.path(handle))
            .map_err(|_missing| CompactError::UnknownHandle(handle.to_string()))?;
        let total = text.lines().count();
        let body: Vec<String> = text
            .lines()
            .skip(offset)
            .take(limit)
            .map(str::to_string)
            .collect();
        let shown_to = offset + body.len();
        let mut out = body.join("\n");
        if shown_to < total {
            out.push_str(&format!(
                "\n[lines {}-{shown_to} of {total}; call expand again with offset {shown_to}]",
                offset + 1
            ));
        }
        Ok(out)
    }

    fn path(&self, handle: &str) -> PathBuf {
        self.dir.join(format!("{handle}.txt"))
    }
}
