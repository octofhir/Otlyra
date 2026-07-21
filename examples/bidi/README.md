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
(Nothing else is waiting on anything; the list above is what exists.)

## Asking the engine, not a script

The standard's way to ask what an element's computed style is would be
`script.evaluate` running `getComputedStyle` in the page. That needs a script
engine, and it answers with what a script can see rather than with what the
engine did. So there is an `otlyra:` module — the prefix the specification
reserves for exactly this — and it answers from the layout that actually ran:

```python
grid = browser.find(".grid")[0]
facts = browser.explain(grid)
```

```
display grid, columns 200px 1fr 1fr
drawn at 1000×195 at (0, 81)
column lines a stylesheet can name: [1, 2, 3, 4]
```

One command rather than four, because the question a person actually has is
*why is this element like this* and the answer is made of all of it at once:
what the cascade computed, what the layout made of it, and where its tracks
fell. Four round trips would be four chances for the page to move between them.

| Command | What it answers |
|---|---|
| `otlyra:explain` | Computed style, box model and tracks, for one node |
| `otlyra:highlight` | Pick a node out, so a screenshot shows which one is meant |
| `otlyra:frameTimings` | How long each stage of the last frame took |

`otlyra:highlight` draws the overlay a person would see — the four shades of the
box model, and a grid's dashed track lines with their numbers. An agent that has
to show somebody *which* element it means has the same problem a person does,
and the browser had already solved it once.

Values are spelled the way a stylesheet spells them. `200px 1fr 1fr`, not the
engine's own `fixed(px(200.0)) fraction(1.0)`: a computed value you cannot put
back into a stylesheet is most of the way to no answer at all.

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

## For an agent: the Model Context Protocol

A program driving a browser has a client library. An agent has a list of tools
and a sentence about each, which is a different shape — and is what MCP is for.

```sh
claude mcp add otlyra -- /path/to/otlyra --mcp
```

```
browser_navigate    Open a page and wait for it to load.
browser_screenshot  A picture of the page as it is now.
browser_find        Find elements by CSS selector…
browser_explain     Why an element looks the way it does…
browser_highlight   Draw the inspector's overlay over an element…
browser_act         Click, type or scroll…
browser_timings     How long each stage of the last frame took…
```

This is not a second protocol. Every tool is one BiDi command, dispatched
through the same session against the same browser: nothing in the MCP server
knows anything about a page that the protocol does not. A second implementation
of *what is on this page* is the one thing the whole design exists to avoid.

Two things are shaped for an agent rather than for a program:

- A screenshot comes back as an **image**, not as base64 in a string. An agent
  that can see the page can settle questions no amount of JSON would.
- A tool that could not do something answers with `isError` and a sentence,
  rather than with a transport error. The call reached the browser and the
  browser answered; an agent can read what it said and try something else.

`--mcp` speaks JSON-RPC on stdin and stdout, which is why every diagnostic in
this program goes to stderr. One stray `println!` would be a parse error in
somebody's agent.

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
