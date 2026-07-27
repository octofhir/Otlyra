//! The browser as a set of tools an agent can call.
//!
//! # Why this as well as the protocol
//!
//! [`crate::bidi`] is what a *program* drives this browser with: Puppeteer,
//! Selenium, anything written against the W3C standard. An agent is a different
//! caller with a different problem — it does not have a client library, it has a
//! list of tools and a description of each — and the Model Context Protocol is
//! what that shape is called.
//!
//! So this is not a second protocol in the sense invariant 5 warns about. It is
//! a second *surface* on one vocabulary: every tool below is one BiDi command,
//! dispatched through the same [`crate::bidi::Session`] against the same
//! browser. Nothing here knows anything about a page that the protocol does not,
//! and a question asked through either arrives at the same place. A second
//! implementation of *what is on this page* is the one thing this design exists
//! to avoid.
//!
//! # Shape
//!
//! JSON-RPC over stdin and stdout, one message per line. Which is why every
//! diagnostic in this program goes to stderr: stdout is the wire, and one stray
//! `println!` would be a parse error in somebody's agent.

use serde_json::{Value, json};

use crate::bidi::{Command, Session};

/// The protocol version this speaks.
///
/// A client that asks for a different one is answered with this rather than
/// refused: the parts used here — `initialize`, `tools/list`, `tools/call` —
/// have not changed between versions, and refusing a client over a date it sent
/// would be refusing to work for no reason a person would accept.
const VERSION: &str = "2025-06-18";

/// One tool, and the BiDi command it is a name for.
struct Tool {
    /// What an agent calls it.
    name: &'static str,
    /// What an agent is told it does. This is the whole of how it decides to
    /// call it, so it says what the tool is *for* rather than what it returns.
    description: &'static str,
    /// The command it becomes.
    method: &'static str,
    /// What it takes, as JSON Schema.
    schema: fn() -> Value,
    /// What the command takes, given what the tool was called with.
    ///
    /// # Why a tool's arguments are not the command's parameters
    ///
    /// For most of them they are, and [`same`] says so. But the protocol is
    /// written for a *client library*, which builds its messages in code and does
    /// not mind that a click is a list of action sources with an element origin
    /// and three actions in it. An agent has no library: it writes the JSON
    /// itself, from a schema, once per turn, and every nested object in that
    /// schema is somewhere it can go wrong and spend a turn finding out.
    ///
    /// So a tool takes what the *task* needs — an element and some text — and
    /// this turns it into what the command needs. The result is still one command
    /// against the one session, which is the rule this module exists to keep;
    /// what changes is only who writes the awkward shape, and it should not be
    /// the caller.
    prepare: fn(&Value) -> Result<Value, String>,
}

/// A tool whose arguments are already the command's parameters.
fn same(arguments: &Value) -> Result<Value, String> {
    Ok(arguments.clone())
}

/// The element an action is aimed at, as an action origin.
///
/// `sharedId` is what `browser_find` and `browser_snapshot` both hand back, so
/// an agent aims with the handle it was given rather than with a coordinate it
/// would have had to work out.
fn origin(arguments: &Value) -> Result<Value, String> {
    let shared = arguments
        .get("sharedId")
        .and_then(Value::as_str)
        .ok_or("this needs a sharedId, as browser_snapshot or browser_find returned it")?;
    Ok(json!({ "type": "element", "element": { "sharedId": shared } }))
}

/// One pointer source that moves to `origin` and presses once.
fn press_at(origin: Value) -> Value {
    json!({
        "type": "pointer",
        "id": "mouse",
        "actions": [
            { "type": "pointerMove", "x": 0, "y": 0, "origin": origin },
            { "type": "pointerDown", "button": 0 },
            { "type": "pointerUp", "button": 0 },
        ],
    })
}

