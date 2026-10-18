use gix::bstr::BStr;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::diff::{Change, ChangeStatus, FileKind};
use crate::error::{PromoteError, git};
use crate::flags::{code_flags, path_flags};
use crate::isolation::Isolation;
use crate::secrets::SecretScreen;

const DEFAULT_PROTECTED: [&str; 7] = [
    ".git",
    ".git/**",
    ".github/workflows/**",
    ".gitlab-ci.yml",
    ".gitmodules",
    ".jensen",
    ".jensen/**",
];

const BINARY_SNIFF_BYTES: usize = 8192;

const RESERVED_COMPONENTS: [&str; 3] = [".git", ".gitmodules", ".jensen"];
const RESERVED_SHORT_PREFIXES: [&str; 2] = ["git~", "gitmod~"];

fn normalized(component: &str) -> String {
    component
        .chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{206a}'..='\u{206f}' | '\u{feff}'
            )
        })
        .collect::<String>()
        .trim_end_matches(['.', ' '])
        .to_lowercase()
}

fn reserved_component(path: &str) -> bool {
    let options = gix::validate::path::component::Options {
        protect_windows: true,
        protect_hfs: true,
        protect_ntfs: true,
    };
    let names: Vec<String> = path.split('/').map(normalized).collect();
    let unsafe_name = path.split('/').any(|component| {
        component.contains(':')
            || component.ends_with(['.', ' '])
            || gix::validate::path::component(BStr::new(component), None, options).is_err()
    });
    unsafe_name
        || names.iter().any(|name| {
            let dotted = format!(".{}", name.trim_start_matches('.'));
            RESERVED_COMPONENTS.contains(&name.as_str())
                || (name.starts_with('.') && RESERVED_COMPONENTS.contains(&dotted.as_str()))
                || RESERVED_SHORT_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
        || names.windows(2).any(
            |pair| matches!(pair, [first, second] if first == ".github" && second == "workflows"),
        )
}

#[derive(Debug, Clone)]
pub struct GateConfig {
    pub protected: Vec<String>,
    pub max_changed_files: usize,
    pub max_added_bytes: u64,
    pub allow_binary: bool,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            protected: Vec::new(),
            max_changed_files: 200,
            max_added_bytes: 5 * 1024 * 1024,
            allow_binary: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub gate: &'static str,
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub files: usize,
    pub added_bytes: u64,
    pub blocks: Vec<Finding>,
    pub flags: Vec<Finding>,
}

impl Report {
    #[must_use]
    pub fn passed(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[derive(Debug, Clone)]
pub struct Cleared {
    pub(crate) run: String,
    pub(crate) commit: String,
    pub(crate) base: String,
}

impl Cleared {
    #[must_use]
    pub fn commit(&self) -> &str {
        &self.commit
    }
}

#[derive(Debug)]
pub struct Review {
    pub report: Report,
    pub cleared: Option<Cleared>,
}

pub struct Gates {
    config: GateConfig,
    protected: GlobSet,
    secrets: SecretScreen,
}

fn finding(gate: &'static str, path: &str, reason: impl Into<String>) -> Finding {
    Finding {
        gate,
        path: path.to_owned(),
        reason: reason.into(),
    }
}

impl Gates {
    pub fn new(config: GateConfig) -> Result<Self, PromoteError> {
        let mut builder = GlobSetBuilder::new();
        for (pattern, literal) in DEFAULT_PROTECTED
            .iter()
            .map(|pattern| ((*pattern).to_owned(), true))
            .chain(
                config
                    .protected
                    .iter()
                    .map(|pattern| (pattern.clone(), false)),
            )
        {
            builder.add(
                GlobBuilder::new(&pattern)
                    .case_insensitive(true)
                    .literal_separator(literal)
                    .build()
                    .map_err(git)?,
            );
        }
        Ok(Self {
            protected: builder.build().map_err(git)?,
            secrets: SecretScreen::new()?,
            config,
        })
    }

    pub fn evaluate(
        &self,
        isolation: &Isolation,
        base: &str,
        head: &str,
    ) -> Result<Report, PromoteError> {
        let changes = isolation.changes(base, head)?;
        let mut report = Report {
            files: changes.len(),
            ..Report::default()
        };
        if changes.len() > self.config.max_changed_files {
            report.blocks.push(finding(
                "size",
                "",
                format!(
                    "{} files changed, the limit is {}",
                    changes.len(),
                    self.config.max_changed_files
                ),
            ));
        }
        for change in &changes {
            self.check_change(isolation, change, &mut report)?;
        }
        if report.added_bytes > self.config.max_added_bytes {
            report.blocks.push(finding(
                "size",
                "",
                format!(
                    "{} bytes added, the limit is {}",
                    report.added_bytes, self.config.max_added_bytes
                ),
            ));
        }
        Ok(report)
    }

    pub fn review(
        &self,
        isolation: &Isolation,
        run: &str,
        commit: &str,
        unmet: &[String],
    ) -> Result<Review, PromoteError> {
        let base = isolation.integration_head(run)?;
        let mut report = self.evaluate(isolation, &base, commit)?;
        for requirement in unmet {
            report.blocks.push(finding(
                "verify",
                "",
                format!("verified channel `{requirement}` is not true"),
            ));
        }
        let cleared = report.passed().then(|| Cleared {
            run: run.to_owned(),
            commit: commit.to_owned(),
            base,
        });
        Ok(Review { report, cleared })
    }

    fn check_change(
        &self,
        isolation: &Isolation,
        change: &Change,
        report: &mut Report,
    ) -> Result<(), PromoteError> {
        if self.protected.is_match(&change.path) || reserved_component(&change.path) {
            report
                .blocks
                .push(finding("path", &change.path, "the path is protected"));
        }
        if change.status == ChangeStatus::Deleted {
            return Ok(());
        }
        match change.kind {
            FileKind::Symlink => {
                report.blocks.push(finding(
                    "path",
                    &change.path,
                    "a symlink was added or changed",
                ));
                return Ok(());
            }
            FileKind::Gitlink => {
                report.blocks.push(finding(
                    "path",
                    &change.path,
                    "a submodule was added or changed",
                ));
                return Ok(());
            }
            FileKind::File | FileKind::Executable => {}
        }
        for reason in path_flags(&change.path) {
            report.flags.push(finding("risk", &change.path, reason));
        }
        let bytes = isolation.blob(&change.id)?;
        report.added_bytes = report
            .added_bytes
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        let sniff = bytes.get(..BINARY_SNIFF_BYTES).unwrap_or(&bytes);
        if sniff.contains(&0) && !self.config.allow_binary {
            report
                .blocks
                .push(finding("size", &change.path, "a binary file was added"));
        }
        for rule in self.secrets.scan(&bytes)? {
            report.blocks.push(finding(
                "secret",
                &change.path,
                format!("matches the secret rule {rule}"),
            ));
        }
        let previous = change
            .previous_id
            .as_deref()
            .map(|id| isolation.blob(id))
            .transpose()?;
        for reason in code_flags(&change.path, previous.as_deref(), &bytes) {
            report.flags.push(finding("risk", &change.path, reason));
        }
        Ok(())
    }
}
