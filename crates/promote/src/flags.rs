use std::collections::BTreeMap;

use tree_sitter::{Language, Node, Parser};

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
}

fn lang_of(path: &str) -> Option<Lang> {
    let extension = path.rsplit_once('.')?.1;
    Some(match extension {
        "rs" => Lang::Rust,
        "py" => Lang::Python,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "ts" => Lang::TypeScript,
        "tsx" => Lang::Tsx,
        "go" => Lang::Go,
        _ => return None,
    })
}

fn language(lang: Lang) -> Language {
    match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    }
}

const MANIFESTS: [&str; 12] = [
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "requirements.txt",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.rs",
    "Dockerfile",
    "Makefile",
    "setup.py",
];

const LOCKFILES: [&str; 8] = [
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "go.sum",
    "Gemfile.lock",
    "uv.lock",
];

#[must_use]
pub fn path_flags(path: &str) -> Vec<&'static str> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut flags = Vec::new();
    if MANIFESTS.contains(&name) {
        flags.push("build or dependency manifest changed");
    }
    if LOCKFILES.contains(&name) {
        flags.push("lockfile changed");
    }
    flags
}

fn call_flags(lang: Lang, node: Node<'_>, source: &[u8]) -> Option<&'static str> {
    let text = |node: Node<'_>| node.utf8_text(source).unwrap_or_default();
    match (lang, node.kind()) {
        (Lang::Rust, "unsafe") => Some("unsafe code"),
        (Lang::Rust, "call_expression") => {
            let function = node.child_by_field_name("function").map(text)?;
            (function.contains("Command::new")).then_some("spawns a process")
        }
        (Lang::Python, "call") => {
            let function = node.child_by_field_name("function").map(text)?;
            if function.starts_with("subprocess.")
                || function == "os.system"
                || function == "os.popen"
            {
                Some("spawns a process")
            } else if function == "eval" || function == "exec" {
                Some("evaluates code")
            } else {
                None
            }
        }
        (Lang::JavaScript | Lang::TypeScript | Lang::Tsx, "call_expression") => {
            let function = node.child_by_field_name("function").map(text)?;
            if function == "eval" {
                Some("evaluates code")
            } else if function == "exec"
                || function == "spawn"
                || function == "execSync"
                || function.starts_with("child_process.")
            {
                Some("spawns a process")
            } else {
                None
            }
        }
        (Lang::JavaScript | Lang::TypeScript | Lang::Tsx, "new_expression") => {
            let constructor = node.child_by_field_name("constructor").map(text)?;
            (constructor == "Function").then_some("evaluates code")
        }
        (Lang::Go, "call_expression") => {
            let function = node.child_by_field_name("function").map(text)?;
            function
                .starts_with("exec.Command")
                .then_some("spawns a process")
        }
        _ => None,
    }
}

fn counts(lang: Lang, source: &[u8]) -> BTreeMap<&'static str, usize> {
    let mut found = BTreeMap::new();
    let mut parser = Parser::new();
    if parser.set_language(&language(lang)).is_err() {
        return found;
    }
    let Some(tree) = parser.parse(source, None) else {
        return found;
    };
    let mut cursor = tree.walk();
    loop {
        if let Some(flag) = call_flags(lang, cursor.node(), source) {
            *found.entry(flag).or_insert(0) += 1;
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return found;
            }
        }
    }
}

#[must_use]
pub fn code_flags(path: &str, old: Option<&[u8]>, new: &[u8]) -> Vec<&'static str> {
    let Some(lang) = lang_of(path) else {
        return Vec::new();
    };
    let before = old.map(|bytes| counts(lang, bytes)).unwrap_or_default();
    counts(lang, new)
        .into_iter()
        .filter(|(flag, count)| *count > before.get(flag).copied().unwrap_or(0))
        .map(|(flag, _)| flag)
        .collect()
}
