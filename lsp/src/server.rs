use crate::analysis::SemanticClass;
use crate::backend::Backend;
use serde_json::Value;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::request::{GotoImplementationParams, GotoImplementationResponse};
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

pub const TOKEN_TYPES: &[&str] = &[
    "keyword",
    "function",
    "type",
    "module",
    "parameter",
    "property",
    "variable",
    "const",
    "string",
    "number",
    "operator",
    "decorator",
    "method",
    "comment",
];

pub const TOKEN_MODIFIERS: &[&str] = &[];

#[derive(Clone)]
pub struct BackendServer {
    client: Client,
    state: Arc<Mutex<Backend>>,
}

impl BackendServer {
    pub fn service(client: Client) -> BackendServer {
        BackendServer {
            client,
            state: Arc::new(Mutex::new(Backend::new())),
        }
    }

    async fn publish_all(&self) {
        let batches = {
            let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
            backend.diagnostics()
        };
        for (uri, diagnostics) in batches {
            self.client
                .publish_diagnostics(uri, diagnostics, None)
                .await;
        }
    }

    async fn publish_one(&self, uri: Url) {
        let (diagnostics, version) = {
            let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let version = backend
                .open_documents()
                .find(|(candidate, _)| *candidate == &uri)
                .and_then(|(_, document)| document.version);
            (
                backend.diagnostics().remove(&uri).unwrap_or_default(),
                version,
            )
        };
        self.client
            .publish_diagnostics(uri, diagnostics, version)
            .await;
    }

