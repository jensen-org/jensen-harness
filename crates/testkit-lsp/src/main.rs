use std::collections::BTreeMap;

use lsp_server::{Connection, Message, Notification, Response};
use lsp_types::notification::{DidOpenTextDocument, Notification as _, PublishDiagnostics};
use lsp_types::request::{DocumentSymbolRequest, References, Request as _};
use lsp_types::{
    Diagnostic, DiagnosticSeverity, DocumentSymbol, DocumentSymbolParams, DocumentSymbolResponse,
    InitializeParams, Location, OneOf, Position, PublishDiagnosticsParams, Range, ReferenceParams,
    ServerCapabilities, SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};

fn range(line: u32) -> Range {
    Range::new(Position::new(line, 0), Position::new(line, 1))
}

#[allow(deprecated)]
fn symbols(text: &str) -> Vec<DocumentSymbol> {
    text.lines()
        .enumerate()
        .filter_map(|(index, text)| {
            let name = text
                .strip_prefix("fn ")?
                .split('(')
                .next()?
                .trim()
                .to_owned();
            let line = u32::try_from(index).ok()?;
            Some(DocumentSymbol {
                name,
                detail: None,
                kind: SymbolKind::FUNCTION,
                tags: None,
                deprecated: None,
                range: range(line),
                selection_range: range(line),
                children: None,
            })
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error + Sync + Send>> {
    let (connection, threads) = Connection::stdio();
    let capabilities = serde_json::to_value(ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        document_symbol_provider: Some(OneOf::Left(true)),
        references_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    })?;
    let params = connection.initialize(capabilities)?;
    let _: InitializeParams = serde_json::from_value(params)?;
    let mut documents: BTreeMap<Url, String> = BTreeMap::new();
    for message in &connection.receiver {
        match message {
            Message::Request(request) => {
                if connection.handle_shutdown(&request)? {
                    break;
                }
                match request.method.as_str() {
                    DocumentSymbolRequest::METHOD => {
                        let params: DocumentSymbolParams = serde_json::from_value(request.params)?;
                        let text = documents
                            .get(&params.text_document.uri)
                            .cloned()
                            .unwrap_or_default();
                        let result = DocumentSymbolResponse::Nested(symbols(&text));
                        connection
                            .sender
                            .send(Message::Response(Response::new_ok(request.id, result)))?;
                    }
                    References::METHOD => {
                        let params: ReferenceParams = serde_json::from_value(request.params)?;
                        let uri = params.text_document_position.text_document.uri;
                        let result = vec![Location::new(uri, range(0))];
                        connection
                            .sender
                            .send(Message::Response(Response::new_ok(request.id, result)))?;
                    }
                    _ => {
                        connection.sender.send(Message::Response(Response::new_err(
                            request.id,
                            -32601,
                            "not implemented".to_owned(),
                        )))?;
                    }
                }
            }
            Message::Notification(notification) => {
                if notification.method == DidOpenTextDocument::METHOD {
                    let params: lsp_types::DidOpenTextDocumentParams =
                        serde_json::from_value(notification.params)?;
                    let uri = params.text_document.uri;
                    let text = params.text_document.text;
                    let diagnostics: Vec<Diagnostic> = text
                        .lines()
                        .enumerate()
                        .filter(|(_, line)| line.contains("TODO"))
                        .filter_map(|(index, _)| u32::try_from(index).ok())
                        .map(|line| Diagnostic {
                            range: range(line),
                            severity: Some(DiagnosticSeverity::WARNING),
                            message: "found a TODO".to_owned(),
                            ..Diagnostic::default()
                        })
                        .collect();
                    documents.insert(uri.clone(), text);
                    connection
                        .sender
                        .send(Message::Notification(Notification::new(
                            PublishDiagnostics::METHOD.to_owned(),
                            PublishDiagnosticsParams::new(uri, diagnostics, None),
                        )))?;
                }
            }
            Message::Response(_) => {}
        }
    }
    threads.join()?;
    Ok(())
}