/// The code point the protocol spells a named key as.
///
/// WebDriver puts the keys that are not characters in a private-use area, which
/// is a table no agent should be asked to have. It sends `"Enter"` and this turns
/// it into what the wire carries.
fn key_code(name: &str) -> Option<&'static str> {
    Some(match name {
        "Enter" | "Return" => "\u{E007}",
        "Tab" => "\u{E004}",
        "Backspace" => "\u{E003}",
        "Escape" | "Esc" => "\u{E00C}",
        "ArrowLeft" | "Left" => "\u{E012}",
        "ArrowUp" | "Up" => "\u{E013}",
        "ArrowRight" | "Right" => "\u{E014}",
        "ArrowDown" | "Down" => "\u{E015}",
        "Home" => "\u{E011}",
        "End" => "\u{E010}",
        "PageUp" => "\u{E00E}",
        "PageDown" => "\u{E00F}",
        "Delete" => "\u{E017}",
        _ => return None,
    })
}

/// A key source that presses everything in `values`, in order.
fn key_source(values: Vec<String>) -> Value {
    let actions: Vec<Value> = values
        .into_iter()
        .flat_map(|value| {
            [
                json!({ "type": "keyDown", "value": value }),
                json!({ "type": "keyUp", "value": value }),
            ]
        })
        .collect();
    json!({ "type": "key", "id": "keyboard", "actions": actions })
}

