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

            // --- what waits on a script engine -----------------------------
            method if method.starts_with("script.") => {
                Err(Error::not_yet(method, "a script engine, which is M12"))
            }

            other => Err(Error::unknown_command(other)),
        }
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
                self.browser.on_event(PlatformEvent::PointerPressed);
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
