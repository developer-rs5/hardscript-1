//! hs-qa-harness — in-process torture runner for the HardScript compiler.
//!
//! Runs the compiler front end / full pipeline over many files or generated
//! programs in a single process, isolating panics with catch_unwind so that
//! one bad input cannot stop a batch. Infinite/hanging inputs are handled
//! externally by running each batch under `timeout`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::exit;

use hs_compiler::fmt;
use hs_compiler::{compile_to_cpp, frontend, typecheck, Diag, ErrorKind};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % (n as u64)) as usize
    }
    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}

fn collect_paths(arg: &str) -> Vec<PathBuf> {
    let p = PathBuf::from(arg);
    if p.is_dir() {
        let mut out = Vec::new();
        let mut stack = vec![p];
        while let Some(d) = stack.pop() {
            let mut entries: Vec<_> = match std::fs::read_dir(&d) {
                Ok(e) => e.filter_map(|e| e.ok()).map(|e| e.path()).collect(),
                Err(_) => continue,
            };
            entries.sort();
            for e in entries {
                if e.is_dir() {
                    stack.push(e);
                } else if e.extension().map(|x| x == "hard").unwrap_or(false) {
                    out.push(e);
                }
            }
        }
        out
    } else if p.extension().map(|x| x == "hard").unwrap_or(false) {
        vec![p]
    } else {
        vec![]
    }
}

