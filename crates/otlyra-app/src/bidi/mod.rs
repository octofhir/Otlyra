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
    pub fn new(browser: Browser, viewport: (u32, u32)) -> Self {
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

            // --- what waits on a script engine -----------------------------
            method if method.starts_with("script.") => {
                Err(Error::not_yet(method, "a script engine, which is M12"))
            }

            other => Err(Error::unknown_command(other)),
        }
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
