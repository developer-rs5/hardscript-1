use crate::builtins;
use crate::text::TextIndex;
use hs_compiler::ast::*;
use hs_compiler::error::is_warning;
use hs_compiler::token::{Kw, Sym};
use hs_compiler::{frontend, lex, Diag, Span, Tok, Token};
use std::collections::{HashMap, HashSet};
use tower_lsp::lsp_types::{Position, Range, Url};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclKind {
    Function,
    Model,
    Field,
    Variable,
    Constant,
    Parameter,
    Route,
    Middleware,
    Socket,
    Test,
    Module,
    Import,
    Builtin,
    Property,
}

impl DeclKind {
    pub fn label(self) -> &'static str {
        match self {
            DeclKind::Function => "function",
            DeclKind::Model => "model",
            DeclKind::Field => "model field",
            DeclKind::Variable => "variable",
            DeclKind::Constant => "constant",
            DeclKind::Parameter => "parameter",
            DeclKind::Route => "route",
            DeclKind::Middleware => "middleware",
            DeclKind::Socket => "WebSocket endpoint",
            DeclKind::Test => "test",
            DeclKind::Module => "standard-library module",
            DeclKind::Import => "import",
            DeclKind::Builtin => "standard-library function",
            DeclKind::Property => "property",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticClass {
    Keyword,
    Function,
    Type,
    Module,
    Parameter,
    Property,
    Variable,
    Constant,
    String,
    Number,
    Operator,
    Decorator,
    Method,
    Comment,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub documentation: String,
    pub kind: DeclKind,
    pub uri: Url,
    pub range: Range,
    pub selection_range: Range,
    pub container: Option<String>,
    pub local: bool,
    pub declaration_offset: usize,
}

#[derive(Clone, Debug)]
pub struct Occurrence {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub documentation: String,
    pub kind: DeclKind,
    pub uri: Url,
    pub range: Range,
    pub container: Option<String>,
    pub local: bool,
}

#[derive(Clone, Debug)]
pub struct ImportInfo {
    pub path: String,
    pub span: Span,
    pub range: Range,
    pub string_range: Range,
}

#[derive(Clone, Debug)]
pub struct SemanticRange {
    pub range: Range,
    pub class: SemanticClass,
}

#[derive(Clone)]
pub struct DocumentAnalysis {
    pub uri: Url,
    pub text: TextIndex,
    pub program: Option<Program>,
    pub parse_diagnostics: Vec<Diag>,
    pub tokens: Vec<Token>,
    pub symbols: Vec<Symbol>,
    pub occurrences: Vec<Occurrence>,
    pub imports: Vec<ImportInfo>,
    pub semantic: Vec<SemanticRange>,
    pub comment_ranges: Vec<Range>,
    token_starts: Vec<usize>,
}

impl DocumentAnalysis {
    pub fn new(uri: Url, source: String) -> DocumentAnalysis {
        let text = TextIndex::new(source);
        let (parsed, parse_diagnostics) = match frontend(text.source(), uri.to_string()) {
            Ok(program) => (Some(program), Vec::new()),
            Err(diagnostics) => (None, diagnostics),
        };
        let (tokens, _) = lex(text.source());
        let tokens = tokens.unwrap_or_default();
        let token_starts = tokens
            .iter()
            .map(|token| text.token_offset(token.span))
            .collect::<Vec<_>>();
        let comments = comment_ranges(text.source());
        let mut analysis = DocumentAnalysis {
            uri: uri.clone(),
            text,
            program: parsed,
            parse_diagnostics,
            tokens,
            symbols: Vec::new(),
            occurrences: Vec::new(),
            imports: Vec::new(),
            semantic: Vec::new(),
            comment_ranges: comments,
            token_starts,
        };
        analysis.build_symbols(uri);
        analysis.build_occurrences();
        analysis.build_semantic();
        analysis
    }

    pub fn has_parse_errors(&self) -> bool {
        !self.parse_diagnostics.is_empty() || self.program.is_none()
    }

    pub fn position_offset(&self, position: Position) -> usize {
        self.text.offset(position)
    }

    pub fn symbol_at(&self, position: Position) -> Option<&Symbol> {
        self.symbols
            .iter()
            .find(|symbol| contains(position, symbol.selection_range))
    }

    pub fn occurrence_at(&self, position: Position) -> Option<&Occurrence> {
        self.occurrences
            .iter()
            .find(|occurrence| contains(position, occurrence.range))
    }

    pub fn exported_symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.iter().filter(|symbol| {
            !symbol.local
                && matches!(
                    symbol.kind,
                    DeclKind::Function | DeclKind::Model | DeclKind::Variable | DeclKind::Constant
                )
        })
    }

    fn build_symbols(&mut self, uri: Url) {
        let Some(program) = self.program.as_ref() else {
            return;
        };
        let mut symbols = Vec::new();
        let mut offsets: Vec<(usize, usize)> = program
            .stmts
            .iter()
            .enumerate()
            .map(|(index, statement)| (index, self.text.token_offset(statement.span())))
            .collect();
        offsets.sort_by_key(|(_, offset)| *offset);
        for (position, (index, start)) in offsets.iter().enumerate() {
            let end = offsets
                .get(position + 1)
                .map(|(_, next)| *next)
                .unwrap_or(self.text.source().len());
            self.collect_top_level(&uri, &program.stmts[*index], *start, end, &mut symbols);
        }
        self.symbols = symbols;
        self.imports = program
            .stmts
            .iter()
            .filter_map(|statement| match statement {
                Stmt::Import { path, span } => {
                    let start = self.text.token_offset(*span);
                    let range = self
                        .symbols
                        .iter()
                        .find(|symbol| symbol.kind == DeclKind::Import && symbol.name == *path)
                        .map(|symbol| symbol.range)
                        .unwrap_or_else(|| self.text.text_range(start, self.text.source().len()));
                    let string_range = self
                        .symbols
                        .iter()
                        .find(|symbol| symbol.kind == DeclKind::Import && symbol.name == *path)
                        .map(|symbol| symbol.selection_range)
                        .unwrap_or(range);
                    Some(ImportInfo {
                        path: path.clone(),
                        span: *span,
                        range,
                        string_range,
                    })
                }
                _ => None,
            })
            .collect();
    }

    fn collect_top_level(
        &self,
        uri: &Url,
        statement: &Stmt,
        start: usize,
        end: usize,
        symbols: &mut Vec<Symbol>,
    ) {
        let source = self.text.source();
        let docs = documentation_before(&self.text, self.text.position(start));
        match statement {
            Stmt::Bring(module, span) => {
                let name = builtins::module_name(module);
                let range = self.text.text_range(start, end);
                let selection = self.selection_for(start, end, name, false).unwrap_or(range);
                symbols.push(Symbol {
                    id: format!("module:{name}"),
                    name: name.to_string(),
                    detail: format!("bring {name}"),
                    documentation: builtins::module_documentation(name)
                        .unwrap_or("HardScript standard-library module.")
                        .to_string(),
                    kind: DeclKind::Module,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
                let _ = span;
            }
            Stmt::Import { path, span } => {
                let range = self.text.text_range(start, end);
                let selection = string_content_range(source, start, end).unwrap_or(range);
                symbols.push(Symbol {
                    id: format!("import:{}:{}", uri.as_str(), path),
                    name: path.clone(),
                    detail: format!("bring {path:?}"),
                    documentation: "Local HardScript module import.".to_string(),
                    kind: DeclKind::Import,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
                let _ = span;
            }
            Stmt::Model(model) => {
                let id = format!("model:{}:{}", uri.as_str(), model.name);
                let range = self.text.text_range(start, end);
                let selection = self
                    .selection_for(start, end, &model.name, false)
                    .unwrap_or(range);
                symbols.push(Symbol {
                    id: id.clone(),
                    name: model.name.clone(),
                    detail: format!("model {} = {}", model.name, model.table),
                    documentation: docs.clone(),
                    kind: DeclKind::Model,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
                for field in &model.fields {
                    let field_start = self.text.token_offset(field.span);
                    let field_range = self.text.text_range(field_start, end);
                    let field_selection = self
                        .selection_for(field_start, end, &field.name, false)
                        .unwrap_or(field_range);
                    symbols.push(Symbol {
                        id: format!("field:{}:{}:{}", uri.as_str(), model.name, field.name),
                        name: field.name.clone(),
                        detail: format!("{}: {}", field.name, field.ty),
                        documentation: format!("Field of model `{}` ({})", model.name, model.table),
                        kind: DeclKind::Field,
                        uri: uri.clone(),
                        range: field_range,
                        selection_range: field_selection,
                        container: Some(format!("model:{}", model.name)),
                        local: false,
                        declaration_offset: field_start,
                    });
                }
            }
            Stmt::Route(route) => {
                let name = format!("{} {}", route.method, route.path);
                let id = format!("route:{}:{}:{}", uri.as_str(), route.method, route.path);
                let range = self.text.text_range(start, end);
                let selection = self
                    .selection_for(start, end, &route.method, false)
                    .unwrap_or(range);
                let parameters = route
                    .params
                    .iter()
                    .map(|parameter| match &parameter.ty {
                        Some(ty) => format!("{}: {ty}", parameter.name),
                        None => parameter.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                symbols.push(Symbol {
                    id,
                    name,
                    detail: format!("{} {:?} ({})", route.method, route.path, parameters),
                    documentation: if docs.is_empty() {
                        "HardScript HTTP route.".to_string()
                    } else {
                        docs
                    },
                    kind: DeclKind::Route,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
                let container = symbols.last().map(|symbol| symbol.id.clone()).unwrap();
                for parameter in &route.params {
                    self.collect_parameter(uri, parameter, &container, start, end, symbols);
                }
                if !route.params.iter().any(|parameter| parameter.name == "req") {
                    self.collect_implicit(uri, "req", "request", &container, start, end, symbols);
                }
                self.collect_block(uri, &route.body, &container, start, end, symbols);
            }
            Stmt::Socket(socket) => {
                let id = format!("socket:{}:{}", uri.as_str(), socket.path);
                let range = self.text.text_range(start, end);
                let selection = string_content_range(source, start, end).unwrap_or(range);
                symbols.push(Symbol {
                    id: id.clone(),
                    name: socket.path.clone(),
                    detail: format!("socket {:?}", socket.path),
                    documentation: "HardScript WebSocket endpoint.".to_string(),
                    kind: DeclKind::Socket,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
                if let Some(body) = &socket.connect {
                    self.collect_block(uri, body, &id, start, end, symbols);
                }
                if let Some(body) = &socket.message {
                    self.collect_implicit(uri, "d", "message", &id, start, end, symbols);
                    self.collect_block(uri, body, &id, start, end, symbols);
                }
                if let Some(body) = &socket.disconnect {
                    self.collect_block(uri, body, &id, start, end, symbols);
                }
            }
            Stmt::Func(function) => {
                let id = format!("function:{}:{}", uri.as_str(), function.name);
                let range = self.text.text_range(start, end);
                let selection = self
                    .selection_for(start, end, &function.name, false)
                    .unwrap_or(range);
                let parameters = function
                    .params
                    .iter()
                    .map(|parameter| match &parameter.ty {
                        Some(ty) => format!("{}: {ty}", parameter.name),
                        None => parameter.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let return_type = function.ret.as_deref().unwrap_or("dynamic");
                let detail = format!("calc {}({}) => {return_type}", function.name, parameters);
                symbols.push(Symbol {
                    id: id.clone(),
                    name: function.name.clone(),
                    detail,
                    documentation: docs,
                    kind: DeclKind::Function,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
                for parameter in &function.params {
                    self.collect_parameter(uri, parameter, &id, start, end, symbols);
                }
                self.collect_block(uri, &function.body, &id, start, end, symbols);
            }
            Stmt::Middleware { name, body, .. } => {
                let id = format!("middleware:{}:{name}", uri.as_str());
                let range = self.text.text_range(start, end);
                let selection = self.selection_for(start, end, name, false).unwrap_or(range);
                symbols.push(Symbol {
                    id: id.clone(),
                    name: name.clone(),
                    detail: format!("before {name}"),
                    documentation: if docs.is_empty() {
                        "HardScript middleware.".to_string()
                    } else {
                        docs
                    },
                    kind: DeclKind::Middleware,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
                self.collect_implicit(uri, "req", "request", &id, start, end, symbols);
                self.collect_block(uri, body, &id, start, end, symbols);
            }
            Stmt::Test(test) => {
                let id = format!("test:{}:{}", uri.as_str(), test.name);
                let range = self.text.text_range(start, end);
                let selection = string_content_range(source, start, end).unwrap_or(range);
                symbols.push(Symbol {
                    id: id.clone(),
                    name: test.name.clone(),
                    detail: format!("test {:?}", test.name),
                    documentation: "HardScript test case.".to_string(),
                    kind: DeclKind::Test,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
                self.collect_block(uri, &test.body, &id, start, end, symbols);
            }
            Stmt::Var(variable) | Stmt::Const(variable) => {
                let constant = matches!(statement, Stmt::Const(_));
                let kind = if constant {
                    DeclKind::Constant
                } else {
                    DeclKind::Variable
                };
                let id = format!("global:{}:{}", uri.as_str(), variable.name);
                let range = self.text.text_range(start, end);
                let selection = self
                    .selection_for(start, end, &variable.name, false)
                    .unwrap_or(range);
                let detail = match &variable.ty {
                    Some(ty) => format!("{}: {ty}", variable.name),
                    None => variable.name.clone(),
                };
                symbols.push(Symbol {
                    id,
                    name: variable.name.clone(),
                    detail,
                    documentation: docs,
                    kind,
                    uri: uri.clone(),
                    range,
                    selection_range: selection,
                    container: None,
                    local: false,
                    declaration_offset: start,
                });
            }
            _ => {}
        }
    }

    fn collect_parameter(
        &self,
        uri: &Url,
        parameter: &FunParam,
        container: &str,
        _start: usize,
        end: usize,
        symbols: &mut Vec<Symbol>,
    ) {
        let declaration_offset = self.text.token_offset(parameter.span);
        let range = self.text.text_range(declaration_offset, end);
        let selection = self
            .selection_for(declaration_offset, end, &parameter.name, false)
            .unwrap_or(range);
        let detail = match &parameter.ty {
            Some(ty) => format!("{}: {ty}", parameter.name),
            None => parameter.name.clone(),
        };
        symbols.push(Symbol {
            id: format!("local:{container}:{}", parameter.name),
            name: parameter.name.clone(),
            detail,
            documentation: String::new(),
            kind: DeclKind::Parameter,
            uri: uri.clone(),
            range,
            selection_range: selection,
            container: Some(container.to_string()),
            local: true,
            declaration_offset,
        });
    }

    fn collect_implicit(
        &self,
        uri: &Url,
        name: &str,
        detail: &str,
        container: &str,
        start: usize,
        end: usize,
        symbols: &mut Vec<Symbol>,
    ) {
        let selection = self
            .selection_for(start, end, name, false)
            .unwrap_or_else(|| self.text.text_range(start, end));
        symbols.push(Symbol {
            id: format!("local:{container}:{name}"),
            name: name.to_string(),
            detail: format!("{name}: {detail}"),
            documentation: format!("Implicit {detail} binding."),
            kind: DeclKind::Parameter,
            uri: uri.clone(),
            range: self.text.text_range(start, end),
            selection_range: selection,
            container: Some(container.to_string()),
            local: true,
            declaration_offset: start,
        });
    }

    fn collect_block(
        &self,
        uri: &Url,
        statements: &[Stmt],
        container: &str,
        owner_start: usize,
        owner_end: usize,
        symbols: &mut Vec<Symbol>,
    ) {
        let mut starts: Vec<(usize, usize)> = statements
            .iter()
            .enumerate()
            .map(|(index, statement)| (index, self.text.token_offset(statement.span())))
            .collect();
        starts.sort_by_key(|(_, offset)| *offset);
        for (position, (index, start)) in starts.iter().enumerate() {
            let end = starts
                .get(position + 1)
                .map(|(_, next)| *next)
                .unwrap_or(owner_end);
            match &statements[*index] {
                Stmt::Var(variable) | Stmt::Const(variable) => {
                    let constant = matches!(&statements[*index], Stmt::Const(_));
                    let kind = if constant {
                        DeclKind::Constant
                    } else {
                        DeclKind::Variable
                    };
                    let range = self.text.text_range(*start, end);
                    let selection = self
                        .selection_for(*start, end, &variable.name, false)
                        .unwrap_or(range);
                    symbols.push(Symbol {
                        id: format!("local:{container}:{}", variable.name),
                        name: variable.name.clone(),
                        detail: match &variable.ty {
                            Some(ty) => format!("{}: {ty}", variable.name),
                            None => variable.name.clone(),
                        },
                        documentation: String::new(),
                        kind,
                        uri: uri.clone(),
                        range,
                        selection_range: selection,
                        container: Some(container.to_string()),
                        local: true,
                        declaration_offset: *start,
                    });
                }
                Stmt::If {
                    then_body,
                    else_body,
                    ..
                } => {
                    self.collect_block(uri, then_body, container, owner_start, owner_end, symbols);
                    self.collect_block(uri, else_body, container, owner_start, owner_end, symbols);
                }
                Stmt::Loop { var, body, .. } => {
                    let range = self.text.text_range(*start, end);
                    let selection = self.selection_for(*start, end, var, false).unwrap_or(range);
                    symbols.push(Symbol {
                        id: format!("local:{container}:{var}"),
                        name: var.clone(),
                        detail: format!("{var}: element"),
                        documentation: "Loop binding.".to_string(),
                        kind: DeclKind::Variable,
                        uri: uri.clone(),
                        range,
                        selection_range: selection,
                        container: Some(container.to_string()),
                        local: true,
                        declaration_offset: *start,
                    });
                    self.collect_block(uri, body, container, owner_start, owner_end, symbols);
                }
                _ => {}
            }
            let _ = owner_start;
        }
    }

    fn selection_for(&self, start: usize, end: usize, name: &str, quoted: bool) -> Option<Range> {
        let first = self.token_starts.partition_point(|value| *value < start);
        for index in first..self.tokens.len() {
            if self.token_starts[index] >= end {
                break;
            }
            if let Tok::Ident(value) = &self.tokens[index].tok {
                if value == name && !quoted {
                    return Some(self.text.token_range(&self.tokens, index));
                }
            }
        }
        if quoted {
            return string_content_range(self.text.source(), start, end);
        }
        self.text
            .source()
            .get(start..end)
            .and_then(|value| value.find(name))
            .map(|relative| {
                self.text
                    .text_range(start + relative, start + relative + name.len())
            })
    }

    fn build_occurrences(&mut self) {
        let lookup = SymbolLookup::new(&self.symbols);
        let blocks = lookup.local_blocks();
        let mut open_blocks: Vec<u32> = Vec::new();
        let mut next_block = 0usize;
        let mut occurrences = Vec::new();
        for (index, token) in self.tokens.iter().enumerate() {
            let Tok::Ident(name) = &token.tok else {
                continue;
            };
            let range = self.text.token_range(&self.tokens, index);
            let offset = self.text.offset(range.start);
            while next_block < blocks.len()
                && self.symbols[blocks[next_block] as usize].range.start <= range.start
            {
                open_blocks.push(blocks[next_block]);
                next_block += 1;
            }
            while let Some(top) = open_blocks.last() {
                if contains(range.start, self.symbols[*top as usize].range) {
                    break;
                }
                open_blocks.pop();
            }
            let container = open_blocks
                .last()
                .map(|top| &self.symbols[*top as usize])
                .filter(|symbol| symbol.declaration_offset <= offset)
                .and_then(|symbol| symbol.container.clone());
            if let Some(symbol) = lookup.symbol_at(&self.symbols, range) {
                occurrences.push(Occurrence {
                    id: symbol.id.clone(),
                    name: symbol.name.clone(),
                    detail: symbol.detail.clone(),
                    documentation: symbol.documentation.clone(),
                    kind: symbol.kind,
                    uri: symbol.uri.clone(),
                    range,
                    container: symbol.container.clone(),
                    local: symbol.local,
                });
                continue;
            }
            if let Some(occurrence) =
                self.classify_identifier(index, name, range, container.as_deref(), &lookup)
            {
                occurrences.push(occurrence);
            }
        }
        let mut seen: HashSet<&str> = HashSet::with_capacity(occurrences.len());
        for occurrence in &occurrences {
            seen.insert(occurrence.id.as_str());
        }
        let mut missing: Vec<Occurrence> = Vec::new();
        for symbol in self.symbols.iter().filter(|symbol| {
            matches!(
                symbol.kind,
                DeclKind::Route | DeclKind::Socket | DeclKind::Test
            )
        }) {
            if seen.contains(symbol.id.as_str()) {
                continue;
            }
            missing.push(Occurrence {
                id: symbol.id.clone(),
                name: symbol.name.clone(),
                detail: symbol.detail.clone(),
                documentation: symbol.documentation.clone(),
                kind: symbol.kind,
                uri: symbol.uri.clone(),
                range: symbol.selection_range,
                container: None,
                local: false,
            });
        }
        occurrences.extend(missing);
        self.occurrences = occurrences;
    }

    fn classify_identifier(
        &self,
        index: usize,
        name: &str,
        range: Range,
        container: Option<&str>,
        lookup: &SymbolLookup<'_>,
    ) -> Option<Occurrence> {
        let previous = index
            .checked_sub(1)
            .and_then(|value| self.tokens.get(value));
        let next = self.tokens.get(index + 1);
        let after_dot = previous
            .map(|token| matches!(&token.tok, Tok::Sym(Sym::Dot)))
            .unwrap_or(false);
        if let Some(container) = container {
            if let Some(symbol) = lookup.local_named(
                name,
                container,
                self.text.offset(range.start),
                &self.symbols,
            ) {
                return Some(symbol_occurrence(symbol, range));
            }
        }
        if after_dot {
            if let Some(module) = index
                .checked_sub(2)
                .and_then(|value| self.tokens.get(value))
                .and_then(|token| match &token.tok {
                    Tok::Ident(name) => Some(name.as_str()),
                    _ => None,
                })
            {
                if Module::NAMES.contains(&module) {
                    if let Some(builtin) = builtins::find(module, name) {
                        return Some(Occurrence {
                            id: format!("builtin:{}:{}", builtin.module, builtin.name),
                            name: builtin.name.to_string(),
                            detail: builtin.signature.to_string(),
                            documentation: builtin.documentation.to_string(),
                            kind: DeclKind::Builtin,
                            uri: self_uri_placeholder(),
                            range,
                            container: None,
                            local: false,
                        });
                    }
                    return Some(Occurrence {
                        id: format!("module:{module}"),
                        name: name.to_string(),
                        detail: format!("{module}.{name}"),
                        documentation: builtins::module_documentation(module)
                            .unwrap_or("Standard-library member.")
                            .to_string(),
                        kind: DeclKind::Module,
                        uri: self_uri_placeholder(),
                        range,
                        container: None,
                        local: false,
                    });
                }
            }
            if let Some(field) = index
                .checked_sub(2)
                .and_then(|model_index| {
                    self.tokens
                        .get(model_index)
                        .and_then(|token| match &token.tok {
                            Tok::Ident(model) if lookup.is_model(model) => Some(model.as_str()),
                            _ => None,
                        })
                })
                .and_then(|model| lookup.field_in_model(name, model, &self.symbols))
                .or_else(|| lookup.field_named(name, &self.symbols))
            {
                return Some(Occurrence {
                    id: field.id.clone(),
                    name: field.name.clone(),
                    detail: field.detail.clone(),
                    documentation: field.documentation.clone(),
                    kind: DeclKind::Field,
                    uri: field.uri.clone(),
                    range,
                    container: field.container.clone(),
                    local: false,
                });
            }
            return Some(Occurrence {
                id: format!("property:{name}"),
                name: name.to_string(),
                detail: name.to_string(),
                documentation: String::new(),
                kind: DeclKind::Property,
                uri: self_uri_placeholder(),
                range,
                container: None,
                local: false,
            });
        }
        if Module::NAMES.contains(&name) {
            return Some(Occurrence {
                id: format!("module:{name}"),
                name: name.to_string(),
                detail: format!("bring {name}"),
                documentation: builtins::module_documentation(name)
                    .unwrap_or("HardScript standard-library module.")
                    .to_string(),
                kind: DeclKind::Module,
                uri: self_uri_placeholder(),
                range,
                container: None,
                local: false,
            });
        }
        let called = next
            .map(|token| matches!(&token.tok, Tok::Sym(Sym::LParen)))
            .unwrap_or(false);
        if called {
            if let Some(function) = lookup.function_named(name, &self.symbols) {
                return Some(symbol_occurrence(function, range));
            }
            return Some(Occurrence {
                id: format!("call:{name}"),
                name: name.to_string(),
                detail: format!("{name}(…)"),
                documentation: "Function call.".to_string(),
                kind: DeclKind::Function,
                uri: self_uri_placeholder(),
                range,
                container: None,
                local: false,
            });
        }
        if let Some(symbol) = lookup.global_named(name, &self.symbols) {
            return Some(symbol_occurrence(symbol, range));
        }
        let property_key = next
            .map(|token| matches!(&token.tok, Tok::Sym(Sym::Colon)))
            .unwrap_or(false);
        Some(Occurrence {
            id: format!("property:{name}"),
            name: name.to_string(),
            detail: name.to_string(),
            documentation: String::new(),
            kind: if property_key {
                DeclKind::Property
            } else {
                DeclKind::Variable
            },
            uri: self_uri_placeholder(),
            range,
            container: container.map(|value| value.to_string()),
            local: container.is_some(),
        })
    }

    fn build_semantic(&mut self) {
        let mut order: Vec<u32> = (0..self.occurrences.len() as u32).collect();
        order.sort_by_key(|index| {
            let occurrence = &self.occurrences[*index as usize];
            (
                occurrence.range.start.line,
                occurrence.range.start.character,
            )
        });
        let mut semantic = Vec::new();
        for (index, token) in self.tokens.iter().enumerate() {
            let class = match &token.tok {
                Tok::Kw(Kw::Get | Kw::Post | Kw::Put | Kw::Delete | Kw::Patch) => {
                    SemanticClass::Method
                }
                Tok::Kw(_) => SemanticClass::Keyword,
                Tok::Ident(_)
                    if index > 0 && matches!(self.tokens[index - 1].tok, Tok::Sym(Sym::Hash)) =>
                {
                    SemanticClass::Decorator
                }
                Tok::Ident(_) => {
                    let start = self.text.token_range(&self.tokens, index).start;
                    let mut found = None;
                    if let Ok(position) =
                        order.binary_search_by_key(&(start.line, start.character), |candidate| {
                            let occurrence = &self.occurrences[*candidate as usize];
                            (
                                occurrence.range.start.line,
                                occurrence.range.start.character,
                            )
                        })
                    {
                        let mut cursor = position;
                        while cursor > 0
                            && (
                                self.occurrences[order[cursor - 1] as usize]
                                    .range
                                    .start
                                    .line,
                                self.occurrences[order[cursor - 1] as usize]
                                    .range
                                    .start
                                    .character,
                            ) == (start.line, start.character)
                        {
                            cursor -= 1;
                        }
                        for candidate in &order[cursor..=position] {
                            let occurrence = &self.occurrences[*candidate as usize];
                            if contains(start, occurrence.range) {
                                found = Some(semantic_for_occurrence(occurrence));
                                break;
                            }
                        }
                    }
                    found.unwrap_or(SemanticClass::Variable)
                }
                Tok::Int(_) | Tok::Float(_) => SemanticClass::Number,
                Tok::Str(_) => SemanticClass::String,
                Tok::Sym(Sym::Hash) => SemanticClass::Decorator,
                Tok::Sym(_) => SemanticClass::Operator,
                Tok::Eof => continue,
            };
            semantic.push(SemanticRange {
                range: self.text.token_range(&self.tokens, index),
                class,
            });
        }
        semantic.extend(
            self.comment_ranges
                .iter()
                .copied()
                .map(|range| SemanticRange {
                    range,
                    class: SemanticClass::Comment,
                }),
        );
        semantic.sort_by_key(|item| {
            (
                item.range.start.line,
                item.range.start.character,
                item.range.end,
            )
        });
        self.semantic = semantic;
    }
}

struct SymbolLookup<'a> {
    by_selection_start: Vec<u32>,
    locals_by_name: HashMap<&'a str, Vec<u32>>,
    globals_by_name: HashMap<&'a str, u32>,
    functions_by_name: HashMap<&'a str, u32>,
    fields_by_name: HashMap<&'a str, Vec<u32>>,
    first_field_by_name: HashMap<&'a str, u32>,
    models: HashSet<&'a str>,
    local_blocks: Vec<u32>,
}

impl<'a> SymbolLookup<'a> {
    fn new(symbols: &'a [Symbol]) -> SymbolLookup<'a> {
        let mut lookup = SymbolLookup {
            by_selection_start: Vec::with_capacity(symbols.len()),
            locals_by_name: HashMap::new(),
            globals_by_name: HashMap::new(),
            functions_by_name: HashMap::new(),
            fields_by_name: HashMap::new(),
            first_field_by_name: HashMap::new(),
            models: HashSet::new(),
            local_blocks: Vec::new(),
        };
        for (index, symbol) in symbols.iter().enumerate() {
            let position = index as u32;
            lookup.by_selection_start.push(position);
            if symbol.local {
                lookup
                    .locals_by_name
                    .entry(symbol.name.as_str())
                    .or_default()
                    .push(position);
                if symbol.container.is_some() {
                    lookup.local_blocks.push(position);
                }
            } else {
                lookup
                    .globals_by_name
                    .entry(symbol.name.as_str())
                    .or_insert(position);
            }
            if symbol.kind == DeclKind::Function {
                lookup
                    .functions_by_name
                    .entry(symbol.name.as_str())
                    .or_insert(position);
            }
            if symbol.kind == DeclKind::Field {
                lookup
                    .fields_by_name
                    .entry(symbol.name.as_str())
                    .or_default()
                    .push(position);
                lookup
                    .first_field_by_name
                    .entry(symbol.name.as_str())
                    .or_insert(position);
            }
            if symbol.kind == DeclKind::Model {
                lookup.models.insert(symbol.name.as_str());
            }
        }
        lookup.by_selection_start.sort_by_key(|position| {
            let symbol = &symbols[*position as usize];
            (
                symbol.selection_range.start.line,
                symbol.selection_range.start.character,
            )
        });
        lookup.local_blocks.sort_by_key(|position| {
            let symbol = &symbols[*position as usize];
            (symbol.range.start.line, symbol.range.start.character)
        });
        lookup
    }

    fn local_blocks(&self) -> &[u32] {
        &self.local_blocks
    }

    fn symbol_at<'s>(&self, symbols: &'s [Symbol], range: Range) -> Option<&'s Symbol> {
        let key = |symbol: &Symbol| {
            (
                symbol.selection_range.start.line,
                symbol.selection_range.start.character,
            )
        };
        let points = [
            (range.start.line, range.start.character),
            (range.end.line, range.end.character),
        ];
        let mut best: Option<u32> = None;
        for point in points {
            let Ok(position) = self
                .by_selection_start
                .binary_search_by_key(&point, |candidate| key(&symbols[*candidate as usize]))
            else {
                continue;
            };
            let mut cursor = position;
            while self
                .by_selection_start
                .get(cursor.wrapping_sub(1))
                .is_some_and(|candidate| key(&symbols[*candidate as usize]) == point)
            {
                cursor -= 1;
            }
            for candidate in &self.by_selection_start[cursor..=position] {
                let symbol = &symbols[*candidate as usize];
                if !(contains(range.start, symbol.selection_range)
                    || contains(range.end, symbol.selection_range))
                {
                    continue;
                }
                best = Some(match best {
                    Some(current) if current <= *candidate => current,
                    _ => *candidate,
                });
            }
        }
        best.map(|position| &symbols[position as usize])
    }

    fn local_named<'s>(
        &self,
        name: &str,
        container: &str,
        offset: usize,
        symbols: &'s [Symbol],
    ) -> Option<&'s Symbol> {
        self.locals_by_name
            .get(name)?
            .iter()
            .rev()
            .find_map(|position| {
                let symbol = &symbols[*position as usize];
                (symbol.container.as_deref() == Some(container)
                    && symbol.declaration_offset <= offset)
                    .then_some(symbol)
            })
    }

    fn is_model(&self, name: &str) -> bool {
        self.models.contains(name)
    }

    fn field_in_model<'s>(
        &self,
        name: &str,
        model: &str,
        symbols: &'s [Symbol],
    ) -> Option<&'s Symbol> {
        let container = format!("model:{model}");
        self.fields_by_name.get(name)?.iter().find_map(|position| {
            let symbol = &symbols[*position as usize];
            (symbol.container.as_deref() == Some(container.as_str())).then_some(symbol)
        })
    }

    fn field_named<'s>(&self, name: &str, symbols: &'s [Symbol]) -> Option<&'s Symbol> {
        self.first_field_by_name
            .get(name)
            .map(|position| &symbols[*position as usize])
    }

    fn function_named<'s>(&self, name: &str, symbols: &'s [Symbol]) -> Option<&'s Symbol> {
        self.functions_by_name
            .get(name)
            .map(|position| &symbols[*position as usize])
    }

    fn global_named<'s>(&self, name: &str, symbols: &'s [Symbol]) -> Option<&'s Symbol> {
        self.globals_by_name
            .get(name)
            .map(|position| &symbols[*position as usize])
    }
}

fn symbol_occurrence(symbol: &Symbol, range: Range) -> Occurrence {
    Occurrence {
        id: symbol.id.clone(),
        name: symbol.name.clone(),
        detail: symbol.detail.clone(),
        documentation: symbol.documentation.clone(),
        kind: symbol.kind,
        uri: symbol.uri.clone(),
        range,
        container: symbol.container.clone(),
        local: symbol.local,
    }
}

fn semantic_for_occurrence(occurrence: &Occurrence) -> SemanticClass {
    match occurrence.kind {
        DeclKind::Function | DeclKind::Builtin => SemanticClass::Function,
        DeclKind::Model => SemanticClass::Type,
        DeclKind::Module | DeclKind::Import => SemanticClass::Module,
        DeclKind::Parameter => SemanticClass::Parameter,
        DeclKind::Field | DeclKind::Property => SemanticClass::Property,
        DeclKind::Constant => SemanticClass::Constant,
        DeclKind::Variable => SemanticClass::Variable,
        DeclKind::Route | DeclKind::Middleware | DeclKind::Socket | DeclKind::Test => {
            SemanticClass::Function
        }
    }
}

fn contains(position: Position, range: Range) -> bool {
    position >= range.start && position <= range.end
}

fn documentation_before(text: &TextIndex, position: Position) -> String {
    let line = position.line as usize;
    let mut lines = Vec::new();
    let mut current = line;
    while current > 0 {
        current -= 1;
        let value = text.line_text(current as u32).trim();
        if value.is_empty() {
            break;
        }
        if let Some(value) = value.strip_prefix("///") {
            lines.push(value.trim().to_string());
            continue;
        }
        if let Some(value) = value.strip_prefix("//") {
            lines.push(value.trim().to_string());
            continue;
        }
        if value.starts_with("/*") && value.ends_with("*/") {
            lines.push(value[2..value.len() - 2].trim().to_string());
            continue;
        }
        break;
    }
    lines.reverse();
    lines.join("\n")
}

fn string_content_range(source: &str, start: usize, end: usize) -> Option<Range> {
    let text = source.get(start..end)?;
    let relative_start = text.find('"')?;
    let content_start = start + relative_start + 1;
    let after = source.get(content_start..end)?;
    let relative_end = after.find('"')?;
    let content_end = content_start + relative_end;
    let prefix = &source[..content_start];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map(|value| value + 1).unwrap_or(0);
    let start_position = Position::new(
        line as u32,
        source[line_start..content_start].encode_utf16().count() as u32,
    );
    let end_position = Position::new(
        line as u32,
        source[line_start..content_end].encode_utf16().count() as u32,
    );
    Some(Range::new(start_position, end_position))
}

fn self_uri_placeholder() -> Url {
    Url::parse("hardscript:///stdlib").unwrap()
}

fn comment_ranges(source: &str) -> Vec<Range> {
    let bytes = source.as_bytes();
    let mut ranges = Vec::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes[offset] == b'"' {
            let triple = bytes
                .get(offset..offset + 3)
                .is_some_and(|value| value == b"\"\"\"");
            if triple {
                offset += 3;
                while offset < bytes.len() {
                    if bytes[offset..].starts_with(b"\"\"\"") {
                        offset += 3;
                        break;
                    }
                    offset += 1;
                }
            } else {
                offset += 1;
                while offset < bytes.len() {
                    if bytes[offset] == b'\\' {
                        offset = (offset + 2).min(bytes.len());
                    } else if bytes[offset] == b'"' {
                        offset += 1;
                        break;
                    } else {
                        offset += 1;
                    }
                }
            }
            continue;
        }
        if bytes[offset..].starts_with(b"//") {
            let start = offset;
            offset += 2;
            while offset < bytes.len() && bytes[offset] != b'\n' {
                offset += 1;
            }
            ranges.extend(split_range_by_line(source, start, offset));
            continue;
        }
        if bytes[offset..].starts_with(b"/*") {
            let start = offset;
            offset += 2;
            while offset < bytes.len() && !bytes[offset..].starts_with(b"*/") {
                offset += 1;
            }
            offset = (offset + 2).min(bytes.len());
            ranges.extend(split_range_by_line(source, start, offset));
            continue;
        }
        offset += source[offset..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }
    ranges
}

fn split_range_by_line(source: &str, start: usize, end: usize) -> Vec<Range> {
    let mut ranges = Vec::new();
    let mut segment_start = start;
    for (relative, byte) in source.as_bytes()[start..end].iter().enumerate() {
        if *byte == b'\n' {
            let line_end = start + relative;
            if segment_start < line_end {
                let prefix = &source[..segment_start];
                let line = prefix.bytes().filter(|value| *value == b'\n').count();
                let line_start = prefix.rfind('\n').map(|value| value + 1).unwrap_or(0);
                ranges.push(Range::new(
                    Position::new(
                        line as u32,
                        source[line_start..segment_start].encode_utf16().count() as u32,
                    ),
                    Position::new(
                        line as u32,
                        source[line_start..line_end].encode_utf16().count() as u32,
                    ),
                ));
            }
            segment_start = line_end + 1;
        }
    }
    if segment_start < end {
        let prefix = &source[..segment_start];
        let line = prefix.bytes().filter(|value| *value == b'\n').count();
        let line_start = prefix.rfind('\n').map(|value| value + 1).unwrap_or(0);
        ranges.push(Range::new(
            Position::new(
                line as u32,
                source[line_start..segment_start].encode_utf16().count() as u32,
            ),
            Position::new(
                line as u32,
                source[line_start..end].encode_utf16().count() as u32,
            ),
        ));
    }
    ranges
}

pub fn span_key(span: Span) -> (usize, usize) {
    (span.line, span.col)
}

pub fn diagnostic_severity(diagnostic: &Diag) -> tower_lsp::lsp_types::DiagnosticSeverity {
    if is_warning(diagnostic.code) && !diagnostic.promote {
        tower_lsp::lsp_types::DiagnosticSeverity::WARNING
    } else {
        tower_lsp::lsp_types::DiagnosticSeverity::ERROR
    }
}

pub fn collect_span_locations(program: &Program) -> HashMap<(usize, usize), String> {
    let mut locations = HashMap::new();
    for statement in &program.stmts {
        collect_statement_spans(statement, &program.path, &mut locations);
    }
    locations
}

fn collect_statement_spans(
    statement: &Stmt,
    path: &str,
    locations: &mut HashMap<(usize, usize), String>,
) {
    locations.insert(span_key(statement.span()), path.to_string());
    match statement {
        Stmt::Model(model) => {
            for field in &model.fields {
                locations.insert(span_key(field.span), path.to_string());
            }
        }
        Stmt::Route(route) => {
            for parameter in &route.params {
                locations.insert(span_key(parameter.span), path.to_string());
            }
            for child in &route.body {
                collect_statement_spans(child, path, locations);
            }
        }
        Stmt::Socket(socket) => {
            for body in [&socket.connect, &socket.message, &socket.disconnect]
                .into_iter()
                .flatten()
            {
                for child in body {
                    collect_statement_spans(child, path, locations);
                }
            }
        }
        Stmt::Func(function) => {
            for parameter in &function.params {
                locations.insert(span_key(parameter.span), path.to_string());
            }
            for child in &function.body {
                collect_statement_spans(child, path, locations);
            }
        }
        Stmt::Middleware { body, .. } | Stmt::Test(TestDef { body, .. }) => {
            for child in body {
                collect_statement_spans(child, path, locations);
            }
        }
        Stmt::Var(variable) | Stmt::Const(variable) => {
            collect_expression_spans(&variable.value, path, locations);
        }
        Stmt::If {
            cond,
            then_body,
            else_body,
            ..
        } => {
            collect_expression_spans(cond, path, locations);
            for child in then_body.iter().chain(else_body) {
                collect_statement_spans(child, path, locations);
            }
        }
        Stmt::Loop { iter, body, .. } => {
            collect_expression_spans(iter, path, locations);
            for child in body {
                collect_statement_spans(child, path, locations);
            }
        }
        Stmt::Return(expression, _) => collect_expression_spans(expression, path, locations),
        Stmt::Race(expressions, _) => {
            for expression in expressions {
                collect_expression_spans(expression, path, locations);
            }
        }
        Stmt::Expect { lhs, rhs, .. } => {
            collect_expression_spans(lhs, path, locations);
            collect_expression_spans(rhs, path, locations);
        }
        Stmt::ExprStmt(expression) => collect_expression_spans(expression, path, locations),
        _ => {}
    }
}

fn collect_expression_spans(
    expression: &Expr,
    path: &str,
    locations: &mut HashMap<(usize, usize), String>,
) {
    locations.insert(span_key(expression.span()), path.to_string());
    match expression {
        Expr::Member(base, _, _) => collect_expression_spans(base, path, locations),
        Expr::Index(base, index, _) => {
            collect_expression_spans(base, path, locations);
            collect_expression_spans(index, path, locations);
        }
        Expr::Call { callee, args, .. } => {
            collect_expression_spans(callee, path, locations);
            for argument in args {
                collect_expression_spans(argument, path, locations);
            }
        }
        Expr::Unary(_, expression, _) | Expr::Range(expression, _, _) => {
            collect_expression_spans(expression, path, locations)
        }
        Expr::Binary(_, left, right, _) => {
            collect_expression_spans(left, path, locations);
            collect_expression_spans(right, path, locations);
        }
        Expr::List(items, _) => {
            for item in items {
                collect_expression_spans(item, path, locations);
            }
        }
        Expr::Obj(values, _) => {
            for (_, value) in values {
                collect_expression_spans(value, path, locations);
            }
        }
        Expr::Match(subject, arms, _) => {
            collect_expression_spans(subject, path, locations);
            for arm in arms {
                if let Some(value) = &arm.value {
                    collect_expression_spans(value, path, locations);
                }
                collect_expression_spans(&arm.body, path, locations);
            }
        }
        Expr::HttpCall {
            body: Some(body), ..
        } => collect_expression_spans(body, path, locations),
        _ => {}
    }
}

pub fn module_expression_name(expression: &Expr) -> Option<&str> {
    match expression {
        Expr::Ident(name, _) => Some(name),
        Expr::Member(base, field, _) => {
            if module_expression_name(base).is_some_and(|name| Module::NAMES.contains(&name)) {
                Some(field)
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn reachable_documents(
    start: &Url,
    documents: &HashMap<Url, DocumentAnalysis>,
    paths: &HashMap<String, Url>,
) -> Vec<Url> {
    let mut pending = vec![start.clone()];
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    while let Some(uri) = pending.pop() {
        if !seen.insert(uri.clone()) {
            continue;
        }
        ordered.push(uri.clone());
        let Some(document) = documents.get(&uri) else {
            continue;
        };
        let Some(path) = uri.to_file_path().ok() else {
            continue;
        };
        let parent = path.parent().map(|value| value.to_path_buf());
        for import in &document.imports {
            let candidate = if import.path.starts_with('.') {
                parent.as_ref().map(|base| base.join(&import.path))
            } else {
                let root = workspace_root_for(&path, paths);
                root.map(|base| base.join(&import.path))
            };
            let Some(candidate) = candidate else {
                continue;
            };
            let candidates = [
                candidate.clone(),
                candidate.with_extension("hard"),
                candidate.join("main.hard"),
            ];
            for candidate in candidates {
                let normalized = normalize_path(&candidate);
                if let Some(target) = paths.get(&normalized) {
                    pending.push(target.clone());
                    break;
                }
            }
        }
    }
    ordered.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    ordered
}

fn workspace_root_for(
    path: &std::path::Path,
    paths: &HashMap<String, Url>,
) -> Option<std::path::PathBuf> {
    let mut current = path.parent();
    while let Some(directory) = current {
        let manifest = directory.join("hard.toml");
        if manifest.is_file() {
            return Some(directory.to_path_buf());
        }
        if paths
            .keys()
            .any(|value| std::path::Path::new(value).starts_with(directory))
        {
            return Some(directory.to_path_buf());
        }
        current = directory.parent();
    }
    None
}

pub fn normalize_path(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri() -> Url {
        Url::parse("file:///tmp/main.hard").unwrap()
    }

    #[test]
    fn indexes_functions_parameters_and_references() {
        let source = "calc add(Int a, Int b) => Int {\n    total <- a + b\n    <- total\n}\n";
        let analysis = DocumentAnalysis::new(uri(), source.to_string());
        assert!(analysis.parse_diagnostics.is_empty());
        assert!(analysis.symbols.iter().any(|symbol| symbol.name == "add"));
        assert!(analysis
            .symbols
            .iter()
            .any(|symbol| symbol.name == "a" && symbol.kind == DeclKind::Parameter));
        assert!(analysis
            .occurrences
            .iter()
            .any(|occurrence| occurrence.name == "total" && occurrence.local));
    }

    #[test]
    fn indexes_models_fields_and_routes() {
        let source = "model User = users [\n    id => Int #id,\n]\nGET \"/users\" :: {\n    <- { id: 1 }\n}\n";
        let analysis = DocumentAnalysis::new(uri(), source.to_string());
        assert!(analysis.parse_diagnostics.is_empty());
        assert!(analysis
            .symbols
            .iter()
            .any(|symbol| symbol.name == "User" && symbol.kind == DeclKind::Model));
        assert!(analysis
            .symbols
            .iter()
            .any(|symbol| symbol.name == "id" && symbol.kind == DeclKind::Field));
        assert!(analysis
            .symbols
            .iter()
            .any(|symbol| symbol.name == "GET /users" && symbol.kind == DeclKind::Route));
    }

    #[test]
    fn semantic_classes_include_builtins_and_comments() {
        let source =
            "bring json\n// docs\ncalc parse(Str text) => dynamic {\n    <- json.parse(text)\n}\n";
        let analysis = DocumentAnalysis::new(uri(), source.to_string());
        assert!(analysis
            .semantic
            .iter()
            .any(|token| token.class == SemanticClass::Module));
        assert!(analysis
            .semantic
            .iter()
            .any(|token| token.class == SemanticClass::Comment));
        assert!(analysis
            .semantic
            .iter()
            .any(|token| token.class == SemanticClass::Function));
    }
}