    fn index_in_background(&self) {
        let state = Arc::clone(&self.state);
        let client = self.client.clone();
        tokio::spawn(async move {
            let batches = {
                let mut backend = state.lock().unwrap_or_else(|error| error.into_inner());
                backend.index_roots();
                backend.diagnostics()
            };
            for (uri, diagnostics) in batches {
                client.publish_diagnostics(uri, diagnostics, None).await;
            }
        });
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for BackendServer {
    #[allow(deprecated)]
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let mut roots = Vec::new();
        if let Some(folders) = &params.workspace_folders {
            for folder in folders {
                if let Ok(path) = folder.uri.to_file_path() {
                    roots.push(path);
                }
            }
        }
        if roots.is_empty() {
            if let Some(uri) = &params.root_uri {
                if let Ok(path) = uri.to_file_path() {
                    roots.push(path);
                }
            }
        }
        if roots.is_empty() {
            if let Some(path) = params.root_path.as_ref().map(PathBuf::from) {
                roots.push(path);
            }
        }
        {
            let mut backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
            backend.clear();
            for root in roots {
                backend.add_workspace_root_without_index(root);
            }
        }
        Ok(InitializeResult {
            capabilities: server_capabilities(),
            server_info: Some(ServerInfo {
                name: "HardScript Language Server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.index_in_background();
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let document = params.text_document;
        let uri = document.uri.clone();
        {
            let mut backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
            backend.open_document(document.uri, document.text, document.version);
        }
        self.publish_one(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri;
        let version = params.text_document.version;
        let Some(change) = params.content_changes.into_iter().next_back() else {
            return;
        };
        let accepted = {
            let mut backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
            backend.update_document(&uri, change.text, version)
        };
        if accepted {
            self.publish_one(uri).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(text) = params.text {
            {
                let mut backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
                backend.save_document(&params.text_document.uri, text);
            }
        }
        self.publish_one(params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        {
            let mut backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
            backend.close_document(&uri);
        }
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        {
            let mut backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
            for change in params.changes {
                backend.refresh_file(&change.uri);
            }
        }
        self.publish_all().await;
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        {
            let mut backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
            for folder in params.event.removed {
                if let Ok(path) = folder.uri.to_file_path() {
                    backend.remove_workspace_root(&path);
                }
            }
            for folder in params.event.added {
                if let Ok(path) = folder.uri.to_file_path() {
                    backend.add_workspace_root_without_index(path);
                }
            }
        }
        self.index_in_background();
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(Some(backend.completion(&params)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend.hover(
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        ))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend.definition(
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        ))
    }

    async fn goto_implementation(
        &self,
        params: GotoImplementationParams,
    ) -> Result<Option<GotoImplementationResponse>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let response = backend.definition(
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        );
        Ok(response.map(|response| match response {
            GotoDefinitionResponse::Scalar(location) => {
                GotoImplementationResponse::Scalar(location)
            }
            GotoDefinitionResponse::Array(locations) => {
                GotoImplementationResponse::Array(locations)
            }
            GotoDefinitionResponse::Link(locations) => GotoImplementationResponse::Link(locations),
        }))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(Some(backend.references(
            &params.text_document_position.text_document.uri,
            params.text_document_position.position,
            params.context.include_declaration,
        )))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend.prepare_rename(&params.text_document.uri, params.position))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match backend.rename(&params) {
            Ok(edit) => Ok(Some(edit)),
            Err(message) => Err(tower_lsp::jsonrpc::Error {
                code: tower_lsp::jsonrpc::ErrorCode::InvalidParams,
                message: Cow::Owned(message),
                data: None,
            }),
        }
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(Some(backend.document_highlights(
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        )))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend.document_symbols(&params.text_document.uri))
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(Some(backend.workspace_symbols(&params.query)))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend
            .semantic_tokens(&params.text_document.uri)
            .map(SemanticTokensResult::Tokens))
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend.format_document(&params.text_document.uri, &params.options))
    }

    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend.format_range(&params.text_document.uri, params.range, &params.options))
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(Some(backend.code_actions(&params)))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
        if params.command != "hardscript.explainError" {
            return Ok(None);
        }
        let code = params
            .arguments
            .iter()
            .find_map(|value| value.as_str())
            .or_else(|| {
                params
                    .arguments
                    .first()
                    .and_then(Value::as_object)
                    .and_then(|object| object.get("code"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default();
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend.explain(code).map(Value::String))
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let backend = self.state.lock().unwrap_or_else(|error| error.into_inner());
        Ok(backend.signature_help(
            &params.text_document_position_params.text_document.uri,
            params.text_document_position_params.position,
        ))
    }
}

pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        position_encoding: Some(PositionEncodingKind::UTF16),
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                will_save: Some(false),
                will_save_wait_until: Some(false),
                save: Some(TextDocumentSyncSaveOptions::Supported(true)),
            },
        )),
        selection_range_provider: None,
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        completion_provider: Some(CompletionOptions {
            resolve_provider: Some(false),
            trigger_characters: Some(vec![
                ".".to_string(),
                "\"".to_string(),
                "/".to_string(),
                ":".to_string(),
                " ".to_string(),
            ]),
            all_commit_characters: None,
            completion_item: None,
            work_done_progress_options: Default::default(),
        }),
        signature_help_provider: Some(SignatureHelpOptions {
            trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
            retrigger_characters: Some(vec![",".to_string()]),
            work_done_progress_options: Default::default(),
        }),
        declaration_provider: None,
        definition_provider: Some(OneOf::Left(true)),
        type_definition_provider: None,
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        references_provider: Some(OneOf::Left(true)),
        document_highlight_provider: Some(OneOf::Left(true)),
        document_symbol_provider: Some(OneOf::Left(true)),
        workspace_symbol_provider: Some(OneOf::Left(true)),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![CodeActionKind::QUICKFIX, CodeActionKind::REFACTOR]),
            resolve_provider: Some(false),
            work_done_progress_options: Default::default(),
        })),
        code_lens_provider: None,
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        document_on_type_formatting_provider: None,
        rename_provider: Some(OneOf::Right(RenameOptions {
            prepare_provider: Some(true),
            work_done_progress_options: Default::default(),
        })),
        document_link_provider: None,
        color_provider: None,
        folding_range_provider: None,
        execute_command_provider: Some(ExecuteCommandOptions {
            commands: vec!["hardscript.explainError".to_string()],
            work_done_progress_options: Default::default(),
        }),
        call_hierarchy_provider: None,
        semantic_tokens_provider: Some(SemanticTokensServerCapabilities::SemanticTokensOptions(
            SemanticTokensOptions {
                legend: SemanticTokensLegend {
                    token_types: TOKEN_TYPES
                        .iter()
                        .map(|value| SemanticTokenType::new(value))
                        .collect(),
                    token_modifiers: TOKEN_MODIFIERS
                        .iter()
                        .map(|value| SemanticTokenModifier::new(value))
                        .collect(),
                },
                range: Some(false),
                full: Some(SemanticTokensFullOptions::Bool(true)),
                work_done_progress_options: Default::default(),
            },
        )),
        moniker_provider: None,
        linked_editing_range_provider: None,
        inline_value_provider: None,
        inlay_hint_provider: None,
        diagnostic_provider: Some(DiagnosticServerCapabilities::Options(DiagnosticOptions {
            identifier: Some("hardscript".to_string()),
            inter_file_dependencies: true,
            workspace_diagnostics: false,
            work_done_progress_options: Default::default(),
        })),
        workspace: Some(WorkspaceServerCapabilities {
            workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                supported: Some(true),
                change_notifications: Some(OneOf::Left(true)),
            }),
            file_operations: None,
        }),
        experimental: Some(serde_json::json!({
            "hardscript": {
                "version": env!("CARGO_PKG_VERSION"),
                "semanticClasses": TOKEN_TYPES
            }
        })),
    }
}

pub fn semantic_class_count() -> usize {
    TOKEN_TYPES.len()
}

pub fn known_semantic_class(name: &str) -> Option<SemanticClass> {
    match name {
        "keyword" => Some(SemanticClass::Keyword),
        "function" => Some(SemanticClass::Function),
        "type" => Some(SemanticClass::Type),
        "module" => Some(SemanticClass::Module),
        "parameter" => Some(SemanticClass::Parameter),
        "property" => Some(SemanticClass::Property),
        "variable" => Some(SemanticClass::Variable),
        "const" => Some(SemanticClass::Constant),
        _ => None,
    }
}
