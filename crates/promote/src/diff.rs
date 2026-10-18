use gix::object::tree::EntryKind;

use crate::error::{PromoteError, git};
use crate::isolation::Isolation;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Executable,
    Symlink,
    Gitlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    pub status: ChangeStatus,
    pub kind: FileKind,
    pub id: String,
    pub previous_id: Option<String>,
}

fn kind_of(mode: gix::objs::tree::EntryMode) -> Option<FileKind> {
    match mode.kind() {
        EntryKind::Tree => None,
        EntryKind::Blob => Some(FileKind::File),
        EntryKind::BlobExecutable => Some(FileKind::Executable),
        EntryKind::Link => Some(FileKind::Symlink),
        EntryKind::Commit => Some(FileKind::Gitlink),
    }
}

fn parse_id(id: &str) -> Result<gix::ObjectId, PromoteError> {
    gix::ObjectId::from_hex(id.as_bytes()).map_err(git)
}

impl Isolation {
    pub fn changes(&self, base: &str, head: &str) -> Result<Vec<Change>, PromoteError> {
        use gix::object::tree::diff::ChangeDetached as Raw;
        let repo = self.open_repo()?;
        let tree = |commit: &str| -> Result<gix::Tree<'_>, PromoteError> {
            repo.find_commit(parse_id(commit)?)
                .map_err(git)?
                .tree()
                .map_err(git)
        };
        let (old, new) = (tree(base)?, tree(head)?);
        let raw = repo.diff_tree_to_tree(&old, &new, None).map_err(git)?;
        let mut changes = Vec::new();
        for change in raw {
            let converted = match change {
                Raw::Addition {
                    location,
                    entry_mode,
                    id,
                    ..
                } => kind_of(entry_mode).map(|kind| Change {
                    path: location.to_string(),
                    status: ChangeStatus::Added,
                    kind,
                    id: id.to_string(),
                    previous_id: None,
                }),
                Raw::Deletion {
                    location,
                    entry_mode,
                    id,
                    ..
                } => kind_of(entry_mode).map(|kind| Change {
                    path: location.to_string(),
                    status: ChangeStatus::Deleted,
                    kind,
                    id: id.to_string(),
                    previous_id: None,
                }),
                Raw::Modification {
                    location,
                    entry_mode,
                    id,
                    previous_id,
                    ..
                } => kind_of(entry_mode).map(|kind| Change {
                    path: location.to_string(),
                    status: ChangeStatus::Modified,
                    kind,
                    id: id.to_string(),
                    previous_id: Some(previous_id.to_string()),
                }),
                Raw::Rewrite {
                    source_location,
                    source_entry_mode,
                    source_id,
                    location,
                    entry_mode,
                    id,
                    copy,
                    ..
                } => {
                    if !copy {
                        changes.extend(kind_of(source_entry_mode).map(|kind| Change {
                            path: source_location.to_string(),
                            status: ChangeStatus::Deleted,
                            kind,
                            id: source_id.to_string(),
                            previous_id: None,
                        }));
                    }
                    kind_of(entry_mode).map(|kind| Change {
                        path: location.to_string(),
                        status: ChangeStatus::Added,
                        kind,
                        id: id.to_string(),
                        previous_id: None,
                    })
                }
            };
            changes.extend(converted);
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    pub fn blob(&self, id: &str) -> Result<Vec<u8>, PromoteError> {
        let repo = self.open_repo()?;
        let found = repo.find_blob(parse_id(id)?).map_err(git)?;
        Ok(found.data.clone())
    }
}
