#!/usr/bin/env python3
"""Drive a real `otlyra --bidi` over WebDriver BiDi and check page input/script.

usage: tools/bidi-smoke.py path/to/otlyra

Starts the browser with `--bidi 0` (the port it binds is printed on stdout),
connects with a minimal standard-library WebSocket client, loads the
`tools/fixtures/bidi-input.html` fixture, clicks its button and types into its
field through `input.performActions`, and reads what the page heard through
`script.evaluate` and `script.callFunction`. Then it serves the fixtures over
loopback HTTP and checks the web APIs in `web-apis.html` through
`script.evaluate`. Exits non-zero on any mismatch.
"""

import base64
import functools
import http.server
import json
import os
import pathlib
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse

FIXTURES = pathlib.Path(__file__).resolve().parent / "fixtures"
FIXTURE = FIXTURES / "bidi-input.html"


def serve_fixtures() -> http.server.ThreadingHTTPServer:
    """The fixtures over HTTP on a free loopback port, for the checks of APIs
    a `file:` page does not get the way a web page does."""
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *args) -> None:
            pass

    handler = functools.partial(Quiet, directory=str(FIXTURES))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


class Client:
    """Just enough RFC 6455 for one JSON text conversation."""

    def __init__(self, url: str) -> None:
        parsed = urllib.parse.urlparse(url)
        self.sock = socket.create_connection((parsed.hostname, parsed.port), timeout=30)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f"GET {parsed.path} HTTP/1.1\r\nHost: {parsed.netloc}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(request.encode())
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError("the server closed during the handshake")
            response += chunk
        if b" 101 " not in response.split(b"\r\n", 1)[0]:
            raise RuntimeError(f"handshake refused: {response!r}")
        self.buffer = response.split(b"\r\n\r\n", 1)[1]
        self.next_id = 0

    def _read(self, count: int) -> bytes:
        while len(self.buffer) < count:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("the server closed the connection")
            self.buffer += chunk
        data, self.buffer = self.buffer[:count], self.buffer[count:]
        return data

    def _send_text(self, text: str) -> None:
        payload = text.encode()
        mask = os.urandom(4)
        header = bytes([0x81])
        if len(payload) < 126:
            header += bytes([0x80 | len(payload)])
        elif len(payload) < 65536:
            header += bytes([0x80 | 126]) + struct.pack(">H", len(payload))
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", len(payload))
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def _receive_text(self) -> str:
        message = b""
        while True:
            first, second = self._read(2)
            length = second & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._read(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._read(8))[0]
            payload = self._read(length)
            opcode = first & 0x0F
            if opcode == 0x8:
                raise RuntimeError("the server closed the session")
            if opcode in (0x9, 0xA):
                continue
            message += payload
            if first & 0x80:
                return message.decode()

    def call(self, method: str, params: dict) -> dict:
        self.next_id += 1
        command_id = self.next_id
        self._send_text(json.dumps({"id": command_id, "method": method, "params": params}))
        while True:
            answer = json.loads(self._receive_text())
            if answer.get("id") != command_id:
                continue  # an event
            if answer.get("type") == "error":
                raise RuntimeError(f"{method}: {answer.get('error')}: {answer.get('message')}")
            return answer["result"]


def evaluate(client: Client, context: str, expression: str) -> dict:
    return client.call(
        "script.evaluate",
        {"expression": expression, "target": {"context": context}, "awaitPromise": True},
    )["result"]


