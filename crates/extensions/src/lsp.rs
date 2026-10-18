use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_lsp::lsp_types::notification::{PublishDiagnostics, ShowMessage};
use async_lsp::lsp_types::{
    ClientCapabilities, Diagnostic, DidOpenTextDocumentParams, DocumentSymbolParams,
    DocumentSymbolResponse, InitializeParams, InitializedParams, PartialResultParams, Position,
    ReferenceContext, ReferenceParams, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Url, WorkDoneProgressParams, WorkspaceFolder,
};
use async_lsp::router::Router;
use async_lsp::{LanguageServer, MainLoop};
use async_trait::async_trait;
use jensen_harness_core::sandbox::NetworkPolicy;
use jensen_harness_core::toolset::Confiner;
use thiserror::Error;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower::ServiceBuilder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspKind {
    Diagnostics,
    Symbols,
    References,
}

#[derive(Debug, Error)]
pub enum LspError {
    #[error("language server `{0}` is not configured")]
    Unknown(String),
    #[error("language server `{server}` failed: {message}")]
    Failed { server: String, message: String },
}

#[derive(Debug, Clone)]
pub struct LspServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub language: String,
    pub network: NetworkPolicy,
    pub timeout: Duration,
}

pub trait LspSpecs: Send + Sync {
    fn server(&self, name: &str) -> Result<LspServerSpec, LspError>;
}

pub struct LspRequest<'a> {
    pub server: &'a str,
    pub kind: LspKind,
    pub root: &'a Path,
    pub files: &'a [PathBuf],
    pub confiner: &'a Confiner,
}

#[async_trait]
pub trait LspDirectory: Send + Sync {
    async fn query(&self, request: LspRequest<'_>) -> Result<String, LspError>;
}

pub struct ConfiguredLsp {
    specs: Arc<dyn LspSpecs>,
}

impl ConfiguredLsp {
    #[must_use]
    pub fn new(specs: Arc<dyn LspSpecs>) -> Self {
        Self { specs }
    }
}

type Diagnostics = Arc<Mutex<BTreeMap<Url, Vec<Diagnostic>>>>;

struct State {
    diagnostics: Diagnostics,
}

struct Stop;

fn failed(server: &str, message: impl std::fmt::Display) -> LspError {
    LspError::Failed {
        server: server.to_owned(),
        message: message.to_string(),
    }
}

fn line(position: Position) -> u32 {
    position.line + 1
}

#[async_trait]
impl LspDirectory for ConfiguredLsp {
    async fn query(&self, request: LspRequest<'_>) -> Result<String, LspError> {
        let spec = self.specs.server(request.server)?;
        tokio::time::timeout(spec.timeout, run(&spec, &request))
            .await
            .map_err(|_| failed(&spec.name, "no answer in time"))?
    }
}

async fn run(spec: &LspServerSpec, request: &LspRequest<'_>) -> Result<String, LspError> {
    let name = spec.name.as_str();
    let (confined, scratch) = request
        .confiner
        .confine(&spec.command, &spec.args, spec.network, false)
        .map_err(|error| failed(name, error))?;
    let mut process = tokio::process::Command::new(&confined.program);
    process
        .args(&confined.args)
        .current_dir(request.root)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
        )
        .env("HOME", &scratch)
        .env("TMPDIR", &scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in &spec.env {
        process.env(key, value);
    }
    let mut child = process.spawn().map_err(|error| failed(name, error))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| failed(name, "no stdin"))?
        .compat_write();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failed(name, "no stdout"))?
        .compat();

    let diagnostics: Diagnostics = Arc::default();
    let shared = Arc::clone(&diagnostics);
    let (mainloop, mut server) = MainLoop::new_client(move |_server| {
        let mut router = Router::new(State {
            diagnostics: shared,
        });
        router
            .notification::<PublishDiagnostics>(|state, params| {
                if let Ok(mut map) = state.diagnostics.lock() {
                    map.insert(params.uri, params.diagnostics);
                }
                ControlFlow::Continue(())
            })
            .notification::<ShowMessage>(|_, _| ControlFlow::Continue(()))
            .event(|_, _: Stop| ControlFlow::Break(Ok(())));
        ServiceBuilder::new()
            .layer(async_lsp::panic::CatchUnwindLayer::default())
            .layer(async_lsp::concurrency::ConcurrencyLayer::default())
            .service(router)
    });
    let driver = tokio::spawn(async move {
        if let Err(error) = mainloop.run_buffered(stdout, stdin).await {
            tracing::debug!(%error, "the language server connection ended");
        }
    });

    let outcome = converse(spec, request, &mut server, &diagnostics).await;

    if server.shutdown(()).await.is_ok()
        && let Err(error) = server.exit(())
    {
        tracing::debug!(%error, "the language server did not take exit");
    }
    if let Err(error) = server.emit(Stop) {
        tracing::debug!(%error, "the language server loop was already closed");
    }
    driver.abort();
    outcome
}

