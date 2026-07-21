"""Drive Otlyra the way an agent would: open a page, look at it, say what it is.

    cargo build -p otlyra-app
    python3 examples/bidi/drive.py examples/bidi/page.html

What this shows is the shape of the thing rather than the whole of it. The
browser answers WebDriver BiDi — the W3C protocol Firefox, Chrome, Puppeteer and
Selenium all speak — so a client written against the standard drives this engine
without knowing anything about it.

What it cannot do yet is anything that needs a script in the page:
`script.evaluate` waits on a script engine. The browser says so plainly rather
than failing in some other way, and the run below asks for it once so you can
see what that looks like.
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from client import BiDiError, Otlyra  # noqa: E402


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    page = sys.argv[1] if len(sys.argv) > 1 else os.path.join(here, "page.html")
    if not page.startswith(("http://", "https://", "file://")):
        page = "file://" + os.path.abspath(page)

    with Otlyra(binary=binary()) as browser:
        print(f"→ navigate {page}")
        print(f"← {browser.navigate(page)}\n")

        tree = browser.send("browsingContext.getTree")
        context = tree["contexts"][0]
        print(f"→ getTree\n← one context, {context['context']}, at {context['url']}\n")

        # The selector is matched by the engine's own matcher — the one the
        # cascade styles with — so this reports the same elements a stylesheet
        # would have hit.
        cards = browser.find(".card")
        print(f"→ locateNodes .card\n← {len(cards)} nodes")
        for card in cards[:2]:
            value = card["value"]
            print(f"    <{value['localName']} {value.get('attributes', {})}>")
        print()

        first = browser.find("#first")[0]
        print(f"→ locateNodes #first\n← sharedId {first['sharedId']}\n")

        # No coordinates anywhere in this call. The browser knows where it drew
        # the element; the driver does not have to, and so cannot be wrong.
        browser.click(first)
        print("→ click #first\n← the browser aimed at the centre of its own box\n")

        browser.scroll(400, 300, 200)
        print("→ scroll\n← the page took the wheel\n")

        shot = browser.screenshot("/tmp/otlyra-bidi.png")
        size = os.path.getsize(shot)
        print(f"→ captureScreenshot\n← {size} bytes of PNG at {shot}\n")

        # Events arrive unasked once they are subscribed to. The browser already
        # keeps its own log and its own list of requests — these are the same
        # facts, sent rather than shown.
        browser.subscribe("log", "network")
        print("→ subscribe log, network")
        browser.navigate(page)
        for message in browser.collect():
            params = message["params"]
            if message["method"] == "network.responseCompleted":
                response = params["response"]
                took = params.get("otlyra:took")
                took = f"{took:.1f} ms" if took else "?"
                print(
                    f"←   {message['method']}: {response['status']} "
                    f"{response['bytesReceived']} bytes in {took}"
                )
            elif message["method"] == "network.beforeRequestSent":
                print(f"←   {message['method']}: {params['request']['url']}")
            else:
                print(f"←   {message['method']}: {params.get('text', '')}")
        print()

        # The honest gap, asked for on purpose. A protocol that answered this
        # with silence, or with an empty result, would be worse than one that
        # says which milestone it is waiting on.
        print("→ script.evaluate 1 + 1")
        try:
            browser.send("script.evaluate", expression="1 + 1")
        except BiDiError as error:
            print(f"← {error.code}: {error}\n")


def binary():
    """Whichever build is newer, or `$OTLYRA` when it is set.

    Newer rather than release-first: a stale release binary from last month is
    exactly the one that will not have the flag you just added, and the failure
    reads as a protocol bug rather than as a build you forgot.
    """
    if os.environ.get("OTLYRA"):
        return os.environ["OTLYRA"]
    builds = [
        path
        for path in ("./target/release/otlyra", "./target/debug/otlyra")
        if os.path.exists(path)
    ]
    if not builds:
        raise SystemExit("build it first: cargo build -p otlyra-app")
    return max(builds, key=os.path.getmtime)


if __name__ == "__main__":
    main()
