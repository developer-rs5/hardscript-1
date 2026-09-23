#!/usr/bin/env python3
"""Generate the QA torture corpora.

Outputs:
  qa/corpus/p1_parse/N-*.hard   Phase 1 parser torture (200+ files)
  qa/corpus/p2_type/N-*.hard    Phase 2 type checker torture (150+ files)
"""
import os
import random
import sys

ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "corpus")
random.seed(20260923)


def w(nm, body):
    path = os.path.join(ROOT, nm)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(body)


# ---------------------------------------------------------------------------
# PHASE 1 — parser torture
# ---------------------------------------------------------------------------
def gen_parse():
    n = 0
    def add(nm, body):
        nonlocal n
        n += 1
        w(f"p1_parse/{n:03d}-{nm}.hard", body)

    # 1. empty / near-empty inputs
    add("empty", "")
    add("whitespace", "   \n\t\n  \n")
    add("only-comments", "# just a comment\n// hello\n/* block */\n")
    add("only-bring", "bring http\n")
    add("only-app", "app @3000\n")
    add("only-model-empty", "model M = m [\n]\n")
    add("only-blank-route", 'GET "/" :: {}\n')
    add("nul-bytes", "\x00\x01\x02\x03\n")
    add("bom-at-start", "\ufeffbring http\n")

    # 2. braces / blocks
    add("stray-close", "}\n")
    add("stray-open", "{\n")
    add("unclosed-brace-route", 'GET "/" :: {\n    x <- 1\n')
    add("extra-close-route", 'GET "/" :: }\n')
    add("nested-empty", "{\n  {\n    {\n    }\n  }\n}\n")
    add("mismatched", "([)]\n")
    add("only-rparen", ")\n")
    add("only-rbracket", "]\n")
    add("semicolons", ";;;\n")
    add("deep-open-10", "{\n" * 10 + "}\n" * 10)
    add("deep-open-120", "{\n" * 120 + "}\n" * 120)
    add("deep-open-1000", "{\n" * 1000 + "}\n" * 1000)
    add("deep-open-10000", "{\n" * 10000 + "}\n" * 10000)
    add("deep-nested-expr", "(" * 200 + "1" + ")" * 200 + "\n")
    add("deep-nested-expr2", "(" * 5000 + "1" + ")" * 5000 + "\n")

    # 3. routes — invalid syntax
    add("route-no-method", '"/" :: {}\n')
    add("route-no-path", 'GET :: {}\n')
    add("route-no-colon", 'GET "/"\n{}\n')
    add("route-single-colon", 'GET "/" : {}\n')
    add("route-params-before", '(id = Str) :: {}\n')
    add("route-params-mixed", 'GET "/" :: (id = Str, name) {}\n')
    add("route-params-no-type", 'GET "/" :: (id) {}\n')
    add("route-params-dup", 'GET "/" :: (id = Str, id = Str) {}\n')
    add("route-params-int", 'GET "/" :: (id = Int) {}\n')
    add("route-params-comma-end", 'GET "/" :: (id = Str,) {}\n')
    add("route-path-number", "GET 123 :: {}\n")
    add("route-path-emoji", 'GET "🎉" :: {}\n')
    add("route-empty-string", 'GET "" :: {}\n')
    add("route-wildcard", 'GET "/*" :: {}\n')
    add("route-wrong-method", 'FROB "/" :: {}\n')
    add("route-lower-method", 'get "/" :: {}\n')
    add("route-dup-exact", 'GET "/" :: {}\nGET "/" :: {}\n')
    add("route-dup-param", 'GET "/x" :: (id = Str) { p <- id }\nGET "/x" :: (id = Str) { p <- id }\n')
    add("route-no-app-server", 'GET "/" :: { x <- 1 }\n')

    # 4. models — invalid syntax
    add("model-no-table", "model M [ id => Int #id, ]\n")
    add("model-table-string", 'model M = "m" [ id => Int #id, ]\n')
    add("model-no-comma", "model M = m [\n  id => Int #id\n  name => Str\n]\n")
    add("model-bad-field-type", "model M = m [ id => List, ]\n")
    add("model-field-no-arrow", "model M = m [ id Int, ]\n")
    add("model-field-arrow-wrong", "model M = m [ id <- Int, ]\n")
    add("model-dup-field", "model M = m [ a => Int, a => Int, ]\n")
    add("model-dup-pk", "model M = m [ a => Int #id, b => Str #id, ]\n")
    add("model-empty-table", "model M =  [ id => Int, ]\n")
    add("model-dup-name", "model M = m [ id => Int #id, ]\nmodel M = n [ id => Int #id, ]\n")
    add("model-pk-int", "model M = m [ id => Str #id, ]\n")
    add("model-bool-field", "model M = m [ ok => Bool, ]\n")
    add("model-json-field", "model M = m [ meta => Json, ]\n")
    add("model-nested", "model M = m [\n  id => Int #id,\n  inner => m2 [\n    x => Int,\n  ],\n]\n")

    # 5. functions — invalid syntax
    add("func-no-arrow", "calc f(x) { x }\n")
    add("func-no-parens", "calc f x => Int { x }\n")
    add("func-no-body", "calc f(x) => Int\n")
    add("func-dup-name", "calc f(x) => Int { x }\ncalc f(y) => Int { y }\n")
    add("func-dup-param", "calc f(x, x) => Int { x }\n")
    add("func-param-literal", "calc f(1) => Int { 1 }\n")
    add("func-return-missing", "calc f(x) => Int {}\n")
    add("func-bad-ret-type", "calc f(x) => Bogus { x }\n")
    add("func-void-ret", "calc f(x) => Void { x }\n")
    add("func-async-no-await", "async calc f() => Int { <- 55 }\n")

    # 6. misc statements
    add("test-no-name", 'test { expect 1 == 1 }\n')
    add("test-name-string", 'test "aha" {\n    expect 1 == 1\n}\n')
    add("expect-no-op", "test t { expect }\n")
    add("expect-raw", "test t { expect 1 2 }\n")
    add("expect-notbool", "test t { expect 1 == 1 && }\n")
    add("middleware-no-name", "before {\n    x <- 1\n}\n")
    add("loop-no-iter", "loop 5 {\n    x <- 1\n}\n")
    add("loop-string-iter", 'loop "x" {\n    a <- 1\n}\n')
    add("loop-list-iter", "loop { 1, 2 } {\n    a <- 1\n}\n")
    add("race-ok-2", "race {\n    a <- 1\n} {\n    b <- 2\n}\n")
    add("race-1-arm", "race {\n    a <- 1\n}\n")
    add("race-0-arm", "race\n")
    add("pick-dup", "pick { 1, 2, 1 }\n")
    add("assign-in-route", 'GET "/" :: {\n    x <- 1\n    x <- 2\n}\n')

    # 7. arbitrary junk / random tokens
    for i in range(30):
        junk = " ".join(random.choice([
            str(random.randint(-10**9, 10**9)),
            '"%s"' % "".join(random.choice("abc012 ._") for _ in range(random.randint(0, 8))),
            "GET", "->", "<-", "::", "}", "{", "(", ")", "app", "model", "calc",
            "socket", "test", "expect", "==", "=>", "=", ",", ":", ".", "..", "...",
        ]) for _ in range(random.randint(1, 40)))
        add(f"junk-{i:02d}", junk + "\n")

    # 8. comments everywhere
    add("comments-everywhere",
        "# top\n# GET blocked\napp @81 # port\n\n"
        "model Table = t [ # comment\n  id => Int #id,# c\n  name => Str, # n\n]\n"
        "secret <- 1 # x\n")
    add("comment-multiline-str", 'GET "/a#b" :: {}\n')
    add("comment-nested", "# # ##\n// // //\n")

    # 9. unicode identifiers / content
    add("unicode-identifiers", "héllo <- 1\nλ <- 2\n变量 <- 3\ndasd <- héllo + λ\n")
    add("unicode-route", 'GET "/héllo" :: {\n    λ <- 42\n}\n')
    add("unicode-string", 'GET "/" :: {\n    x <- "日本語テキスト 🐱"\n}\n')
    add("unicode-mixed", "模型 -> 🚀\n")
    add("rtl-text", 'GET "/" :: {\n    x <- "مرحبا بالعالم"\n}\n')

    # 10. very large files
    big_lines = ["# line %d — padding padding padding %d" % (i, i) for i in range(10000)]
    add("large-10000-lines", "\n".join(big_lines) + "\n")
    huge = ["GET \"/r%d\" :: { __ <- %d }" % (i, i) for i in range(5000)]
    add("large-5000-routes", "\n".join(huge) + "\n")
    add("huge-single-string", 'GET "/" :: {\n    x <- "' + "a" * 200000 + '"\n}\n')
    add("huge-list", 'GET "/" :: {\n    x <- [' + ", ".join(str(i) for i in range(50000)) + "]\n}\n")

    # 11. keywords as identifiers
    add("kw-as-var", "GET <- 1\n")
    for kw in ["bring", "app", "model", "calc", "test", "expect", "socket", "loop",
               "race", "pick", "async", "wait", "before", "connect", "message"]:
        add(f"kw-var-{kw}", f"{kw} <- 1\n")
    add("keywords-in-route-body", 'GET "/" :: {\n    msg <- "x"\n}\n')

    # 12. missing bits and pieces
    add("model-and-route-no-app",
        'model M = m [ id => Int #id, ]\nGET "/" :: { x <- 1 }\n')
    add("route-in-model", 'model M = m [ id => Int #id, GET "/" :: {}, ]\n')
    add("make-garbage-capitals", 'get "/" :: {}\nPut "/" :: {}\n')
    add("space-in-ident", "my var <- 1\n")
    add("tab-indent", 'GET "/" :: {\n\tx <- 1\n}\n')
    add("crlf", 'GET "/" :: {\r\n    x <- 1\r\n}\r\n')
    add("double-quote-esc", 'GET "/" :: {\n    x <- "a\\"b"\n}\n')
    add("trailing-token", 'GET "/" :: { x <- 1 };;\n')
    add("only-quote", '"\n')
    add("only-dcolon", '::\n')
    add("only-arrow", '<-\n')
    add("only-at", '@\n')
    add("at-in-expr", 'GET "/" :: {\n    x <- @3000\n}\n')
    add("model-number-table", "model M = 123 [ id => Int, ]\n")
    add("route-param-path-conflict", 'GET "/:id/x/:id" :: (id = Str) {}\n')
    add("ws-route", 'ws: socket "/s" {\n    message {\n        websocket.self\n    }\n}\n')
    add("ws-connect-body", 'ws: socket "/s" {\n    connect {\n        websocket.join("g1")\n    }\n}\n')
    add("ws-disconnect-body", 'ws: socket "/s" {\n    disconnect {\n        websocket.leave()\n    }\n}\n')
    add("ws-no-message", 'ws: socket "/s" {}\n')
    add("socket-no-ws", 'socket "/s" {\n    message {}\n}\n')
    add("socket-in-model", 'model M = m [ id => Int #id, socket "/" {}, ]\n')

    # seed-hook: dump expected totals
    print(f"phase1: generated {n} files")


