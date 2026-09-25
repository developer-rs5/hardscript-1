use hs_lsp::analysis::{DeclKind, DocumentAnalysis, SemanticClass};
use hs_lsp::backend::Backend;
use hs_lsp::text::{fuzzy_score, TextIndex};
use hs_lsp::Backend as LspBackend;
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::*;

fn uri(name: &str) -> Url {
    Url::from_file_path(Path::new("/tmp/hardscript-lsp-tests").join(name)).unwrap()
}

fn analysis(name: &str, source: &str) -> DocumentAnalysis {
    DocumentAnalysis::new(uri(name), source.to_string())
}

fn backend(name: &str, source: &str) -> (Backend, Url) {
    let document_uri = uri(name);
    let mut backend = Backend::new();
    backend.open_document(document_uri.clone(), source.to_string(), 1);
    (backend, document_uri)
}

fn completion(source: &str, line: u32, character: u32) -> Vec<CompletionItem> {
    let (backend, document_uri) = backend("main.hard", source);
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: document_uri },
            position: Position::new(line, character),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    match backend.completion(&params) {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    }
}

fn has_completion(source: &str, line: u32, character: u32, label: &str) -> bool {
    completion(source, line, character)
        .iter()
        .any(|item| item.label == label)
}

fn position(source: &str, needle: &str) -> Position {
    let index = TextIndex::new(source.to_string());
    let offset = source.find(needle).unwrap();
    index.position(offset)
}

fn function_source() -> &'static str {
    "/// Adds numbers\ncalc add(Int a, Int b) => Int {\n    total <- a + b\n    <- total\n}\n"
}

#[test]
fn text_utf16_emoji_position() {
    let index = TextIndex::new("a😀b".to_string());
    assert_eq!(index.position("a".len()), Position::new(0, 1));
    assert_eq!(index.position("a😀".len()), Position::new(0, 3));
    assert_eq!(index.offset(Position::new(0, 3)), "a😀".len());
}

#[test]
fn text_mid_codepoint_is_clamped() {
    let index = TextIndex::new("a😀b".to_string());
    assert_eq!(index.position(2), Position::new(0, 1));
    assert_eq!(index.offset(Position::new(0, 2)), 1);
}

#[test]
fn text_out_of_range_clamps_to_end() {
    let index = TextIndex::new("abc".to_string());
    assert_eq!(index.position(99), Position::new(0, 3));
    assert_eq!(index.offset(Position::new(9, 0)), 3);
}

#[test]
fn text_word_at_cursor() {
    let index = TextIndex::new("value\n".to_string());
    assert_eq!(index.word_at(Position::new(0, 2)).unwrap().text, "value");
    assert_eq!(
        index.word_before(Position::new(0, 5)).unwrap().text,
        "value"
    );
}

#[test]
fn text_word_rejects_string_content() {
    let index = TextIndex::new("value <- \"value\"\n".to_string());
    assert!(index.word_at(Position::new(0, 13)).is_none());
}

#[test]
fn text_word_handles_unicode_identifier() {
    let index = TextIndex::new("café <- 1\n".to_string());
    assert_eq!(index.word_at(Position::new(0, 2)).unwrap().text, "café");
}

#[test]
fn text_import_string_detection() {
    let index = TextIndex::new("bring \"./utils\"\n".to_string());
    assert!(index.is_import_string(Position::new(0, 10)));
    assert!(!index.is_import_string(Position::new(1, 0)));
}

#[test]
fn text_import_string_handles_unterminated_input() {
    let index = TextIndex::new("bring \"./u".to_string());
    let (start, end, value) = index.string_at(Position::new(0, 10)).unwrap();
    assert_eq!(value, "./u");
    assert_eq!(&index.source()[start..end], "\"./u");
}

#[test]
fn text_token_range_excludes_trivia() {
    let document = analysis("main.hard", "calc add(   ) => Int {\n}\n");
    let add = document
        .tokens
        .iter()
        .position(
            |token| matches!(&token.tok, hs_compiler::token::Tok::Ident(name) if name == "add"),
        )
        .unwrap();
    let range = document.text.token_range(&document.tokens, add);
    assert_eq!(range.start, Position::new(0, 5));
    assert_eq!(range.end, Position::new(0, 8));
}

#[test]
fn text_comment_range_multiline() {
    let document = analysis("main.hard", "/* one\n two */\ncalc x() => Int {}\n");
    assert!(document.comment_ranges.len() >= 2);
    assert!(document
        .semantic
        .iter()
        .any(|item| item.class == SemanticClass::Comment));
}

#[test]
fn text_fuzzy_exact_match() {
    assert_eq!(fuzzy_score("calculate", "calculate"), Some(20_000));
}

