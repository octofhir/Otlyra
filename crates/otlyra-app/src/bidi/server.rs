//! The WebSocket a client speaks the protocol over.
//!
//! One connection at a time, on the thread that owns the browser. A browser
//! cannot be driven from two places at once without inventing an answer to
//! *whose navigation wins*, so a second client waits in the listener's backlog
//! until the first goes away — which is a queue rather than a refusal, and is
//! what a driver that reconnects after a crash needs.
//!
//! Blocking, and on the browser's own thread. The page holds `Rc`s all the way
//! down and is not `Send`; an async runtime around it would buy nothing, cost a
//! second scheduler beside the fetch threads, and put a lock between a command
//! and the state it is about.

use std::net::{SocketAddr, TcpListener};

use serde_json::Value;

use super::{Command, Session, success};

/// A listening endpoint, and the address it ended up on.
pub struct Server {
    listener: TcpListener,
    address: SocketAddr,
}

/// Start listening on `port` of the loopback.
///
/// The loopback and nothing else: this endpoint drives a browser — it navigates,
/// it clicks, it reads what is on screen — and a port that answered the network
/// would be handing that to whoever asked. A client on another machine belongs
/// behind an explicit tunnel the person set up, not behind a default.
///
/// Port `0` asks the system for a free one, which is what a test wants and what
/// a driver that reads the address back can use.
pub fn listen(port: u16) -> std::io::Result<Server> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))?;
    let address = listener.local_addr()?;
    Ok(Server { listener, address })
}

impl Server {
    /// Where clients connect.
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The address as a client writes it.
    pub fn url(&self) -> String {
        format!("ws://{}/session", self.address)
    }

    /// Accept one client and answer it until it goes away.
    ///
    /// Returns when the client disconnects, so a caller that wants to serve
    /// another calls again. A driver that quits and reconnects is the ordinary
    /// case and does not need the browser restarted with it.
    pub fn serve_one(&self, session: &mut Session) -> std::io::Result<()> {
        let (stream, peer) = self.listener.accept()?;
        tracing::info!(%peer, "a driver connected");
        // A read that gives up after a moment, so a client that subscribes and
        // then waits still hears about things. Without it the loop would sit in
        // `read` until the client said something, and an event would only ever
        // arrive behind the next command — which is not an event, it is an
        // answer with a delay.
        stream.set_read_timeout(Some(POLL))?;
        let mut socket = match tungstenite::accept(stream) {
            Ok(socket) => socket,
            Err(error) => {
                tracing::warn!(%error, "a connection was not a WebSocket");
                return Ok(());
            }
        };

        loop {
            let message = match socket.read() {
                Ok(message) => Some(message),
                // Nothing was said in time, which is not a failure: it is the
                // gap events are delivered in.
                Err(tungstenite::Error::Io(error)) if would_block(&error) => None,
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    break;
                }
                Err(error) => {
                    tracing::warn!(%error, "the connection failed");
                    break;
                }
            };

            if let Some(message) = message {
                let text = match message {
                    tungstenite::Message::Text(text) => text.to_string(),
                    tungstenite::Message::Close(_) => break,
                    // Ping and pong are answered by tungstenite itself; a binary
                    // frame is not something this protocol ever sends.
                    _ => continue,
                };

                let reply = answer(session, &text);
                if !send(&mut socket, &reply) {
                    break;
                }
            }

            // After the answer, so a command that causes an event — a navigation
            // causes several — is answered before the events it caused arrive.
            let mut failed = false;
            for event in session.drain_events() {
                if !send(&mut socket, &event) {
                    failed = true;
                    break;
                }
            }
            if failed {
                break;
            }
        }

        tracing::info!(%peer, "the driver went away");
        Ok(())
    }
}

/// How long a read waits before the loop goes round to deliver events.
///
/// Short enough that an event is not noticeably late, long enough that an idle
/// connection is not a spin. A driver watching a page load is watching something
/// that takes tens of milliseconds a step.
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Whether an IO error is *nothing happened yet* rather than a failure.
///
/// A timed-out read is spelled differently on different systems, and both mean
/// the same thing here.
fn would_block(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Send one message, saying whether the connection survived it.
fn send(socket: &mut tungstenite::WebSocket<std::net::TcpStream>, message: &Value) -> bool {
    socket
        .send(tungstenite::Message::Text(message.to_string().into()))
        .is_ok()
}

/// Turn one message into one reply.
///
/// Apart from the socket so it can be tested without one: what a protocol gets
/// wrong is almost never the framing.
pub(super) fn answer(session: &mut Session, text: &str) -> Value {
    let command = match Command::parse(text) {
        Ok(command) => command,
        // A message with no id cannot be answered with an error carrying its id,
        // because it has none. The specification says to send one anyway.
        Err(error) => return error.to_message(None),
    };
    match session.dispatch(&command) {
        Ok(result) => success(command.id, result),
        Err(error) => error.to_message(Some(command.id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bidi::Session;
    use crate::browser::Browser;
    use crate::fetcher::{Loaded, Loader};
    use serde_json::json;

    struct Nothing;

    impl Loader for Nothing {
        fn load(&self, _url: &str) -> Result<Loaded, String> {
            Err("this test fetches nothing".to_owned())
        }
    }

    fn session() -> Session {
        Session::new(Browser::new(Nothing), (400, 300))
    }

    #[test]
    fn a_reply_carries_the_id_it_was_asked_under() {
        let mut session = session();
        let reply = answer(&mut session, r#"{"id":7,"method":"session.status"}"#);
        assert_eq!(reply["type"], json!("success"));
        assert_eq!(reply["id"], json!(7));
    }

    #[test]
    fn a_message_that_is_not_a_command_is_still_answered() {
        let mut session = session();
        // Silence would leave a client waiting on a reply that is never coming,
        // which is worse than an error it can print.
        let reply = answer(&mut session, "{}");
        assert_eq!(reply["type"], json!("error"));
        assert_eq!(reply["id"], Value::Null);
        assert_eq!(reply["error"], json!("invalid argument"));
    }

    #[test]
    fn an_error_is_answered_under_the_id_that_caused_it() {
        let mut session = session();
        let reply = answer(&mut session, r#"{"id":3,"method":"nope.nothing"}"#);
        assert_eq!(reply["type"], json!("error"));
        assert_eq!(reply["id"], json!(3));
        assert_eq!(reply["error"], json!("unknown command"));
    }

    #[test]
    fn port_zero_asks_the_system_for_one_and_says_which() {
        let server = listen(0).expect("the loopback is bindable");
        assert!(server.address().port() > 0);
        assert!(server.url().starts_with("ws://127.0.0.1:"));
        // The loopback and nothing else: this endpoint drives a browser.
        assert!(server.address().ip().is_loopback());
    }
}
