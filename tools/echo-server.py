#!/usr/bin/env python3
"""A server that prints what a form sent it, for trying `tests/pages/try.html` by hand.

Not part of the browser and not run by anything: it exists so that a form with a
file in it can be sent somewhere that says what arrived. `just echo-server`.
"""

import email
import http.server
import socketserver
import sys

PORT = 8744


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("content-length", 0))
        body = self.rfile.read(length)
        content_type = self.headers.get("content-type", "")
        print(f"\n{self.command} {self.path}")
        print(f"  content-type: {content_type}")
        print(f"  {len(body)} bytes")

        lines = []
        if content_type.startswith("multipart/form-data"):
            parsed = email.message_from_bytes(
                f"Content-Type: {content_type}\r\nMIME-Version: 1.0\r\n\r\n".encode() + body
            )
            for part in parsed.get_payload():
                name = part.get_param("name", header="content-disposition")
                filename = part.get_filename()
                payload = part.get_payload(decode=True) or b""
                if filename is None:
                    lines.append(f"  {name} = {payload.decode('utf-8', 'replace')}")
                else:
                    lines.append(
                        f"  {name} = file {filename!r} ({part.get_content_type()}, "
                        f"{len(payload)} bytes)"
                    )
        else:
            lines.append("  " + body.decode("utf-8", "replace"))

        for line in lines:
            print(line)
        sys.stdout.flush()

        answer = ("<!doctype html><meta charset=utf-8><title>arrived</title>"
                  "<h1>It arrived</h1><pre>" + "\n".join(lines) + "</pre>").encode()
        self.send_response(200)
        self.send_header("content-type", "text/html; charset=utf-8")
        self.send_header("content-length", str(len(answer)))
        self.end_headers()
        self.wfile.write(answer)

    def log_message(self, *args):
        pass


socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", PORT), Handler) as server:
    print(f"listening on http://127.0.0.1:{PORT}/take — ^C to stop")
    server.serve_forever()
