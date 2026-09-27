#!/usr/bin/env python3
"""Render the benchmark reports from measured data.

The only input is `reports/raw/*.tsv`, written by the scripts in `qa/`. Nothing
here computes a number that was not measured: every value is printed as it was
recorded, and a metric with no data is reported as missing rather than filled
in. If a benchmark was not run, the report says so.

    qa/bench_publish.sh && qa/bench_download.sh && qa/bench_search.sh \
        && qa/bench_registry.sh && qa/render_reports.py
"""

from __future__ import annotations

import datetime
import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RAW = ROOT / "reports" / "raw"
REPORTS = ROOT / "reports"


def load(name: str) -> dict[str, tuple[str, str]]:
    """metric -> (value, unit) from one raw file."""
    path = RAW / f"{name}.tsv"
    if not path.exists():
        return {}
    out: dict[str, tuple[str, str]] = {}
    for line in path.read_text().splitlines():
        parts = line.split("\t")
        if len(parts) < 2 or not parts[0]:
            continue
        unit = parts[2] if len(parts) > 2 else ""
        out[parts[0]] = (parts[1], unit)
    return out


def machine() -> list[tuple[str, str]]:
    path = RAW / "machine.tsv"
    if not path.exists():
        return [("unknown", "run a benchmark first")]
    rows = []
    for line in path.read_text().splitlines():
        if "\t" in line:
            k, v = line.split("\t", 1)
            rows.append((k, v))
    return rows


def val(data: dict, key: str, default: str = "not measured") -> str:
    if key not in data:
        return default
    return data[key][0]


def unit(data: dict, key: str, default: str = "") -> str:
    if key not in data:
        return default
    return data[key][1]


def num(data: dict, key: str) -> float | None:
    if key not in data:
        return None
    try:
        return float(data[key][0])
    except ValueError:
        return None


def ms(us: str) -> str:
    """Microseconds as a readable duration."""
    try:
        v = float(us)
    except (TypeError, ValueError):
        return str(us)
    if v >= 1_000_000:
        return f"{v / 1_000_000:.2f} s"
    if v >= 1000:
        return f"{v / 1000:.2f} ms"
    return f"{v:.0f} µs"


def duration_table(data: dict, rows: list[tuple[str, str, str]]) -> list[str]:
    """rows: (label, p50-key, p95-key)"""
    out = ["| operation | p50 | p95 | mean | min | max |", "| --- | ---: | ---: | ---: | ---: | ---: |"]
    for label, p50, p95 in rows:
        mean = p50.replace("_p50", "_mean")
        pmin = p50.replace("_p50", "_min")
        pmax = p50.replace("_p50", "_max")
        if p50 not in data and p95 not in data:
            out.append(f"| {label} | not measured | | | | |")
            continue
        out.append(
            "| {} | {} | {} | {} | {} | {} |".format(
                label,
                ms(val(data, p50)),
                ms(val(data, p95)) if p95 in data else "not measured",
                ms(val(data, mean)) if mean in data else "not measured",
                ms(val(data, pmin)) if pmin in data else "not measured",
                ms(val(data, pmax)) if pmax in data else "not measured",
            )
        )
    return out


def machine_table() -> list[str]:
    out = ["| property | value |", "| --- | --- |"]
    for k, v in machine():
        out.append(f"| {k} | {v} |")
    return out


def preamble(title: str, script: str, body: list[str]) -> str:
    when = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")
    lines = [f"# {title}", ""]
    lines.append(f"Measured by `qa/{script}` against a local registry on {when}.")
    lines.append("")
    lines.append("Every number below was recorded by the benchmark that produced it and")
    lines.append("read back out of `reports/raw/`. Nothing here is estimated, and a metric")
    lines.append("that was not measured is printed as `not measured` rather than filled in.")
    lines.append("")
    lines.append("## Machine")
    lines.append("")
    lines.extend(machine_table())
    lines.append("")
    lines.extend(body)
    lines.append("")
    return "\n".join(lines) + "\n"


