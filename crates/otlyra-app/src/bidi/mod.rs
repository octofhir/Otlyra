//! WebDriver BiDi: the browser answering questions from outside itself.
//!
//! # Why this protocol and not Chrome's
//!
//! The point of a remote protocol here is that people drive this browser with
//! the tools they already have. That argues for the Chrome DevTools Protocol —
//! until you look at what actually happens. CDP is Chromium's private protocol:
//! Firefox dropped it in 129, and Playwright's CDP client is written against
//! Chromium's internals rather than against a specification, so answering to the
//! same method names would not make it drive us. The path by which Playwright,
//! Puppeteer and Selenium drive a *non-Chromium* engine is WebDriver BiDi, which
//! is a W3C standard with a written specification and a conformance suite.
//!
//! So: the standard where a standard exists. Where one does not — computed
//! styles, fragment geometry, the tracks a grid was given — BiDi's own answer is
//! `script.evaluate` with a page script, which needs a script engine and, worse,
//! returns what a script can see rather than what the engine did. Those live in
//! an `otlyra:` module instead, named so that nobody mistakes them for the
//! standard, which is what the specification reserves vendor prefixes for.
//!
//! # What it cannot do yet
//!
//! `script.evaluate` needs M12's script engine. Stock Playwright leans on it for
//! almost everything, so it will connect and then fail; that is stated rather
//! than worked around. Everything that does not need a page script — navigating,
//! finding nodes by selector, input, screenshots, the log, the network — does
//! not wait for it, and is enough for an agent to do real work.
//!
//! # Shape
//!
//! One command in, one result out, over a WebSocket, in JSON. The dispatch is a
//! plain match on the method name against a [`Browser`], because that is what a
//! protocol *is* here: a second way to ask the questions the inspector already
//! asks, answered from the same place. A second source of truth for what the
//! page is would be the one bug this whole design exists to avoid.

mod server;

pub use server::{Server, listen};

use serde_json::{Value, json};

use crate::browser::Browser;

/// What the protocol calls this implementation.
pub const NAME: &str = "otlyra";

/// The vendor prefix for what the standard has no command for.
pub const VENDOR: &str = "otlyra";

/// One message from a client.
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// The client's number for it, echoed back on the answer.
    pub id: u64,
    /// The module and method, as `module.method`.
    pub method: String,
    /// Whatever the method takes.
    pub params: Value,
}

impl Command {
    /// Read one command out of a JSON message.
    ///
    /// The specification requires `id` and `method`; a message without them is
    /// not a command and cannot be answered with an error carrying its id,
    /// because it has none.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let value: Value = serde_json::from_str(text)
            .map_err(|error| Error::invalid(format!("not JSON: {error}")))?;
        let id = value
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::invalid("a command needs an id"))?;
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid("a command needs a method"))?
            .to_owned();
        Ok(Self {
            id,
            method,
            params: value.get("params").cloned().unwrap_or_else(|| json!({})),
        })
    }

    /// One parameter, as a string.
    fn string(&self, name: &str) -> Result<&str, Error> {
        self.params
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid(format!("{} needs a string {name}", self.method)))
    }
}

/// Why a command could not be answered.
///
/// The `error` field is one of the specification's names, because a client
/// matches on it; the message is ours and is for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The specification's error code.
    pub code: &'static str,
    /// What went wrong, in a sentence.
    pub message: String,
}

impl Error {
    /// The client sent something the specification does not allow.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "invalid argument",
            message: message.into(),
        }
    }

    /// A method this implementation does not have.
    pub fn unknown_command(method: &str) -> Self {
        Self {
            code: "unknown command",
            message: format!("{method} is not implemented"),
        }
    }

    /// A method that is in the specification and waits on something we lack.
    ///
    /// Told apart from *unknown* on purpose: a client that meets this one is
    /// looking at a gap with a date on it, not at a typo.
    pub fn not_yet(method: &str, waiting_on: &str) -> Self {
        Self {
            code: "unsupported operation",
            message: format!("{method} needs {waiting_on}"),
        }
    }

    /// A node handle that names nothing, or a node that is not there.
    pub fn no_such_node(message: &str) -> Self {
        Self {
            code: "no such node",
            message: message.to_owned(),
        }
    }

    /// A context id that names nothing.
    pub fn no_such_context(context: &str) -> Self {
        Self {
            code: "no such frame",
            message: format!("no browsing context {context}"),
        }
    }

    /// The message a client receives.
    pub fn to_message(&self, id: Option<u64>) -> Value {
        json!({
            "type": "error",
            "id": id,
            "error": self.code,
            "message": self.message,
        })
    }
}

/// The browser, and the session a client has opened on it.
///
/// Holds no page state of its own. Every answer is read out of the browser at
/// the moment it is asked for, so what a client sees and what the window shows
/// cannot drift.
pub struct Session {
    /// The browser being driven.
    pub browser: Browser,
    /// What the client has subscribed to.
    events: Vec<String>,
    /// Whether `session.new` has been answered.
    open: bool,
    /// How large a screenshot is taken at.
    viewport: (u32, u32),
    /// How far through the journal this client has been told.
    log_cursor: u64,
    /// Which requests it has been told about, and which of those have finished.
    ///
    /// By request number rather than by a count: a request finishes long after
    /// it was made, and out of order with its neighbours, so *how many* is not a
    /// place in either stream.
    announced: std::collections::HashSet<u64>,
    completed: std::collections::HashSet<u64>,
}

/// The context id the one tab is known by.
///
/// One context because there is one tab being driven: tab handling over the
/// protocol is a later stage, and inventing ids for tabs a client cannot yet
/// address would be inventing a vocabulary nobody speaks.
pub const CONTEXT: &str = "otlyra-context-1";

impl Session {
    /// A session over `browser`, drawing at `viewport` logical pixels.
    ///
    /// The browser's own interface is hidden. What the protocol calls a
    /// screenshot is a picture of the *browsing context* — the page — and a
    /// toolbar in it would be furniture a driver never asked for and would have
    /// to subtract from every coordinate it computed.
    pub fn new(mut browser: Browser, viewport: (u32, u32)) -> Self {
        browser.hide_interface();
        Self {
            browser,
            events: Vec::new(),
            open: false,
            viewport,
            // Start where the journal is *now*: a client that connects to a
            // browser which has been running for an hour wants what happens
            // next, not an hour of backlog it never asked for.
            log_cursor: crate::observability::journal().cursor(),
            announced: std::collections::HashSet::new(),
            completed: std::collections::HashSet::new(),
        }
    }

