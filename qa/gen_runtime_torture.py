#!/usr/bin/env python3
"""Emit the Phase 4 runtime-torture single app with 100 routes + modules + WS."""
import os

OUT = os.path.join(os.path.dirname(__file__), "runtime_torture.hard")

lines = []
w = lines.append
w("app @3031")
w("")
w('GET "/" :: {')
w('    <- "root"')
w("}")

# 100 plain routes, one per index, plus two param routes
for i in range(100):
    w(f'GET "/r{i}" :: {{ <- {i} }}')
w('GET "/u/:id" :: (id = Str) {')
w('    <- { "id": id }')
w("}")
w('GET "/sum/:a/:b" :: (a = Str, b = Str) {')
w('    <- { "s": a + b }')
w("}")
w('POST "/echo" :: (body = Obj) {')
w("    <- body")
w("}")
w('POST "/elev" :: (body = Obj) {')
w('    <- body.x')
w("}")

# fs torture
w("""GET "/fs" :: {
    fs.write("/tmp/hs_rt_test.txt", "hello")
    fs.append("/tmp/hs_rt_test.txt", " world")
    s1 <- fs.size("/tmp/hs_rt_test.txt")
    rd <- fs.read("/tmp/hs_rt_test.txt")
    ex <- fs.exists("/tmp/hs_rt_test.txt")
    fs.write("/tmp/hs_rt_big.txt", crypto.random_hex(2 * 1024 * 1024))
    big <- fs.size("/tmp/hs_rt_big.txt")
    fs.remove("/tmp/hs_rt_test.txt")
    <- { "rd": rd, "sz": s1, "ex": ex, "big": big }
}
""")

# crypto vector + jwt roundtrip
w('GET "/sha" :: { <- crypto.sha256("abc") }')
w('GET "/sha0" :: { <- crypto.sha256("") }')
w('GET "/sha-a" :: { <- crypto.sha256("a") }')
w('GET "/shafox" :: { <- crypto.sha256("The quick brown fox jumps over the lazy dog") }')
w('GET "/shalong" :: { <- crypto.sha256("abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq") }')
w('GET "/sha1abc" :: { <- crypto.sha1("abc") }')
w('GET "/md5abc" :: { <- crypto.md5("abc") }')
w('GET "/hmac" :: { <- crypto.hmac("key", "The quick brown fox jumps over the lazy dog") }')
w('GET "/b64" :: { <- crypto.base64("hello") }')
w('GET "/jwt" :: { <- jwt.sign({ "sub": "1" }, "hs-secret") }')
# verify uses a query param read via the route-param binding
w('GET "/jwtv" :: (t = Str) { <- jwt.verify(t, "hs-secret") }')

# dynamic (clock)
w('GET "/sec" :: { <- time.now() }')

# websocket echo room
w("""socket "/s" {
    connect :: {
        websocket.join("room")
    }
    message(d) :: {
        websocket.broadcast_room("room", "echo:" + d)
    }
    disconnect :: { }
}
""")

with open(OUT, "w") as f:
    f.write("\n".join(lines) + "\n")
print(f"wrote {OUT}")