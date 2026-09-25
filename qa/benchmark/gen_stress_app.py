#!/usr/bin/env python3
"""Generate a benchmark app with N static routes + the mixed-lane core.

Used by the ms2.0 http-stress lab to prove router growth (1e3 - 1e4 static
routes) does not degrade the hot lanes. The mixed lane (/, /hello/:name,
POST /echo) is identical across sizes so RPS is comparable.
Run: qa/benchmark/gen_stress_app.py <N> <out.hard>
"""
import sys


def main():
    n = int(sys.argv[1])
    out = sys.argv[2]
    lines = ["app @8080", ""]
    for i in range(n):
        lines.append(f'GET "/r{i}" :: {{ <- {{ ok: true, i: {i} }} }}')
    lines.append('')
    lines.append('GET "/" :: { <- { ok: true } }')
    lines.append('')
    lines.append('GET "/hello/:name" :: (name = Str) { <- { hello: name } }')
    lines.append('')
    lines.append('POST "/echo" :: (body = Obj) { <- { echo: body } }')
    with open(out, "w") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()