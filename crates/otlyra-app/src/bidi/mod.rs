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
//! than worked around. Everything that does not need a page script — tabs,
//! navigation and history, viewports, finding nodes by selector, input,
//! screenshots, the log, the network — does not wait for it, and is enough for
//! an agent to do real work.
//!
//! Nor is there a cookie jar or a second user context, so `storage.*` and
//! `browser.createUserContext` answer *unknown command* rather than answering
//! emptily: a client told there are no cookies would believe it.
//!
//! # A context is a tab
//!
//! Every tab is a browsing context a driver can name, and naming one is what
//! makes the browser act on it — commands act on the active tab, so naming and
//! switching are the same act. A frame would be a context of its own in BiDi
//! and this engine has no frames, so every context is reported with no parent
//! and no children rather than with a tree that is not there.
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
    /// Which tabs this client has been told about, and where each was.
    ///
    /// Both halves in one map, because the two questions a lifecycle event
    /// answers — is this tab new, and has it been anywhere — are asked of the
    /// same list at the same moment.
    known: std::collections::HashMap<crate::browser::TabId, String>,
}

/// The context id the one tab is known by.
///
/// One context because there is one tab being driven: tab handling over the
/// protocol is a later stage, and inventing ids for tabs a client cannot yet
/// address would be inventing a vocabulary nobody speaks.
pub const CONTEXT: &str = "otlyra-context-1";

/// What a tab is called on the wire.
///
/// A context name is a string to a client, and a tab's identity is a number, so
/// this is the one place the two are spelled against each other.
fn context_name(id: crate::browser::TabId) -> String {
    format!("otlyra-context-{}", id.0)
}

