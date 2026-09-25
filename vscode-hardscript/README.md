# HardScript for VS Code

HardScript language support for `.hard` files. The extension ships a TextMate grammar, a snippet
library, an LSP client, workspace tasks, a Debug Adapter Protocol adapter, and commands backed by
the `hard` CLI.

## Requirements

- VS Code 1.85 or newer
- Node.js 18 or newer for extension development
- A HardScript workspace built with the Rust toolchain
- `g++`, `gcc`, and `make` available for native builds

Build the HardScript workspace before using a local server:

```sh
cargo build --release -p hs-lsp
cargo build --release -p hs-cli
```

The resulting binaries are normally `target/release/hs-lsp` and `target/release/hard`. The extension does not compile Rust automatically.

## Install and build the extension

From this directory:

```sh
npm install
npm run check
npx @vscode/vsce ls
npx @vscode/vsce package
```

Open `vscode-hardscript` in VS Code and press `F5` to launch an Extension Development Host. Install the generated `.vsix` with **Extensions: Install from VSIX...** when packaging for another machine.

The extension keeps its JavaScript entry points in `extension.js` and `debugAdapter/hardDebugAdapter.js`; no TypeScript build chain is required.

## Syntax highlighting

`./syntaxes/hardscript.tmLanguage.json` defines the `source.hardscript` grammar and is registered in
`contributes.grammars`, so `.hard` files are highlighted without building anything. It covers:

