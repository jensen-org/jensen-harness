use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::CompactError;

/// Raw tool output kept on disk under a handle, so a result that was cut
/// for the model can still be read in full, a slice at a time.
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
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| CompactError::Store(error.to_string()))?;
        let handle = format!("r{}", self.next.fetch_add(1, Ordering::Relaxed));
        std::fs::write(self.path(&handle), raw)
            .map_err(|error| CompactError::Store(error.to_string()))?;
        Ok(handle)
    }

    /// Lines `offset..offset + limit` of a stored output, exactly as stored.
    ///
    /// # Errors
    /// [`CompactError::UnknownHandle`] for a handle this store did not issue.
    pub fn slice(&self, handle: &str, offset: usize, limit: usize) -> Result<String, CompactError> {
        let valid = handle
            .strip_prefix('r')
            .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()));
        if !valid {
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
