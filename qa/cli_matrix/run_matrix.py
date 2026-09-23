#!/usr/bin/env python3
"""Phase 5 — run the CLI matrix against the real `hard` binary.

For each row in matrix.tsv: cd into its sandbox case dir (cases/<id>/),
run `hard <argv>`, record exit code + first stderr line, and compare with
expected_rc. Writes:
  qa/cli_matrix/results.tsv  id  argv  expected  actual  status  first_stderr
  qa/cli_matrix/results.json (machine-readable)

status = PASS (actual == expected) | FINDING (mismatch) | DISCOV (expected not
asserted by design — recorded for regression).

Usage:  run_matrix.py <path-to-hard-binary>
"""

import csv, json, os, shutil, signal, subprocess, sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
import gen_matrix as _G
CASES = ROOT / "qa" / "cli_matrix" / "cases"
TSV = ROOT / "qa" / "cli_matrix" / "matrix.tsv"
OUT_TSV = ROOT / "qa" / "cli_matrix" / "results.tsv"
OUT_JSON = ROOT / "qa" / "cli_matrix" / "results.json"

BIN = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / "target/release/hard")
TIMEOUT = 25  # seconds; run/bench lanes self-terminate after warmup


def main():
    with TSV.open() as f:
        rows = list(csv.DictReader(f, delimiter="\t"))
    extras = ROOT / "qa" / "cli_matrix" / "extras.tsv"
    if extras.exists():
        with extras.open() as f:
            rows += list(csv.DictReader(f, delimiter="\t"))
    print(f"run: {len(rows)} lanes (matrix + extras)")

    results = []
    for r in rows:
        cid = r["id"]
        argv = r["argv"].split()
        d = CASES / cid
        if d.is_dir():
            import shutil as _sh
            _sh.rmtree(d, ignore_errors=True)
        d.mkdir(parents=True, exist_ok=True)
        _sh.rmtree(d, ignore_errors=True)
        d.mkdir(parents=True, exist_ok=True)
        try:
            _G.SETUPS[r.get("setup", "missing")](d)
        except KeyError:
            pass
        if argv and argv[0] == "run":
            # A timeout-killed `hard run` orphans its `.hard/main` server,
            # which keeps holding port 3000 and breaks later run lanes.
            # Reap any orphaned server processes (scan /proc cmdlines).
            for pd in Path("/proc").glob("[0-9]*"):
                try:
                    cmd = (pd / "cmdline").read_bytes().replace(b"\0", b" ")
                except OSError:
                    continue
                if b".hard/main" in cmd:
                    try:
                        os.kill(int(pd.name), signal.SIGKILL)
                    except (OSError, ProcessLookupError):
                        pass
            shutil.rmtree(d / ".hard", ignore_errors=True)
        try:
            p = subprocess.run(
                [BIN] + argv,
                cwd=d, capture_output=True, text=True, timeout=TIMEOUT,
            )
            rc, stderr = p.returncode, (p.stderr or "").strip().splitlines()
        except subprocess.TimeoutExpired:
            status = "PASS" if r["expected_rc"] == "124" else "TIME_OUT"
            results.append([cid, r["argv"], r["expected_rc"], "124",
                            status, "stayed up past %ds" % TIMEOUT])
            continue
        first_err = stderr[0] if stderr else ""
        exp = int(r["expected_rc"])
        status = "PASS" if rc == exp else "FINDING"
        results.append([cid, r["argv"], str(exp), str(rc), status, first_err])

    with OUT_TSV.open("w") as f:
        f.write("\t".join(["id", "argv", "expected_rc", "actual_rc",
                           "status", "first_stderr"]) + "\n")
        for row in results:
            f.write("\t".join(row) + "\n")

    js = [dict(zip(["id", "argv", "expected_rc", "actual_rc", "status",
                    "first_stderr"], row)) for row in results]
    JSON_OUT = OUT_JSON  # rename for clarity
    with JSON_OUT.open("w") as f:
        json.dump(js, f, indent=2)

    total = len(results)
    passed = sum(1 for r in results if r[4] == "PASS")
    findings = sum(1 for r in results if r[4] != "PASS")
    print(f"cases: {total}  pass: {passed}  findings: {findings}")
    print(f"tsv: {OUT_TSV.relative_to(ROOT)}  json: {JSON_OUT.relative_to(ROOT)}")
    for r in results:
        if r[4] == "FINDING":
            print(f"  FINDING {r[0]:<8} argv={r[1]:<48} exp={r[2]} got={r[3]} | {r[5]}")


if __name__ == "__main__":
    main()
