"""A WebDriver BiDi client for Otlyra, in the standard library and nothing else.

Short on purpose. The protocol is JSON over a WebSocket: a command goes out with
an id, an answer comes back under the same id. Everything below that is framing,
and the framing is ninety lines — which is the point. You do not need a package
to talk to this browser, and reading this file is a fair way to learn what the
browser will answer.

For real work use a real client — Puppeteer and Selenium both speak BiDi, and
they handle reconnection, timeouts and events properly. This is here so the
protocol can be *seen*.
"""

import base64
import json
import os
import socket
import struct
import subprocess
import sys
import time


class Otlyra:
    """A running Otlyra, and a socket to it."""

    def __init__(self, binary="./target/debug/otlyra", width=1000, height=700):
        # Port 0 asks the system for a free one, and the browser prints the
        # address it got. Guessing a port is how two of these fight each other.
        self.process = subprocess.Popen(
            [binary, "--bidi", "0", "--width", str(width), "--height", str(height)],
            stdout=subprocess.PIPE,
            text=True,
        )
        url = self.process.stdout.readline().strip()
        if not url.startswith("ws://"):
            raise RuntimeError(f"expected a ws:// address and got {url!r}")
        self.socket = _connect(url)
        self.next_id = 0
        self.send("session.new")

    def send(self, method, **params):
        """Send one command and return its result, raising on an error."""
        self.next_id += 1
        _write(self.socket, {"id": self.next_id, "method": method, "params": params})
        reply = _read(self.socket)
        if reply.get("type") == "error":
            raise BiDiError(method, reply.get("error"), reply.get("message"))
        return reply.get("result", {})

    def navigate(self, url):
        """Go to `url` and wait for it to arrive."""
        return self.send("browsingContext.navigate", url=url)["url"]

    def screenshot(self, path):
        """Write what is on screen to `path`, as a PNG."""
        data = self.send("browsingContext.captureScreenshot")["data"]
        with open(path, "wb") as file:
            file.write(base64.b64decode(data))
        return path

    def find(self, selector):
        """Every node matching a CSS selector, as the engine's own matcher sees it."""
        return self.send(
            "browsingContext.locateNodes",
            locator={"type": "css", "value": selector},
        )["nodes"]

    def click(self, node):
        """Click the centre of a node.

        The centre is worked out by the browser, from where it actually drew the
        element. That is the whole point of naming an element instead of a
        coordinate: the driver does not have to know the layout, and so cannot
        disagree with it.
        """
        origin = {"type": "element", "element": {"sharedId": node["sharedId"]}}
        return self.send(
            "input.performActions",
            actions=[
                {
                    "type": "pointer",
                    "id": "mouse",
                    "actions": [
                        {"type": "pointerMove", "x": 0, "y": 0, "origin": origin},
                        {"type": "pointerDown", "button": 0},
                        {"type": "pointerUp", "button": 0},
                    ],
                }
            ],
        )

    def type(self, text):
        """Type `text`, one key at a time, wherever the focus is."""
        actions = []
        for character in text:
            actions.append({"type": "keyDown", "value": character})
            actions.append({"type": "keyUp", "value": character})
        return self.send(
            "input.performActions",
            actions=[{"type": "key", "id": "keyboard", "actions": actions}],
        )

    def scroll(self, x, y, amount):
        """Turn the wheel by `amount` at a point."""
        return self.send(
            "input.performActions",
            actions=[
                {
                    "type": "wheel",
                    "id": "wheel",
                    "actions": [
                        {
                            "type": "scroll",
                            "x": x,
                            "y": y,
                            "deltaX": 0,
                            "deltaY": amount,
                        }
                    ],
                }
            ],
        )

    def close(self):
        try:
            self.send("session.end")
        except Exception:
            pass
        self.socket.close()
        self.process.terminate()
        self.process.wait(timeout=5)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


class BiDiError(RuntimeError):
    """What the browser said when it would not do something."""

    def __init__(self, method, code, message):
        super().__init__(f"{method}: {code}: {message}")
        self.code = code
        self.method = method


# --- the framing, which is all this file is -------------------------------


def _connect(url):
    host, rest = url[len("ws://") :].split(":", 1)
    port, _, path = rest.partition("/")
    sock = socket.create_connection((host, int(port)))
    key = base64.b64encode(os.urandom(16)).decode()
    sock.sendall(
        (
            f"GET /{path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )
    handshake = b""
    while b"\r\n\r\n" not in handshake:
        handshake += sock.recv(4096)
    if b" 101 " not in handshake.split(b"\r\n")[0]:
        raise RuntimeError(f"the browser refused the upgrade: {handshake[:120]!r}")
    return sock


def _write(sock, message):
    payload = json.dumps(message).encode()
    # A client's frames are always masked; the mask is four random bytes.
    mask = os.urandom(4)
    header = bytes([0x81])
    length = len(payload)
    if length < 126:
        header += bytes([0x80 | length])
    elif length < 1 << 16:
        header += bytes([0x80 | 126]) + struct.pack(">H", length)
    else:
        header += bytes([0x80 | 127]) + struct.pack(">Q", length)
    masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
    sock.sendall(header + mask + masked)


def _read(sock):
    def exactly(count):
        out = b""
        while len(out) < count:
            chunk = sock.recv(count - len(out))
            if not chunk:
                raise EOFError("the browser closed the connection")
            out += chunk
        return out

    header = exactly(2)
    length = header[1] & 0x7F
    if length == 126:
        length = struct.unpack(">H", exactly(2))[0]
    elif length == 127:
        length = struct.unpack(">Q", exactly(8))[0]
    return json.loads(exactly(length))


if __name__ == "__main__":
    page = sys.argv[1] if len(sys.argv) > 1 else "https://example.com/"
    with Otlyra() as browser:
        print("→", page)
        print("←", browser.navigate(page))
        print("←", browser.screenshot("/tmp/otlyra.png"))
