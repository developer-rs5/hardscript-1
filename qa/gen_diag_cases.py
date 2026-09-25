#!/usr/bin/env python3
"""Diagnostics V2 snapshot-case generator (M3.4.7).

Writes one directory per case under qa/diag_cases/<NN>-<name>/ containing:
  main.hard      the program to build (deterministic path inside a temp cwd)
  *.hard         extra modules (multi-file cases: import cycle, local brings)
  invoke         optional, the full CLI invocation line (default `build`)

Each case must terminate in a deterministic stderr with NO_COLOR set and no
path leakage (the runner copies the case into a temp project root and runs
`hard <invoke>` there). Avoid templates that reach codegen/g++ (HS0502
embeds compiler output paths) - every case below is verified to produce
only parse/type/module diagnostics or lint warnings.

Regenerate any time the catalog/renderer changes materially; the golden
files under qa/diag_snapshots/ are then re-recorded with:
    qa/diag_snap.sh record
"""

import os
import shutil

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "diag_cases")

INFO = """# Diagnostics V2 snapshot suite (M3.4.7).
#
# One directory per case -> `hard <invoke>` -> stderr (NO_COLOR) compared
# byte-for-byte against qa/diag_snapshots/<name>.txt. Deterministic 100%:
# every case below avoids HS0502 (native g++ output embeds build paths) and
# reaches only parse/type/module diagnostics or lint warnings.
"""


def w(name, src, invoke="build", extra=None):
    d = os.path.join(ROOT, name)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, "main.hard"), "w") as f:
        f.write(src)
    if invoke != "build":
        with open(os.path.join(d, "invoke"), "w") as f:
            f.write(invoke)
    for sub, content in (extra or {}).items():
        with open(os.path.join(d, sub), "w") as f:
            f.write(content)


CASES = {}

# ---------------- parse & lex ----------------
CASES["parse/001-extra-rbrace"] = "GET \"/\" :: {\n  <- 1\n}\n}\napp @3000\n"
CASES["parse/002-two-junk-top-level"] = "app @3000\n@@\n@@\n"
CASES["parse/003-junk-route-body"] = (
    "GET \"/a\" :: {\n  <- 1\n  ?\n  ?\n  ?\n  <- 2\n}\napp @3000\n"
)
CASES["parse/004-junk-then-route-survives"] = "app @3000\n@@\nGET \"/b\" :: { <- 1 }\n"
CASES["parse/005-unterminated-string"] = 'app @3000\nx ::= "oops\n'
CASES["parse/006-unterminated-paren"] = "app @3000\nx ::= (1 + 2\n"
CASES["parse/007-unclosed-block"] = "app @3000\nGET \"/\" :: {\n  <- 1\n"
CASES["parse/008-bring-without-name"] = "app @3000\nbring\n"
CASES["parse/009-bring-number"] = "app @3000\nbring 42\n"
CASES["parse/010-nesting-too-deep"] = "app @3000\n" + "(" * 260
CASES["parse/011-expression-too-long"] = "app @3000\nx ::= " + " + ".join(
    ["1"] * 300
)
CASES["parse/012-race-cheese"] = "app @3000\nrace [] {\n  <- 1\n}\n"
CASES["parse/013-socket-cheese"] = "app @3000\nsocket {\n  connect {\n    a\n  }\n}\n"
CASES["parse/014-route-missing-path"] = "app @3000\nGET ::\n"
CASES["parse/015-route-missing-colons"] = 'app @3000\nGET "/" { <- 1 }\n'
CASES["parse/016-func-missing-body"] = "app @3000\ncalc f() => Int\n"
CASES["parse/017-func-missing-param-name"] = "app @3000\ncalc f(Int) => Int { <- 1 }\n"
CASES["parse/018-route-param-parse"] = 'GET "/x" :: (id = ) { <- 1 }\napp @3000\n'

