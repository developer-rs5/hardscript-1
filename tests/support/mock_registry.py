#!/usr/bin/env python3
"""Mock .hspkg registry used by the package-manager regression suite.

Serves a small HTTP/1.1 registry over 127.0.0.1:

  GET /packages/{name}           -> metadata JSON
  GET /packages/{name}/{version} -> raw .hspkg archive bytes
  GET /search?q={query}          -> { "results": [...] }

The registry database is a JSON fixture (see tests/fixtures/pm-registry.json)
which describes packages/versions and the source files each archive contains.
At startup every archive is built deterministically and its sha256 recorded;
the sha256 is what metadata reports as `integrity`. A package version with
`"corrupt": true` serves bytes with the final byte flipped *while* publishing
the pristine sha, so the client's integrity check must fail.

Every request is appended (one line per request) to the logfile given with
--logfile, so tests can assert download counts.
"""
import hashlib
import json
import socket
import struct
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

MAGIC = b"HSPKG\x00\x01"


def jdump(obj):
    """Compact JSON: the client's parser mirrors the reference serializer,
    which emits no whitespace ("," ":" separators)."""
    return json.dumps(obj, separators=(",", ":")).encode()


def pack_archive(files):
    """Build an .hspkg archive from [(rel_path, bytes), ...]."""
    files = sorted(files, key=lambda f: f[0])
    out = bytearray(MAGIC)
    out += struct.pack("<I", len(files))
    for rel, data in files:
        rb = rel.encode("utf-8")
        out += struct.pack("<Q", len(rb))
        out += rb
        out += struct.pack("<Q", len(data))
        out += data
    return bytes(out)


class MockRegistry(BaseHTTPRequestHandler):
    server_version = "mock-hs-registry/0.4"

    def log_request(self, code="-", size="-"):
        # quiet, but write a structured request log line
        if self.server.logfile:
            line = f"{self.path} -> {code}\n"
            with open(self.server.logfile, "a") as f:
                f.write(line)

    def _send(self, code, body, ctype):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        if body:
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

    def do_GET(self):
        path = self.path.split("?")[0]
        parts = [p for p in path.split("/") if p]

        if parts and parts[0] == "search":
            q = ""
            if "?" in self.path:
                qpart = self.path.split("?", 1)[1]
                for kv in qpart.split("&"):
                    if kv.startswith("q="):
                        q = kv[2:]
            results = self.server.db.get("search", {}).get(q, [])
            body = jdump({"results": results})
            self._send(200, body, "application/json")
            return

        if len(parts) == 2 and parts[0] == "packages":
            self._metadata(parts[1])
            return
        if len(parts) == 3 and parts[0] == "packages":
            self._download(parts[1], parts[2])
            return

        self._send(404, b'{"error":"not found"}', "application/json")

    def _metadata(self, name):
        pkg = self.server.db["packages"].get(name)
        if not pkg:
            self._send(404, b'{"error":"no such package"}', "application/json")
            return
        versions = []
        for ver in pkg["versions"]:
            entry = {
                "version": ver["version"],
                "dependencies": [
                    {"name": d, "req": r}
                    for d, r in sorted(ver.get("dependencies", {}).items())
                ],
                "integrity": "sha256:" + ver["_sha256"],
            }
            if ver.get("description"):
                entry["description"] = ver["description"]
            versions.append(entry)
        body = jdump({"name": name, "versions": versions})
        self._send(200, body, "application/json")

    def _download(self, name, version):
        pkg = self.server.db["packages"].get(name)
        if not pkg:
            self._send(404, b"", "application/octet-stream")
            return
        for ver in pkg["versions"]:
            if ver["version"] != version:
                continue
            if ver.get("corrupt"):
                body = ver["_serve"][:-1] + bytes([ver["_serve"][-1] ^ 0xFF])
            else:
                body = ver["_serve"]
            self._send(200, body, "application/octet-stream")
            return
        self._send(404, b"", "application/octet-stream")


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def load_db(path):
    with open(path) as f:
        spec = json.load(f)
    db = {"packages": {}, "search": spec.get("search", {})}
    for pkg in spec["packages"]:
        name = pkg["name"]
        for ver in pkg["versions"]:
            files = []
            for fspec in ver.get("files", [("main.hard", "")]):
                path_, content = fspec
                files.append((path_, content.encode("utf-8")))
            archive = pack_archive(files)
            v = dict(ver)
            v["_sha256"] = hashlib.sha256(archive).hexdigest()
            v["_serve"] = archive
            db["packages"].setdefault(name, {"versions": []})
            db["packages"][name]["versions"].append(v)
    return db


def main():
    if len(sys.argv) < 3:
        print("usage: mock_registry.py <db.json> <port> [--logfile FILE]", file=sys.stderr)
        sys.exit(2)
    db_path = sys.argv[1]
    port = int(sys.argv[2])
    logfile = None
    if "--logfile" in sys.argv:
        logfile = sys.argv[sys.argv.index("--logfile") + 1]

    db = load_db(db_path)
    server = ThreadingHTTPServer(("127.0.0.1", port), MockRegistry)
    server.db = db
    server.logfile = logfile
    # announce readiness to the runner
    print(f"READY {port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()