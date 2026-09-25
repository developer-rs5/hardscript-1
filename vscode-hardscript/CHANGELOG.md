# Changelog

All notable changes to the HardScript VS Code extension are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.0] - 2026-09-25

### Added

- TextMate grammar at `syntaxes/hardscript.tmLanguage.json` for the `source.hardscript` scope,
  registered through `contributes.grammars`. It covers every keyword in `docs/SPEC.md`
  (`bring`, `app`, `model`, `run`, `GET`, `POST`, `PUT`, `DELETE`, `PATCH`, `socket`, `connect`,
  `message`, `disconnect`, `before`, `loop`, `pick`, `async`, `wait`, `race`, `test`, `expect`,
  `calc`), the type names, the ten builtin modules and `module.function` calls, double-quoted and
  triple-quoted strings with escapes, integers and floats, `//` and `/* */` comments, `#attribute`
  markers, the operator set, and dedicated scopes for `calc` signatures, `model` declarations,
  routes with their binding lists, middleware, sockets, and tests.
- Snippet library at `snippets/hardscript.json` with 46 snippets registered through
  `contributes.snippets`: functions, async functions, models, a route for each HTTP verb, route
  parameter lists, sockets with `connect`/`message`/`disconnect`, middleware, tests, in-process
  requests, `bring` for each of the ten builtin modules, variables, constants, returns,
  `if`/`if else`, `loop`, `pick`, `race`, `wait`, `expect`, object and list literals, and the
  `websocket` helpers.
- Debug Adapter Protocol adapter at `debugAdapter/hardDebugAdapter.js` speaking DAP over stdio. It
  implements `initialize`, `launch`/`attach`, `configurationDone`, `setBreakpoints` as verified
  markers, `setFunctionBreakpoints`, `setExceptionBreakpoints`, `threads`, `stackTrace` with a
  single frame, `scopes`/`variables` with an empty `Locals` scope, `continue`, `next`, `stepIn`,
  `stepOut`, `pause`, `evaluate`, `terminate`, and `disconnect`. It spawns `hard run <file>` with
  the launch configuration arguments, environment, and working directory, streams program stdout and
  stderr as output events, and starts the program in its own process group so terminating a session
  never orphans the compiled server.
- `DebugAdapterDescriptorFactory` registration in `extension.js` and `debugAdapterExecutable` plus
  the required `program` attribute in the `hardscript` debugger contribution.
- `DebugConfigurationProvider` that defaults `program`, `cwd`, `args`, and `hardPath`.
- Launch configuration attributes `hardPath`, `env`, `envFile`, `serverMode`, and `serverPort`.
  `serverMode` and `serverPort` append `--serverMode=stdio` and `--serverPort=0` to the program
  arguments and are omitted entirely when unset, so existing launch configurations keep working.
- Custom `debug` task command resolved to a `CustomExecution` that starts a debug session, making the
  debug path reachable from **Tasks: Run Task**.
- `provideTasks` implementation that discovers `hard.toml` entry points and `example/*.hard` or
  `examples/<name>/main.hard` files and offers workspace-scoped `build`, `run`, `test`, and `format`
  tasks for each of them. `resolveTask` now allows the full documented set
  (`build`, `run`, `test`, `format`, `doctor`, `debug`).
- `.vscode/launch.json` with the Extension Development Host configuration and sample HardScript
  debug configurations, and `.vscode/tasks.json` with check, build, run, test, format, doctor, and
  debug tasks. Narrow `.gitignore` exceptions track only these two files.
- `npm run check` script backed by `scripts/check.js`: it runs `node --check` on every `.js` file in
  the project, including the debug adapter, and validates every `.json` file. It is dependency-free
  and runs on Linux, macOS, and Windows.

### Changed

- **HardScript: Explain Error** now calls `workspace/executeCommand` with the
  `hardscript.explainError` command and the `HSxxxx` code as its argument, replacing the custom
  `hardscript/explain` requests that the server never answered. The `hard errors --markdown`
  catalog and the active diagnostic message remain as fallbacks.
- The language client document selector now includes `untitled:hardscript` documents.
- The extension version is 0.5.0 in `package.json` and `package-lock.json`.
- `README.md` documents the grammar, snippets, tasks, debugging, commands, and settings.

## [0.1.0]

### Added

- Initial extension with the `hardscript` language, language-configuration, LSP client for
  `hs-lsp`, `hard` CLI commands, the `hardscript` task provider, the `$hardscript` problem matcher,
  and a placeholder `hardscript` debug type.
