#!/usr/bin/env python3
"""Phase 2 type-torture corpus generator.

Produces files under qa/corpus/p2_type/ (replacing existing) plus
qa/corpus/p2_expect.json mapping filename -> expectation:

    {"expect": "err", "msg": "..."}   must FAIL typecheck with this message
    {"expect": "ok"}                  must PASS frontend + typecheck

Generated deterministically. Every file targets the surface the v0.1
typechecker actually implements (undefined refs, duplicate declarations,
undefined calls) plus valid programs that stress scopes.
"""
import json
import os
import shutil

OUT = os.path.join(os.path.dirname(__file__), "corpus", "p2_type")
EXPECT_PATH = os.path.join(os.path.dirname(__file__), "corpus", "p2_expect.json")

FUNC = "calc add(Num x, Num y) => Num { <- x + y }\n"
MODEL = "model M = t [\n    id => Int #id,\n    n => Str,\n]\n"
SOCKET = (
    'socket "/s" {\n'
    "    connect :: { },\n"
    "    message(d) :: { },\n"
    "    disconnect :: { },\n"
    "}\n"
)
ROUTE = 'GET "/" :: {\n    <- 1\n}\n'


def err(msg):
    return {"expect": "err", "msg": msg}


def ok():
    return {"expect": "ok"}


def write(name, src, exp):
    with open(os.path.join(OUT, name), "w") as f:
        f.write(src)
    cases[name] = exp


shutil.rmtree(OUT, ignore_errors=True)
os.makedirs(OUT, exist_ok=True)
cases = {}

# ---- undefined identifiers in every block context -----------------------
undef_ctx = [
    ("route", 'GET "/" :: {\n    <- nope\n}\n'),
    ("calc",
     "calc f(Num x) => Num {\n    <- nope\n}\n" + ROUTE),
    ("middleware",
     'before gate :: {\n    <- nope\n}\n' + ROUTE),
    ("socket",
     'socket "/s" {\n    connect :: { <- nope }\n}\n' + ROUTE),
    ("test", 'test "t" {\n    <- nope\n}\n' + ROUTE),
    ("if",
     'GET "/" :: {\n    ?(nope) { <- 1 }\n    <- 2\n}\n'),
    ("loop",
     'GET "/" :: {\n    loop x => nope { <- x }\n    <- 2\n}\n'),
    ("race",
     'GET "/" :: {\n    race [ nope ]\n    <- 2\n}\n'),
    ("pick",
     'GET "/" :: {\n    <- pick nope { 1 => "a", * => "z" }\n}\n'),
    ("expect",
     'test "t" {\n    expect nope == 1\n}\n' + ROUTE),
]
for i, (ctx, src) in enumerate(undef_ctx, 1):
    if i == 1:
        write(f"{i:03d}-undef-{ctx}.hard", src, err("`nope` is not defined"))
    else:
        write(f"{i:03d}-undef-{ctx}.hard", src, err("not defined"))

# ---- undefined identifiers inside expressions ----------------------------
expr_ctx = [
    ("obj", 'GET "/" :: {\n    <- { "k": nope }\n}\n'),
    ("list", 'GET "/" :: {\n    <- [ nope ]\n}\n'),
    ("index-base", 'GET "/" :: {\n    <- nope[0]\n}\n'),
    ("index-idx", 'GET "/" :: {\n    <- [1][nope]\n}\n'),
    ("member-base", 'GET "/" :: {\n    <- nope.field\n}\n'),
    ("binary", 'GET "/" :: {\n    <- 1 + nope\n}\n'),
    ("unary", 'GET "/" :: {\n    <- -nope\n}\n'),
    ("arg", 'GET "/" :: {\n    <- add(nope, 1)\n}\n' + FUNC),
    ("callee",
     'GET "/" :: {\n    <- bogus(1)\n}\n'
     + func if False else FUNC + '\nGET "/" :: {\n    <- bogus(1)\n}\n'),
    ("var-value", "v <- nope\n" + ROUTE),
    ("const-value", "v ::= nope\n" + ROUTE),
    ("nested", 'GET "/" :: {\n    ?(1 == 1) { <- [ { "a": nope } ] }\n    <- 2\n}\n'),
]
for i, (ctx, src) in enumerate(expr_ctx, len(undef_ctx) + 1):
    write(f"{i:03d}-undef-in-{ctx}.hard", src, err("not defined"))