# ---------------- typecheck & suggestions ----------------
CASES["type/019-undefined-variable"] = "app @3000\nGET \"/\" :: { <- user_id }\n"
CASES["type/020-undefined-variable-suggestion"] = "app @3000\nGET \"/\" :: { <- usr_id }\n"
CASES["type/021-undefined-function"] = (
    "app @3000\ncalc g() => Int { <- nope() }\nGET \"/\" :: { <- 1 }\n"
)
CASES["type/022-undefined-function-suggestion"] = (
    "app @3000\ncalc greet() => Int { <- 1 }\ncalc g() => Int { <- greets() }\n"
    'GET "/" :: { <- 1 }\n'
)
CASES["type/023-unknown-module-suggestion"] = "app @3000\nbring httpx\n"
CASES["type/024-duplicate-model-related"] = (
    "model User = users [\n  id => Int,\n]\nmodel User = users [\n  id => Int,\n]\n"
)
CASES["type/025-duplicate-route"] = (
    'GET "/" :: { <- 1 }\nGET "/" :: { <- 2 }\napp @3000\n'
)
CASES["type/026-duplicate-middleware"] = (
    "before logger :: { <- 1 }\nbefore logger :: { <- 2 }\napp @3000\n"
)

# ---------------- module graph ----------------
CASES["module/027-bring-local-missing"] = "app @3000\nbring \"./nope\"\n"
CASES["type/047-dup-calc"] = (
    "app @3000\ncalc f() => Int { <- 1 }\ncalc f() => Int { <- 2 }\n"
    "GET \"/\" :: { <- f() }\n"
)
CASES["type/048-dup-test"] = (
    "app @3000\ntest \"t\" { <- 1 }\ntest \"t\" { <- 2 }\nGET \"/\" :: { <- 1 }\n"
)
CASES["type/049-dup-model-a"] = (
    "app @3000\nmodel A = a1 [\n  id => Int,\n]\nmodel A = a2 [\n  id => Int,\n]\n"
    'GET "/" :: { <- 1 }\n'
)

CASES["module/029-missing-input-file"] = "app @3000\n", "build nope.hard"
CASES["module/030-no-input"] = "app @3000\n", "build"

# ---------------- warnings ----------------
CASES["warn/031-unused-global"] = "app @3000\nk ::= 1\nGET \"/\" :: { <- 1 }\n"
CASES["warn/032-unused-local"] = (
    "app @3000\ntest \"t\" {\n  qq ::= 1\n}\nGET \"/\" :: { <- 1 }\n"
)
CASES["warn/033-unused-loop-var"] = None  # loop bindings count as used; not emitted
CASES["warn/034-constant-condition-true"] = (
    "app @3000\ncalc f() => Int {\n  ?(2) { <- 1 }\n  <- 0\n}\nGET \"/\" :: { <- f() }\n"
)
CASES["warn/035-constant-condition-false"] = (
    "app @3000\ncalc f() => Int {\n  ?(false) { <- 1 }\n  <- 2\n}\nGET \"/\" :: { <- f() }\n"
)
CASES["warn/036-always-true-comparison"] = (
    "app @3000\ncalc f() => Int {\n  ?(9 > 2) { <- 1 }\n  <- 0\n}\nGET \"/\" :: { <- f() }\n"
)
CASES["warn/037-always-false-comparison"] = (
    "app @3000\ncalc f() => Int {\n  a ::= 7 < 3\n  <- 0\n}\nGET \"/\" :: { <- f() }\n"
)
CASES["warn/038-unreachable-statement"] = (
    'GET "/a" :: {\n  x ::= 1\n  <- x\n  z ::= 2\n  <- z\n}\napp @3000\n'
)
CASES["warn/039-empty-conditional-block"] = (
    "app @3000\ncalc f() => Int {\n  ?(true) {\n  }\n  <- 0\n}\nGET \"/\" :: { <- f() }\n"
)
CASES["warn/040-mixed-warnings"] = (
    "app @3000\ncalc f() => Int {\n  b ::= 1 == 2\n  <- 0\n}\n"
    "k ::= 9 > 2\nGET \"/\" :: { <- f() }\n"
)