#[test]
fn text_fuzzy_prefix_match() {
    assert!(fuzzy_score("calculate", "calc").unwrap() > fuzzy_score("xcalc", "calc").unwrap());
}

#[test]
fn text_fuzzy_subsequence_match() {
    assert!(fuzzy_score("calculate", "clc").is_some());
}

#[test]
fn text_fuzzy_rejects_missing_character() {
    assert_eq!(fuzzy_score("calculate", "xyz"), None);
}

#[test]
fn analysis_indexes_function() {
    let document = analysis("main.hard", function_source());
    assert!(document.parse_diagnostics.is_empty());
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "add" && symbol.kind == DeclKind::Function));
}

#[test]
fn analysis_indexes_parameters() {
    let document = analysis("main.hard", function_source());
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "a" && symbol.kind == DeclKind::Parameter));
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "b" && symbol.kind == DeclKind::Parameter));
}

#[test]
fn analysis_indexes_local_variable() {
    let document = analysis("main.hard", function_source());
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "total" && symbol.local));
}

#[test]
fn analysis_indexes_model() {
    let document = analysis("main.hard", "model User = users [\n    id => Int #id,\n]\n");
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "User" && symbol.kind == DeclKind::Model));
}

#[test]
fn analysis_indexes_model_field() {
    let document = analysis("main.hard", "model User = users [\n    id => Int #id,\n]\n");
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "id" && symbol.kind == DeclKind::Field));
}

#[test]
fn analysis_indexes_route() {
    let document = analysis("main.hard", "GET \"/users\" :: {\n    <- { id: 1 }\n}\n");
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "GET /users" && symbol.kind == DeclKind::Route));
}

#[test]
fn analysis_indexes_route_parameter() {
    let document = analysis(
        "main.hard",
        "GET \"/users\" :: (id = Int) {\n    <- { id: 1 }\n}\n",
    );
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "id" && symbol.kind == DeclKind::Parameter));
}

#[test]
fn analysis_indexes_socket() {
    let document = analysis("main.hard", "socket \"/chat\" {\n    message :: {}\n}\n");
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "/chat" && symbol.kind == DeclKind::Socket));
}

#[test]
fn analysis_indexes_test() {
    let document = analysis(
        "main.hard",
        "test \"works\" {\n    expect true == true\n}\n",
    );
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "works" && symbol.kind == DeclKind::Test));
}

#[test]
fn analysis_indexes_global() {
    let document = analysis("main.hard", "value <- 1\n");
    assert!(document
        .symbols
        .iter()
        .any(|symbol| symbol.name == "value" && symbol.kind == DeclKind::Variable));
}

#[test]
fn analysis_indexes_import() {
    let document = analysis("main.hard", "bring \"./utils\"\n");
    assert_eq!(document.imports.len(), 1);
    assert_eq!(document.imports[0].path, "./utils");
}

#[test]
fn analysis_indexes_builtin_usage() {
    let document = analysis(
        "main.hard",
        "bring json\ncalc f(Str text) => dynamic {\n    <- json.parse(text)\n}\n",
    );
    assert!(document
        .occurrences
        .iter()
        .any(|item| item.id == "builtin:json:parse"));
}

#[test]
fn analysis_indexes_semantic_keyword() {
    let document = analysis("main.hard", function_source());
    assert!(document
        .semantic
        .iter()
        .any(|item| item.class == SemanticClass::Keyword));
}

#[test]
fn analysis_indexes_semantic_number() {
    let document = analysis("main.hard", "value <- 12\n");
    assert!(document
        .semantic
        .iter()
        .any(|item| item.class == SemanticClass::Number));
}

#[test]
fn analysis_indexes_semantic_string() {
    let document = analysis("main.hard", "value <- \"hello\"\n");
    assert!(document
        .semantic
        .iter()
        .any(|item| item.class == SemanticClass::String));
}

#[test]
fn analysis_indexes_semantic_decorator() {
    let document = analysis("main.hard", "model User = users [\n    id => Int #id,\n]\n");
    assert!(document
        .semantic
        .iter()
        .any(|item| item.class == SemanticClass::Decorator));
}

#[test]
fn analysis_reports_parse_errors() {
    let document = analysis("main.hard", "calc broken( => Int {}\n");
    assert!(document.has_parse_errors());
    assert!(!document.parse_diagnostics.is_empty());
}

#[test]
fn backend_starts_empty() {
    let backend = LspBackend::new();
    assert_eq!(backend.document_count(), 0);
}

#[test]
fn backend_publishes_clean_diagnostics() {
    let (backend, document_uri) = backend("main.hard", function_source());
    assert!(backend
        .diagnostics()
        .get(&document_uri)
        .is_some_and(|items| items.is_empty()));
}

