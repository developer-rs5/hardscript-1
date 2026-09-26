use crate::analysis::{
    diagnostic_severity, normalize_path, reachable_documents, DeclKind, DocumentAnalysis,
    Occurrence, SemanticClass, Symbol,
};
use crate::builtins;
use crate::text::{common_change_range, contains_position, fuzzy_score, is_identifier};
use hs_compiler::ast::{Module, Program};
use hs_compiler::error::Diag;
use hs_compiler::token::Sym;
use hs_compiler::{catalog, fmt, warn};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
    CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, Diagnostic,
    DiagnosticRelatedInformation, DiagnosticTag, DocumentSymbolResponse, FormattingOptions,
    GotoDefinitionResponse, Hover, HoverContents, Location, MarkupContent, MarkupKind,
    NumberOrString, Position, PrepareRenameResponse, Range, RenameParams, SemanticToken,
    SemanticTokens, SignatureHelp, SignatureInformation, SymbolInformation, SymbolKind, TextEdit,
    Url, WorkspaceEdit,
};
use walkdir::WalkDir;

const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;

type ProgramDiagnostics = (Vec<Diag>, HashMap<(usize, usize), Url>);

#[derive(Clone)]
pub struct IndexedDocument {
    analysis: DocumentAnalysis,
    pub(crate) version: Option<i32>,
    open: bool,
}

pub struct Backend {
    documents: BTreeMap<Url, IndexedDocument>,
    roots: Vec<PathBuf>,
}

impl Default for Backend {
    fn default() -> Backend {
        Backend::new()
    }
}

impl Backend {
    pub fn new() -> Backend {
        Backend {
            documents: BTreeMap::new(),
            roots: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.documents.clear();
        self.roots.clear();
    }

    pub fn add_workspace_root(&mut self, root: PathBuf) -> usize {
        self.add_workspace_root_without_index(root);
        self.index_roots()
    }

    pub fn add_workspace_root_without_index(&mut self, root: PathBuf) {
        let canonical = root.canonicalize().unwrap_or(root);
        if !self.roots.iter().any(|value| value == &canonical) {
            self.roots.push(canonical);
        }
        self.roots.sort();
    }

    pub fn remove_workspace_root(&mut self, root: &Path) {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        self.roots.retain(|value| value != &canonical);
        let removed_prefixes: Vec<String> = self
            .documents
            .keys()
            .filter_map(|uri| uri.to_file_path().ok())
            .filter(|path| path.starts_with(&canonical))
            .map(|path| normalize_path(&path))
            .collect();
        self.documents.retain(|uri, document| {
            let retained = uri
                .to_file_path()
                .ok()
                .map(|path| !removed_prefixes.contains(&normalize_path(&path)))
                .unwrap_or(true);
            retained || document.open
        });
    }

    pub fn index_roots(&mut self) -> usize {
        let mut indexed = 0usize;
        let roots = self.roots.clone();
        for root in roots {
            for path in source_files(&root) {
                if self
                    .documents
                    .get(&uri_for_path(&path))
                    .is_some_and(|document| document.open)
                {
                    continue;
                }
                if let Some(text) = read_source(&path) {
                    let uri = uri_for_path(&path);
                    self.documents.insert(
                        uri.clone(),
                        IndexedDocument {
                            analysis: DocumentAnalysis::new(uri.clone(), text),
                            version: None,
                            open: false,
                        },
                    );
                    indexed += 1;
                }
            }
        }
        indexed
    }

    pub fn open_document(&mut self, uri: Url, text: String, version: i32) {
        self.documents.insert(
            uri.clone(),
            IndexedDocument {
                analysis: DocumentAnalysis::new(uri, text),
                version: Some(version),
                open: true,
            },
        );
    }

    pub fn update_document(&mut self, uri: &Url, text: String, version: i32) -> bool {
        let current = self
            .documents
            .get(uri)
            .and_then(|document| document.version)
            .unwrap_or(i32::MIN);
        if version < current {
            return false;
        }
        self.documents.insert(
            uri.clone(),
            IndexedDocument {
                analysis: DocumentAnalysis::new(uri.clone(), text),
                version: Some(version),
                open: true,
            },
        );
        true
    }

    pub fn save_document(&mut self, uri: &Url, text: String) {
        let version = self
            .documents
            .get(uri)
            .and_then(|document| document.version)
            .or(Some(i32::MIN));
        self.documents.insert(
            uri.clone(),
            IndexedDocument {
                analysis: DocumentAnalysis::new(uri.clone(), text),
                version,
                open: true,
            },
        );
    }

    pub fn close_document(&mut self, uri: &Url) {
        let disk_text = uri.to_file_path().ok().and_then(|path| read_source(&path));
        let current_text = self
            .documents
            .get(uri)
            .map(|document| document.analysis.text.source().to_string());
        let text = disk_text.or(current_text);
        if let (Some(text), Some(document)) = (text, self.documents.get_mut(uri)) {
            document.analysis = DocumentAnalysis::new(uri.clone(), text);
            document.version = None;
            document.open = false;
        }
    }

    pub fn refresh_file(&mut self, uri: &Url) {
        if self
            .documents
            .get(uri)
            .is_some_and(|document| document.open)
        {
            return;
        }
        let path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => {
                self.documents.remove(uri);
                return;
            }
        };
        match read_source(&path) {
            Some(text) => {
                self.documents.insert(
                    uri.clone(),
                    IndexedDocument {
                        analysis: DocumentAnalysis::new(uri.clone(), text),
                        version: None,
                        open: false,
                    },
                );
            }
            None => {
                self.documents.remove(uri);
            }
        }
    }

    pub fn document_count(&self) -> usize {
        self.documents.len()
    }

    pub fn open_documents(&self) -> impl Iterator<Item = (&Url, &IndexedDocument)> {
        self.documents.iter().filter(|(_, document)| document.open)
    }

    pub fn document(&self, uri: &Url) -> Option<&DocumentAnalysis> {
        self.documents.get(uri).map(|document| &document.analysis)
    }

    pub fn diagnostics(&self) -> BTreeMap<Url, Vec<Diagnostic>> {
        let mut output: BTreeMap<Url, Vec<Diagnostic>> = self
            .open_documents()
            .map(|(uri, _)| (uri.clone(), Vec::new()))
            .collect();
        let path_map = self.path_map();
        for (uri, document) in self.open_documents() {
            for diagnostic in &document.analysis.parse_diagnostics {
                push_diagnostic_at(&mut output, uri, &document.analysis, diagnostic);
            }
        }
        let mut cache: HashMap<Vec<Url>, Option<ProgramDiagnostics>> = HashMap::new();
        for (uri, _) in self
            .open_documents()
            .map(|(uri, _)| (uri, ()))
            .collect::<Vec<_>>()
        {
            let reachable = reachable_documents(uri, &self.analyses(), &path_map);
            if !cache.contains_key(&reachable) {
                cache.insert(reachable.clone(), self.program_diagnostics(uri, &reachable));
            }
            let Some((diagnostics, span_locations)) = cache.get(&reachable).cloned().flatten()
            else {
                continue;
            };
            for diagnostic in diagnostics {
                let location = diagnostic
                    .location
                    .as_deref()
                    .and_then(|path| self.location_uri(path, &path_map))
                    .or_else(|| {
                        diagnostic.span.and_then(|span| {
                            span_locations
                                .get(&crate::analysis::span_key(span))
                                .cloned()
                        })
                    })
                    .or_else(|| Some(uri.clone()));
                if let Some(target) = location {
                    if let Some(target_document) = self.documents.get(&target) {
                        push_diagnostic_at(
                            &mut output,
                            &target,
                            &target_document.analysis,
                            &diagnostic,
                        );
                    }
                }
            }
        }
        for diagnostics in output.values_mut() {
            deduplicate_diagnostics(diagnostics);
            diagnostics.sort_by_key(|diagnostic| {
                (
                    diagnostic.range.start.line,
                    diagnostic.range.start.character,
                    diagnostic_code(diagnostic),
                )
            });
        }
        output
    }

