#!/usr/bin/env python3
"""HTTP load driver for the v0.1.1 benchmark suite.

Uniform client for all server stacks. Supports two lanes:
  --keepalive 0 : one new TCP connection per request (mirrors the
                 Connection: close world; the only lane HardScript supports)
  --keepalive 1 : persistent connections (Node/Bun/Go/Axum)

Mixed workload mirroring the identical REST API:
  GET /             (weight 1)
  GET /hello/<name> (weight 2)
  POST /echo        (weight 1)

Prints a single JSON line with rps + p50/p95/p99 latency in ms.
"""
import argparse
import concurrent.futures
import json
import random
import socket
import time


def http_exchange(sock, req: bytes) -> bytes:
    sock.sendall(req)
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(65536)
        if not chunk:
            raise ConnectionError("closed mid-headers")
        buf += chunk
    head, _, rest = buf.partition(b"\r\n\r\n")
    cl = 0
    for line in head.split(b"\r\n"):
        if line.lower().startswith(b"content-length:"):
            cl = int(line.split(b":")[1].strip())
    while len(rest) < cl:
        chunk = sock.recv(65536)
        if not chunk:
            raise ConnectionError("closed mid-body")
        rest += chunk
    return head + b"\r\n\r\n" + rest[:cl]


def build_request(rng, ep):
    host = "127.0.0.1"
    if ep == "root":
        return b"GET / HTTP/1.1\r\nHost: %s\r\n\r\n" % host.encode()
    if ep == "hello":
        # ASCII url-safe name
        name = "bob" + str(rng.randrange(0, 999))
        return b"GET /hello/%s HTTP/1.1\r\nHost: %s\r\n\r\n" % (name.encode("ascii"), host.encode())
    body = b'{"x":7,"s":"bench"}'
    return (
        b"POST /echo HTTP/1.1\r\nHost: %s\r\nContent-Type: application/json\r\n"
        b"Content-Length: %d\r\n\r\n%s" % (host.encode(), len(body), body)
    )


def worker(deadline, keepalive, rng, out):
    socks = {}
    n = 0
    while time.monotonic() < deadline:
        ep = rng.choices(["root", "hello", "echo"], weights=[1, 2, 1])[0]
        req = build_request(rng, ep)
        t0 = time.monotonic()
        try:
            sock = None
            if keepalive:
                sock = socks.get(ep)
            if sock is None:
                sock = socket.create_connection(("127.0.0.1", 8080), timeout=10)
                if keepalive:
                    socks[ep] = sock
            http_exchange(sock, req)
            out.append((time.monotonic() - t0) * 1000.0)
            n += 1
        except OSError:
            sock = None
            socks.pop(ep, None)
            out.append(None)
            try:
                sock and sock.close()
            except Exception:
                pass
    return n


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--duration", type=float, default=8.0)
    ap.add_argument("--warmup", type=float, default=3.0)
    ap.add_argument("--concurrency", type=int, default=32)
    ap.add_argument("--keepalive", type=int, default=0, choices=[0, 1])
    ap.add_argument("--seed", type=int, default=11)
    args = ap.parse_args()

    # warmup
    end = time.monotonic() + args.warmup
    with concurrent.futures.ThreadPoolExecutor(args.concurrency) as ex:
        futs = [ex.submit(worker, end, args.keepalive, random.Random(args.seed + i), []) for i in range(args.concurrency)]
        concurrent.futures.wait(futs)

    out: list = []
    end = time.monotonic() + args.duration
    with concurrent.futures.ThreadPoolExecutor(args.concurrency) as ex:
        futs = [ex.submit(worker, end, args.keepalive, random.Random(args.seed + i), out) for i in range(args.concurrency)]
        concurrent.futures.wait(futs)

    lat = sorted(x for x in out if x is not None)
    err = len(out) - len(lat)
    dur = args.duration
    result = {
        "rps": round(len(lat) / dur, 1),
        "p50": round(lat[len(lat) * 50 // 100], 2) if lat else 0,
        "p95": round(lat[len(lat) * 95 // 100], 2) if lat else 0,
        "p99": round(lat[len(lat) * 99 // 100], 2) if lat else 0,
        "n": len(lat),
        "err": err,
        "keepalive": args.keepalive,
        "concurrency": args.concurrency,
        "duration_s": args.duration,
    }
    print(json.dumps(result))


if __name__ == "__main__":
    main()