#[test]
fn backend_publishes_parse_diagnostics() {
    let (backend, document_uri) = backend("main.hard", "calc broken( => Int {}\n");
    assert!(!backend.diagnostics().get(&document_uri).unwrap().is_empty());
}

#[test]
fn backend_completes_function_prefix() {
    let source = function_source();
    assert!(has_completion(source, 2, 9, "total"));
}

#[test]
fn backend_completes_empty_context() {
    let source = "calc add(Int a) => Int {}\n\n";
    assert!(has_completion(source, 2, 0, "add"));
}

#[test]
fn backend_completes_local_symbol() {
    let source = "calc add(Int a) => Int {\n    total <- a\n    \n}\n";
    assert!(has_completion(source, 3, 4, "total"));
}

#[test]
fn backend_completes_model_field() {
    let source = "model User = users [\n    id => Int #id,\n]\nvalue <- User.id\n";
    let cursor = source.find("User.id").unwrap() + "User.".len();
    let index = TextIndex::new(source.to_string());
    assert!(has_completion(
        source,
        index.position(cursor).line,
        index.position(cursor).character,
        "id"
    ));
}

#[test]
fn backend_completes_builtin_member() {
    let source = "bring json\ncalc f(Str text) => dynamic {\n    <- json.parse(text)\n}\n";
    let cursor = source.find("json.parse").unwrap() + "json.".len();
    let index = TextIndex::new(source.to_string());
    assert!(has_completion(
        source,
        index.position(cursor).line,
        index.position(cursor).character,
        "parse"
    ));
}

#[test]
fn backend_completes_builtin_module() {
    let source = "bring json\n\n";
    assert!(has_completion(source, 2, 0, "json"));
}

#[test]
fn backend_completes_import_path() {
    let mut backend = LspBackend::new();
    let main_uri = uri("main.hard");
    let other_uri = uri("utils.hard");
    backend.open_document(main_uri.clone(), "bring \"./u".to_string(), 1);
    backend.open_document(other_uri, "calc helper() => Int {}\n".to_string(), 1);
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: main_uri },
            position: Position::new(0, 10),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items = match backend.completion(&params) {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    assert!(items.iter().any(|item| item.label == "./utils"));
}

#[test]
fn backend_hovers_function() {
    let (backend, document_uri) = backend("main.hard", function_source());
    let hover = backend
        .hover(&document_uri, position(function_source(), "a +"))
        .unwrap();
    assert!(format!("{:?}", hover.contents).contains("parameter"));
}

#[test]
fn backend_hovers_builtin() {
    let source = "bring json\ncalc f(Str text) => dynamic {\n    <- json.parse(text)\n}\n";
    let (backend, document_uri) = backend("main.hard", source);
    let hover = backend
        .hover(&document_uri, position(source, "parse"))
        .unwrap();
    assert!(format!("{:?}", hover.contents).contains("json.parse"));
}

#[test]
fn backend_definition_finds_parameter() {
    let source = function_source();
    let (backend, document_uri) = backend("main.hard", source);
    let response = backend
        .definition(&document_uri, position(source, "a +"))
        .unwrap();
    match response {
        GotoDefinitionResponse::Scalar(location) => assert_eq!(location.uri, document_uri),
        GotoDefinitionResponse::Array(locations) => assert_eq!(locations[0].uri, document_uri),
        GotoDefinitionResponse::Link(_) => panic!(),
    }
}

#[test]
fn backend_references_include_declaration() {
    let source = function_source();
    let (backend, document_uri) = backend("main.hard", source);
    let locations = backend.references(&document_uri, position(source, "total"), true);
    assert_eq!(locations.len(), 2);
}

#[test]
fn backend_references_exclude_declaration() {
    let source = function_source();
    let (backend, document_uri) = backend("main.hard", source);
    let locations = backend.references(&document_uri, position(source, "total"), false);
    assert_eq!(locations.len(), 1);
}

#[test]
fn backend_prepare_rename_returns_range() {
    let source = function_source();
    let (backend, document_uri) = backend("main.hard", source);
    assert!(backend
        .prepare_rename(&document_uri, position(source, "total"))
        .is_some());
}

#[test]
fn backend_rename_returns_edits() {
    let source = function_source();
    let (backend, document_uri) = backend("main.hard", source);
    let result = backend.rename(&RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: document_uri },
            position: position(source, "total"),
        },
        new_name: "sum".to_string(),
        work_done_progress_params: Default::default(),
    });
    assert!(result.is_ok());
    assert_eq!(
        result
            .unwrap()
            .changes
            .unwrap()
            .values()
            .next()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn backend_rejects_invalid_rename() {
    let source = function_source();
    let (backend, document_uri) = backend("main.hard", source);
    let result = backend.rename(&RenameParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: document_uri },
            position: position(source, "total"),
        },
        new_name: "not-valid".to_string(),
        work_done_progress_params: Default::default(),
    });
    assert!(result.is_err());
}