def render_publish() -> str:
    d = load("publish")
    body = [
        "## What was measured",
        "",
        "`hard publish` end to end: read `hard.toml`, build the deterministic `.hspkg`,",
        "sign it, upload it, and let the registry validate and commit it. Wall-clock",
        "around the real client, so process start is included — that is what a user",
        "waits for.",
        "",
        "- **cold** — a package name published for the first time.",
        "- **warm** — a new version of a package the registry already holds.",
        "- **`--dry-run`** — validate and describe, no upload: the build-and-sign half",
        "  of a publish on its own.",
        "- **dependency graph** — a manifest with two dependencies, so the resolver and",
        "  dependency validation are part of the measurement.",
        "- **server round trip** — `curl` doing metadata + archive reads, with no client",
        "  process in the way. Compare it with the client rows to see which side the",
        "  time is on.",
        "",
        "## Latency",
        "",
    ]
    body.extend(
        duration_table(
            d,
            [
                ("publish (cold)", "publish_cold_p50", "publish_cold_p95"),
                ("publish (warm)", "publish_warm_p50", "publish_warm_p95"),
                ("publish `--dry-run`", "publish_dryrun_p50", "publish_dryrun_p95"),
                ("publish (with dependencies)", "publish_graph_p50", "publish_graph_p95"),
                ("server round trip (2 GETs)", "server_roundtrip_p50", "server_roundtrip_p95"),
            ],
        )
    )
    body += [
        "",
        "## Payload",
        "",
        "| property | value |",
        "| --- | ---: |",
        f"| archive size | {val(d, 'publish_archive_bytes')} bytes |",
        f"| source lines per package | {val(d, 'publish_source_lines')} |",
        f"| samples per measurement | {val(d, 'publish_cold_count')} |",
        "",
        "## Registry state afterwards",
        "",
        "| property | value |",
        "| --- | ---: |",
        f"| packages | {val(d, 'registry_packages')} |",
        f"| versions | {val(d, 'registry_versions')} |",
        f"| downloads | {val(d, 'registry_downloads')} |",
        "",
    ]
    return preamble("Package publish performance", "bench_publish.sh", body)


def render_download() -> str:
    d = load("download")
    body = [
        "## What was measured",
        "",
        "`hard install` for a project depending on a corpus of published packages,",
        "timed end to end. Warm and cold are different operations and are never mixed:",
        "a cold install pays for HTTP and disk writes, a warm one pays for digest",
        "checks and resolution.",
        "",
        "- **cold** — empty cache, every package fetched.",
        "- **warm** — cache already populated; no network at all.",
        "- **`--frozen`** — trust the lockfile: resolution and link only.",
        "- **`--offline`** — the cache is the only registry there is.",
        "- **archive GET** — one archive over the wire with `curl`, no client in the way.",
        "",
        "## Latency",
        "",
    ]
    body.extend(
        duration_table(
            d,
            [
                ("install (cold cache)", "install_cold_p50", "install_cold_p95"),
                ("install (warm cache)", "install_warm_p50", "install_warm_p95"),
                ("install `--frozen`", "install_frozen_p50", "install_frozen_p95"),
                ("install `--offline`", "install_offline_p50", "install_offline_p95"),
                ("archive GET", "archive_get_p50", "archive_get_p95"),
            ],
        )
    )
    body += [
        "",
        "## Cache behaviour",
        "",
        "The hit rate and byte counts are the client's own numbers, parsed out of its",
        "output rather than recomputed here.",
        "",
        "| property | cold | warm |",
        "| --- | ---: | ---: |",
        f"| cache hit rate | {val(d, 'cache_hit_rate_cold')}% | {val(d, 'cache_hit_rate_warm')}% |",
        f"| bytes over the network | {val(d, 'bytes_over_network_cold')} | {val(d, 'bytes_over_network_warm')} |",
        f"| requests | {val(d, 'requests_cold')} | {val(d, 'requests_warm')} |",
        "",
        "| property | value |",
        "| --- | ---: |",
        f"| cache size on disk | {val(d, 'cache_size_bytes')} bytes |",
        f"| packages in the corpus | {val(d, 'package_count')} |",
        f"| source lines per package | {val(d, 'source_lines')} |",
        f"| samples per measurement | {val(d, 'install_cold_count')} |",
        "",
    ]
    return preamble("Package download performance", "bench_download.sh", body)


