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
//! navigation and history, viewports, finding nodes three ways, input,
//! screenshots, cookies, the log, the network — does not wait for it, and is
//! enough for an agent to do real work.
//!
//! There is one user context and one jar, so `storage.*` answers about the jar
//! there is and `browser.createUserContext` answers *unknown command*. What a
//! client sends as a partition key is answered with the one partition, rather
//! than refused: a client that sent the default meant this one.
//!
//! The response phase of network interception is the gap left in what does not
//! need a script: pausing between the headers and the body would mean a loader
//! that hands a response back in two pieces, and ours returns one finished
//! resource. It says so rather than accepting an intercept it would never
//! report.
//!
//! # What a caller with no eyes reads
//!
//! `otlyra:readPage` and `otlyra:snapshot` are the two commands an agent leans
//! on, and both come off the accessibility tree rather than off the DOM — see
//! [`crate::digest`] for why that is the one honest source for *what is on this
//! page*. Between them a caller can read a page, find the thing it wants and
//! click it without ever having written a selector, which is the sequence that
//! otherwise costs a screenshot and a guess per turn.
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
//!
//! # Nothing waits inside a command
//!
//! A navigation takes as long as the network does, and the thread that runs it is
//! the thread that reads the socket. So a command that cannot be answered yet is
//! *started* and parked — [`Session::begin`] hands back [`Outcome::Parked`], and
//! [`Session::resolve`] answers it under the same id once the browser has got
//! where the client asked. Between the two, the loop keeps reading: a driver can
//! ask about anything else, is sent its events on time, and — when request
//! interception arrives — can answer a request the load is still waiting on,
//! which a command that waited inside itself could never do.
//!
//! `wait` is the specification's, with the specification's default: `none`
//! answers once the load has begun, `interactive` once the document is parsed,
//! `complete` once nothing is outstanding. The three are one enum in the browser
//! ([`crate::browser::Readiness`]) rather than three ad-hoc checks, because they
//! are also what the lifecycle events report.
//!
//! [`Session::dispatch`] still waits, and is what a test and the tool surface in
//! [`crate::mcp`] use — both have one message outstanding at a time and nothing
//! to do with an answer that arrives later.
//!
//! # The lifecycle, per tab
//!
//! `browsingContext.navigationStarted`, `domContentLoaded` and `load`, each
//! carrying the navigation it belongs to, so a client that started two can tell
//! which finished. Diffed out of the browser's own state rather than pushed from
//! the loader, which keeps the protocol at the edge — see
//! [`Session::context_events`].

pub mod intercept;
mod server;

pub use server::{Server, listen};

use serde_json::{Value, json};

use crate::browser::{Browser, Readiness};

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
    /// Which tabs this client has been told about, and what it was told.
    ///
    /// One entry per tab because every lifecycle question — is this tab new, has
    /// it started going somewhere, is its document ready, has it finished — is
    /// asked of the same list at the same moment, and answering them from
    /// separate records is how two of them come to disagree.
    known: std::collections::HashMap<crate::browser::TabId, Seen>,
    /// What this client has asked to have held before it is sent.
    ///
    /// On the session rather than on the browser because it is one client's
    /// instruction: a driver that goes away must leave a browser that loads
    /// pages, and a held request with nobody to release it is a page that never
    /// finishes. See [`intercept`].
    intercepts: Vec<intercept::Intercept>,
    /// The next intercept's name.
    next_intercept: u64,
    /// Which held requests this client has already been told about.
    blocked: std::collections::HashSet<u64>,
    /// Commands that have been started and cannot be answered yet.
    ///
    /// The whole of what makes this non-blocking. A `navigate` that waited inside
    /// `dispatch` would hold the read loop for the length of the load, so a
    /// driver could not subscribe, could not ask about anything else, and — once
    /// there is request interception — could not answer the very request the load
    /// is waiting on. Parked here instead, and answered by [`Session::resolve`]
    /// once the browser has got far enough.
    waiting: Vec<Waiting>,
    /// The window this session drives, when it drives one.
    ///
    /// Present for a [`Session::windowed`] session and absent otherwise. It is
    /// the whole difference between the two: with it, an action is delivered the
    /// way a window delivers it and a picture comes off the compositor's retained
    /// surface — the same `compose` → damage → present path a person is looking
    /// at. Without it, the browser is driven with no window at all and a picture
    /// is one offscreen `paint` of the page.
    window: Option<otlyra_platform::FramePump>,
}

/// How far a client asked a navigation to get before it is answered.
///
/// The specification's own three, and its own default: a client that says
/// nothing means `complete`, which is what a person means by *open this page*.
/// `none` is the one that matters for anything clever — it answers the moment the
/// load has started, which is what a driver that wants to watch the load, or to
/// intercept a request the load makes, has to have.
fn readiness_asked_for(command: &Command) -> Result<Readiness, Error> {
    match command
        .params
        .get("wait")
        .and_then(Value::as_str)
        .unwrap_or("complete")
    {
        "none" => Ok(Readiness::Started),
        "interactive" => Ok(Readiness::Interactive),
        "complete" => Ok(Readiness::Complete),
        other => Err(Error::invalid(format!(
            "{other:?} is not a readiness state: it is none, interactive or complete"
        ))),
    }
}

/// What a readiness is called on the wire.
fn readiness_word(readiness: Readiness) -> &'static str {
    match readiness {
        Readiness::Started => "none",
        Readiness::Interactive => "interactive",
        Readiness::Complete => "complete",
    }
}

/// What a client has last been told about one tab.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    /// Where it was.
    url: String,
    /// Which navigation that was.
    navigation: Option<u64>,
    /// How far that navigation had got.
    readiness: Readiness,
}

/// A command that has been started and is waiting on the browser.
struct Waiting {
    /// The client's number for it, which is what the answer is sent under.
    id: u64,
    /// The tab it is about.
    tab: crate::browser::TabId,
    /// How far that tab's load has to get before it can be answered.
    until: Readiness,
    /// When to answer anyway.
    ///
    /// A driver waiting forever on a page that never finishes is a driver that
    /// has hung, and *it took too long* is a fact it can act on.
    deadline: std::time::Instant,
}

