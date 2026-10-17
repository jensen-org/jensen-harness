//! Turns each tool's structured result into the shortest text that still
//! answers the model's question.

use std::collections::BTreeMap;

use super::generic::Limits;
use super::outline;
use super::shell;
use super::store::RawStore;
use super::{CompactError, Compacted};
use crate::tools::edit::EditOutcome;
use crate::tools::grep::{self, Hit};

const WHOLE_FILE_MAX_LINES: usize = 200;
const WHOLE_FILE_MAX_BYTES: usize = 12_000;
const RANGE_DEFAULT_LINES: usize = 200;
const SYMBOL_MATCHES_MAX: usize = 3;
const GREP_PER_FILE: usize = 8;
const GREP_TOTAL: usize = 60;
const GREP_LINE_CHARS: usize = 200;
const GLOB_LISTED_MAX: usize = 50;
const GLOB_PER_DIRECTORY: usize = 5;
const EXPAND_MAX_LINES: usize = 400;

#[derive(Debug, Clone, Default)]
pub struct ReadArgs {
    pub symbol: Option<String>,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

fn numbered(text: &str, first_line: usize) -> String {
    text.lines()
        .enumerate()
        .map(|(index, line)| format!("{}\t{line}", first_line + index))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A file is sent whole only when it is small. Otherwise the model gets its
/// outline, or one symbol, or one line range, whichever it asked for.
///
/// # Errors
/// [`CompactError::Parse`] when a grammar cannot be loaded.
pub fn read(path: &str, content: &str, args: &ReadArgs) -> Result<Compacted, CompactError> {
    let total_lines = content.lines().count();
    let text = if let Some(symbol) = &args.symbol {
        read_symbol(path, content, symbol)?
    } else if args.start_line.is_some() || args.end_line.is_some() {
        read_range(content, total_lines, args)
    } else if total_lines <= WHOLE_FILE_MAX_LINES && content.len() <= WHOLE_FILE_MAX_BYTES {
        content.to_string()
    } else if let Some(outline) = outline::outline(path, content)? {
        format!(
            "{path}: {total_lines} lines, outline only. Pass symbol, or start_line and end_line, for bodies.\n{outline}"
        )
    } else {
        let head: String = content
            .lines()
            .take(RANGE_DEFAULT_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "{path}: {total_lines} lines, showing the first {RANGE_DEFAULT_LINES}. Pass start_line and end_line for more.\n{}",
            numbered(&head, 1)
        )
    };
    Ok(Compacted::new(content, text))
}

fn read_symbol(path: &str, content: &str, symbol: &str) -> Result<String, CompactError> {
    let matches = outline::symbol(path, content, symbol)?;
    if matches.is_empty() {
        let outline = outline::outline(path, content)?.unwrap_or_default();
        return Ok(format!(
            "no declaration named {symbol} in {path}.\n{outline}"
        ));
    }
    let shown = matches
        .iter()
        .take(SYMBOL_MATCHES_MAX)
        .map(|found| {
            format!(
                "{path}:{}-{}\n{}",
                found.start_line,
                found.end_line,
                numbered(&found.text, found.start_line)
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let extra = matches.len().saturating_sub(SYMBOL_MATCHES_MAX);
    Ok(if extra > 0 {
        format!("{shown}\n[{extra} more declarations named {symbol}]")
    } else {
        shown
    })
}

fn read_range(content: &str, total_lines: usize, args: &ReadArgs) -> String {
    let start = args.start_line.unwrap_or(1).max(1);
    let end = args
        .end_line
        .unwrap_or(start + RANGE_DEFAULT_LINES - 1)
        .min(total_lines);
    if start > total_lines {
        return format!("the file has {total_lines} lines; start_line {start} is past the end");
    }
    let body: String = content
        .lines()
        .skip(start - 1)
        .take(end.saturating_sub(start) + 1)
        .collect::<Vec<_>>()
        .join("\n");
    numbered(&body, start)
}

fn clip(line: &str) -> String {
    if line.chars().count() <= GREP_LINE_CHARS {
        return line.trim().to_string();
    }
    let kept: String = line.trim().chars().take(GREP_LINE_CHARS).collect();
    format!("{kept}…")
}

/// Matches grouped by file, capped per file and in total, with the count of
/// what was left out so the model can narrow the search.
#[must_use]
pub fn grep(hits: &[Hit]) -> Compacted {
    let raw: String = hits
        .iter()
        .map(|hit| format!("{}:{}:{}", hit.path, hit.line_number, hit.line))
        .collect::<Vec<_>>()
        .join("\n");
    if hits.is_empty() {
        return Compacted::new(&raw, "no matches".to_string());
    }
    let mut by_file: BTreeMap<&str, Vec<&Hit>> = BTreeMap::new();
    for hit in hits {
        by_file.entry(hit.path.as_str()).or_default().push(hit);
    }
    let mut out = Vec::new();
    let mut shown = 0usize;
    for (path, file_hits) in &by_file {
        if shown >= GREP_TOTAL {
            break;
        }
        out.push(format!("{path} ({})", file_hits.len()));
        for hit in file_hits.iter().take(GREP_PER_FILE) {
            if shown >= GREP_TOTAL {
                break;
            }
            out.push(format!("  {}: {}", hit.line_number, clip(&hit.line)));
            shown += 1;
        }
        let hidden = file_hits.len().saturating_sub(GREP_PER_FILE);
        if hidden > 0 {
            out.push(format!("  ... +{hidden} more in this file"));
        }
    }
    let left_out = hits.len().saturating_sub(shown);
    if left_out > 0 {
        let at_least = if hits.len() >= grep::MAX_HITS {
            "+"
        } else {
            ""
        };
        out.push(format!(
            "[{shown} of {}{at_least} matches shown across {} files; narrow the pattern or the path]",
            hits.len(),
            by_file.len()
        ));
    }
    Compacted::new(&raw, out.join("\n"))
}

/// A short list as is; a long one as directories with counts and a few names.
#[must_use]
pub fn glob(paths: &[String]) -> Compacted {
    let raw = paths.join("\n");
    if paths.is_empty() {
        return Compacted::new(&raw, "no files".to_string());
    }
    if paths.len() <= GLOB_LISTED_MAX {
        return Compacted::new(&raw, raw.clone());
    }
    let mut by_directory: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for path in paths {
        let (directory, name) = path.split_once('/').unwrap_or((".", path.as_str()));
        by_directory.entry(directory).or_default().push(name);
    }
    let mut out = vec![format!(
        "{} files in {} directories",
        paths.len(),
        by_directory.len()
    )];
    for (directory, names) in &by_directory {
        let listed = names
            .iter()
            .take(GLOB_PER_DIRECTORY)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        let more = names.len().saturating_sub(GLOB_PER_DIRECTORY);
        out.push(if more > 0 {
            format!("{directory}/ ({}): {listed}, +{more} more", names.len())
        } else {
            format!("{directory}/ ({}): {listed}", names.len())
        });
    }
    out.push("[narrow the pattern to list more]".to_string());
    Compacted::new(&raw, out.join("\n"))
}

#[must_use]
pub fn edit(path: &str, outcome: &EditOutcome) -> Compacted {
    let text = format!(
        "edited {path}: {} replacement{}, first at line {}",
        outcome.replacements,
        if outcome.replacements == 1 { "" } else { "s" },
        outcome.first_line
    );
    Compacted::new(&text, text.clone())
}

/// Output filtered by command. Whatever a filter cuts is teed to `store`
/// first, and the result ends with the handle to ask for it back.
///
/// # Errors
/// [`CompactError::Store`] when the raw output cannot be kept.
pub fn shell(
    command: &str,
    exit_code: i32,
    stdout: &str,
    stderr: &str,
    store: &RawStore,
) -> Result<Compacted, CompactError> {
    let limits = Limits::default();
    let out = shell::filter(command, stdout, limits);
    let err = shell::filter(command, stderr, limits);
    let mut parts = Vec::new();
    if exit_code != 0 {
        parts.push(format!("exit {exit_code}"));
    }
    if !out.text.is_empty() {
        parts.push(out.text.clone());
    }
    if !err.text.is_empty() {
        parts.push(if out.text.is_empty() {
            err.text.clone()
        } else {
            format!("stderr:\n{}", err.text)
        });
    }
    if out.text.is_empty() && err.text.is_empty() {
        parts.push("(no output)".to_string());
    }
    let mut text = parts.join("\n");
    let raw = format!("{stdout}{stderr}");
    if out.lossy || err.lossy {
        let handle = store.tee(&format!("stdout:\n{stdout}\nstderr:\n{stderr}"))?;
        text.push_str(&format!("\n[filtered; full output: expand {handle}]"));
    }
    Ok(Compacted::new(&raw, text))
}

/// A slice of a stored raw output.
///
/// # Errors
/// [`CompactError::UnknownHandle`] for a handle this store did not issue.
pub fn expand(
    store: &RawStore,
    handle: &str,
    offset: usize,
    limit: usize,
) -> Result<Compacted, CompactError> {
    let text = store.slice(handle, offset, limit.min(EXPAND_MAX_LINES))?;
    Ok(Compacted::new(&text, text.clone()))
}
