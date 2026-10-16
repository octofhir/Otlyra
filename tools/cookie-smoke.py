#!/usr/bin/env python3
"""Check SameSite cookies end to end in a real Otlyra, driven over BiDi.

Usage: tools/cookie-smoke.py path/to/otlyra

One loopback server answers as two sites, `localhost` and `127.0.0.1`. A
cookie is set on `localhost` with no SameSite attribute, which makes it Lax.
A page on `127.0.0.1` then asks `localhost` for a picture, a `fetch` and a
form post, and none of them may carry it. A page on `localhost` asks for a
picture of its own, which must carry it. Exits non-zero on any mismatch.
"""

import http.server
import importlib.util
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import time

TOOLS = pathlib.Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("bidi_smoke", TOOLS / "bidi-smoke.py")
bidi = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bidi)

PNG = bytes.fromhex(
    "89504e470d0a1a0a0000000d4948445200000001000000010806000000"
    "1f15c4890000000d49444154789c6360000002000154a24f5d0000000049454e44ae426082"
)


class Handler(http.server.BaseHTTPRequestHandler):
    cookies: dict = {}

    def reply(self, content_type, body, extra=()):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        for name, value in extra:
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:  # noqa: N802 - the standard library's name
        Handler.cookies[self.path] = self.headers.get("Cookie")
        port = self.server.server_address[1]
        if self.path == "/set":
            self.reply("text/html", b"<title>set</title>", [("Set-Cookie", "lax=1; Path=/")])
        elif self.path == "/cross":
            page = (
                f"<title>cross</title><img src=http://localhost:{port}/cross.png>"
                f"<form id=f method=post action=http://localhost:{port}/post><input name=a value=1></form>"
                f"<script>fetch('http://localhost:{port}/api').catch(() => 0)</script>"
            )
            self.reply("text/html", page.encode())
        elif self.path == "/same":
            self.reply("text/html", b"<title>same</title><img src=/same.png>")
        elif self.path.endswith(".png"):
            self.reply("image/png", PNG)
        else:
            self.reply("text/plain", b"x", [("Access-Control-Allow-Origin", "*")])

    def do_POST(self) -> None:  # noqa: N802 - the standard library's name
        Handler.cookies[self.path] = self.headers.get("Cookie")
        self.rfile.read(int(self.headers.get("Content-Length") or 0))
        self.reply("text/html", b"<title>posted</title>")

    def log_message(self, *args) -> None:
        pass


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    binary = pathlib.Path(sys.argv[1]).resolve()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    port = server.server_address[1]
    with tempfile.TemporaryDirectory(prefix="otlyra-cookie-smoke-") as profile:
        env = dict(os.environ, OTLYRA_CONFIG_DIR=profile, OTLYRA_NO_KEYCHAIN="1")
        env.setdefault("OTLYRA_LOG", "error")
        browser = subprocess.Popen(
            [str(binary), "--no-secrets", "--bidi", "0"], stdout=subprocess.PIPE, env=env, text=True
        )
        try:
            client = bidi.Client(browser.stdout.readline().strip())
            client.call("session.new", {"capabilities": {}})
            context = client.call("browsingContext.getTree", {})["contexts"][0]["context"]

            def navigate(url):
                client.call("browsingContext.navigate", {"context": context, "url": url, "wait": "complete"})

            navigate(f"http://localhost:{port}/set")
            navigate(f"http://127.0.0.1:{port}/cross")
            time.sleep(0.5)
            bidi.evaluate(client, context, "document.getElementById('f').submit(), 1")
            time.sleep(1.0)
            navigate(f"http://localhost:{port}/same")
            time.sleep(0.3)

            expected = [
                ("a cross-site picture carries no Lax cookie", "/cross.png", None),
                ("a cross-site fetch carries no Lax cookie", "/api", None),
                ("a cross-site form post carries no Lax cookie", "/post", None),
                ("a same-site page carries it", "/same", "lax=1"),
                ("a same-site picture carries it", "/same.png", "lax=1"),
            ]
            failed = 0
            for name, path, cookie in expected:
                actual = Handler.cookies.get(path, "not requested")
                ok = actual == cookie
                failed += not ok
                print(f"{'ok  ' if ok else 'FAIL'} {name}: {actual}")
            return 1 if failed else 0
        finally:
            browser.kill()
            browser.wait()
            server.shutdown()


if __name__ == "__main__":
    sys.exit(main())
