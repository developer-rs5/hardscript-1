//! Official project templates (`hard new <name>`).
//!
//! Every template ships: README.md, hard.toml, main.hard (with inline tests),
//! Dockerfile and .gitignore. `hard new <name>` where `<name>` matches a
//! template scaffolds it; otherwise it falls back to the default project.

use std::path::Path;

/// A bundled scaffold. `files` are `(relative path, contents)`.
pub struct Template {
    pub name: &'static str,
    pub description: &'static str,
    pub port: u16,
    pub files: &'static [(&'static str, &'static str)],
}

/// The list of official template names.
pub const TEMPLATES_INVENTORY: &[&str] = &[
    "hello-world",
    "rest-api",
    "auth-api",
    "websocket-chat",
    "postgres-crud",
    "graphql-api",
    "microservice",
    "cron-service",
    "ai-api",
    "worker",
    "ecommerce-api",
    "package-library",
];

const GITIGNORE: &str = ".hard/\n*.o\n*.hsbin\ntarget/\n";

fn hard_toml(name: &str, desc: &str, port: u16) -> String {
    let port_line = if port > 0 {
        format!("port = {}\n", port)
    } else {
        String::new()
    };
    format!(
        "schema = 1\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         edition = \"2027\"\n\
         description = \"{desc}\"\n\
         \n\
         [compiler]\n\
         opt = 3\n\
         warnings = true\n\
         \n\
         [server]\n\
         {port_line}\
         \n\
         [dependencies]\n"
    )
}

fn readme(name: &str, desc: &str, port: u16, routes: &str) -> String {
    let port_note = if port > 0 {
        format!("\nThe server listens on **port {port}**.\n")
    } else {
        "\nThis is a library — it exposes no HTTP server.\n".to_string()
    };
    format!(
        "# {name}\n\n{desc}\n{port_note}\n\n## Run\n\n```sh\nhard run\n```\n\n## Test\n\n```sh\nhard test\n```\n\n## Routes\n\n{routes}\n\n## Docker\n\n```sh\ndocker build -t {name} .\ndocker run -p {port}:{port} {name}\n```\n"
    )
}

fn dockerfile(port: u16) -> String {
    if port == 0 {
        return "FROM debian:bookworm-slim AS builder\nWORKDIR /build\nCOPY . .\nRUN hard build main.hard\n".to_string();
    }
    format!(
        "# Build stage: requires `hard` on PATH.\n\
         FROM debian:bookworm-slim AS builder\n\
         WORKDIR /build\n\
         COPY . .\n\
         RUN hard build main.hard\n\
         \n\
         # Runtime stage.\n\
         FROM debian:bookworm-slim\n\
         RUN apt-get update && apt-get install -y ca-certificates\n\
         WORKDIR /app\n\
         COPY --from=builder /build/.hard/main /app/server\n\
         EXPOSE {port}\n\
         CMD [\"/app/server\"]\n"
    )
}

fn template(name: &'static str, desc: &'static str, port: u16, main_hard: &str, routes: &str) -> Template {
    let files: Vec<(&'static str, &'static str)> = vec![
        ("README.md", leak(readme(name, desc, port, routes))),
        ("hard.toml", leak(hard_toml(name, desc, port))),
        ("main.hard", leak(main_hard.to_string())),
        ("Dockerfile", leak(dockerfile(port))),
        (".gitignore", leak(GITIGNORE.to_string())),
    ];
    Template {
        name,
        description: desc,
        port,
        files: Box::leak(files.into_boxed_slice()),
    }
}

/// Leak a `String` to a `&'static str` (templates are static by design).
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// All templates in a stable order (matches `TEMPLATES_INVENTORY`).
pub fn all() -> Vec<Template> {
    vec![
        hello_world(),
        rest_api(),
        auth_api(),
        websocket_chat(),
        postgres_crud(),
        graphql_api(),
        microservice(),
        cron_service(),
        ai_api(),
        worker(),
        ecommerce_api(),
        package_library(),
    ]
}

/// Find a template by name.
pub fn find(name: &str) -> Option<Template> {
    all().into_iter().find(|t| t.name == name)
}

/// Render a stable multi-line inventory of every template.
pub fn list_lines() -> Vec<String> {
    let mut out = Vec::new();
    for t in all() {
        let files: Vec<&str> = t.files.iter().map(|(p, _)| *p).collect();
        out.push(format!("{}  (port {})  {}", t.name, t.port, files.join(", ")));
    }
    out
}

