use hs_compiler::{frontend, render_all};
use hs_lsp::server::BackendServer;
use std::env;
use std::io::{self, Read};
use std::process;
use tower_lsp::{LspService, Server};

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--check") {
        check(&arguments[1..]);
        return;
    }
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(BackendServer::service);
    Server::new(stdin, stdout, socket).serve(service).await;
}

fn check(arguments: &[String]) {
    if arguments.is_empty() {
        let mut source = String::new();
        if io::stdin().read_to_string(&mut source).is_err() {
            eprintln!("hs-lsp: could not read stdin");
            process::exit(1);
        }
        check_source(&source, "<stdin>");
        return;
    }
    match std::fs::read_to_string(&arguments[0]) {
        Ok(source) => check_source(&source, &arguments[0]),
        Err(error) => {
            eprintln!("hs-lsp: {}: {error}", arguments[0]);
            process::exit(1);
        }
    }
}

fn check_source(source: &str, path: &str) {
    match frontend(source, path) {
        Ok(_) => println!("ok {path}: no diagnostics"),
        Err(diagnostics) => {
            eprint!("{}", render_all(&diagnostics));
            process::exit(1);
        }
    }
}
