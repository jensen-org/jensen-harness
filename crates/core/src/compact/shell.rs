//! Command aware filters for shell output. The command is parsed only to
//! pick a filter and is never rewritten; anything compound (pipes, `&&`,
//! redirects, substitutions) or unrecognized gets the generic filter.

use super::generic::{self, Filtered, Limits};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub program: String,
    pub subcommand: Option<String>,
}

const OPERATOR_CHARS: [char; 7] = ['|', '&', ';', '<', '>', '`', '$'];

#[must_use]
pub fn parse_command(command: &str) -> Option<Parsed> {
    if command.contains('\n') {
        return None;
    }
    let tokens = shlex::split(command)?;
    if tokens.iter().any(|token| token.contains(OPERATOR_CHARS)) {
        return None;
    }
    let mut rest = tokens
        .iter()
        .skip_while(|token| token.contains('=') && !token.starts_with('-'));
    let program = rest.next()?;
    let program = program.rsplit('/').next().unwrap_or(program).to_string();
    let mut subcommand = None;
    let mut skip_next = false;
    for token in rest {
        if skip_next {
            skip_next = false;
        } else if program == "git" && matches!(token.as_str(), "-C" | "-c") {
            skip_next = true;
        } else if !token.starts_with('-') && !token.starts_with('+') {
            subcommand = Some(token.clone());
            break;
        }
    }
    Some(Parsed {
        program,
        subcommand,
    })
}

type FilterFn = fn(&str) -> Filtered;

fn select(parsed: &Parsed) -> Option<FilterFn> {
    let sub = parsed.subcommand.as_deref();
    match (parsed.program.as_str(), sub) {
        ("cargo", Some("test" | "nextest")) => Some(cargo_test),
        ("cargo", Some("build" | "b" | "check" | "c" | "clippy" | "doc")) => Some(cargo_build),
        ("git", Some("status")) => Some(git_status),
        ("git", Some("log")) => Some(git_log),
        ("git", Some("diff")) => Some(git_diff),
        ("pytest" | "py.test", _) => Some(pytest),
        ("go", Some("test")) => Some(go_test),
        ("npm" | "pnpm" | "yarn" | "bun", Some("test")) | ("jest" | "vitest", _) => Some(js_test),
        _ => None,
    }
}

#[must_use]
pub fn filter(command: &str, output: &str, limits: Limits) -> Filtered {
    let cleaned = generic::strip_ansi(output).replace("\r\n", "\n");
    let specific = parse_command(command)
        .and_then(|parsed| select(&parsed))
        .map(|run| run(&cleaned));
    let Some(specific) = specific else {
        return generic::generic(&cleaned, limits);
    };
    let lines: Vec<String> = specific.text.lines().map(str::to_string).collect();
    let capped = generic::cap(generic::collapse_repeats(&lines), limits);
    Filtered {
        text: capped.text,
        lossy: specific.lossy || capped.lossy,
    }
}

fn blocks_starting_with(lines: &[&str], prefixes: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut open = false;
    for line in lines {
        if prefixes.iter().any(|prefix| line.starts_with(prefix)) {
            open = true;
        } else if line.trim().is_empty() {
            if open {
                out.push(String::new());
            }
            open = false;
        }
        if open {
            out.push((*line).to_string());
        }
    }
    out
}

