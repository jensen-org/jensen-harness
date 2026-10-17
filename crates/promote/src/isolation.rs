use std::path::{Component, Path, PathBuf};

use gix::bstr::BStr;
use gix::object::tree::EntryKind;
use gix::refs::transaction::PreviousValue;

use crate::error::{PromoteError, git, io};
use crate::worktree;

const COMMITTER_NAME: &str = "Jensen Harness";
const COMMITTER_EMAIL: &str = "harness@jensen.invalid";

#[derive(Debug, Clone)]
pub struct NodeWorktree {
    pub run: String,
    pub node: String,
    pub path: PathBuf,
    pub branch: String,
    pub(crate) name: String,
}

#[derive(Debug, Clone)]
pub struct Provenance {
    pub run: String,
    pub node: String,
    pub session: String,
    pub model: String,
    pub spec: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub commit: String,
    pub tree: String,
    pub parent: String,
    pub created: bool,
}

#[derive(Debug, Clone)]
pub struct Isolation {
    repo: PathBuf,
    workspaces: PathBuf,
}

fn checked(name: &str) -> Result<&str, PromoteError> {
    let safe = !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if safe {
        Ok(name)
    } else {
        Err(PromoteError::InvalidName(name.to_owned()))
    }
}

fn open_isolated(path: &Path) -> Result<gix::Repository, PromoteError> {
    let options = gix::open::Options::isolated().config_overrides([
        format!("user.name={COMMITTER_NAME}"),
        format!("user.email={COMMITTER_EMAIL}"),
    ]);
    gix::open_opts(path, options).map_err(git)
}

fn tip_of(repo: &gix::Repository, reference: &str) -> Result<Option<gix::ObjectId>, PromoteError> {
    let found = repo.try_find_reference(reference).map_err(git)?;
    found
        .map(|mut found| found.peel_to_id().map(gix::Id::detach).map_err(git))
        .transpose()
}

impl Isolation {
    /// # Errors
    /// [`PromoteError::NotARepository`] when `repo` is not a git repository.
    pub fn open(repo: &Path, workspaces: &Path) -> Result<Self, PromoteError> {
        let repo = repo.canonicalize().map_err(|error| io(repo, error))?;
        open_isolated(&repo).map_err(|error| PromoteError::NotARepository {
            path: repo.clone(),
            reason: error.to_string(),
        })?;
        Ok(Self {
            repo,
            workspaces: workspaces.to_path_buf(),
        })
    }

    fn open_repo(&self) -> Result<gix::Repository, PromoteError> {
        open_isolated(&self.repo)
    }

    #[must_use]
    pub fn integration_ref(run: &str) -> String {
        format!("refs/heads/jensen/{run}/integration")
    }

    #[must_use]
    pub fn node_ref(run: &str, node: &str) -> String {
        format!("refs/heads/jensen/{run}/node/{node}")
    }

    /// Creates the run's integration branch at the repository's HEAD, or
    /// returns its tip when it already exists.
    ///
    /// # Errors
    /// [`PromoteError`] when the name is unsafe or git refuses.
    pub fn begin_run(&self, run: &str) -> Result<String, PromoteError> {
        checked(run)?;
        let repo = self.open_repo()?;
        let name = Self::integration_ref(run);
        if let Some(tip) = tip_of(&repo, &name)? {
            return Ok(tip.to_string());
        }
        let head = repo.head_id().map_err(git)?.detach();
        repo.reference(
            name.as_str(),
            head,
            PreviousValue::MustNotExist,
            "jensen: begin run",
        )
        .map_err(git)?;
        Ok(head.to_string())
    }

    /// # Errors
    /// [`PromoteError`] when the run has no integration branch.
    pub fn integration_head(&self, run: &str) -> Result<String, PromoteError> {
        checked(run)?;
        let repo = self.open_repo()?;
        tip_of(&repo, &Self::integration_ref(run))?
            .map(|tip| tip.to_string())
            .ok_or_else(|| PromoteError::Git(format!("run {run} has no integration branch")))
    }

    /// Creates the node's branch at the integration head and a linked
    /// worktree on it, or returns the existing one after checking it.
    ///
    /// # Errors
    /// [`PromoteError`] when git refuses or an existing worktree was
    /// tampered with.
    pub fn prepare_node(&self, run: &str, node: &str) -> Result<NodeWorktree, PromoteError> {
        checked(run)?;
        checked(node)?;
        let repo = self.open_repo()?;
        let base = tip_of(&repo, &Self::integration_ref(run))?
            .ok_or_else(|| PromoteError::Git(format!("run {run} has no integration branch")))?;
        let branch = Self::node_ref(run, node);
        if tip_of(&repo, &branch)?.is_none() {
            repo.reference(
                branch.as_str(),
                base,
                PreviousValue::MustNotExist,
                "jensen: begin node",
            )
            .map_err(git)?;
        }
        let path = self.workspaces.join(run).join(node);
        let handle = NodeWorktree {
            run: run.to_owned(),
            node: node.to_owned(),
            path,
            branch,
            name: format!("{run}.{node}"),
        };
        if handle.path.exists() {
            worktree::verify(&self.repo, &handle)?;
            return Ok(handle);
        }
        worktree::create(&self.repo, &self.workspaces, &handle)?;
        worktree::verify(&self.repo, &handle)?;
        Ok(handle)
    }