/// Write a template's files into `dest_dir` (which is created). The project is
/// named `project_name`; it replaces any `name =` in a hard.toml the template
/// ships so renames stay consistent.
pub fn scaffold(tpl: &Template, project_name: &str, dest_dir: &Path) -> Result<(), String> {
    if dest_dir.exists() {
        return Err(format!("'{}' already exists", dest_dir.display()));
    }
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("cannot create {}: {e}", dest_dir.display()))?;
    for (rel, contents) in tpl.files {
        // Substitute the project name into hard.toml and keep the rest as-is.
        let contents = if *rel == "hard.toml" {
            let lines: Vec<&str> = contents.lines().collect();
            let mut out = Vec::with_capacity(lines.len());
            for line in lines {
                if let Some(rest) = line.strip_prefix("name = ") {
                    out.push(format!("name = \"{project_name}\""));
                    let _ = rest;
                } else {
                    out.push(line.to_string());
                }
            }
            out.join("\n")
        } else {
            contents.to_string()
        };
        // main.hard must match the canonical formatter's output (fmt --check):
        // statements are emitted each followed by a newline, so the file ends
        // on a blank line after the last statement.
        let contents = if *rel == "main.hard" {
            let trimmed = contents.trim_end();
            if trimmed.is_empty() {
                contents
            } else {
                format!("{trimmed}\n\n")
            }
        } else {
            contents
        };
        let path = dest_dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, contents)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    Ok(())
}

fn hello_world() -> Template {
    template(
        "hello-world",
        "The smallest HardScript service: one route, one test.",
        3000,
        r#"bring http

app @3000

GET "/" :: {
    <- { hello: "world" }
}

test "hello world" {
    res <- GET "/"
    expect res.status == 200
    expect res.body.hello == "world"
}
"#,
        "| GET | `/` | `{ \"hello\": \"world\" }` |",
    )
}

fn rest_api() -> Template {
    template(
        "rest-api",
        "JSON CRUD REST API backed by a file store.",
        8080,
        r#"bring http

bring json

bring fs

bring time

app @8080

model User = users [
    id => Int #id,
    name => Str,
]

store <- ".hard/users.json"

GET "/users" :: {
    ?(fs.exists(store)) {
        <- json.parse(fs.read(store))
    }
    fs.write(store, "[]")
    <- json.parse(fs.read(store))
}

GET "/users/:id" :: (id = Str) {
    users <- json.parse(fs.read(store))
    loop user => users {
        ?(user.id + "" == id) {
            <- { user: user }
        }
    }
    <- { error: "user not found" }
}

POST "/users" :: (body = User) {
    users <- json.parse(fs.read(store))
    created <- { id: time.now(), name: body.name }
    fs.write(store, json.stringify(users + [created]))
    <- { status: 201, user: created }
}

DELETE "/users/:id" :: (id = Str) {
    users <- json.parse(fs.read(store))
    loop user => users {
        ?(user.id + "" == id) {
            <- { status: 200, deleted: { id: user.id, name: user.name } }
        }
    }
    <- { status: 404, error: "user not found" }
}

test "create then read" {
    fs.write(store, "[]")
    created <- POST "/users" { { name: "grace" } }
    expect created.body.status == 201
    expect created.body.user.name == "grace"
    list_res <- GET "/users"
    expect list_res.body[0].name == "grace"
    missing <- DELETE "/users/does-not-exist"
    expect missing.body.error == "user not found"
}
"#,
        "| GET | `/users` | list users |\n| GET | `/users/:id` | fetch one |\n| POST | `/users` | create |\n| DELETE | `/users/:id` | delete |",
    )
}

fn auth_api() -> Template {
    template(
        "auth-api",
        "JWT register/login/verify API with token signing.",
        9090,
        r#"bring http

bring crypto

bring jwt

app @9090

secret <- "dev-secret-change-me"

POST "/register" :: (body = User) {
    token <- jwt.sign({ user: body.name, role: "member" }, secret)
    <- { status: 201, token: token, user: body.name }
}

POST "/login" :: (body = User) {
    token <- jwt.sign({ user: body.name, role: "member" }, secret)
    <- { status: 200, token: token }
}

POST "/verify" :: (body = User) {
    ok <- jwt.verify(body.token, secret)
    <- { verified: ok }
}

test "register then login" {
    reg <- POST "/register" { { name: "ada", key: "hunter2" } }
    expect reg.body.status == 201
    expect reg.body.user == "ada"
    login <- POST "/login" { { name: "ada", key: "hunter2" } }
    expect login.body.status == 200
    expect login.body.token != ""
    verify <- POST "/verify" { { token: login.body.token } }
    expect verify.body.verified == true
}
"#,
        "| POST | `/register` | create + issue JWT |\n| POST | `/login` | issue JWT |\n| POST | `/verify` | verify a token |",
    )
}