# ---- duplicate declarations ----------------------------------------------
dups = [
    ("model", "model M = a [ x => Int, ]\nmodel M = b [ y => Int, ]\n" + ROUTE,
     "duplicate declaration of `model M`"),
    ("calc",
     "calc f(Num x) => Num { <- x }\ncalc f(Num y) => Num { <- y }\n" + ROUTE,
     "duplicate declaration of `f`"),
    ("test",
     'test "t" { <- 1 }\ntest "t" { <- 2 }\n' + ROUTE,
     "duplicate declaration of `test t`"),
    ("middleware",
     'before g :: { }\nbefore g :: { }\n' + ROUTE,
     "duplicate declaration of `middleware g`"),
    ("route",
     'GET "/" :: { <- 1 }\nGET "/" :: { <- 2 }\n',
     "duplicate declaration of `GET /`"),
    ("route-param",
     'GET "/x/:id" :: (id = Str) { <- id }\n'
     'GET "/x/:id" :: (id = Str) { <- id }\n',
     "duplicate declaration of `GET /x/:id`"),
]
for i, (ctx, src, msg) in enumerate(dups, len(undef_ctx) + len(expr_ctx) + 1):
    write(f"{i:03d}-dup-{ctx}.hard", src, err(msg))

# ---- valid programs that must pass ---------------------------------------
valid = [
    ("bug3-global-chain", "a <- 1\nb <- a + 1\nc <- b * 2\n" + ROUTE),
    ("bug3-global-fwd", "b <- a + 1\na <- 1\n" + ROUTE),
    ("bug3-global-mod", 'a <- "x"\nb <- json.stringify(a)\n' + ROUTE),
    ("globals-in-route",
     "g1 <- 1\ng2 <- g1 + 1\n" + 'GET "/" :: {\n    <- g2\n}\n' + FUNC),
    ("globals-in-middleware",
     "g <- 1\nbefore gate :: { <- g }\n" + ROUTE),
    ("globals-in-socket",
     "g <- 1\nsocket \"/s\" { connect :: { g <- g + 1 } }\n" + ROUTE),
    ("globals-in-test", "g <- 1\ntest \"t\" { <- g }\n" + ROUTE),
    ("func-call-valid", FUNC + 'GET "/" :: {\n    <- add(1, 2)\n}\n'),
    ("func-call-nested", FUNC + 'GET "/" :: {\n    <- add(add(1, 2), 3)\n}\n'),
    ("func-global-shadow",
     "calc f(Num x) => Num { <- x }\nf <- f(2)\n" + ROUTE),
    ("module-json", 'GET "/" :: {\n    <- json.stringify({ "a": 1 })\n}\n'),
    ("module-env", 'GET "/" :: {\n    <- env.get("HOME", "x")\n}\n'),
    ("module-crypto", 'GET "/" :: {\n    <- crypto.sha256("x")\n}\n'),
    ("module-time", 'GET "/" :: {\n    <- time.now()\n}\n'),
    ("module-runtime", 'GET "/" :: {\n    <- runtime.argc()\n}\n'),
    ("module-method-object",
     'GET "/" :: {\n    a <- json.parse("{\\\"a\\\":1}")\n    <- json.keys(a)\n}\n'),
    ("req-usage", 'GET "/" :: {\n    <- req.method\n}\n'),
    ("req-params",
     'GET "/u/:id" :: (id = Str) {\n    <- id\n}\n'),
    ("req-body",
     'POST "/b" :: (body = Obj) {\n    <- body.field\n}\n'),
    ("scope-if", 'GET "/" :: {\n    x <- 0\n    ?(1 == 1) { x <- 1 }\n    <- x\n}\n'),
    ("scope-loop",
     'GET "/" :: {\n    t <- 0\n    loop x => [1,2,3] { t <- t + x }\n    <- t\n}\n'),
    ("scope-race",
     'GET "/" :: {\n    race [ 1, 2 ]\n    <- 1\n}\n'),
    ("scope-pick",
     'GET "/" :: {\n    <- pick 2 { 1 => "a", * => "z" }\n}\n'),
    ("scope-expect-route",
     'GET "/" :: {\n    expect 1 == 1\n    <- 1\n}\n'),
    ("test-block-uses-global", "g <- 1\ntest \"t\" { expect g == 1 }\n" + ROUTE),
    ("socket-message-global",
     'g <- 1\nsocket "/s" { message(d) :: { g <- g + 1 } }\n' + ROUTE),
    ("model-valid", MODEL + ROUTE),
    ("calc-ret-types",
     "calc ci() => Int { <- 1 }\ncalc cs() => Str { <- \"x\" }\n"
     "calc cb() => Bool { <- 1 == 1 }\ncalc cl() => List { <- [1] }\n"
     "calc co() => Obj { <- { \"a\": 1 } }\n"
     "calc cn() => Num { }\n"
     + ROUTE),
    ("deep-nest-ok",
     "1" * 1 if False else (lambda p: 'GET "/" :: {\n    <- %s\n}\n' % p)(
         "(" * 120 + "1" + ")" * 120)),
    ("chain-ok", 'GET "/" :: {\n    x <- 1\n    <- x + x + x + x\n}\n'),
]
for i, (ctx, src) in enumerate(valid, len(undef_ctx) + len(expr_ctx) + len(dups) + 1):
    write(f"{i:03d}-valid-{ctx}.hard", src, ok())

