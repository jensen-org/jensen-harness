use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use cap_std::ambient_authority;
use cap_std::fs::Dir;

use async_trait::async_trait;
use jensen_harness_core::providers::ToolSpec;
use jensen_harness_core::toolset::{ProviderReply, Replay, ToolProvider, ToolSetError};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 1024;
const MAX_COMPATIBILITY: usize = 500;
const MAX_FILE_BYTES: u64 = 256 * 1024;
pub const TOOL: &str = "skill";

#[derive(Debug, Error)]
pub enum SkillError {
    #[error("{path}: {reason}")]
    Invalid { path: String, reason: String },
    #[error("skill `{0}` is not in any skills directory")]
    Unknown(String),
    #[error("skill `{name}` is in two skills directories: {first} and {second}")]
    Shadowed {
        name: String,
        first: String,
        second: String,
    },
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    compatibility: Option<String>,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub dir: PathBuf,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    skills: BTreeMap<String, Skill>,
}

fn invalid(path: &Path, reason: impl Into<String>) -> SkillError {
    SkillError::Invalid {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    let (front, after) = rest.split_at(end);
    let body = after
        .strip_prefix("\n---")?
        .trim_start_matches(['\r', '\n']);
    Some((front, body))
}

fn parse(file: &Path, directory: &str, text: &str) -> Result<Skill, SkillError> {
    let (front, body) = split_frontmatter(text)
        .ok_or_else(|| invalid(file, "no YAML frontmatter between --- lines"))?;
    let parsed: Frontmatter = serde_saphyr::from_str(front)
        .map_err(|error| invalid(file, format!("frontmatter: {error}")))?;
    if !valid_name(&parsed.name) {
        return Err(invalid(
            file,
            format!(
                "name `{}` must be 1 to {MAX_NAME} lowercase letters, digits and single hyphens, not starting or ending with one",
                parsed.name
            ),
        ));
    }
    if parsed.name != directory {
        return Err(invalid(
            file,
            format!(
                "name `{}` must match its directory `{directory}`",
                parsed.name
            ),
        ));
    }
    if parsed.description.trim().is_empty() || parsed.description.len() > MAX_DESCRIPTION {
        return Err(invalid(
            file,
            format!("description must be 1 to {MAX_DESCRIPTION} characters"),
        ));
    }
    if parsed
        .compatibility
        .as_ref()
        .is_some_and(|text| text.len() > MAX_COMPATIBILITY)
    {
        return Err(invalid(
            file,
            format!("compatibility is over {MAX_COMPATIBILITY} characters"),
        ));
    }
    Ok(Skill {
        name: parsed.name,
        description: parsed.description.trim().to_owned(),
        body: body.to_owned(),
        dir: file.parent().map(Path::to_path_buf).unwrap_or_default(),
        license: parsed.license,
        compatibility: parsed.compatibility,
        allowed_tools: parsed
            .allowed_tools
            .map(|list| list.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
    })
}

impl SkillCatalog {
    pub fn discover(roots: &[PathBuf]) -> Result<Self, SkillError> {
        let mut skills: BTreeMap<String, Skill> = BTreeMap::new();
        for root in roots {
            let Ok(entries) = std::fs::read_dir(root) else {
                continue;
            };
            let Ok(confined_root) = Dir::open_ambient_dir(root, ambient_authority()) else {
                continue;
            };
            let mut found: Vec<_> = entries.filter_map(Result::ok).collect();
            found.sort_by_key(std::fs::DirEntry::file_name);
            for entry in found {
                let directory = entry.file_name().to_string_lossy().into_owned();
                let file = entry.path().join("SKILL.md");
                let Ok(meta) = confined_root.symlink_metadata(&directory) else {
                    continue;
                };
                if !meta.is_dir() {
                    continue;
                }
                let dir = confined_root
                    .open_dir(&directory)
                    .map_err(|error| invalid(&file, error.to_string()))?;
                match dir.symlink_metadata("SKILL.md") {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(invalid(&file, error.to_string())),
                    Ok(meta) if !meta.is_file() => {
                        return Err(invalid(&file, "SKILL.md is not a regular file"));
                    }
                    Ok(_) => {}
                }
                let text = read_capped(&dir, "SKILL.md", MAX_FILE_BYTES)
                    .map_err(|reason| invalid(&file, reason))?;
                let skill = parse(&file, &directory, &text)?;
                if let Some(first) = skills.get(&skill.name) {
                    return Err(SkillError::Shadowed {
                        name: skill.name,
                        first: first.dir.display().to_string(),
                        second: skill.dir.display().to_string(),
                    });
                }
                skills.insert(skill.name.clone(), skill);
            }
        }
        Ok(Self { skills })
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.skills.keys().map(String::as_str)
    }

    pub fn select(&self, names: &[String]) -> Result<Vec<Skill>, SkillError> {
        let mut picked: Vec<Skill> = Vec::new();
        for name in names {
            let skill = self
                .skills
                .get(name)
                .ok_or_else(|| SkillError::Unknown(name.clone()))?;
            if !picked.iter().any(|seen| seen.name == skill.name) {
                picked.push(skill.clone());
            }
        }
        picked.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(picked)
    }
}

pub struct SkillProvider {
    skills: Vec<Skill>,
}

impl SkillProvider {
    #[must_use]
    pub fn new(skills: Vec<Skill>) -> Self {
        Self { skills }
    }

    fn find(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }
}

fn read_capped(dir: &Dir, relative: &str, limit: u64) -> Result<String, String> {
    let before = dir
        .symlink_metadata(relative)
        .map_err(|error| format!("{relative}: {error}"))?;
    if !before.is_file() {
        return Err(format!("{relative} is not a regular file"));
    }
    let file = dir
        .open(relative)
        .map_err(|error| format!("{relative}: {error}"))?;
    let meta = file.metadata().map_err(|error| error.to_string())?;
    if !meta.is_file() {
        return Err(format!("{relative} is not a regular file"));
    }
    let mut text = String::new();
    file.take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("{relative}: {error}"))?;
    if u64::try_from(text.len()).unwrap_or(u64::MAX) > limit {
        return Err(format!("{relative} is over {limit} bytes"));
    }
    Ok(text)
}

#[async_trait]
impl ToolProvider for SkillProvider {
    fn specs(&self) -> Vec<ToolSpec> {
        let names: Vec<&str> = self
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        vec![ToolSpec {
            name: TOOL.to_owned(),
            description: "Load an attached skill's instructions, or one of its bundled files."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "enum": names},
                    "file": {"type": "string"},
                },
                "required": ["name"],
            }),
        }]
    }

    fn replay(&self, _name: &str) -> Replay {
        Replay::Repeat
    }

    fn system_note(&self) -> Option<String> {
        let mut note = String::from(
            "Skills you can load with the skill tool when a task matches (name: description):",
        );
        for skill in &self.skills {
            note.push_str(&format!("\n- {}: {}", skill.name, skill.description));
        }
        Some(note)
    }

    async fn invoke(&self, _name: &str, input: &Value) -> Result<ProviderReply, ToolSetError> {
        let reply = |text: String, is_error: bool| Ok(ProviderReply { text, is_error });
        let Some(wanted) = input.get("name").and_then(Value::as_str) else {
            return reply("ERROR skill: name is required".to_owned(), true);
        };
        let Some(skill) = self.find(wanted) else {
            return reply(format!("ERROR skill: no attached skill `{wanted}`"), true);
        };
        let Some(file) = input.get("file").and_then(Value::as_str) else {
            return reply(skill.body.clone(), false);
        };
        let dir = match Dir::open_ambient_dir(&skill.dir, ambient_authority()) {
            Ok(dir) => dir,
            Err(error) => return reply(format!("ERROR skill: {error}"), true),
        };
        match read_capped(&dir, file, MAX_FILE_BYTES) {
            Ok(text) => reply(text, false),
            Err(message) => reply(format!("ERROR skill: {message}"), true),
        }
    }
}
