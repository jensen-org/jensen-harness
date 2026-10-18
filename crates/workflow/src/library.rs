use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::validate::SpecError;

#[derive(Debug, Clone, Default)]
pub struct Library {
    dirs: Vec<PathBuf>,
}

impl Library {
    #[must_use]
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self { dirs }
    }

    #[must_use]
    pub fn for_workflow(file: &Path, configured: &[PathBuf], user: Option<PathBuf>) -> Self {
        let mut dirs = Vec::new();
        if let Some(parent) = file.parent() {
            dirs.push(parent.join("agents"));
        }
        dirs.extend(configured.iter().cloned());
        dirs.extend(user);
        Self { dirs }
    }

    fn find(&self, node: &str, template: &str) -> Result<Value, SpecError> {
        if template.is_empty()
            || template.starts_with('.')
            || !template
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(SpecError::TemplateInvalid {
                template: template.to_owned(),
                message: "a template name is letters, digits, dashes and underscores".to_owned(),
            });
        }
        let mut found: Vec<PathBuf> = Vec::new();
        for dir in &self.dirs {
            for extension in ["yaml", "yml"] {
                let candidate = dir.join(format!("{template}.{extension}"));
                if candidate.is_file() && !found.iter().any(|seen| same_file(seen, &candidate)) {
                    found.push(candidate);
                }
            }
        }
        match found.as_slice() {
            [] => Err(SpecError::TemplateMissing {
                node: node.to_owned(),
                template: template.to_owned(),
            }),
            [only] => {
                let text =
                    std::fs::read_to_string(only).map_err(|error| SpecError::TemplateInvalid {
                        template: template.to_owned(),
                        message: format!("{}: {error}", only.display()),
                    })?;
                serde_saphyr::from_str(&text).map_err(|error| SpecError::TemplateInvalid {
                    template: template.to_owned(),
                    message: format!("{}: {error}", only.display()),
                })
            }
            [first, second, ..] => Err(SpecError::TemplateAmbiguous {
                node: node.to_owned(),
                template: template.to_owned(),
                first: first.display().to_string(),
                second: second.display().to_string(),
            }),
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn merge(base: Value, over: Value) -> Value {
    match (base, over) {
        (Value::Object(mut base), Value::Object(over)) => {
            for (key, value) in over {
                let merged = match base.remove(&key) {
                    Some(existing) => merge(existing, value),
                    None => value,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (Value::Array(mut base), Value::Array(over)) => {
            for item in over {
                if !base.contains(&item) {
                    base.push(item);
                }
            }
            Value::Array(base)
        }
        (_, over) => over,
    }
}

fn expand(
    library: &Library,
    node: &str,
    mut body: Map<String, Value>,
    chain: &mut Vec<String>,
) -> Result<Map<String, Value>, SpecError> {
    let Some(uses) = body.remove("uses") else {
        return Ok(body);
    };
    let Value::String(template) = uses else {
        return Err(SpecError::TemplateInvalid {
            template: String::new(),
            message: format!("`uses` of node `{node}` must be a template name"),
        });
    };
    if chain.contains(&template) {
        chain.push(template);
        return Err(SpecError::TemplateCycle {
            node: node.to_owned(),
            chain: chain.join(" -> "),
        });
    }
    chain.push(template.clone());
    let Value::Object(mut base) = library.find(node, &template)? else {
        return Err(SpecError::TemplateInvalid {
            template,
            message: "a template is a mapping".to_owned(),
        });
    };
    normalize_mcp(&mut base);
    normalize_mcp(&mut body);
    let base = expand(library, node, base, chain)?;
    chain.pop();
    let Value::Object(merged) = merge(Value::Object(base), Value::Object(body)) else {
        return Err(SpecError::TemplateInvalid {
            template,
            message: "the merge did not give a mapping".to_owned(),
        });
    };
    Ok(merged)
}

fn names(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn push_unique(target: &mut Map<String, Value>, key: &str, extra: Vec<String>) {
    let list = target
        .entry(key.to_owned())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Value::Array(items) = list {
        for name in extra {
            let value = Value::String(name);
            if !items.contains(&value) {
                items.push(value);
            }
        }
    }
}

fn normalize_mcp(body: &mut Map<String, Value>) {
    if let Some(Value::Array(items)) = body.get("mcp") {
        let map: Map<String, Value> = items
            .iter()
            .filter_map(|item| item.as_str())
            .map(|name| (name.to_owned(), Value::Object(Map::new())))
            .collect();
        body.insert("mcp".to_owned(), Value::Object(map));
    }
}

fn apply_declarations(
    node: &str,
    body: &mut Map<String, Value>,
    channels: &Map<String, Value>,
) -> Result<(), SpecError> {
    let template = body
        .get("template_name")
        .and_then(Value::as_str)
        .unwrap_or("template")
        .to_owned();
    body.remove("description");
    body.remove("template_name");
    if let Some(Value::Object(inputs)) = body.remove("inputs") {
        push_unique(body, "reads", inputs.keys().cloned().collect());
    }
    if let Some(outputs) = body.remove("outputs") {
        if let Value::Object(map) = &outputs {
            for (channel, declared) in map {
                let wanted = declared.get("schema");
                let actual = channels.get(channel).and_then(|c| c.get("schema"));
                if let (Some(wanted), Some(actual)) = (wanted, actual)
                    && wanted != actual
                {
                    return Err(SpecError::TemplateOutputMismatch {
                        node: node.to_owned(),
                        template,
                        channel: channel.clone(),
                        declared: wanted.to_string(),
                        found: actual.to_string(),
                    });
                }
            }
        }
        push_unique(body, "writes", names(Some(&outputs)));
    }
    if let Some(Value::Object(context)) = body.get("context") {
        let inherited: Vec<String> = match context.get("inherit") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|item| item.get("channel").and_then(Value::as_str))
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        };
        push_unique(body, "reads", inherited);
    }
    normalize_mcp(body);
    Ok(())
}

pub fn resolve(yaml: &str, library: &Library) -> Result<Value, SpecError> {
    let mut root: Value =
        serde_saphyr::from_str(yaml).map_err(|error| SpecError::Yaml(error.to_string()))?;
    let channels = root
        .get("channels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let Some(Value::Object(nodes)) = root.get_mut("nodes") else {
        return Ok(root);
    };
    for (id, node) in nodes.iter_mut() {
        let Value::Object(body) = node else {
            continue;
        };
        let is_agent = body.get("kind").and_then(Value::as_str) == Some("agent");
        if body.contains_key("uses") && !is_agent {
            return Err(SpecError::TemplateInvalid {
                template: String::new(),
                message: format!("node `{id}` uses a template but is not an agent"),
            });
        }
        let used = body.get("uses").cloned();
        let mut expanded = expand(library, id, std::mem::take(body), &mut Vec::new())?;
        if let Some(Value::String(name)) = used {
            expanded.insert("template_name".to_owned(), Value::String(name));
        }
        if is_agent {
            apply_declarations(id, &mut expanded, &channels)?;
        }
        *body = expanded;
    }
    Ok(root)
}
