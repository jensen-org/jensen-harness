use std::io::Write as _;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{ErrorData, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct Text {
    text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct Count {
    lines: usize,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct Target {
    path: String,
}

#[derive(Debug, Clone)]
struct Fixture {
    tool_router: ToolRouter<Self>,
}

fn log(line: &str) -> Result<(), ErrorData> {
    let Ok(path) = std::env::var("TESTKIT_LOG") else {
        return Ok(());
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
    writeln!(file, "{line}").map_err(|error| ErrorData::internal_error(error.to_string(), None))
}

async fn pause(variable: &str) {
    if let Some(seconds) = std::env::var(variable)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
    }
}

fn text(value: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(value.into())])
}

#[tool_router(router = tool_router)]
impl Fixture {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Echo the text back", annotations(read_only_hint = true))]
    async fn echo(&self, Parameters(input): Parameters<Text>) -> Result<CallToolResult, ErrorData> {
        log(&format!("echo:{}", input.text))?;
        pause("TESTKIT_ECHO_DELAY").await;
        Ok(text(input.text))
    }

    #[tool(description = "Return many lines", annotations(read_only_hint = true))]
    async fn big(&self, Parameters(input): Parameters<Count>) -> Result<CallToolResult, ErrorData> {
        Ok(text(
            (0..input.lines)
                .map(|n| format!("line {n}\n"))
                .collect::<String>(),
        ))
    }

    #[tool(description = "Draw a shape and remember it")]
    async fn draw(&self, Parameters(input): Parameters<Text>) -> Result<CallToolResult, ErrorData> {
        log(&format!("draw:{}", input.text))?;
        pause("TESTKIT_DRAW_DELAY").await;
        Ok(text(format!("drew {}", input.text)))
    }

    #[tool(description = "Never answers", annotations(read_only_hint = true))]
    async fn slow(&self) -> Result<CallToolResult, ErrorData> {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        Ok(text("late"))
    }

    #[tool(description = "Write a file wherever it is told")]
    async fn write_file(
        &self,
        Parameters(input): Parameters<Target>,
    ) -> Result<CallToolResult, ErrorData> {
        match std::fs::write(&input.path, "written") {
            Ok(()) => Ok(text(format!("wrote {}", input.path))),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(
                error.to_string(),
            )])),
        }
    }

    #[tool(
        description = "Read an environment variable",
        annotations(read_only_hint = true)
    )]
    async fn env(&self, Parameters(input): Parameters<Text>) -> Result<CallToolResult, ErrorData> {
        Ok(text(
            std::env::var(&input.text).unwrap_or_else(|_| "unset".to_owned()),
        ))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Fixture {
    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ListResourcesResult, ErrorData> {
        Ok(rmcp::model::ListResourcesResult::default())
    }

    async fn read_resource(
        &self,
        request: rmcp::model::ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::ReadResourceResponse, ErrorData> {
        if request.uri == "note://design" {
            Ok(
                rmcp::model::ReadResourceResult::new(vec![rmcp::model::ResourceContents::text(
                    "the design brief",
                    request.uri,
                )])
                .into(),
            )
        } else {
            Err(ErrorData::invalid_params(
                format!("no resource {}", request.uri),
                None,
            ))
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = Fixture::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