    /// Writes the worktree's files into the real repository as one commit on
    /// the node branch. Nothing is read from the worktree's git metadata.
    ///
    /// # Errors
    /// [`PromoteError`] on a tampered worktree, a symlink that leaves it, or
    /// a git failure.
    pub fn snapshot(
        &self,
        handle: &NodeWorktree,
        provenance: &Provenance,
    ) -> Result<Snapshot, PromoteError> {
        worktree::verify(&self.repo, handle)?;
        let repo = self.open_repo()?;
        let tip = tip_of(&repo, &handle.branch)?
            .ok_or_else(|| PromoteError::Git(format!("branch {} does not exist", handle.branch)))?;
        let tree = write_tree(&repo, handle)?;
        let tip_tree = repo
            .find_commit(tip)
            .map_err(git)?
            .tree_id()
            .map_err(git)?
            .detach();
        if tip_tree == tree {
            return Ok(Snapshot {
                commit: tip.to_string(),
                tree: tree.to_string(),
                parent: tip.to_string(),
                created: false,
            });
        }
        let seconds = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| PromoteError::Io(error.to_string()))?
            .as_secs();
        let time = format!("{seconds} +0000");
        let signature = gix::actor::SignatureRef {
            name: BStr::new(COMMITTER_NAME),
            email: BStr::new(COMMITTER_EMAIL),
            time: &time,
        };
        let message = format!(
            "jensen: work of {node} in run {run}\n\nJensen-Run: {run}\nJensen-Node: {node}\nJensen-Session: {session}\nJensen-Model: {model}\nJensen-Spec: {spec}\n",
            node = provenance.node,
            run = provenance.run,
            session = provenance.session,
            model = provenance.model,
            spec = provenance.spec,
        );
        let commit = repo
            .commit_as(
                signature,
                signature,
                handle.branch.as_str(),
                message,
                tree,
                [tip],
            )
            .map_err(git)?
            .detach();
        Ok(Snapshot {
            commit: commit.to_string(),
            tree: tree.to_string(),
            parent: tip.to_string(),
            created: true,
        })
    }

    /// # Errors
    /// [`PromoteError`] when git refuses to remove the worktree.
    pub fn remove_node(&self, handle: &NodeWorktree) -> Result<(), PromoteError> {
        worktree::remove(&self.repo, handle)
    }
}

fn confined_target(target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth: i64 = 0;
    for component in target.components() {
        match component {
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            _ => {}
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

fn write_tree(
    repo: &gix::Repository,
    handle: &NodeWorktree,
) -> Result<gix::ObjectId, PromoteError> {
    let root = &handle.path;
    let empty = repo.empty_tree();
    let mut editor = empty.edit().map_err(git)?;
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .parents(false)
        .git_global(false)
        .git_exclude(false)
        .git_ignore(true)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_path(Ord::cmp)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walker {
        let entry = entry.map_err(|error| PromoteError::Io(error.to_string()))?;
        let Some(kind) = entry.file_type() else {
            continue;
        };
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| PromoteError::Io(error.to_string()))?;
        if relative.as_os_str().is_empty() || kind.is_dir() {
            continue;
        }
        let rela = relative
            .to_str()
            .ok_or_else(|| PromoteError::NonUtf8Path(relative.to_path_buf()))?;
        if kind.is_symlink() {
            let target =
                std::fs::read_link(entry.path()).map_err(|error| io(entry.path(), error))?;
            let resolved = relative.parent().unwrap_or(Path::new("")).join(&target);
            if !confined_target(&resolved) || target.is_absolute() {
                return Err(PromoteError::EscapingSymlink {
                    node: handle.node.clone(),
                    path: rela.to_owned(),
                });
            }
            let bytes = target.to_string_lossy().into_owned().into_bytes();
            let id = repo.write_blob(bytes).map_err(git)?.detach();
            editor.upsert(rela, EntryKind::Link, id).map_err(git)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        let bytes = std::fs::read(entry.path()).map_err(|error| io(entry.path(), error))?;
        let executable = is_executable(entry.path())?;
        let id = repo.write_blob(bytes).map_err(git)?.detach();
        let entry_kind = if executable {
            EntryKind::BlobExecutable
        } else {
            EntryKind::Blob
        };
        editor.upsert(rela, entry_kind, id).map_err(git)?;
    }
    Ok(editor.write().map_err(git)?.detach())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> Result<bool, PromoteError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|error| io(path, error))?
        .permissions()
        .mode();
    Ok(mode & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> Result<bool, PromoteError> {
    Ok(false)
}