    fn program_diagnostics(&self, uri: &Url, reachable: &[Url]) -> Option<ProgramDiagnostics> {
        if reachable.iter().any(|candidate| {
            self.documents
                .get(candidate)
                .is_some_and(|document| document.analysis.has_parse_errors())
        }) {
            return None;
        }
        // `models` is left empty like the build path's merge: it is a cache of
        // what the model statements already say, and `model_defs()` recovers it
        // from the merged statements.
        let mut merged = Program {
            stmts: Vec::new(),
            path: uri.to_string(),
            models: Vec::new(),
        };
        let mut files = Vec::new();
        let mut span_locations = HashMap::new();
        for candidate in reachable {
            let Some(document) = self.documents.get(candidate) else {
                continue;
            };
            let Some(program) = document.analysis.program.as_ref() else {
                continue;
            };
            merged.stmts.extend(program.stmts.clone());
            files.extend(std::iter::repeat(candidate.to_string()).take(program.stmts.len()));
            for span in crate::analysis::collect_span_locations(program).keys() {
                span_locations
                    .entry(*span)
                    .or_insert_with(|| candidate.clone());
            }
        }
        if merged.stmts.is_empty() {
            return None;
        }
        let file_names: Vec<Option<String>> = files
            .iter()
            .map(|uri| Some(uri.as_str().to_string()))
            .collect();
        let mut diagnostics = hs_compiler::typecheck::check_with(&merged, &file_names);
        diagnostics.extend(warn::analyze(&merged, &file_names));
        Some((diagnostics, span_locations))
    }

    pub fn completion(&self, params: &CompletionParams) -> CompletionResponse {
        let Some(analysis) = self.document(&params.text_document_position.text_document.uri) else {
            return CompletionResponse::Array(Vec::new());
        };
        let position = params.text_document_position.position;
        if analysis.text.is_import_string(position) {
            return self.import_completions(analysis, position);
        }
        if let Some(module) = member_module(analysis, position) {
            return CompletionResponse::Array(
                builtins::for_module(&module)
                    .filter(|builtin| score(builtin.name, analysis, position).is_some())
                    .map(|builtin| {
                        let mut item = CompletionItem {
                            label: builtin.name.to_string(),
                            kind: Some(CompletionItemKind::FUNCTION),
                            detail: Some(format!(
                                "{} → {}",
                                builtin.signature, builtin.return_type
                            )),
                            documentation: Some(markup(builtin.documentation)),
                            sort_text: Some(format!("{:06}", 30_000 - builtin.name.len() as i32)),
                            ..CompletionItem::default()
                        };
                        if !has_module_import(analysis, builtin.module) {
                            item.additional_text_edits =
                                Some(vec![import_edit(analysis, builtin.module, None)]);
                        }
                        item
                    })
                    .collect(),
            );
        }
        if let Some(model) = member_model(self, analysis, position) {
            let mut items = self
                .all_symbols()
                .filter(|symbol| {
                    symbol.kind == DeclKind::Field
                        && symbol
                            .container
                            .as_deref()
                            .is_some_and(|container| container == &format!("model:{model}"))
                })
                .filter(|symbol| score(&symbol.name, analysis, position).is_some())
                .map(symbol_completion)
                .collect::<Vec<_>>();
            items.sort_by(|left, right| left.sort_text.cmp(&right.sort_text));
            return CompletionResponse::Array(items);
        }
        let mut items = Vec::new();
        let offset = analysis.text.offset(position);
        for symbol in self
            .all_symbols()
            .filter(|symbol| symbol.local && contains_position(position, symbol.range))
        {
            if score(&symbol.name, analysis, position).is_some() {
                items.push(symbol_completion(symbol));
            }
        }
        for symbol in self.all_symbols() {
            if !matches!(
                symbol.kind,
                DeclKind::Function | DeclKind::Model | DeclKind::Variable | DeclKind::Constant
            ) || score(&symbol.name, analysis, position).is_none()
            {
                continue;
            }
            let mut item = symbol_completion(symbol);
            if symbol.uri != *self.current_uri(analysis) {
                if let Some((path, target)) = import_for_symbol(analysis, symbol) {
                    if !has_import(analysis, &path) {
                        item.additional_text_edits =
                            Some(vec![import_edit(analysis, &path, Some(target))]);
                    }
                }
            }
            items.push(item);
        }
        for (module, documentation) in builtins::MODULES {
            if score(module, analysis, position).is_none() {
                continue;
            }
            let mut item = CompletionItem {
                label: module.to_string(),
                kind: Some(CompletionItemKind::MODULE),
                detail: Some("HardScript standard-library module".to_string()),
                documentation: Some(markup(*documentation)),
                sort_text: Some(format!("{:06}", 20_000)),
                ..CompletionItem::default()
            };
            if !has_module_import(analysis, module) {
                item.additional_text_edits = Some(vec![import_edit(analysis, module, None)]);
            }
            items.push(item);
        }
        for symbol in self.all_symbols().filter(|symbol| {
            matches!(
                symbol.kind,
                DeclKind::Route | DeclKind::Middleware | DeclKind::Socket | DeclKind::Test
            ) && contains_position(position, symbol.range)
        }) {
            items.push(symbol_completion(symbol));
        }
        items.extend(language_snippets(analysis, position));
        let _ = offset;
        deduplicate_completion(&mut items);
        items.sort_by(|left, right| left.sort_text.cmp(&right.sort_text));
        CompletionResponse::Array(items.into_iter().take(200).collect())
    }