    /// Whether the client has subscribed to `event`.
    pub fn subscribed(&self, event: &str) -> bool {
        self.events.iter().any(|name| {
            name == event
                || event
                    .split_once('.')
                    .is_some_and(|(module, _)| name == module)
        })
    }

    /// Answer one command.
    ///
    /// The result is the `result` object of a success message; the caller wraps
    /// it. Errors come back as [`Error`] and are wrapped the same way, so there
    /// is one place that knows the message envelope.
    pub fn dispatch(&mut self, command: &Command) -> Result<Value, Error> {
        match command.method.as_str() {
            // --- session ---------------------------------------------------
            "session.status" => Ok(json!({
                // Always ready: there is no state a client has to wait for, and
                // saying otherwise would make every client poll for nothing.
                "ready": !self.open,
                "message": if self.open {
                    "a session is already open"
                } else {
                    "ready"
                },
            })),
            "session.new" => {
                self.open = true;
                Ok(json!({
                    "sessionId": "otlyra-session-1",
                    "capabilities": self.capabilities(),
                }))
            }
            "session.end" => {
                self.open = false;
                self.events.clear();
                Ok(json!({}))
            }
            "session.subscribe" => {
                let events = command
                    .params
                    .get("events")
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::invalid("session.subscribe needs events"))?;
                for event in events.iter().filter_map(Value::as_str) {
                    if !self.events.iter().any(|known| known == event) {
                        self.events.push(event.to_owned());
                    }
                }
                Ok(json!({}))
            }
            "session.unsubscribe" => {
                let events = command
                    .params
                    .get("events")
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::invalid("session.unsubscribe needs events"))?;
                let dropped: Vec<&str> = events.iter().filter_map(Value::as_str).collect();
                self.events
                    .retain(|event| !dropped.contains(&event.as_str()));
                Ok(json!({}))
            }

            // --- browsingContext -------------------------------------------
            "browsingContext.getTree" => Ok(json!({
                "contexts": [self.context()],
            })),
            "browsingContext.navigate" => {
                let url = command.string("url")?.to_owned();
                self.check_context(command)?;
                self.browser.navigate(&url);
                self.browser.wait_for_load(LOAD_TIMEOUT);
                self.browser.prepare_frame(self.viewport(), LOAD_TIMEOUT);
                Ok(json!({
                    "navigation": Value::Null,
                    "url": self.browser.url(),
                }))
            }
            "browsingContext.reload" => {
                self.check_context(command)?;
                self.browser.reload();
                self.browser.wait_for_load(LOAD_TIMEOUT);
                self.browser.prepare_frame(self.viewport(), LOAD_TIMEOUT);
                Ok(json!({
                    "navigation": Value::Null,
                    "url": self.browser.url(),
                }))
            }
            "browsingContext.captureScreenshot" => {
                self.check_context(command)?;
                let png = self
                    .browser
                    .screenshot(self.viewport())
                    .map_err(|error| Error {
                        code: "unable to capture screen",
                        message: error,
                    })?;
                Ok(json!({ "data": base64(&png) }))
            }

            "browsingContext.locateNodes" => {
                self.check_context(command)?;
                self.locate(command)
            }

            // --- input -----------------------------------------------------
            "input.performActions" => {
                self.check_context(command)?;
                self.perform(command)?;
                // A frame after acting, so the next question is asked of what
                // the action produced rather than of what was there before it.
                self.browser.prepare_frame(self.viewport(), LOAD_TIMEOUT);
                Ok(json!({}))
            }
            "input.releaseActions" => {
                // Nothing is held between commands: a press that was not
                // released is released by the command that pressed it, because
                // this implementation performs an action list to completion.
                Ok(json!({}))
            }

            // --- the vendor module -----------------------------------------
            //
            // What the standard has no command for. BiDi's own answer to "what
            // is this element's computed style" is `script.evaluate` running
            // `getComputedStyle` in the page — which needs a script engine and,
            // worse, returns what a script can see rather than what the engine
            // did. These come from the layout that actually ran.
            "otlyra:explain" => {
                self.check_context(command)?;
                self.explain(command)
            }
            "otlyra:highlight" => {
                self.check_context(command)?;
                self.highlight(command)
            }
            "otlyra:frameTimings" => Ok(json!({
                "timings": crate::observability::journal()
                    .latest()
                    .into_iter()
                    .map(|timing| json!({
                        "stage": timing.span,
                        "took": timing.took.as_secs_f64() * 1000.0,
                    }))
                    .collect::<Vec<_>>(),
            })),

            // --- what waits on a script engine -----------------------------
            method if method.starts_with("script.") => {
                Err(Error::not_yet(method, "a script engine, which is M12"))
            }