/// Everything an agent can do to this browser.
///
/// # How long this list should be
///
/// It is read in full before every decision, so every tool costs something on
/// every turn it is not the answer. That argues for a short list — but not for a
/// list so short that the common tasks have no name and an agent has to assemble
/// them out of a general one. A `browser_act` that takes raw action sources is
/// one tool instead of four and costs more than four, because writing that JSON
/// correctly is a skill and getting it wrong costs a turn.
///
/// So: one tool per thing a caller actually wants to do, and none for things it
/// wants rarely enough to reach for `browser_act` instead — which is still here,
/// and is what a drag, a chord or a multi-touch gesture is written with.
fn tools() -> Vec<Tool> {
    fn nothing() -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn url() -> Value {
        json!({
            "type": "object",
            "properties": { "url": { "type": "string", "description": "Where to go." } },
            "required": ["url"],
        })
    }
    fn selector() -> Value {
        json!({
            "type": "object",
            "properties": {
                "locator": {
                    "type": "object",
                    "properties": {
                        "type": {
                            "enum": ["css", "xpath", "innerText", "accessibility"],
                            "description": "How the element is named: by CSS selector, by XPath, \
                                            by the words it shows, or by its role and name.",
                        },
                        "value": {
                            "description": "A CSS selector; an XPath expression; the words for \
                                            innerText; or {\"role\": \"button\", \"name\": \
                                            \"Send\"} for accessibility.",
                        },
                        "matchType": {
                            "enum": ["full", "partial"],
                            "description": "innerText only. Default full.",
                        },
                        "ignoreCase": { "type": "boolean", "description": "innerText only." },
                    },
                    "required": ["type", "value"],
                },
                "maxNodeCount": { "type": "integer", "description": "At most this many." },
            },
            "required": ["locator"],
        })
    }
    fn node() -> Value {
        json!({
            "type": "object",
            "properties": {
                "sharedId": {
                    "type": "string",
                    "description": "A node handle, as browser_snapshot or browser_find returned it.",
                },
            },
            "required": ["sharedId"],
        })
    }
    fn actions() -> Value {
        json!({
            "type": "object",
            "properties": {
                "actions": {
                    "type": "array",
                    "description": "WebDriver BiDi action sources: pointer, key or wheel.",
                },
            },
            "required": ["actions"],
        })
    }
    fn snapshot() -> Value {
        json!({
            "type": "object",
            "properties": {
                "interactiveOnly": {
                    "type": "boolean",
                    "description": "Only what can be clicked, typed into or chosen. Much shorter \
                                    on a page of prose, and what you want before acting.",
                },
                "maxDepth": { "type": "integer", "description": "How far down to go." },
            },
        })
    }
    fn typing() -> Value {
        json!({
            "type": "object",
            "properties": {
                "sharedId": { "type": "string", "description": "The field to type into." },
                "text": { "type": "string", "description": "What to type." },
                "submit": {
                    "type": "boolean",
                    "description": "Press Enter afterwards, which is how a search box is used.",
                },
            },
            "required": ["sharedId", "text"],
        })
    }
    fn keys() -> Value {
        json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "description": "Enter, Tab, Escape, Backspace, Delete, Home, End, PageUp, \
                                    PageDown, ArrowUp/Down/Left/Right — or any text to type it.",
                },
            },
            "required": ["key"],
        })
    }
    fn scrolling() -> Value {
        json!({
            "type": "object",
            "properties": {
                "deltaY": {
                    "type": "number",
                    "description": "Down is positive. One screen is about the viewport height.",
                },
                "deltaX": { "type": "number", "description": "Right is positive." },
                "sharedId": {
                    "type": "string",
                    "description": "Scroll with the pointer over this element. Omit for the page.",
                },
            },
        })
    }
    fn clipping() -> Value {
        json!({
            "type": "object",
            "properties": {
                "sharedId": {
                    "type": "string",
                    "description": "Photograph just this element. Omit for the whole page.",
                },
            },
        })
    }
    fn cookie_filter() -> Value {
        json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "domain": { "type": "string" },
                        "path": { "type": "string" },
                    },
                },
            },
        })
    }
    fn waiting() -> Value {
        json!({
            "type": "object",
            "properties": {
                "locator": {
                    "type": "object",
                    "properties": {
                        "type": { "const": "css" },
                        "value": { "type": "string", "description": "A CSS selector." },
                    },
                    "description": "Also report whether this is on the page once loading is done.",
                },
                "timeout": { "type": "integer", "description": "Milliseconds. Default 10000." },
            },
        })
    }

    vec![
        Tool {
            name: "browser_navigate",
            description: "Open a page and wait for it to load.",
            method: "browsingContext.navigate",
            schema: url,
            prepare: same,
        },
        Tool {
            name: "browser_read",
            description: "What the page says, as Markdown: headings, prose, lists, tables and \
                          links with their destinations. Start here. It is the cheapest way to \
                          find out what a page is, and unlike a screenshot it can be quoted.",
            method: "otlyra:readPage",
            schema: nothing,
            prepare: same,
        },
        Tool {
            name: "browser_snapshot",
            description: "What is on the page and what can be done to it: every element with its \
                          role, its name and a handle. Ask with interactiveOnly before acting — \
                          that is the list of things worth clicking, and each row's handle is \
                          what browser_click and browser_type take, so you never have to guess \
                          a selector.",
            method: "otlyra:snapshot",
            schema: snapshot,
            prepare: same,
        },
        Tool {
            name: "browser_screenshot",
            description: "A picture of the page as it is now, or of one element by its handle. \
                          For questions about how something looks; browser_read is better for \
                          what it says, and a picture of one element costs a fraction of a \
                          picture of the page.",
            method: "browsingContext.captureScreenshot",
            schema: clipping,
            prepare: |arguments| match arguments.get("sharedId") {
                None => Ok(json!({})),
                Some(_) => Ok(json!({
                    "clip": { "type": "element", "element": origin(arguments)?["element"] },
                })),
            },
        },
        Tool {
            name: "browser_window",
            description: "A picture of the whole browser window — tab strip, toolbar, page and \
                          inspector — exactly as the compositor drew it on screen. This is the \
                          one way to check the browser's own interface; browser_screenshot \
                          answers about the page. Needs a session that is driving a window.",
            method: "otlyra:captureWindow",
            schema: nothing,
            prepare: same,
        },
        Tool {
            name: "browser_find",
            description: "Find elements by CSS selector, by XPath, by the words they show, or \
                          by their role and name. Returns a handle for each, which every other \
                          tool takes.",
            method: "browsingContext.locateNodes",
            schema: selector,
            prepare: same,
        },
        Tool {
            name: "browser_click",
            description: "Click an element, by its handle. Aims at the middle of where the \
                          browser actually drew it, so it lands where a person's click would.",
            method: "input.performActions",
            schema: node,
            prepare: |arguments| Ok(json!({ "actions": [press_at(origin(arguments)?)] })),
        },
        Tool {
            name: "browser_type",
            description: "Type into a field, by its handle. Clicks it first, so what is typed \
                          goes where it was meant to.",
            method: "input.performActions",
            schema: typing,
            prepare: |arguments| {
                let text = arguments
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("browser_type needs some text")?;
                let mut values = vec![text.to_owned()];
                if arguments.get("submit").and_then(Value::as_bool) == Some(true) {
                    values.push("\u{E007}".to_owned());
                }
                Ok(json!({
                    "actions": [press_at(origin(arguments)?), key_source(values)],
                }))
            },
        },
        Tool {
            name: "browser_press",
            description: "Press a key wherever the keyboard currently is: Enter to submit, Tab \
                          to move on, Escape to dismiss, the arrows to move within a control.",
            method: "input.performActions",
            schema: keys,
            prepare: |arguments| {
                let key = arguments
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or("browser_press needs a key")?;
                let value = key_code(key).unwrap_or(key).to_owned();
                Ok(json!({ "actions": [key_source(vec![value])] }))
            },
        },
        Tool {
            name: "browser_scroll",
            description: "Scroll the page, or whatever is under an element you name. Positive \
                          deltaY goes down.",
            method: "input.performActions",
            schema: scrolling,
            prepare: |arguments| {
                let delta = |name: &str| arguments.get(name).and_then(Value::as_f64);
                let mut action = json!({
                    "type": "scroll",
                    "x": 0,
                    "y": 0,
                    "deltaX": delta("deltaX").unwrap_or(0.0),
                    // A scroll with no distance means a screen, which is what a
                    // caller that said only "scroll down" meant.
                    "deltaY": delta("deltaY").unwrap_or(600.0),
                });
                if arguments.get("sharedId").is_some() {
                    action["origin"] = origin(arguments)?;
                }
                Ok(json!({
                    "actions": [{ "type": "wheel", "id": "wheel", "actions": [action] }],
                }))
            },
        },
        Tool {
            name: "browser_act",
            description: "Anything the tools above cannot say: a drag, a chord, a sequence. \
                          Actions are WebDriver BiDi action sources; a pointer action may take \
                          an element as its origin.",
            method: "input.performActions",
            schema: actions,
            prepare: same,
        },
        Tool {
            name: "browser_wait",
            description: "Wait for the page to finish loading, and say whether a selector is on \
                          it once it has. Note that without a script engine a loaded document \
                          does not change on its own, so this answers as soon as loading ends.",
            method: "otlyra:waitFor",
            schema: waiting,
            prepare: same,
        },
        Tool {
            name: "browser_back",
            description: "Go back one page in this tab's history.",
            method: "browsingContext.traverseHistory",
            schema: nothing,
            prepare: |_| Ok(json!({ "delta": -1 })),
        },
        Tool {
            name: "browser_forward",
            description: "Go forward one page in this tab's history.",
            method: "browsingContext.traverseHistory",
            schema: nothing,
            prepare: |_| Ok(json!({ "delta": 1 })),
        },
        Tool {
            name: "browser_reload",
            description: "Load this tab's page again.",
            method: "browsingContext.reload",
            schema: nothing,
            prepare: same,
        },
        Tool {
            name: "browser_tabs",
            description: "Every tab that is open, and where each one is.",
            method: "browsingContext.getTree",
            schema: nothing,
            prepare: same,
        },
        Tool {
            name: "browser_console",
            description: "What the browser has said about itself while it worked: warnings from \
                          the parser, the cascade and the network. Where to look when a page \
                          came out wrong.",
            method: "otlyra:console",
            schema: nothing,
            prepare: same,
        },
        Tool {
            name: "browser_network",
            description: "Every request this browser made, what came back and how long it took. \
                          Where to look when something did not appear.",
            method: "otlyra:network",
            schema: nothing,
            prepare: same,
        },
        Tool {
            name: "browser_cookies",
            description: "The cookies the jar is holding, optionally filtered by name, domain \
                          or path.",
            method: "storage.getCookies",
            schema: cookie_filter,
            prepare: same,
        },
        Tool {
            name: "browser_explain",
            description: "Why an element looks the way it does: the computed style, the \
                          box model in numbers, and the grid or flex tracks it laid its \
                          children into. Answered by the layout engine itself rather than \
                          by a script in the page.",
            method: "otlyra:explain",
            schema: node,
            prepare: same,
        },
        Tool {
            name: "browser_highlight",
            description: "Draw the inspector's overlay over an element, so the next \
                          screenshot shows which one is meant. Call with no handle to \
                          clear it.",
            method: "otlyra:highlight",
            schema: node,
            prepare: same,
        },
        Tool {
            name: "browser_timings",
            description: "How long each stage of the last frame took: parse, style, \
                          layout, paint. The first place to look at a slow page.",
            method: "otlyra:frameTimings",
            schema: nothing,
            prepare: same,
        },
    ]
}