# ---------------------------------------------------------------------------
# PHASE 2 — type checker torture (parses OK, must fail typecheck)
# ---------------------------------------------------------------------------
def gen_type():
    n = 0
    def add(nm, body):
        nonlocal n
        n += 1
        w(f"p2_type/{n:03d}-{nm}.hard", body)

    def route(inner="    __ <- 1"):
        return 'GET "/%" :: {\n' + "    " + inner + "\n}\n"

    def fn(name="f", params="x = Int", ret="Int", body="    <- x", call=""):
        return (f"calc {name}({params}) => {ret} {{\n{body}\n}}\n\n"
                f"GET \"/c\" :: {{\n{call}\n}}\n")

    # unknown identifiers / modules
    add("undefined-var", route("    __ <- nope"))
    add("undefined-fn-call", route("    __ <- f(1)"))
    add("undefined-module", route("    __ <- bogus.get(\"x\")"))
    add("undefined-mod-fn", route("    __ <- crypto.hmac(1)"))
    add("undefined-in-arith", route("    __ <- 1 + ghost"))
    add("undefined-nested", route("    __ <- (ghost + 1) * 2"))
    add("undefined-arg", route("    __ <- json.parse(nope)"))
    add("undefined-loop-var", "loop 3 {\n    __ <- nope\n}\n")
    add("undefined-test", "test t {\n    expect nope == 1\n}\n")

    # duplicate declarations
    add("dup-var-same", 'GET "/" :: {\n    a <- 1\n    a <- 2\n}\n')
    add("dup-const", 'GET "/" :: {\n    a ::= 1\n    a ::= 2\n}\n')
    add("dup-var-const", 'GET "/" :: {\n    a <- 1\n    a ::= 2\n}\n')
    add("dup-model", "model M = m [ id => Int #id, ]\nmodel M = n [ id => Int #id, ]\n")
    add("dup-func", "calc f(x) => Int { x }\ncalc f(y) => Int { y }\n")
    add("dup-test-name", 'test "t" {\n    expect 1 == 1\n}\ntest "t" {\n    expect 1 == 1\n}\n')
    add("dup-route", 'GET "/" :: { __ <- 1 }\nGET "/" :: { __ <- 1 }\n')
    add("dup-route-param", 'GET "/x" :: (id = Str) {}\nGET "/x" :: (id = Str) {}\n')
    add("dup-func-param", "calc f(x, x) => Int { x }\n")
    add("func-var-collide-global", "g <- 1\ncalc f() => Int { g <- 2; g }\n")
    add("dup-middleware", 'before {\n    __ <- 1\n}\nbefore {\n    __ <- 2\n}\n')
    add("dup-socket", 'ws: socket "/s" {}\nws: socket "/s" {}\n')

    # wrong types in arithmetic / comparisons
    add("arith-str-plus", route("    __ <- \"a\" + 1"))
    add("arith-str-minus", route("    __ <- \"a\" - \"b\""))
    add("arith-obj-plus", route("    __ <- {} + {}"))
    add("arith-list-mul", route("    __ <- [1] * 2"))
    add("cmp-str-int", route("    __ <- \"a\" < 1"))
    add("cmp-obj-int", route("    __ <- {} == 1"))
    add("cmp-list-list", route("    __ <- [1] == [2]"))
    add("cmp-bool-int", route("    __ <- true == 1"))
    add("not-num", route("    __ <- !\"x\""))
    add("mod-str", route("    __ <- 1 % \"x\""))

    # invalid return types / missing returns
    add("func-noreturn", "calc f(x) => Int {}\n")
    add("func-noreturn2", "calc f(x) => Int { y }\n")
    add("func-ret-expr", "calc f(x) => Int { x }\n")
    add("func-ret-str", "calc f(x) => Int { \"no\" }\n")
    add("func-ret-list", "calc f(x) => Int { [1, 2] }\n")
    add("func-ret-obj", "calc f(x) => Int { { \"a\": 1 } }\n")
    add("func-void-ret", "calc f(x) => Void { 5 }\n")
    add("func-bool-return", "calc f(x) => Bool { 1 }\n")
    add("func-dup-return", "calc f(x) => Int {\n    <- x\n    <- 2\n}\n")
    add("func-call-wrong-arity", route(fn(call="    __ <- f()")))
    add("func-call-wrong-arity2", route(fn(call="    __ <- f(1, 2, 3)")))
    add("func-call-wrong-type", route(fn(call="    __ <- f(\"s\")")))
    add("func-call-undef", route("    __ <- f1(1)"))

    # invalid assignments
    add("assign-to-literal", 'GET "/" :: {\n    1 <- 2\n}\n')
    add("assign-to-expr", 'GET "/" :: {\n    1 + 1 <- 2\n}\n')
    add("assign-to-str", 'GET "/" :: {\n    "x" <- 2\n}\n')
    add("assign-undef", 'GET "/" :: {\n    z <- 2\n    z <- z\n}\n')
    add("const-reassign", 'GET "/" :: {\n    z ::= 1\n    z <- 2\n}\n')
    add("reassign-funcname", "calc f(x) => Int { x }\nf <- 1\n")

    # model/table issues
    add("model-unsafe-query", 'model M = users [ id => Int #id, ]\nGET "/" :: {\n    __ <- M\n}\n')
    add("model-name-undefined", "GET \"/\" :: {\n    __ <- users.all\n}\n")
    add("model-null-pk", "model M = m [ name => Str, ]\n")
    add("pk-type-str", "model M = m [ id => Str #id, ]\n")
    add("two-id-pks", "model M = m [ a => Int #id, b => Int #id, ]\n")

    # misc type errors
    add("routes-nonjson-return", 'GET "/" :: {\n    __ <- \"text\"\n}\n')
    add("oneline-return", 'GET "/" :: {\n    <- 1\n}\n')
    add("middleware-nonjson", 'before {\n    __ <- \"x\"\n}\n')
    add("expect-wrong-op", 'test t {\n    expect 1 + 1\n}\n')
    add("if-cond-value", route("    if 5 { a <- 1 }"))
    add("compare-in-test", 'test t {\n    expect \"a\" == 1\n}\n')
    add("return-in-route-ctx", 'GET "/" :: {\n    <- 1\n    <- 2\n}\n')
    add("shadow-test", 'test "t" {\n    t <- 1\n}\n')

    # typed route params
    add("param-int", 'GET "/u/:id" :: (id = Int) {\n    __ <- id\n}\n')
    add("param-bool", 'GET "/u/:ok" :: (ok = Bool) {\n    __ <- ok\n}\n')
    add("param-body-and-type", 'POST "/b" :: (data = Str) {\n    __ <- data\n}\n')
    add("expect-bool-from-fn", 'calc t(x) => Bool { x }\n\ntest tt {\n    expect t(1 == 1)\n}\n')

    print(f"phase2: generated {n} files")


if __name__ == "__main__":
    kind = sys.argv[1] if len(sys.argv) > 1 else "all"
    os.makedirs(ROOT, exist_ok=True)
    if kind in ("all", "p1"):
        gen_parse()
    if kind in ("all", "p2"):
        gen_type()