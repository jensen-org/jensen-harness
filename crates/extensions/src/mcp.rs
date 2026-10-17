use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jensen_harness_core::providers::ToolSpec;
use jensen_harness_core::sandbox::NetworkPolicy;
use jensen_harness_core::toolset::{ProviderReply, Replay, ToolProvider, ToolSet, ToolSetError};
use rmcp::model::{CallToolRequestParams, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use thiserror::Error;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum McpError {
    #[error("mcp server `{0}` is not configured")]
    Unknown(String),
    #[error("mcp server `{server}` could not be set up: {message}")]
    Config { server: String, message: String },
    #[error("mcp server `{server}` could not be reached: {message}")]
    Connect { server: String, message: String },
}

#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(***)")
    }
}

#[derive(Debug, Clone)]
pub enum McpTransportSpec {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        token: Option<Secret>,
    },
}

#[derive(Debug, Clone)]
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpTransportSpec,
    pub network: NetworkPolicy,
    pub writable: bool,
    pub tools: Vec<String>,
    pub timeout: Duration,
}

pub trait McpDirectory: Send + Sync {
    fn server(&self, name: &str) -> Result<McpServerSpec, McpError>;
}

struct Entry {
    exposed: String,
    original: String,
    spec: ToolSpec,
    read_only: bool,
}

pub struct McpProvider {
    server: String,
    client: RunningService<RoleClient, ()>,
    entries: Vec<Entry>,
    timeout: Duration,
}

fn exposed_name(server: &str, tool: &str) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    format!("mcp__{}__{}", clean(server), clean(tool))
}

fn connect_error(server: &str, error: impl std::fmt::Display) -> McpError {
    McpError::Connect {
        server: server.to_owned(),
        message: error.to_string(),
    }
}

pub async fn connect(
    spec: &McpServerSpec,
    node_allow: &[String],
    tools: &ToolSet,
) -> Result<McpProvider, McpError> {
    let server = spec.name.as_str();
    let client: RunningService<RoleClient, ()> = match &spec.transport {
        McpTransportSpec::Stdio { command, args, env } => {
            let (confined, scratch) = tools
                .confine_process(command, args, spec.network, spec.writable)
                .map_err(|error| McpError::Config {
                    server: server.to_owned(),
                    message: error.to_string(),
                })?;
            let mut process = tokio::process::Command::new(&confined.program);
            process
                .args(&confined.args)
                .current_dir(tools.root())
                .env_clear()
                .env(
                    "PATH",
                    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
                )
                .env("HOME", &scratch)
                .env("TMPDIR", &scratch);
            for (key, value) in env {
                process.env(key, value);
            }
            let transport =
                TokioChildProcess::new(process).map_err(|error| connect_error(server, error))?;
            tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
                .await
                .map_err(|_| connect_error(server, "no answer to the handshake in time"))?
                .map_err(|error| connect_error(server, error))?
        }
        McpTransportSpec::Http { url, token } => {
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if let Some(token) = token {
                config = config.auth_header(token.expose());
            }
            let transport = StreamableHttpClientTransport::from_config(config);
            tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport))
                .await
                .map_err(|_| connect_error(server, "no answer to the handshake in time"))?
                .map_err(|error| connect_error(server, error))?
        }
    };
    let listed = client
        .list_all_tools()
        .await
        .map_err(|error| connect_error(server, error))?;
    let mut entries: Vec<Entry> = Vec::new();
    for tool in listed {
        let original = tool.name.to_string();
        let allowed = (spec.tools.is_empty() || spec.tools.contains(&original))
            && (node_allow.is_empty() || node_allow.contains(&original));
        let read_only = tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint)
            == Some(true);
        if !allowed || (!spec.writable && !read_only) {
            continue;
        }
        let exposed = exposed_name(server, &original);
        if entries.iter().any(|entry| entry.exposed == exposed) {
            return Err(McpError::Config {
                server: server.to_owned(),
                message: format!("two tools are called `{exposed}` once renamed"),
            });
        }
        let description = tool
            .description
            .as_deref()
            .or(tool.title.as_deref())
            .unwrap_or("")
            .to_owned();
        entries.push(Entry {
            spec: ToolSpec {
                name: exposed.clone(),
                description: format!("[{server}] {description}"),
                input_schema: Value::Object((*tool.input_schema).clone()),
            },
            read_only,
            exposed,
            original,
        });
    }
    entries.sort_by(|a, b| a.exposed.cmp(&b.exposed));
    Ok(McpProvider {
        server: spec.name.clone(),
        client,
        entries,
        timeout: spec.timeout,
    })
}

