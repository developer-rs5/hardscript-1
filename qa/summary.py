#!/usr/bin/env python3
"""Generate the v0.9-alpha summary from measured data and the git history.

Every number comes from somewhere: the QA run, the benchmark raw files, or
`git log`. Nothing here is typed in by hand except the prose.

    qa/run-registry-tests.sh      # produces reports/qa-run.txt
    qa/bench_*.sh                 # produces reports/raw/*.tsv
    qa/summary.py                 # this
"""

from __future__ import annotations

import datetime
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REPORTS = ROOT / "reports"


def git(*args: str) -> str:
    try:
        return subprocess.run(
            ["git", *args], cwd=ROOT, capture_output=True, text=True, check=False
        ).stdout.strip()
    except Exception:
        return ""


def load(name: str) -> dict[str, str]:
    path = REPORTS / "raw" / f"{name}.tsv"
    if not path.exists():
        return {}
    out: dict[str, str] = {}
    for line in path.read_text().splitlines():
        parts = line.split("\t")
        if len(parts) >= 2 and parts[0]:
            out[parts[0]] = parts[1]
    return out


def ms(key: str, data: dict[str, str]) -> str:
    v = data.get(key)
    if not v:
        return "not measured"
    try:
        f = float(v)
    except ValueError:
        return v
    if f >= 1000:
        return f"{f/1000:.1f} ms"
    return f"{f:.0f} us"


def qa_areas() -> list[tuple[str, str, str, str, str]]:
    """area, suite, checks, minimum, result from reports/qa-run.txt."""
    path = REPORTS / "qa-run.txt"
    if not path.exists():
        return []
    rows: list[tuple[str, str, str, str, str]] = []
    for line in path.read_text().splitlines():
        m = re.match(r"^\| (\w+) \| `([^`]+)` \| (\d+) \| (\d+) \| (.*) \|$", line)
        if m:
            rows.append((m.group(1), m.group(2), m.group(3), m.group(4), m.group(5)))
    return rows


def qa_total() -> int:
    path = REPORTS / "qa-run.txt"
    if not path.exists():
        return 0
    m = re.search(r"\*\*(\d+) checks in total", path.read_text())
    return int(m.group(1)) if m else 0


def qa_cargo() -> str:
    path = REPORTS / "qa-run.txt"
    if not path.exists():
        return "not run"
    m = re.search(r"cargo test --workspace: \*\*(\d+) passed, (\d+) failed\*\*", path.read_text())
    return f"{m.group(1)} passed, {m.group(2)} failed" if m else "not run"


def sanitizer_rows() -> list[tuple[str, str, str]]:
    path = REPORTS / "qa-run.txt"
    if not path.exists():
        return []
    rows = []
    for line in path.read_text().splitlines():
        m = re.match(r"^\| ([a-z]+)\b[^|]*\| ([^|]+) \| (.*) \|$", line)
        if m and m.group(1) in {"asan", "lsan", "tsan", "miri", "ubsan"}:
            label = m.group(1)
            if "undefined" in line:
                label = "miri (undefined behaviour)"
            how = m.group(2).strip().strip("`").replace("` on ", " on ")
            rows.append((label, how, m.group(3)))
    return rows


def commits() -> list[tuple[str, str, str]]:
    out = []
    log = git("log", "--reverse", "--format=%h\t%s\t%ad", "--date=short")
    for line in log.splitlines():
        parts = line.split("\t")
        if len(parts) == 3:
            out.append((parts[0], parts[1], parts[2]))
    return out


def diffstat() -> tuple[int, int, int]:
    num = den = 0
    files = 0
    raw = git("diff", "--numstat", "23c96c7..HEAD")
    for line in raw.splitlines():
        parts = line.split("\t")
        if len(parts) == 3:
            files += 1
            for p in parts[:2]:
                if p.isdigit():
                    num += int(p)
                    den += int(p)
    return files, num, den


