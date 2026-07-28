#!/usr/bin/env python3
"""Copy a live page and everything it draws itself with into a directory.

Why a mirror exists at all. A number that compares our rendering to Chrome's is
worth having only if it is the same number tomorrow, and a live page is not the
same page tomorrow: it carries a timestamp, a rotating banner, a different
advertisement and a different set of hashed bundle names every hour. A mirror
freezes one page. It also gives a headless reference something it can actually
fetch — pointed at a live address, Chrome renders whatever the network and its
own cookies produce, which is not what we rendered.

**The scripts are stripped.** Not to make our number look better: with them in,
each browser's copy grows whatever its own engine wrote into the page, and the
difference stops being about layout. A mirror is a question about boxes, text
and pictures. `--keep-scripts` is there for when the question is a different one.

What is taken: the document, its stylesheets (and what *they* name — fonts,
background pictures, `@import`ed sheets), and its `<img>`. Everything lands in
`assets/` under a name derived from its address, so the same page mirrored twice
produces the same directory.

    tools/mirror.py https://ya.ru target/mirrors/ya.ru
    just reference target/mirrors/ya.ru/index.html 1280 900
"""

from __future__ import annotations

import argparse
import concurrent.futures
import gzip
import hashlib
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request
import zlib
from http.cookiejar import CookieJar

# A page served to something that says it is a browser is the page the reference
# browsers will be given. Asking as a script gets a different document — or a
# consent wall — and mirroring that measures nothing.
USER_AGENT = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36"
)

# What a `<script>` is in HTML: everything up to the first `</script`, whatever
# that text happens to be. The tokenizer's rule, so this cuts where a parser
# would.
SCRIPT = re.compile(rb"<script\b[^>]*>.*?</script\s*>", re.DOTALL | re.IGNORECASE)
# A `<link>` that only preloads a script has nothing left to point at.
PRELOAD_SCRIPT = re.compile(
    rb"<link\b[^>]*\bas\s*=\s*[\"']?script[\"']?[^>]*>", re.IGNORECASE
)

STYLESHEET = re.compile(
    rb"<link\b[^>]*\brel\s*=\s*[\"']?stylesheet[\"']?[^>]*>", re.IGNORECASE
)
STYLE_BLOCK = re.compile(rb"<style\b[^>]*>(.*?)</style\s*>", re.DOTALL | re.IGNORECASE)
ATTR = rb"""%s\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+))"""
HREF = re.compile(ATTR % rb"href", re.IGNORECASE)
SRC = re.compile(ATTR % rb"src", re.IGNORECASE)
SRCSET = re.compile(ATTR % rb"srcset", re.IGNORECASE)
IMG = re.compile(rb"<(?:img|source)\b[^>]*>", re.IGNORECASE)
CSS_URL = re.compile(r"""url\(\s*(?:"([^"]*)"|'([^']*)'|([^)]*))\s*\)""")
CSS_IMPORT = re.compile(r"""@import\s+(?:url\(\s*)?(?:"([^"]*)"|'([^']*)')""")


def attr_value(match: re.Match[bytes]) -> bytes:
    """The one group of the three an attribute pattern can match in."""
    return next(group for group in match.groups() if group is not None)


