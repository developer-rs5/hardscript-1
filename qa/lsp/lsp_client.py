#!/usr/bin/env python3
import json
import os
import select
import subprocess
import threading
import time


class LspFailure(RuntimeError):
    pass


class LspClient:
    def __init__(self, binary, workspace):
        self.process = subprocess.Popen(
            [str(binary)],
            cwd=str(workspace),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self.buffer = bytearray()
        self.next_id = 1
        self.responses = {}
        self.notifications = []
        self.stderr = []
        self.stderr_thread = threading.Thread(target=self._drain_stderr, daemon=True)
        self.stderr_thread.start()

    def _drain_stderr(self):
        if self.process.stderr is None:
            return
        for line in iter(self.process.stderr.readline, b""):
            self.stderr.append(line.decode("utf-8", "replace").rstrip("\n"))

    def _read_more(self, timeout):
        if self.process.stdout is None:
            raise LspFailure("server stdout is closed")
        ready, _, _ = select.select([self.process.stdout], [], [], max(0.0, timeout))
        if not ready:
            raise LspFailure("timed out waiting for server output")
        chunk = os.read(self.process.stdout.fileno(), 65536)
        if not chunk:
            raise LspFailure("server closed stdout")
        self.buffer.extend(chunk)

    def _read_line(self, deadline):
        while True:
            marker = self.buffer.find(b"\r\n")
            if marker >= 0:
                line = bytes(self.buffer[:marker])
                del self.buffer[:marker + 2]
                return line
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise LspFailure("timed out reading a message header")
            self._read_more(remaining)

    def read_message(self, timeout=10.0):
        deadline = time.monotonic() + timeout
        headers = {}
        while True:
            line = self._read_line(deadline)
            if not line:
                break
            decoded = line.decode("ascii", "replace")
            if ":" in decoded:
                name, value = decoded.split(":", 1)
                headers[name.strip().lower()] = value.strip()
        try:
            length = int(headers["content-length"])
        except (KeyError, ValueError) as error:
            raise LspFailure(f"invalid message headers: {headers}") from error
        while len(self.buffer) < length:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise LspFailure("timed out reading a message body")
            self._read_more(remaining)
        body = bytes(self.buffer[:length])
        del self.buffer[:length]
        try:
            return json.loads(body.decode("utf-8"))
        except json.JSONDecodeError as error:
            raise LspFailure(f"invalid JSON body: {error}") from error

    def send(self, message):
        payload = json.dumps(message, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        frame = f"Content-Length: {len(payload)}\r\n\r\n".encode("ascii") + payload
        if self.process.stdin is None:
            raise LspFailure("server stdin is closed")
        self.process.stdin.write(frame)
        self.process.stdin.flush()

    def notify(self, method, params=None):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.send(message)

    def request(self, method, params=None, timeout=15.0):
        request_id = self.next_id
        self.next_id += 1
        message = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        started = time.monotonic()
        self.send(message)
        deadline = time.monotonic() + timeout
        while request_id not in self.responses:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise LspFailure(f"timed out waiting for {method}")
            incoming = self.read_message(remaining)
            if "id" in incoming and "method" not in incoming:
                self.responses[incoming["id"]] = incoming
            elif "method" in incoming:
                self.notifications.append(incoming)
        response = self.responses.pop(request_id)
        response["_latency_ms"] = (time.monotonic() - started) * 1000.0
        if "error" in response:
            raise LspFailure(f"{method} failed: {response['error']}")
        return response.get("result"), response["_latency_ms"]

    def wait_for_notification(self, method, predicate=None, timeout=10.0):
        for index, notification in enumerate(self.notifications):
            if notification.get("method") != method:
                continue
            if predicate is None or predicate(notification.get("params")):
                self.notifications.pop(index)
                return notification.get("params")
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise LspFailure(f"timed out waiting for {method}")
            incoming = self.read_message(remaining)
            if incoming.get("method") == method and (predicate is None or predicate(incoming.get("params"))):
                return incoming.get("params")
            if "id" in incoming and "method" not in incoming:
                self.responses[incoming["id"]] = incoming
            elif "method" in incoming:
                self.notifications.append(incoming)

    def close(self):
        if self.process.poll() is None:
            try:
                self.request("shutdown", timeout=20.0)
                self.notify("exit")
                if self.process.stdin is not None:
                    self.process.stdin.close()
            except (LspFailure, BrokenPipeError, OSError):
                pass
        try:
            self.process.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5.0)
        return self.process.returncode
