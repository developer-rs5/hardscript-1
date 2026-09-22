# HardScript Language Specification — v0.1

Version: v0.1-dev
Status: Developer preview — the surface described here is the implemented, tested feature set.

---

## 1. Overview

HardScript is a compiled scripting language for back-end services. A `.hard` file
declares a web application (routes, middleware, sockets), pure functions (`calc`),
data models, and in-file tests. The compiler lowers the program to C++ and links a
single-header runtime to produce a native executable.

The language is whitespace-sensitive-free (newline ≠ statement terminator; statements
span tokens until a complete parse). Blocks use `{ }`. Comments are `//` (line) and
`/* ... */` (block).

---

## 2. File extension

- Source files: `.hard`
- Key file: `main.hard` (the default target of the `hard` CLI)
- Project configuration: `hard.toml` (name, version, description — informational in v0.1)
- Generated artifacts live under `.hard/` (C++ source, runtime header, binary)

---

## 3. Lexical structure

### 3.1 Identifiers

```
ident      ::= [a-zA-Z_] [a-zA-Z0-9_]*
```

`true` and `false` are boolean literals even though they lex as identifiers.

### 3.2 Literals

```
int    ::= [0-9]+                      // stored as signed 64-bit (Int)
float  ::= [0-9]+ "." [0-9]+           // IEEE-754 double (Float)
text   ::= "..." (with escapes \n \t \r \" \\ \' \0)
        |  """..."""                    // raw block string
bool   ::= true | false
list   ::= [ expr ("," expr)* | "" ]    // empty list: [ ]
object ::= { key ":" expr ("," key ":" expr)* | "" }
key    ::= ident | text                 // keys may be written unquoted
nil    ::= (no literal; produced by absent values)
```

`text` literals support the escapes `\n`, `\t`, `\r`, `\"`, `\\`, `\'`, `\0`.
Triple-quoted strings `"""..."""` capture content verbatim (no escapes).

### 3.3 Comments

```
// line comment — runs to end of line
/* block comment — may span lines, must be closed */
```

### 3.4 Numbers

- Integers beyond the i64 range clamp to `i64::MAX`.
- A `.` is only part of a number when followed by a digit.

---

## 4. Keywords

```
bring   app     model    GET     POST    PUT
DELETE  PATCH   socket   connect message disconnect
before  loop    pick     async   wait    race
test    expect  calc
```

`RUN` lexes as a keyword but is reserved (no behavior yet).

---

## 5. Operators

### 5.1 Arithmetic

| Op | Meaning      |
|----|--------------|
| `+` | add / concatenate |
| `-` | subtract (binary) / negate (unary) |
| `*` | multiply |
| `/` | divide |
| `%` | modulo |

### 5.2 Comparison

| Op | Meaning       |
|----|---------------|
| `==` | equal |
| `!=` | not equal |
| `<` `<=` | less than / less or equal |
| `>` `>=` | greater than / greater or equal |

### 5.3 Logic

| Op | Meaning   |
|----|-----------|
| `&&` | logical and (short-circuit) |
| `||` | logical or (short-circuit) |
| `!` | logical not (unary) |

### 5.4 Declarations and flow

| Op | Meaning                             |
|----|-------------------------------------|
| `<-` | declare a mutable variable `name <- expr`, or return a value from a block when it is the first token of a statement |
| `::=` | declare a constant `name ::= expr` |
| `=>` | loop binding (`loop x => iter`), type annotation after a `calc` signature, `pick` arm separator |
| `::` | separates the head and body of a route, middleware, or socket event |
| `:` | introduces the `:{` else-branch of an `?(...)` |
| `.` | member access (single level)   |
| `[ ]` | index access                   |
| `(...)` | call                            |
| `=` | type binding in route params (`(id = Str)`) and model table names |
| `#` | model attribute marker (`#id`) |
| `...` | lexed range token (parser support pending in v0.1; see §16) |
| `@` | port marker (`app @3000`)     |

Precedence (high → low): unary → `* / %` → `+ -` → `< <= > >= == !=` → `&&` → `||`.

---

## 6. Types

HardScript is dynamically typed at runtime over a small set of value types.
Static annotations are accepted as documentation and validation metadata.

| Type | Values                          | Examples                    |
|------|---------------------------------|-----------------------------|
| Int  | signed 64-bit integers          | `3000`, `-1`                |
| Float| IEEE-754 doubles                | `3.14`, `1.5`               |
| Text | UTF-8 strings                  | `"hello"`, `"""verbatim"""` |
| Bool | `true` / `false`                | `true`                      |
| List | ordered values                  | `[1, 2, 3]`                 |
| Object| key/value pairs (map)           | `{ name: "ada" }`           |
| Nil  | absence of value                | (implicit)                  |

- `+` concatenates text with any value (values render as text).
- Equality (`==`) is structural for lists and objects.
- Truthiness: `nil` and `false` are falsy; everything else (including `0` and `""`) is truthy.