class Mirror:
    def __init__(self, out: str, keep_scripts: bool) -> None:
        self.out = out
        self.assets = os.path.join(out, "assets")
        self.keep_scripts = keep_scripts
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(CookieJar())
        )
        # url -> local file name in `assets/`, or None for one that would not
        # come down. Both are answers and both stop a second attempt.
        self.taken: dict[str, str | None] = {}
        self.failures = 0

    def fetch(self, url: str) -> tuple[bytes, str]:
        """The bytes at `url`, and the address they finally came from."""
        request = urllib.request.Request(
            url,
            headers={
                "User-Agent": USER_AGENT,
                "Accept": "text/html,application/xhtml+xml,image/*,*/*;q=0.8",
                "Accept-Language": "en-US,en;q=0.9,ru;q=0.8",
                # Not `br`: decoding brotli needs a dependency, and every server
                # that offers it also offers one of these.
                "Accept-Encoding": "gzip, deflate",
            },
        )
        with self.opener.open(request, timeout=30) as response:
            body = response.read()
            encoding = response.headers.get("Content-Encoding", "").lower()
            final = response.geturl()
        if encoding == "gzip":
            body = gzip.decompress(body)
        elif encoding == "deflate":
            body = zlib.decompress(body, -zlib.MAX_WBITS)
        return body, final

    def local_name(self, url: str, body: bytes) -> str:
        """A file name derived from the address, so a second mirror agrees.

        The extension is kept when the address has a usable one, because a
        reference browser decides what a file *is* partly from its name once it
        is loaded over `file:` and there are no headers left to say.
        """
        path = urllib.parse.urlsplit(url).path
        stem, extension = os.path.splitext(os.path.basename(path))
        if not re.fullmatch(r"\.[A-Za-z0-9]{1,5}", extension or ""):
            extension = sniff_extension(body)
        stem = re.sub(r"[^A-Za-z0-9._-]", "-", stem)[:40] or "asset"
        digest = hashlib.sha1(url.encode()).hexdigest()[:10]
        return f"{stem}-{digest}{extension}"

    def take(self, url: str, base: str) -> str | None:
        """Download one thing named by `url` against `base`; give its local name."""
        absolute = absolutize(url, base)
        if absolute is None:
            return None
        if absolute in self.taken:
            return self.taken[absolute]
        # Reserved before the fetch: two stylesheets naming the same font would
        # otherwise both fetch it.
        self.taken[absolute] = None
        try:
            body, _final = self.fetch(absolute)
        except (urllib.error.URLError, OSError, ValueError, EOFError) as error:
            print(f"  missed {absolute}: {error}", file=sys.stderr)
            self.failures += 1
            return None
        name = self.local_name(absolute, body)
        with open(os.path.join(self.assets, name), "wb") as file:
            file.write(body)
        self.taken[absolute] = name
        return name

    def take_stylesheet(self, url: str, base: str, depth: int = 0) -> str | None:
        """A stylesheet, and everything it names, with its own text rewritten.

        A sheet's `url()` is resolved against *the sheet*, not the document, so
        this has to happen where the sheet's own address is known. Its assets
        land beside it in `assets/`, which is why a rewritten reference is a
        bare file name.
        """
        absolute = absolutize(url, base)
        if absolute is None:
            return None
        if absolute in self.taken:
            return self.taken[absolute]
        self.taken[absolute] = None
        try:
            body, final = self.fetch(absolute)
        except (urllib.error.URLError, OSError, ValueError, EOFError) as error:
            print(f"  missed {absolute}: {error}", file=sys.stderr)
            self.failures += 1
            return None
        text = self.rewrite_css(body.decode("utf-8", "replace"), final, depth)
        name = self.local_name(absolute, body)
        with open(os.path.join(self.assets, name), "w", encoding="utf-8") as file:
            file.write(text)
        self.taken[absolute] = name
        return name

    def rewrite_css(self, text: str, base: str, depth: int) -> str:
        """Take what a sheet names and point it at the copies."""
        # `@import` first: a sheet that imports another is two sheets, and the
        # second one's own `url()` resolve against *it*. Bounded, because a pair
        # of sheets importing each other is a real thing on the web.
        def on_import(match: re.Match[str]) -> str:
            target = next(g for g in match.groups() if g is not None)
            if depth >= 4:
                return match.group(0)
            name = self.take_stylesheet(target, base, depth + 1)
            return match.group(0) if name is None else f'@import "{name}"'

        text = CSS_IMPORT.sub(on_import, text)

        def on_url(match: re.Match[str]) -> str:
            target = next(g for g in match.groups() if g is not None).strip()
            if target.startswith("data:") or target.startswith("#"):
                return match.group(0)
            name = self.take(target, base)
            return match.group(0) if name is None else f'url("{name}")'

        return CSS_URL.sub(on_url, text)

    def rewrite_html(self, html: bytes, base: str) -> bytes:
        """The document, with every reference in it pointed at a local copy.

        Rewritten by substitution rather than by parsing and re-serializing: the
        tree is the thing being measured, and a mirror that rebuilt it would be
        measuring its own parser too.
        """
        if not self.keep_scripts:
            html = SCRIPT.sub(b"", html)
            html = PRELOAD_SCRIPT.sub(b"", html)

        # Stylesheets, in parallel: they are the slow half of a mirror and each
        # one is independent of the others.
        sheets = STYLESHEET.findall(html)
        hrefs: list[bytes] = []
        for tag in sheets:
            found = HREF.search(tag)
            if found:
                hrefs.append(attr_value(found))
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(lambda href: self.take_stylesheet(href.decode(), base), hrefs))

        def replace_attribute(tag: bytes, pattern: re.Pattern[bytes], name: str | None) -> bytes:
            found = pattern.search(tag)
            if found is None or name is None:
                return tag
            return tag[: found.start()] + f'{name}'.encode() + tag[found.end() :]

        def on_stylesheet(match: re.Match[bytes]) -> bytes:
            tag = match.group(0)
            found = HREF.search(tag)
            if found is None:
                return tag
            target = absolutize(attr_value(found).decode(), base)
            name = self.taken.get(target) if target else None
            if name is None:
                # A sheet that did not come down is a sheet the reference must
                # not go to the network for either.
                return b"<!-- stylesheet dropped -->"
            return replace_attribute(tag, HREF, f'href="assets/{name}"')

        html = STYLESHEET.sub(on_stylesheet, html)

        def on_image(match: re.Match[bytes]) -> bytes:
            tag = match.group(0)
            found = SRC.search(tag)
            if found:
                target = attr_value(found).decode()
                if not target.startswith("data:"):
                    name = self.take(target, base)
                    if name is not None:
                        tag = replace_attribute(tag, SRC, f'src="assets/{name}"')
            found = SRCSET.search(tag)
            if found:
                rewritten = self.rewrite_srcset(attr_value(found).decode(), base)
                if rewritten is not None:
                    tag = replace_attribute(tag, SRCSET, f'srcset="{rewritten}"')
            return tag

        html = IMG.sub(on_image, html)

        def on_style_block(match: re.Match[bytes]) -> bytes:
            text = match.group(1).decode("utf-8", "replace")
            # Against the document, and pointed into `assets/`: an inline sheet
            # has no address of its own, so its `url()` resolve against the page
            # and its copies are one directory down from it.
            rewritten = self.rewrite_css(text, base, 0)
            rewritten = CSS_URL.sub(
                lambda m: 'url("assets/{}")'.format(
                    next(g for g in m.groups() if g is not None).strip()
                )
                if not next(g for g in m.groups() if g is not None).strip().startswith(("data:", "assets/", "http", "//"))
                else m.group(0),
                rewritten,
            )
            whole = match.group(0)
            return whole[: match.start(1) - match.start(0)] + rewritten.encode() + whole[match.end(1) - match.start(0) :]

        return STYLE_BLOCK.sub(on_style_block, html)

    def rewrite_srcset(self, value: str, base: str) -> str | None:
        """Every candidate in a `srcset`, or `None` if none of them came down."""
        out = []
        for candidate in value.split(","):
            parts = candidate.strip().split()
            if not parts:
                continue
            name = self.take(parts[0], base)
            if name is None:
                continue
            out.append(" ".join([f"assets/{name}", *parts[1:]]))
        return ", ".join(out) if out else None

    def run(self, url: str) -> int:
        os.makedirs(self.assets, exist_ok=True)
        print(f"fetching {url}")
        html, final = self.fetch(url)
        print(f"  {len(html)} bytes from {final}")
        html = self.rewrite_html(html, final)
        index = os.path.join(self.out, "index.html")
        with open(index, "wb") as file:
            file.write(html)
        kept = sum(1 for name in self.taken.values() if name is not None)
        print(f"wrote {index}: {kept} assets, {self.failures} missed")
        # A mirror with a hole in it is still a mirror — a font that 404s is a
        # font neither browser gets — so this is a report, not a failure.
        return 0