fn is_cargo_noise(line: &str) -> bool {
    let trimmed = line.trim_start();
    (line.starts_with("test ") && (line.ends_with(" ... ok") || line.contains(" ... ignored")))
        || (line.starts_with("running ") && line.contains(" test"))
        || [
            "Compiling ",
            "Running ",
            "Doc-tests ",
            "Finished ",
            "Fresh ",
            "Blocking ",
        ]
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

fn cargo_test(text: &str) -> Filtered {
    let all: Vec<&str> = text.lines().collect();
    let failures_at = all.iter().position(|line| line.trim_end() == "failures:");
    let head = failures_at
        .and_then(|at| all.get(..at))
        .unwrap_or(all.as_slice());
    let mut kept = blocks_starting_with(head, &["error", "warning"]);
    match failures_at {
        Some(at) => kept.extend(
            all.iter()
                .skip(at)
                .filter(|line| !is_cargo_noise(line))
                .map(|line| (*line).to_string()),
        ),
        None => kept.extend(
            all.iter()
                .filter(|line| line.starts_with("test result:"))
                .map(|line| (*line).to_string()),
        ),
    }
    if kept.iter().all(|line| line.trim().is_empty()) {
        return Filtered::lossy("ok (no test results printed)".to_string());
    }
    Filtered::lossy(kept.join("\n"))
}

fn cargo_build(text: &str) -> Filtered {
    let all: Vec<&str> = text.lines().collect();
    let mut kept = blocks_starting_with(&all, &["error", "warning"]);
    if let Some(finished) = all
        .iter()
        .rev()
        .find(|line| line.trim_start().starts_with("Finished "))
    {
        kept.push(finished.trim_start().to_string());
    }
    if kept.is_empty() {
        return Filtered::lossy("ok".to_string());
    }
    Filtered::lossy(kept.join("\n"))
}

fn git_status(text: &str) -> Filtered {
    let mut branch = None;
    let mut tracking = None;
    let mut section = "";
    let mut entries: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("On branch ") {
            branch = Some(name.to_string());
        } else if line.starts_with("Your branch") {
            tracking = Some(line.to_string());
        } else if line.starts_with("Changes to be committed") {
            section = "staged";
        } else if line.starts_with("Changes not staged") {
            section = "unstaged";
        } else if line.starts_with("Untracked files") {
            section = "untracked";
        } else if let Some(entry) = line.strip_prefix('\t') {
            let (kind, path) = entry.split_once(':').map_or(("?", entry), |(kind, path)| {
                (
                    match kind {
                        "modified" => "M",
                        "new file" => "A",
                        "deleted" => "D",
                        "renamed" => "R",
                        _ => "?",
                    },
                    path.trim(),
                )
            });
            entries.push(format!("{section} {kind} {path}"));
        }
    }
    let Some(branch) = branch else {
        return Filtered::exact(text.to_string());
    };
    let mut out = vec![format!("branch {branch}")];
    out.extend(tracking);
    if entries.is_empty() {
        out.push("clean".to_string());
    }
    out.extend(entries);
    Filtered::lossy(out.join("\n"))
}

fn git_log(text: &str) -> Filtered {
    let mut out = Vec::new();
    let mut sha = None;
    let mut seen_blank = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("commit ") {
            sha = Some(rest.chars().take(7).collect::<String>());
            seen_blank = false;
        } else if line.trim().is_empty() {
            seen_blank = true;
        } else if seen_blank && let Some(short) = sha.take() {
            out.push(format!("{short} {}", line.trim()));
        }
    }
    if out.is_empty() {
        return Filtered::exact(text.to_string());
    }
    Filtered::lossy(out.join("\n"))
}

fn git_diff(text: &str) -> Filtered {
    let mut out = Vec::new();
    let mut dropped = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest.rsplit_once(" b/").map_or(rest, |(_, path)| path);
            out.push(format!("## {path}"));
        } else if line.starts_with("new file mode") {
            out.push("new file".to_string());
        } else if line.starts_with("deleted file mode") {
            out.push("deleted".to_string());
        } else if line.starts_with("rename ") || line.starts_with("Binary files") {
            out.push(line.to_string());
        } else if line.starts_with("@@") || line.starts_with('+') || line.starts_with('-') {
            if !(line.starts_with("+++ ") || line.starts_with("--- ")) {
                out.push(line.to_string());
            }
        } else {
            dropped = true;
        }
    }
    if out.is_empty() {
        return Filtered::exact(text.to_string());
    }
    Filtered {
        text: out.join("\n"),
        lossy: dropped,
    }
}

fn pytest(text: &str) -> Filtered {
    let mut out = Vec::new();
    let mut in_failures = false;
    for line in text.lines() {
        if line.starts_with('=') {
            if line.contains("FAILURES") || line.contains(" ERRORS ") {
                in_failures = true;
                out.push(line.to_string());
                continue;
            }
            in_failures = false;
            let summary = ["passed", "failed", "error", "no tests ran", "skipped"]
                .iter()
                .any(|word| line.contains(word));
            if summary {
                out.push(line.to_string());
            }
        } else if in_failures || line.starts_with("FAILED ") || line.starts_with("ERROR ") {
            out.push(line.to_string());
        }
    }
    Filtered::lossy(out.join("\n"))
}

fn go_test(text: &str) -> Filtered {
    let mut out = Vec::new();
    let mut ok_packages = 0usize;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if line.starts_with("ok ") || line.starts_with("ok\t") {
            ok_packages += 1;
        } else if ["=== RUN", "=== PAUSE", "=== CONT", "=== NAME", "--- PASS"]
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
            || trimmed == "PASS"
        {
            continue;
        } else {
            out.push(line.to_string());
        }
    }
    if ok_packages > 0 {
        out.push(format!("ok: {ok_packages} packages"));
    }
    Filtered::lossy(out.join("\n"))
}

fn js_test(text: &str) -> Filtered {
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with('✓')
                || trimmed.starts_with('√')
                || trimmed.starts_with('✔')
                || trimmed.starts_with("PASS "))
        })
        .collect();
    Filtered::lossy(kept.join("\n"))
}