            other => Err(Error::unknown_command(other)),
        }
    }

    /// Everything that has happened since this was last asked, as events.
    ///
    /// Pulled rather than pushed. The browser is driven from one thread and the
    /// things worth reporting — what it said, what it fetched — are already kept
    /// where they can be read; a callback into the socket from wherever they are
    /// produced would put the protocol inside the fetcher and inside the log.
    /// This keeps the protocol at the edge, where it belongs.
    pub fn drain_events(&mut self) -> Vec<Value> {
        let mut events = Vec::new();
        if self.subscribed("log.entryAdded") {
            let (records, cursor) = crate::observability::journal().since(self.log_cursor);
            self.log_cursor = cursor;
            events.extend(records.into_iter().map(log_entry));
        }
        if self.subscribed("network.beforeRequestSent")
            || self.subscribed("network.responseCompleted")
        {
            events.extend(self.network_events());
        }
        events
    }

    /// What the fetcher has done that this client has not been told about.
    fn network_events(&mut self) -> Vec<Value> {
        use crate::fetcher::Status;

        let exchanges: Vec<crate::fetcher::Exchange> = self.browser.exchanges().to_vec();
        let mut events = Vec::new();
        for exchange in exchanges {
            if self.announced.insert(exchange.id) && self.subscribed("network.beforeRequestSent") {
                events.push(request_event(&exchange));
            }
            let finished = !matches!(exchange.status, Status::Pending);
            if finished
                && self.completed.insert(exchange.id)
                && self.subscribed("network.responseCompleted")
            {
                events.push(response_event(&exchange));
            }
        }
        events
    }

    /// Everything the engine knows about one node, in one answer.
    ///
    /// One command rather than four, because the question a person actually has
    /// is *why is this element like this* and the answer is made of all of it at
    /// once: what the cascade computed, what the layout made of it, and — when
    /// it lays its children into tracks — where those tracks fell. Four round
    /// trips would be four chances for the page to move between them.
    fn explain(&mut self, command: &Command) -> Result<Value, Error> {
        let node = self.node_named(command)?;
        let facts = self
            .browser
            .box_facts(node)
            .ok_or_else(|| Error::no_such_node("that node was not drawn"))?;

        let page = self
            .browser
            .active_page()
            .ok_or_else(|| Error::no_such_node("nothing is loaded in this context"))?;
        let style = page
            .boxes()
            .box_for(node)
            .and_then(|id| page.boxes().get(id))
            .map(|box_node| crate::inspector::describe(&box_node.style))
            .unwrap_or_default();

        let content = facts.edges.content_of(facts.border);
        let edges = |sides: (f64, f64, f64, f64)| json!({ "left": sides.0, "top": sides.1, "right": sides.2, "bottom": sides.3 });
        let rect = |rect: crate::ui::Rect| json!({ "x": rect.x, "y": rect.y, "width": rect.width, "height": rect.height });

        Ok(json!({
            "node": node_value(page.document(), node),
            "computed": style
                .into_iter()
                .map(|(name, value)| (name.to_owned(), Value::String(value)))
                .collect::<serde_json::Map<String, Value>>(),
            "box": {
                // The border box is where the last frame *drew* it, which is the
                // same rectangle a click is tested against.
                "border": rect(facts.border),
                "content": rect(content),
                "margin": edges(facts.edges.margin),
                "borderWidth": edges(facts.edges.border),
                "padding": edges(facts.edges.padding),
                "containingWidth": facts.containing,
            },
            "tracks": facts.tracks.as_ref().map(|tracks| json!({
                "numbered": tracks.numbered,
                "columns": lines_json(&tracks.columns),
                "rows": lines_json(&tracks.rows),
            })),
        }))
    }

    /// Choose a node, so the next screenshot shows it picked out.
    ///
    /// The overlay a person sees, asked for by a program. An agent that has to
    /// show somebody *which* element it means has the same problem a person
    /// does, and the browser already solved it once.
    fn highlight(&mut self, command: &Command) -> Result<Value, Error> {
        // A command with no node clears it, which is how a driver puts the page
        // back the way it found it.
        let node = match command.params.get("sharedId") {
            None | Some(Value::Null) => None,
            Some(_) => Some(self.node_named(command)?),
        };
        self.browser.inspector_mut().selected = node;
        self.browser.prepare_frame(self.viewport(), LOAD_TIMEOUT);
        Ok(json!({ "highlighted": node.map(shared_id) }))
    }

    /// The node a command names by handle.
    fn node_named(&self, command: &Command) -> Result<otlyra_dom::NodeId, Error> {
        let shared = command.string("sharedId")?;
        let node = node_of(shared)
            .ok_or_else(|| Error::no_such_node(&format!("{shared} names no node")))?;
        let page = self
            .browser
            .active_page()
            .ok_or_else(|| Error::no_such_node("nothing is loaded in this context"))?;
        // A handle from a document that has since been replaced names a node
        // that is not there any more, and saying so beats answering about
        // whatever else took its number.
        if page.document().get(node).is_none() {
            return Err(Error::no_such_node(&format!(
                "{shared} is not in the document that is loaded"
            )));
        }
        Ok(node)
    }

    /// Find the nodes a locator names.
    ///
    /// The selector engine is the page's own — the one the cascade matches with
    /// — so a client that asks for `.card` is told about the same elements a
    /// stylesheet would have styled. A second matcher would be a second answer
    /// to *what does this selector mean*.
    fn locate(&mut self, command: &Command) -> Result<Value, Error> {
        let locator = command
            .params
            .get("locator")
            .ok_or_else(|| Error::invalid("locateNodes needs a locator"))?;
        let kind = locator.get("type").and_then(Value::as_str).unwrap_or("css");
        if kind != "css" {
            // `innerText` and `accessibility` are in the specification and are
            // not here yet. Saying which is missing beats a silent empty list,
            // which reads as *nothing matched*.
            return Err(Error::not_yet(
                &format!("locateNodes with a {kind} locator"),
                "a locator this implementation does not have yet",
            ));
        }
        let selector = locator
            .get("value")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid("a css locator needs a value"))?;

        let page = self
            .browser
            .active_page()
            .ok_or_else(|| Error::no_such_node("nothing is loaded in this context"))?;
        let document = page.document();
        let matched = otlyra_css::stylo_dom::select(document, selector)
            .map_err(|error| Error::invalid(format!("{selector:?} is not a selector: {error}")))?;

        let nodes: Vec<Value> = matched
            .into_iter()
            .map(|node| node_value(document, node))
            .collect();
        Ok(json!({ "nodes": nodes }))
    }

    /// Perform one list of input actions, in order.
    ///
    /// Delivered as the platform events a person's mouse and keyboard produce,
    /// through the same path a window's events take. A driver that had its own
    /// way in would be able to reach states a person cannot, which is the one
    /// thing an automation protocol must not do.
    fn perform(&mut self, command: &Command) -> Result<(), Error> {
        let sources = command
            .params
            .get("actions")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::invalid("performActions needs actions"))?
            .clone();

        for source in sources {
            let kind = source.get("type").and_then(Value::as_str).unwrap_or("none");
            let actions = source
                .get("actions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for action in actions {
                self.act(kind, &action)?;
            }
        }
        Ok(())
    }

    /// One action from one source.
    fn act(&mut self, source: &str, action: &Value) -> Result<(), Error> {
        use otlyra_platform::{Painter, PlatformEvent};

        let kind = action
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid("an action needs a type"))?;

        match (source, kind) {
            (_, "pause") => Ok(()),
            ("pointer", "pointerMove") => {
                let (x, y) = self.point_of(action)?;
                self.browser.on_event(PlatformEvent::PointerMoved { x, y });
                Ok(())
            }
            ("pointer", "pointerDown") => {
                // A driver's press is always a fresh single click: the protocol
                // has no click count, and a double-click arrives as two presses
                // the *page* may interpret, not something to synthesise here.
                self.browser
                    .on_event(PlatformEvent::PointerPressed { clicks: 1 });
                Ok(())
            }
            ("pointer", "pointerUp") => {
                self.browser.on_event(PlatformEvent::PointerReleased);
                Ok(())
            }
            ("wheel", "scroll") => {
                let (x, y) = self.point_of(action)?;
                self.browser.on_event(PlatformEvent::PointerMoved { x, y });
                let delta =
                    |name: &str| action.get(name).and_then(Value::as_f64).unwrap_or_default();
                self.browser.on_event(PlatformEvent::Scroll {
                    x: delta("deltaX"),
                    y: delta("deltaY"),
                    source: otlyra_platform::ScrollSource::Wheel,
                });
                Ok(())
            }
            ("key", "keyDown") => {
                let value = action
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::invalid("keyDown needs a value"))?;
                for event in key_events(value) {
                    self.browser.on_event(event);
                }
                Ok(())
            }
            // A key coming back up types nothing: what a key *did* happened on
            // the way down, and delivering it twice would type everything twice.
            ("key", "keyUp") => Ok(()),
            (source, kind) => Err(Error::not_yet(
                &format!("a {kind} action from a {source} source"),
                "an action this implementation does not have yet",
            )),
        }
    }

    /// Where an action points, in the page's own coordinates.
    ///
    /// An element origin is resolved against where the engine actually drew the
    /// element, which is the same rectangle a click is tested against. That is
    /// the point of naming an element rather than a coordinate: the driver does
    /// not have to know the layout, and cannot disagree with it.
    fn point_of(&self, action: &Value) -> Result<(f64, f64), Error> {
        let x = action.get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let y = action.get("y").and_then(Value::as_f64).unwrap_or(0.0);

        let origin = action.get("origin");
        let shared = origin
            .and_then(|origin| origin.get("element"))
            .and_then(|element| element.get("sharedId"))
            .and_then(Value::as_str);
        let Some(shared) = shared else {
            return Ok((x, y));
        };

        let node = node_of(shared)
            .ok_or_else(|| Error::no_such_node(&format!("{shared} names no node")))?;
        let page = self
            .browser
            .active_page()
            .ok_or_else(|| Error::no_such_node("nothing is loaded in this context"))?;
        let rect = page
            .boxes()
            .box_for(node)
            .and_then(|id| page.rect_of(id))
            .ok_or_else(|| Error::no_such_node(&format!("{shared} was not drawn")))?;
        // The centre, as the specification says, and then whatever offset the
        // action asked for on top of it.
        Ok((
            f64::from(rect.x + rect.width / 2.0) + x,
            f64::from(rect.y + rect.height / 2.0) + y,
        ))
    }

    /// What this implementation says it can do.
    ///
    /// Honest rather than flattering: a client that is told a capability is
    /// present and finds it missing has been lied to in the one place a protocol
    /// exists to prevent.
    fn capabilities(&self) -> Value {
        json!({
            "browserName": NAME,
            "browserVersion": crate::about::VERSION,
            "platformName": std::env::consts::OS,
            "acceptInsecureCerts": false,
            "userAgent": format!("{NAME}/{}", crate::about::VERSION),
        })
    }

    /// The one browsing context, as the protocol describes one.
    fn context(&self) -> Value {
        json!({
            "context": CONTEXT,
            "url": self.browser.url(),
            "children": [],
            "parent": Value::Null,
            "userContext": "default",
        })
    }

    /// Refuse a command aimed at a context that is not ours.
    fn check_context(&self, command: &Command) -> Result<(), Error> {
        match command.params.get("context").and_then(Value::as_str) {
            None | Some(CONTEXT) => Ok(()),
            Some(other) => Err(Error::no_such_context(other)),
        }
    }

    fn viewport(&self) -> otlyra_platform::Viewport {
        otlyra_platform::Viewport::new(self.viewport.0, self.viewport.1, 1.0)
    }
}