def sniff_extension(body: bytes) -> str:
    """What a file is, from its first bytes, for one that was named without it."""
    for magic, extension in (
        (b"\x89PNG", ".png"),
        (b"\xff\xd8\xff", ".jpg"),
        (b"GIF8", ".gif"),
        (b"RIFF", ".webp"),
        (b"wOF2", ".woff2"),
        (b"wOFF", ".woff"),
        (b"\x00\x01\x00\x00", ".ttf"),
        (b"OTTO", ".otf"),
    ):
        if body.startswith(magic):
            return extension
    if body.lstrip()[:5].lower() in (b"<?xml", b"<svg"):
        return ".svg"
    return ".bin"


def absolutize(url: str, base: str) -> str | None:
    """One address against another, or `None` for one nothing can be fetched at."""
    url = url.strip()
    if not url or url.startswith(("data:", "about:", "javascript:", "#", "blob:")):
        return None
    joined = urllib.parse.urljoin(base, url)
    return joined if urllib.parse.urlsplit(joined).scheme in ("http", "https") else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("url", help="the page to copy")
    parser.add_argument("directory", help="where to put it")
    parser.add_argument(
        "--keep-scripts",
        action="store_true",
        help="leave the scripts in, for a comparison that is about them",
    )
    args = parser.parse_args()
    return Mirror(args.directory, args.keep_scripts).run(args.url)


if __name__ == "__main__":
    sys.exit(main())