# ---- deep nesting / long chains (parser-guard interaction) ---------------
deep = [
    ("deep-nest-err",
     lambda p: 'GET "/" :: {\n    <- %s\n}\n' % p, err("nesting too deep")),
]
for i, (ctx, gen, exp) in enumerate(deep, len(undef_ctx) + len(expr_ctx) + len(dups) + len(valid) + 1):
    p = "1"
    for _ in range(600):
        p = "(" + p + ")"
    write(f"{i:03d}-err-{ctx}.hard", gen(p), exp)

# ---- misc accepted-but-unchecked (documented behavior, expect ok) --------
lenient = [
    ("unchecked-module-fn-arity", "crypto.hmac(\"k\", \"d\", 1)\n" + ROUTE),
    ("unchecked-module-fn-name", 'json.parsee("{}")\n' + ROUTE),
    ("reassign-var", 'GET "/" :: {\n    a <- 1\n    a <- 2\n    <- a\n}\n'),
    ("redeclare-const", "a ::= 1\na ::= 2\n" + ROUTE),
    ("reassign-const", "a ::= 1\na <- 2\n" + ROUTE),
    ("dup-var-global", "a <- 1\na <- 2\n" + ROUTE),
    ("expr-stmt-noreturn",
     'GET "/" :: {\n    1\n    <- 2\n}\n'),
]
for i, (ctx, src) in enumerate(lenient, len(undef_ctx) + len(expr_ctx) + len(dups) + len(valid) + len(deep) + 1):
    write(f"{i:03d}-lenient-{ctx}.hard", src, ok())

