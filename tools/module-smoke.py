#!/usr/bin/env python3
"""Load a module graph over HTTP in a real Otlyra and check it with BiDi.

Usage: tools/module-smoke.py path/to/otlyra

Serves tools/fixtures/modules on a loopback port with a standard-library
HTTP server (JavaScript as text/javascript, `/old/*` redirected to `/lib/*`,
anything else a 404), drives the browser through WebDriver BiDi exactly as
tools/bidi-smoke.py does, and reads the page's results back with
`script.evaluate`. Exits non-zero on any mismatch.
"""

import http.server
import importlib.util
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import threading
import time

TOOLS = pathlib.Path(__file__).resolve().parent
ROOT = TOOLS / "fixtures" / "modules"

_spec = importlib.util.spec_from_file_location("bidi_smoke", TOOLS / "bidi-smoke.py")
bidi = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(bidi)

TYPES = {".html": "text/html; charset=utf-8", ".js": "text/javascript", ".txt": "text/plain"}


class Handler(http.server.BaseHTTPRequestHandler):
    requests: list = []

    def do_GET(self) -> None:  # noqa: N802 - the standard library's name
        Handler.requests.append(self.path)
        path = self.path.split("?", 1)[0]
        if path.startswith("/old/"):
            self.send_response(302)
            self.send_header("Location", "/lib/" + path[len("/old/"):])
            self.end_headers()
            return
        file = (ROOT / path.lstrip("/")).resolve()
        if ROOT not in file.parents or not file.is_file():
            self.send_error(404)
            return
        body = file.read_bytes()
        self.send_response(200)
        self.send_header("Content-Type", TYPES.get(file.suffix, "application/octet-stream"))
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args) -> None:
        pass


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        return 2
    binary = pathlib.Path(sys.argv[1]).resolve()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    origin = f"http://127.0.0.1:{server.server_address[1]}"
    with tempfile.TemporaryDirectory(prefix="otlyra-module-smoke-") as profile:
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
            client = bidi.Client(url)
            client.call("session.new", {"capabilities": {}})
            context = client.call("browsingContext.getTree", {})["contexts"][0]["context"]
            client.call(
                "browsingContext.navigate",
                {"context": context, "url": f"{origin}/index.html", "wait": "complete"},
            )

            def value(expression: str):
                return bidi.evaluate(client, context, expression)

            # The dynamic import finishes after the load; give it a moment.
            deadline = time.monotonic() + 5
            while value("results.done === true") != {"type": "boolean", "value": True}:
                if time.monotonic() > deadline:
                    break
                time.sleep(0.05)

            def string(text: str) -> dict:
                return {"type": "string", "value": text}

            checks = [
                ("modules ran in order around the load events", value("order.join(',')"),
                 string("classic,main:ab:util:/lib/r.js,inline:util,mime error,404 error,dcl,load")),
                ("an inline module's import.meta.url is the document", value("results.inlineMeta"),
                 string(f"{origin}/index.html")),
                ("an external module's import.meta.url is its own", value("results.mainMeta"),
                 string(f"{origin}/main.js")),
                ("currentScript is null in a module", value("results.currentScript"), {"type": "null"}),
                ("a module runs after the whole document is parsed", value("results.sawParagraph"),
                 {"type": "boolean", "value": True}),
                ("a module is strict", value("results.strict"), {"type": "boolean", "value": True}),
                ("a shared module evaluated once", value("utilEvaluations"), {"type": "number", "value": 1}),
                ("a dynamic import through the import map settled", value("results.lazy"),
                 string("lazy:lazy.js")),
            ]
            asked = [path for path in Handler.requests if path != "/favicon.ico"]
            repeats = sorted({path for path in asked if asked.count(path) > 1})
            checks.append(("every module fetched once", repeats, []))

            failed = 0
            for name, actual, expected in checks:
                ok = actual == expected
                failed += not ok
                print(f"{'ok  ' if ok else 'FAIL'} {name}: {json.dumps(actual)}")
            print(f"requests: {asked}")
            return 1 if failed else 0
        finally:
            browser.kill()
            browser.wait()
            server.shutdown()


if __name__ == "__main__":
    sys.exit(main())