    pub fn hover(&self, uri: &Url, position: Position) -> Option<Hover> {
        let analysis = self.document(uri)?;
        if analysis.text.is_import_string(position) {
            let path = analysis
                .text
                .string_at(position)
                .map(|(_, _, value)| value)?;
            let target = self.resolve_import(analysis, &path)?;
            let value = format!(
                "```hardscript\nbring {:?}\n```\n\nLocal module import.\n\nResolves to `{}`",
                path,
                target.to_file_path().ok()?.display()
            );
            let range = analysis
                .text
                .string_at(position)
                .map(|(start, end, _)| analysis.text.text_range(start, end))?;
            return Some(Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: Some(range),
            });
        }
        let occurrence = self.canonical_occurrence(analysis, analysis.occurrence_at(position)?);
        let markdown = if occurrence.kind == DeclKind::Import {
            let target = self.resolve_import(analysis, &occurrence.name)?;
            format!(
                "```hardscript\n{}\n```\n\nLocal module import.\n\nResolves to `{}`",
                occurrence.detail,
                target.to_file_path().ok()?.display()
            )
        } else if let Some(symbol) = self.symbol_for_occurrence(&occurrence) {
            symbol_hover(symbol)
        } else if let Some(builtin) = occurrence
            .id
            .strip_prefix("builtin:")
            .and_then(|value| value.split_once(':'))
            .and_then(|(module, name)| builtins::find(module, name))
        {
            format!(
                "```hardscript\n{} → {}\n```\n\n{}",
                builtin.signature, builtin.return_type, builtin.documentation
            )
        } else {
            format!("```hardscript\n{}\n```", occurrence.detail)
        };
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: markdown,
            }),
            range: Some(occurrence.range),
        })
    }

    pub fn definition(&self, uri: &Url, position: Position) -> Option<GotoDefinitionResponse> {
        let analysis = self.document(uri)?;
        if analysis.text.is_import_string(position) {
            let path = analysis
                .text
                .string_at(position)
                .map(|(_, _, value)| value)?;
            return self.resolve_import_path(analysis, &path).map(|target| {
                GotoDefinitionResponse::Scalar(Location::new(
                    target,
                    Range::new(Position::new(0, 0), Position::new(0, 0)),
                ))
            });
        }
        let occurrence = self.canonical_occurrence(analysis, analysis.occurrence_at(position)?);
        if occurrence.kind == DeclKind::Import {
            let target = self.resolve_import(analysis, &occurrence.name)?;
            return Some(GotoDefinitionResponse::Scalar(Location::new(
                target,
                Range::new(Position::new(0, 0), Position::new(0, 0)),
            )));
        }
        let locations = self.definitions_for(&occurrence);
        if locations.is_empty() {
            None
        } else if locations.len() == 1 {
            Some(GotoDefinitionResponse::Scalar(locations[0].clone()))
        } else {
            Some(GotoDefinitionResponse::Array(locations))
        }
    }

    pub fn references(
        &self,
        uri: &Url,
        position: Position,
        include_declaration: bool,
    ) -> Vec<Location> {
        let Some(analysis) = self.document(uri) else {
            return Vec::new();
        };
        let occurrence = match analysis.occurrence_at(position) {
            Some(occurrence) => self.canonical_occurrence(analysis, occurrence),
            None => return Vec::new(),
        };
        if matches!(
            occurrence.kind,
            DeclKind::Builtin | DeclKind::Module | DeclKind::Property
        ) {
            return Vec::new();
        }
        let mut locations = Vec::new();
        for document in self.documents.values() {
            for candidate in &document.analysis.occurrences {
                if !reference_matches(candidate, &occurrence) {
                    continue;
                }
                if !include_declaration
                    && candidate.range == occurrence.range
                    && document
                        .analysis
                        .symbol_at(candidate.range.start)
                        .is_some_and(|symbol| symbol.selection_range == candidate.range)
                {
                    continue;
                }
                locations.push(Location::new(
                    document.analysis.uri.clone(),
                    candidate.range,
                ));
            }
        }
        deduplicate_locations(&mut locations);
        locations
    }

    pub fn prepare_rename(&self, uri: &Url, position: Position) -> Option<PrepareRenameResponse> {
        let analysis = self.document(uri)?;
        let occurrence = self.canonical_occurrence(analysis, analysis.occurrence_at(position)?);
        if matches!(
            occurrence.kind,
            DeclKind::Builtin
                | DeclKind::Module
                | DeclKind::Import
                | DeclKind::Route
                | DeclKind::Socket
                | DeclKind::Test
                | DeclKind::Property
                | DeclKind::Middleware
        ) || occurrence.id.starts_with("call:")
            || self.rename_is_ambiguous(&occurrence)
        {
            return None;
        }
        Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: occurrence.range,
            placeholder: occurrence.name.clone(),
        })
    }

    pub fn rename(&self, params: &RenameParams) -> Result<WorkspaceEdit, String> {
        let uri = &params.text_document_position.text_document.uri;
        let position = params.text_document_position.position;
        if !is_identifier(&params.new_name) {
            return Err("HardScript symbols must start with a letter or underscore and contain only letters, digits, or underscores".to_string());
        }
        if self.prepare_rename(uri, position).is_none() {
            return Err("the symbol cannot be renamed safely at this location".to_string());
        }
        let analysis = self
            .document(uri)
            .ok_or_else(|| "unknown document".to_string())?;
        let occurrence = analysis
            .occurrence_at(position)
            .ok_or_else(|| "no symbol at this position".to_string())?;
        let locations = self.references(uri, position, true);
        if locations.is_empty() {
            return Err("no editable references were found".to_string());
        }
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for location in locations {
            let Some(document) = self.documents.get(&location.uri) else {
                continue;
            };
            if document.analysis.has_parse_errors() {
                return Err("rename is blocked by syntax errors in an affected file".to_string());
            }
            changes.entry(location.uri).or_default().push(TextEdit {
                range: location.range,
                new_text: params.new_name.clone(),
            });
        }
        let _ = occurrence;
        for edits in changes.values_mut() {
            edits.sort_by_key(|edit| (edit.range.start.line, edit.range.start.character));
            edits.dedup_by(|left, right| left.range == right.range);
        }
        Ok(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        })
    }

    #[allow(deprecated)]
    pub fn document_symbols(&self, uri: &Url) -> Option<DocumentSymbolResponse> {
        let analysis = self.document(uri)?;
        let symbols = analysis
            .symbols
            .iter()
            .filter(|symbol| !symbol.local)
            .filter(|symbol| !matches!(symbol.kind, DeclKind::Module | DeclKind::Import))
            .map(|symbol| {
                let mut information = SymbolInformation {
                    name: symbol.name.clone(),
                    kind: lsp_symbol_kind(symbol.kind),
                    tags: None,
                    deprecated: None,
                    location: Location::new(symbol.uri.clone(), symbol.range),
                    container_name: None,
                };
                if symbol.kind == DeclKind::Field {
                    information.container_name = symbol
                        .container
                        .as_ref()
                        .and_then(|value| value.strip_prefix("model:"))
                        .map(str::to_string);
                }
                information
            })
            .collect();
        Some(DocumentSymbolResponse::Flat(symbols))
    }

    #[allow(deprecated)]
    pub fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        self.all_symbols()
            .filter(|symbol| !symbol.local)
            .filter(|symbol| {
                symbol.kind != DeclKind::Import
                    && (query.is_empty() || fuzzy_score(&symbol.name, query).is_some())
            })
            .map(|symbol| SymbolInformation {
                name: symbol.name.clone(),
                kind: lsp_symbol_kind(symbol.kind),
                tags: None,
                deprecated: None,
                location: Location::new(symbol.uri.clone(), symbol.range),
                container_name: symbol
                    .container
                    .as_ref()
                    .and_then(|value| value.split_once(':').map(|(_, name)| name.to_string())),
            })
            .collect()
    }

    pub fn semantic_tokens(&self, uri: &Url) -> Option<SemanticTokens> {
        let analysis = self.document(uri)?;
        let mut entries = analysis
            .semantic
            .iter()
            .flat_map(|semantic| {
                split_semantic_range(analysis, semantic.range)
                    .into_iter()
                    .map(move |range| (range, semantic_type_index(semantic.class)))
            })
            .filter(|(range, _)| range.start < range.end)
            .collect::<Vec<_>>();
        entries.sort_by_key(|(range, class_index)| (range.start, *class_index));
        let mut data = Vec::with_capacity(entries.len());
        let mut previous = Position::new(0, 0);
        for (range, class_index) in entries {
            let delta_line = range.start.line.saturating_sub(previous.line);
            let delta_start = if delta_line == 0 {
                range.start.character.saturating_sub(previous.character)
            } else {
                range.start.character
            };
            data.push(SemanticToken {
                delta_line,
                delta_start,
                length: range.end.character.saturating_sub(range.start.character),
                token_type: class_index,
                token_modifiers_bitset: 0,
            });
            previous = range.start;
        }
        Some(SemanticTokens {
            result_id: None,
            data,
        })
    }

    pub fn format_document(&self, uri: &Url, options: &FormattingOptions) -> Option<Vec<TextEdit>> {
        let analysis = self.document(uri)?;
        if analysis.has_parse_errors() {
            return None;
        }
        let program = analysis.program.as_ref()?;
        let source = analysis.text.source();
        if source.trim().is_empty() {
            return Some(Vec::new());
        }
        let formatted = fmt::format(program);
        let _ = options;
        if source == formatted {
            return Some(Vec::new());
        }
        Some(vec![TextEdit {
            range: analysis.text.full_range(),
            new_text: formatted,
        }])
    }

    pub fn format_range(
        &self,
        uri: &Url,
        range: Range,
        options: &FormattingOptions,
    ) -> Option<Vec<TextEdit>> {
        let analysis = self.document(uri)?;
        if analysis.has_parse_errors() {
            return None;
        }
        let formatted = fmt::format(analysis.program.as_ref()?);
        let source = analysis.text.source();
        let (start, old_end, new_end) = common_change_range(source, &formatted)?;
        let edit_range = analysis.text.text_range(start, old_end);
        if edit_range.start < range.start || edit_range.end > range.end {
            return None;
        }
        let _ = options;
        Some(vec![TextEdit {
            range: edit_range,
            new_text: formatted[start.min(formatted.len())..new_end].to_string(),
        }])
    }

    pub fn code_actions(&self, params: &CodeActionParams) -> CodeActionResponse {
        let Some(analysis) = self.document(&params.text_document.uri) else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        for diagnostic in &params.context.diagnostics {
            let code = diagnostic_code(diagnostic);
            if code == catalog::format(catalog::W_UNUSED_VARIABLE)
                && diagnostic.message.contains("local value")
            {
                if let Some(symbol) = analysis.symbol_at(diagnostic.range.start) {
                    let end = local_statement_end(analysis, symbol);
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!("Remove unused variable `{}`", symbol.name),
                        kind: Some(CodeActionKind::QUICKFIX),
                        diagnostics: Some(vec![diagnostic.clone()]),
                        edit: Some(WorkspaceEdit {
                            changes: Some(HashMap::from([(
                                params.text_document.uri.clone(),
                                vec![TextEdit {
                                    range: Range::new(diagnostic.range.start, end),
                                    new_text: String::new(),
                                }],
                            )])),
                            document_changes: None,
                            change_annotations: None,
                        }),
                        command: None,
                        is_preferred: Some(true),
                        disabled: None,
                        data: None,
                    }));
                }
            }
            let name = undefined_name(&diagnostic.message);
            if let Some(name) = name {
                for symbol in self.all_symbols().filter(|symbol| {
                    symbol.name == name
                        && matches!(
                            symbol.kind,
                            DeclKind::Function
                                | DeclKind::Model
                                | DeclKind::Variable
                                | DeclKind::Constant
                        )
                }) {
                    if let Some((path, target)) = import_for_symbol(analysis, symbol) {
                        if !has_import(analysis, &path) {
                            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                                title: format!("Import `{}` from {path:?}", symbol.name),
                                kind: Some(CodeActionKind::QUICKFIX),
                                diagnostics: Some(vec![diagnostic.clone()]),
                                edit: Some(WorkspaceEdit {
                                    changes: Some(HashMap::from([(
                                        params.text_document.uri.clone(),
                                        vec![import_edit(analysis, &path, Some(target))],
                                    )])),
                                    document_changes: None,
                                    change_annotations: None,
                                }),
                                command: None,
                                is_preferred: Some(true),
                                disabled: None,
                                data: None,
                            }));
                        }
                    }
                }
            }
            if code.starts_with("HS") {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: format!("Explain {code}"),
                    kind: Some(CodeActionKind::QUICKFIX),
                    diagnostics: Some(vec![diagnostic.clone()]),
                    command: Some(tower_lsp::lsp_types::Command {
                        title: format!("Explain {code}"),
                        command: "hardscript.explainError".to_string(),
                        arguments: Some(vec![serde_json::Value::String(code.clone())]),
                    }),
                    ..CodeAction::default()
                }));
            }
        }
        deduplicate_actions(&mut actions);
        actions
    }

    pub fn explain(&self, code: &str) -> Option<String> {
        let number = code
            .strip_prefix("HS")
            .or_else(|| code.strip_prefix("hs"))?
            .parse::<u16>()
            .ok()?;
        let definition = catalog::lookup(number)?;
        Some(format!(
            "### {} — {}\n\n{}\n\n**Common causes**\n{}\n\n**Fixes**\n{}\n\n```hardscript\n{}\n```",
            catalog::format(definition.number),
            definition.name,
            definition.meaning,
            bullets(definition.causes),
            bullets(definition.fixes),
            definition.example
        ))
    }

    pub fn signature_help(&self, uri: &Url, position: Position) -> Option<SignatureHelp> {
        let analysis = self.document(uri)?;
        let cursor = analysis.text.offset(position);
        let mut nesting = 0i32;
        let mut open = None;
        for (index, token) in analysis.tokens.iter().enumerate().rev() {
            if analysis.text.token_offset(token.span) >= cursor {
                continue;
            }
            match token.tok {
                hs_compiler::token::Tok::Sym(Sym::RParen | Sym::RBracket | Sym::RBrace) => {
                    nesting += 1;
                }
                hs_compiler::token::Tok::Sym(Sym::LBracket | Sym::LBrace) => {
                    nesting = (nesting - 1).max(0);
                }
                hs_compiler::token::Tok::Sym(Sym::LParen) => {
                    if nesting == 0 {
                        open = Some(index);
                        break;
                    }
                    nesting -= 1;
                }
                _ => {}
            }
        }
        let open = open?;
        let name_index = open.checked_sub(1)?;
        let name = match &analysis.tokens[name_index].tok {
            hs_compiler::token::Tok::Ident(name) => name.as_str(),
            _ => return None,
        };
        let mut active_parameter = 0u32;
        let mut nested = 0i32;
        for token in analysis.tokens.iter().skip(open + 1) {
            if analysis.text.token_offset(token.span) >= cursor {
                break;
            }
            match token.tok {
                hs_compiler::token::Tok::Sym(Sym::LParen | Sym::LBracket | Sym::LBrace) => {
                    nested += 1;
                }
                hs_compiler::token::Tok::Sym(Sym::RParen | Sym::RBracket | Sym::RBrace) => {
                    nested = (nested - 1).max(0);
                }
                hs_compiler::token::Tok::Sym(Sym::Comma) if nested == 0 => {
                    active_parameter += 1;
                }
                _ => {}
            }
        }
        let (label, documentation, parameters) = if let Some(symbol) = self
            .all_symbols()
            .find(|symbol| symbol.kind == DeclKind::Function && symbol.name == name)
        {
            let parameters = signature_parameters(&symbol.detail);
            (
                symbol.detail.clone(),
                symbol.documentation.clone(),
                parameters,
            )
        } else if name_index >= 2
            && matches!(
                analysis.tokens[name_index - 1].tok,
                hs_compiler::token::Tok::Sym(Sym::Dot)
            )
            && matches!(
                &analysis.tokens[name_index - 2].tok,
                hs_compiler::token::Tok::Ident(module)
                    if Module::NAMES.contains(&module.as_str())
            )
        {
            let module = match &analysis.tokens[name_index - 2].tok {
                hs_compiler::token::Tok::Ident(module) => module.as_str(),
                _ => return None,
            };
            let builtin = builtins::find(module, name)?;
            let parameters = signature_parameters(builtin.signature);
            (
                builtin.signature.to_string(),
                builtin.documentation.to_string(),
                parameters,
            )
        } else {
            return None;
        };
        let parameters = parameters
            .into_iter()
            .map(|label| tower_lsp::lsp_types::ParameterInformation {
                label: tower_lsp::lsp_types::ParameterLabel::Simple(label),
                documentation: None,
            })
            .collect::<Vec<_>>();
        Some(SignatureHelp {
            signatures: vec![SignatureInformation {
                label,
                documentation: if documentation.is_empty() {
                    None
                } else {
                    Some(markup(documentation))
                },
                parameters: if parameters.is_empty() {
                    None
                } else {
                    Some(parameters)
                },
                active_parameter: Some(active_parameter),
            }],
            active_signature: Some(0),
            active_parameter: Some(active_parameter),
        })
    }

    pub fn document_highlights(
        &self,
        uri: &Url,
        position: Position,
    ) -> Vec<tower_lsp::lsp_types::DocumentHighlight> {
        let Some(analysis) = self.document(uri) else {
            return Vec::new();
        };
        let occurrence = match analysis.occurrence_at(position) {
            Some(occurrence) => self.canonical_occurrence(analysis, occurrence),
            None => return Vec::new(),
        };
        analysis
            .occurrences
            .iter()
            .filter(|candidate| candidate.id == occurrence.id)
            .map(|candidate| tower_lsp::lsp_types::DocumentHighlight {
                range: candidate.range,
                kind: Some(if candidate.range == occurrence.range {
                    tower_lsp::lsp_types::DocumentHighlightKind::WRITE
                } else {
                    tower_lsp::lsp_types::DocumentHighlightKind::READ
                }),
            })
            .collect()
    }

    pub fn completion_latency_sample(&self, uri: &Url, position: Position) -> Option<Duration> {
        let start = Instant::now();
        let params = CompletionParams {
            text_document_position: tower_lsp::lsp_types::TextDocumentPositionParams {
                text_document: tower_lsp::lsp_types::TextDocumentIdentifier { uri: uri.clone() },
                position,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
            context: None,
        };
        let _ = self.completion(&params);
        self.document(uri).map(|_| start.elapsed())
    }

    pub fn hover_latency_sample(&self, uri: &Url, position: Position) -> Option<Duration> {
        let start = Instant::now();
        let _ = self.hover(uri, position);
        self.document(uri).map(|_| start.elapsed())
    }

    pub fn rename_latency_sample(&self, uri: &Url, position: Position) -> Option<Duration> {
        let start = Instant::now();
        let _ = self.references(uri, position, true);
        self.document(uri).map(|_| start.elapsed())
    }

    fn analyses(&self) -> HashMap<Url, DocumentAnalysis> {
        self.documents
            .iter()
            .map(|(uri, document)| (uri.clone(), document.analysis.clone()))
            .collect()
    }

    fn path_map(&self) -> HashMap<String, Url> {
        self.documents
            .keys()
            .filter_map(|uri| {
                let path = uri.to_file_path().ok()?;
                Some((normalize_path(&path), uri.clone()))
            })
            .collect()
    }

    fn location_uri(&self, location: &str, paths: &HashMap<String, Url>) -> Option<Url> {
        if let Ok(uri) = Url::parse(location) {
            if uri.scheme() == "file" {
                return Some(uri);
            }
        }
        let path = PathBuf::from(location);
        if path.is_absolute() {
            return Url::from_file_path(path).ok();
        }
        paths.get(&normalize_path(&path)).cloned()
    }

    fn all_symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.documents
            .values()
            .flat_map(|document| document.analysis.symbols.iter())
    }

    fn current_uri<'a>(&self, analysis: &'a DocumentAnalysis) -> &'a Url {
        &analysis.uri
    }

    fn canonical_occurrence(
        &self,
        analysis: &DocumentAnalysis,
        occurrence: &Occurrence,
    ) -> Occurrence {
        if self.all_symbols().any(|symbol| symbol.id == occurrence.id) {
            return occurrence.clone();
        }
        if occurrence.local
            || matches!(
                occurrence.kind,
                DeclKind::Builtin | DeclKind::Module | DeclKind::Import
            )
        {
            return occurrence.clone();
        }
        let mut candidates = self
            .all_symbols()
            .filter(|symbol| symbol.name == occurrence.name)
            .filter(|symbol| match occurrence.kind {
                DeclKind::Function => symbol.kind == DeclKind::Function,
                DeclKind::Variable | DeclKind::Constant => {
                    matches!(symbol.kind, DeclKind::Variable | DeclKind::Constant)
                }
                _ => false,
            })
            .collect::<Vec<_>>();
        if candidates.len() > 1 {
            let imported = candidates
                .iter()
                .filter(|symbol| self.symbol_is_imported(analysis, symbol))
                .copied()
                .collect::<Vec<_>>();
            if !imported.is_empty() {
                candidates = imported;
            }
        }
        let Some(symbol) = candidates.into_iter().next() else {
            return occurrence.clone();
        };
        Occurrence {
            id: symbol.id.clone(),
            name: symbol.name.clone(),
            detail: symbol.detail.clone(),
            documentation: symbol.documentation.clone(),
            kind: symbol.kind,
            uri: symbol.uri.clone(),
            range: occurrence.range,
            container: symbol.container.clone(),
            local: false,
        }
    }

    fn symbol_is_imported(&self, analysis: &DocumentAnalysis, symbol: &Symbol) -> bool {
        symbol.uri != analysis.uri
            && analysis.imports.iter().any(|import| {
                self.resolve_import_path(analysis, &import.path).as_ref() == Some(&symbol.uri)
            })
    }

    fn symbol_for_occurrence(&self, occurrence: &crate::analysis::Occurrence) -> Option<&Symbol> {
        self.all_symbols().find(|symbol| symbol.id == occurrence.id)
    }

    fn definitions_for(&self, occurrence: &crate::analysis::Occurrence) -> Vec<Location> {
        self.all_symbols()
            .filter(|symbol| symbol.id == occurrence.id)
            .map(|symbol| Location::new(symbol.uri.clone(), symbol.selection_range))
            .collect()
    }

    fn rename_is_ambiguous(&self, occurrence: &crate::analysis::Occurrence) -> bool {
        if occurrence.local {
            return self
                .all_symbols()
                .filter(|symbol| {
                    symbol.local
                        && symbol.name == occurrence.name
                        && symbol.container == occurrence.container
                })
                .count()
                > 1;
        }
        self.all_symbols()
            .filter(|symbol| symbol.id == occurrence.id)
            .count()
            > 1
    }

    fn resolve_import(&self, analysis: &DocumentAnalysis, path: &str) -> Option<Url> {
        self.resolve_import_path(analysis, path)
    }

    fn resolve_import_path(&self, analysis: &DocumentAnalysis, path: &str) -> Option<Url> {
        let current = self.current_uri(analysis).to_file_path().ok()?;
        let parent = current.parent()?;
        let candidate = if path.starts_with('.') {
            parent.join(path)
        } else {
            self.roots
                .iter()
                .find(|root| current.starts_with(root))
                .cloned()
                .unwrap_or_else(|| parent.to_path_buf())
                .join(path)
        };
        let candidates = [
            candidate.clone(),
            candidate.with_extension("hard"),
            candidate.join("main.hard"),
        ];
        candidates
            .into_iter()
            .filter_map(|path| Url::from_file_path(path).ok())
            .find(|uri| self.documents.contains_key(uri))
    }

    fn import_completions(
        &self,
        analysis: &DocumentAnalysis,
        position: Position,
    ) -> CompletionResponse {
        let (start, end, value) =
            analysis
                .text
                .string_at(position)
                .unwrap_or((0, 0, String::new()));
        let cursor = analysis.text.offset(position).min(end.max(start));
        let prefix = analysis
            .text
            .source()
            .get(start + 1..cursor)
            .unwrap_or(&value)
            .to_string();
        let current = self.current_uri(analysis);
        let current_path = current.to_file_path().unwrap_or_default();
        let mut items = Vec::new();
        for target in self.documents.keys() {
            if target == current {
                continue;
            }
            let target_path = match target.to_file_path() {
                Ok(path) => path,
                Err(_) => continue,
            };
            let import = relative_import(&current_path, &target_path);
            if fuzzy_score(&import, &prefix).is_none() {
                continue;
            }
            items.push(CompletionItem {
                label: import.clone(),
                kind: Some(CompletionItemKind::FILE),
                detail: Some("HardScript module".to_string()),
                filter_text: Some(import.clone()),
                sort_text: Some(format!("{:06}", 40_000 - import.len() as i32)),
                text_edit: Some(tower_lsp::lsp_types::CompletionTextEdit::Edit(TextEdit {
                    range: analysis.text.text_range(start + 1, cursor),
                    new_text: import,
                })),
                ..CompletionItem::default()
            });
        }
        items.sort_by(|left, right| left.sort_text.cmp(&right.sort_text));
        CompletionResponse::Array(items)
    }
}

fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .chars()
                .any(|value| value == '\0')
                && !matches!(
                    entry.file_name().to_string_lossy().as_ref(),
                    ".git" | "target" | "node_modules"
                )
        })
    {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|value| value.to_str()) != Some("hard")
        {
            continue;
        }
        if entry
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(u64::MAX)
            <= MAX_DOCUMENT_BYTES
        {
            paths.push(entry.into_path());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn read_source(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_DOCUMENT_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

fn uri_for_path(path: &Path) -> Url {
    Url::from_file_path(path).unwrap_or_else(|_| Url::parse("hardscript:///unknown").unwrap())
}

fn markup(value: impl Into<String>) -> tower_lsp::lsp_types::Documentation {
    tower_lsp::lsp_types::Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value: value.into(),
    })
}

fn lsp_symbol_kind(kind: DeclKind) -> SymbolKind {
    match kind {
        DeclKind::Function | DeclKind::Builtin => SymbolKind::FUNCTION,
        DeclKind::Model => SymbolKind::CLASS,
        DeclKind::Field | DeclKind::Property => SymbolKind::FIELD,
        DeclKind::Variable | DeclKind::Parameter => SymbolKind::VARIABLE,
        DeclKind::Constant => SymbolKind::CONSTANT,
        DeclKind::Route => SymbolKind::METHOD,
        DeclKind::Middleware => SymbolKind::OBJECT,
        DeclKind::Socket => SymbolKind::EVENT,
        DeclKind::Test => SymbolKind::METHOD,
        DeclKind::Module | DeclKind::Import => SymbolKind::MODULE,
    }
}