Type names used in signatures are nominal identifiers (`Str`, `User`, `Int`, ...).
They participate in documentation and route binding, not runtime dispatch.

---

## 7. Imports

```
bring <module>
```

Valid modules in v0.1:

| Module | Contents |
|--------|----------|
| `http` | routing (the HTTP verbs are top-level keywords) |
| `json` | `json.parse(text)`, `json.stringify(value)`, `json.keys(obj)` |
| `crypto` | sha256, sha256bin, sha1, md5, hmac, base64, base64url, base64decode, uuid, random_hex, token |
| `jwt` | `jwt.sign(payload, secret)`, `jwt.verify(token, secret)` |
| `fs` | read, write, append, exists, is_dir, list, remove, size |
| `env` | `env.get(name)` |
| `runtime` | print, args, argc, argv, pid, hostname, platform, cpus |
| `time` | `time.now()`, `time.iso()`, `time.sleep(seconds)` |
| `websocket` | self, reply, join, leave, broadcast, broadcast_room |
| `postgres` | `connect(info)`, `query(fd, sql)` |

There are no exports in v0.1: top-level routes, sockets, models, functions and tests
are the public surface (see `hard docs`).

---

## 8. Application

```
app @<port>
```

Declares the listen port (default `3000`). `app` must appear exactly once at top level.

---

## 9. Variables and constants

```
name <- expr      // variable (mutable)
name ::= expr     // constant

CONST:
    at top level both forms emit a file-scoped value; inside a block they emit a
    block-local value.
```

Top-level variables are shared across routes, middleware, sockets and tests.

---

## 10. Functions

```
calc name (<Type> <arg>, ...) => <ReturnType> { body }
async calc name (<Type> <arg>, ...) => <ReturnType> { body }   // async is accepted; v0.1 runs synchronously
```

- Parameters are annotated `Type name`.
- A function returns the value of a `<- expr` statement; a function that ends
  without returning yields `nil`.
- Functions are invoked as `name(arg1, arg2)`.
- Reserved notes:
  - v0.1 parameter parsing requires the typed form `(Type name, ...)`. Unadorned
    `(name)` and duplicate-type layouts are undefined.

---

## 11. Models

```
model <Name> = users [
    <field> => <Type> [#attribute ...],
    ...
]
```

- `= <table>` is optional; the table name defaults to the lowercased model name.
  The `= <table>` form takes a bare identifier (`users`), not a quoted string.
- Every field row ends with a comma (a trailing comma after the last field is fine).
- Attributes (`#id`, `#unique`, ...) are metadata.
- In v0.1 models are documentation/blueprint metadata consumed by `hard docs`;
  they do not generate schema code or database migrations.

---

## 12. HTTP routes

```
GET    "/path" :: (param = Str) { ... }
POST   "/path" :: (body = Type) { ... }
PUT    "/path" :: (body = Type) { ... }
DELETE "/path" :: (id = Str)    { ... }
PATCH  "/path" :: (body = Type) { ... }
```

- Route paths may embed parameters as `:name` segments.
- Route parameter bindings are declared after the `::`, inside a parenthesized
  list, before the opening block. Binding forms (in order):
  - `(name = Str)` → binds the matching URL segment or query value to `name`.
  - `(body = Type)` → binds the parsed JSON request body object to `body`.
- The route block returns a response with `<- expr`:
  - object/list → JSON (`application/json`)
  - text → `text/plain`
  - `nil` → empty `204`
- In v0.1 a matched route always responds `200` on success; errors thrown by the
  handler are caught and reported with `500`. There is no way to set an
  arbitrary HTTP status yet; applications carry their own status/error fields in
  the response body.
- Route handlers may throw at runtime; the server returns `500` and continues.

Example:

```
GET "/users/:id" :: (id = Str) {
    <- { "user": "u-" + id }
}
```

---

## 13. Middleware

```
before <name> :: { ... }
```

- Runs in declaration order before every matching route.
- A middleware that executes `<- expr` short-circuits with that response
  (e.g. an auth gate). Otherwise it must call `x <- 2` never; simply ending the
  block continues to the next middleware/route.
- Middleware names are identifiers (not quoted strings) in v0.1.

---

## 14. Conditionals

```
?( <cond> ) { <then-block> }        // no else
?( <cond> ) { <then-block> } :{ <else-block> }
```

The condition is truthiness-tested.

---

## 15. Loops

```
loop <var> => <expr> { <block> }
```

- The iterable is evaluated once.
- If the value is a list, the variable takes each element.
- If the value is a text string, the variable takes each character (as text).
- Any other single value is visited once.

There is no `break`/`continue` in v0.1.

---

## 16. Match

```
pick <expr> {
    <equal-value> => <expr>,
    ...
    * => <default-expr>,
}
```

- Arms are compared structurally (`==`) against the scrutinee.
- The default arm is the key `*`.
- `pick` is an expression and returns a value.