/// Answer one JSON-RPC message.
///
/// `None` for a notification, which by the rules of JSON-RPC is answered with
/// silence rather than with an empty reply.
pub fn answer(session: &mut Session, text: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(error) => {
            return Some(failure(Value::Null, -32700, &format!("not JSON: {error}")));
        }
    };
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

    match method {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(VERSION);
            Some(result(
                id,
                json!({
                    "protocolVersion": asked,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": crate::bidi::NAME,
                        "version": crate::about::VERSION,
                    },
                }),
            ))
        }
        // A notification has no id and gets no answer.
        method if method.starts_with("notifications/") => None,
        "tools/list" => Some(result(
            id,
            json!({
                "tools": tools()
                    .into_iter()
                    .map(|tool| json!({
                        "name": tool.name,
                        "description": tool.description,
                        "inputSchema": (tool.schema)(),
                    }))
                    .collect::<Vec<_>>(),
            }),
        )),
        "tools/call" => Some(call(session, id, &params)),
        "ping" => Some(result(id, json!({}))),
        other => Some(failure(
            id,
            -32601,
            &format!("{other} is not a method this server has"),
        )),
    }
}

/// Run one tool.
fn call(session: &mut Session, id: Value, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let Some(tool) = tools().into_iter().find(|tool| tool.name == name) else {
        return failure(
            id,
            -32602,
            &format!("{name} is not a tool this browser has"),
        );
    };

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let params = match (tool.prepare)(&arguments) {
        Ok(params) => params,
        // A tool called with arguments it cannot be turned into a command from is
        // told so as a result, for the same reason a failed command is: the agent
        // can read the sentence and call it again correctly, where a transport
        // error tells it only that something went wrong.
        Err(why) => {
            return result(
                id,
                json!({
                    "isError": true,
                    "content": [{ "type": "text", "text": why }],
                }),
            );
        }
    };

    let command = Command {
        id: 0,
        method: tool.method.to_owned(),
        params,
    };

    match session.dispatch(&command) {
        // A tool that failed is reported as a *result* saying so, not as a
        // JSON-RPC error: the call reached the browser and the browser answered.
        // An agent can read the sentence and try something else, where a
        // transport error tells it only that something went wrong.
        Err(error) => result(
            id,
            json!({
                "isError": true,
                "content": [{ "type": "text", "text": format!("{}: {}", error.code, error.message) }],
            }),
        ),
        Ok(value) => result(id, json!({ "content": content(name, &value) })),
    }
}

