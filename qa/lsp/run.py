#!/usr/bin/env python3
import argparse
import json
import os
import pathlib
import sys
import tempfile
import time

from lsp_client import LspClient, LspFailure
from lsp_helpers import document_params, file_uri, offset_for, open_document


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--output", default="qa/lsp/results.json")
    args = parser.parse_args()
    binary = pathlib.Path(args.binary).expanduser().resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        print(f"lsp qa: server binary is not executable: {binary}", file=sys.stderr)
        return 2

    main_source = '''/// Entry point
bring json

model User = users [
    id => Int #id,
    name => Str #name,
]

calc add(Int a, Int b) => Int {
    total <- a + b
    <- total
}

GET "/users" :: (id = Int) {
    <- { id: 1 }
}

value <- add(1, 2)
'''
    format_source = "calc add(Int a,Int b)=>Int {<-(a+b)}\n"
    broken_source = "calc broken( => Int {}\n"
    indexed_source = '''/// Indexed only, never opened
calc indexed_target(Int value) => Int {
    <- value
}
'''
    with tempfile.TemporaryDirectory(prefix="hs-lsp-qa-") as temporary:
        workspace = pathlib.Path(temporary)
        (workspace / "main.hard").write_text(main_source, encoding="utf-8")
        (workspace / "format.hard").write_text(format_source, encoding="utf-8")
        (workspace / "broken.hard").write_text(broken_source, encoding="utf-8")
        (workspace / "indexed.hard").write_text(indexed_source, encoding="utf-8")
        main_uri = file_uri(workspace / "main.hard")
        format_uri = file_uri(workspace / "format.hard")
        broken_uri = file_uri(workspace / "broken.hard")
        indexed_uri = file_uri(workspace / "indexed.hard")
        root_uri = file_uri(workspace)
        client = LspClient(binary, workspace)
        records = []
        failures = []
        initialize_ms = 0.0

        def case(name, action):
            started = time.monotonic()
            try:
                detail = action(client)
            except Exception as error:
                failures.append(f"{name}: {error}")
                records.append({
                    "name": name,
                    "status": "fail",
                    "duration_ms": (time.monotonic() - started) * 1000.0,
                    "detail": str(error),
                })
                print(f"lsp qa: FAIL {name}: {error}", file=sys.stderr)
            else:
                records.append({
                    "name": name,
                    "status": "pass",
                    "duration_ms": (time.monotonic() - started) * 1000.0,
                    "detail": str(detail) if detail is not None else None,
                })
                print(f"lsp qa: pass {name}")

        try:
            result, initialize_ms = client.request(
                "initialize",
                {
                    "processId": os.getpid(),
                    "rootUri": root_uri,
                    "workspaceFolders": [{"uri": root_uri, "name": "lsp-qa"}],
                    "clientInfo": {"name": "hardscript-lsp-qa", "version": "0.5.0"},
                    "capabilities": {
                        "general": {"positionEncodings": ["utf-16"]},
                        "workspace": {"workspaceFolders": True},
                    },
                },
            )
            if not result or result.get("capabilities", {}).get("positionEncoding") != "utf-16":
                raise LspFailure("initialize did not advertise UTF-16 positions")
            capabilities = result.get("capabilities", {})
            for name in ("hoverProvider", "definitionProvider", "referencesProvider", "renameProvider", "documentSymbolProvider", "workspaceSymbolProvider"):
                if not capabilities.get(name):
                    raise LspFailure(f"initialize capability missing: {name}")
            client.notify("initialized", {})
            open_document(client, main_uri, 1, main_source)

            def hover(action_client):
                value, latency = action_client.request(
                    "textDocument/hover",
                    {"textDocument": {"uri": main_uri}, "position": offset_for(main_source, "total <- a")},
                )
                contents = json.dumps(value)
                if not value or "total" not in contents or "variable" not in contents:
                    raise LspFailure(f"unexpected hover: {value}")
                return f"{latency:.2f}ms"

            case("hover", hover)

            def definition(action_client):
                value, _ = action_client.request(
                    "textDocument/definition",
                    {"textDocument": {"uri": main_uri}, "position": offset_for(main_source, "a + b")},
                )
                if not value:
                    raise LspFailure("definition returned null")
                return json.dumps(value)[:160]

            case("definition", definition)

            def references(action_client):
                value, _ = action_client.request(
                    "textDocument/references",
                    {
                        "textDocument": {"uri": main_uri},
                        "position": offset_for(main_source, "total <- a"),
                        "context": {"includeDeclaration": True},
                    },
                )
                if not isinstance(value, list) or len(value) < 2:
                    raise LspFailure(f"references returned {value!r}")
                return len(value)

            case("references", references)

            def completion(action_client):
                position = offset_for(main_source, "total <- a", len("    tot"))
                value, _ = action_client.request(
                    "textDocument/completion",
                    {"textDocument": {"uri": main_uri}, "position": position},
                )
                labels = [item.get("label") for item in (value or []) if isinstance(item, dict)]
                if "total" not in labels:
                    raise LspFailure(f"completion labels: {labels[:20]}")
                return len(labels)

            case("completion", completion)

            def document_symbols(action_client):
                value, _ = action_client.request("textDocument/documentSymbol", {"textDocument": {"uri": main_uri}})
                names = [item.get("name") for item in (value or []) if isinstance(item, dict)]
                if "add" not in names:
                    raise LspFailure(f"document symbols: {names}")
                return len(names)

            case("document_symbols", document_symbols)

            def semantic_tokens(action_client):
                value, _ = action_client.request("textDocument/semanticTokens/full", {"textDocument": {"uri": main_uri}})
                if not value or not value.get("data"):
                    raise LspFailure(f"semantic tokens: {value!r}")
                return len(value["data"]) // 5

            case("semantic_tokens", semantic_tokens)

            def signature_help(action_client):
                value, _ = action_client.request(
                    "textDocument/signatureHelp",
                    {"textDocument": {"uri": main_uri}, "position": offset_for(main_source, "add(1, 2)", len("add("))},
                )
                if not value or not value.get("signatures"):
                    raise LspFailure(f"signature help: {value!r}")
                return value.get("activeParameter")

            case("signature_help", signature_help)

            def prepare_rename(action_client):
                value, _ = action_client.request(
                    "textDocument/prepareRename",
                    {"textDocument": {"uri": main_uri}, "position": offset_for(main_source, "total <- a")},
                )
                if not value:
                    raise LspFailure("prepareRename returned null")
                return value

            case("prepare_rename", prepare_rename)

            def rename(action_client):
                value, _ = action_client.request(
                    "textDocument/rename",
                    {
                        "textDocument": {"uri": main_uri},
                        "position": offset_for(main_source, "total <- a"),
                        "newName": "sum",
                    },
                )
                if not value or not value.get("changes"):
                    raise LspFailure(f"rename returned {value!r}")
                return len(value["changes"])

            case("rename", rename)

            def format_document(action_client):
                open_document(action_client, format_uri, 1, format_source)
                value, _ = action_client.request(
                    "textDocument/formatting",
                    {
                        "textDocument": {"uri": format_uri},
                        "options": {"tabSize": 4, "insertSpaces": True},
                    },
                )
                if value is None:
                    raise LspFailure("formatting returned null")
                return len(value)

            case("formatting", format_document)

            def code_action(action_client):
                diagnostics = open_document(action_client, broken_uri, 1, broken_source)
                items = diagnostics.get("diagnostics", []) if diagnostics else []
                if not items:
                    raise LspFailure("broken document published no diagnostics")
                value, _ = action_client.request(
                    "textDocument/codeAction",
                    {
                        "textDocument": {"uri": broken_uri},
                        "range": items[0]["range"],
                        "context": {"diagnostics": items},
                    },
                )
                if not value:
                    raise LspFailure("code action returned no actions")
                return value[0].get("title")

            case("code_action", code_action)

            def execute_command(action_client):
                value, _ = action_client.request(
                    "workspace/executeCommand",
                    {"command": "hardscript.explainError", "arguments": ["HS0002"]},
                )
                if not isinstance(value, str) or "HS0002" not in value:
                    raise LspFailure(f"explain result: {value!r}")
                return len(value)

            case("execute_command", execute_command)

            def workspace_symbols(action_client):
                value, _ = action_client.request("workspace/symbol", {"query": "add"})
                if not value or not any(item.get("name") == "add" for item in value):
                    raise LspFailure(f"workspace symbols: {value!r}")
                return len(value)

            case("workspace_symbols", workspace_symbols)

            def workspace_index(action_client):
                deadline = time.monotonic() + 10.0
                value = None
                while time.monotonic() < deadline:
                    value, _ = action_client.request("workspace/symbol", {"query": "indexed_target"})
                    if value:
                        break
                    time.sleep(0.1)
                if not value or not any(item.get("name") == "indexed_target" for item in value):
                    raise LspFailure(f"background index did not index indexed.hard: {value!r}")
                return value[0]["location"]["uri"].rsplit("/", 1)[-1]

            case("workspace_index", workspace_index)

            def did_change(action_client):
                changed = main_source.replace("value <- add(1, 2)", "value <- add(2, 3)")
                action_client.notify(
                    "textDocument/didChange",
                    {
                        "textDocument": {"uri": main_uri, "version": 2},
                        "contentChanges": [{"text": changed}],
                    },
                )
                params = action_client.wait_for_notification(
                    "textDocument/publishDiagnostics",
                    lambda value: value.get("uri") == main_uri and value.get("version") == 2,
                )
                return len(params.get("diagnostics", []))

            case("did_change", did_change)

        finally:
            return_code = client.close()

    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps({
        "tool": "hardscript-lsp-qa",
        "server": str(binary),
        "initialize_ms": initialize_ms,
        "return_code": return_code,
        "passed": len(records) - len(failures),
        "failed": len(failures),
        "records": records,
        "stderr": client.stderr,
    }, indent=2) + "\n", encoding="utf-8")
    print(f"lsp qa: {len(records) - len(failures)} passed, {len(failures)} failed; results: {output}")
    if failures:
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    if return_code not in (0, None):
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