/// Track lines, as a client reads them.
///
/// The number is what a stylesheet names the line by, and it is absent where
/// there is no name — the far side of a gutter is the same line seen from the
/// other end, and a container edge no track reaches is not a line at all.
fn lines_json(lines: &[crate::inspector::Line]) -> Vec<Value> {
    lines
        .iter()
        .map(|line| json!({ "at": line.at, "number": line.number }))
        .collect()
}

/// The envelope every event arrives in.
fn event(method: &str, params: Value) -> Value {
    json!({ "type": "event", "method": method, "params": params })
}

/// Milliseconds since the epoch, which is what the protocol stamps with.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or_default()
}

/// One line the browser said, as a `log.entryAdded`.
fn log_entry(record: crate::observability::Record) -> Value {
    event(
        "log.entryAdded",
        json!({
            "level": match record.level {
                tracing::Level::ERROR => "error",
                tracing::Level::WARN => "warn",
                tracing::Level::INFO => "info",
                _ => "debug",
            },
            // The specification names `console` and `javascript` for the entries
            // it knows about. This is neither: it is the browser talking about
            // itself, and calling it `javascript` would be a lie a client could
            // act on.
            "type": VENDOR,
            "source": { "context": CONTEXT },
            "text": record.message,
            "timestamp": now(),
            "otlyra:target": record.target,
        }),
    )
}

/// A request the browser made, as a `network.beforeRequestSent`.
fn request_event(exchange: &crate::fetcher::Exchange) -> Value {
    event(
        "network.beforeRequestSent",
        json!({
            "context": CONTEXT,
            "isRedirect": false,
            "navigation": Value::Null,
            "redirectCount": 0,
            "timestamp": now(),
            "request": {
                "request": exchange.id.to_string(),
                "url": exchange.url,
                // Every fetch this browser makes is a GET. When it makes another
                // kind this will say so, rather than saying so early.
                "method": "GET",
                "headers": [],
                "cookies": [],
            },
            "otlyra:kind": format!("{:?}", exchange.kind).to_lowercase(),
        }),
    )
}