/// What starting a command produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// It is finished; here is the result.
    Done(Value),
    /// It has been started and will be answered out of [`Session::resolve`].
    Parked,
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
        Self::over(browser, viewport, None)
    }

    /// A session over `browser` that drives the browser's whole *window*.
    ///
    /// The interface stays where it is, an action is delivered through the same
    /// path the window delivers one, and a picture is the composited surface the
    /// window would be showing — chrome, page, inspector and all, in window
    /// coordinates. This is what an agent needs to check a browser-owned
    /// interface: the page-only session above cannot see the toolbar, and a
    /// whole-surface `paint` cannot see the compositor.
    ///
    /// There is no GPU and no event loop behind it. The window it stands in for
    /// is a retained raster surface, which is the same surface the live window
    /// rasterizes into before its swapchain blit.
    pub fn windowed(mut browser: Browser, viewport: (u32, u32)) -> Self {
        let mut window = otlyra_platform::FramePump::new(otlyra_platform::Viewport::new(
            viewport.0, viewport.1, 1.0,
        ));
        if let Err(error) = window.open(&mut browser) {
            tracing::error!(%error, "the driven window could not draw its first frame");
        }
        Self::over(browser, viewport, Some(window))
    }

    fn over(
        browser: Browser,
        viewport: (u32, u32),
        window: Option<otlyra_platform::FramePump>,
    ) -> Self {
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
            intercepts: Vec::new(),
            next_intercept: 0,
            blocked: std::collections::HashSet::new(),
            waiting: Vec::new(),
            window,
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

    /// Answer one command, waiting for it if it needs waiting for.
    ///
    /// The blocking door, kept for the callers that have nothing else to do while
    /// they wait: a test, and the tool surface in [`crate::mcp`], which speaks a
    /// protocol with one message outstanding at a time and so cannot use the
    /// answer that arrives later anyway.
    ///
    /// The socket does *not* come through here — see [`Session::begin`].
    pub fn dispatch(&mut self, command: &Command) -> Result<Value, Error> {
        match self.begin(command)? {
            Outcome::Done(value) => Ok(value),
            Outcome::Parked => {
                loop {
                    self.pump();
                    for (id, answer) in self.resolve() {
                        if id == command.id {
                            return answer;
                        }
                    }
                    if self.waiting.is_empty() {
                        // Parked and then resolved under another id, which cannot
                        // happen — but answering nothing would hang the caller.
                        return Ok(json!({}));
                    }
                    // A short sleep rather than a spin: the fetch threads are
                    // doing the work and this thread has nothing to add.
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
    }

    /// Advance the browser by whatever has finished, without waiting for
    /// anything.
    ///
    /// One call per turn of a loop that also has a socket to read. Everything
    /// that takes time here happens on the fetch threads; this is the moment
    /// their results become the browser's state.
    pub fn pump(&mut self) -> bool {
        let changed = self.browser.pump();
        if changed {
            self.draw_frame();
        }
        changed
    }

    /// Answer every parked command the browser has caught up with.
    ///
    /// Paired with [`Session::begin`]: what that parked, this hands back, under
    /// the id it was asked with. A command whose deadline passed is answered with
    /// a timeout rather than left parked, because a driver waiting on a reply
    /// that is never coming is worse served than one told the page was slow.
    pub fn resolve(&mut self) -> Vec<(u64, Result<Value, Error>)> {
        let now = std::time::Instant::now();
        let mut answers = Vec::new();
        let mut still = Vec::new();

        for waiting in std::mem::take(&mut self.waiting) {
            let Some(index) = self.browser.tab_index(waiting.tab) else {
                // The tab went away under it. Not an error the client caused, and
                // not something to keep waiting on.
                answers.push((
                    waiting.id,
                    Err(Error::no_such_context(&context_name(waiting.tab))),
                ));
                continue;
            };
            let readiness = self.browser.readiness(index);
            if readiness >= waiting.until {
                self.draw_frame();
                answers.push((waiting.id, Ok(self.navigation_result(index))));
                continue;
            }
            if now >= waiting.deadline {
                answers.push((
                    waiting.id,
                    Err(Error {
                        code: "unknown error",
                        message: format!(
                            "the page had not reached {} when the wait ran out",
                            readiness_word(waiting.until)
                        ),
                    }),
                ));
                continue;
            }
            still.push(waiting);
        }

        self.waiting = still;
        answers
    }

    /// What a navigation answers with, once it has got far enough.
    fn navigation_result(&self, index: usize) -> Value {
        let tab = &self.browser.tabs()[index];
        json!({
            "navigation": tab.navigation.map(|id| id.to_string()),
            "url": tab.url,
        })
    }

    /// Park a command until the tab at `index` reaches `until`.
    fn park(&mut self, command: &Command, index: usize, until: Readiness) -> Outcome {
        // Already there — a blank tab, a system page, anything served from the
        // cache within the same turn. Answered now rather than parked for a turn
        // of the loop, so a fast load costs no round trip.
        if self.browser.readiness(index) >= until {
            self.draw_frame();
            return Outcome::Done(self.navigation_result(index));
        }
        let timeout = command
            .params
            .get("timeout")
            .and_then(Value::as_u64)
            .map_or(LOAD_TIMEOUT, std::time::Duration::from_millis);
        self.waiting.push(Waiting {
            id: command.id,
            tab: self.browser.tabs()[index].id,
            until,
            deadline: std::time::Instant::now() + timeout,
        });
        Outcome::Parked
    }

    /// Start one command.
    ///
    /// The result is the `result` object of a success message; the caller wraps
    /// it. Errors come back as [`Error`] and are wrapped the same way, so there
    /// is one place that knows the message envelope.
    ///
    /// # Why some commands do not finish here
    ///
    /// A navigation takes as long as the network does, and this thread is also
    /// the one reading the socket. Finishing it here would mean a driver that
    /// cannot ask anything else, cannot be sent an event, and cannot answer a
    /// request the load itself is waiting on. So a navigation is *started* and
    /// [`Outcome::Parked`], and [`Session::resolve`] answers it when the browser
    /// has got where the client asked it to get to.
    pub fn begin(&mut self, command: &Command) -> Result<Outcome, Error> {
        // Everything that finishes at once goes through the match below and is
        // wrapped; the three that may not are handled first.
        match command.method.as_str() {
            "browsingContext.navigate" => {
                let url = command.string("url")?.to_owned();
                self.check_context(command)?;
                let until = readiness_asked_for(command)?;
                self.browser.navigate(&url);
                // One pump before parking: a load served from the cache or from a
                // canned loader can already be finished, and answering it in the
                // same turn is a round trip a driver does not have to make.
                self.pump();
                let index = self.browser.active();
                Ok(self.park(command, index, until))
            }
            "browsingContext.reload" => {
                self.check_context(command)?;
                let until = readiness_asked_for(command)?;
                self.browser.reload();
                self.pump();
                let index = self.browser.active();
                Ok(self.park(command, index, until))
            }
            // A step through the history is a load like any other, and it used to
            // be the one command that waited for one on this thread — once per
            // step, so a `delta` of five was five load timeouts with the socket
            // unread. Started and parked like the other two.
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
                    // Each step's load is started here and left to the pump; only
                    // the last one is what the client is told about.
                    self.pump();
                }
                let index = self.browser.active();
                Ok(self.park(command, index, Readiness::Complete))
            }
            _ => self.dispatch_now(command).map(Outcome::Done),
        }
    }

    /// Forget everything one connection asked for.
    ///
    /// A driver that goes away must leave a browser that loads pages. Its
    /// intercepts go with it — otherwise the gate stays installed with nobody
    /// left to answer it, and every address it matches is held forever — and so
    /// does what it was told and what it was waiting for, because the next client
    /// on this socket is a different client and neither is its to inherit.
    pub fn disconnected(&mut self) {
        let held: Vec<u64> = self.browser.held().iter().map(|one| one.id).collect();
        self.intercepts.clear();
        self.apply_intercepts();
        // The gate is down, but these were stopped while it was up and nothing
        // will ask for them again. Failed rather than left: a page waiting on a
        // request nobody holds any more is a page that never finishes.
        for id in held {
            self.browser.fail_request(id, "the driver went away");
        }
        self.blocked.clear();
        self.waiting.clear();
        // What the next client has been told is nothing, whatever this one heard.
        self.events.clear();
        self.known.clear();
        self.announced.clear();
        self.completed.clear();
        self.open = false;
    }

    fn dispatch_now(&mut self, command: &Command) -> Result<Value, Error> {
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
                if let Some(window) = self.window.as_mut() {
                    window.resize(
                        &mut self.browser,
                        otlyra_platform::Viewport::new(self.viewport.0, self.viewport.1, 1.0),
                    );
                }
                self.prepare_frame();
                Ok(json!({}))
            }
            // All three of these are started in `begin` and answered out of
            // `resolve`. Reaching them here would mean a caller went round the
            // front door.
            "browsingContext.navigate"
            | "browsingContext.reload"
            | "browsingContext.traverseHistory" => Err(Error {
                code: "unknown error",
                message: format!("{} is started rather than dispatched", command.method),
            }),
            "browsingContext.captureScreenshot" => {
                self.check_context(command)?;
                // The one place waiting for a picture or a font is right: this
                // command *is* the request for a settled frame. The loop's own
                // turns no longer wait, so a driver that never asks for a picture
                // never pays for one — and this wait gives up on anything the
                // gate is holding, because releasing that would be this thread's
                // own job.
                self.prepare_frame();
                let png = match self.clip_of(command)? {
                    Some(clip) => self
                        .browser
                        .screenshot_clipped(self.viewport(), clip)
                        .map_err(|message| Error {
                            code: "unable to capture screen",
                            message,
                        })?,
                    None => self.picture()?,
                };
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
                self.settle_window();
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
            "otlyra:captureWindow" => {
                self.driven_window()?;
                self.settle_window();
                let window = self.driven_window()?;
                let damage = window.damage();
                let frames = window.frames();
                let png = self.picture()?;
                Ok(json!({
                    "data": base64(&png),
                    "damage": damage_json(damage),
                    "frames": frames,
                }))
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
            "otlyra:readPage" => {
                self.check_context(command)?;
                self.read_page()
            }
            "otlyra:snapshot" => {
                self.check_context(command)?;
                self.snapshot(command)
            }
            "otlyra:waitFor" => {
                self.check_context(command)?;
                self.wait_for(command)
            }
            "otlyra:console" => Ok(json!({ "entries": self.console() })),
            "otlyra:network" => Ok(json!({
                "requests": self
                    .browser
                    .exchanges()
                    .iter()
                    .map(exchange_json)
                    .collect::<Vec<_>>(),
            })),

            // --- network interception --------------------------------------
            "network.addIntercept" => self.add_intercept(command),
            "network.removeIntercept" => self.remove_intercept(command),
            "network.continueRequest" => self.continue_request(command),
            "network.provideResponse" => self.provide_response(command),
            "network.failRequest" => self.fail_request(command),
            "network.continueResponse" | "network.continueWithAuth" => Err(Error::not_yet(
                &command.method,
                "a response phase, which needs a loader that hands a response back \
                 in two pieces",
            )),

            // --- storage ---------------------------------------------------
            //
            // There is one jar and it is the browser's, not a context's: this
            // engine has no second user context to keep a second one in. So the
            // `partition` a client sends is answered with the one partition there
            // is rather than refused, which is what a client that sent the
            // default would expect anyway.
            "storage.getCookies" => self.get_cookies(command),
            "storage.setCookie" => self.set_cookie(command),
            "storage.deleteCookies" => self.delete_cookies(command),

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
        // Held requests first, and whether or not the client subscribed: a
        // driver that asked for an intercept has said it wants these, and a
        // request stopped with nobody told about it is a page that hangs.
        if !self.intercepts.is_empty() {
            events.extend(self.held_events());
        }
        if self.subscribed("network.beforeRequestSent")
            || self.subscribed("network.responseCompleted")
        {
            events.extend(self.network_events());
        }
        events.extend(self.context_events());
        events
    }

    /// Requests being held that this client has not been told about.
    ///
    /// Reported as `beforeRequestSent` with `isBlocked`, which is the
    /// specification's way of saying *this one is stopped and waiting for you*.
    /// Told once: a held request stays held until a command releases it, and
    /// repeating it every turn of the loop would be a driver drowning in the
    /// news that nothing has changed.
    fn held_events(&mut self) -> Vec<Value> {
        let context = context_name(self.browser.active_id());
        let held: Vec<(u64, String, &'static str)> = self
            .browser
            .held()
            .iter()
            .filter(|held| !self.blocked.contains(&held.id))
            .map(|held| (held.id, held.url.clone(), held.method))
            .collect();

        let mut events = Vec::new();
        for (id, url, method) in held {
            self.blocked.insert(id);
            let intercepts: Vec<String> = self
                .intercepts
                .iter()
                .filter(|one| one.matches(&url))
                .map(|one| one.id.clone())
                .collect();
            events.push(event(
                "network.beforeRequestSent",
                json!({
                    "context": context,
                    "isBlocked": true,
                    "intercepts": intercepts,
                    "navigation": Value::Null,
                    "redirectCount": 0,
                    "timestamp": now(),
                    "request": {
                        "request": id.to_string(),
                        "url": url,
                        "method": method,
                        "headers": [],
                        "cookies": [],
                        "bodySize": 0,
                        "headersSize": 0,
                        "timings": {},
                    },
                    "initiator": { "type": "other" },
                }),
            ));
        }
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

    /// What has happened to the tabs since this client was last told.
    ///
    /// Diffed rather than pushed, for the reason every other event here is: the
    /// browser is driven from one thread and what it has is readable, so the
    /// protocol stays at the edge instead of reaching into `new_tab` and into the
    /// loader.
    ///
    /// # The lifecycle, per tab
    ///
    /// Three events, and each one is a *transition* rather than a state, which is
    /// why the last thing said about each tab is kept: a client is told a document
    /// became ready, not that it is ready, and a poll that saw the same state
    /// twice must not say it twice.
    ///
    /// - `navigationStarted` — this tab began going somewhere new.
    /// - `domContentLoaded` — its document is parsed and laid out. Everything
    ///   still outstanding is a stylesheet or a picture, and a driver that only
    ///   needs to click something can act on this one.
    /// - `load` — nothing is outstanding.
    ///
    /// All three carry the navigation they belong to, so a client that started
    /// two navigations can tell which finished.
    fn context_events(&mut self) -> Vec<Value> {
        let open: Vec<(crate::browser::TabId, Seen)> = self
            .browser
            .tabs()
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                (
                    tab.id,
                    Seen {
                        url: tab.url.clone(),
                        navigation: tab.navigation,
                        readiness: self.browser.readiness(index),
                    },
                )
            })
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

        for (index, (id, now_at)) in open.iter().enumerate() {
            let before = self.known.get(id);
            let lifecycle = |method: &str, state: &Seen| {
                let mut payload = json!({
                    "context": context_name(*id),
                    "navigation": state.navigation.map(|id| id.to_string()),
                    "timestamp": now(),
                    "url": state.url,
                });
                payload["userContext"] = json!("default");
                event(method, payload)
            };

            // A new navigation is one this client has not been told the name of.
            let started = now_at.navigation.is_some()
                && before.is_none_or(|before| before.navigation != now_at.navigation);
            if started && self.subscribed("browsingContext.navigationStarted") {
                events.push(lifecycle("browsingContext.navigationStarted", now_at));
            }

            // A readiness this navigation had not reached when the client was
            // last told. A new navigation resets what it had reached, which is
            // what makes going somewhere else report its own ready and load
            // rather than staying silent because the tab before it was complete.
            //
            // A tab that has never navigated reports neither, however complete
            // it is: a blank tab has not loaded a document, and saying it did
            // would have a client waiting for the *next* one hear about this one.
            let reached = |state: Readiness| {
                now_at.navigation.is_some()
                    && now_at.readiness >= state
                    && (started || before.is_none_or(|before| before.readiness < state))
            };
            if reached(Readiness::Interactive)
                && self.subscribed("browsingContext.domContentLoaded")
            {
                events.push(lifecycle("browsingContext.domContentLoaded", now_at));
            }
            if reached(Readiness::Complete) && self.subscribed("browsingContext.load") {
                events.push(lifecycle("browsingContext.load", now_at));
            }
            let _ = index;
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
        self.prepare_frame();
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
    /// What rectangle a screenshot was asked for, if it named one.
    ///
    /// The specification's two: a box in the page's own coordinates, and an
    /// element — which is answered from where the last frame actually *drew* it,
    /// the same rectangle a click is tested against. That is the point of naming
    /// an element rather than a box: the caller does not have to know the layout
    /// and cannot disagree with it.
    fn clip_of(&mut self, command: &Command) -> Result<Option<otlyra_layout::Rect>, Error> {
        let Some(clip) = command.params.get("clip") else {
            return Ok(None);
        };
        match clip.get("type").and_then(Value::as_str) {
            Some("box") => {
                let number = |name: &str| {
                    clip.get(name)
                        .and_then(Value::as_f64)
                        .ok_or_else(|| Error::invalid(format!("a box clip needs a {name}")))
                };
                let (x, y, width, height) = (
                    number("x")?,
                    number("y")?,
                    number("width")?,
                    number("height")?,
                );
                if width <= 0.0 || height <= 0.0 {
                    return Err(Error::invalid(
                        "a clip needs a width and a height above zero",
                    ));
                }
                Ok(Some(otlyra_layout::Rect::new(
                    x as f32,
                    y as f32,
                    width as f32,
                    height as f32,
                )))
            }
            Some("element") => {
                let shared = clip
                    .get("element")
                    .and_then(|element| element.get("sharedId"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::invalid("an element clip needs a sharedId"))?;
                let node = node_of(shared)
                    .ok_or_else(|| Error::no_such_node(&format!("{shared} names no node")))?;
                // A frame first: an element that has not been drawn has no
                // rectangle, and one drawn before the last command is not where
                // it is now.
                self.prepare_frame();
                let facts = self
                    .browser
                    .box_facts(node)
                    .ok_or_else(|| Error::no_such_node(&format!("{shared} was not drawn")))?;
                let border = facts.border;
                Ok(Some(otlyra_layout::Rect::new(
                    border.x as f32,
                    border.y as f32,
                    (border.width as f32).max(1.0),
                    (border.height as f32).max(1.0),
                )))
            }
            other => Err(Error::invalid(format!(
                "{:?} is not a clip type: it is box or element",
                other.unwrap_or_default()
            ))),
        }
    }

    /// The page as prose.
    ///
    /// A caller with no eyes and no script engine still has to be able to answer
    /// *what does this page say*, and the two ways it could have got there are
    /// both worse: a picture costs a picture per turn and cannot be read back,
    /// and a DOM dump is markup rather than content. This is the accessibility
    /// tree read as Markdown — see [`crate::digest`] for why that tree and not a
    /// walk of the document.
    fn read_page(&mut self) -> Result<Value, Error> {
        // A frame first: the tree is built from the *boxes*, so a page that has
        // not been laid out since it arrived has nothing to describe.
        self.prepare_frame();
        let page = self
            .browser
            .active_page()
            .ok_or_else(|| Error::no_such_node("nothing is loaded in this context"))?;
        let title = crate::page::title_of(page.document());
        let items = crate::a11y::describe_page(page);
        Ok(json!({
            "url": self.browser.url(),
            "title": title,
            "text": crate::digest::text(&items, title.as_deref()),
        }))
    }

    /// Everything on the page that can be named, and which of it can be acted on.
    ///
    /// The companion to [`Session::read_page`]: that one answers *what does this
    /// say*, this one answers *what can I do here*, and both come off the one
    /// tree. Every row carries the handle every other command takes, so a caller
    /// goes from reading the page to clicking on it without ever having guessed
    /// a selector — which is the step an agent gets wrong.
    fn snapshot(&mut self, command: &Command) -> Result<Value, Error> {
        let filter = crate::digest::Filter {
            interactive_only: command
                .params
                .get("interactiveOnly")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            max_depth: command
                .params
                .get("maxDepth")
                .and_then(Value::as_u64)
                .map(|depth| depth as usize),
        };

        self.prepare_frame();
        let page = self
            .browser
            .active_page()
            .ok_or_else(|| Error::no_such_node("nothing is loaded in this context"))?;
        let rows = crate::digest::outline(&crate::a11y::describe_page(page), filter);
        Ok(json!({
            "url": self.browser.url(),
            "title": crate::page::title_of(page.document()),
            "nodes": rows.iter().map(row_json).collect::<Vec<_>>(),
        }))
    }

    /// Wait until the page is in the state a caller is waiting for.
    ///
    /// # What waiting can and cannot mean here
    ///
    /// With no script engine, nothing changes a loaded document. So a wait for a
    /// selector is a wait for the *load*, and then one look: a selector that is
    /// not there when the load finished will not appear later, and a command that
    /// slept five seconds to discover that would be five seconds of an agent's
    /// time spent proving something already known. The timeout is therefore spent
    /// on the load and not on a poll, and the answer says which it was.
    ///
    /// This is the one command here whose meaning will change at M12, and it is
    /// written so the change is additive: the shape of the answer is already
    /// *found or not, and how long it took*.
    fn wait_for(&mut self, command: &Command) -> Result<Value, Error> {
        let timeout = command
            .params
            .get("timeout")
            .and_then(Value::as_u64)
            .map_or(LOAD_TIMEOUT, std::time::Duration::from_millis);
        let started = std::time::Instant::now();
        self.browser.wait_for_load(timeout);
        self.prepare_frame();

        let selector = command
            .params
            .get("locator")
            .and_then(|locator| locator.get("value"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let matched = match &selector {
            None => None,
            Some(selector) => {
                let page = self
                    .browser
                    .active_page()
                    .ok_or_else(|| Error::no_such_node("nothing is loaded in this context"))?;
                Some(
                    otlyra_css::stylo_dom::select(page.document(), selector).map_err(|error| {
                        Error::invalid(format!("{selector:?} is not a selector: {error}"))
                    })?,
                )
            }
        };

        Ok(json!({
            "loading": self.browser.tabs().iter().any(crate::browser::Tab::loading),
            "took": started.elapsed().as_secs_f64() * 1000.0,
            "found": matched.as_ref().map_or(Value::Null, |nodes| json!(!nodes.is_empty())),
            "nodes": matched.map_or_else(Vec::new, |nodes| {
                let document = self.browser.active_page().map(crate::page::PageScene::document);
                document.map_or_else(Vec::new, |document| {
                    nodes.into_iter().map(|node| node_value(document, node)).collect()
                })
            }),
        }))
    }

    /// What the browser has said about itself, as a list rather than as events.
    ///
    /// The same records `log.entryAdded` carries, for a caller that asks rather
    /// than subscribes — which is every caller over a request-and-answer
    /// transport, and so every agent. Read from the same journal, so the two
    /// cannot disagree.
    fn console(&self) -> Vec<Value> {
        crate::observability::journal()
            .records()
            .into_iter()
            .map(|record| {
                json!({
                    "level": record.level.as_str().to_lowercase(),
                    "source": record.target,
                    "text": record.message,
                })
            })
            .collect()
    }

    /// Start holding requests this client wants to see before they are sent.
    fn add_intercept(&mut self, command: &Command) -> Result<Value, Error> {
        intercept::check_phases(command.params.get("phases"))?;
        let patterns = match command.params.get("urlPatterns") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(patterns)) => patterns
                .iter()
                .map(intercept::Pattern::parse)
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(Error::invalid("urlPatterns has to be a list")),
        };

        self.next_intercept += 1;
        let id = format!("otlyra-intercept-{}", self.next_intercept);
        self.intercepts.push(intercept::Intercept {
            id: id.clone(),
            patterns,
        });
        self.apply_intercepts();
        Ok(json!({ "intercept": id }))
    }

    /// Stop holding what one intercept was about.
    fn remove_intercept(&mut self, command: &Command) -> Result<Value, Error> {
        let id = command.string("intercept")?.to_owned();
        let before = self.intercepts.len();
        self.intercepts.retain(|one| one.id != id);
        if self.intercepts.len() == before {
            return Err(Error {
                code: "no such intercept",
                message: format!("{id} names no intercept of this session's"),
            });
        }
        self.apply_intercepts();
        Ok(json!({}))
    }

    /// Hand the browser the question *should this be held*, as one predicate.
    ///
    /// Rebuilt whenever the list changes rather than consulted through a shared
    /// handle: the gate runs on the browser's own thread inside `fetch`, and a
    /// lock there would put the protocol's state on the loading path.
    fn apply_intercepts(&mut self) {
        if self.intercepts.is_empty() {
            // Nothing held, so a driver that removed its last intercept — or went
            // away — leaves a browser that loads pages.
            self.browser.hold_requests(None);
            return;
        }
        let intercepts = self.intercepts.clone();
        self.browser.hold_requests(Some(Box::new(move |url: &str| {
            intercepts.iter().any(|one| one.matches(url))
        })));
    }

    /// The held request a command names, checked before anything is done to it.
    fn held_named(&self, command: &Command) -> Result<u64, Error> {
        let name = command.string("request")?;
        let id = name.parse::<u64>().map_err(|_| Error {
            code: "no such request",
            message: format!("{name} is not a request handle"),
        })?;
        if !self.browser.held().iter().any(|held| held.id == id) {
            // A driver working from a stale list, which is worth saying: doing
            // nothing quietly would have it wait for a load that already went.
            return Err(Error {
                code: "no such request",
                message: format!("{name} is not a request being held"),
            });
        }
        Ok(id)
    }

    /// Let a held request go, with whatever the driver changed about it.
    fn continue_request(&mut self, command: &Command) -> Result<Value, Error> {
        let id = self.held_named(command)?;
        // Cookies are not a driver's to write. The jar decides what a site is
        // entitled to be sent, and a command that could set the header directly
        // could send one site's session to another — which is the one thing an
        // automation protocol must not let a page's own script do either.
        if command.params.get("cookies").is_some() {
            return Err(Error::not_yet(
                "continueRequest with cookies",
                "a way to write the cookie header that the jar still decides, \
                 which would be a cookie jar with a second door",
            ));
        }
        let change = crate::fetcher::Change {
            headers: command
                .params
                .get("headers")
                .and_then(Value::as_array)
                .map(|headers| {
                    headers
                        .iter()
                        .filter_map(|header| {
                            let name = header.get("name").and_then(Value::as_str)?;
                            let value = header.get("value").and_then(body_text)?;
                            Some((name.to_owned(), value))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            url: command
                .params
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_owned),
            body: command
                .params
                .get("body")
                .and_then(body_of)
                .map(|bytes| otlyra_net::Body {
                    content_type: "application/octet-stream".to_owned(),
                    bytes,
                }),
        };
        self.browser.resume_request(id, change);
        self.blocked.remove(&id);
        Ok(json!({}))
    }

    /// Answer a held request with a response nobody sent.
    fn provide_response(&mut self, command: &Command) -> Result<Value, Error> {
        let id = self.held_named(command)?;
        let headers: Vec<(String, String)> = command
            .params
            .get("headers")
            .and_then(Value::as_array)
            .map(|headers| {
                headers
                    .iter()
                    .filter_map(|header| {
                        let name = header.get("name").and_then(Value::as_str)?;
                        let value = header.get("value").and_then(body_text)?;
                        Some((name.to_owned(), value))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let content_type = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone());
        let url = self
            .browser
            .held()
            .iter()
            .find(|held| held.id == id)
            .map(|held| held.url.clone())
            .unwrap_or_default();

        let response = crate::fetcher::Loaded {
            bytes: command
                .params
                .get("body")
                .and_then(body_of)
                .unwrap_or_default(),
            charset: None,
            content_type,
            nosniff: false,
            // A response with no status named is a `200`: a driver writing one
            // by hand means *this worked*, and making it say so is a required
            // field nobody would ever vary.
            status: Some(
                command
                    .params
                    .get("statusCode")
                    .and_then(Value::as_u64)
                    .unwrap_or(200) as u16,
            ),
            request_headers: Vec::new(),
            response_headers: headers,
            final_url: url,
            // Written rather than fetched, and rather than cached. Saying
            // `network` would have the request list claim a socket was opened.
            served: otlyra_net::Served::Network,
        };
        self.browser.fulfil_request(id, response);
        self.blocked.remove(&id);
        Ok(json!({}))
    }

    /// Stop a held request, which is what blocking one means.
    fn fail_request(&mut self, command: &Command) -> Result<Value, Error> {
        let id = self.held_named(command)?;
        self.browser.fail_request(id, "blocked by a driver");
        self.blocked.remove(&id);
        Ok(json!({}))
    }

    /// The cookies the jar is holding, filtered the way the specification says.
    fn get_cookies(&mut self, command: &Command) -> Result<Value, Error> {
        let filter = command.params.get("filter");
        let want = |key: &str| {
            filter
                .and_then(|filter| filter.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let (name, domain, path) = (want("name"), want("domain"), want("path"));

        let cookies: Vec<Value> = self.browser.cookies().with(|jar| {
            jar.all()
                .iter()
                .filter(|cookie| {
                    name.as_ref().is_none_or(|name| &cookie.name == name)
                        && domain.as_ref().is_none_or(|domain| {
                            // A host-only cookie matches its host and a domain
                            // cookie matches any host under it, which is the same
                            // rule that decides whether it is sent.
                            cookie.domain == *domain
                                || (!cookie.host_only
                                    && domain.ends_with(&format!(".{}", cookie.domain)))
                        })
                        && path.as_ref().is_none_or(|path| &cookie.path == path)
                })
                .map(cookie_json)
                .collect()
        });
        Ok(json!({ "cookies": cookies, "partitionKey": { "userContext": "default" } }))
    }

    /// Put one cookie in the jar, as if a response had set it.
    ///
    /// Through the jar's own storage rules rather than around them: a driver that
    /// could write a cookie the browser would never have accepted could put the
    /// page in a state no server can produce, which is the one thing an
    /// automation protocol must not allow.
    fn set_cookie(&mut self, command: &Command) -> Result<Value, Error> {
        let cookie = command
            .params
            .get("cookie")
            .ok_or_else(|| Error::invalid("setCookie needs a cookie"))?;
        let text = |key: &str| cookie.get(key).and_then(Value::as_str);
        let name = text("name").ok_or_else(|| Error::invalid("a cookie needs a name"))?;
        let domain = text("domain").ok_or_else(|| Error::invalid("a cookie needs a domain"))?;
        // The specification carries a value as `{type, value}` so it can also be
        // sent base64; the string form is what every client actually sends.
        let value = cookie
            .get("value")
            .and_then(|value| {
                value
                    .get("value")
                    .and_then(Value::as_str)
                    .or(value.as_str())
            })
            .ok_or_else(|| Error::invalid("a cookie needs a value"))?;

        let mut line = format!("{name}={value}");
        line.push_str(&format!("; Domain={domain}"));
        line.push_str(&format!("; Path={}", text("path").unwrap_or("/")));
        if cookie.get("secure").and_then(Value::as_bool) == Some(true) {
            line.push_str("; Secure");
        }
        if cookie.get("httpOnly").and_then(Value::as_bool) == Some(true) {
            line.push_str("; HttpOnly");
        }
        if let Some(same_site) = text("sameSite") {
            line.push_str(&format!("; SameSite={same_site}"));
        }
        if let Some(expiry) = cookie.get("expiry").and_then(Value::as_u64) {
            line.push_str(&format!("; Max-Age={}", expiry.saturating_sub(now())));
        }

        // Stored against the cookie's own domain, because there is no response
        // here to have arrived from one.
        let scheme = if cookie.get("secure").and_then(Value::as_bool) == Some(true) {
            "https"
        } else {
            "http"
        };
        let host = domain.trim_start_matches('.');
        let url = otlyra_net::url::normalize(&format!("{scheme}://{host}/"))
            .map_err(|error| Error::invalid(format!("{domain:?} is not a domain: {error}")))?;

        self.browser
            .cookies_mut()
            .with(|jar| jar.set(&url, &line, std::time::SystemTime::now()))
            .map_err(|refused| Error::invalid(format!("the jar refused it: {refused:?}")))?;
        Ok(json!({ "partitionKey": { "userContext": "default" } }))
    }

    /// Take cookies out of the jar. Answers how many went.
    fn delete_cookies(&mut self, command: &Command) -> Result<Value, Error> {
        let filter = command.params.get("filter");
        let want = |key: &str| {
            filter
                .and_then(|filter| filter.get(key))
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        let (name, domain, path) = (want("name"), want("domain"), want("path"));
        // A delete with no filter at all empties the jar, which is what the
        // specification says and what a driver clearing state between runs means.
        let removed = self.browser.cookies_mut().with(|jar| {
            jar.remove(|cookie| {
                name.as_ref().is_none_or(|name| &cookie.name == name)
                    && domain
                        .as_ref()
                        .is_none_or(|domain| &cookie.domain == domain)
                    && path.as_ref().is_none_or(|path| &cookie.path == path)
            })
        });
        Ok(json!({
            "removed": removed,
            "partitionKey": { "userContext": "default" },
        }))
    }

    /// Find nodes, by any of the three ways the specification has of naming one.
    ///
    /// `css` is the selector engine and `xpath` is [`otlyra_dom::xpath`]. The
    /// other two are questions about what a page *presents* rather than about how
    /// it is written — the words a person would click on, and the role and name a
    /// reader would announce — and both are answered from the accessibility tree,
    /// which is the browser's one account of that.
    fn locate(&mut self, command: &Command) -> Result<Value, Error> {
        let locator = command
            .params
            .get("locator")
            .ok_or_else(|| Error::invalid("locateNodes needs a locator"))?
            .clone();
        let kind = locator
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("css")
            .to_owned();
        let limit = command
            .params
            .get("maxNodeCount")
            .and_then(Value::as_u64)
            .map_or(usize::MAX, |count| count as usize);

        // The two presentational locators need the boxes, and the boxes need a
        // frame. Done before the borrow below rather than inside it.
        if kind != "css" {
            self.prepare_frame();
        }

        let page = self
            .browser
            .active_page()
            .ok_or_else(|| Error::no_such_node("nothing is loaded in this context"))?;
        let document = page.document();

        let matched: Vec<otlyra_dom::NodeId> = match kind.as_str() {
            "css" => {
                let selector = locator
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::invalid("a css locator needs a value"))?;
                otlyra_css::stylo_dom::select(document, selector).map_err(|error| {
                    Error::invalid(format!("{selector:?} is not a selector: {error}"))
                })?
            }
            "innerText" => {
                let wanted = locator
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::invalid("an innerText locator needs a value"))?;
                // The specification's own defaults: an exact match, case
                // sensitive, unless the client says otherwise.
                let partial = locator.get("matchType").and_then(Value::as_str) == Some("partial");
                let fold = locator.get("ignoreCase").and_then(Value::as_bool) == Some(true);
                let depth = locator
                    .get("maxDepth")
                    .and_then(Value::as_u64)
                    .map(|depth| depth as usize);

                crate::digest::outline(
                    &crate::a11y::describe_page(page),
                    crate::digest::Filter {
                        interactive_only: false,
                        max_depth: depth,
                    },
                )
                .into_iter()
                .filter(|row| {
                    row.name
                        .as_deref()
                        .is_some_and(|name| matches_text(name, wanted, partial, fold))
                })
                .filter_map(|row| row.node)
                .collect()
            }
            "accessibility" => {
                let value = locator
                    .get("value")
                    .ok_or_else(|| Error::invalid("an accessibility locator needs a value"))?;
                let role = value.get("role").and_then(Value::as_str);
                let name = value.get("name").and_then(Value::as_str);
                if role.is_none() && name.is_none() {
                    return Err(Error::invalid(
                        "an accessibility locator needs a role, a name, or both",
                    ));
                }
                crate::digest::outline(
                    &crate::a11y::describe_page(page),
                    crate::digest::Filter::default(),
                )
                .into_iter()
                .filter(|row| {
                    // A role is compared with the underscores a client sends
                    // rather than the words a reader speaks: `list item` here is
                    // `listitem` in ARIA.
                    role.is_none_or(|role| same_role(row.role, role))
                        && name.is_none_or(|name| row.name.as_deref() == Some(name))
                })
                .filter_map(|row| row.node)
                .collect()
            }
            "xpath" => {
                let expression = locator
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::invalid("an xpath locator needs a value"))?;
                otlyra_dom::xpath::select(document, expression).map_err(|error| {
                    // An expression that does not parse is the client's mistake
                    // and is named as one, with the place in it: an empty list
                    // would have read as *nothing on this page matched*.
                    Error::invalid(format!("{expression:?} is not valid XPath: {error}"))
                })?
            }
            other => {
                // Saying which locator is missing beats a silent empty list,
                // which reads as *nothing matched*.
                return Err(Error::not_yet(
                    &format!("locateNodes with a {other} locator"),
                    "a locator this implementation does not have yet",
                ));
            }
        };

        let nodes: Vec<Value> = matched
            .into_iter()
            .take(limit)
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
        use otlyra_platform::PlatformEvent;

        let kind = action
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid("an action needs a type"))?;

        match (source, kind) {
            (_, "pause") => Ok(()),
            ("pointer", "pointerMove") => {
                let (x, y) = self.point_of(action)?;
                self.deliver(PlatformEvent::PointerMoved { x, y });
                Ok(())
            }
            ("pointer", "pointerDown") => {
                // A driver's press is always a fresh single click: the protocol
                // has no click count, and a double-click arrives as two presses
                // the *page* may interpret, not something to synthesise here.
                self.deliver(PlatformEvent::PointerPressed { clicks: 1 });
                Ok(())
            }
            ("pointer", "pointerUp") => {
                self.deliver(PlatformEvent::PointerReleased);
                Ok(())
            }
            ("wheel", "scroll") => {
                let (x, y) = self.point_of(action)?;
                self.deliver(PlatformEvent::PointerMoved { x, y });
                let delta =
                    |name: &str| action.get(name).and_then(Value::as_f64).unwrap_or_default();
                self.deliver(PlatformEvent::Scroll {
                    x: delta("deltaX"),
                    y: delta("deltaY"),
                    source: otlyra_platform::ScrollSource::Wheel,
                    modifiers: Default::default(),
                });
                Ok(())
            }
            ("key", "keyDown") => {
                let value = action
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::invalid("keyDown needs a value"))?;
                for event in key_events(value) {
                    self.deliver(event);
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

    /// Deliver one platform event the way the thing being driven delivers it.
    ///
    /// A window session hands it to the window, which asks the browser what
    /// frame it wants — so an action that changes nothing draws nothing, exactly
    /// as it does for a person. A page session has no window to ask, and hands
    /// the event straight to the browser.
    fn deliver(&mut self, event: otlyra_platform::PlatformEvent) {
        match self.window.as_mut() {
            Some(window) => {
                window.event(&mut self.browser, event);
            }
            None => {
                use otlyra_platform::Painter;
                self.browser.on_event(event);
            }
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

    /// Settle everything a frame asks for, and draw one.
    ///
    /// For the commands that change what the page *is* — a navigation, a reload,
    /// a new viewport. The offscreen frame is what makes a background picture and
    /// a web font be asked for and waited on; the window then draws what arrived.
    fn prepare_frame(&mut self) {
        self.browser.prepare_frame(self.viewport(), LOAD_TIMEOUT);
        if let Some(window) = self.window.as_mut()
            && let Err(error) = window.frame(&mut self.browser)
        {
            tracing::error!(%error, "the driven window could not draw a frame");
        }
    }

    /// Draw a frame from what has arrived, waiting for nothing.
    ///
    /// What the command loop uses. [`Self::prepare_frame`] waits on the network
    /// for the pictures and fonts a frame asked for, and this thread is the one
    /// that reads commands — including the command that would release the very
    /// request it is waiting on. Settling belongs to the commands that mean
    /// *give me a finished picture*, and nowhere else.
    fn draw_frame(&mut self) {
        self.browser.draw_frame(self.viewport());
        if let Some(window) = self.window.as_mut()
            && let Err(error) = window.frame(&mut self.browser)
        {
            tracing::error!(%error, "the driven window could not draw a frame");
        }
    }

    /// Draw the frames the browser asked for after an interaction, and nothing
    /// else.
    ///
    /// Deliberately *not* [`Self::prepare_frame`]: an offscreen paint would build
    /// every list again and hand the compositor a frame it had to redraw whole,
    /// which is exactly the kind of frame that hides a missing invalidation. What
    /// this draws is what the window would have drawn — including nothing at all,
    /// when the browser says the interaction changed nothing.
    fn settle_window(&mut self) {
        let Some(window) = self.window.as_mut() else {
            self.browser.prepare_frame(self.viewport(), LOAD_TIMEOUT);
            return;
        };
        if let Err(error) = window.settle(&mut self.browser) {
            tracing::error!(%error, "the driven window could not settle");
        }
    }

    /// A picture of what this session is driving, as a PNG.
    ///
    /// The window's own composited surface when there is a window, and one
    /// offscreen paint of the page when there is not.
    fn picture(&mut self) -> Result<Vec<u8>, Error> {
        let viewport = self.viewport();
        match self.window.as_mut() {
            Some(window) => window.png().map_err(|error| Error {
                code: "unable to capture screen",
                message: error.to_string(),
            }),
            None => self.browser.screenshot(viewport).map_err(|error| Error {
                code: "unable to capture screen",
                message: error,
            }),
        }
    }

    /// The window this session drives, or a refusal naming what it would take.
    fn driven_window(&mut self) -> Result<&mut otlyra_platform::FramePump, Error> {
        self.window
            .as_mut()
            .ok_or_else(|| Error::not_yet("otlyra:captureWindow", "a session driving a window"))
    }
}

/// What a composited frame redrew, as a client reads it.
///
/// A rectangle where one region moved, `null` where the frame changed nothing,
/// and `"full"` where the whole surface was redrawn. Worth reporting because it
/// is the difference between *the model changed* and *the screen changed*: a
/// driver that clicks away from a field and sees no damage has found a bug that
/// no amount of asking the browser about its own state would show.
fn damage_json(damage: otlyra_platform::Damage) -> Value {
    match damage {
        otlyra_platform::Damage::Unchanged => Value::Null,
        otlyra_platform::Damage::Full => json!("full"),
        otlyra_platform::Damage::Region(rect) => json!({
            "x": rect.x,
            "y": rect.y,
            "width": rect.width,
            "height": rect.height,
        }),
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
                // The specification's own field, and it was hardcoded to `false`
                // — which made a cache that worked indistinguishable from one
                // that did nothing, for a client and for whoever wrote it.
                "fromCache": exchange.served != otlyra_net::Served::Network,
                "headers": headers_json(&exchange.response_headers),
                "mimeType": exchange.content_type.clone().map_or(Value::Null, Value::from),
                "protocol": Value::Null,
                "content": { "size": bytes },
            },
            // Two numbers, because they answer different questions: how slow the
            // transport was, and how long the request waited for a thread.
            "otlyra:took": exchange.took.map(|took| took.as_secs_f64() * 1000.0),
            "otlyra:waited": exchange.waited.map(|waited| waited.as_secs_f64() * 1000.0),
            // `fromCache` cannot tell a hit apart from a revalidation, and the
            // difference is the most useful thing a cache does: a request that
            // asked and was told nothing changed still crossed the network.
            "otlyra:served": served_word(exchange.served),
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

/// Whether a node's text is what an `innerText` locator asked for.
fn matches_text(name: &str, wanted: &str, partial: bool, fold: bool) -> bool {
    // Whitespace is collapsed on both sides: the words a page presents are what
    // is being matched, and a line break in the markup is not one of them.
    let tidy = |text: &str| {
        let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if fold { joined.to_lowercase() } else { joined }
    };
    let (name, wanted) = (tidy(name), tidy(wanted));
    if partial {
        name.contains(&wanted)
    } else {
        name == wanted
    }
}

/// Whether the word a reader uses for a role is the one a client asked for.
///
/// A client sends ARIA's spelling — `listitem`, `checkbox` — and the tree holds
/// the words a reader speaks, which have spaces in them. Compared with the spaces
/// taken out rather than with a table of both spellings, which would be a second
/// list to keep in step with the first.
fn same_role(spoken: &str, asked: &str) -> bool {
    let bare = |role: &str| role.replace([' ', '-', '_'], "").to_lowercase();
    bare(spoken) == bare(asked)
}

/// One row of a snapshot, as a client reads it.
///
/// `sharedId` is the same handle `locateNodes` hands back, which is the whole
/// point of the command: what a caller reads and what it acts on are one name.
fn row_json(row: &crate::digest::Row) -> Value {
    let mut value = json!({
        "depth": row.depth,
        "role": row.role,
        "interactive": row.interactive,
    });
    if let Some(node) = row.node {
        value["sharedId"] = json!(shared_id(node));
    }
    if let Some(name) = &row.name {
        value["name"] = json!(name);
    }
    if let Some(text) = &row.value {
        value["value"] = json!(text);
    }
    if let Some(url) = &row.url {
        value["url"] = json!(url);
    }
    if let Some((x, y, width, height)) = row.bounds {
        value["bounds"] = json!({ "x": x, "y": y, "width": width, "height": height });
    }
    if row.disabled {
        value["disabled"] = json!(true);
    }
    if let Some(checked) = row.checked {
        value["checked"] = json!(checked);
    }
    value
}

/// One cookie, spelled the way the specification spells one.
fn cookie_json(cookie: &otlyra_net::cookie::Cookie) -> Value {
    json!({
        "name": cookie.name,
        "value": { "type": "string", "value": cookie.value },
        "domain": cookie.domain,
        "path": cookie.path,
        "size": cookie.name.len() + cookie.value.len(),
        "httpOnly": cookie.http_only,
        "secure": cookie.secure,
        "sameSite": format!("{:?}", cookie.same_site).to_lowercase(),
        "expiry": cookie.expires.and_then(|expires| {
            expires
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|since| since.as_secs())
        }),
    })
}

/// One request the browser made, as a list entry rather than as two events.
///
/// The same facts `network.beforeRequestSent` and `network.responseCompleted`
/// carry, in one object, for a caller that asks instead of subscribing. Headers
/// and bodies are left out: a list of forty requests with every header on each is
/// a wall, and the pane that wants them has them.
fn exchange_json(exchange: &crate::fetcher::Exchange) -> Value {
    use crate::fetcher::Status;

    json!({
        "id": exchange.id,
        "method": exchange.method,
        "url": exchange.url,
        "kind": format!("{:?}", exchange.kind).to_lowercase(),
        "status": exchange.code,
        "contentType": exchange.content_type,
        "state": match &exchange.status {
            Status::Pending => "pending",
            Status::Ok(_) => "complete",
            Status::Failed(_) => "failed",
        },
        "bytes": match &exchange.status {
            Status::Ok(bytes) => json!(bytes),
            _ => Value::Null,
        },
        "error": match &exchange.status {
            Status::Failed(why) => json!(why),
            _ => Value::Null,
        },
        "took": exchange.took.map(|took| took.as_secs_f64() * 1000.0),
        // What no timing can tell a caller: whether the network was touched.
        "served": served_word(exchange.served),
        "fromCache": exchange.served != otlyra_net::Served::Network,
    })
}

/// The bytes of a `BytesValue`, which the specification carries two ways.
///
/// `{type: "string", value}` for text and `{type: "base64", value}` for anything
/// else. A bare string is accepted too, because clients send one and refusing it
/// would be refusing to work over a shape nobody misunderstands.
fn body_of(value: &Value) -> Option<Vec<u8>> {
    if let Some(text) = value.as_str() {
        return Some(text.as_bytes().to_vec());
    }
    let inner = value.get("value").and_then(Value::as_str)?;
    match value.get("type").and_then(Value::as_str) {
        Some("base64") => unbase64(inner),
        _ => Some(inner.as_bytes().to_vec()),
    }
}

/// The same, as text, for a header value.
fn body_text(value: &Value) -> Option<String> {
    String::from_utf8(body_of(value)?).ok()
}

/// Standard base64, the other way round from [`base64`].
///
/// Written out for the same reason that one is: it is twenty lines, it is used
/// here and nowhere else, and a crate for it would be a crate to keep up to date
/// for as long as this program exists.
fn unbase64(text: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut held: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes() {
        if byte == b'=' || byte.is_ascii_whitespace() {
            continue;
        }
        let value = ALPHABET.iter().position(|one| *one == byte)? as u32;
        held = (held << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((held >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

/// Where a response came from, as a client reads it.
fn served_word(served: otlyra_net::Served) -> &'static str {
    match served {
        otlyra_net::Served::Network => "network",
        otlyra_net::Served::Cache => "cache",
        otlyra_net::Served::Revalidated => "revalidated",
    }
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

    #[test]
    fn a_session_with_no_window_says_so_rather_than_photographing_a_page() {
        let mut session = session();
        let refused = session
            .dispatch(&command(1, "otlyra:captureWindow", json!({})))
            .expect_err("a session driving no window cannot picture one");

        // Told apart from a screenshot on purpose: the two answer different
        // questions, and handing back the page here would be answering the one
        // that was not asked.
        assert_eq!(refused.code, "unsupported operation");
        assert!(refused.message.contains("driving a window"), "{refused:?}");
    }

    #[test]
    fn a_window_session_pictures_the_whole_window_and_says_what_it_redrew() {
        let mut session = Session::windowed(Browser::new(Pages), (400, 300));
        session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://driven.example/"}),
            ))
            .expect("navigated");
        let captured = session
            .dispatch(&command(2, "otlyra:captureWindow", json!({})))
            .expect("a window");

        assert!(
            captured["data"]
                .as_str()
                .is_some_and(|data| data.starts_with("iVBORw0KGgo"))
        );
        // A frame has been drawn, and the interface is still there: a window
        // session is the one session that does not hide it.
        assert!(captured["frames"].as_u64().is_some_and(|frames| frames > 0));
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
                json!({"locator": {"type": "context", "value": {"context": "x"}}}),
            ))
            .unwrap_err();
        // An empty list would have read as *nothing matched*, which is a
        // different fact and would send a driver looking at its selector.
        assert_eq!(error.code, "unsupported operation");
    }

    #[test]
    fn an_element_can_be_found_by_xpath() {
        let mut session = opened();
        let found = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "xpath", "value": "//p[@id='greeting']"}}),
            ))
            .expect("a match");

        let nodes = found["nodes"].as_array().expect("nodes");
        assert_eq!(nodes.len(), 1, "{found:#}");
        assert_eq!(nodes[0]["value"]["localName"], json!("p"));

        // The expression every driver writes, and the one a CSS selector cannot.
        let by_text = session
            .dispatch(&command(
                3,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "xpath", "value": "//p[text()='hello']"}}),
            ))
            .expect("a match");
        assert_eq!(by_text["nodes"].as_array().expect("nodes").len(), 1);
    }

    #[test]
    fn an_xpath_that_does_not_parse_says_where_rather_than_matching_nothing() {
        let mut session = opened();
        let error = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "xpath", "value": "//p["}}),
            ))
            .unwrap_err();
        assert_eq!(error.code, "invalid argument");
        assert!(error.message.contains("XPath"), "{}", error.message);
    }

    #[test]
    fn an_element_can_be_found_by_the_words_it_shows() {
        let mut session = opened();
        let found = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "innerText", "value": "hello"}}),
            ))
            .expect("a match");

        // The point of this locator: a caller that can read the page can find
        // what it read without having worked out a selector for it.
        let nodes = found["nodes"].as_array().expect("nodes");
        assert_eq!(nodes.len(), 1, "{found:#}");
        assert_eq!(nodes[0]["value"]["localName"], json!("p"));
    }

    #[test]
    fn an_element_can_be_found_by_the_role_and_name_a_reader_would_announce() {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://roles.example/"}),
            ))
            .expect("navigated");
        let found = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {
                    "type": "accessibility",
                    "value": {"role": "paragraph"},
                }}),
            ))
            .expect("a match");

        assert!(
            !found["nodes"].as_array().expect("nodes").is_empty(),
            "{found:#}"
        );
    }

    #[test]
    fn asking_for_at_most_one_node_gets_at_most_one() {
        let mut session = opened();
        let found = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "css", "value": "*"}, "maxNodeCount": 1}),
            ))
            .expect("a match");
        assert_eq!(found["nodes"].as_array().expect("nodes").len(), 1);
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
            .dispatch(&command(2, "browser.createUserContext", json!({})))
            .unwrap_err();
        assert_eq!(unknown.code, "unknown command");
    }

    /// A loader that takes long enough that a load is still in flight when the
    /// command that started it is answered.
    ///
    /// Without one of these, every test here would be a test of a load that had
    /// already finished — which is exactly the case that cannot tell a blocking
    /// navigation from a parked one.
    struct Slow;

    impl Loader for Slow {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            std::thread::sleep(std::time::Duration::from_millis(120));
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: b"<title>Slow</title><body><p>eventually".to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    /// A loader with enough on the page to read and to act on.
    struct Site;

    impl Loader for Site {
        fn load(&self, url: &str) -> Result<Loaded, String> {
            Ok(Loaded {
                content_type: Some("text/html".to_owned()),
                bytes: br#"<title>Catalogue</title><body>
                    <h1>Catalogue</h1>
                    <p>Two things we have.</p>
                    <ul><li>A thing</li><li>Another thing</li></ul>
                    <a href="https://next.example/">Onwards</a>
                    <label for=q>Search</label><input id=q value=cats>
                    <button id=go>Go</button>"#
                    .to_vec(),
                charset: Some("utf-8".to_owned()),
                final_url: url.to_owned(),
                ..Default::default()
            })
        }
    }

    fn site() -> Session {
        let mut session = Session::new(Browser::new(Site), (800, 600));
        session
            .dispatch(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://catalogue.example/"}),
            ))
            .expect("navigated");
        session
    }

    #[test]
    fn the_page_can_be_read_without_a_script_engine_and_without_a_picture() {
        let mut session = site();
        let read = session
            .dispatch(&command(2, "otlyra:readPage", json!({})))
            .expect("a reading");

        let text = read["text"].as_str().expect("text");
        assert_eq!(read["title"], json!("Catalogue"));
        // The four shapes an agent needs off a page: its headings, its prose, its
        // lists, and where its links go.
        assert!(text.contains("# Catalogue"), "{text}");
        assert!(text.contains("Two things we have."), "{text}");
        assert!(text.contains("- A thing"), "{text}");
        assert!(text.contains("https://next.example/"), "{text}");
    }

    #[test]
    fn a_snapshot_hands_back_handles_the_other_commands_take() {
        let mut session = site();
        let snapshot = session
            .dispatch(&command(
                2,
                "otlyra:snapshot",
                json!({"interactiveOnly": true}),
            ))
            .expect("a snapshot");

        let nodes = snapshot["nodes"].as_array().expect("nodes");
        assert!(!nodes.is_empty(), "{snapshot:#}");
        let button = nodes
            .iter()
            .find(|node| node["role"] == json!("button"))
            .unwrap_or_else(|| panic!("{snapshot:#}"));

        // The whole point of the command: what was read is what can be acted on,
        // with no selector guessed in between.
        let shared = button["sharedId"].as_str().expect("a handle").to_owned();
        session
            .dispatch(&command(
                3,
                "input.performActions",
                json!({"actions": [{
                    "type": "pointer",
                    "id": "mouse",
                    "actions": [
                        {"type": "pointerMove", "x": 0, "y": 0,
                         "origin": {"type": "element", "element": {"sharedId": shared}}},
                        {"type": "pointerDown", "button": 0},
                        {"type": "pointerUp", "button": 0},
                    ],
                }]}),
            ))
            .expect("clicked");
    }

    #[test]
    fn asking_only_for_what_can_be_acted_on_is_shorter_than_the_whole_page() {
        let mut session = site();
        let whole = session
            .dispatch(&command(2, "otlyra:snapshot", json!({})))
            .expect("a snapshot");
        let acting = session
            .dispatch(&command(
                3,
                "otlyra:snapshot",
                json!({"interactiveOnly": true}),
            ))
            .expect("a snapshot");

        let count = |value: &Value| value["nodes"].as_array().map_or(0, Vec::len);
        assert!(count(&acting) < count(&whole), "{acting:#}");
        assert!(
            acting["nodes"]
                .as_array()
                .expect("nodes")
                .iter()
                .all(|node| node["interactive"] == json!(true))
        );
    }

    #[test]
    fn waiting_answers_whether_what_was_waited_for_is_there() {
        let mut session = site();
        let found = session
            .dispatch(&command(
                2,
                "otlyra:waitFor",
                json!({"locator": {"type": "css", "value": "button#go"}}),
            ))
            .expect("waited");
        assert_eq!(found["found"], json!(true));
        assert_eq!(found["loading"], json!(false));

        let missing = session
            .dispatch(&command(
                3,
                "otlyra:waitFor",
                json!({"locator": {"type": "css", "value": ".nowhere"}, "timeout": 50}),
            ))
            .expect("waited");
        // Answered rather than refused: *it is not there* is a fact a caller can
        // act on, and it is not an error.
        assert_eq!(missing["found"], json!(false));
    }

    #[test]
    fn the_network_can_be_listed_by_a_caller_that_cannot_subscribe() {
        let mut session = site();
        let listed = session
            .dispatch(&command(2, "otlyra:network", json!({})))
            .expect("a list");

        let requests = listed["requests"].as_array().expect("requests");
        assert!(!requests.is_empty(), "{listed:#}");
        assert!(
            requests
                .iter()
                .any(|request| request["url"] == json!("https://catalogue.example/")),
            "{listed:#}"
        );
    }

    #[test]
    fn a_cookie_can_be_set_read_back_and_deleted() {
        let mut session = site();
        session
            .dispatch(&command(
                2,
                "storage.setCookie",
                json!({"cookie": {
                    "name": "session",
                    "value": {"type": "string", "value": "abc"},
                    "domain": "catalogue.example",
                    "path": "/",
                }}),
            ))
            .expect("set");

        let read = session
            .dispatch(&command(
                3,
                "storage.getCookies",
                json!({"filter": {"name": "session"}}),
            ))
            .expect("read");
        assert_eq!(read["cookies"][0]["value"]["value"], json!("abc"));

        let deleted = session
            .dispatch(&command(
                4,
                "storage.deleteCookies",
                json!({"filter": {"name": "session"}}),
            ))
            .expect("deleted");
        assert_eq!(deleted["removed"], json!(1));

        let after = session
            .dispatch(&command(5, "storage.getCookies", json!({})))
            .expect("read");
        assert!(after["cookies"].as_array().expect("cookies").is_empty());
    }

    #[test]
    fn a_navigation_that_waits_for_nothing_is_answered_before_it_arrives() {
        let mut session = Session::new(Browser::new(Slow), (800, 600));
        let started = std::time::Instant::now();
        let outcome = session
            .begin(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://slow.example/", "wait": "none"}),
            ))
            .expect("started");

        // The point of `wait: none`: the answer is here while the load is not.
        // Without it a driver cannot watch a load, and cannot answer anything the
        // load itself is waiting on.
        assert!(
            matches!(outcome, Outcome::Done(_)),
            "should not have parked"
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
        assert_eq!(
            session.browser.readiness(session.browser.active()),
            Readiness::Started
        );
    }

    #[test]
    fn a_navigation_that_waits_is_parked_rather_than_holding_the_loop() {
        let mut session = Session::new(Browser::new(Slow), (800, 600));
        let outcome = session
            .begin(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://slow.example/"}),
            ))
            .expect("started");
        assert!(matches!(outcome, Outcome::Parked));

        // While it is parked the session still answers: this is the whole
        // difference between a driver that can work during a load and one that
        // has to sit through it.
        let status = session
            .dispatch(&command(2, "session.status", json!({})))
            .expect("answered");
        assert!(status["ready"].is_boolean());

        // And the parked one is answered when the browser gets there.
        let answer = loop {
            session.pump();
            if let Some((id, answer)) = session.resolve().pop() {
                assert_eq!(id, 1);
                break answer.expect("navigated");
            }
        };
        assert_eq!(answer["url"], json!("https://slow.example/"));
        assert!(answer["navigation"].is_string());
    }

    #[test]
    fn a_readiness_a_client_does_not_have_a_word_for_is_refused() {
        let mut session = session();
        let error = session
            .begin(&command(
                1,
                "browsingContext.navigate",
                json!({"url": "https://a.example/", "wait": "eventually"}),
            ))
            .unwrap_err();
        assert_eq!(error.code, "invalid argument");
    }

    #[test]
    fn a_tab_reports_starting_becoming_ready_and_finishing() {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "session.subscribe",
                json!({"events": ["browsingContext"]}),
            ))
            .expect("subscribed");
        session.drain_events();

        session
            .dispatch(&command(
                2,
                "browsingContext.navigate",
                json!({"url": "https://a.example/"}),
            ))
            .expect("navigated");

        let names: Vec<String> = session
            .drain_events()
            .into_iter()
            .map(|event| event["method"].as_str().unwrap_or_default().to_owned())
            .collect();

        // All three, in the order a load goes through them. A driver waiting on
        // the middle one — the document is there, the pictures are not — is the
        // one this browser can now serve.
        assert!(
            names.contains(&"browsingContext.navigationStarted".to_owned()),
            "{names:?}"
        );
        assert!(
            names.contains(&"browsingContext.domContentLoaded".to_owned()),
            "{names:?}"
        );
        assert!(
            names.contains(&"browsingContext.load".to_owned()),
            "{names:?}"
        );
        let place = |method: &str| names.iter().position(|name| name == method);
        assert!(place("browsingContext.navigationStarted") < place("browsingContext.load"));

        // And nothing is said twice about a load that has already been reported.
        assert!(session.drain_events().is_empty());
    }

    #[test]
    fn every_lifecycle_event_names_the_navigation_it_belongs_to() {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "session.subscribe",
                json!({"events": ["browsingContext.load"]}),
            ))
            .expect("subscribed");
        session.drain_events();

        let answer = session
            .dispatch(&command(
                2,
                "browsingContext.navigate",
                json!({"url": "https://a.example/"}),
            ))
            .expect("navigated");
        let events = session.drain_events();
        let load = events
            .iter()
            .find(|event| event["method"] == json!("browsingContext.load"))
            .unwrap_or_else(|| panic!("{events:#?}"));

        // A client that started two navigations tells them apart by this, so the
        // event and the command it came from have to agree.
        assert_eq!(load["params"]["navigation"], answer["navigation"]);
        assert!(load["params"]["navigation"].is_string());
    }

    #[test]
    fn going_somewhere_else_reports_its_own_load_rather_than_staying_quiet() {
        let mut session = session();
        session
            .dispatch(&command(
                1,
                "session.subscribe",
                json!({"events": ["browsingContext.load"]}),
            ))
            .expect("subscribed");
        session.drain_events();

        let mut loads = Vec::new();
        for (id, url) in [(2, "https://one.example/"), (3, "https://two.example/")] {
            session
                .dispatch(&command(
                    id,
                    "browsingContext.navigate",
                    json!({ "url": url }),
                ))
                .expect("navigated");
            loads.extend(
                session
                    .drain_events()
                    .into_iter()
                    .filter(|event| event["method"] == json!("browsingContext.load")),
            );
        }

        // Two loads, under two names. Reporting only the first is what a
        // readiness diff does if a new navigation does not reset it.
        assert_eq!(loads.len(), 2, "{loads:#?}");
        assert_ne!(
            loads[0]["params"]["navigation"],
            loads[1]["params"]["navigation"]
        );
    }

    /// Add an intercept, start a navigation without waiting for it, and hand
    /// back the handle of the request that is now being held.
    ///
    /// `wait: none` is the whole shape of interception: the navigation is
    /// answered while its request is stopped, and the command that releases it
    /// arrives afterwards. A navigation that waited would be waiting for a
    /// command that could not be sent.
    fn intercepted(session: &mut Session, url: &str, patterns: Value) -> String {
        session
            .dispatch(&command(
                1,
                "network.addIntercept",
                json!({ "phases": ["beforeRequestSent"], "urlPatterns": patterns }),
            ))
            .expect("intercept added");
        session
            .dispatch(&command(
                2,
                "browsingContext.navigate",
                json!({ "url": url, "wait": "none" }),
            ))
            .expect("started");

        let events = session.drain_events();
        let blocked = events
            .iter()
            .find(|event| event["method"] == json!("network.beforeRequestSent"))
            .unwrap_or_else(|| panic!("nothing was held: {events:#?}"));
        assert_eq!(blocked["params"]["isBlocked"], json!(true));
        blocked["params"]["request"]["request"]
            .as_str()
            .expect("a handle")
            .to_owned()
    }

    /// A driver that goes away must leave a browser that loads pages.
    ///
    /// The gate lives on the browser and outlives the connection that installed
    /// it. Left there, every address it matches is held forever — and the next
    /// client is never told about them either, because they were already
    /// announced to the client that vanished.
    #[test]
    fn a_driver_that_goes_away_takes_its_intercepts_with_it() {
        let mut session = session();
        intercepted(
            &mut session,
            "https://held.example/",
            json!([{ "type": "pattern", "hostname": "held.example" }]),
        );
        assert!(
            !session.browser.held().is_empty(),
            "nothing was held, so this proves nothing"
        );

        session.disconnected();

        assert!(
            session.browser.held().is_empty(),
            "a held request outlived the driver that was going to answer it"
        );
        // And the gate is down: the same address loads rather than stopping.
        session
            .dispatch(&command(
                9,
                "browsingContext.navigate",
                json!({ "url": "https://held.example/" }),
            ))
            .expect("navigated");
        assert!(
            session.browser.held().is_empty(),
            "the gate was still installed with nobody left to answer it"
        );
    }

    /// A step through the history is a load, and a load is parked rather than
    /// waited for. It used to wait on this thread once per step, so a `delta` of
    /// five was five load timeouts with the socket unread.
    #[test]
    fn a_step_through_the_history_is_started_rather_than_waited_for() {
        let mut session = session();
        for (id, url) in [(1, "https://one.example/"), (2, "https://two.example/")] {
            session
                .dispatch(&command(
                    id,
                    "browsingContext.navigate",
                    json!({ "url": url }),
                ))
                .expect("navigated");
        }

        // Answered through the same door as a navigation: `begin` starts it, and
        // `resolve` says when it arrived.
        let back = session
            .dispatch(&command(
                3,
                "browsingContext.traverseHistory",
                json!({ "delta": -1 }),
            ))
            .expect("stepped back");
        assert_eq!(back["url"], json!("https://one.example/"), "{back:#?}");

        // And reaching it the other way round is refused rather than run twice.
        let round_the_back = session.dispatch_now(&command(
            4,
            "browsingContext.traverseHistory",
            json!({ "delta": -1 }),
        ));
        assert!(round_the_back.is_err());
    }

    #[test]
    fn a_screenshot_can_be_asked_for_one_element_rather_than_the_page() {
        let mut session = site();
        let found = session
            .dispatch(&command(
                2,
                "browsingContext.locateNodes",
                json!({"locator": {"type": "css", "value": "button#go"}}),
            ))
            .expect("a match");
        let shared = found["nodes"][0]["sharedId"]
            .as_str()
            .expect("a handle")
            .to_owned();

        let whole = session
            .dispatch(&command(3, "browsingContext.captureScreenshot", json!({})))
            .expect("a picture");
        let button = session
            .dispatch(&command(
                4,
                "browsingContext.captureScreenshot",
                json!({"clip": {"type": "element", "element": {"sharedId": shared}}}),
            ))
            .expect("a picture");

        // Both are PNGs, and the one of a button is smaller than the one of the
        // page — which is the whole claim: nothing outside the rectangle was
        // rasterized, so a clip of a hundred pixels costs a hundred pixels.
        for picture in [&whole, &button] {
            assert!(
                picture["data"]
                    .as_str()
                    .is_some_and(|data| data.starts_with("iVBORw0KGgo")),
                "{picture:#}"
            );
        }
        let size = |picture: &Value| picture["data"].as_str().unwrap_or_default().len();
        assert!(
            size(&button) < size(&whole),
            "{} vs {}",
            size(&button),
            size(&whole)
        );
    }

    #[test]
    fn a_box_clip_is_taken_in_the_page_own_coordinates() {
        let mut session = site();
        let picture = session
            .dispatch(&command(
                2,
                "browsingContext.captureScreenshot",
                json!({"clip": {"type": "box", "x": 0, "y": 0, "width": 40, "height": 20}}),
            ))
            .expect("a picture");
        assert!(
            picture["data"]
                .as_str()
                .is_some_and(|data| data.starts_with("iVBORw0KGgo"))
        );

        // A rectangle with nothing in it is a mistake worth naming: a picture of
        // it would be zero pixels wide, which no reader can look at.
        let refused = session
            .dispatch(&command(
                3,
                "browsingContext.captureScreenshot",
                json!({"clip": {"type": "box", "x": 0, "y": 0, "width": 0, "height": 20}}),
            ))
            .unwrap_err();
        assert_eq!(refused.code, "invalid argument");
    }

    #[test]
    fn a_held_request_can_be_answered_with_a_response_nobody_sent() {
        let mut session = session();
        let held = intercepted(
            &mut session,
            "https://mocked.example/",
            json!([{ "type": "pattern", "hostname": "mocked.example" }]),
        );

        session
            .dispatch(&command(
                3,
                "network.provideResponse",
                json!({
                    "request": held,
                    "statusCode": 200,
                    "headers": [{ "name": "Content-Type", "value": {"type": "string", "value": "text/html"} }],
                    "body": { "type": "string", "value": "<title>Made up</title><p>invented" },
                }),
            ))
            .expect("provided");

        // The page is the one the driver wrote. No server answered it, and there
        // is no server to answer it — which is the case this exists for.
        while session.browser.readiness(session.browser.active()) != Readiness::Complete {
            session.pump();
        }
        let read = session
            .dispatch(&command(4, "otlyra:readPage", json!({})))
            .expect("a reading");
        assert!(
            read["text"]
                .as_str()
                .unwrap_or_default()
                .contains("invented"),
            "{read:#}"
        );
    }

    #[test]
    fn a_held_request_can_be_stopped() {
        let mut session = session();
        let held = intercepted(
            &mut session,
            "https://blocked.example/",
            json!([{ "type": "pattern", "hostname": "blocked.example" }]),
        );
        session
            .dispatch(&command(
                3,
                "network.failRequest",
                json!({ "request": held }),
            ))
            .expect("failed");

        while session.browser.readiness(session.browser.active()) != Readiness::Complete {
            session.pump();
        }
        // Blocked is not *pending forever*: the load ended, and it ended badly,
        // which is what a page whose script was refused has to be able to see.
        let listed = session
            .dispatch(&command(4, "otlyra:network", json!({})))
            .expect("a list");
        let request = &listed["requests"][0];
        assert_eq!(request["state"], json!("failed"), "{listed:#}");
    }

    #[test]
    fn a_held_request_can_be_sent_somewhere_else() {
        let mut session = session();
        let held = intercepted(
            &mut session,
            "https://original.example/",
            json!([{ "type": "pattern", "hostname": "original.example" }]),
        );
        session
            .dispatch(&command(
                3,
                "network.continueRequest",
                json!({ "request": held, "url": "https://fixture.example/" }),
            ))
            .expect("continued");

        while session.browser.readiness(session.browser.active()) != Readiness::Complete {
            session.pump();
        }
        // The list says where it actually went, not where it was going before
        // the driver moved it.
        let listed = session
            .dispatch(&command(4, "otlyra:network", json!({})))
            .expect("a list");
        assert_eq!(
            listed["requests"][0]["url"],
            json!("https://fixture.example/"),
            "{listed:#}"
        );
    }

    #[test]
    fn a_held_request_can_be_let_go_with_headers_the_driver_wrote() {
        let mut session = session();
        let held = intercepted(
            &mut session,
            "https://a.example/",
            json!([{ "type": "pattern", "hostname": "a.example" }]),
        );
        session
            .dispatch(&command(
                3,
                "network.continueRequest",
                json!({
                    "request": held,
                    "headers": [{
                        "name": "Accept-Language",
                        "value": {"type": "string", "value": "cy-GB"},
                    }],
                }),
            ))
            .expect("continued");

        while session.browser.readiness(session.browser.active()) != Readiness::Complete {
            session.pump();
        }
        assert_eq!(
            session.browser.exchanges()[0].url,
            "https://a.example/",
            "the address was not the thing being changed"
        );
    }

    #[test]
    fn a_cookie_a_driver_wrote_is_refused_rather_than_sent() {
        let mut session = session();
        let held = intercepted(
            &mut session,
            "https://a.example/",
            json!([{ "type": "pattern", "hostname": "a.example" }]),
        );
        let error = session
            .dispatch(&command(
                3,
                "network.continueRequest",
                json!({
                    "request": held,
                    "cookies": [{"name": "session", "value": {"type": "string", "value": "x"}}],
                }),
            ))
            .unwrap_err();

        // The jar decides what a site is entitled to be sent. A command that could
        // write this could send one site's session to another.
        assert_eq!(error.code, "unsupported operation");
    }

    #[test]
    fn nothing_is_held_once_the_last_intercept_is_gone() {
        let mut session = session();
        let added = session
            .dispatch(&command(
                1,
                "network.addIntercept",
                json!({ "phases": ["beforeRequestSent"] }),
            ))
            .expect("added");
        let id = added["intercept"].as_str().expect("a name").to_owned();
        session
            .dispatch(&command(
                2,
                "network.removeIntercept",
                json!({ "intercept": id }),
            ))
            .expect("removed");

        // A driver that removed its last intercept — or went away — has to leave
        // a browser that loads pages.
        session
            .dispatch(&command(
                3,
                "browsingContext.navigate",
                json!({ "url": "https://a.example/" }),
            ))
            .expect("navigated");
        assert!(session.browser.held().is_empty());
    }

    #[test]
    fn a_request_that_is_no_longer_held_says_so_rather_than_doing_nothing() {
        let mut session = session();
        let held = intercepted(
            &mut session,
            "https://a.example/",
            json!([{ "type": "pattern", "hostname": "a.example" }]),
        );
        session
            .dispatch(&command(
                3,
                "network.failRequest",
                json!({ "request": held.clone() }),
            ))
            .expect("failed");

        let again = session
            .dispatch(&command(
                4,
                "network.failRequest",
                json!({ "request": held }),
            ))
            .unwrap_err();
        // Quietly doing nothing would leave the driver waiting for a load that
        // already ended.
        assert_eq!(again.code, "no such request");
    }

    #[test]
    fn base64_survives_a_round_trip_through_a_provided_body() {
        // A driver mocking a picture sends base64, and a body decoded wrongly is
        // a picture that will not decode with nothing saying why.
        let bytes: Vec<u8> = (0u8..=255).collect();
        let round = unbase64(&base64(&bytes)).expect("decoded");
        assert_eq!(round, bytes);
        assert_eq!(unbase64(&base64(b"a")).expect("decoded"), b"a");
        assert_eq!(unbase64(&base64(b"ab")).expect("decoded"), b"ab");
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
