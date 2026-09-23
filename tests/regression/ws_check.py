#!/usr/bin/env python3
"""Regression check for the ws_leave room-teardown bug (BUG 9).

Scenario that fails on the old runtime:
  1. C1 connects, joins "room", sends pingC1, receives its echo, disconnects.
     (Old ws_leave destroyed the whole "room" membership here.)
  2. C2 connects, joins "room", sends pingC2.
     It MUST receive echo:pingC2. On the old runtime the room set was wiped,
     so broadcast_room no-op'd and C2 never saw its echo.

Usage: ws_check.py PORT EXPECT_STRING
Exits 0 when the second client receives its echo.
"""
import base64, os, socket, struct, sys, threading, time

PORT = int(sys.argv[1])
HOST = "127.0.0.1"


def rx(s, n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c:
            raise EOFError
        b += c
    return b


def handshake():
    s = socket.create_connection((HOST, PORT), timeout=8)
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall((
        f"GET /s HTTP/1.1\r\nHost: {HOST}:{PORT}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    ).encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        buf += s.recv(4096)
    if b"101" not in buf.split(b"\r\n", 1)[0]:
        raise RuntimeError("no 101")
    return s


def send_text(s, payload):
    mask = os.urandom(4)
    masked = bytes(c ^ mask[j % 4] for j, c in enumerate(payload))
    n = len(payload)
    header = b"\x81" + bytes([0x80 | n])
    s.sendall(header + mask + masked)


def wait_echo(s, want, deadline=10):
    end = time.time() + deadline
    while time.time() < end:
        s.settimeout(end - time.time())
        b0 = rx(s, 1)[0]
        b1 = rx(s, 1)[0]
        op = b0 & 0x0F
        ln = b1 & 0x7F
        if ln == 126:
            ln = struct.unpack(">H", rx(s, 2))[0]
        elif ln == 127:
            ln = struct.unpack(">Q", rx(s, 8))[0]
        if b1 & 0x80:
            rx(s, 4)
        if op == 1 and rx(s, ln) == want:
            return True
    return False


def burst(n=40):
    """Concurrent connects/joins/messages: exercises the ws_clients/rooms map
    registration path under load (BUG 8 data-race guard)."""
    lock = threading.Lock()
    got = [0]
    failed = [0]

    def one(i):
        try:
            s = handshake()
            send_text(s, ("pingB%d" % i).encode())
            okb = wait_echo(s, ("echo:pingB%d" % i).encode(), deadline=15)
            s.close()
            with lock:
                got[0] += 1 if okb else 0
                failed[0] += 0 if okb else 1
        except Exception:
            with lock:
                failed[0] += 1

    ts = [threading.Thread(target=one, args=(i,)) for i in range(n)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    return got[0], failed[0]


def main():
    c1 = handshake()
    send_text(c1, b"pingC1")
    ok1 = wait_echo(c1, b"echo:pingC1")
    c1.close()
    time.sleep(0.4)  # let the server process the disconnect/leave
    c2 = handshake()
    send_text(c2, b"pingC2")
    ok2 = wait_echo(c2, b"echo:pingC2")
    c2.close()
    g, f = burst()
    print(f"ws_check: c1_echo={ok1} c2_echo_after_c1_left={ok2} burst_ok={g} burst_failed={f}")
    sys.exit(0 if (ok1 and ok2 and g == 40 and f == 0) else 1)


if __name__ == "__main__":
    main()