/// What became of it, as a `network.responseCompleted`.
///
/// A failure is reported here too, with its reason, rather than through
/// `fetchError`: the browser knows the request ended and why, and a client
/// waiting on one event for both outcomes is a client that cannot hang.
fn response_event(exchange: &crate::fetcher::Exchange) -> Value {
    use crate::fetcher::Status;
    let (status, text, bytes) = match &exchange.status {
        Status::Ok(bytes) => (200, String::new(), *bytes),
        Status::Failed(error) => (0, error.clone(), 0),
        Status::Pending => (0, "still out".to_owned(), 0),
    };
    event(
        "network.responseCompleted",
        json!({
            "context": CONTEXT,
            "isRedirect": false,
            "navigation": Value::Null,
            "redirectCount": 0,
            "timestamp": now(),
            "request": {
                "request": exchange.id.to_string(),
                "url": exchange.url,
                "method": "GET",
                "headers": [],
                "cookies": [],
            },
            "response": {
                "url": exchange.url,
                "status": status,
                "statusText": text,
                "bytesReceived": bytes,
                "fromCache": false,
                "headers": [],
                "mimeType": Value::Null,
                "protocol": Value::Null,
                "content": { "size": bytes },
            },
            // Two numbers, because they answer different questions: how slow the
            // transport was, and how long the request waited for a thread.
            "otlyra:took": exchange.took.map(|took| took.as_secs_f64() * 1000.0),
            "otlyra:waited": exchange.waited.map(|waited| waited.as_secs_f64() * 1000.0),
        }),
    )
}

/// One node, as the protocol describes one.
///
/// A `sharedId` a client can hand back, and enough of the node to recognise it
/// without a second round trip. Not the subtree: a client that wants children
/// asks for them, and a node deep in a page would otherwise carry the rest of
/// the document with it.
fn node_value(document: &otlyra_dom::Document, node: otlyra_dom::NodeId) -> Value {
    let Some(data) = document.get(node) else {
        return json!({ "type": "node", "sharedId": shared_id(node) });
    };
    let mut value = json!({
        "childNodeCount": document.children(node).count(),
    });
    match &data.data {
        otlyra_dom::NodeData::Element(element) => {
            let attributes: serde_json::Map<String, Value> = element
                .attrs
                .iter()
                .map(|attr| {
                    (
                        attr.name.local.as_ref().to_owned(),
                        Value::String(attr.value.to_string()),
                    )
                })
                .collect();
            value["nodeType"] = json!(1);
            value["localName"] = json!(element.name.local.as_ref());
            value["namespaceURI"] = json!(element.name.ns.as_ref());
            value["attributes"] = Value::Object(attributes);
        }
        otlyra_dom::NodeData::Text(text) => {
            value["nodeType"] = json!(3);
            value["nodeValue"] = json!(text.to_string());
        }
        otlyra_dom::NodeData::Comment(text) => {
            value["nodeType"] = json!(8);
            value["nodeValue"] = json!(text.to_string());
        }
        otlyra_dom::NodeData::Doctype { name, .. } => {
            value["nodeType"] = json!(10);
            value["nodeValue"] = json!(name.to_string());
        }
        otlyra_dom::NodeData::Document => value["nodeType"] = json!(9),
    }
    json!({
        "type": "node",
        "sharedId": shared_id(node),
        "value": value,
    })
}

/// The handle a client holds a node by.
///
/// The engine's own node number, written out. A table of handles beside the
/// document would be a second naming of the same nodes, and would have to be
/// swept when one went away.
fn shared_id(node: otlyra_dom::NodeId) -> String {
    otlyra_dom::node_id_to_u64(node).to_string()
}

/// The node a handle names, if it is one of ours.
fn node_of(shared: &str) -> Option<otlyra_dom::NodeId> {
    shared.parse::<u64>().ok().map(otlyra_dom::node_id_from_u64)
}

/// The platform events one key value produces.
///
/// A named key is a key press; anything else is a character, which is a press
/// *and* the text it types — the same two events a window delivers, because the
/// browser above cannot tell where they came from and must not be able to.
fn key_events(value: &str) -> Vec<otlyra_platform::PlatformEvent> {
    use otlyra_platform::{Key, Modifiers, PlatformEvent};

    let pressed = |key: Key| PlatformEvent::KeyPressed {
        key,
        modifiers: Modifiers::default(),
    };
    // The specification spells the named keys as code points in a private-use
    // area; these are the ones a driver actually sends.
    let named = match value {
        "\u{E006}" | "\u{E007}" | "\n" | "\r" => Some(Key::Enter),
        "\u{E003}" => Some(Key::Backspace),
        "\u{E004}" | "\t" => Some(Key::Tab),
        "\u{E00C}" => Some(Key::Escape),
        "\u{E012}" => Some(Key::Left),
        "\u{E013}" => Some(Key::Up),
        "\u{E014}" => Some(Key::Right),
        "\u{E015}" => Some(Key::Down),
        "\u{E011}" => Some(Key::Home),
        "\u{E010}" => Some(Key::End),
        "\u{E00E}" => Some(Key::PageUp),
        "\u{E00F}" => Some(Key::PageDown),
        "\u{E017}" => Some(Key::Delete),
        _ => None,
    };
    if let Some(key) = named {
        return vec![pressed(key)];
    }
    value
        .chars()
        .flat_map(|character| {
            [
                pressed(Key::Character(character)),
                PlatformEvent::TextInput(character),
            ]
        })
        .collect()
}

