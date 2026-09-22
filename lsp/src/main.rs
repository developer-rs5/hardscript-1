use hs_compiler::{frontend, render_all, Diag};
use std::env;
use std::io::{self, Read};
use std::process;

/// `hs-lsp <file>` — checks a HardScript source file and prints diagnostics.
/// With no arguments it reads source from stdin.
fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 {
        check_file(&args[1]);
        return;
    }
    let mut src = String::new();
    if io::stdin().read_to_string(&mut src).is_err() {
        eprintln!("hs-lsp: could not read stdin");
        process::exit(1);
    }
    check_str(&src, "<stdin>");
}

fn check_file(path: &str) {
    match std::fs::read_to_string(path) {
        Ok(src) => check_str(&src, path),
        Err(e) => eprintln!("hs-lsp: {path}: {e}"),
    }
}

fn check_str(src: &str, path: &str) {
    match frontend(src, path) {
        Ok(_) => println!("ok {path}: no diagnostics"),
        Err(diags) => {
            report(&diags);
            process::exit(1);
        }
    }
}

fn report(diags: &[Diag]) {
    eprint!("{}", render_all(diags));
}