# ---- expand to 150: calc round-trips, scope matrices, extra err forms -----
extra_valid = []
mname = "MOD"
MIN = "calc f(Num x) => Num { <- x + 1 }\n"
ROUTED = 'GET "/d" :: {\n    <- f(g)\n}\n'
# calc referenced from every context + globals in every context
extra_valid += [
    ("calc-in-calc", MIN + "calc g2(Num z) => Num { <- f(z) }\n" + ROUTE),
    ("calc-in-mid", MIN + 'before gm :: { <- f(1) }\n' + ROUTE),
    ("calc-in-socket", MIN + 'socket "/s" { connect :: { <- f(1) } }\n' + ROUTE),
    ("calc-in-test", MIN + 'test "t" { expect f(1) == 2 }\n' + ROUTE),
    ("calc-in-pick", MIN + "calc h(Num w) => Num {\n    <- pick w { 1 => 2, * => 3 }\n}\n" + ROUTE),
    ("calc-in-loop", MIN + "calc h2(Num q) => Num {\n    t <- 0\n    loop x => [1,2] { t <- t + x }\n    <- t + f(q)\n}\n" + ROUTE),
    ("calc-empty-body", "calc z() => Nil { }\n" + ROUTE),
    ("async-calc", "async calc az(Num a) => Num { <- a }\n" + ROUTE),
    ("calc-multi", "calc m1(Num a) => Num { <- a }\ncalc m2(Num a) => Num { <- m1(a) + 1 }\ncalc m3(Num a) => Num { <- m2(m1(a)) }\n" + ROUTE),
    # globals across all contexts
    ("global-in-calc",
     "g <- 10\ncalc gc(Num x) => Num { <- x + g }\n" + ROUTE),
    ("multi-globals",
     "a <- \"x\"\nb <- json.stringify(a)\nc <- [a, b]\n"
     "calc comb() => List { <- [a, b, c] }\n" + ROUTE),
    ("global-mod-chain",
     "k <- \"k\"\n" + "GET \"/t\" :: {\n    <- crypto.hmac(k, \"data\")\n}\n"),
    # nested scope resolutions
    ("scope-deep",
     'GET "/" :: {\n    x <- 1\n    ?(x == 1) {\n        ?(x == 1) {\n            y <- x + 1\n            ?(y == 2) { z <- y }\n        }\n    }\n    <- x\n}\n'),
    ("scope-loop-nested",
     'GET "/" :: {\n    t <- 0\n    loop a => [[1],[2]] { loop b => a { t <- t + b[0] } }\n    <- t\n}\n'),
    ("scope-shadow-param",
     'GET "/s/:v" :: (v = Str) {\n    v <- "shadow"\n    <- v\n}\n'),
    ("scope-param-across",
     "calc toU(Str s) => Str { <- s }\n"
     'GET "/n/:u" :: (u = Str) {\n    <- toU(u)\n}\n'),
    ("race-discard", "calc slow(Num x) => Num { <- x }\n" + 'GET "/" :: {\n    race [ slow(1), 2 ]\n    <- 1\n}\n'),
    ("wait-basic",
     "calc wn() => Num { <- 42 }\n" + 'GET "/" :: {\n    x <- wait wn()\n    <- x\n}\n'),
    ("pick-default-obj",
     'GET "/" :: {\n    o <- { "a": 1 }\n    <- pick o.a { nil => 0, * => o.a }\n}\n', err("`nil` is not defined")),
    ("chain-ok100",
     "x <- 1\n" + "GET \"/\" :: {\n    <- " + "x + ".join(["1"] * 100) + "\n}\n"),
    ("chain-members-ok50",
     'o <- { "a": { "b": { "c": 7 } } }\n'
     + "GET \"/\" :: {\n    <- o.a.b.c\n}\n"),
    ("model-doc-only", "model U = users [\n    id => Int #id,\n    name => Str #unique,\n]\n" + ROUTE),
    ("index-str", 'GET "/" :: {\n    <- "hello"[1]\n}\n'),
    ("index-list-of-list", 'GET "/" :: {\n    l <- [[1],[2]]\n    <- l[1][0]\n}\n'),
    ("index-expr", 'GET "/" :: {\n    <- [10,20,30][1 + 1]\n}\n'),
    ("range-lexes", 'GET "/" :: {\n    <- [ 1 ... 3 ]\n}\n', err("expected ']' to close list")),
    ("expect-ok-route",
     'GET "/" :: {\n    expect 1 == 1\n    <- 1\n}\n'),
    ("expect-str-eq", 'GET "/" :: {\n    expect "a" == "a"\n    <- 1\n}\n'),
    ("expect-fn-call", FUNC + 'GET "/" :: {\n    expect add(1, 2) == 3\n    <- 1\n}\n'),
    ("boot-underscore-arg", 'GET "/" :: {\n    <- _(1)\n}\n'),
    ("route-body-expr-ret",
     'GET "/" :: {\n    x <- { "s": 200 }\n    <- x\n}\n'),
    ("socket-connect-global-call",
     "calc sc() => Str { <- \"ok\" }\ng <- 1\n"
     'socket "/s" { message(d) :: { <- sc() } }\n' + ROUTE),
    ("nested-object-kv",
     'GET "/" :: {\n    <- { "a": { "b": [1, { "c": 2 }] } }\n}\n'),
    ("string-escapes", 'GET "/" :: {\n    <- "a\\n\\tb\\"c\\\'\'d"\n}\n'),
]
extra_err = [
    ("undef-in-calc-param", "calc f(Num x) => Num { <- x + q }\n" + ROUTE),
    ("undef-in-calc-body", "calc f(Num x) => Num { y <- 1\n    <- y + z }\n" + ROUTE),
    ("undef-in-global", "a <- b\n" + ROUTE),
    ("undef-in-const", "a ::= b\n" + ROUTE),
    ("undef-self-ref", "a <- a\n" + ROUTE, ok()),
    ("undef-member", 'GET "/" :: {\n    <- {"a":1}.nope\n}\n', ok()),
    ("undef-index-empty", 'GET "/" :: {\n    <- nope[0]\n}\n'),
    ("undef-match-arm", 'GET "/" :: {\n    <- pick 1 { q => 2, * => 3 }\n}\n'),
    ("undef-race-arg", "calc f2(Num x) => Num { <- x }\n" + 'GET "/" :: {\n    race [ f2(nope2) ]\n    <- 1\n}\n'),
    ("undef-fn-as-arg", 'GET "/" :: {\n    <- bogus_fn(1)\n}\n'),
    ("undef-user-fn", 'GET "/" :: {\n    <- missing_calc(2)\n}\n'),
    ("dup-calc-params",
     "calc d(Num x, Num x) => Num { <- x }\n" + ROUTE, ok()),
    ("dup-middleware-route-name",
     'before g :: { }\ncalc g() => Num { <- 1 }\n' + ROUTE, ok()),
]
for i, (ctx, src, *exp_) in enumerate(extra_valid, len(undef_ctx) + len(expr_ctx) + len(dups) + len(valid) + len(deep) + len(lenient) + 1):
    exp = exp_[0] if exp_ else ok()
    write(f"{i:03d}-valid-{ctx}.hard", src, exp)