fn panic_msg(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else {
        format!("{:?}", payload.type_id())
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: qa-harness <parse|typecheck|full|fuzz|fmtcheck|timing> [options]\n\
             \n\
             parse <dir|file>\n\
             typecheck <dir|file>\n\
             full <dir|file>\n\
             fuzz <count> <seed> [--out dir]\n\
             fmtcheck <dir|file>\n\
             timing <dir|file>"
        );
        exit(2);
    }

    let cmd = args.remove(0);
    match cmd.as_str() {
        "parse" => run(&args, Stage::Parse),
        "typecheck" => run(&args, Stage::Typecheck),
        "full" => run(&args, Stage::Full),
        "fmtcheck" => run(&args, Stage::FmtCheck),
        "timing" => run(&args, Stage::Timing),
        "fuzz" => fuzz(&args),
        other => {
            eprintln!("unknown command `{other}`");
            exit(2);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Parse,
    Typecheck,
    Full,
    FmtCheck,
    Timing,
}

fn run(args: &[String], stage: Stage) {
    if args.is_empty() {
        eprintln!("missing <dir|file>");
        exit(2);
    }
    let files = collect_paths(&args[0]);
    if files.is_empty() {
        eprintln!("no .hard files found at {}", args[0]);
        exit(2);
    }

    let mut n_ok = 0usize;
    let mut n_diag = 0usize;
    let mut n_panic = 0usize;
    let mut n_unstable = 0usize;
    let mut total_us: u128 = 0;

    for (i, f) in files.iter().enumerate() {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let out = catch_unwind(AssertUnwindSafe(|| {
            let t0 = std::time::Instant::now();
            let r = match stage {
                Stage::Parse => frontend(&src, "<qa>").map(|_| String::new()),
                Stage::Typecheck => frontend(&src, "<qa>").and_then(|p| {
                    let d = typecheck::check(&p);
                    if d.is_empty() {
                        Ok(String::new())
                    } else {
                        Err(d)
                    }
                }),
                Stage::Full => compile_to_cpp(&src, "<qa>"),
                Stage::FmtCheck => fmtcheck_src(&src),
                Stage::Timing => compile_to_cpp(&src, "<qa>"),
            };
            let us = t0.elapsed().as_micros();
            total_us += us;
            r
        }));
        match out {
            Ok(Ok(_)) => n_ok += 1,
            Ok(Err(ds)) => {
                if stage == Stage::FmtCheck {
                    n_unstable += 1;
                    eprintln!("UNSTABLE [{}]: {}", f.display(), ds.first().map(|d| d.message.clone()).unwrap_or_default());
                } else {
                    n_diag += 1;
                }
            }
            Err(p) => {
                n_panic += 1;
                eprintln!("PANIC [{}] {}", f.display(), panic_msg(&p));
            }
        }
        if i % 100 == 99 {
            eprintln!(".. {}/{}", i + 1, files.len());
        }
    }

    match stage {
        Stage::Timing => println!(
            "timing: {} files, {} ok, {} diag, {} panics, {} ms total",
            files.len(),
            n_ok,
            n_diag,
            n_panic,
            total_us / 1000
        ),
        _ => println!(
            "SUMMARY: files={} ok={} diag_instances={} panics={} unstable={}",
            files.len(),
            n_ok,
            n_diag,
            n_panic,
            n_unstable
        ),
    }
    if n_panic > 0 {
        exit(1);
    }
}

/// formatter must (a) produce parseable output and (b) be idempotent.
fn fmtcheck_src(src: &str) -> Result<String, Vec<Diag>> {
    let prog = frontend(src, "<qa>").map_err(|_| vec![])?;
    let once = fmt::format(&prog);
    let prog2 = frontend(&once, "<qa>")?;
    let twice = fmt::format(&prog2);
    if twice != once {
        return Err(vec![Diag::new_nospan(
            ErrorKind::Type,
            format!("formatter not idempotent:\n--- once ---\n{once}\n--- twice ---\n{twice}"),
        )]);
    }
    Ok(once)
}

fn fuzz(args: &[String]) {
    if args.len() < 2 {
        eprintln!("fuzz needs <count> <seed>");
        exit(2);
    }
    let count: usize = args[0].parse().expect("count");
    let seed: u64 = args[1].parse().expect("seed");
    let outdir = args
        .iter()
        .position(|a| a == "--out")
        .and_then(|i| args.get(i + 1).cloned());

    let mut rng = Rng(seed);
    let mut n_ok = 0usize;
    let mut n_diag = 0usize;
    let mut n_panic = 0usize;

    for i in 0..count {
        let src = gen_program(&mut rng);
        let r = catch_unwind(AssertUnwindSafe(|| compile_to_cpp(&src, "<fuzz>")));
        match r {
            Ok(Ok(_)) => n_ok += 1,
            Ok(Err(ds)) => n_diag += ds.len(),
            Err(p) => {
                n_panic += 1;
                let msg = panic_msg(&p);
                eprintln!("PANIC[{i}] seed={seed}: {msg}");
                if let Some(dir) = &outdir {
                    let _ = std::fs::create_dir_all(dir);
                    let _ = std::fs::write(PathBuf::from(dir).join(format!("panic-{i}.hard")), &src);
                }
            }
        }
        if i % 2000 == 1999 {
            eprintln!(".. fuzz {}/{}", i + 1, count);
        }
    }
    println!(
        "FUZZ SUMMARY: n={count} ok={n_ok} diag_instances={n_diag} panics={n_panic}"
    );
    if n_panic > 0 {
        exit(1);
    }
}

// ---------------------------------------------------------------------------
// Random program generator (grammar-biased chaos; exercises parser + typecheck
// + codegen, occasional fully valid programs)
// ---------------------------------------------------------------------------

const TOKENS: &[&str] = &[
    "GET", "POST", "PUT", "DELETE", "PATCH", "socket", "model", "calc", "test",
    "bring", "app", "connect", "message", "disconnect", "before", "loop", "pick",
    "async", "wait", "race", "expect", "RUN",
];

const WRITE_TOKENS: &[&str] = &[
    "[", "]", "{", "}", "(", ")", ",", ":", "::", "::=", "<-", "=>", ".", "#", "?",
    "*", "+", "-", "/", "%", "!", "<", "<=", ">", ">=", "==", "!=", "&&", "||", "=",
    "...",
];

const IDENT_PREFIXES: &[&str] = &[
    "a", "b", "user", "id", "name", "x", "req", "res", "db", "store", "tok", "msg",
    "room", "hélène", "λ", "δοκιμή", "变量",
];

const MODULES: &[&str] = &[
    "crypto", "fs", "env", "json", "jwt", "time", "runtime", "websocket", "postgres",
    "http",
];

const MOD_FNS: &[&str] = &[
    "sha256", "sha1", "md5", "base64", "uuid", "random_hex", "token", "parse",
    "stringify", "keys", "get", "read", "write", "exists", "list", "size", "sign",
    "verify", "now", "iso", "sleep", "args", "argc", "argv", "print", "pid",
    "hostname", "platform", "cpus", "broadcast", "broadcast_room", "reply", "join",
    "leave", "self", "connect", "query",
];

fn ident(rng: &mut Rng) -> String {
    let p = rng.pick(IDENT_PREFIXES);
    if rng.below(3) == 0 {
        format!("{}{}", p, rng.below(1000))
    } else {
        p.to_string()
    }
}

fn gen_string(rng: &mut Rng) -> String {
    let n = rng.below(14);
    let pool: &[u8] = b"abcXYZ019 .*_-/\\:{},'\n\x01\xff";
    let mut s = String::with_capacity(n + 2);
    s.push('"');
    for _ in 0..n {
        s.push(*rng.pick(pool) as char);
    }
    s.push('"');
    s
}

fn gen_value(rng: &mut Rng) -> String {
    match rng.below(6) {
        0 => rng.below(100000).to_string(),
        1 => format!("{}.{}", rng.below(100), rng.below(100)),
        2 => gen_string(rng),
        3 => {
            if rng.below(2) == 0 {
                "{ }".to_string()
            } else {
                let mut s = String::from("{ ");
                let n = rng.below(4);
                for _ in 0..n {
                    let k = gen_string(rng);
                    let v = gen_value(rng);
                    s.push_str(&format!("{k}: {v}, "));
                }
                s.push('}');
                s
            }
        }
        4 => {
            let mut s = String::from("[ ");
            let n = rng.below(4);
            for _ in 0..n {
                let v = gen_value(rng);
                s.push_str(&format!("{v}, "));
            }
            s.push(']');
            s
        }
        _ => ident(rng),
    }
}

fn gen_expr(rng: &mut Rng) -> String {
    match rng.below(10) {
        0..=4 => gen_value(rng),
        5 => {
            let l = gen_expr(rng);
            let op = rng.pick(&[
                "+", "-", "*", "/", "%", "==", "!=", "<", "<=", ">", ">=", "&&", "||",
            ]);
            let r = gen_expr(rng);
            format!("{l} {op} {r}")
        }
        6 => format!("!{}", gen_expr(rng)),
        7 => {
            let m = rng.pick(MODULES);
            let f = rng.pick(MOD_FNS);
            let argn = if *f == "self" || *f == "leave" || *f == "args" { 0 } else { rng.below(3) };
            let mut call = format!("{m}.{f}(");
            for _ in 0..argn {
                let a = gen_expr(rng);
                call.push_str(&format!("{a}, "));
            }
            call.push(')');
            call
        }
        8 => format!("?({})", gen_expr(rng)),
        _ => ident(rng),
    }
}

fn gen_body(rng: &mut Rng) -> String {
    let mut s = String::from("{\n");
    let n = 1 + rng.below(4);
    for _ in 0..n {
        match rng.below(4) {
            0 => {
                let nm = ident(rng);
                let e = gen_expr(rng);
                s.push_str(&format!("        {nm} <- {e}\n"));
            }
            1 => s.push_str(&format!(
                "        if {} {{ __ <- 1 }}\n",
                rng.pick(&["true", "false", "1 == 1"])
            )),
            2 => {
                let m = rng.pick(MODULES);
                if *m == "websocket" {
                    let e = gen_expr(rng);
                    s.push_str(&format!("        {e}\n"));
                } else {
                    let nm = ident(rng);
                    let e = gen_expr(rng);
                    s.push_str(&format!("        {nm} <- {e}\n"));
                }
            }
            _ => {
                let e = gen_expr(rng);
                s.push_str(&format!("        {e}\n"));
            }
        }
    }
    s.push('}');
    s
}

fn gen_program(rng: &mut Rng) -> String {
    // ~1/3 fully chaotic token soup (early parse errors, deep recursion)
    if rng.below(3) == 0 {
        let n = 5 + rng.below(120);
        return gen_tokens(rng, n);
    }

    let mut s = String::from("bring http\n");
    if rng.below(2) == 0 {
        s.push_str("bring stdio\n");
    }
    if rng.below(2) == 0 {
        s.push_str(&format!("app @{}\n", 3000 + rng.below(100)));
    }

    let lines = 1 + rng.below(14);
    for idx in 0..lines {
        match rng.below(9) {
            0 => s.push_str(&format!(
                "GET \"/rt{}\" :: {{ __ <- {{\"i\": {}, \"s\": {}}} }}\n",
                idx,
                idx,
                gen_string(rng)
            )),
            1 => s.push_str(&format!(
                "{} \"/p{}\" :: (id = Str) {{ __ <- id }}\n",
                rng.pick(&["GET", "PUT", "DELETE", "PATCH"]),
                idx
            )),
            2 => s.push_str(&format!(
                "POST \"/b{}\" :: (data) {{ __ <- data }}\n",
                idx
            )),
            3 => s.push_str(&format!(
                "test \"t{idx}\" {{ expect 1 == 1 }}\n"
            )),
            4 => s.push_str(&format!(
                "calc f{idx}(x) => Int {{ <- x + {} }}\n",
                idx
            )),
            5 => s.push_str(&format!("model M{idx} = m{} [\n", idx)),
            6 => s.push_str(&format!(
                "ws: socket \"/ws{}\" {{\n    message {{ __ <- websocket.reply(\"pong\") }}\n}}\n",
                idx
            )),
            7 => s.push_str(&format!("mid{idx}: before {{ __ <- 1 }}\n")),
            _ => s.push_str(&format!(
                "{} {} {{}}\n",
                rng.pick(&["GET", "POST", "PUT", "DELETE"]),
                gen_string(rng)
            )),
        }
        s.push('\n');
    }

    // occasionally scatter model rows into a model
    s = s.replace("model M0 = m0 [\n", "model M0 = m0 [\n    id => Int #id,\n    name => Str,\n]\n");
    for idx in 1..=6 {
        let m = if rng.below(2) == 0 { "M".to_string() } else { format!("T{}", idx) };
        let _ = m;
    }
    s
}

fn gen_tokens(rng: &mut Rng, n: usize) -> String {
    let mut s = String::new();
    let mut stack = 0usize;
    for _ in 0..n {
        match rng.below(12) {
            0..=5 => s.push_str(rng.pick(WRITE_TOKENS)),
            6 => s.push_str(rng.pick(TOKENS)),
            7 => s.push_str(&ident(rng)),
            8 => s.push_str(&gen_string(rng)),
            9 => {
                s.push('{');
                stack += 1;
            }
            10 => {
                if stack > 0 {
                    s.push('}');
                    stack -= 1;
                } else {
                    s.push('}');
                }
            }
            _ => s.push_str(&rng.below(100000).to_string()),
        }
        s.push(' ');
    }
    for _ in 0..stack {
        s.push_str("} ");
    }
    s
}