#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_lines: usize,
    pub head: usize,
    pub tail: usize,
    pub max_line_chars: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_lines: 120,
            head: 40,
            tail: 60,
            max_line_chars: 300,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filtered {
    pub text: String,
    pub lossy: bool,
}

impl Filtered {
    #[must_use]
    pub fn exact(text: String) -> Self {
        Self { text, lossy: false }
    }

    #[must_use]
    pub fn lossy(text: String) -> Self {
        Self { text, lossy: true }
    }
}

#[must_use]
pub fn strip_ansi(text: &str) -> String {
    // The stripper treats a tab as a control code and drops it.
    const TAB: char = '\u{E000}';
    strip_ansi_escapes::strip_str(text.replace('\t', &TAB.to_string())).replace(TAB, "\t")
}

/// A run of three or more identical lines becomes one line with a count.
#[must_use]
pub fn collapse_repeats(lines: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    let mut index = 0;
    while let Some(line) = lines.get(index) {
        let run = lines
            .iter()
            .skip(index)
            .take_while(|candidate| *candidate == line)
            .count();
        if run >= 3 {
            out.push(format!("{line} (x{run})"));
        } else {
            out.extend(std::iter::repeat_n(line.clone(), run));
        }
        index += run;
    }
    out
}

fn clip(line: &str, max_chars: usize) -> (String, bool) {
    if line.chars().count() <= max_chars {
        return (line.to_string(), false);
    }
    let kept: String = line.chars().take(max_chars).collect();
    (format!("{kept}…"), true)
}

/// Clips long lines and keeps the head and the tail of a long listing,
/// since the start says what ran and the end says how it finished.
#[must_use]
pub fn cap(lines: Vec<String>, limits: Limits) -> Filtered {
    let mut lossy = false;
    let mut clipped: Vec<String> = lines
        .iter()
        .map(|line| {
            let (text, cut) = clip(line.trim_end(), limits.max_line_chars);
            lossy |= cut;
            text
        })
        .collect();
    while clipped.last().is_some_and(String::is_empty) {
        clipped.pop();
    }
    if clipped.len() > limits.max_lines {
        let cut = clipped.len().saturating_sub(limits.head + limits.tail);
        let mut kept: Vec<String> = clipped.iter().take(limits.head).cloned().collect();
        kept.push(format!("[... {cut} lines cut ...]"));
        kept.extend(
            clipped
                .iter()
                .skip(clipped.len().saturating_sub(limits.tail))
                .cloned(),
        );
        return Filtered::lossy(kept.join("\n"));
    }
    Filtered {
        text: clipped.join("\n"),
        lossy,
    }
}

/// What every output gets when no command specific filter applies.
#[must_use]
pub fn generic(text: &str, limits: Limits) -> Filtered {
    let lines: Vec<String> = strip_ansi(text)
        .replace("\r\n", "\n")
        .lines()
        .map(str::to_string)
        .collect();
    cap(collapse_repeats(&lines), limits)
}
