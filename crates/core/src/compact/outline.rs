//! Signatures instead of bodies. A large source file is sent as its outline,
//! one line per declaration with the lines it spans, and the model asks for
//! the one symbol or line range it actually needs.

use tree_sitter::{Language, Node, Parser};

use super::CompactError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
}

fn lang_for(path: &str) -> Option<Lang> {
    let extension = path.rsplit_once('.').map(|(_, extension)| extension)?;
    Some(match extension {
        "rs" => Lang::Rust,
        "py" => Lang::Python,
        "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
        "ts" | "mts" | "cts" => Lang::TypeScript,
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

/// Declarations that hold a body worth skipping; the walk does not enter them.
fn is_function(lang: Lang, kind: &str) -> bool {
    match lang {
        Lang::Rust => kind == "function_item",
        Lang::Python => kind == "function_definition",
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
            kind,
            "function_declaration" | "method_definition" | "generator_function_declaration"
        ),
        Lang::Go => matches!(kind, "function_declaration" | "method_declaration"),
    }
}

fn is_declaration(lang: Lang, kind: &str) -> bool {
    is_function(lang, kind)
        || match lang {
            Lang::Rust => matches!(
                kind,
                "struct_item"
                    | "enum_item"
                    | "trait_item"
                    | "impl_item"
                    | "mod_item"
                    | "const_item"
                    | "static_item"
                    | "type_item"
                    | "macro_definition"
            ),
            Lang::Python => kind == "class_definition",
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => matches!(
                kind,
                "class_declaration"
                    | "abstract_class_declaration"
                    | "interface_declaration"
                    | "type_alias_declaration"
                    | "enum_declaration"
                    | "lexical_declaration"
            ),
            Lang::Go => matches!(kind, "type_declaration" | "const_declaration"),
        }
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.start_byte()..node.end_byte()).unwrap_or("")
}

fn signature(node: Node<'_>, source: &str) -> String {
    let end = node
        .child_by_field_name("body")
        .map_or_else(|| node.end_byte(), |body| body.start_byte());
    let head = source.get(node.start_byte()..end).unwrap_or("");
    let first_lines = if node.child_by_field_name("body").is_some() {
        head
    } else {
        head.lines().next().unwrap_or("")
    };
    let collapsed = first_lines.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_end_matches(['{', ' ']);
    if trimmed.chars().count() > 160 {
        let cut: String = trimmed.chars().take(160).collect();
        format!("{cut}…")
    } else {
        trimmed.to_string()
    }
}

fn declared_name(node: Node<'_>, source: &str) -> Option<String> {
    let direct = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"));
    if let Some(name) = direct {
        return Some(text(name, source).to_string());
    }
    let mut cursor = node.walk();
    let first = node.named_children(&mut cursor).next()?;
    first
        .child_by_field_name("name")
        .map(|name| text(name, source).to_string())
}

fn parse(path: &str, source: &str) -> Result<Option<tree_sitter::Tree>, CompactError> {
    let Some(lang) = lang_for(path) else {
        return Ok(None);
    };
    let mut parser = Parser::new();
    parser
        .set_language(&language(lang))
        .map_err(|error| CompactError::Parse(error.to_string()))?;
    Ok(parser.parse(source, None))
}

fn walk<'tree>(lang: Lang, node: Node<'tree>, depth: usize, out: &mut Vec<(usize, Node<'tree>)>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if is_declaration(lang, child.kind()) {
            out.push((depth, child));
            if !is_function(lang, child.kind()) {
                walk(lang, child, depth + 1, out);
            }
        } else {
            walk(lang, child, depth, out);
        }
    }
}

/// One line per declaration as `start-end  signature`, nested ones
/// indented. `None` for a file type this module has no grammar for.
///
/// # Errors
/// [`CompactError::Parse`] when the grammar cannot be loaded.
pub fn outline(path: &str, source: &str) -> Result<Option<String>, CompactError> {
    let (Some(lang), Some(tree)) = (lang_for(path), parse(path, source)?) else {
        return Ok(None);
    };
    let mut found = Vec::new();
    walk(lang, tree.root_node(), 0, &mut found);
    if found.is_empty() {
        return Ok(None);
    }
    let lines: Vec<String> = found
        .iter()
        .map(|(depth, node)| {
            format!(
                "{}-{}  {}{}",
                node.start_position().row + 1,
                node.end_position().row + 1,
                "  ".repeat(*depth),
                signature(*node, source)
            )
        })
        .collect();
    Ok(Some(lines.join("\n")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolMatch {
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
}

/// Every declaration named `symbol`, each with its full source.
///
/// # Errors
/// [`CompactError::Parse`] when the grammar cannot be loaded.
pub fn symbol(path: &str, source: &str, symbol: &str) -> Result<Vec<SymbolMatch>, CompactError> {
    let (Some(lang), Some(tree)) = (lang_for(path), parse(path, source)?) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    walk(lang, tree.root_node(), 0, &mut found);
    Ok(found
        .into_iter()
        .filter(|(_, node)| declared_name(*node, source).as_deref() == Some(symbol))
        .map(|(_, node)| SymbolMatch {
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            text: text(node, source).to_string(),
        })
        .collect())
}