fn websocket_chat() -> Template {
    template(
        "websocket-chat",
        "Realtime chat over WebSockets with rooms and broadcast.",
        9091,
        r#"bring http

bring websocket

app @9091

socket "/chat" {
    connect :: {
        websocket.join("lobby")
        websocket.broadcast(websocket.self + " joined")
    }
    message(d) :: {
        ?(d == "ping") {
            websocket.reply("pong")
        }
        ?(d == "leave") {
            websocket.leave()
        }
    }
    disconnect :: {
        websocket.broadcast(websocket.self + " left")
    }
}

test "server is defined on port 9091" {
    res <- GET "/chat"
    expect res.status == 404
}
"#,
        "| WS | `/chat` | join lobby, broadcast joins/leaves, `ping` -> `pong` |",
    )
}

fn postgres_crud() -> Template {
    template(
        "postgres-crud",
        "PostgreSQL CRUD over the native wire protocol.",
        9092,
        r#"bring http

bring postgres

app @9092

db <- postgres.connect("host=127.0.0.1 port=5432 user=postgres dbname=hardscript")

GET "/users" :: {
    <- { users: postgres.query(db, "select id, name from users order by id") }
}

GET "/users/:id" :: (id = Str) {
    rows <- postgres.query(db, "select id, name from users where id = " + id)
    <- { users: rows }
}

POST "/users" :: (body = User) {
    postgres.query(db, "insert into users (name) values ('" + body.name + "')")
    <- { status: 201 }
}

DELETE "/users/:id" :: (id = Str) {
    postgres.query(db, "delete from users where id = " + id)
    <- { status: 200 }
}

test "server starts" {
    res <- GET "/"
    expect res.status == 404
}
"#,
        "| GET | `/users` | all users |\n| GET | `/users/:id` | one user |\n| POST | `/users` | insert |\n| DELETE | `/users/:id` | delete |\n\n> **Requires PostgreSQL.** The server connects to `host=127.0.0.1 port=5432 user=postgres dbname=hardscript` at boot, so `hard run` and `hard test` need a live database. Create the table first:\n\n```sql\ncreate table users (id serial primary key, name text not null);\n```",
    )
}

fn graphql_api() -> Template {
    template(
        "graphql-api",
        "A GraphQL-style gateway: one endpoint, typed queries.",
        4000,
        r#"bring http

bring json

bring time

app @4000

schema <- { query: { hello: "Hello from HardScript", time: "server time" }, mutation: { addUser: "create a user" } }

GET "/graphql" :: {
    <- { schema: schema }
}

POST "/graphql" :: (body = Obj) {
    q <- body.query
    ?(q == "{ hello }") {
        <- { data: { hello: "Hello from HardScript" } }
    }
    ?(q == "{ time }") {
        <- { data: { time: time.now() } }
    }
    <- { errors: [{ msg: "query not supported" }], q: q }
}

test "hello query" {
    res <- POST "/graphql" { { query: "{ hello }" } }
    expect res.body.data.hello == "Hello from HardScript"
}
"#,
        "| GET | `/graphql` | introspection (schema) |\n| POST | `/graphql` | run a GraphQL query |",
    )
}

fn microservice() -> Template {
    template(
        "microservice",
        "A small focused service: health and a task endpoint.",
        8000,
        r#"bring http

app @8000

GET "/health" :: {
    <- { status: "ok" }
}

GET "/version" :: {
    <- { version: "0.1.0", name: "microservice" }
}

test "health is ok" {
    res <- GET "/health"
    expect res.body.status == "ok"
    v <- GET "/version"
    expect v.body.name == "microservice"
}
"#,
        "| GET | `/health` | health check |\n| GET | `/version` | version info |",
    )
}

fn cron_service() -> Template {
    template(
        "cron-service",
        "Scheduled-job service: jobs registry with a run endpoint.",
        7000,
        r#"bring http

bring json

bring time

app @7000

jobs <- [{ name: "heartbeat", every: 10 }, { name: "purge-cache", every: 60 }]

GET "/jobs" :: {
    <- { jobs: jobs }
}

GET "/run/:t" :: (t = Str) {
    loop job => jobs {
        ?(json.parse(t) % job.every == 0) {
            <- { due: true, job: job.name }
        }
    }
    <- { due: false, at: time.now() }
}

test "job runner" {
    res <- GET "/jobs"
    expect res.body.jobs[0].name == "heartbeat"
    due <- GET "/run/60"
    expect due.body.due == true
    expect due.body.job == "heartbeat"
}
"#,
        "| GET | `/jobs` | list scheduled jobs |\n| GET | `/run/:t` | first job due at time `t` |",
    )
}

