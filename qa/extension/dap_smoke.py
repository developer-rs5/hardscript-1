#!/usr/bin/env python3
import argparse
import json
import os
import pathlib
import select
import subprocess
import sys
import time


class DapFailure(RuntimeError):
    pass


class DapClient:
    def __init__(self, argv, cwd):
        self.process = subprocess.Popen(
            argv,
            cwd=str(cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        self.buffer = bytearray()
        self.sequence = 1
        self.events = []
        self.latencies = {}

    def send(self, command, arguments=None):
        message = {"seq": self.sequence, "type": "request", "command": command}
        self.sequence += 1
        if arguments is not None:
            message["arguments"] = arguments
        payload = json.dumps(message).encode("utf-8")
        self.process.stdin.write(f"Content-Length: {len(payload)}\r\n\r\n".encode("ascii") + payload)
        self.process.stdin.flush()
        return message["seq"]

    def read(self, timeout=60.0):
        deadline = time.monotonic() + timeout
        while True:
            marker = self.buffer.find(b"\r\n\r\n")
            if marker >= 0:
                header = bytes(self.buffer[:marker]).decode("ascii", "replace")
                length = 0
                for line in header.split("\r\n"):
                    if line.lower().startswith("content-length"):
                        length = int(line.split(":", 1)[1])
                start = marker + 4
                if len(self.buffer) >= start + length:
                    body = bytes(self.buffer[start:start + length])
                    del self.buffer[:start + length]
                    return json.loads(body)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise DapFailure("timed out waiting for debug adapter output")
            select.select([self.process.stdout], [], [], remaining)
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise DapFailure("debug adapter closed its output stream")
            self.buffer.extend(chunk)

    def wait_response(self, request_seq, command, timeout=60.0):
        started = time.monotonic()
        deadline = time.monotonic() + timeout
        while True:
            message = self.read(max(0.1, deadline - time.monotonic()))
            if message.get("type") == "response" and message.get("request_seq") == request_seq:
                if not message.get("success", False):
                    raise DapFailure(f"{command} failed: {message.get('message')}")
                self.latencies[command] = (time.monotonic() - started) * 1000.0
                return message
            if message.get("type") == "event":
                self.events.append(message)

    def pump(self, deadline, stop_on):
        while time.monotonic() < deadline:
            message = self.read(max(0.1, deadline - time.monotonic()))
            if message.get("type") != "event":
                continue
            self.events.append(message)
            if message.get("event") in stop_on:
                return message
        raise DapFailure(f"timed out waiting for events {sorted(stop_on)}")

    def event_names(self):
        return [event.get("event") for event in self.events]

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        try:
            self.process.wait(timeout=5.0)
        except subprocess.TimeoutExpired:
            pass
        return self.process.returncode


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--adapter", required=True)
    parser.add_argument("--program", required=True)
    parser.add_argument("--hard", required=True)
    parser.add_argument("--breakpoint-line", type=int, default=8)
    parser.add_argument("--output", default="qa/extension/results.json")
    args = parser.parse_args()

    adapter = pathlib.Path(args.adapter).resolve()
    program = pathlib.Path(args.program).resolve()
    hard = pathlib.Path(args.hard).resolve()
    for candidate in (adapter, program, hard):
        if not candidate.is_file():
            print(f"extension qa: missing {candidate}", file=sys.stderr)
            return 2

    project = program.parent
    client = DapClient(["node", str(adapter)], project)
    records = []
    failures = []
    exit_code = None

    def case(name, action):
        started = time.monotonic()
        try:
            detail = action(client)
        except Exception as error:
            failures.append(f"{name}: {error}")
            records.append({"name": name, "status": "fail", "duration_ms": (time.monotonic() - started) * 1000.0, "detail": str(error)})
            print(f"extension qa: FAIL {name}: {error}", file=sys.stderr)
        else:
            records.append({"name": name, "status": "pass", "duration_ms": (time.monotonic() - started) * 1000.0, "detail": str(detail) if detail is not None else None})
            print(f"extension qa: pass {name}")

    try:
        def initialize(_):
            client.wait_response(
                client.send("initialize", {
                    "adapterID": "hardscript",
                    "clientID": "hardscript-extension-qa",
                    "adapterSpecificArgs": {},
                    "linesStartAt1": True,
                    "columnsStartAt1": True,
                    "pathFormat": "path",
                    "supportsRunInTerminalRequest": False,
                }),
                "initialize",
            )
            return "capabilities negotiated"

        case("initialize", initialize)

        def launch(_):
            client.wait_response(
                client.send("launch", {
                    "program": str(program),
                    "cwd": str(project),
                    "args": [],
                    "stopOnEntry": False,
                    "hardPath": str(hard),
                }),
                "launch",
            )
            return f"launched {program.name}"

        case("launch", launch)

        def set_breakpoints(_):
            response = client.wait_response(
                client.send("setBreakpoints", {
                    "source": {"name": program.name, "path": str(program)},
                    "breakpoints": [{"line": args.breakpoint_line}],
                    "lines": [args.breakpoint_line],
                    "sourceModified": False,
                }),
                "setBreakpoints",
            )
            verified = response.get("body", {}).get("breakpoints", [])
            if not verified or not verified[0].get("verified"):
                raise DapFailure(f"breakpoint not verified: {response.get('body')}")
            return f"line {args.breakpoint_line} verified"

        case("set_breakpoints", set_breakpoints)

        def configuration_done(_):
            client.wait_response(client.send("configurationDone"), "configurationDone")
            return "configuration accepted"

        case("configuration_done", configuration_done)

        def program_started(_):
            client.pump(time.monotonic() + 60.0, {"stopped"})
            stopped = [event for event in client.events if event.get("event") == "stopped"]
            if not stopped or stopped[0].get("body", {}).get("line") != args.breakpoint_line:
                raise DapFailure(f"unexpected stop: {stopped}")
            names = client.event_names()
            for required in ("process", "thread"):
                if required not in names:
                    raise DapFailure(f"missing {required} event: {names}")
            return f"stopped at line {args.breakpoint_line}"

        case("program_start", program_started)

        def program_output(_):
            client.pump(time.monotonic() + 60.0, {"output"})
            outputs = [event for event in client.events if event.get("event") == "output"]
            categories = {event.get("body", {}).get("category") for event in outputs}
            if "console" not in categories:
                raise DapFailure(f"missing adapter console output: {categories}")
            return f"categories {sorted(categories)}"

        case("program_output", program_output)

        def stack_and_scopes(_):
            client.wait_response(client.send("threads"), "threads")
            client.wait_response(client.send("stackTrace", {"threadId": 1}), "stackTrace")
            response = client.wait_response(client.send("scopes", {"frameId": 1}), "scopes")
            frames = response.get("body", {}).get("scopes", [])
            if not frames:
                raise DapFailure("no scopes reported")
            return f"{len(frames)} scope(s)"

        case("stack_and_scopes", stack_and_scopes)

        def disconnect(_):
            client.wait_response(client.send("disconnect", {"restart": False, "terminateDebuggee": True}), "disconnect")
            client.pump(time.monotonic() + 30.0, {"terminated"})
            try:
                client.process.wait(timeout=10.0)
            except subprocess.TimeoutExpired:
                raise DapFailure("debug adapter did not exit after disconnect")
            return f"adapter exited with code {client.process.returncode}"

        case("disconnect", disconnect)
    finally:
        exit_code = client.close()

    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps({
        "tool": "hardscript-extension-qa",
        "adapter": str(adapter),
        "program": str(program),
        "hard": str(hard),
        "breakpoint_line": args.breakpoint_line,
        "events": client.event_names(),
        "adapter_exit_code": exit_code,
        "passed": len(records) - len(failures),
        "failed": len(failures),
        "records": records,
    }, indent=2) + "\n", encoding="utf-8")
    print(f"extension qa: {len(records) - len(failures)} passed, {len(failures)} failed; results: {output}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