# ---------------- warning policies ----------------
CASES["policy/041-deny-numeric"] = (
    "app @3000\ncalc f() => Int {\n  ?(2) { <- 1 }\n  <- 0\n}\nGET \"/\" :: { <- f() }\n",
    "build --deny 2006",
)
CASES["policy/042-deny-name"] = (
    "app @3000\nk ::= 1\nGET \"/\" :: { <- 1 }\n",
    "build --deny unused-variable",
)
CASES["policy/043-deny-multi"] = (
    "app @3000\ncalc f() => Int {\n  a ::= 7 < 3\n  ?(false) { <- 1 }\n  <- 0\n}\n"
    'GET "/" :: { <- f() }\n',
    "build --deny hs2015,constant-condition",
)
CASES["policy/044-deny-case-insensitive"] = (
    "app @3000\nk ::= 1\nGET \"/\" :: { <- 1 }\n",
    "build --deny Hs2001",
)
CASES["policy/045-warnings-none"] = (
    "app @3000\nk ::= 1\nGET \"/\" :: { <- 1 }\n",
    "build --warnings none",
)
CASES["policy/046-warnings-selector"] = (
    "app @3000\nk ::= 1\ncalc f() => Int {\n  ?(2) { <- 1 }\n  <- 0\n}\n"
    'GET "/" :: { <- f() }\n',
    "build --warnings 2006",
)
CASES["policy/047-unknown-warning-selector"] = (
    "app @3000\n",
    "build --deny bogus",
)
CASES["policy/048-warnings-all-explicit"] = (
    "app @3000\nk ::= 1\nGET \"/\" :: { <- 1 }\n",
    "build --warnings all",
)


# Line-shifted twins of the earliest deterministic cases: every rebuild must
# reproduce the identical span (line/column) in the golden, so these pin down
# that the renderer's coordinates are deterministic across runs and cwd.
def shifted(base, lead):
    if isinstance(base, tuple):
        src, invoke, extra = base
    else:
        src, invoke, extra = base, "build", None
    return "\n" * lead + src, invoke, extra

CASES["warn/050-unused-global-shifted1"] = shifted(
    CASES["warn/031-unused-global"], 2
)
CASES["warn/051-unused-global-shifted5"] = shifted(
    CASES["warn/031-unused-global"], 5
)
CASES["parse/052-rbrace-shifted2"] = shifted(CASES["parse/001-extra-rbrace"], 2)
CASES["parse/053-rbrace-shifted7"] = shifted(CASES["parse/001-extra-rbrace"], 7)
CASES["warn/054-const-cond-shifted3"] = shifted(
    CASES["warn/036-always-true-comparison"], 3
)
CASES["warn/055-unreachable-shifted4"] = shifted(
    CASES["warn/038-unreachable-statement"], 4
)
CASES["type/056-dup-route-shifted2"] = shifted(CASES["type/025-duplicate-route"], 2)
CASES["parse/057-route-missing-colons-shifted2"] = shifted(
    CASES["parse/015-route-missing-colons"], 2
)

CASES["socket/060-dup-socket"] = (
    "app @3000\nsocket s1 {\n  connect :: { <- 1 }\n}\nsocket s1 {\n  connect :: { <- 1 }\n}\n"
    'GET "/" :: { <- 1 }\n'
)
CASES["loop/061-loop-race-in-body"] = (
    "app @3000\nGET \"/\" :: {\n  loop i => [1, 2] {\n    race -> a :: { <- a }\n    <- 0\n  }\n  <- 0\n}\n"
)  # HS0002 expected '[' after 'race' inside loop body
CASES["module/062-bring-local-missing-req"] = (
    "app @3000\nbring \"./utility\"\nGET \"/\" :: { <- 1 }\n"
)  # HS0302 module './utility' not found (missing local file)

def main():
    if os.path.exists(ROOT):
        shutil.rmtree(ROOT)
    os.makedirs(ROOT)
    for key, val in CASES.items():
        if val is None:
            continue
        if isinstance(val, tuple):
            if len(val) == 2:
                src, invoke = val
                extra = None
            else:
                src, invoke, extra = val
        else:
            src, invoke, extra = val, "build", None
        w(key, src, invoke, extra)
    # 030: `build` with no entrypoint in the project root (no main.hard) ->
    # HS0302 `module 'main' not found`. The invoke marker is written even
    # though it equals the default so the runner treats the empty dir as a
    # case.
    d030 = os.path.join(ROOT, "module", "030-no-input")
    os.remove(os.path.join(d030, "main.hard"))
    with open(os.path.join(d030, "invoke"), "w") as f:
        f.write("build")
    with open(os.path.join(ROOT, "INFO.txt"), "w") as f:
        f.write(INFO)
    print(f"wrote {len(CASES)} case dirs under {ROOT}")




CASES["warn/065-mirror-unused-local-route"] = (
    "app @3000\nGET \"/\" :: {\n  unused ::= 1\n  <- 0\n}\n"
)
CASES["warn/066-mirror-unreachable-nested"] = (
    "app @3000\nGET \"/\" :: {\n  ?(true) {\n    <- 1\n    <- 2\n  }\n  <- 0\n}\n"
)

if __name__ == "__main__":
    main()
