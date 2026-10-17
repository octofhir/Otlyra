//! The page's end of its WebSockets and event streams.
//!
//! The connections themselves are the browser's: it is told to open one
//! through the opener it installed ([`set_opener`]), and runs it on a thread
//! of its own. What the connection reports comes back through an inbox the
//! page drains between turns, and the page's script hears it as events —
//! the same shape as a worker's messages (`worker.rs`).

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

/// The most connections one page may hold open at once.
pub const MAX_SOCKETS: usize = 64;

/// What is being opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketKind {
    /// A WebSocket, offering these subprotocols.
    WebSocket {
        /// The subprotocols, in the page's order.
        protocols: Vec<String>,
    },
    /// An `EventSource`.
    EventSource,
}

/// Something the page asks of its connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketCommand {
    /// Send a text message.
    Text(String),
    /// Send a binary message.
    Binary(Vec<u8>),
    /// Close, with a code and a reason.
    Close(u16, String),
}

/// Something a connection reports to its page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketEvent {
    /// Connected; the subprotocol the server chose.
    Open(String),
    /// A text message.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
    /// An event-stream event: its type, data and last event id.
    Event(String, String, String),
    /// The connection failed or broke.
    Error(String),
    /// The connection ended: code, reason, and whether cleanly.
    Close(u16, String, bool),
}

/// Where a connection's events go: the page's inbox, and a wake for the
/// browser.
#[derive(Clone)]
pub struct SocketSink {
    id: u64,
    inbox: Arc<Mutex<Vec<(u64, SocketEvent)>>>,
}

impl SocketSink {
    /// Report `event` to the page.
    pub fn send(&self, event: SocketEvent) {
        if let Ok(mut inbox) = self.inbox.lock() {
            inbox.push((self.id, event));
        }
        crate::worker::wake();
    }
}

/// One connection the page asked for, for the browser to open.
pub struct SocketOpen {
    /// What it is.
    pub kind: SocketKind,
    /// The absolute address.
    pub url: String,
    /// The page's origin, which a WebSocket handshake names.
    pub origin: String,
    /// What the page sends it.
    pub commands: Receiver<SocketCommand>,
    /// Where its events go.
    pub events: SocketSink,
}

/// How the browser opens a connection. Set once, by the embedder; without
/// one every connection fails, as one to an unreachable server does.
static OPENER: OnceLock<Arc<dyn Fn(SocketOpen) + Send + Sync>> = OnceLock::new();

/// Say how connections are opened.
pub fn set_opener(opener: Arc<dyn Fn(SocketOpen) + Send + Sync>) {
    let _ = OPENER.set(opener);
}

/// A page's connections, and what they have reported.
pub(crate) struct Sockets {
    next: u64,
    open: HashMap<u64, Sender<SocketCommand>>,
    inbox: Arc<Mutex<Vec<(u64, SocketEvent)>>>,
    events: VecDeque<(u64, SocketEvent)>,
}

impl Default for Sockets {
    fn default() -> Self {
        Self {
            next: 0,
            open: HashMap::new(),
            inbox: Arc::new(Mutex::new(Vec::new())),
            events: VecDeque::new(),
        }
    }
}

impl std::fmt::Debug for Sockets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sockets")
            .field("open", &self.open.len())
            .finish_non_exhaustive()
    }
}

impl Drop for Sockets {
    fn drop(&mut self) {
        for sender in self.open.values() {
            let _ = sender.send(SocketCommand::Close(1001, String::new()));
        }
    }
}

impl Sockets {
    /// Ask the browser to open a connection: its number, or `None` when there
    /// is no network to open it on or the page has as many as it may.
    pub(crate) fn open(&mut self, kind: SocketKind, url: String, origin: String) -> Option<u64> {
        let opener = OPENER.get()?;
        if self.open.len() >= MAX_SOCKETS {
            return None;
        }
        self.next += 1;
        let id = self.next;
        let (sender, commands) = mpsc::channel();
        self.open.insert(id, sender);
        opener(SocketOpen {
            kind,
            url,
            origin,
            commands,
            events: SocketSink {
                id,
                inbox: Arc::clone(&self.inbox),
            },
        });
        Some(id)
    }

    /// Hand connection `id` something to do.
    pub(crate) fn command(&mut self, id: u64, command: SocketCommand) {
        if let Some(sender) = self.open.get(&id) {
            let _ = sender.send(command);
        }
    }

    /// Take what the connections reported; whether events wait for the glue.
    pub(crate) fn drain(&mut self) -> bool {
        let taken = match self.inbox.lock() {
            Ok(mut inbox) => std::mem::take(&mut *inbox),
            Err(_) => return false,
        };
        for (id, event) in taken {
            if matches!(event, SocketEvent::Close(..)) {
                self.open.remove(&id);
            }
            self.events.push_back((id, event));
        }
        !self.events.is_empty()
    }

    /// The next event for the glue: `[id, type, …]`.
    pub(crate) fn next_event(&mut self) -> Option<Vec<String>> {
        let (id, event) = self.events.pop_front()?;
        let id = id.to_string();
        Some(match event {
            SocketEvent::Open(protocol) => vec![id, "open".into(), protocol],
            SocketEvent::Text(text) => vec![id, "text".into(), text],
            SocketEvent::Binary(bytes) => {
                vec![
                    id,
                    "binary".into(),
                    bytes.iter().map(|&b| char::from(b)).collect(),
                ]
            }
            SocketEvent::Event(kind, data, last) => vec![id, "event".into(), kind, data, last],
            SocketEvent::Error(message) => vec![id, "error".into(), message],
            SocketEvent::Close(code, reason, clean) => {
                vec![
                    id,
                    "close".into(),
                    code.to_string(),
                    reason,
                    clean.to_string(),
                ]
            }
        })
    }
}