Integer ranges (`expr ... expr`) lex but are not yet lowered; avoid them in v0.1.

---

## 17. Async, wait, race

```
async calc name(...) => T { ... }   // accepted; executes synchronously in v0.1

x <- wait expr                       // evaluates expr and yields its value

race [ expr1, expr2, ... ]           // statement
```

- `race` spawns each expression on its own thread; the statement completes when
  the first task finishes, and the win is unknown until v0.2 (results are
  discarded). Use `race` to narrow a slow fallback window.

---

## 18. WebSockets

```
socket "/path" {
    connect :: { ... }
    message(d) :: { ... }
    disconnect :: { ... }
}
```

- Event bodies may be omitted.
- Inside `message`, the payload is bound to `d` as text.
- Available (with `bring websocket`, call as `websocket.<fn>`):

| Call | Effect |
|------|--------|
| `websocket.self` | current connection id (text) |
| `websocket.reply(text)` | send text back to the current connection |
| `websocket.join(room)` | join the room |
| `websocket.leave` | leave current room |
| `websocket.broadcast(text)` | broadcast to all connections in the current room |
| `websocket.broadcast_room(room, text)` | broadcast to a named room |

---

## 19. Tests

```
test "<name>" {
    ... statements ...
    res <- GET "/path" { }            // in-process HTTP call
    res <- POST "/path" { { body } }  // with request body
    expect <value>                     // bare assertion (truthy)
    expect <a> == <b>                  // comparison assertion
    expect <a> (== | != | <  | <= | >  | >=) <b>
}
```

- Every `GET/POST/PUT/DELETE/PATCH "/path" { body }` statement in a test performs an
  in-process request against the declared `app`. The result is an object with
  `status` (Int), `headers` (object), and `body` (parsed JSON).
- `expect` fails the test when the assertion is false; all tests run and a
  summary is printed.
- `hard test` runs the suite (`--test`) and prints `N passed, M failed`;
  exits non-zero when any test fails.

---

## 20. Route returns and error handling

- A return inside a route/middleware (`<- v`) produces a Response.
- Returning an object/list → `200` JSON; text → `200` text; `nil` → `204`.
- Uncaught handler exceptions → `500` response (message body), server keeps running.
- No `try`/`catch` syntax exists in v0.1. Guards belong in middleware.
- The only error-reporting built-ins in v0.1 are `runtime.print` and test `expect`.

---

## 21. Formatter rules (`hard fmt`)

Canonical formatting applied by the AST re-emitter:

- Indentation: 4 spaces per block level.
- One blank line between top-level declarations.
- Objects render as `{ key: value, ... }` with single spaces around `:` and after
  the comma; unquoted keys are canonical.
- Lists render as `[a, b, c]` (single spaces).
- String literals render quoted with escape sequences.
- `<-`, `::=`, `=>`, `==` etc. are surrounded by single spaces.
- Route heads use `VERB "/path" :: {`, socket heads `socket "/path" {`,
  tests `test "name" {`, middleware `before name :: {`.
- `hard fmt --check` verifies a file is canonical without writing.

---

## 22. Grammar summary

```
program    ::= (stmt)* EOF
stmt       ::= bring module
             | app "@" int
             | modeldecl | route | middleware | socket | func | test
             | var | const | if | loop | race | return | expect | expr
var        ::= ident "<-" expr
const      ::= ident "::=" expr
return     ::= "<-" expr
if         ::= "?(" expr ")" block (":{" block)?          // else block
loop       ::= "loop" ident "=>" expr block
race       ::= "race" "[" expr ("," expr)* "]"
expect     ::= "expect" expr ((==|!=|<|<=|>|>=) expr)?
func       ::= ("async")? "calc" ident "(" typaram* ")" ("=>" typ)? block
typaram    ::= ident ident
route      ::= (GET|POST|PUT|DELETE|PATCH) string "::" routeparams? block
routeparams::= "(" ident "=" ident ("," ident "=" ident)* ")"
middleware ::= "before" ident "::" block
socket     ::= "socket" string "{" (connect|message|disconnect)* "}"
event      ::= (connect|message|disconnect) ("(" ident* ")")? "::" block
test       ::= "test" string block
modeldecl  ::= "model" ident ("=" ident)? "[" (ident "=>" ident attr* ",")* "]"
attr       ::= "#" ident
block      ::= "{" stmt* "}"
expr       ::= or
or         ::= and ("||" and)*
and        ::= cmp ("&&" cmp)*
cmp        ::= add ((==|!=|<|<=|>|>=) add)*
add        ::= mul ((+|-) mul)*
mul        ::= unary ((*|/|%) unary)*
unary      ::= ("-")? ("!")? ("wait")? postfix
postfix    ::= atom (("." ident) | ("[" expr "]") | ("(" args ")"))*
atom       ::= int | float | text | bool | ident
             | list | object | "(" expr ")"
             | "pick" expr block-arm
             | (GET|POST|PUT|DELETE|PATCH) string (block-body)?
```