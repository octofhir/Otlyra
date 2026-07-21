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
}

/// Everything an agent can do to this browser.
///
/// Deliberately short. A tool list is read in full before every decision, so a
/// tool that is rarely the right answer costs something on every turn it is not
/// used — which is most of them.
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
                        "type": { "const": "css" },
                        "value": { "type": "string", "description": "A CSS selector." },
                    },
                    "required": ["type", "value"],
                },
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
                    "description": "A node handle, as browser_find returned it.",
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

    vec![
        Tool {
            name: "browser_navigate",
            description: "Open a page and wait for it to load.",
            method: "browsingContext.navigate",
            schema: url,
        },
        Tool {
            name: "browser_screenshot",
            description: "A picture of the page as it is now.",
            method: "browsingContext.captureScreenshot",
            schema: nothing,
        },
        Tool {
            name: "browser_find",
            description: "Find elements by CSS selector. Returns a handle for each, \
                          which every other tool takes.",
            method: "browsingContext.locateNodes",
            schema: selector,
        },
        Tool {
            name: "browser_explain",
            description: "Why an element looks the way it does: the computed style, the \
                          box model in numbers, and the grid or flex tracks it laid its \
                          children into. Answered by the layout engine itself rather than \
                          by a script in the page.",
            method: "otlyra:explain",
            schema: node,
        },
        Tool {
            name: "browser_highlight",
            description: "Draw the inspector's overlay over an element, so the next \
                          screenshot shows which one is meant. Call with no handle to \
                          clear it.",
            method: "otlyra:highlight",
            schema: node,
        },
        Tool {
            name: "browser_act",
            description: "Click, type or scroll. Actions are WebDriver BiDi action \
                          sources; a pointer action may take an element as its origin, \
                          which aims at the middle of where the browser actually drew it.",
            method: "input.performActions",
            schema: actions,
        },
        Tool {
            name: "browser_timings",
            description: "How long each stage of the last frame took: parse, style, \
                          layout, paint. The first place to look at a slow page.",
            method: "otlyra:frameTimings",
            schema: nothing,
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

    let command = Command {
        id: 0,
        method: tool.method.to_owned(),
        params: params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({})),
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
    if name == "browser_screenshot"
        && let Some(data) = value.get("data").and_then(Value::as_str)
    {
        return vec![json!({
            "type": "image",
            "data": data,
            "mimeType": "image/png",
        })];
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