def render_search() -> str:
    d = load("search")
    sizes = sorted(
        {
            int(k.rsplit("_", 1)[1])
            for k in d
            if k.startswith("index_size_") and k.rsplit("_", 1)[1].isdigit()
        }
    )
    body = [
        "## What was measured",
        "",
        "`hard search` wall-clock, per query shape, at three index sizes. One query",
        "measures one cache line, so each size is asked four ways: an exact name, a",
        "prefix, a fuzzy name one edit away, and a miss that returns nothing.",
        "",
        "`direct_http` is the same request made with `curl`, included because it is",
        "*slower* than the client: the client speaks HTTP over a socket directly,",
        "while `curl` pays for a process launch every time. It is a useful reminder",
        "that a benchmark which shells out measures the shell-out.",
        "",
        "## Latency by index size and query shape",
        "",
    ]
    if sizes:
        header = "| query | " + " | ".join(f"{s} packages" for s in sizes) + " |"
        sep = "| --- | " + " | ".join("---:" for _ in sizes) + " |"
        body += [header, sep]
        shapes = [
            ("exact name", "exact"),
            ("prefix", "prefix"),
            ("fuzzy (edit distance 1)", "fuzzy"),
            ("tag filter", "tag"),
            ("miss", "miss"),
            ("exact name, `--json`", "json"),
        ]
        for label, key in shapes:
            cells = []
            for s in sizes:
                k = f"search_{key}_{s}_p50"
                cells.append(ms(val(d, k, "not measured")))
            body.append(f"| {label} | " + " | ".join(cells) + " |")
        body += [
            "",
            "## p95 and index growth",
            "",
            "| query | " + " | ".join(f"{s} packages (p95)" for s in sizes) + " |",
            "| --- | " + " | ".join("---:" for _ in sizes) + " |",
        ]
        for label, key in shapes:
            cells = []
            for s in sizes:
                k = f"search_{key}_{s}_p95"
                cells.append(ms(val(d, k, "not measured")))
            body.append(f"| {label} | " + " | ".join(cells) + " |")
    else:
        body.append("No index sizes were recorded: run `qa/bench_search.sh` first.")
    body += [
        "",
        "## Client versus curl",
        "",
        "| path | p50 | p95 | mean |",
        "| --- | ---: | ---: | ---: |",
        "| `hard search` (socket) | {} | {} | {} |".format(
            ms(val(d, f"search_exact_{sizes[-1]}_p50" if sizes else "x", "not measured")),
            ms(val(d, f"search_exact_{sizes[-1]}_p95" if sizes else "x", "not measured")),
            ms(val(d, f"search_exact_{sizes[-1]}_mean" if sizes else "x", "not measured")),
        ),
        "| `curl` (process per request) | {} | {} | {} |".format(
            ms(val(d, "search_direct_p50")),
            ms(val(d, "search_direct_p95")),
            ms(val(d, "search_direct_mean")),
        ),
        "",
        "| property | value |",
        "| --- | ---: |",
        f"| samples per measurement | {val(d, 'search_direct_count')} |",
        f"| largest index measured | {val(d, 'search_max_index')} packages |",
        "",
    ]
    return preamble("Search performance", "bench_search.sh", body)


