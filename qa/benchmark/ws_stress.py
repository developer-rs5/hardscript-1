#!/usr/bin/env python3
"""WebSocket client stress for the ms2.0 stress lab (RFC6455, raw sockets).

Connects N concurrent clients to ws://127.0.0.1:PORT/s, each sends M masked
text frames (round-trip: server broadcasts "echo:<ping>"), measures aggregate
message throughput and per-message latency. Mirrors qa/phase4_ws.py framing.

Run: qa/benchmark/ws_stress.py <port> <N> <M_per_client>
"""
import base64, os, socket, struct, sys, threading, time

PORT = int(sys.argv[1])
N = int(sys.argv[2])
M = int(sys.argv[3]) if len(sys.argv) > 3 else 4
DEADLINE = int(sys.argv[4]) if len(sys.argv) > 4 else 20
ok = [0]
errs = [0]
lat = []
lock = threading.Lock()


def recv_exact(sock, n):
    b = b""
    while len(b) < n:
        c = sock.recv(n - len(b))
        if not c:
            raise EOFError("closed")
        b += c
    return b


def frame(payload):  # masked client->server text frame
    mask = os.urandom(4)
    masked = bytes(c ^ mask[j % 4] for j, c in enumerate(payload))
    n = len(payload)
    if n < 126:
        h = bytes([0x80 | n])
    elif n < 65536:
        h = bytes([0x80 | 126]) + struct.pack(">H", n)
    else:
        h = bytes([0x80 | 127]) + struct.pack(">Q", n)
    return b"\x81" + h + mask + masked


def client(i):
    try:
        s = socket.create_connection(("127.0.0.1", PORT), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET /s HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        s.sendall(req.encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += s.recv(4096)
        if b"101" not in buf.split(b"\r\n", 1)[0]:
            raise RuntimeError("no 101")
        for k in range(M):
            want = f"echo:ping{i}_{k}".encode()
            payload = f"ping{i}_{k}".encode()
            t0 = time.monotonic()
            s.sendall(frame(payload))
            deadline = time.time() + DEADLINE
            got = False
            while time.time() < deadline and not got:
                b0 = recv_exact(s, 1)[0]
                b1 = recv_exact(s, 1)[0]
                opcode = b0 & 0x0F
                ln = b1 & 0x7F
                if ln == 126:
                    ln = struct.unpack(">H", recv_exact(s, 2))[0]
                elif ln == 127:
                    ln = struct.unpack(">Q", recv_exact(s, 8))[0]
                if b1 & 0x80:
                    recv_exact(s, 4)
                data = recv_exact(s, ln)
                if opcode == 1 and data == want:
                    got = True
            if not got:
                raise RuntimeError("no echo")
            with lock:
                lat.append((time.monotonic() - t0) * 1000.0)
        s.close()
        with lock:
            ok[0] += 1
    except Exception:
        with lock:
            errs[0] += 1
    finally:
        try:
            s.close()
        except Exception:
            pass


t0 = time.monotonic()
threads = [threading.Thread(target=client, args=(i,)) for i in range(N)]
for t in threads:
    t.start()
for t in threads:
    t.join()
dur = max(time.monotonic() - t0, 1e-6)
lat.sort()
messages = N * M
rps = messages / dur
p50 = lat[len(lat) * 50 // 100] if lat else 0
p95 = lat[len(lat) * 95 // 100] if lat else 0
p99 = lat[len(lat) * 99 // 100] if lat else 0
print(
    f"ws_stress: clients={N} msg/client={M} messages={messages} "
    f"wall={dur:.2f}s msgs/s={rps:.1f} p50={p50:.2f} p95={p95:.2f} "
    f"p99={p99:.2f} ok={ok[0]} err={errs[0]}"
)
sys.exit(0 if ok[0] == N and errs[0] == 0 else 1)