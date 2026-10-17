#!/usr/bin/env python3
"""Chrome page-load timings over CDP: cold (fresh profile) then warm runs.

usage: chrome_load.py RUNS URL...
Prints one JSON line per run: url, run, response, dcl, load, fcp (ms since
navigation start, from the page's own Navigation/Paint Timing entries) and
wall (ms from Page.navigate to Page.loadEventFired, as seen from outside).
"""
import base64, json, os, shutil, socket, struct, subprocess, sys, tempfile, time, urllib.parse, urllib.request

CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"


class Client:
    def __init__(self, url):
        p = urllib.parse.urlparse(url)
        self.sock = socket.create_connection((p.hostname, p.port), timeout=120)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((f"GET {p.path} HTTP/1.1\r\nHost: {p.netloc}\r\nUpgrade: websocket\r\n"
                           f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
        r = b""
        while b"\r\n\r\n" not in r:
            r += self.sock.recv(4096)
        self.buf = r.split(b"\r\n\r\n", 1)[1]
        self.n = 0
        self.events = []

    def _read(self, k):
        while len(self.buf) < k:
            c = self.sock.recv(65536)
            if not c:
                raise RuntimeError("closed")
            self.buf += c
        d, self.buf = self.buf[:k], self.buf[k:]
        return d

    def send(self, text):
        b = text.encode(); m = os.urandom(4); h = bytes([0x81])
        if len(b) < 126: h += bytes([0x80 | len(b)])
        elif len(b) < 65536: h += bytes([0x80 | 126]) + struct.pack(">H", len(b))
        else: h += bytes([0x80 | 127]) + struct.pack(">Q", len(b))
        self.sock.sendall(h + m + bytes(x ^ m[i % 4] for i, x in enumerate(b)))

    def recv(self):
        msg = b""
        while True:
            a, b = self._read(2); n = b & 0x7F
            if n == 126: n = struct.unpack(">H", self._read(2))[0]
            elif n == 127: n = struct.unpack(">Q", self._read(8))[0]
            pl = self._read(n)
            if (a & 0x0F) in (0x9, 0xA): continue
            msg += pl
            if a & 0x80: return json.loads(msg)

    def call(self, method, params=None):
        self.n += 1; i = self.n
        self.send(json.dumps({"id": i, "method": method, "params": params or {}}))
        while True:
            m = self.recv()
            if m.get("id") == i:
                if "error" in m: raise RuntimeError(m["error"])
                return m["result"]
            self.events.append(m)

    def wait_event(self, name, timeout):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            for k, e in enumerate(self.events):
                if e.get("method") == name:
                    del self.events[k]; return e
            self.sock.settimeout(max(0.1, end - time.monotonic()))
            try: self.events.append(self.recv())
            except socket.timeout: break
        return None


TIMINGS = """(() => {
  const n = performance.getEntriesByType('navigation')[0];
  const f = performance.getEntriesByName('first-contentful-paint')[0];
  return JSON.stringify({response: n.responseEnd, dcl: n.domContentLoadedEventEnd,
    load: n.loadEventEnd, fcp: f ? f.startTime : null});
})()"""


def main():
    runs = int(sys.argv[1]); urls = sys.argv[2:]
    port = 9333
    for url in urls:
        profile = tempfile.mkdtemp(prefix="chrome-perf-")
        proc = subprocess.Popen([CHROME, "--headless=new", f"--remote-debugging-port={port}",
                                 f"--user-data-dir={profile}", "--window-size=1280,900",
                                 "--no-first-run", "--no-default-browser-check", "about:blank"],
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            for _ in range(100):
                try:
                    targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json"))
                    page = next(t for t in targets if t["type"] == "page")
                    break
                except Exception:
                    time.sleep(0.1)
            c = Client(page["webSocketDebuggerUrl"])
            c.call("Page.enable")
            for run in range(runs):
                c.call("Page.navigate", {"url": "about:blank"}); c.wait_event("Page.loadEventFired", 10)
                c.events.clear()
                t0 = time.monotonic()
                c.call("Page.navigate", {"url": url})
                ev = c.wait_event("Page.loadEventFired", 60)
                wall = (time.monotonic() - t0) * 1000
                time.sleep(0.3)
                r = json.loads(c.call("Runtime.evaluate", {"expression": TIMINGS, "returnByValue": True})["result"]["value"])
                r.update(url=url, run=run, cache="cold" if run == 0 else "warm", wall=round(wall), fired=ev is not None)
                print(json.dumps({k: (round(v) if isinstance(v, float) else v) for k, v in r.items()}), flush=True)
        finally:
            proc.terminate(); proc.wait()
            shutil.rmtree(profile, ignore_errors=True)


main()