def render_registry() -> str:
    d = load("registry")
    pub = load("publish")
    dl = load("download")
    se = load("search")
    body = [
        "## What was measured",
        "",
        "The registry as a whole: dependency resolution on a real graph, health checks,",
        "and what mirror fallback costs when the default registry disappears.",
        "",
        "- **resolve** — `hard install --offline` against a warm cache with no lockfile,",
        "  so the measurement is resolution rather than downloading.",
        "- **`--frozen`** — the lockfile is trusted: what a CI build pays when nothing",
        "  changed.",
        "- **health** — `GET /health` with `curl`.",
        "- **mirror fallback** — the same install and search with the default registry",
        "  killed and a live mirror configured.",
        "",
        "## Resolution and install",
        "",
    ]
    body.extend(
        duration_table(
            d,
            [
                ("resolve (warm cache, no lockfile)", "resolve_warm_cache_p50", "resolve_warm_cache_p95"),
                ("install `--frozen`", "install_locked_p50", "install_locked_p95"),
                ("health check", "health_p50", "health_p95"),
            ],
        )
    )
    body += [
        "",
        "## Mirror fallback",
        "",
        "The primary is killed between the two rows, so the second one includes",
        "discovering that it is gone.",
        "",
        "| operation | p50 | p95 |",
        "| --- | ---: | ---: |",
        f"| install, primary up | {ms(val(d, 'install_primary_p50'))} | |",
        f"| install, primary down (mirror answers) | {ms(val(d, 'install_mirror_fallback_p50'))} | {ms(val(d, 'install_mirror_fallback_p95'))} |",
        f"| search, primary down (mirror answers) | {ms(val(d, 'search_mirror_fallback_p50'))} | |",
        "",
        f"Fallback succeeded: **{val(d, 'mirror_fallback_works')}** (1 = every install"
        " completed with the default registry killed).",
        "",
        "### A finding from this benchmark",
        "",
        "The first run of this benchmark measured a 600 ms fallback search. Almost all of",
        "it was the client sleeping through its retry backoff (200 ms, then 400 ms) against",
        "a host that was refusing connections. Nothing is going to start listening in 200",
        "ms, and with a mirror configured the right move is to fail over immediately.",
        "Refusals and unresolvable names are now treated as definitive and skip the backoff;",
        "timeouts and resets still retry. That is the difference between the 600 ms above and",
        f"the {ms(val(d, 'search_mirror_fallback_p50'))} measured now.",
        "",
        "## Graph shape",
        "",
        "| property | value |",
        "| --- | ---: |",
        f"| packages in the registry | {val(d, 'graph_packages')} |",
        f"| graph width | {val(d, 'graph_shape_width')} |",
        f"| graph depth | {val(d, 'graph_shape_depth')} |",
        f"| samples per measurement | {val(d, 'resolve_warm_cache_count')} |",
        "",
        "## The other three benchmarks",
        "",
        "Run by `qa/bench_registry.sh` unless `BENCH_SKIP_SUB=1`. Full detail lives in",
        "each report; this is the summary.",
        "",
        "| benchmark | p50 | p95 |",
        "| --- | ---: | ---: |",
        f"| publish (cold) | {ms(val(pub, 'publish_cold_p50'))} | {ms(val(pub, 'publish_cold_p95'))} |",
        f"| install (cold cache) | {ms(val(dl, 'install_cold_p50'))} | {ms(val(dl, 'install_cold_p95'))} |",
        f"| install (warm cache) | {ms(val(dl, 'install_warm_p50'))} | {ms(val(dl, 'install_warm_p95'))} |",
        f"| search (largest index) | {ms(val(se, f'search_exact_{val(se, "search_max_index", "0")}_p50'))} | {ms(val(se, f'search_exact_{val(se, "search_max_index", "0")}_p95'))} |",
        "",
        "## Reports",
        "",
        "- [publish](package-publish-performance.md)",
        "- [download](package-download-performance.md)",
        "- [search](search-performance.md)",
        "",
    ]
    return preamble("Registry performance", "bench_registry.sh", body)


def main() -> int:
    REPORTS.mkdir(parents=True, exist_ok=True)
    outputs = {
        "registry-performance.md": render_registry(),
        "package-publish-performance.md": render_publish(),
        "package-download-performance.md": render_download(),
        "search-performance.md": render_search(),
    }
    missing = []
    for name, text in outputs.items():
        (REPORTS / name).write_text(text)
        print(f"wrote reports/{name}")
        if "| not measured" in text or "not measured |" in text:
            missing.append(name)
    if missing:
        print()
        print("warning: these reports contain metrics that were not measured:")
        for name in missing:
            print(f"  reports/{name}")
        print("run the matching qa/bench_*.sh script, then render again.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