def main() -> int:
    pub = load("publish")
    dl = load("download")
    se = load("search")
    reg = load("registry")
    max_index = se.get("search_max_index", "?")
    areas = qa_areas()
    total = qa_total()
    files, added, removed = diffstat()
    when = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d")

    lines: list[str] = []
    add = lines.append

    add("# v0.9-alpha — the registry ecosystem")
    add("")
    add(f"Summary generated on {when} from the QA run, the benchmark data in")
    add("`reports/raw/`, and the git history. Regenerate with `qa/summary.py`.")
    add("")
    add("## What v0.9-alpha is")
    add("")
    add("A package ecosystem: a registry service, a client that can publish,")
    add("download, resolve, search, authenticate, verify signatures, and survive")
    add("the registry going away. Ten commits, one branch, no deployment code")
    add("touched.")
    add("")
    add("```")
    add("hard publish                 build a .hspkg and sign it")
    add("hard add jwt                 resolve, download, verify, link, lock")
    add("hard search --tag auth       ranked search across every mirror")
    add("hard verify jwt@1.10.0       check a signature against the trust store")
    add("hard registry health         is the registry reachable, and with which key")
    add("hard install --offline       from the cache, with no network at all")
    add("```")
    add("")

    add("## The milestones")
    add("")
    add("| commit | what it added |")
    add("| --- | --- |")
    descriptions = {
        "feat(registry): registry server": "the service: HTTP API, SQLite, archives, signing, feeds",
        "feat(registry): publish api": "publishing, ownership, dry runs, `hard publish`",
        "feat(pm): package download client": "cache-first resumable downloads, integrity, `hard download`",
        "feat(pm): dependency resolver v2": "bounded backtracking, lockfile preference, precise errors",
        "feat(registry): authentication": "accounts, sessions, tokens, scopes",
        "feat(pm): registry search": "client search, offline fallback, ranked and explained",
        "feat(registry): package signatures": "trust store, verification modes, lockfile trust fields",
        "feat(registry): mirrors and offline cache": "mirrors, health, sync, cache repair",
        "perf(registry): registry benchmarks": "the harness and four measured reports",
        "test(registry): final registry QA": "the gate, the sanitizers, the docs",
    }
    for sha, subject, date in commits():
        if subject in descriptions:
            add(f"| `{sha}` {date} | {descriptions[subject]} |")
    add("")

    add("## Measured")
    add("")
    add("All from `reports/raw/`, on the machine described in each report.")
    add("")
    add("| operation | p50 | p95 |")
    add("| --- | ---: | ---: |")
    add(f"| publish, cold | {ms('publish_cold_p50', pub)} | {ms('publish_cold_p95', pub)} |")
    add(f"| publish, `--dry-run` | {ms('publish_dryrun_p50', pub)} | {ms('publish_dryrun_p95', pub)} |")
    add(f"| install, 25 packages, cold cache | {ms('install_cold_p50', dl)} | {ms('install_cold_p95', dl)} |")
    add(f"| install, 25 packages, warm cache | {ms('install_warm_p50', dl)} | {ms('install_warm_p95', dl)} |")
    add(f"| install, warm cache, `--frozen` | {ms('install_frozen_p50', dl)} | {ms('install_frozen_p95', dl)} |")
    add(f"| install, `--offline` | {ms('install_offline_p50', dl)} | {ms('install_offline_p95', dl)} |")
    add(f"| search, {max_index}-package index | {ms(f'search_exact_{max_index}_p50', se)} | {ms(f'search_exact_{max_index}_p95', se)} |")
    add(f"| search, miss, {max_index}-package index | {ms(f'search_miss_{max_index}_p50', se)} | {ms(f'search_miss_{max_index}_p95', se)} |")
    add(f"| resolution, 40 packages + 8x5 graph | {ms('resolve_warm_cache_p50', reg)} | {ms('resolve_warm_cache_p95', reg)} |")
    add(f"| mirror fallback, search | {ms('search_mirror_fallback_p50', reg)} | {ms('search_mirror_fallback_p95', reg)} |")
    add(f"| mirror fallback, install | {ms('install_mirror_fallback_p50', reg)} | {ms('install_mirror_fallback_p95', reg)} |")
    add("")
    add("| property | value |")
    add("| --- | ---: |")
    add(f"| cache hit rate, warm install | {dl.get('cache_hit_rate_warm', 'not measured')}% |")
    add(f"| cache hit rate, cold install | {dl.get('cache_hit_rate_cold', 'not measured')}% |")
    add(f"| cache size for 25 packages | {int(dl.get('cache_size_bytes', 0) or 0) // 1024} KiB |")
    add(f"| mirror fallback completed | {reg.get('mirror_fallback_works', 'not measured')} (1 = yes) |")
    add("")
    add("Full detail: [registry](registry-performance.md), "
        "[publish](package-publish-performance.md), "
        "[download](package-download-performance.md), "
        "[search](search-performance.md).")
    add("")

    add("## QA")
    add("")
    add(f"`cargo test --workspace`: **{qa_cargo()}**")
    add("")
    if areas:
        add("| area | suite | checks | minimum | result |")
        add("| --- | --- | ---: | ---: | --- |")
        for area, suite, checks, minimum, result in areas:
            add(f"| {area} | `{suite}` | {checks} | {minimum} | {result} |")
        add("")
    add(f"**{total} checks in total** across the unit tests and every suite.")
    add("")
    srows = sanitizer_rows()
    if srows:
        add("### sanitizers")
        add("")
        add("| check | how | result |")
        add("| --- | --- | --- |")
        for name, how, result in srows:
            add(f"| {name} | `{how}` | {result} |")
        add("")
        add("UBSan does not exist in Rust: `-Zsanitizer` has no `undefined` value.")
        add("Miri is the undefined-behaviour check, and it is reported as Miri.")
        add("")

    add("## Bugs the milestones found")
    add("")
    add("Each of these was found by a test or a benchmark, not by reading code:")
    add("")
    add("| where | what was wrong |")
    add("| --- | --- |")
    add("| `SqliteStore::latest` | ordered versions as *text*, so a package with 1.9.0 and 1.10.0 reported 1.9.0 as latest |")
    add("| `Version: Ord` | ranked a pre-release above its own release (1.10.0-rc.1 > 1.10.0) |")
    add("| registry `/search` | `?tag=` silently degraded into \"match everything\"; `total` equalled the page size |")
    add("| `pm/src/toml.rs` | `[[array.of.tables]]` was parsed as a plain table, so mirror syntax could not be read at all |")
    add("| `pm/src/registry.rs` | a failed `curl` returned `Ok(status 0)`, so every https failure looked like a reply and could not trigger mirror fallback |")
    add("| `pm/src/registry.rs` | a refused connection was retried with a 200ms+400ms backoff: 600ms of sleeping before every fallback |")
    add("| resolver | yanked versions were never filtered, so `hard yank` had no effect on a fresh install |")
    add("| `hard install` | ignored the manifest's `[registry] default` while `hard search` used it |")
    add("| `hard download` | `--force` did not force anything and `--output` wrote nothing |")
    add("| `Cache::repair` | removed the archive but left the extracted sources, so stale code could be linked |")
    add("| install verification | under `warn`, a failed check produced no warning at all |")
    add("")

    add("## Size")
    add("")
    add(f"- {files} files changed across the ten commits")
    add(f"- {added:,} lines added, {removed:,} removed")
    add(f"- {len(commits())} commits on the branch")
    add("")

    add("## Known limits")
    add("")
    add("- **The signing key is a test key by default.** A real registry must")
    add("  supply `HARD_REGISTRY_SIGNING_KEY`; `/keys` and `hard doctor` both say")
    add("  so out loud.")
    add("- **No TLS in the registry itself.** Put it behind a reverse proxy.")
    add("- **PBKDF2 at 100 000 iterations**, not Argon2id. The work factor is one")
    add("  constant in `hard-registry/src/security.rs`.")
    add("- **Names are global per registry.** No namespaces.")
    add("- **One lockfile resolves against one set of registries.**")
    add("- **`hard install` has no install scripts.** A package is data.")
    add("- **Sanitizer runs cover the unit and integration tests, not the shell")
    add("  suites**: the shell suites spawn a release binary, which is not built")
    add("  with a sanitizer.")
    add("")

    add("## Reports")
    add("")
    add("- [registry-performance.md](registry-performance.md)")
    add("- [package-publish-performance.md](package-publish-performance.md)")
    add("- [package-download-performance.md](package-download-performance.md)")
    add("- [search-performance.md](search-performance.md)")
    add("- [registry-qa-report.md](registry-qa-report.md)")
    add("- [qa-run.txt](qa-run.txt) — the machine-readable QA run")
    add("- [../docs/PM.md](../docs/PM.md) — the package manager")
    add("- [../docs/REGISTRY.md](../docs/REGISTRY.md) — the registry service")
    add("")

    (REPORTS / "v0.9-alpha-summary.md").write_text("\n".join(lines) + "\n")
    print("wrote reports/v0.9-alpha-summary.md")
    return 0


if __name__ == "__main__":
    sys.exit(main())