| Area | Scoped constructs |
| --- | --- |
| Keywords | `bring`, `app`, `model`, `run`, `GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `socket`, `connect`, `message`, `disconnect`, `before`, `loop`, `pick`, `async`, `wait`, `race`, `test`, `expect`, `calc` |
| Types | `Int`, `Float`, `Text`, `Bool`, `List`, `Object`, `Nil`, `Str` |
| Builtin modules | `http`, `json`, `crypto`, `jwt`, `fs`, `env`, `runtime`, `time`, `websocket`, `postgres`, including `module.function` calls |
| Literals | double-quoted strings with `\n \t \r \" \\ \' \0` escapes, `"""triple-quoted"""` raw strings, integers, floats, `true`, `false`, `nil` |
| Comments | `//` line comments and `/* ... */` block comments |
| Attributes | model attributes such as `#id` and `#unique` |
| Operators | `<-`, `::=`, `=>`, `::`, `:(`, `==`, `!=`, `<=`, `>=`, `&&`, `\|\|`, `!`, `+ - * / %`, `...`, `.`, `@` |
| Declarations | `calc`/`async calc` signatures with typed parameters and return types, `model Name = table [`, `before name ::`, `socket "/path"`, `test "name"`, `app @port`, `verb "/path" :: (param = Type)` routes and their binding lists |

## Snippets

`./snippets/hardscript.json` is registered in `contributes.snippets` and provides 46 snippets for
`.hard` files: `fn`, `async`, `model`, `modelnotable`, `get`, `post`, `put`, `delete`, `patch`,
`routeparam`, `routeid`, `socket`, `connect`, `message`, `disconnect`, `before`, `test`,
`request`, `requestbody`, `expect`, `expecteq`, `var`, `const`, `return`, `if`, `ifelse`, `loop`,
`pick`, `race`, `wait`, `obj`, `list`, `app`, the ten `bring <module>` snippets (`bring http`,
`bring json`, `bring crypto`, `bring jwt`, `bring fs`, `bring env`, `bring runtime`, `bring time`,
`bring websocket`, `bring postgres`), and the `websocket.reply`, `websocket.broadcast`, and
`websocket.join` helpers.

## Binary resolution

The server, the CLI, and the debug adapter use the same ordered resolution strategy:

1. `hardscript.serverPath`, `hardscript.cliPath`, or a launch configuration `hardPath`
2. `target/release/<binary>` in the workspace folders or their parents
3. `target/debug/<binary>` in the workspace folders or their parents
4. A binary bundled under the extension's `bin`, `server`, or `resources/bin` directory
5. The executable search path

The server searches for `hs-lsp`, `hardscript-lsp`, and `hard-lsp`. The CLI and the debug adapter
search for `hard`. The debug adapter also honors the `HARD_BIN` environment variable. Paths may be
absolute, workspace-relative, executable names, or use `~` and `${workspaceFolder}`.

## Settings

| Setting | Purpose |
| --- | --- |
| `hardscript.serverPath` | Override the language-server executable. |
| `hardscript.serverArgs` | Add arguments to the language-server process. |
| `hardscript.cliPath` | Override the `hard` executable used by commands and tasks. |
| `hardscript.hardPath` | Deprecated alias for `hardscript.cliPath`. |
| `hardscript.trace.server` | Set LSP tracing to `off`, `messages`, or `verbose`. |

Resource-scoped settings are honored independently for each folder in a multi-root workspace. The
language client sends all workspace folders in its initialization options and uses the active folder
as its primary workspace folder. The document selector covers both `file:hardscript` and
`untitled:hardscript` documents.

## Commands

The Command Palette contributes exactly these commands:

- **HardScript: Build** runs `hard build` for the active `.hard` file.
- **HardScript: Run** runs `hard run` for the active file.
- **HardScript: Test** runs `hard test` for the active file.
- **HardScript: Format** runs `hard fmt` for the active file.
- **HardScript: Doctor** runs `hard doctor`.
- **HardScript: Explain Error** finds an `HSxxxx` code in the active diagnostic or selection, asks
  the language server through `workspace/executeCommand` with the `hardscript.explainError`
  command, and falls back to `hard errors --markdown` when the server cannot answer.

CLI stdout and stderr are streamed to the **HardScript** output channel. Missing executables, non-zero exits, and server failures are reported without rejecting the VS Code command.

## Tasks

A `hardscript` task accepts `build`, `run`, `test`, `format`, `doctor`, or `debug`, plus optional
`file`, `args`, `stopOnEntry`, `serverMode`, and `serverPort` values:

```json
{
  "version": "2.0.0",
  "tasks": [
    {
      "type": "hardscript",
      "command": "test",
      "file": "main.hard",
      "problemMatcher": ["$hardscript"]
    }
  ]
}
```

`provideTasks` discovers entry points in every workspace folder and offers `build`, `run`, `test`,
and `format` for each of them, scoped to that folder:

- a `hard.toml` manifest, using `main.hard` (or the only `.hard` file) next to it
- `example/*.hard` and `examples/*.hard`
- one level of nesting such as `examples/<name>/main.hard`
- loose `.hard` files in the workspace root when no manifest is present

`resolveTask` maps the definition to a `ProcessExecution` for the resolved `hard` binary. `build`,
`run`, `test`, and `format` use the `$hardscript` problem matcher; `doctor` runs without a matcher.
`debug` resolves to a `CustomExecution` that starts a HardScript debug session for `file`, and never
fails when no CLI is installed. When a task omits `file`, the active `.hard` editor is used, then
`main.hard`.

## Debugging

The extension registers a `DebugAdapterDescriptorFactory` that runs
`debugAdapter/hardDebugAdapter.js` with the extension host's Node runtime, and contributes a
`hardscript` debug type backed by `debugAdapterExecutable`. A `DebugConfigurationProvider` fills in
`program`, `cwd`, `hardPath`, and `args` when a launch configuration omits them.

```json
{
  "type": "hardscript",
  "request": "launch",
  "name": "HardScript: debug rest-api",
  "program": "${workspaceFolder}/examples/rest-api/main.hard",
  "cwd": "${workspaceFolder}/examples/rest-api",
  "args": [],
  "serverMode": "stdio",
  "serverPort": 0,
  "stopOnEntry": false
}
```

| Attribute | Purpose |
| --- | --- |
| `program` | Required `.hard` file to build and run. |
| `cwd` | Working directory for the build and the program. |
| `args` | Arguments passed to the compiled program. |
| `hardPath` | Override for the `hard` executable used by the adapter. |
| `env` / `envFile` | Extra environment entries for the program. |
| `serverMode` | Appends `--serverMode=<value>` to the program arguments. |
| `serverPort` | Appends `--serverPort=<value>` to the program arguments. |
| `stopOnEntry` | Stop at the first line instead of the first breakpoint. |

`serverMode` and `serverPort` are optional. Configurations that omit them behave exactly as before,
which keeps existing launch files valid. The v0.1 runtime ignores the flags it does not implement;
they are appended only so a future server mode can be selected.

The adapter speaks the Debug Adapter Protocol over stdio and implements `initialize`, `launch` and
`attach`, `configurationDone`, `setBreakpoints`, `setFunctionBreakpoints`,
`setExceptionBreakpoints`, `threads`, `stackTrace`, `scopes`, `variables`, `continue`, `next`,
`stepIn`, `stepOut`, `pause`, `evaluate`, `terminate`, and `disconnect`. It spawns
`hard run <program>` with the configured arguments, environment, and working directory, reports the
`hard` executable it resolved, forwards program stdout and stderr as output events, and reports
`exited` and `terminated` when the program ends. The program is started in its own process group so
terminating a session never leaves a server behind.

Breakpoint support is intentionally focused. `setBreakpoints` echoes every requested line back as a
verified marker, `stopOnEntry` or the first marker produces a `stopped` event, `stackTrace` returns
a single frame for the current marker line, and `variables` returns an empty `Locals` scope. The
HardScript toolchain exposes no debug value protocol, so stepping walks the marker lines, expression
evaluation returns an empty result, and variables are not available.

## Formatting on save

Formatting is delegated to the language client's `textDocument/formatting` provider. Set the normal VS Code editor option to format HardScript files on save:

```json
"[hardscript]": {
  "editor.formatOnSave": true
}
```

The extension does not run `hard fmt` from a save listener. If the language server does not advertise a formatting provider, the LSP client cannot format the document until that capability is available.

## Development

`.vscode/launch.json` and `.vscode/tasks.json` are checked in and are the only `.vscode` files the
repository tracks:

- `F5` starts an Extension Development Host through **Run Extension** after running `npm run check`
- the sample **HardScript: debug …** configurations debug the bundled examples
- **HardScript: debug current file** and **HardScript: debug hello-world** tasks open a debug
  session through the custom `debug` task execution

`npm run check` is dependency-free and cross-platform: `scripts/check.js` walks the project, runs
`node --check` on every `.js` file, and parses every `.json` file, so `extension.js`,
`debugAdapter/hardDebugAdapter.js`, and `scripts/check.js` are all syntax-checked in one command.

## License

MIT
