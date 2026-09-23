#!/usr/bin/env python3
"""Raw-socket RFC6455 client used by qa/phase4_runtime.sh.

Connects N clients to ws://127.0.0.1:PORT/s, each sends masked text frame
"ping<id>", and waits until it receives its own broadcast echo "echo:ping<id>".
Returns number of clients that received their own echo.
"""
import base64, bz2, hashlib, os, socket, struct, sys, threading, time

PORT = int(sys.argv[1])
N = int(sys.argv[2])
HOST = "127.0.0.1"
ok = [0]
lock = threading.Lock()
errs = [0]


def recv_exact(sock, n):
    b = b""
    while len(b) < n:
        chunk = sock.recv(n - len(b))
        if not chunk:
            raise EOFError("closed")
        b += chunk
    return b


def client(i):
    try:
        s = socket.create_connection((HOST, PORT), timeout=10)
        key = base64.b64encode(os.urandom(16)).decode()
        req = (
            f"GET /s HTTP/1.1\r\nHost: {HOST}:{PORT}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        s.sendall(req.encode())
        # read until \r\n\r\n
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += s.recv(4096)
        if b"101" not in buf.split(b"\r\n", 1)[0]:
            raise RuntimeError("no 101: %r" % buf[:120])
        # send masked text frame "ping<i>"
        payload = f"ping{i}".encode()
        mask = os.urandom(4)
        masked = bytes(c ^ mask[j % 4] for j, c in enumerate(payload))
        n = len(payload)
        header = b"\x81"
        if n < 126:
            header += bytes([0x80 | n])
        elif n < 65536:
            header += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", n)
        s.sendall(header + mask + masked)
        # read frames until our echo appears
        want = f"echo:ping{i}".encode()
        deadline = time.time() + 15
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
            if b1 & 0x80:  # masked (servers must not, but tolerate)
                recv_exact(s, 4)
            data = recv_exact(s, ln)
            if opcode == 1 and data == want:
                got = True
        s.close()
        if got:
            with lock:
                ok[0] += 1
        else:
            with lock:
                errs[0] += 1
    except Exception as e:
        with lock:
            errs[0] += 1


threads = [threading.Thread(target=client, args=(i,)) for i in range(N)]
for t in threads:
    t.start()
for t in threads:
    t.join()
print(f"phase4_ws: {ok[0]}/{N} clients received their own echo (errors={errs[0]})")
sys.exit(0 if ok[0] == N else 1)