async fn converse(
    spec: &LspServerSpec,
    request: &LspRequest<'_>,
    server: &mut async_lsp::ServerSocket,
    diagnostics: &Diagnostics,
) -> Result<String, LspError> {
    let name = spec.name.as_str();
    let root = request
        .root
        .canonicalize()
        .map_err(|error| failed(name, error))?;
    let root_uri =
        Url::from_file_path(&root).map_err(|()| failed(name, "the root is not a path"))?;
    server
        .initialize(InitializeParams {
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: root_uri,
                name: "root".to_owned(),
            }]),
            capabilities: ClientCapabilities::default(),
            ..InitializeParams::default()
        })
        .await
        .map_err(|error| failed(name, error))?;
    server
        .initialized(InitializedParams {})
        .map_err(|error| failed(name, error))?;

    let mut opened: Vec<(PathBuf, Url)> = Vec::new();
    for relative in request.files {
        let path = root.join(relative);
        let text = std::fs::read_to_string(&path).map_err(|error| failed(name, error))?;
        let uri = Url::from_file_path(&path).map_err(|()| failed(name, "not a file path"))?;
        server
            .did_open(DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: spec.language.clone(),
                    version: 0,
                    text,
                },
            })
            .map_err(|error| failed(name, error))?;
        opened.push((relative.clone(), uri));
    }

    let mut out: Vec<String> = Vec::new();
    match request.kind {
        LspKind::Diagnostics => {
            let waited = tokio::time::Instant::now();
            loop {
                let ready = diagnostics
                    .lock()
                    .map(|map| opened.iter().all(|(_, uri)| map.contains_key(uri)))
                    .unwrap_or(false);
                if ready || waited.elapsed() > Duration::from_secs(5) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let map = diagnostics.lock().map_err(|error| failed(name, error))?;
            for (relative, uri) in &opened {
                for item in map.get(uri).into_iter().flatten() {
                    out.push(format!(
                        "{}:{}:{} {:?} {}",
                        relative.display(),
                        line(item.range.start),
                        item.range.start.character + 1,
                        item.severity,
                        item.message.lines().next().unwrap_or_default()
                    ));
                }
            }
            if out.is_empty() {
                out.push("no diagnostics".to_owned());
            }
        }
        LspKind::Symbols | LspKind::References => {
            for (relative, uri) in &opened {
                let symbols = server
                    .document_symbol(DocumentSymbolParams {
                        text_document: TextDocumentIdentifier { uri: uri.clone() },
                        work_done_progress_params: WorkDoneProgressParams::default(),
                        partial_result_params: PartialResultParams::default(),
                    })
                    .await
                    .map_err(|error| failed(name, error))?;
                let found: Vec<(String, Position)> = match symbols {
                    Some(DocumentSymbolResponse::Nested(items)) => items
                        .into_iter()
                        .map(|item| (item.name, item.selection_range.start))
                        .collect(),
                    Some(DocumentSymbolResponse::Flat(items)) => items
                        .into_iter()
                        .map(|item| (item.name, item.location.range.start))
                        .collect(),
                    None => Vec::new(),
                };
                for (symbol, position) in found {
                    if request.kind == LspKind::Symbols {
                        out.push(format!(
                            "{}:{} {symbol}",
                            relative.display(),
                            line(position)
                        ));
                        continue;
                    }
                    let references = server
                        .references(ReferenceParams {
                            text_document_position: TextDocumentPositionParams {
                                text_document: TextDocumentIdentifier { uri: uri.clone() },
                                position,
                            },
                            work_done_progress_params: WorkDoneProgressParams::default(),
                            partial_result_params: PartialResultParams::default(),
                            context: ReferenceContext {
                                include_declaration: false,
                            },
                        })
                        .await
                        .map_err(|error| failed(name, error))?
                        .unwrap_or_default();
                    let places: Vec<String> = references
                        .iter()
                        .take(5)
                        .map(|place| {
                            let file = place
                                .uri
                                .to_file_path()
                                .ok()
                                .and_then(|path| {
                                    path.strip_prefix(&root).ok().map(Path::to_path_buf)
                                })
                                .unwrap_or_default();
                            format!("{}:{}", file.display(), line(place.range.start))
                        })
                        .collect();
                    out.push(format!(
                        "{symbol}: {} reference(s) {}",
                        references.len(),
                        places.join(", ")
                    ));
                }
            }
            if out.is_empty() {
                out.push("no symbols".to_owned());
            }
        }
    }
    Ok(out.join("\n"))
}