/// How long a navigation is waited for before it is answered anyway.
const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Standard base64, which is what the protocol carries a screenshot in.
///
/// Written out rather than taken as a dependency: it is fifteen lines, it is
/// used in one place, and a crate for it would be a crate to keep up to date
/// for as long as this program exists.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let bits = u32::from(buffer[0]) << 16 | u32::from(buffer[1]) << 8 | u32::from(buffer[2]);
        for index in 0..4 {
            // A chunk of one byte carries two characters and two pads; a chunk
            // of two carries three and one.
            if index <= chunk.len() {
                let sextet = (bits >> (18 - index * 6)) & 0b11_1111;
                out.push(char::from(ALPHABET[sextet as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The message a successful command is answered with.
pub fn success(id: u64, result: Value) -> Value {
    json!({ "type": "success", "id": id, "result": result })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetcher::{Loaded, Loader};

    /// A loader that answers everything with the same small page.
    struct Pages;

    impl Loader for Pages {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<title>Driven</title><body><p id=greeting>hello".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    fn session() -> Session {
        Session::new(Browser::new(Pages), (800, 600))
    }

    fn command(id: u64, method: &str, params: Value) -> Command {
        Command {
            id,
            method: method.to_owned(),
            params,
        }
    }

    #[test]
    fn a_command_needs_an_id_and_a_method() {
        assert!(Command::parse(r#"{"id":1,"method":"session.status"}"#).is_ok());
        assert_eq!(
            Command::parse(r#"{"method":"session.status"}"#)
                .unwrap_err()
                .code,
            "invalid argument"
        );
        assert_eq!(
            Command::parse("not json at all").unwrap_err().code,
            "invalid argument"
        );
    }

    #[test]
    fn params_default_to_nothing_rather_than_to_a_failure() {
        // A method that takes nothing is called without params by most clients.
        let parsed = Command::parse(r#"{"id":2,"method":"session.status"}"#).expect("a command");
        assert_eq!(parsed.params, json!({}));
    }

    #[test]
    fn a_session_reports_what_it_is_before_it_reports_what_it_can_do() {
        let mut session = session();
        let status = session
            .dispatch(&command(1, "session.status", json!({})))
            .expect("status always answers");
        assert_eq!(status["ready"], json!(true));

        let opened = session
            .dispatch(&command(2, "session.new", json!({})))
            .expect("a session opens");
        assert_eq!(opened["capabilities"]["browserName"], json!(NAME));

        // A second client asking now is told the browser is taken rather than
        // being handed a session that would fight the first for one browser.
        let status = session
            .dispatch(&command(3, "session.status", json!({})))
            .expect("status always answers");
        assert_eq!(status["ready"], json!(false));
    }

    #[test]
    fn subscribing_to_a_module_subscribes_to_its_events() {
        let mut session = session();
        session
            .dispatch(&command(1, "session.subscribe", json!({"events": ["log"]})))
            .expect("subscribed");
        // The specification lets a client name a module and mean all of it.
        assert!(session.subscribed("log.entryAdded"));
        assert!(!session.subscribed("network.responseCompleted"));

        session
            .dispatch(&command(
                2,
                "session.unsubscribe",
                json!({"events": ["log"]}),
            ))
            .expect("unsubscribed");
        assert!(!session.subscribed("log.entryAdded"));
    }

    #[test]
    fn navigating_reports_where_it_arrived() {
        let mut session = session();
        let result = session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({"context": CONTEXT, "url": "https://driven.example/"}),
            ))
            .expect("navigated");
        assert_eq!(result["url"], json!("https://driven.example/"));

        let tree = session
            .dispatch(&command(2, "browsingContext.getTree", json!({})))
            .expect("a tree");
        assert_eq!(tree["contexts"][0]["url"], json!("https://driven.example/"));
        assert_eq!(tree["contexts"][0]["context"], json!(CONTEXT));
    }

    #[test]
    fn a_command_aimed_at_a_context_we_do_not_have_is_refused() {
        let mut session = session();
        let error = session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({"context": "somebody-elses-tab", "url": "https://driven.example/"}),
            ))
            .unwrap_err();
        assert_eq!(error.code, "no such frame");
    }

    #[test]
    fn a_screenshot_comes_back_as_a_png_in_base64() {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://driven.example/"}),
            ))
            .expect("navigated");
        let shot = session
            .dispatch(&command(2, "browsingContext.captureScreenshot", json!({})))
            .expect("a screenshot");

        let data = shot["data"].as_str().expect("base64 text");
        // The signature a PNG starts with, as those bytes look in base64.
        assert!(
            data.starts_with("iVBORw0KGgo"),
            "{}",
            &data[..16.min(data.len())]
        );
    }

    /// Navigate, and draw a frame, which is what gives the page geometry a
    /// click can be tested against.
    fn opened() -> Session {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://driven.example/"}),
            ))
            .expect("navigated");
        session
    }

    #[test]
    fn a_selector_finds_the_nodes_the_cascade_would_have_matched() {
        let mut session = opened();
        let found = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "css", "value": "#greeting"}}),
            ))
            .expect("located");

        let nodes = found["nodes"].as_array().expect("a list");
        assert_eq!(nodes.len(), 1, "{nodes:?}");
        assert_eq!(nodes[0]["type"], json!("node"));
        assert_eq!(nodes[0]["value"]["localName"], json!("p"));
        assert_eq!(nodes[0]["value"]["nodeType"], json!(1));
        assert_eq!(nodes[0]["value"]["attributes"]["id"], json!("greeting"));
        // A handle the client can hand back, and that names the same node when
        // it does.
        let shared = nodes[0]["sharedId"].as_str().expect("a handle");
        assert!(node_of(shared).is_some(), "{shared:?}");
    }

    #[test]
    fn a_selector_that_matches_nothing_is_an_empty_list_and_not_an_error() {
        let mut session = opened();
        let found = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "css", "value": ".nothing-here"}}),
            ))
            .expect("located nothing, which is an answer");
        assert_eq!(found["nodes"], json!([]));
    }

    #[test]
    fn a_selector_that_is_not_one_says_so() {
        let mut session = opened();
        let error = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "css", "value": ">>> not a selector"}}),
            ))
            .unwrap_err();
        assert_eq!(error.code, "invalid argument");
    }

    #[test]
    fn a_locator_we_do_not_have_yet_is_told_apart_from_one_that_matched_nothing() {
        let mut session = opened();
        let error = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "innerText", "value": "hello"}}),
            ))
            .unwrap_err();
        // An empty list would have read as *nothing matched*, which is a
        // different fact and would send a driver looking at its selector.
        assert_eq!(error.code, "unsupported operation");
    }

    #[test]
    fn an_action_aimed_at_an_element_lands_where_the_engine_drew_it() {
        let mut session = opened();
        let found = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "css", "value": "#greeting"}}),
            ))
            .expect("located");
        let shared = found["nodes"][0]["sharedId"]
            .as_str()
            .expect("a handle")
            .to_owned();

        // The centre of the element, worked out by the browser rather than by
        // the driver: naming an element is exactly the promise that the driver
        // does not have to know the layout.
        let action = json!({
            "type": "pointerMove",
            "x": 0,
            "y": 0,
            "origin": {"type": "element", "element": {"sharedId": shared}},
        });
        let (x, y) = session.point_of(&action).expect("a point");

        let page = session.browser.active_page().expect("a page");
        let node =
            node_of(found["nodes"][0]["sharedId"].as_str().expect("a handle")).expect("a node");
        let rect = page
            .boxes()
            .box_for(node)
            .and_then(|id| page.rect_of(id))
            .expect("the element was drawn");
        assert_eq!(x, f64::from(rect.x + rect.width / 2.0));
        assert_eq!(y, f64::from(rect.y + rect.height / 2.0));
    }

    #[test]
    fn a_handle_that_names_nothing_is_refused_rather_than_clicked_at_the_origin() {
        let session = opened();
        let error = session
            .point_of(&json!({
                "type": "pointerMove",
                "origin": {"type": "element", "element": {"sharedId": "not-a-number"}},
            }))
            .unwrap_err();
        assert_eq!(error.code, "no such node");
    }

    #[test]
    fn a_pointer_action_list_is_performed_in_order() {
        let mut session = opened();
        // A click is three actions from one source, and all three have to arrive
        // for the browser to have seen a click at all.
        session
            .dispatch(&command(
                2,
                "input.performActions",
                json!({"actions": [{
                    "type": "pointer",
                    "id": "mouse",
                    "actions": [
                        {"type": "pointerMove", "x": 40, "y": 20},
                        {"type": "pointerDown", "button": 0},
                        {"type": "pointerUp", "button": 0},
                    ],
                }]}),
            ))
            .expect("performed");
    }

    #[test]
    fn typing_a_character_presses_a_key_and_types_it() {
        use otlyra_platform::{Key, PlatformEvent};

        // Both events, because a window sends both and the browser above cannot
        // tell where they came from — nor should it be able to.
        let events = key_events("a");
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            PlatformEvent::KeyPressed {
                key: Key::Character('a'),
                ..
            }
        ));
        assert!(matches!(events[1], PlatformEvent::TextInput('a')));

        // A named key is a press and types nothing.
        let enter = key_events("\u{E007}");
        assert_eq!(enter.len(), 1);
        assert!(matches!(
            enter[0],
            PlatformEvent::KeyPressed {
                key: Key::Enter,
                ..
            }
        ));
    }

    #[test]
    fn an_action_we_do_not_have_says_which_one() {
        let mut session = opened();
        let error = session
            .dispatch(&command(
                2,
                "input.performActions",
                json!({"actions": [{
                    "type": "pointer",
                    "id": "pen",
                    "actions": [{"type": "pointerCancel"}],
                }]}),
            ))
            .unwrap_err();
        assert_eq!(error.code, "unsupported operation");
        assert!(error.message.contains("pointerCancel"), "{}", error.message);
    }

    #[test]
    fn nothing_is_reported_to_a_client_that_did_not_ask() {
        let mut session = opened();
        // A navigation fetched something, and a client that subscribed to
        // nothing hears about none of it. Sending events nobody asked for is
        // how a protocol turns a quiet connection into a firehose.
        assert!(session.drain_events().is_empty());
    }

    #[test]
    fn a_request_is_reported_once_when_made_and_once_when_it_ends() {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "session.subscribe",
                json!({"events": ["network"]}),
            ))
            .expect("subscribed");
        session
            .dispatch(&command(
                2,
                "browsingContext.navigate",
                json!({"url": "https://driven.example/"}),
            ))
            .expect("navigated");

        let events = session.drain_events();
        let methods: Vec<&str> = events
            .iter()
            .filter_map(|event| event["method"].as_str())
            .collect();
        assert!(
            methods.contains(&"network.beforeRequestSent"),
            "{methods:?}"
        );
        assert!(
            methods.contains(&"network.responseCompleted"),
            "{methods:?}"
        );

        // The address it was asked for, and how much came back.
        let completed = events
            .iter()
            .find(|event| event["method"] == json!("network.responseCompleted"))
            .expect("one completed");
        assert_eq!(
            completed["params"]["request"]["url"],
            json!("https://driven.example/")
        );
        assert!(
            completed["params"]["response"]["bytesReceived"]
                .as_u64()
                .is_some_and(|bytes| bytes > 0)
        );

        // And asked again, the same request is not reported a second time: an
        // event stream that repeated itself would have a client counting the
        // same load twice.
        assert!(session.drain_events().is_empty());
    }

    #[test]
    fn a_failed_request_ends_with_a_reason_rather_than_never_ending() {
        struct Broken;
        impl Loader for Broken {
            fn load(&self, _url: &str) -> Result<Loaded, String> {
                Err("the socket said no".to_owned())
            }
        }

        let mut session = Session::new(Browser::new(Broken), (400, 300));
        session
            .dispatch(&command(
                1,
                "session.subscribe",
                json!({"events": ["network.responseCompleted"]}),
            ))
            .expect("subscribed");
        session
            .dispatch(&command(
                2,
                "browsingContext.navigate",
                json!({"url": "https://broken.example/"}),
            ))
            .expect("navigation is answered even when the load is not");

        let events = session.drain_events();
        let completed = events
            .iter()
            .find(|event| event["method"] == json!("network.responseCompleted"))
            .expect("a request that failed still ended");
        // A client waiting on one event for both outcomes cannot hang on this.
        assert_eq!(
            completed["params"]["response"]["statusText"],
            json!("the socket said no")
        );
    }

    #[test]
    fn what_the_browser_says_reaches_a_client_that_asked_for_it() {
        let journal = crate::observability::journal();
        let mut session = session();
        session
            .dispatch(&command(1, "session.subscribe", json!({"events": ["log"]})))
            .expect("subscribed");
        // Whatever the journal held when the session opened is behind the
        // cursor, so only what happens next arrives.
        session.drain_events();

        journal.record_for_test(tracing::Level::WARN, "otlyra_app::test", "something odd");
        let events = session.drain_events();
        let entry = events
            .iter()
            .find(|event| event["method"] == json!("log.entryAdded"))
            .expect("the line arrived");
        assert_eq!(entry["params"]["text"], json!("something odd"));
        assert_eq!(entry["params"]["level"], json!("warn"));
        // Not `javascript`: this is the browser talking about itself, and saying
        // otherwise would be a lie a client could act on.
        assert_eq!(entry["params"]["type"], json!(VENDOR));
    }

    /// The handle of the first node matching `selector`.
    fn handle(session: &mut Session, selector: &str) -> String {
        let found = session
            .dispatch(&command(
                99,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "css", "value": selector}}),
            ))
            .expect("located");
        found["nodes"][0]["sharedId"]
            .as_str()
            .expect("a handle")
            .to_owned()
    }

    #[test]
    fn one_command_says_what_the_cascade_and_the_layout_both_did() {
        let mut session = opened();
        let shared = handle(&mut session, "#greeting");
        let explained = session
            .dispatch(&command(3, "otlyra:explain", json!({"sharedId": shared})))
            .expect("explained");

        // What the cascade computed, from the style the engine actually used
        // rather than from a script asking the page about itself.
        assert_eq!(explained["computed"]["display"], json!("block"));
        assert!(explained["computed"]["font-size"].is_string());

        // And what the layout made of it. The border box is where the last
        // frame drew it, so it is the rectangle a click is tested against.
        let border = &explained["box"]["border"];
        assert!(border["width"].as_f64().is_some_and(|width| width > 0.0));
        assert!(border["height"].as_f64().is_some_and(|height| height > 0.0));

        // The content box is the border box less what is around it, and the
        // arithmetic is the engine's rather than the client's to redo.
        let content = &explained["box"]["content"];
        assert!(
            content["width"].as_f64().unwrap_or_default()
                <= border["width"].as_f64().unwrap_or_default()
        );

        assert_eq!(explained["node"]["value"]["localName"], json!("p"));
        // A paragraph lays nothing into tracks, and says so rather than
        // returning an empty set that reads as *a grid with no lines*.
        assert_eq!(explained["tracks"], Value::Null);
    }

    #[test]
    fn a_container_explains_the_tracks_it_laid_its_children_into() {
        struct Grid;
        impl Loader for Grid {
            fn load(&self, url: &str) -> Result<Loaded, String> {
                Ok(Loaded {
                    content_type: Some("text/html".to_owned()),
                    bytes: b"<style>.g{display:grid;gap:10px;\
                             grid-template-columns:100px 100px}</style>\
                             <div class=g><div>a</div><div>b</div></div>"
                        .to_vec(),
                    charset: Some("utf-8".to_owned()),
                    final_url: url.to_owned(),
                    ..Default::default()
                })
            }
        }

        let mut session = Session::new(Browser::new(Grid), (800, 600));
        session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://grid.example/"}),
            ))
            .expect("navigated");
        let shared = handle(&mut session, ".g");
        let explained = session
            .dispatch(&command(3, "otlyra:explain", json!({"sharedId": shared})))
            .expect("explained");

        let tracks = &explained["tracks"];
        assert_eq!(tracks["numbered"], json!(true));
        let columns = tracks["columns"].as_array().expect("column lines");
        // Every line is somewhere; only the ones a stylesheet can name are
        // numbered, which is the whole of what the overlay draws.
        assert!(columns.iter().all(|line| line["at"].as_f64().is_some()));
        assert!(columns.iter().any(|line| line["number"] == json!(1)),);
        assert_eq!(explained["computed"]["display"], json!("grid"));
    }

    #[test]
    fn a_handle_from_a_document_that_has_gone_is_refused() {
        let mut session = opened();
        let error = session
            .dispatch(&command(
                3,
                "otlyra:explain",
                json!({"sharedId": "18446744073709551615"}),
            ))
            .unwrap_err();
        // Answering about whatever else took that number would be worse than
        // refusing: it would be an answer about the wrong element.
        assert_eq!(error.code, "no such node");
    }

    #[test]
    fn highlighting_picks_a_node_out_and_lets_it_go_again() {
        let mut session = opened();
        let shared = handle(&mut session, "#greeting");

        let result = session
            .dispatch(&command(3, "otlyra:highlight", json!({"sharedId": shared})))
            .expect("highlighted");
        assert_eq!(result["highlighted"], json!(shared));
        assert!(session.browser.inspector_mut().selected.is_some());

        // And with no node named, the page goes back the way it was found.
        let cleared = session
            .dispatch(&command(4, "otlyra:highlight", json!({})))
            .expect("cleared");
        assert_eq!(cleared["highlighted"], Value::Null);
        assert!(session.browser.inspector_mut().selected.is_none());
    }

    #[test]
    fn what_needs_a_script_engine_says_so_rather_than_saying_nothing() {
        let mut session = session();
        let error = session
            .dispatch(&command(1, "script.evaluate", json!({})))
            .unwrap_err();
        // Told apart from an unknown method on purpose: this is a gap with a
        // date on it, and a client that meets it is not looking at a typo.
        assert_eq!(error.code, "unsupported operation");
        assert!(error.message.contains("M12"), "{}", error.message);

        let unknown = session
            .dispatch(&command(2, "storage.getCookies", json!({})))
            .unwrap_err();
        assert_eq!(unknown.code, "unknown command");
    }

    #[test]
    fn base64_matches_the_encoding_everything_else_speaks() {
        // The three padding cases, which are the only place an encoder goes
        // wrong: no padding, one byte over, two bytes over.
        assert_eq!(base64(b"abcdef"), "YWJjZGVm");
        assert_eq!(base64(b"abcde"), "YWJjZGU=");
        assert_eq!(base64(b"abcd"), "YWJjZA==");
        assert_eq!(base64(b""), "");
    }
}
