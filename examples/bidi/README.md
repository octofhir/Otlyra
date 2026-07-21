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
| `browsingContext.locateNodes` | Find nodes by CSS selector |
| `input.performActions` | Move, click, scroll, type |
| `log.entryAdded` | What the browser said, as it says it |
| `network.beforeRequestSent` | Every request, as it goes out |
| `network.responseCompleted` | What came back, with sizes and timings |

Waiting on other work, and saying so when asked:

- `script.evaluate` and the rest of `script` need a script engine — milestone
  M12. Stock Playwright leans on it for nearly everything, so Playwright will
  connect to this and then fail. That is a real limitation and not a bug to
  report.
- The `otlyra:` module — computed styles, fragment geometry, the tracks a grid
  was given. The engine knows all of it; the standard has no command for asking.

## Naming an element rather than a point

`input.performActions` takes an `origin`, and the useful one is an element:

```python
first = browser.find("#first")[0]
browser.click(first)          # no coordinates anywhere
```

The browser resolves that against where it *actually drew* the element — the
same rectangle a real click is tested against. A driver that computed the point
itself would be keeping a second opinion about the layout, and the two would
disagree the first time anything moved.

The selector is matched by the engine's own matcher, the one the cascade styles
with, so `.card` finds exactly the elements a stylesheet would have hit.

## Events

Subscribe, and the browser sends things without being asked:

```python
browser.subscribe("log", "network")
browser.navigate(page)
for message in browser.collect():
    print(message["method"], message["params"])
```

```
network.beforeRequestSent  file:///…/page.html
network.responseCompleted  200, 887 bytes in 2.1 ms
```

These are not a second set of measurements taken for the protocol's sake. The
browser already keeps its own log and its own list of requests — they are what
the inspector's Console and Network panes read — so an event is the same fact,
sent rather than shown. Naming a module subscribes to everything in it, which is
what the specification says.

Two timings come back under `otlyra:` keys, because they answer different
questions: `otlyra:took` is how slow the transport was and `otlyra:waited` is how
long the request sat waiting for a fetch thread. One number would hide which of
the two a slow page is suffering from.

A client that subscribed to nothing is sent nothing. A request is reported once
when it goes out and once when it ends — including when it ends badly, with the
reason in `statusText`, so a client waiting on one event for both outcomes
cannot hang.

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
