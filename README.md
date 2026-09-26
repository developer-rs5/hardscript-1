# HardScript

<div align="center">

```
 ##    ##  ######   ######   ##     ##  ########  ########
 ###   ## ##    ## ##    ##  ###   ###  ##        ##     ##
 ####  ## ##       ##        #### ####  ######    ########
 ## ## ## ##  ###  ##  ####  ## ### ##  ##        ##   ##
 ##  #### ##    ## ##    ##  ##     ##  ########  ##    ##
```

**A batteries-included programming language for building HTTP APIs, real-time apps, and back-end services — compiled to native C++.**

</div>

HardScript is a statically-oriented scripting language that compiles to C++ and links a single-header runtime. Write route handlers, WebSockets, database queries, JWT auth, and background races in one small file; get a native executable in a few seconds.

- **Simplicity first.** A handful of keywords, no build system to configure, zero dependencies to install beyond a C++ compiler.
- **Friendly syntax.** HTTP verbs *are* the routing syntax (`GET "/" :: { ... }`). Tests and servers live in the same file.
- **Native performance.** Generated C++ compiles with `g++` down to a single binary with a thread-per-connection HTTP server.
- **In-the-box everything.** JSON, crypto, JWT, file system, environment, time, PostgreSQL (native wire protocol), and WebSockets.

> **Status: v0.1 developer preview.** The compiler, runtime, CLI, formatter, docs generator, and test runner work end-to-end. The public API and file format are stabilizing. See [Roadmap](#roadmap).

---

## Features

| Area | What you get |
|------|--------------|
| Language | `bring` imports, `app @port`, routes, middleware, models, `calc` functions, `loop`, `pick` (match), `race`, `async`/`wait`, sockets, tests |
| HTTP | Route handlers returning JSON or text, path/query/bound-URL params, request body as objects, `before` middleware |
| Tests | In-file `test` blocks run via `hard test`, HTTP calls with `GET "/" {}`, `expect` assertions |
| WebSockets | `socket "/path"` with `connect` / `message` / `disconnect` events, rooms, broadcast, replies |
| Data | JSON parse/stringify, first-class object/map/list values, PostgreSQL over the native wire protocol |
| Security | SHA-1 / SHA-256 / MD5 / HMAC, base64, UUIDs, random tokens, JWT sign & verify |
| Toolchain | `hard` CLI: `new`, `build`, `run`, `test`, `fmt`, `docs`, `add`, `doctor`, `bench`; diagnostics with location + suggestion |
| Runtime | Split C++ runtime under `runtime/` (`hs_runtime.hpp` umbrella + `value`/`io`/`crypto`/`http`/`sched`/`postgres`/`util`), thread-per-connection server, static file serving |

---

## Installation

### Prerequisites

- **Rust toolchain** (1.74+) — `cargo`, `rustc`
- **C++17 compiler** — `g++` (GCC 9+) or `clang++`
- **make** (optional)
- Linux / macOS. Postgres module connects over TCP (no client library required).

### Build the `hard` toolchain

```sh
git clone <your-repo-url> HardScript
cd HardScript

# debug build
cargo build

# or release build (recommended)
cargo build --release
```

The CLI binary is written to `target/release/hard`. Add it to your `PATH`:

```sh
export PATH="$PWD/target/release:$PATH"
```

Check that everything is installed correctly:

```sh
hard doctor
```

---

## Hello World

Create a project and run it:

```sh
hard new hello
cd hello
hard run
```

`hello/main.hard`:

```
bring http

app @3000

GET "/" :: {
    <- { "hello": "world" }
}
```

Point a browser or `curl` at `http://localhost:3000/`:

```sh
curl http://localhost:3000/
# {"hello":"world"}
```

---

## HTTP API example

A small REST API with path params, JSON bodies, and an in-file test:

```
bring http

app @8080

GET "/users/:id" :: (id = Str) {
    <- { "user": "u-" + id, "port": 8080 }
}

POST "/users" :: (body = User) {
    <- { "status": 201, "id": 1, "name": body.name }
}

test "user flow" {
    res <- POST "/users" { { "name": "ada" } }
    expect res.body.status == 201
    expect res.body.name == "ada"
}
```

Run the tests, then the server:

```sh
hard test    # 1 passed, 0 failed
hard run     # server on :8080
```

---

## CLI reference

```
hard new <name>              Create a new HardScript project
hard build [file]            Compile to a native executable
hard run   [file] [args..]   Build and run the server
hard test  [file]            Build and run the test suite
hard fmt   [file]            Reformat a source file in place
hard fmt   --check [file]    Verify formatting without writing
hard docs  [file]            Generate API.md for a source file
hard add   <module>          Add a module reference to hard.toml
hard doctor                  Check the toolchain (g++, runtime)
hard bench [file]            Release-build and report timings
hard migrate <diff|up|down|status>  Diff models and run migrations
hard seed   [file]           Run seed files against the database
hard help                    Show help
```

`file` defaults to `main.hard`. `build` writes the generated C++ and runtime to `.hard/` and produces the binary there.

---

## Project architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        HardScript compiler                        │
│                                                                  │
│  .hard ──► Lexer ──► Parser ──► AST ──► Type Check ──► Optimizer │
│  source    token.rs  parser.rs   ast.rs  typecheck.rs  optimizer │
│                                                                  │
│             ┌──────────────────────────────────────────────────┐ │
│             │                 Code Generator                  │ │
│             │                     codegen.rs                    │ │
│             │  generates C++ against the single-header runtime │ │
│             └──────────────────────────────────────────────────┘ │
│                              │                                   │
│                              ▼                                   │
│                     g++  (C++17, -pthread)                       │
│                              ▼                                   │
│               ┌──────────────────────────────┐                  │
│               │  Native executable           │                  │
│               │  runtime/ (hs_runtime.hpp umbrella) │            │
│               │  Haskell-free, hands-free    │                  │
│               └──────────────────────────────┘                  │
└──────────────────────────────────────────────────────────────────┘
```

```
compiler/           Rust compiler crate (hs-compiler)
  src/lexer.rs      tokenizer                  src/codegen.rs  C++ code generator
  src/parser.rs     recursive-descent parser   src/typecheck.rs static checks
  src/ast.rs        AST types                  src/error.rs    diagnostics
  src/fmt.rs        formatter                  src/docs.rs     API.md generator
cli/                `hard` CLI (build/run/test/fmt/docs/bench/...)
lsp/                minimal language server (diagnostics today)
runtime/            C++ runtime split into focused headers:
                    runtime/hs_runtime.hpp is the umbrella (value, io, crypto,
                    http, sched, postgres, util)
tests/              integration scripts        examples/       official examples
reports/            automated smoke-test reports
```

---

## Roadmap

### v0.1 — developer preview (current)

- [x] Lexer, parser, AST, type checker, C++ code generator
- [x] HTTP server runtime, JSON responses
- [x] `hard build / run / test / fmt / docs / bench`
- [x] Tests with in-file `expect` assertions
- [x] WebSockets, crypto/JWT, PostgreSQL wire protocol, fs/env/time modules
- [x] Project examples, integration tests, smoke report

### v0.2

- Package registry (`hard add` backed by a real index)
- Deeper static type inference and typed diagnostics
- Async runtime with a green-thread scheduler
- LSP completion and in-editor diagnostics

### Later

- LLVM backend, optimizer passes, cloud deploy tooling

---

## License

Placeholder — to be decided before the first public release. All rights reserved.

---

## Contributing

This is a developer preview. API and format changes are expected until v0.2.