/// What a tool's answer looks like to an agent.
///
/// A screenshot comes back as an image, because an agent that can see the page
/// can answer questions about it that no amount of JSON would settle. Everything
/// else is its JSON, pretty-printed: an agent reads it, and a wall of one line
/// is a wall.
fn content(name: &str, value: &Value) -> Vec<Value> {
    if matches!(name, "browser_screenshot" | "browser_window")
        && let Some(data) = value.get("data").and_then(Value::as_str)
    {
        let mut content = vec![json!({
            "type": "image",
            "data": data,
            "mimeType": "image/png",
        })];
        // What the frame redrew, in words, because it is the one thing a picture
        // cannot say: an agent comparing two identical pictures cannot tell a
        // window that did not change from one that redrew itself the same.
        if let Some(damage) = value.get("damage") {
            content.push(json!({
                "type": "text",
                "text": format!("damage: {damage}"),
            }));
        }
        return content;
    }
    vec![json!({
        "type": "text",
        "text": serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    })]
}

fn result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn failure(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Read messages from `input` and answer them on `output`, until the input ends.
pub fn serve(
    session: &mut Session,
    input: impl std::io::BufRead,
    mut output: impl std::io::Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = answer(session, &line) {
            writeln!(output, "{reply}")?;
            // Flushed every time: an agent is waiting on this line, and a
            // buffered answer is an agent that has hung.
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::Browser;
    use crate::fetcher::{Loaded, Loader};

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
        Session::new(Browser::new(Pages), (400, 300))
    }

    fn ask(session: &mut Session, message: Value) -> Value {
        answer(session, &message.to_string()).expect("an answer")
    }

    #[test]
    fn an_agent_is_told_what_the_browser_can_do() {
        let mut session = session();
        let listed = ask(
            &mut session,
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        );
        let tools = listed["result"]["tools"].as_array().expect("tools");

        // Every tool says what it is for, and takes a schema an agent can fill
        // in without guessing.
        assert!(!tools.is_empty());
        for tool in tools {
            assert!(tool["name"].as_str().is_some_and(|name| !name.is_empty()));
            assert!(
                tool["description"]
                    .as_str()
                    .is_some_and(|text| text.len() > 20),
                "{tool:?}"
            );
            assert_eq!(tool["inputSchema"]["type"], json!("object"));
        }
    }

    #[test]
    fn a_notification_is_answered_with_silence() {
        let mut session = session();
        // JSON-RPC says so, and a client that got a reply to one would have an
        // id to match it against that it never sent.
        assert!(
            answer(
                &mut session,
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
            )
            .is_none()
        );
    }

    #[test]
    fn the_version_a_client_asked_for_is_the_one_it_is_answered_with() {
        let mut session = session();
        let hello = ask(
            &mut session,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": "2024-11-05"},
            }),
        );
        assert_eq!(hello["result"]["protocolVersion"], json!("2024-11-05"));
        assert_eq!(hello["result"]["serverInfo"]["name"], json!("otlyra"));
        assert!(hello["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn a_tool_is_one_command_against_the_same_browser() {
        let mut session = session();
        ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {
                    "name": "browser_navigate",
                    "arguments": {"url": "https://driven.example/"},
                },
            }),
        );

        // The browser really went there: the tool is a name for a command, not
        // a second implementation of one.
        assert_eq!(session.browser.url(), "https://driven.example/");
    }

    #[test]
    fn a_screenshot_comes_back_as_something_an_agent_can_look_at() {
        let mut session = session();
        ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "browser_navigate", "arguments": {"url": "https://a.example/"}},
            }),
        );
        let shot = ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "browser_screenshot", "arguments": {}},
            }),
        );

        let content = &shot["result"]["content"][0];
        assert_eq!(content["type"], json!("image"));
        assert_eq!(content["mimeType"], json!("image/png"));
        assert!(
            content["data"]
                .as_str()
                .is_some_and(|data| data.starts_with("iVBORw0KGgo"))
        );
    }

    #[test]
    fn a_tool_that_could_not_do_it_says_so_where_an_agent_will_read_it() {
        let mut session = session();
        let refused = ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "browser_explain", "arguments": {"sharedId": "nope"}},
            }),
        );

        // A result rather than a transport error: the call reached the browser
        // and the browser answered, and an agent can read the sentence and try
        // something else.
        assert_eq!(refused["result"]["isError"], json!(true));
        assert!(refused.get("error").is_none());
        let text = refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(text.contains("no such node"), "{text}");
    }

    #[test]
    fn every_tool_names_a_command_the_browser_actually_has() {
        let mut session = session();
        for tool in tools() {
            // Called with nothing, which most of them refuse — but never with
            // *unknown command*, which would mean a tool wired to a method that
            // is not there and an agent finding out one turn at a time.
            let params = (tool.prepare)(&json!({})).unwrap_or_else(|_| json!({}));
            if let Err(error) = session.dispatch(&Command {
                id: 0,
                method: tool.method.to_owned(),
                params,
            }) {
                assert_ne!(error.code, "unknown command", "{}: {error:?}", tool.name);
            }
        }
    }

    #[test]
    fn a_click_is_a_handle_rather_than_a_list_of_action_sources() {
        let mut session = session();
        ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "browser_navigate", "arguments": {"url": "https://a.example/"}},
            }),
        );
        let found = ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {
                    "name": "browser_find",
                    "arguments": {"locator": {"type": "css", "value": "#greeting"}},
                },
            }),
        );
        let text = found["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        let nodes: Value = serde_json::from_str(text).expect("json");
        let shared = nodes["nodes"][0]["sharedId"]
            .as_str()
            .expect("a handle")
            .to_owned();

        let clicked = ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "browser_click", "arguments": {"sharedId": shared}},
            }),
        );
        // The agent wrote `{sharedId}` and the browser was sent three actions
        // with an element origin. That translation is the whole reason this tool
        // is not `browser_act`.
        assert!(clicked["result"]["isError"].is_null(), "{clicked:#}");
    }

    #[test]
    fn a_tool_called_wrongly_is_told_what_it_needs_rather_than_dropped() {
        let mut session = session();
        let refused = ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "browser_click", "arguments": {}},
            }),
        );

        assert_eq!(refused["result"]["isError"], json!(true));
        let text = refused["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        // Naming the missing argument is what lets an agent fix it on the next
        // turn instead of trying the same call again.
        assert!(text.contains("sharedId"), "{text}");
    }

    #[test]
    fn typing_can_submit_in_one_call() {
        // A search box is used by typing and pressing Enter, and an agent that
        // had to do it in two calls would sometimes do only the first.
        let params =
            (tools()
                .into_iter()
                .find(|tool| tool.name == "browser_type")
                .expect("the tool")
                .prepare)(&json!({"sharedId": "1", "text": "cats", "submit": true}))
            .expect("prepared");

        let keys = &params["actions"][1]["actions"];
        assert_eq!(keys[0]["value"], json!("cats"));
        assert_eq!(keys[2]["value"], json!("\u{E007}"));
    }

    #[test]
    fn a_named_key_is_spelled_the_way_the_wire_spells_it() {
        // An agent sends "Enter". The protocol carries a private-use code point,
        // which is a table no agent should be asked to have.
        let press = tools()
            .into_iter()
            .find(|tool| tool.name == "browser_press")
            .expect("the tool");
        let params = (press.prepare)(&json!({"key": "Enter"})).expect("prepared");
        assert_eq!(
            params["actions"][0]["actions"][0]["value"],
            json!("\u{E007}")
        );

        // Anything that is not a named key is typed as it stands.
        let typed = (press.prepare)(&json!({"key": "x"})).expect("prepared");
        assert_eq!(typed["actions"][0]["actions"][0]["value"], json!("x"));
    }

    #[test]
    fn the_page_can_be_read_as_words() {
        let mut session = session();
        ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "browser_navigate", "arguments": {"url": "https://a.example/"}},
            }),
        );
        let read = ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": "browser_read", "arguments": {}},
            }),
        );

        let text = read["result"]["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("hello"), "{text}");
    }

    #[test]
    fn a_tool_nobody_has_is_a_transport_error_because_no_call_was_made() {
        let mut session = session();
        let missing = ask(
            &mut session,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "browser_teleport", "arguments": {}},
            }),
        );
        assert_eq!(missing["error"]["code"], json!(-32602));
    }

    #[test]
    fn a_line_of_rubbish_is_answered_rather_than_ignored() {
        let mut session = session();
        let reply = answer(&mut session, "}{").expect("an answer");
        assert_eq!(reply["error"]["code"], json!(-32700));
    }

    #[test]
    fn serving_reads_a_line_and_writes_a_line() {
        let mut session = session();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
            "\n",
        );
        let mut output = Vec::new();
        serve(&mut session, input.as_bytes(), &mut output).expect("served");

        let lines: Vec<&str> = std::str::from_utf8(&output)
            .expect("utf-8")
            .lines()
            .collect();
        // Two answers for three messages: the notification is answered with
        // silence.
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("protocolVersion"));
        assert!(lines[1].contains(r#""id":2"#));
    }
}
