#!/usr/bin/env python3
"""Phase 3 codegen/snapshot case generator.

Emits valid programs covering the full v0.1 surface under qa/codegen_cases/
plus qa/codegen_cases/cases.json recording which routes to exercise.

Response assertions are self-healing: the runner records actual responses on
first run in qa/codegen_snapshots/<name>.resp and fails if a later run
differs. Determinism of the emitted C++ is checked by the runner.
"""
import json
import os

OUT = os.path.join(os.path.dirname(__file__), "codegen_cases")
os.makedirs(OUT, exist_ok=True)
cases = {}
P = 3031


def add(name, src, routes=None):
    ports = f"app @{P}\n"
    cases[name] = {"routes": routes or []}
    with open(os.path.join(OUT, name + ".hard"), "w") as f:
        f.write(ports + src)


add("01-hello", 'GET "/" :: {\n    <- "hello"\n}\n', [{"m": "GET", "p": "/"}])

add("02-param", 'GET "/u/:id" :: (id = Str) {\n    <- "u-" + id\n}\n',
    [{"m": "GET", "p": "/u/42"}, {"m": "GET", "p": "/u/abc"}])

add("03-body", 'POST "/b" :: (body = Obj) {\n    <- body.x\n}\n',
    [{"m": "POST", "p": "/b", "d": '{"x": 7}'}])

add("04-calc",
    "calc add(Num a, Num b) => Num { <- a + b }\n"
    "calc fib(Num n) => Num {\n    ?(n < 2) { <- n }\n    <- fib(n - 1) + fib(n - 2)\n}\n"
    'GET "/add" :: { <- add(20, 22) }\n'
    'GET "/fib" :: { <- fib(10) }\n',
    [{"m": "GET", "p": "/add"}, {"m": "GET", "p": "/fib"}])

add("05-globals",
    'k ::= "k"\nv <- 40\n'
    'GET "/" :: { <- v + 2 }\n',
    [{"m": "GET", "p": "/"}])

add("06-modules",
    'GET "/crypto" :: {\n    <- crypto.sha256bin("abc")\n}\n'
    'GET "/json" :: { <- json.stringify({ "a": 1 }) }\n',
    [{"m": "GET", "p": "/crypto"}, {"m": "GET", "p": "/json"}])

add("07-middleware",
    'before gate :: {\n    ?(false) { <- { "error": "denied" } }\n}\n'
    'GET "/" :: {\n    <- "ok"\n}\n',
    [{"m": "GET", "p": "/"}])

add("08-loop",
    'GET "/" :: {\n    t <- 0\n    loop x => [1, 2, 3, 4] { t <- t + x }\n    <- t\n}\n',
    [{"m": "GET", "p": "/"}])

add("09-pick",
    'GET "/" :: {\n    <- pick 99 { 1 => "one", * => "other" }\n}\n',
    [{"m": "GET", "p": "/"}])

add("10-wait-race",
    "calc slow(Num x) => Num { <- x }\n"
    'GET "/" :: {\n    a <- wait slow(6)\n    race [ slow(1), 2 ]\n    <- a\n}\n',
    [{"m": "GET", "p": "/"}])

add("11-expect",
    'GET "/" :: {\n    expect 1 == 1\n    expect ("a" + "b") == "ab"\n    <- 1\n}\n',
    [{"m": "GET", "p": "/"}])

add("12-struct",
    'GET "/" :: {\n    o <- { "a": { "b": [10, 20, 30] } }\n    <- o.a.b[2]\n}\n',
    [{"m": "GET", "p": "/"}])

add("13-strings",
    'GET "/" :: {\n    <- "line\\n\\tend"\n}\n',
    [{"m": "GET", "p": "/"}])

add("14-logic",
    'GET "/" :: {\n    <- (1 < 2 && 3 != 4) || !false\n}\n',
    [{"m": "GET", "p": "/"}])

add("15-model", "model U = users [\n    id => Int #id,\n    name => Str #unique,\n]\n" +
    'GET "/" :: { <- "ok" }\n',
    [{"m": "GET", "p": "/"}])

add("16-fs",
    'GET "/" :: {\n    fs.write("/tmp/hs_snap.txt", "snap")\n    <- fs.read("/tmp/hs_snap.txt")\n}\n',
    [{"m": "GET", "p": "/"}])

with open(os.path.join(OUT, "cases.json"), "w") as f:
    json.dump(cases, f, indent=1, sort_keys=True)
print(f"wrote {len(cases)} codegen cases")