fn score(candidate: &str, analysis: &DocumentAnalysis, position: Position) -> Option<i32> {
    let Some(word) = analysis.text.word_before(position) else {
        return Some(0);
    };
    fuzzy_score(candidate, &word.text)
}

fn symbol_completion(symbol: &Symbol) -> CompletionItem {
    CompletionItem {
        label: symbol.name.clone(),
        kind: Some(match symbol.kind {
            DeclKind::Function | DeclKind::Builtin => CompletionItemKind::FUNCTION,
            DeclKind::Model => CompletionItemKind::STRUCT,
            DeclKind::Field | DeclKind::Property => CompletionItemKind::FIELD,
            DeclKind::Variable | DeclKind::Parameter => CompletionItemKind::VARIABLE,
            DeclKind::Constant => CompletionItemKind::CONSTANT,
            DeclKind::Route => CompletionItemKind::METHOD,
            DeclKind::Middleware => CompletionItemKind::MODULE,
            DeclKind::Socket => CompletionItemKind::EVENT,
            DeclKind::Test => CompletionItemKind::FUNCTION,
            DeclKind::Module | DeclKind::Import => CompletionItemKind::MODULE,
        }),
        detail: Some(symbol.detail.clone()),
        documentation: if symbol.documentation.is_empty() {
            None
        } else {
            Some(markup(symbol.documentation.clone()))
        },
        sort_text: Some(format!(
            "{:06}",
            fuzzy_score(&symbol.name, &symbol.name).unwrap_or_default()
        )),
        filter_text: Some(symbol.name.clone()),
        ..CompletionItem::default()
    }
}