#[test]
fn backend_document_symbols() {
    let (backend, document_uri) = backend("main.hard", function_source());
    let response = backend.document_symbols(&document_uri).unwrap();
    match response {
        DocumentSymbolResponse::Flat(symbols) => {
            assert!(symbols.iter().any(|symbol| symbol.name == "add"))
        }
        DocumentSymbolResponse::Nested(_) => panic!(),
    }
}

#[test]
fn backend_workspace_symbols() {
    let (backend, _) = backend("main.hard", function_source());
    assert!(backend
        .workspace_symbols("add")
        .iter()
        .any(|symbol| symbol.name == "add"));
}

#[test]
fn backend_semantic_tokens_are_encoded() {
    let (backend, document_uri) = backend("main.hard", function_source());
    let tokens = backend.semantic_tokens(&document_uri).unwrap();
    assert!(!tokens.data.is_empty());
}

#[test]
fn backend_formats_document() {
    let source = "calc add(Int a,Int b)=>Int {<-(a+b)}\n";
    let (backend, document_uri) = backend("main.hard", source);
    let edits = backend
        .format_document(&document_uri, &FormattingOptions::default())
        .unwrap();
    assert_eq!(edits.len(), 1);
}

#[test]
fn backend_range_format_rejects_outside_change() {
    let source = function_source();
    let (backend, document_uri) = backend("main.hard", source);
    assert!(backend
        .format_range(
            &document_uri,
            Range::new(Position::new(0, 0), Position::new(0, 0)),
            &FormattingOptions::default()
        )
        .is_none());
}

#[test]
fn backend_signature_help_for_second_parameter() {
    let source = "calc add(Int a, Int b) => Int {}\nvalue <- add(1, )\n";
    let (backend, document_uri) = backend("main.hard", source);
    let cursor = source.find("add(1, )").unwrap() + "add(1, ".len();
    let index = TextIndex::new(source.to_string());
    let help = backend
        .signature_help(&document_uri, index.position(cursor))
        .unwrap();
    assert_eq!(help.active_parameter, Some(1));
}

