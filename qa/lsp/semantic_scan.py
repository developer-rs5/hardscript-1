#!/usr/bin/env python3
import argparse
import collections
import json
import pathlib
import sys
import time

from lsp_client import LspClient, LspFailure
from lsp_helpers import document_params, file_uri


def initialize(client, root):
    result, latency = client.request(
        "initialize",
        {
            "processId": None,
            "rootUri": file_uri(root),
            "workspaceFolders": [{"uri": file_uri(root), "name": root.name}],
            "capabilities": {"general": {"positionEncodings": ["utf-16"]}},
        },
    )
    client.notify("initialized", {})
    return result, latency


def decode_tokens(data, legend):
    counts = collections.Counter()
    line = 0
    column = 0
    for index in range(0, len(data), 5):
        delta_line, delta_start, length, token_type, token_modifiers = data[index:index + 5]
        if delta_line:
            line += delta_line
            column = delta_start
        else:
            column += delta_start
        for bit in range(token_modifiers.bit_length()):
            if token_modifiers & (1 << bit):
                counts[f"modifier:{legend['tokenModifiers'][bit]}"] += 1
        name = legend["tokenTypes"][token_type] if token_type < len(legend["tokenTypes"]) else f"type:{token_type}"
        counts[f"type:{name}"] += 1
        counts["tokens"] += 1
    return counts, line + 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--corpus", default="qa/corpus")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--output", default="qa/lsp/semantic-tokens.json")
    args = parser.parse_args()

    binary = pathlib.Path(args.binary).resolve()
    corpus = pathlib.Path(args.corpus).resolve()
    files = sorted(corpus.rglob("*.hard"))
    if args.limit:
        files = files[: args.limit]
    if not files:
        print(f"semantic scan: no .hard files under {corpus}", file=sys.stderr)
        return 2

    client = LspClient(binary, corpus)
    index_timings = []
    try:
        started = time.monotonic()
        result, initialize_ms = initialize(client, corpus)
        legend = result["capabilities"]["semanticTokensProvider"]["legend"]
        index_started = time.monotonic()
        symbol_probe = None
        deadline = time.monotonic() + 120.0
        while time.monotonic() < deadline:
            symbols, _ = client.request("workspace/symbol", {"query": ""})
            index_timings.append((time.monotonic() - index_started) * 1000.0)
            if symbols:
                symbol_probe = len(symbols)
                break
            time.sleep(0.05)
        index_ready_ms = (time.monotonic() - index_started) * 1000.0
        if symbol_probe is None:
            raise LspFailure("workspace index did not produce symbols")

        totals = collections.Counter()
        per_file = []
        scanned = 0
        slowest = (0.0, "")
        for path in files:
            text = path.read_text(encoding="utf-8", errors="replace")
            uri = file_uri(path)
            started = time.monotonic()
            client.notify("textDocument/didOpen", document_params(uri, 1, text))
            tokens, _ = client.request("textDocument/semanticTokens/full", {"textDocument": {"uri": uri}})
            client.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
            latency_ms = (time.monotonic() - started) * 1000.0
            if latency_ms > slowest[0]:
                slowest = (latency_ms, str(path.relative_to(corpus)))
            data = tokens.get("data") if tokens else []
            counts, lines = decode_tokens(data or [], legend)
            totals.update(counts)
            per_file.append({
                "path": str(path.relative_to(corpus)),
                "bytes": len(text.encode("utf-8")),
                "lines": text.count("\n") + 1,
                "tokens": counts.get("tokens", 0),
                "latency_ms": round(latency_ms, 3),
            })
            scanned += 1
            if scanned % 50 == 0:
                print(f"semantic scan: {scanned}/{len(files)} files")
    finally:
        client.close()

    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    token_types = {key.split(":", 1)[1]: value for key, value in totals.items() if key.startswith("type:")}
    token_modifiers = {key.split(":", 1)[1]: value for key, value in totals.items() if key.startswith("modifier:")}
    output.write_text(json.dumps({
        "tool": "hardscript-lsp-semantic-scan",
        "binary": str(binary),
        "corpus": str(corpus),
        "files_scanned": scanned,
        "bytes_scanned": sum(item["bytes"] for item in per_file),
        "lines_scanned": sum(item["lines"] for item in per_file),
        "tokens_total": totals.get("tokens", 0),
        "legend_types": legend["tokenTypes"],
        "legend_modifiers": legend["tokenModifiers"],
        "type_counts": token_types,
        "modifier_counts": token_modifiers,
        "initialize_ms": initialize_ms,
        "index_ready_ms": index_ready_ms,
        "workspace_symbol_count": symbol_probe,
        "slowest_file": slowest[1],
        "slowest_latency_ms": round(slowest[0], 3),
        "total_request_ms": round(sum(item["latency_ms"] for item in per_file), 3),
        "per_file": per_file,
    }, indent=2) + "\n", encoding="utf-8")
    print(
        f"semantic scan: {scanned} files, {totals.get('tokens', 0)} tokens, "
        f"{len(token_types)} distinct types, slowest {slowest[1]} {slowest[0]:.1f}ms; results: {output}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