fn has_module_import(analysis: &DocumentAnalysis, module: &str) -> bool {
    analysis
        .symbols
        .iter()
        .any(|symbol| symbol.kind == DeclKind::Module && symbol.name == module)
}

fn has_import(analysis: &DocumentAnalysis, path: &str) -> bool {
    analysis
        .imports
        .iter()
        .any(|import| normalize_import_path(&import.path) == normalize_import_path(path))
}

fn normalize_import_path(path: &str) -> String {
    let without_extension = path.strip_suffix(".hard").unwrap_or(path);
    let mut normalized = without_extension.replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }
    normalized
}

fn import_edit(analysis: &DocumentAnalysis, path: &str, target: Option<PathBuf>) -> TextEdit {
    let import = target
        .as_deref()
        .and_then(|target| self_path(analysis).map(|current| relative_import(&current, target)))
        .unwrap_or_else(|| normalize_import_path(path));
    let mut line = String::from("bring ");
    if builtins::MODULES
        .iter()
        .any(|(module, _)| *module == import)
    {
        line.push_str(&import);
    } else {
        line.push_str(&format!("{import:?}"));
    }
    line.push_str("\n\n");
    TextEdit {
        range: Range::new(Position::new(0, 0), Position::new(0, 0)),
        new_text: line,
    }
}