fn render(result: &rmcp::model::CallToolResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in &result.content {
        parts.push(match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Image(image) => {
                format!(
                    "[image {}, {} bytes of data left out]",
                    image.mime_type,
                    image.data.len()
                )
            }
            ContentBlock::Audio(audio) => {
                format!(
                    "[audio {}, {} bytes of data left out]",
                    audio.mime_type,
                    audio.data.len()
                )
            }
            ContentBlock::Resource(_) => "[embedded resource left out]".to_owned(),
            ContentBlock::ResourceLink(link) => format!("[resource {}]", link.uri),
            _ => "[content of a kind this harness does not read]".to_owned(),
        });
    }
    if parts.is_empty()
        && let Some(structured) = &result.structured_content
    {
        parts.push(structured.to_string());
    }
    parts.join("\n")
}

impl McpProvider {
    pub async fn fetch_resource(&self, uri: &str) -> Result<String, McpError> {
        let read = tokio::time::timeout(
            self.timeout,
            self.client
                .read_resource(rmcp::model::ReadResourceRequestParams::new(uri)),
        )
        .await
        .map_err(|_| connect_error(&self.server, "no answer to a resource read in time"))?
        .map_err(|error| connect_error(&self.server, error))?;
        let mut parts: Vec<String> = Vec::new();
        for content in read.contents {
            match content {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => {
                    parts.push(text)
                }
                _ => parts.push("[binary resource content left out]".to_owned()),
            }
        }
        Ok(parts.join("\n"))
    }

    pub async fn fetch_tool(&self, original: &str, args: &Value) -> Result<String, McpError> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.original == original)
            .ok_or_else(|| McpError::Config {
                server: self.server.clone(),
                message: format!("no tool `{original}` is available"),
            })?;
        if !entry.read_only {
            return Err(McpError::Config {
                server: self.server.clone(),
                message: format!(
                    "tool `{original}` may change things and cannot be used to fetch context"
                ),
            });
        }
        let reply = self
            .invoke(&entry.exposed, args)
            .await
            .map_err(|error| connect_error(&self.server, error))?;
        if reply.is_error {
            return Err(connect_error(&self.server, reply.text));
        }
        Ok(reply.text)
    }
}

#[async_trait]
impl ToolProvider for McpProvider {
    fn specs(&self) -> Vec<ToolSpec> {
        self.entries
            .iter()
            .map(|entry| entry.spec.clone())
            .collect()
    }

    fn replay(&self, name: &str) -> Replay {
        match self.entries.iter().find(|entry| entry.exposed == name) {
            Some(entry) if entry.read_only => Replay::Repeat,
            _ => Replay::Never,
        }
    }

    async fn invoke(&self, name: &str, input: &Value) -> Result<ProviderReply, ToolSetError> {
        let reply = |text: String, is_error: bool| Ok(ProviderReply { text, is_error });
        let Some(entry) = self.entries.iter().find(|entry| entry.exposed == name) else {
            return reply(format!("ERROR {name}: this server has no such tool"), true);
        };
        let mut params = CallToolRequestParams::new(entry.original.clone());
        params.arguments = match input {
            Value::Object(map) => Some(map.clone()),
            Value::Null => None,
            _ => {
                return reply(
                    format!("ERROR {name}: the input must be a JSON object"),
                    true,
                );
            }
        };
        match tokio::time::timeout(self.timeout, self.client.call_tool(params)).await {
            Err(_) => reply(
                format!(
                    "ERROR {name}: no answer from `{}` in {} seconds",
                    self.server,
                    self.timeout.as_secs()
                ),
                true,
            ),
            Ok(Err(error)) => reply(
                format!("ERROR {name}: server `{}` failed: {error}", self.server),
                true,
            ),
            Ok(Ok(result)) => reply(render(&result), result.is_error == Some(true)),
        }
    }
}

pub type SharedDirectory = Arc<dyn McpDirectory>;