for i, (ctx, src, *msgexp) in enumerate(extra_err, len(undef_ctx) + len(expr_ctx) + len(dups) + len(valid) + len(deep) + len(lenient) + len(extra_valid) + 1):
    exp = msgexp[0] if msgexp else err("not defined")
    write(f"{i:03d}-err-{ctx}.hard", src, exp)

# pad to 150 with deterministic valid program matrix
modules_pad = [
    ("json", 'json.stringify({ "a": 1 })'),
    ("crypto", 'crypto.sha256bin("x")'),
    ("env", 'env.get("PATH", "")'),
    ("time", 'time.iso(time.now())'),
    ("runtime", 'runtime.platform()'),
]
pad_idx = len(undef_ctx) + len(expr_ctx) + len(dups) + len(valid) + len(deep) + len(lenient) + len(extra_valid) + len(extra_err) + 1
n = 0
for mi, (mod, call) in enumerate(modules_pad):
    for ctx in ["route", "calc", "middleware", "socket", "test"]:
        if mi == 0 and ctx == "route":
            pass
        body = {
            "route": 'GET "/" :: {\n    <- %s\n}\n' % call,
            "calc": "calc cf%d() => Num {\n    <- %s\n}\n" % (n, call) + ROUTE,
            "middleware": 'before gm%d :: { <- %s\n    <- 1\n}\n' % (n, call) + ROUTE,
            "socket": 'socket "/s%d" { connect :: { <- %s }\n}\n' % (n, call) + ROUTE,
            "test": 'test "t%d" { expect 1 == 1 || %s }\n' % (n, call) + ROUTE,
        }[ctx]
        write(f"{pad_idx:03d}-valid-mod-{mod}-{ctx}.hard", body, ok())
        pad_idx += 1
        n += 1
for i in range(10):
    write(f"{pad_idx:03d}-valid-globals-pad{i}.hard",
          "g{a} <- {a}\ncalc h{a}(Num x) => Num {{ <- x + g{a} }}\nGET \"/{a}\" :: {{ <- h{a}(g{a}) }}\n".format(a=i), ok())
    pad_idx += 1

with open(EXPECT_PATH, "w") as f:
    json.dump(cases, f, indent=1, sort_keys=True)
print(f"wrote {len(cases)} p2 files to {OUT}")
print(f"err={sum(1 for c in cases.values() if c['expect']=='err')} ok={sum(1 for c in cases.values() if c['expect']=='ok')}")