#[test]
fn backend_code_action_explains_diagnostic() {
    let source = "value <- missing\n";
    let (backend, document_uri) = backend("main.hard", source);
    let diagnostic = backend.diagnostics().get(&document_uri).unwrap()[0].clone();
    let actions = backend.code_actions(&CodeActionParams {
        text_document: TextDocumentIdentifier { uri: document_uri },
        range: diagnostic.range,
        context: CodeActionContext {
            diagnostics: vec![diagnostic],
            only: None,
            trigger_kind: None,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    assert!(actions
        .iter()
        .any(|action| format!("{action:?}").contains("Explain")));
}

#[test]
fn backend_explain_catalog_code() {
    let (backend, _) = backend("main.hard", function_source());
    assert!(backend.explain("HS0002").is_some());
    assert!(backend.explain("not-a-code").is_none());
}

#[test]
fn backend_rejects_stale_document_version() {
    let (mut backend, document_uri) = backend("main.hard", function_source());
    assert!(!backend.update_document(&document_uri, function_source().replace("add", "old"), 0));
    assert!(backend.update_document(&document_uri, function_source().replace("add", "new"), 2));
}

#[test]
fn backend_save_preserves_version() {
    let (mut backend, document_uri) = backend("main.hard", function_source());
    backend.save_document(&document_uri, function_source().replace("add", "saved"));
    assert!(backend
        .document(&document_uri)
        .unwrap()
        .symbols
        .iter()
        .any(|symbol| symbol.name == "saved"));
}

#[test]
fn backend_cross_file_symbols_have_distinct_ids() {
    let mut backend = LspBackend::new();
    let first = uri("first.hard");
    let second = uri("second.hard");
    backend.open_document(first, "calc same() => Int {}\n".to_string(), 1);
    backend.open_document(second, "calc same() => Int {}\n".to_string(), 1);
    let symbols = backend.workspace_symbols("same");
    assert_eq!(symbols.len(), 2);
}

#[test]
fn backend_cross_file_completion_includes_symbols() {
    let mut backend = LspBackend::new();
    let first = uri("first.hard");
    let second = uri("second.hard");
    backend.open_document(first, "calc helper() => Int {}\n".to_string(), 1);
    backend.open_document(second.clone(), "\n".to_string(), 1);
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: second },
            position: Position::new(1, 0),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items = match backend.completion(&params) {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    assert!(items.iter().any(|item| item.label == "helper"));
}

#[test]
fn backend_close_document_clears_open_state() {
    let (mut backend, document_uri) = backend("main.hard", function_source());
    backend.close_document(&document_uri);
    assert!(!backend
        .open_documents()
        .any(|(candidate, _)| candidate == &document_uri));
}

#[test]
fn text_full_range_handles_empty_document() {
    let index = TextIndex::new(String::new());
    assert_eq!(
        index.full_range(),
        Range::new(Position::new(0, 0), Position::new(0, 0))
    );
}

#[test]
fn text_line_text_strips_line_endings() {
    let index = TextIndex::new("a\r\nb\n".to_string());
    assert_eq!(index.line_text(0), "a");
    assert_eq!(index.line_text(1), "b");
}

#[test]
fn text_common_change_range_handles_unicode() {
    let (start, old_end, new_end) = hs_lsp::text::common_change_range("a😀b", "a😀c").unwrap();
    assert_eq!(start, 5);
    assert_eq!(old_end, 6);
    assert_eq!(new_end, 6);
}

#[test]
fn text_is_identifier_accepts_unicode() {
    assert!(hs_lsp::text::is_identifier("café"));
    assert!(!hs_lsp::text::is_identifier("1bad"));
}

#[test]
fn server_capabilities_advertise_utf16() {
    let capabilities = hs_lsp::server_capabilities();
    assert_eq!(
        capabilities.position_encoding,
        Some(PositionEncodingKind::UTF16)
    );
}

#[test]
fn server_semantic_legend_has_all_classes() {
    assert_eq!(hs_lsp::server::semantic_class_count(), 14);
    assert!(hs_lsp::server::known_semantic_class("function").is_some());
    assert!(hs_lsp::server::known_semantic_class("unknown").is_none());
}

#[test]
fn analysis_symbol_ids_are_unique_for_documents() {
    let first = analysis("first.hard", "calc same() => Int {}\n");
    let second = analysis("second.hard", "calc same() => Int {}\n");
    assert_ne!(first.symbols[0].id, second.symbols[0].id);
}

#[test]
fn analysis_model_field_container_is_stable() {
    let document = analysis("main.hard", "model User = users [\n    id => Int #id,\n]\n");
    let field = document
        .symbols
        .iter()
        .find(|symbol| symbol.kind == DeclKind::Field)
        .unwrap();
    assert_eq!(field.container.as_deref(), Some("model:User"));
}

#[test]
fn analysis_builtin_hover_metadata_exists() {
    let document = analysis("main.hard", "bring json\nvalue <- json.parse(\"x\")\n");
    assert!(document
        .occurrences
        .iter()
        .any(|item| item.documentation.contains("JSON")));
}

#[test]
fn backend_builtin_member_has_import_edit() {
    let source = "calc f(Str text) => dynamic {\n    <- json.parse(text)\n}\n";
    let cursor = source.find("json.parse").unwrap() + "json.".len();
    let index = TextIndex::new(source.to_string());
    let items = completion(
        source,
        index.position(cursor).line,
        index.position(cursor).character,
    );
    let item = items.iter().find(|item| item.label == "parse").unwrap();
    assert!(item.additional_text_edits.is_some());
}

#[test]
fn backend_import_edit_uses_local_path() {
    let mut backend = LspBackend::new();
    let main_uri = uri("main.hard");
    let other_uri = uri("nested/other.hard");
    backend.open_document(main_uri.clone(), "\n".to_string(), 1);
    backend.open_document(other_uri, "calc helper() => Int {}\n".to_string(), 1);
    let params = CompletionParams {
        text_document_position: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: main_uri },
            position: Position::new(1, 0),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
        context: None,
    };
    let items = match backend.completion(&params) {
        CompletionResponse::Array(items) => items,
        CompletionResponse::List(list) => list.items,
    };
    let item = items.iter().find(|item| item.label == "helper").unwrap();
    let edit = item.additional_text_edits.as_ref().unwrap();
    assert!(edit[0].new_text.contains("bring"));
}

#[test]
fn backend_document_count_tracks_open_and_closed() {
    let (mut backend, document_uri) = backend("main.hard", function_source());
    assert_eq!(backend.document_count(), 1);
    backend.close_document(&document_uri);
    assert_eq!(backend.document_count(), 1);
}

#[test]
fn backend_empty_definition_is_none() {
    let (backend, document_uri) = backend("main.hard", "\n");
    assert!(backend
        .definition(&document_uri, Position::new(0, 0))
        .is_none());
}

#[test]
fn backend_empty_hover_is_none() {
    let (backend, document_uri) = backend("main.hard", "\n");
    assert!(backend.hover(&document_uri, Position::new(0, 0)).is_none());
}

#[test]
fn backend_empty_references_are_empty() {
    let (backend, document_uri) = backend("main.hard", "\n");
    assert!(backend
        .references(&document_uri, Position::new(0, 0), true)
        .is_empty());
}

#[test]
fn backend_empty_semantic_tokens_are_empty() {
    let (backend, document_uri) = backend("main.hard", "\n");
    assert!(backend
        .semantic_tokens(&document_uri)
        .unwrap()
        .data
        .is_empty());
}

#[test]
fn backend_empty_format_is_empty_edit() {
    let (backend, document_uri) = backend("main.hard", "\n");
    assert_eq!(
        backend.format_document(&document_uri, &FormattingOptions::default()),
        Some(Vec::new())
    );
}

#[test]
fn backend_empty_document_symbols_are_empty() {
    let (backend, document_uri) = backend("main.hard", "\n");
    match backend.document_symbols(&document_uri).unwrap() {
        DocumentSymbolResponse::Flat(symbols) => assert!(symbols.is_empty()),
        DocumentSymbolResponse::Nested(_) => panic!(),
    }
}

#[test]
fn backend_open_document_replaces_previous_analysis() {
    let (mut backend, document_uri) = backend("main.hard", "calc old() => Int {}\n");
    backend.open_document(
        document_uri.clone(),
        "calc new() => Int {}\n".to_string(),
        2,
    );
    assert!(backend
        .document(&document_uri)
        .unwrap()
        .symbols
        .iter()
        .any(|symbol| symbol.name == "new"));
    assert!(!backend
        .document(&document_uri)
        .unwrap()
        .symbols
        .iter()
        .any(|symbol| symbol.name == "old"));
}

#[test]
fn backend_index_root_is_deterministic() {
    let mut backend = LspBackend::new();
    let root = PathBuf::from("/tmp/hardscript-lsp-tests");
    backend.add_workspace_root(root.clone());
    let first = backend.add_workspace_root(root);
    assert!(first <= backend.document_count());
}

#[test]
fn backend_remove_root_keeps_open_document() {
    let (mut backend, document_uri) = backend("main.hard", function_source());
    let root = PathBuf::from("/tmp/hardscript-lsp-tests");
    backend.add_workspace_root(root.clone());
    backend.remove_workspace_root(&root);
    assert!(backend
        .open_documents()
        .any(|(uri, _)| uri == &document_uri));
}

#[test]
fn backend_source_file_filter_is_scoped() {
    let mut backend = LspBackend::new();
    backend.add_workspace_root(PathBuf::from("/tmp/hardscript-lsp-tests"));
    assert!(backend.document_count() <= 1);
}

#[test]
fn backend_location_fallback_keeps_open_document() {
    let (backend, document_uri) = backend("main.hard", "value <- 1\n");
    assert!(backend.diagnostics().contains_key(&document_uri));
}

#[test]
fn backend_code_action_without_diagnostics_is_empty() {
    let (backend, document_uri) = backend("main.hard", function_source());
    let actions = backend.code_actions(&CodeActionParams {
        text_document: TextDocumentIdentifier { uri: document_uri },
        range: Range::default(),
        context: CodeActionContext::default(),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    });
    assert!(actions.is_empty());
}

#[test]
fn backend_highlights_same_symbol() {
    let source = function_source();
    let (backend, document_uri) = backend("main.hard", source);
    assert_eq!(
        backend
            .document_highlights(&document_uri, position(source, "total"))
            .len(),
        2
    );
}

#[test]
fn backend_highlights_unknown_position_are_empty() {
    let (backend, document_uri) = backend("main.hard", function_source());
    assert!(backend
        .document_highlights(&document_uri, Position::new(99, 0))
        .is_empty());
}

#[test]
fn backend_prepare_rename_rejects_builtin() {
    let source = "bring json\nvalue <- json.parse(\"x\")\n";
    let (backend, document_uri) = backend("main.hard", source);
    assert!(backend
        .prepare_rename(&document_uri, position(source, "parse"))
        .is_none());
}

#[test]
fn backend_prepare_rename_rejects_unknown() {
    let (backend, document_uri) = backend("main.hard", "\n");
    assert!(backend
        .prepare_rename(&document_uri, Position::new(0, 0))
        .is_none());
}

#[test]
fn backend_explain_accepts_lowercase_code() {
    let (backend, _) = backend("main.hard", "\n");
    assert!(backend.explain("hs0002").is_some());
}

#[test]
fn backend_explain_rejects_out_of_range_code() {
    let (backend, _) = backend("main.hard", "\n");
    assert!(backend.explain("HS9999").is_none());
}

#[test]
fn backend_signature_help_unknown_call_is_none() {
    let source = "value <- unknown(1, )\n";
    let (backend, document_uri) = backend("main.hard", source);
    let cursor = source.find("unknown(1, )").unwrap() + "unknown(1, ".len();
    let index = TextIndex::new(source.to_string());
    assert!(backend
        .signature_help(&document_uri, index.position(cursor))
        .is_none());
}

#[test]
fn backend_signature_help_first_parameter() {
    let source = "calc add(Int a, Int b) => Int {}\nvalue <- add()\n";
    let (backend, document_uri) = backend("main.hard", source);
    let cursor = source.find("add()").unwrap() + "add(".len();
    let index = TextIndex::new(source.to_string());
    let help = backend
        .signature_help(&document_uri, index.position(cursor))
        .unwrap();
    assert_eq!(help.active_parameter, Some(0));
}

#[test]
fn backend_signature_help_builtin() {
    let source = "bring json\nvalue <- json.parse()\n";
    let (backend, document_uri) = backend("main.hard", source);
    let cursor = source.find("parse()").unwrap() + "parse(".len();
    let index = TextIndex::new(source.to_string());
    let help = backend
        .signature_help(&document_uri, index.position(cursor))
        .unwrap();
    assert_eq!(help.active_parameter, Some(0));
}

#[test]
fn backend_semantic_tokens_non_ascii_length() {
    let source = "value <- \"😀\"\n";
    let (backend, document_uri) = backend("main.hard", source);
    let tokens = backend.semantic_tokens(&document_uri).unwrap();
    assert!(tokens.data.iter().any(|token| token.length == 2));
}

#[test]
fn backend_definition_import_string() {
    let mut backend = LspBackend::new();
    let main_uri = uri("main.hard");
    let other_uri = uri("utils.hard");
    backend.open_document(main_uri.clone(), "bring \"./utils\"\n".to_string(), 1);
    backend.open_document(other_uri, "calc helper() => Int {}\n".to_string(), 1);
    let source = "bring \"./utils\"\n";
    let index = TextIndex::new(source.to_string());
    let response = backend
        .definition(&main_uri, index.position(source.find("utils").unwrap()))
        .unwrap();
    assert!(matches!(response, GotoDefinitionResponse::Scalar(_)));
}

#[test]
fn backend_hover_import_string() {
    let mut backend = LspBackend::new();
    let main_uri = uri("main.hard");
    let other_uri = uri("utils.hard");
    backend.open_document(main_uri.clone(), "bring \"./utils\"\n".to_string(), 1);
    backend.open_document(other_uri, "calc helper() => Int {}\n".to_string(), 1);
    let source = "bring \"./utils\"\n";
    let index = TextIndex::new(source.to_string());
    assert!(backend
        .hover(&main_uri, index.position(source.find("utils").unwrap()))
        .is_some());
}

#[test]
fn backend_completion_has_snippet_for_function_line() {
    let source = "calc \n";
    assert!(has_completion(source, 0, 5, "calc"));
}

#[test]
fn backend_completion_has_model_snippet() {
    let source = "model \n";
    assert!(has_completion(source, 0, 6, "model"));
}

#[test]
fn backend_completion_has_route_snippet() {
    let source = "GET \n";
    assert!(has_completion(source, 0, 4, "GET"));
}

#[test]
fn backend_workspace_symbol_query_is_case_insensitive() {
    let (backend, _) = backend("main.hard", function_source());
    assert!(backend
        .workspace_symbols("ADD")
        .iter()
        .any(|symbol| symbol.name == "add"));
}

#[test]
fn backend_workspace_symbol_query_empty_returns_exports() {
    let (backend, _) = backend("main.hard", function_source());
    assert!(!backend.workspace_symbols("").is_empty());
}

#[test]
fn backend_document_symbols_exclude_imports() {
    let (backend, document_uri) = backend("main.hard", "bring \"./utils\"\n");
    match backend.document_symbols(&document_uri).unwrap() {
        DocumentSymbolResponse::Flat(symbols) => assert!(symbols.is_empty()),
        DocumentSymbolResponse::Nested(_) => panic!(),
    }
}

#[test]
fn backend_format_invalid_document_is_none() {
    let (backend, document_uri) = backend("main.hard", "calc broken( => Int {}\n");
    assert!(backend
        .format_document(&document_uri, &FormattingOptions::default())
        .is_none());
}

#[test]
fn backend_range_format_full_document_returns_edit() {
    let source = "calc add(Int a,Int b)=>Int {<-(a+b)}\n";
    let (backend, document_uri) = backend("main.hard", source);
    let range = Range::new(Position::new(0, 0), Position::new(1, 0));
    assert!(backend
        .format_range(&document_uri, range, &FormattingOptions::default())
        .is_some());
}

#[test]
fn backend_save_unknown_document_is_safe() {
    let mut backend = LspBackend::new();
    let document_uri = uri("unknown.hard");
    backend.save_document(&document_uri, "value <- 1\n".to_string());
    assert!(backend.document(&document_uri).is_some());
}

const HANDLERS_SOURCE: &str = "/// Greets the caller\ncalc greeting() => Str { <- \"hi\" }\n";
const MAIN_SOURCE: &str =
    "bring \"./handlers\"\nbring http\napp @3032\n\nGET \"/\" :: { <- { greeting: greeting() } }\n";

struct Workspace {
    _directory: tempfile::TempDir,
    backend: Backend,
    main: Url,
    handlers: Url,
}

fn cross_file_workspace() -> Workspace {
    let directory = tempfile::tempdir().expect("temp dir");
    let root = directory.path();
    std::fs::write(root.join("handlers.hard"), HANDLERS_SOURCE).expect("write handlers");
    std::fs::write(root.join("main.hard"), MAIN_SOURCE).expect("write main");
    let mut backend = Backend::new();
    backend.add_workspace_root(root.to_path_buf());
    let main = Url::from_file_path(root.join("main.hard")).expect("main uri");
    let handlers = Url::from_file_path(root.join("handlers.hard")).expect("handlers uri");
    backend.open_document(main.clone(), MAIN_SOURCE.to_string(), 1);
    Workspace {
        _directory: directory,
        backend,
        main,
        handlers,
    }
}

#[test]
fn backend_indexes_imported_file_symbols() {
    let workspace = cross_file_workspace();
    let names: Vec<String> = workspace
        .backend
        .workspace_symbols("greeting")
        .into_iter()
        .map(|symbol| symbol.name)
        .collect();
    assert_eq!(names, vec!["greeting".to_string()]);
}

#[test]
fn backend_definition_crosses_file_boundary() {
    let workspace = cross_file_workspace();
    let target = position(MAIN_SOURCE, "greeting()");
    let response = workspace
        .backend
        .definition(&workspace.main, target)
        .expect("definition");
    let location = match response {
        GotoDefinitionResponse::Scalar(location) => location,
        GotoDefinitionResponse::Link(_) | GotoDefinitionResponse::Array(_) => {
            panic!("unexpected shape")
        }
    };
    assert_eq!(location.uri, workspace.handlers);
    assert_eq!(location.range.start.line, 1);
}

#[test]
fn backend_references_cross_files_and_skip_object_keys() {
    let workspace = cross_file_workspace();
    let target = position(MAIN_SOURCE, "greeting()");
    let locations = workspace.backend.references(&workspace.main, target, true);
    let mut files: Vec<String> = locations
        .iter()
        .map(|location| location.uri.to_string())
        .collect();
    files.sort();
    let expected = vec![workspace.handlers.to_string(), workspace.main.to_string()];
    files.sort();
    assert_eq!(files, expected);
    let call_site = locations
        .iter()
        .find(|location| location.uri == workspace.main)
        .expect("call site");
    assert_eq!(call_site.range.start.line, 4);
    assert_eq!(call_site.range.start.character, 28);
}

#[test]
fn backend_rename_edits_both_files() {
    let workspace = cross_file_workspace();
    let target = position(MAIN_SOURCE, "greeting()");
    let edits = workspace
        .backend
        .rename(&RenameParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier {
                    uri: workspace.main.clone(),
                },
                position: target,
            },
            new_name: "salutation".to_string(),
            work_done_progress_params: Default::default(),
        })
        .expect("rename")
        .changes
        .expect("changes");
    assert_eq!(edits.len(), 2);
    assert_eq!(edits[&workspace.main].len(), 1);
    assert_eq!(edits[&workspace.handlers].len(), 1);
    assert_eq!(edits[&workspace.main][0].new_text, "salutation");
}

#[test]
fn backend_references_reports_declaration_only_when_requested() {
    let workspace = cross_file_workspace();
    let declaration = position(HANDLERS_SOURCE, "greeting");
    let locations = workspace
        .backend
        .references(&workspace.handlers, declaration, true);
    assert_eq!(locations.len(), 2);
    let without_declaration = workspace
        .backend
        .references(&workspace.handlers, declaration, false);
    assert_eq!(without_declaration.len(), 1);
    assert_eq!(without_declaration[0].uri, workspace.main);
}