/// The tab a context name refers to, if it names one at all.
fn context_id(name: &str) -> Option<crate::browser::TabId> {
    name.strip_prefix("otlyra-context-")
        .and_then(|rest| rest.parse().ok())
        .map(crate::browser::TabId)
}

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
            known: std::collections::HashMap::new(),
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

            // --- browser ---------------------------------------------------
            "browser.close" => {
                // The session goes with it. There is no window to shut from
                // here — the shell owns that — so what this can honestly do is
                // end the session, which is what a client is asking for when it
                // says it is finished.
                self.open = false;
                self.events.clear();
                Ok(json!({}))
            }
            "browser.getUserContexts" => Ok(json!({
                // One profile, and no way to make another: user contexts are
                // separate cookie jars and storage, and there is neither yet.
                "userContexts": [{ "userContext": "default" }],
            })),

            // --- browsingContext -------------------------------------------
            "browsingContext.getTree" => Ok(json!({
                "contexts": (0..self.browser.tabs().len())
                    .map(|index| self.context_of(index))
                    .collect::<Vec<_>>(),
            })),
            "browsingContext.create" => {
                let id = self.browser.open_tab();
                // `background` decides whether the reader ends up looking at it.
                // A driver that omits it gets the tab it just made, which is
                // what every other command it sends will assume.
                let background = command
                    .params
                    .get("background")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !background && let Some(index) = self.browser.tab_index(id) {
                    self.browser.select_tab(index);
                }
                Ok(json!({ "context": context_name(id) }))
            }
            "browsingContext.close" => {
                let index = self.target(command)?;
                self.browser.close_tab(index);
                Ok(json!({}))
            }
            "browsingContext.activate" => {
                // `target` already switched to it, which is the whole command.
                self.target(command)?;
                Ok(json!({}))
            }
            "browsingContext.setViewport" => {
                self.check_context(command)?;
                if let Some(viewport) = command.params.get("viewport") {
                    let number = |key: &str| {
                        viewport
                            .get(key)
                            .and_then(Value::as_u64)
                            .map(|value| value as u32)
                    };
                    match (number("width"), number("height")) {
                        (Some(width), Some(height)) if width > 0 && height > 0 => {
                            self.viewport = (width, height);
                        }
                        _ => {
                            return Err(Error::invalid(
                                "setViewport needs a width and a height above zero",
                            ));
                        }
                    }
                }
                // Drawn at the new size before answering, so the next question
                // is asked of a page laid out for it.
                self.browser.prepare_frame(self.viewport(), LOAD_TIMEOUT);
                Ok(json!({}))
            }
            "browsingContext.traverseHistory" => {
                self.check_context(command)?;
                let delta = command
                    .params
                    .get("delta")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| Error::invalid("traverseHistory needs a delta"))?;
                // One step at a time, and it stops where the history does: a
                // delta past either end is as far as it goes rather than an
                // error, which is what going back twice from one entry means.
                for _ in 0..delta.unsigned_abs() {
                    if delta > 0 {
                        if !self.browser.can_go_forward() {
                            break;
                        }
                        self.browser.go_forward();
                    } else {
                        if !self.browser.can_go_back() {
                            break;
                        }
                        self.browser.go_back();
                    }
                    self.browser.wait_for_load(LOAD_TIMEOUT);
                }
                self.browser.prepare_frame(self.viewport(), LOAD_TIMEOUT);
                Ok(json!({}))
            }
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
        events.extend(self.context_events());
        events
    }

    /// What the fetcher has done that this client has not been told about.
    fn network_events(&mut self) -> Vec<Value> {
        use crate::fetcher::Status;

        // The fetcher is the browser's, not a tab's: it records what was asked
        // for and not which tab asked. So an event names the context that is
        // active, which is the tab a driver is working in and is right whenever
        // one tab is being driven at a time — and is stated here rather than
        // implied, because it is the one thing in these events that is a guess.
        let context = context_name(self.browser.active_id());
        let exchanges: Vec<crate::fetcher::Exchange> = self.browser.exchanges().to_vec();
        let mut events = Vec::new();
        for exchange in exchanges {
            if self.announced.insert(exchange.id) && self.subscribed("network.beforeRequestSent") {
                events.push(request_event(&context, &exchange));
            }
            let finished = !matches!(exchange.status, Status::Pending);
            if finished
                && self.completed.insert(exchange.id)
                && self.subscribed("network.responseCompleted")
            {
                events.push(response_event(&context, &exchange));
            }
        }
        events
    }

    /// Tabs that have opened or closed since this client was last told.
    ///
    /// Diffed rather than pushed, for the reason every other event here is: the
    /// browser is driven from one thread and what it has is readable, so the
    /// protocol stays at the edge instead of reaching into `new_tab`.
    fn context_events(&mut self) -> Vec<Value> {
        let open: Vec<(crate::browser::TabId, String)> = self
            .browser
            .tabs()
            .iter()
            .map(|tab| (tab.id, tab.url.clone()))
            .collect();
        let mut events = Vec::new();

        if self.subscribed("browsingContext.contextCreated") {
            for (index, (id, _)) in open.iter().enumerate() {
                if !self.known.contains_key(id) {
                    events.push(event(
                        "browsingContext.contextCreated",
                        self.context_of(index),
                    ));
                }
            }
        }
        if self.subscribed("browsingContext.contextDestroyed") {
            for id in self.known.keys() {
                if !open.iter().any(|(open, _)| open == id) {
                    events.push(event(
                        "browsingContext.contextDestroyed",
                        json!({
                            "context": context_name(*id),
                            "url": "",
                            "children": [],
                            "parent": Value::Null,
                            "userContext": "default",
                        }),
                    ));
                }
            }
        }
        // A tab whose address changed has been somewhere. There is one signal
        // for arriving and none for the document being ready before its
        // subresources are, so `load` is what is reported and
        // `domContentLoaded` is not — a client waiting on an event that never
        // comes is worse served than one told the event does not exist.
        if self.subscribed("browsingContext.load") {
            for (index, (id, url)) in open.iter().enumerate() {
                let moved = self.known.get(id).is_some_and(|last| last != url);
                if moved && !url.is_empty() {
                    let mut payload = self.context_of(index);
                    payload["navigation"] = Value::Null;
                    payload["timestamp"] = json!(now());
                    events.push(event("browsingContext.load", payload));
                }
            }
        }

        self.known = open.into_iter().collect();
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

    /// One tab, as the protocol describes a browsing context.
    ///
    /// No children and no parent: a frame is a context of its own in BiDi and
    /// this engine has no frames, so saying otherwise would describe a tree that
    /// is not there.
    fn context_of(&self, index: usize) -> Value {
        let tabs = self.browser.tabs();
        let tab = &tabs[index];
        json!({
            "context": context_name(tab.id),
            "url": tab.url,
            "children": [],
            "parent": Value::Null,
            "userContext": "default",
        })
    }

    /// Which tab a command is aimed at, made active so the browser acts on it.
    ///
    /// Commands name a context and the browser acts on whichever tab is active,
    /// so *naming* one and *switching to* it are the same act here. That is not
    /// a shortcut: a driver that navigates a background tab expects the
    /// navigation to happen, and the alternative is a second navigation path
    /// that only the protocol uses.
    fn target(&mut self, command: &Command) -> Result<usize, Error> {
        let Some(name) = command.params.get("context").and_then(Value::as_str) else {
            return Ok(self.browser.active());
        };
        // A real tab first. The name the session answered to before it had more
        // than one — `CONTEXT` — is also the name the *first* tab has, so
        // checking it first would turn every command aimed at that tab into a
        // command aimed at whichever tab happened to be active. It is a
        // fallback for a client that hardcoded the constant against a browser
        // whose first tab is gone, and nothing more.
        let index = match context_id(name).and_then(|id| self.browser.tab_index(id)) {
            Some(index) => index,
            None if name == CONTEXT => self.browser.active(),
            None => return Err(Error::no_such_context(name)),
        };
        if index != self.browser.active() {
            self.browser.select_tab(index);
        }
        Ok(index)
    }

    /// Refuse a command aimed at a context that is not ours.
    fn check_context(&mut self, command: &Command) -> Result<(), Error> {
        self.target(command).map(|_| ())
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

/// Headers as the protocol spells them: a name and a value that says it is
/// text, which is the only kind this browser has to report.
fn headers_json(headers: &[(String, String)]) -> Vec<Value> {
    headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": { "type": "string", "value": value } }))
        .collect()
}

/// The request half of both network events, which is the same object in each.
fn request_json(exchange: &crate::fetcher::Exchange) -> Value {
    json!({
        "request": exchange.id.to_string(),
        "url": exchange.url,
        "method": exchange.method,
        "headers": headers_json(&exchange.request_headers),
        // No cookie jar, so there are none to report rather than none to have.
        "cookies": [],
    })
}

/// A request the browser made, as a `network.beforeRequestSent`.
fn request_event(context: &str, exchange: &crate::fetcher::Exchange) -> Value {
    event(
        "network.beforeRequestSent",
        json!({
            "context": context,
            "isRedirect": false,
            "navigation": Value::Null,
            "redirectCount": 0,
            "timestamp": now(),
            "request": request_json(exchange),
            "otlyra:kind": format!("{:?}", exchange.kind).to_lowercase(),
        }),
    )
}

/// What became of it, as a `network.responseCompleted`.
///
/// A failure is reported here too, with its reason, rather than through
/// `fetchError`: the browser knows the request ended and why, and a client
/// waiting on one event for both outcomes is a client that cannot hang.
fn response_event(context: &str, exchange: &crate::fetcher::Exchange) -> Value {
    use crate::fetcher::Status;
    // The status a server actually answered with. It used to be a hardcoded
    // `200` for anything the transport returned, which made a 404 with an error
    // page indistinguishable from the page asked for — the same thing the
    // network pane was wrong about until the code was threaded up to it.
    let (status, text, bytes) = match &exchange.status {
        Status::Ok(bytes) => (exchange.code.unwrap_or(200), String::new(), *bytes),
        Status::Failed(error) => (0, error.clone(), 0),
        Status::Pending => (0, "still out".to_owned(), 0),
    };
    event(
        "network.responseCompleted",
        json!({
            "context": context,
            "isRedirect": false,
            "navigation": Value::Null,
            "redirectCount": 0,
            "timestamp": now(),
            "request": request_json(exchange),
            "response": {
                "url": exchange.url,
                "status": status,
                "statusText": text,
                "bytesReceived": bytes,
                "fromCache": false,
                "headers": headers_json(&exchange.response_headers),
                "mimeType": exchange.content_type.clone().map_or(Value::Null, Value::from),
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

    /// A tab is a browsing context, and a driver reaches every one of them by
    /// name. Its position is not its name: closing a tab shifts every tab after
    /// it, and a client holding an index would then be holding a different tab.
    #[test]
    fn every_tab_is_a_context_a_driver_can_name() {
        let mut session = session();
        let first = session
            .dispatch(&command(1, "browsingContext.getTree", json!({})))
            .unwrap();
        let first = first["contexts"][0]["context"].as_str().unwrap().to_owned();

        let second = session
            .dispatch(&command(
                2,
                "browsingContext.create",
                json!({ "type": "tab" }),
            ))
            .unwrap()["context"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_ne!(first, second);

        let tree = session
            .dispatch(&command(3, "browsingContext.getTree", json!({})))
            .unwrap();
        assert_eq!(tree["contexts"].as_array().unwrap().len(), 2);

        // Each is navigable by its own name, and naming one is what makes the
        // browser act on it.
        session
            .dispatch(&command(
                4,
                "browsingContext.navigate",
                json!({ "context": first, "url": "https://one.example/" }),
            ))
            .unwrap();
        session
            .dispatch(&command(
                5,
                "browsingContext.navigate",
                json!({ "context": second, "url": "https://two.example/" }),
            ))
            .unwrap();

        let tree = session
            .dispatch(&command(6, "browsingContext.getTree", json!({})))
            .unwrap();
        let url_of = |name: &str| {
            tree["contexts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|context| context["context"] == name)
                .map(|context| context["url"].as_str().unwrap().to_owned())
                .unwrap()
        };
        assert!(url_of(&first).contains("one.example"));
        assert!(url_of(&second).contains("two.example"));

        // Closing the first shifts the second's index and not its name.
        session
            .dispatch(&command(
                7,
                "browsingContext.close",
                json!({ "context": first }),
            ))
            .unwrap();
        let tree = session
            .dispatch(&command(8, "browsingContext.getTree", json!({})))
            .unwrap();
        assert_eq!(tree["contexts"].as_array().unwrap().len(), 1);
        assert_eq!(tree["contexts"][0]["context"], second.as_str());

        // And a name that no longer names anything is refused rather than
        // quietly answered by whatever is active.
        assert_eq!(
            session
                .dispatch(&command(
                    9,
                    "browsingContext.navigate",
                    json!({ "context": first, "url": "https://three.example/" }),
                ))
                .unwrap_err()
                .code,
            "no such frame"
        );
    }

    /// A real name always wins over the compatibility one.
    ///
    /// `CONTEXT` is what the session answered to before it had more than one
    /// tab, and it is *also* what a tab called `1` would be called. Resolved in
    /// the wrong order, naming that tab meant "whatever is active", so
    /// navigating the first tab navigated the second. A live browser found it
    /// and the unit tests could not, because tab names come from a counter the
    /// test binary shares and never start at one — so the ordering is asserted
    /// here directly rather than through a name that happens to collide.
    #[test]
    fn a_name_that_is_a_tab_beats_the_name_that_is_a_fallback() {
        let mut session = session();
        let first = context_name(session.browser.tabs()[0].id);
        let opened = session.browser.open_tab();
        let second = session.browser.tab_index(opened).unwrap();
        session.browser.select_tab(second);

        // The first tab is named while the second is active: the command must
        // land on the one it named.
        let target = session
            .target(&command(1, "x", json!({ "context": first })))
            .unwrap();
        assert_eq!(target, 0);
        assert_eq!(session.browser.active(), 0);

        // And the fallback still answers for a client that hardcoded it, since
        // no tab here is called that.
        session.browser.select_tab(second);
        assert_eq!(
            session
                .target(&command(2, "x", json!({ "context": CONTEXT })))
                .unwrap(),
            second,
            "the compatibility name means whatever is active, and only when it names no tab"
        );
    }

    /// Back and forward, which the browser has had per tab since W1 and the
    /// protocol had no way to ask for.
    #[test]
    fn traverse_history_walks_a_tab_and_stops_at_its_ends() {
        let mut session = session();
        for (id, url) in [(1, "https://one.example/"), (2, "https://two.example/")] {
            session
                .dispatch(&command(
                    id,
                    "browsingContext.navigate",
                    json!({ "url": url }),
                ))
                .unwrap();
        }

        let here = |session: &mut Session| {
            session
                .dispatch(&command(99, "browsingContext.getTree", json!({})))
                .unwrap()["contexts"][0]["url"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert!(here(&mut session).contains("two.example"));

        session
            .dispatch(&command(
                3,
                "browsingContext.traverseHistory",
                json!({ "delta": -1 }),
            ))
            .unwrap();
        assert!(here(&mut session).contains("one.example"));

        // Past the end is as far as it goes rather than an error: going back
        // twice from one entry means going back once.
        session
            .dispatch(&command(
                4,
                "browsingContext.traverseHistory",
                json!({ "delta": -5 }),
            ))
            .unwrap();
        assert!(here(&mut session).contains("one.example"));

        session
            .dispatch(&command(
                5,
                "browsingContext.traverseHistory",
                json!({ "delta": 1 }),
            ))
            .unwrap();
        assert!(here(&mut session).contains("two.example"));
    }

    /// The viewport is what a screenshot and a layout are made at, so setting it
    /// has to reach both.
    #[test]
    fn set_viewport_changes_what_the_page_is_laid_out_at() {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({ "url": "https://one.example/" }),
            ))
            .unwrap();
        session
            .dispatch(&command(
                2,
                "browsingContext.setViewport",
                json!({ "viewport": { "width": 400, "height": 300 } }),
            ))
            .unwrap();
        assert_eq!(session.viewport, (400, 300));

        // A viewport with no room in it is refused rather than laid out against.
        assert_eq!(
            session
                .dispatch(&command(
                    3,
                    "browsingContext.setViewport",
                    json!({ "viewport": { "width": 0, "height": 300 } }),
                ))
                .unwrap_err()
                .code,
            "invalid argument"
        );
    }

    /// Opening and closing a tab is something a client can watch for.
    #[test]
    fn a_client_is_told_when_a_context_opens_and_closes() {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "session.subscribe",
                json!({ "events": ["browsingContext"] }),
            ))
            .unwrap();
        // The tab that was already open is announced once, and then not again.
        assert_eq!(session.drain_events().len(), 1);
        assert!(session.drain_events().is_empty());

        let opened = session
            .dispatch(&command(2, "browsingContext.create", json!({})))
            .unwrap()["context"]
            .as_str()
            .unwrap()
            .to_owned();
        let events = session.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["method"], "browsingContext.contextCreated");
        assert_eq!(events[0]["params"]["context"], opened.as_str());

        session
            .dispatch(&command(
                3,
                "browsingContext.close",
                json!({ "context": opened }),
            ))
            .unwrap();
        let events = session.drain_events();
        assert_eq!(events[0]["method"], "browsingContext.contextDestroyed");
        assert_eq!(events[0]["params"]["context"], opened.as_str());
    }

    /// A `404` that returned a body is not a `200`. The event used to say it was.
    #[test]
    fn a_network_event_carries_the_status_the_server_answered_with() {
        use crate::fetcher::{Exchange, ResourceKind, Status};
        let mut missing =
            Exchange::for_test(7, ResourceKind::Document, "https://x/gone", Status::Ok(18));
        missing.code = Some(404);
        missing.response_headers = vec![("content-type".to_owned(), "text/html".to_owned())];
        missing.content_type = Some("text/html".to_owned());

        let value = response_event("otlyra-context-1", &missing);
        assert_eq!(value["params"]["response"]["status"], 404);
        assert_eq!(value["params"]["response"]["mimeType"], "text/html");
        assert_eq!(
            value["params"]["response"]["headers"][0]["name"],
            "content-type"
        );
        assert_eq!(value["params"]["request"]["method"], "GET");
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
        // A context is named after the tab it is, so the name is whatever that
        // tab was called rather than a constant — but it is a name, and the
        // session answers to it.
        let name = tree["contexts"][0]["context"].as_str().expect("a name");
        assert!(name.starts_with("otlyra-context-"));
        assert!(
            session
                .dispatch(&command(
                    3,
                    "browsingContext.captureScreenshot",
                    json!({ "context": name }),
                ))
                .is_ok()
        );
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
