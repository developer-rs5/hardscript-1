#!/usr/bin/env python3
import argparse
import json
import pathlib
import statistics
import sys
import time

from lsp_client import LspClient
from lsp_helpers import document_params, file_uri


def percentile(samples, fraction):
    if not samples:
        return 0.0
    ordered = sorted(samples)
    index = min(len(ordered) - 1, max(0, int(round(fraction * (len(ordered) - 1)))))
    return ordered[index]


def summary(samples):
    return {
        "count": len(samples),
        "total_ms": round(sum(samples), 3),
        "min_ms": round(min(samples), 3) if samples else 0.0,
        "p50_ms": round(percentile(samples, 0.50), 3),
        "p90_ms": round(percentile(samples, 0.90), 3),
        "p95_ms": round(percentile(samples, 0.95), 3),
        "max_ms": round(max(samples), 3) if samples else 0.0,
        "mean_ms": round(statistics.fmean(samples), 3) if samples else 0.0,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--corpus", default="qa/corpus")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--output", default="qa/lsp/perf.json")
    parser.add_argument("--slowest", type=int, default=8)
    args = parser.parse_args()

    binary = pathlib.Path(args.binary).resolve()
    corpus = pathlib.Path(args.corpus).resolve()
    files = sorted(corpus.rglob("*.hard"))
    if args.limit:
        files = files[: args.limit]
    if not files:
        print(f"perf scan: no .hard files under {corpus}", file=sys.stderr)
        return 2

    client = LspClient(binary, corpus)
    per_file = []
    try:
        started = time.monotonic()
        result, initialize_ms = client.request(
            "initialize",
            {
                "processId": None,
                "rootUri": file_uri(corpus),
                "workspaceFolders": [{"uri": file_uri(corpus), "name": corpus.name}],
                "capabilities": {"general": {"positionEncodings": ["utf-16"]}},
            },
        )
        client.notify("initialized", {})
        index_started = time.monotonic()
        symbols = []
        while time.monotonic() - index_started < 180.0:
            symbols, _ = client.request("workspace/symbol", {"query": ""})
            if symbols:
                break
            time.sleep(0.02)
        index_ready_ms = (time.monotonic() - index_started) * 1000.0
        for path in files:
            text = path.read_text(encoding="utf-8", errors="replace")
            uri = file_uri(path)
            opened = time.monotonic()
            client.notify("textDocument/didOpen", document_params(uri, 1, text))
            published = client.wait_for_notification(
                "textDocument/publishDiagnostics",
                lambda params: params.get("uri") == uri,
                timeout=120.0,
            )
            diagnostics_ms = (time.monotonic() - opened) * 1000.0
            started = time.monotonic()
            tokens, _ = client.request(
                "textDocument/semanticTokens/full", {"textDocument": {"uri": uri}}, timeout=120.0
            )
            tokens_ms = (time.monotonic() - started) * 1000.0
            client.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
            per_file.append({
                "path": str(path.relative_to(corpus)),
                "bytes": len(text.encode("utf-8")),
                "lines": text.count("\n") + 1,
                "diagnostics": len(published.get("diagnostics", [])),
                "tokens": len(tokens.get("data") or []) // 5 if tokens else 0,
                "diagnostics_ms": round(diagnostics_ms, 3),
                "semantic_tokens_ms": round(tokens_ms, 3),
            })
    finally:
        client.close()

    payload = {
        "tool": "hardscript-lsp-perf-scan",
        "binary": str(binary),
        "corpus": str(corpus),
        "files": len(per_file),
        "bytes": sum(item["bytes"] for item in per_file),
        "lines": sum(item["lines"] for item in per_file),
        "initialize_ms": round(initialize_ms, 3),
        "index_ready_ms": round(index_ready_ms, 3),
        "workspace_symbol_count": len(symbols or []),
        "diagnostics": summary([item["diagnostics_ms"] for item in per_file]),
        "semantic_tokens": summary([item["semantic_tokens_ms"] for item in per_file]),
        "slowest": sorted(
            per_file, key=lambda item: item["diagnostics_ms"], reverse=True
        )[: args.slowest],
        "per_file": per_file,
    }
    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    print(
        "perf scan: {files} files, diagnostics p50 {p50:.1f}ms p95 {p95:.1f}ms max {max:.1f}ms, "
        "tokens p50 {tp50:.1f}ms; results: {output}".format(
            files=payload["files"],
            p50=payload["diagnostics"]["p50_ms"],
            p95=payload["diagnostics"]["p95_ms"],
            max=payload["diagnostics"]["max_ms"],
            tp50=payload["semantic_tokens"]["p50_ms"],
            output=output,
        )
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