fn self_path(analysis: &DocumentAnalysis) -> Option<PathBuf> {
    analysis.uri.to_file_path().ok()
}

fn import_for_symbol(analysis: &DocumentAnalysis, symbol: &Symbol) -> Option<(String, PathBuf)> {
    let target = symbol.uri.to_file_path().ok()?;
    let target = target.with_extension("");
    let path = relative_import_for_completion(analysis, &target)?;
    Some((path.clone(), target))
}

fn relative_import_for_completion(analysis: &DocumentAnalysis, target: &Path) -> Option<String> {
    let current = current_analysis_path(analysis)?;
    Some(relative_import(&current, target))
}

fn current_analysis_path(analysis: &DocumentAnalysis) -> Option<PathBuf> {
    analysis.uri.to_file_path().ok()
}

fn relative_import(from: &Path, to: &Path) -> String {
    let from_directory = from.parent().unwrap_or(Path::new(""));
    let target = to.with_extension("");
    let from_components: Vec<_> = from_directory.components().collect();
    let target_components: Vec<_> = target.components().collect();
    let common = from_components
        .iter()
        .zip(&target_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts = Vec::new();
    for _ in common..from_components.len() {
        parts.push("..".to_string());
    }
    for component in &target_components[common..] {
        parts.push(component.as_os_str().to_string_lossy().to_string());
    }
    if parts.is_empty() {
        return ".".to_string();
    }
    let result = parts.join("/");
    if parts.first().map(String::as_str) == Some("..") {
        result
    } else {
        format!("./{result}")
    }
}

fn member_module(analysis: &DocumentAnalysis, position: Position) -> Option<String> {
    let offset = analysis.text.offset(position);
    let line_start = analysis.text.source()[..offset]
        .rfind('\n')
        .map(|value| value + 1)
        .unwrap_or(0);
    let before = &analysis.text.source()[line_start..offset];
    let trimmed = before.trim_end_matches(|value: char| value.is_alphanumeric() || value == '_');
    if !trimmed.ends_with('.') {
        return None;
    }
    let base = trimmed[..trimmed.len() - 1].trim_end();
    let name = base
        .rsplit(|value: char| !(value.is_alphanumeric() || value == '_'))
        .next()?;
    Module::NAMES.contains(&name).then(|| name.to_string())
}

fn member_model(
    backend: &Backend,
    analysis: &DocumentAnalysis,
    position: Position,
) -> Option<String> {
    let offset = analysis.text.offset(position);
    let line_start = analysis.text.source()[..offset]
        .rfind('\n')
        .map(|value| value + 1)
        .unwrap_or(0);
    let before = &analysis.text.source()[line_start..offset];
    let trimmed = before.trim_end_matches(|value: char| value.is_alphanumeric() || value == '_');
    if !trimmed.ends_with('.') {
        return None;
    }
    let name = trimmed[..trimmed.len() - 1]
        .rsplit(|value: char| !(value.is_alphanumeric() || value == '_'))
        .next()?;
    backend
        .all_symbols()
        .any(|symbol| symbol.kind == DeclKind::Model && symbol.name == name)
        .then(|| name.to_string())
}

fn language_snippets(analysis: &DocumentAnalysis, position: Position) -> Vec<CompletionItem> {
    let line = analysis.text.line_text(position.line).trim_start();
    let snippets = if line.starts_with("calc ") {
        vec![(
            "calc",
            "calc ${1:name}(${2:Int value}) => ${3:Int} {\n\t$0\n}",
        )]
    } else if line.starts_with("model ") {
        vec![(
            "model",
            "model ${1:Name} = ${2:items} [\n\t${3:id} => Int,\n]",
        )]
    } else if line.starts_with("GET ") {
        vec![("GET", "GET \"${1:/}\" :: {\n\t$0\n}")]
    } else {
        Vec::new()
    };
    snippets
        .into_iter()
        .map(|(label, insert_text)| CompletionItem {
            label: label.to_string(),
            kind: Some(CompletionItemKind::SNIPPET),
            detail: Some("HardScript snippet".to_string()),
            insert_text: Some(insert_text.to_string()),
            insert_text_format: Some(tower_lsp::lsp_types::InsertTextFormat::SNIPPET),
            sort_text: Some("050000".to_string()),
            ..CompletionItem::default()
        })
        .collect()
}

fn push_diagnostic_at(
    output: &mut BTreeMap<Url, Vec<Diagnostic>>,
    uri: &Url,
    analysis: &DocumentAnalysis,
    diagnostic: &Diag,
) {
    let range = diagnostic_range(analysis, diagnostic);
    let code = catalog::format(diagnostic.code);
    let mut message = diagnostic.message.clone();
    if let Some(expected) = &diagnostic.expected {
        message.push_str(&format!("\nExpected: {expected}"));
    }
    if let Some(received) = &diagnostic.received {
        message.push_str(&format!("\nReceived: {received}"));
    }
    if let Some(help) = &diagnostic.help {
        message.push_str(&format!("\n{help}"));
    }
    let mut related = diagnostic
        .related
        .iter()
        .filter_map(|entry| {
            let location = entry.location.as_ref()?;
            let location_uri = Url::parse(location)
                .ok()
                .or_else(|| Url::from_file_path(PathBuf::from(location)).ok())?;
            let range = entry
                .span
                .map(|span| {
                    Range::new(
                        Position::new(
                            span.line.saturating_sub(1) as u32,
                            span.col.saturating_sub(1) as u32,
                        ),
                        Position::new(span.line.saturating_sub(1) as u32, span.col as u32),
                    )
                })
                .unwrap_or_default();
            Some(DiagnosticRelatedInformation {
                location: Location::new(location_uri, range),
                message: entry
                    .label
                    .clone()
                    .unwrap_or_else(|| "Related declaration".to_string()),
            })
        })
        .collect::<Vec<_>>();
    for note in &diagnostic.notes {
        related.push(DiagnosticRelatedInformation {
            location: Location::new(uri.clone(), range),
            message: note.clone(),
        });
    }
    let tags =
        if (catalog::W_UNUSED_VARIABLE..=catalog::W_UNUSED_FUNCTION).contains(&diagnostic.code) {
            Some(vec![DiagnosticTag::UNNECESSARY])
        } else {
            None
        };
    let lsp = Diagnostic {
        range,
        severity: Some(diagnostic_severity(diagnostic)),
        code: Some(NumberOrString::String(code)),
        code_description: None,
        source: Some("hardscript".to_string()),
        message,
        related_information: if related.is_empty() {
            None
        } else {
            Some(related)
        },
        tags,
        data: Some(serde_json::json!({
            "kind": diagnostic.kind.name(),
            "help": diagnostic.help.clone(),
            "notes": diagnostic.notes.clone(),
            "suggestion": diagnostic.suggestion.clone(),
        })),
    };
    output.entry(uri.clone()).or_default().push(lsp);
}

fn diagnostic_range(analysis: &DocumentAnalysis, diagnostic: &Diag) -> Range {
    let fallback = Range::new(Position::new(0, 0), Position::new(0, 1));
    let Some(span) = diagnostic.span else {
        return fallback;
    };
    let start = analysis.text.span_start(span);
    if let Some(index) = analysis.tokens.iter().position(|token| token.span == span) {
        return Range::new(start, analysis.text.token_end(&analysis.tokens, index));
    }
    Range::new(start, analysis.text.advance(start, 1))
}

fn deduplicate_diagnostics(diagnostics: &mut Vec<Diagnostic>) {
    let mut seen = HashSet::new();
    diagnostics.retain(|diagnostic| {
        seen.insert((
            diagnostic.range.start.line,
            diagnostic.range.start.character,
            diagnostic.range.end.line,
            diagnostic.range.end.character,
            diagnostic_code(diagnostic),
            diagnostic.message.clone(),
        ))
    });
}

fn deduplicate_completion(items: &mut Vec<CompletionItem>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.label.clone()));
}

