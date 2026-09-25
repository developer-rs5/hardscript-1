#!/usr/bin/env python3
import argparse
import json
import pathlib
import statistics
import sys
import tempfile
import time

from lsp_client import LspClient, LspFailure
from lsp_helpers import document_params, file_uri, offset_for

FIXTURE_MODEL = """model User = users [
    id => Int #id,
    name => Str,
]
"""

FIXTURE_HANDLERS = """bring "./models/user"

calc greeting() => Str { <- "hi" }
"""

FIXTURE_MAIN = """bring "./handlers"
bring http
app @3032

GET "/" :: { <- { greeting: greeting() } }
"""

FIXTURE_UNIT = """/// Generated fixture for index scaling measurements.
model Item{n} = items{n} [
    id => Int #id,
]

calc total{n}() => Int {{ <- 0 }}

GET "/item{n}" :: {{ <- {{ ok: total{n}() }} }}
"""


def initialize(client, root, workspace_folders=None):
    folders = workspace_folders or [{"uri": file_uri(root), "name": pathlib.Path(root).name}]
    result, latency = client.request(
        "initialize",
        {
            "processId": None,
            "rootUri": file_uri(root),
            "workspaceFolders": folders,
            "capabilities": {"general": {"positionEncodings": ["utf-16"]}},
        },
    )
    client.notify("initialized", {})
    return result, latency


def wait_for_index(client, timeout=180.0):
    started = time.monotonic()
    polls = 0
    symbols = None
    while time.monotonic() - started < timeout:
        symbols, latency = client.request("workspace/symbol", {"query": ""}, timeout=timeout)
        polls += 1
        if symbols:
            return {
                "ready_ms": (time.monotonic() - started) * 1000.0,
                "polls": polls,
                "symbols": len(symbols),
                "first_query_ms": latency,
            }
        time.sleep(0.02)
    raise LspFailure("workspace index did not produce symbols")


def query_latencies(client, queries):
    samples = []
    for query in queries:
        symbols, latency = client.request("workspace/symbol", {"query": query})
        samples.append({"query": query, "ms": round(latency, 3), "symbols": len(symbols or [])})
    return samples


def measure_scaling(binary, counts, root):
    rows = []
    for count in counts:
        with tempfile.TemporaryDirectory(prefix=f"hs-index-{count}-", dir=root) as directory:
            workspace = pathlib.Path(directory)
            for index in range(count):
                (workspace / f"mod_{index:04}.hard").write_text(
                    FIXTURE_UNIT.format(n=index), encoding="utf-8"
                )
            client = LspClient(binary, workspace)
            try:
                _, initialize_ms = initialize(client, workspace)
                index = wait_for_index(client)
                rows.append({
                    "files": count,
                    "bytes": sum(
                        path.stat().st_size for path in sorted(workspace.rglob("*.hard"))
                    ),
                    "initialize_ms": round(initialize_ms, 3),
                    "index_ready_ms": round(index["ready_ms"], 3),
                    "workspace_symbols": index["symbols"],
                    "first_query_ms": round(index["first_query_ms"], 3),
                })
            finally:
                client.close()
    return rows


def measure_multimodule(binary, root):
    workspace = pathlib.Path(tempfile.mkdtemp(prefix="hs-multimodule-", dir=root))
    (workspace / "models").mkdir()
    (workspace / "models" / "user.hard").write_text(FIXTURE_MODEL, encoding="utf-8")
    (workspace / "handlers.hard").write_text(FIXTURE_HANDLERS, encoding="utf-8")
    main_text = FIXTURE_MAIN
    (workspace / "main.hard").write_text(main_text, encoding="utf-8")

    second = pathlib.Path(tempfile.mkdtemp(prefix="hs-multimodule-extra-", dir=root))
    (second / "extra.hard").write_text(
        FIXTURE_UNIT.format(n=999), encoding="utf-8"
    )

    client = LspClient(binary, workspace)
    result = {}
    try:
        _, initialize_ms = initialize(client, workspace)
        index = wait_for_index(client)
        queries = query_latencies(client, ["greeting", "User", "item", "zzz"])
        uri = file_uri(workspace / "main.hard")
        client.notify("textDocument/didOpen", document_params(uri, 1, main_text))
        published = client.wait_for_notification(
            "textDocument/publishDiagnostics",
            lambda params: params.get("uri") == uri,
        )
        call_position = offset_for(main_text, "greeting()")
        definition, definition_ms = client.request(
            "textDocument/definition",
            {"textDocument": {"uri": uri}, "position": call_position},
        )
        hover, hover_ms = client.request(
            "textDocument/hover", {"textDocument": {"uri": uri}, "position": call_position}
        )
        references, references_ms = client.request(
            "textDocument/references",
            {"textDocument": {"uri": uri}, "position": call_position, "context": {"includeDeclaration": True}},
        )
        documents, documents_ms = client.request(
            "workspace/symbol", {"query": "greeting"}
        )
        client.notify(
            "workspace/didChangeWorkspaceFolders",
            {
                "event": {
                    "added": [{"uri": file_uri(second), "name": second.name}],
                    "removed": [],
                }
            },
        )
        added = wait_for_index(client)
        result = {
            "initialize_ms": round(initialize_ms, 3),
            "index_ready_ms": round(index["ready_ms"], 3),
            "workspace_symbols": index["symbols"],
            "query_latencies": queries,
            "diagnostics_count": len(published.get("diagnostics", [])),
            "definition_ms": round(definition_ms, 3),
            "definition_target": definition["uri"] if definition else None,
            "hover_ms": round(hover_ms, 3),
            "hover_present": bool(hover),
            "references_ms": round(references_ms, 3),
            "reference_count": len(references or []),
            "workspace_symbol_query_ms": round(documents_ms, 3),
            "root_added_index_ms": round(added["ready_ms"], 3),
            "root_added_symbols": added["symbols"],
        }
    finally:
        client.close()
    return result, workspace, second


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--corpus", default="qa/corpus")
    parser.add_argument("--counts", default="25,100,400")
    parser.add_argument("--output", default="qa/lsp/index-scan.json")
    args = parser.parse_args()

    binary = pathlib.Path(args.binary).resolve()
    corpus = pathlib.Path(args.corpus).resolve()
    if not binary.exists():
        print(f"index scan: missing binary {binary}", file=sys.stderr)
        return 2
    counts = [int(value) for value in args.counts.split(",") if value.strip()]

    with tempfile.TemporaryDirectory(prefix="hs-index-scan-") as scratch:
        scaling = measure_scaling(binary, counts, scratch)
        print(
            "index scan: scaling "
            + ", ".join(
                f"{row['files']} files {row['index_ready_ms']:.0f}ms/{row['workspace_symbols']} symbols"
                for row in scaling
            )
        )
        multimodule, workspace, second = measure_multimodule(binary, scratch)
        print(
            f"index scan: multi-module {multimodule['workspace_symbols']} symbols, "
            f"definition {multimodule['definition_ms']:.1f}ms, references {multimodule['reference_count']}"
        )

    payload = {
        "tool": "hardscript-lsp-index-scan",
        "binary": str(binary),
        "corpus": str(corpus),
        "scaling": scaling,
        "multimodule": multimodule,
        "multimodule_definition": multimodule.get("definition_target"),
    }
    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    print(f"index scan: results: {output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