def poll(read, seconds: float = 10.0) -> dict:
    """What `read` answers once it is a non-empty string, or its last answer
    when that takes longer than `seconds`: for what a page learns turns
    after it asked."""
    deadline = time.monotonic() + seconds
    while True:
        answer = read()
        if answer.get("value") or time.monotonic() >= deadline:
            return answer
        time.sleep(0.05)


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    binary = pathlib.Path(sys.argv[1]).resolve()
    with tempfile.TemporaryDirectory(prefix="otlyra-bidi-smoke-") as profile:
        env = dict(os.environ, OTLYRA_CONFIG_DIR=profile, OTLYRA_NO_KEYCHAIN="1")
        env.setdefault("OTLYRA_LOG", "error")
        browser = subprocess.Popen(
            [str(binary), "--no-secrets", "--bidi", "0"],
            stdout=subprocess.PIPE,
            env=env,
            text=True,
        )
        try:
            url = browser.stdout.readline().strip()
            if not url.startswith("ws://"):
                raise RuntimeError(f"no BiDi address on stdout: {url!r}")
            client = Client(url)
            client.call("session.new", {"capabilities": {}})
            context = client.call("browsingContext.getTree", {})["contexts"][0]["context"]
            client.call(
                "browsingContext.navigate",
                {"context": context, "url": FIXTURE.as_uri(), "wait": "complete"},
            )
            checks = []

            def node(selector: str) -> str:
                found = client.call(
                    "browsingContext.locateNodes",
                    {"context": context, "locator": {"type": "css", "value": selector}},
                )
                return found["nodes"][0]["sharedId"]

            def click(selector: str) -> None:
                client.call(
                    "input.performActions",
                    {
                        "context": context,
                        "actions": [
                            {
                                "type": "pointer",
                                "id": "mouse",
                                "actions": [
                                    {
                                        "type": "pointerMove",
                                        "x": 0,
                                        "y": 0,
                                        "origin": {
                                            "type": "element",
                                            "element": {"sharedId": node(selector)},
                                        },
                                    },
                                    {"type": "pointerDown", "button": 0},
                                    {"type": "pointerUp", "button": 0},
                                ],
                            }
                        ],
                    },
                )

            click("#go")
            checks.append(("click reached the listener", evaluate(client, context, "log.join(' ')"),
                           {"type": "string", "value": "click:true"}))
            click("#field")
            client.call(
                "input.performActions",
                {
                    "context": context,
                    "actions": [
                        {
                            "type": "key",
                            "id": "keyboard",
                            "actions": [
                                {"type": "keyDown", "value": "o"},
                                {"type": "keyUp", "value": "o"},
                                {"type": "keyDown", "value": "k"},
                                {"type": "keyUp", "value": "k"},
                            ],
                        }
                    ],
                },
            )
            checks.append(("typing reached the field's value", evaluate(client, context, "field.value"),
                           {"type": "string", "value": "ok"}))
            checks.append(("input events were heard", evaluate(client, context, "inputs"),
                           {"type": "number", "value": 2}))
            called = client.call(
                "script.callFunction",
                {
                    "functionDeclaration": "(a, b) => a * b",
                    "arguments": [{"type": "number", "value": 6}, {"type": "number", "value": 7}],
                    "target": {"context": context},
                    "awaitPromise": False,
                },
            )["result"]
            checks.append(("callFunction answers by value", called, {"type": "number", "value": 42}))

            server = serve_fixtures()
            origin = f"http://127.0.0.1:{server.server_address[1]}"
            client.call(
                "browsingContext.navigate",
                {"context": context, "url": f"{origin}/web-apis.html", "wait": "complete"},
            )
            checks.append((
                "a 2D canvas draws shapes, transforms, pictures and text",
                evaluate(client, context, "canvasCheck()"),
                {"type": "string", "value": "255,0,0,255 0,0,255,255 0,255,0,255 0,0,0,0 true"},
            ))
            evaluate(client, context, "workerCheck()")
            checks.append((
                "a dedicated worker imports, fetches and answers through structured clone",
                poll(lambda: evaluate(client, context, "workerAnswer")),
                {"type": "string", "value": "true,true,1,2,imported,true,undefined"},
            ))
            server.shutdown()

            failed = 0
            for name, actual, expected in checks:
                ok = actual == expected
                failed += not ok
                print(f"{'ok  ' if ok else 'FAIL'} {name}: {json.dumps(actual)}")
            return 1 if failed else 0
        finally:
            browser.kill()
            browser.wait()


if __name__ == "__main__":
    sys.exit(main())