fn deduplicate_actions(actions: &mut Vec<CodeActionOrCommand>) {
    let mut seen = HashSet::new();
    actions.retain(|action| {
        let CodeActionOrCommand::CodeAction(action) = action else {
            return true;
        };
        seen.insert((action.title.clone(), action.kind.clone()))
    });
}

fn reference_matches(candidate: &Occurrence, canonical: &Occurrence) -> bool {
    if candidate.id == canonical.id {
        return true;
    }
    if matches!(
        canonical.kind,
        DeclKind::Property | DeclKind::Module | DeclKind::Builtin | DeclKind::Import
    ) {
        return false;
    }
    let name = match candidate.id.strip_prefix("call:") {
        Some(name) => name,
        None => match candidate.id.strip_prefix("property:") {
            Some(name) if candidate.kind == DeclKind::Variable => name,
            _ => return false,
        },
    };
    name == canonical.name
}

fn deduplicate_locations(locations: &mut Vec<Location>) {
    locations.sort_by_key(|location| {
        (
            location.uri.as_str().to_string(),
            location.range.start.line,
            location.range.start.character,
        )
    });
    locations.dedup_by(|left, right| {
        left.uri == right.uri
            && left.range.start == right.range.start
            && left.range.end == right.range.end
    });
}

fn symbol_hover(symbol: &Symbol) -> String {
    let location = symbol
        .uri
        .to_file_path()
        .ok()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| symbol.uri.to_string());
    let documentation = if symbol.documentation.is_empty() {
        "No source documentation was found immediately above this declaration.".to_string()
    } else {
        symbol.documentation.clone()
    };
    format!(
        "```hardscript\n{}\n```\n\n{}\n\n---\n{} **{}** at `{}:{}`.\n\n*Compiler note: annotations describe declared types; unannotated values remain dynamic.*",
        symbol.detail,
        documentation,
        symbol.kind.label(),
        symbol.name,
        location,
        symbol.selection_range.start.line + 1
    )
}

fn diagnostic_code(diagnostic: &Diagnostic) -> String {
    match &diagnostic.code {
        Some(NumberOrString::String(value)) => value.clone(),
        Some(NumberOrString::Number(value)) => catalog::format(*value as u16),
        None => String::new(),
    }
}

fn undefined_name(message: &str) -> Option<String> {
    let rest = message.strip_prefix('`')?.split('`').next()?;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

fn local_statement_end(analysis: &DocumentAnalysis, symbol: &Symbol) -> Position {
    let start = symbol.declaration_offset;
    let source = analysis.text.source();
    let mut offset = start;
    let mut depth = 0i32;
    let mut string = false;
    let mut escaped = false;
    while offset < source.len() {
        let Some(character) = source[offset..].chars().next() else {
            break;
        };
        if string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                string = false;
            }
        } else {
            match character {
                '"' => string = true,
                '{' | '[' | '(' => depth += 1,
                '}' | ']' | ')' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                '\n' if depth == 0 => break,
                _ => {}
            }
        }
        offset += character.len_utf8();
    }
    analysis.text.position(offset)
}

fn semantic_type_index(class: SemanticClass) -> u32 {
    match class {
        SemanticClass::Keyword => 0,
        SemanticClass::Function => 1,
        SemanticClass::Type => 2,
        SemanticClass::Module => 3,
        SemanticClass::Parameter => 4,
        SemanticClass::Property => 5,
        SemanticClass::Variable => 6,
        SemanticClass::Constant => 7,
        SemanticClass::String => 8,
        SemanticClass::Number => 9,
        SemanticClass::Operator => 10,
        SemanticClass::Decorator => 11,
        SemanticClass::Method => 12,
        SemanticClass::Comment => 13,
    }
}

fn split_semantic_range(analysis: &DocumentAnalysis, range: Range) -> Vec<Range> {
    if range.start.line == range.end.line {
        return vec![range];
    }
    (range.start.line..=range.end.line)
        .map(|line| {
            if line == range.start.line {
                Range::new(
                    range.start,
                    Position::new(
                        line,
                        analysis.text.line_text(line).encode_utf16().count() as u32,
                    ),
                )
            } else if line == range.end.line {
                Range::new(Position::new(line, 0), range.end)
            } else {
                let length = analysis.text.line_text(line).encode_utf16().count() as u32;
                Range::new(Position::new(line, 0), Position::new(line, length))
            }
        })
        .filter(|range| range.start < range.end)
        .collect()
}

fn signature_parameters(signature: &str) -> Vec<String> {
    let Some(open) = signature.find('(') else {
        return Vec::new();
    };
    let Some(close) = signature[open + 1..]
        .rfind(')')
        .map(|value| open + 1 + value)
    else {
        return Vec::new();
    };
    let value = signature[open + 1..close].trim();
    if value.is_empty() {
        return Vec::new();
    }
    value
        .split(',')
        .map(str::trim)
        .map(str::to_string)
        .collect()
}

fn bullets(values: &[&str]) -> String {
    values
        .iter()
        .map(|value| format!("- {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}