fn ai_api() -> Template {
    template(
        "ai-api",
        "AI gateway scaffold: completions endpoint + simple router.",
        7070,
        r#"bring http

bring crypto

bring json

app @7070

models <- ["hard-mini", "hard-pro"]

POST "/v1/completions" :: (body = Obj) {
    prompt <- body.prompt
    ?(prompt == "hello") {
        <- { id: crypto.uuid(), kind: "hard-pro", choices: [{ text: "Hi there! I am a HardScript model." }] }
    }
    <- { id: crypto.uuid(), kind: "hard-pro", choices: [{ text: "echo: " + prompt }] }
}

GET "/v1/models" :: {
    <- { models: models }
}

test "completions respond" {
    res <- POST "/v1/completions" { { kind: "hard-pro", prompt: "hello" } }
    expect res.body.kind == "hard-pro"
    expect res.body.choices[0].text == "Hi there! I am a HardScript model."
}
"#,
        "| GET | `/v1/models` | list models |\n| POST | `/v1/completions` | generate a response |",
    )
}

fn worker() -> Template {
    template(
        "worker",
        "Background worker with a task queue: enqueue and drain.",
        7100,
        r#"bring http

app @7100

POST "/enqueue" :: (body = Obj) {
    <- { status: 202, task: body.task, queue: "accepted" }
}

GET "/queue" :: {
    <- { depth: 0, status: "idle" }
}

test "enqueue then drain" {
    r <- POST "/enqueue" { { task: "ping" } }
    expect r.body.status == 202
    expect r.body.task == "ping"
    d <- GET "/queue"
    expect d.body.status == "idle"
}
"#,
        "| POST | `/enqueue` | add a job (202) |\n| GET | `/queue` | queue depth + status |",
    )
}

fn ecommerce_api() -> Template {
    template(
        "ecommerce-api",
        "Storefront: catalog, cart and checkout.",
        5000,
        r#"bring http

bring json

bring fs

bring time

app @5000

catalog <- [{ name: "book", price: 10 }, { name: "mug", price: 8 }]

cart <- ".hard/cart.json"

GET "/products" :: {
    <- { products: catalog, count: 2 }
}

POST "/products" :: (body = Obj) {
    <- { status: 201, product: { name: body.name, price: body.price } }
}

POST "/cart/add" :: (body = Obj) {
    ?(fs.exists(cart)) {
        state <- json.parse(fs.read(cart))
        count <- state.count + 1
        amount <- state.amount + body.price
        fs.write(cart, json.stringify({ count: count, amount: amount }))
        <- { status: 200, count: count, amount: amount }
    }
    fs.write(cart, json.stringify({ count: 1, amount: body.price }))
    <- { status: 200, count: 1, amount: body.price }
}

POST "/checkout" :: {
    ?(fs.exists(cart)) {
        state <- json.parse(fs.read(cart))
        fs.write(cart, json.stringify({ count: 0, amount: 0 }))
        <- { order: time.now(), total: state.amount }
    }
    <- { order: time.now(), total: 0 }
}

test "catalog and cart" {
    fs.write(cart, json.stringify({ count: 0, amount: 0 }))
    created <- POST "/products" { { name: "book", price: 10 } }
    expect created.body.status == 201
    added <- POST "/cart/add" { { name: "book", price: 10 } }
    expect added.body.count == 1
    checkout <- POST "/checkout" { {  } }
    expect checkout.body.total == 10
}
"#,
        "| GET | `/products` | list catalog |\n| POST | `/products` | validate a product |\n| POST | `/cart/add` | add a product to the cart |\n| POST | `/checkout` | place an order |",
    )
}

fn package_library() -> Template {
    template(
        "package-library",
        "A reusable library of functions (no server) to publish and consume.",
        0,
        r#"bring crypto

calc greeting(Str name) => Str {
    <- "hello, " + name
}

calc shout(Str s) => Str {
    <- s + "!"
}

calc token(Int n) => Str {
    <- crypto.token(n)
}

test "library helpers" {
    expect greeting("world") == "hello, world"
    expect shout("hi") == "hi!"
    expect token(8) != ""
}
"#,
        "No routes — consume with `bring \"./pkg\"` from another project.",
    )
}