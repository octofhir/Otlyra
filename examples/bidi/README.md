# Driving Otlyra from outside it

Otlyra answers [WebDriver BiDi](https://www.w3.org/TR/webdriver-bidi/) — the W3C
protocol for driving a browser from another program. It is the same protocol
Firefox speaks, the one Puppeteer uses for Firefox by default, and the one
Selenium and WebdriverIO are built on.

```sh
cargo build -p otlyra-app
python3 examples/bidi/drive.py
```

That opens a browser with no window, navigates it to `page.html`, asks what
context it has, takes a screenshot, and asks for one thing the browser cannot do
yet so you can see what a refusal looks like.

## Why this protocol and not Chrome's

The obvious answer is the Chrome DevTools Protocol, because that is what most
tools speak. It turns out not to work:

- CDP is Chromium's private protocol. Firefox dropped it in 129.
- Playwright's CDP client is written against Chromium's internals rather than
  against a specification, so answering to the same method names would not make
  it drive a different engine.
- The path by which Playwright, Puppeteer and Selenium drive a *non-Chromium*
  browser is BiDi.

So BiDi is both the standard and the practical answer. Where the standard has no
command for something — computed styles, fragment geometry, the tracks a grid
was given — those live under an `otlyra:` prefix, which is what the
specification reserves vendor extensions for. Nobody will mistake them for the
standard, and nobody has to give up asking the engine what it actually did.

## What works, and what waits

Working now:

| Command | What it does |
|---|---|
| `session.status` | Whether the browser is free to be driven |
| `session.new` / `session.end` | Open and close a session |
| `session.subscribe` / `unsubscribe` | Which events you want |
| `browsingContext.getTree` | What contexts exist, and where they are |
| `browsingContext.navigate` | Go somewhere, and wait for it |
| `browsingContext.reload` | Again |
| `browsingContext.captureScreenshot` | A PNG of what is on screen |

Waiting on other work, and saying so when asked:

- `script.evaluate` and the rest of `script` need a script engine — milestone
  M12. Stock Playwright leans on it for nearly everything, so Playwright will
  connect to this and then fail. That is a real limitation and not a bug to
  report.
- `browsingContext.locateNodes` (find by CSS selector) and `input.performActions`
  (click, type, scroll) are next, and neither needs a script engine.
- `log` and `network` events: the browser already keeps both — its console and
  its request list are what the inspector panel reads — so this is a matter of
  broadcasting them, not of collecting them.

## The client

`client.py` is a complete BiDi client in the Python standard library and nothing
else — no `pip install`, about ninety lines of which most is WebSocket framing.
It is here to be read: the protocol is JSON in, JSON out, and seeing that is
worth more than a dependency.

For real work, use a real client. Puppeteer and Selenium both speak BiDi and
handle reconnection, timeouts and events properly.

## The address

The browser binds the **loopback only**, and prints the address it got on
stdout:

```sh
$ ./target/debug/otlyra --bidi 0
ws://127.0.0.1:52999/session
```

Pass `--bidi` with no number for the conventional 9222, or `0` to have the system
pick a free port — which is what `client.py` does, because two drivers guessing
the same port is how they fight each other.

Loopback only is deliberate. This endpoint navigates, clicks and reads what is on
screen; a port that answered the network would hand all of that to whoever asked.
Reaching it from another machine is a tunnel you set up on purpose.
