//! Long-lived connections a page holds open: WebSocket (RFC 6455, HTML §9.3)
//! and EventSource (HTML §9.2).
//!
//! Each runs on a thread of its own and blocks there, never on the thread a
//! page's script runs on: [`run_websocket`] and [`run_event_stream`] are what
//! that thread does, from connecting until the connection ends. What arrives
//! is handed to `on_event` as it arrives; what the page sends comes in on
//! `commands`.
//!
//! Both speak HTTP/1.1 over plain TCP or rustls, with the system's trust
//! store, rather than through the request client: neither is a request that
//! ends, and neither can be pooled.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// Something a connection reports to its page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketEvent {
    /// Connected.
    Open {
        /// The subprotocol the server chose, or empty.
        protocol: String,
    },
    /// A text message, or for an event stream an event's data.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
    /// One event of an event stream: its type, data and id.
    Event {
        /// Its type, `message` unless the stream named one.
        kind: String,
        /// Its data, lines joined.
        data: String,
        /// The last event id the stream set.
        id: String,
    },
    /// The connection failed or broke.
    Error(String),
    /// The connection ended.
    Close {
        /// The close code.
        code: u16,
        /// The reason the closing end gave.
        reason: String,
        /// Whether both ends agreed to it.
        clean: bool,
    },
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

/// What the page has for its connection, when asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Something to do.
    Command(SocketCommand),
    /// Nothing yet.
    Empty,
    /// The page is gone.
    Gone,
}

/// How often a connection waiting for the server looks for something the
/// page wants sent.
const POLL: Duration = Duration::from_millis(25);

/// A TCP stream, or one wrapped in TLS.
enum Stream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Tls(stream) => stream.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(stream) => stream,
            Self::Tls(stream) => &stream.sock,
        }
    }
}

/// Connect to `url`'s host, with TLS for a secure scheme.
fn connect(url: &url::Url, secure: bool) -> Result<Stream, String> {
    let host = url.host_str().ok_or("the address has no host")?.to_owned();
    let port = url
        .port_or_known_default()
        .unwrap_or(if secure { 443 } else { 80 });
    let tcp = TcpStream::connect((host.as_str(), port)).map_err(|error| error.to_string())?;
    tcp.set_nodelay(true).ok();
    if !secure {
        return Ok(Stream::Plain(tcp));
    }
    use rustls_platform_verifier::ConfigVerifierExt;
    let mut config =
        rustls::ClientConfig::with_platform_verifier().map_err(|error| error.to_string())?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let name = rustls::pki_types::ServerName::try_from(host).map_err(|error| error.to_string())?;
    let connection =
        rustls::ClientConnection::new(Arc::new(config), name).map_err(|error| error.to_string())?;
    Ok(Stream::Tls(Box::new(rustls::StreamOwned::new(
        connection, tcp,
    ))))
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Hold a WebSocket open to `url` until either end closes it.
///
/// `protocols` are the subprotocols the page offered, `origin` its origin
/// and `cookie` the `Cookie` header the browser would send there.
pub fn run_websocket(
    url: &url::Url,
    protocols: &[String],
    origin: &str,
    cookie: Option<&str>,
    commands: &mut dyn FnMut() -> Next,
    on_event: &mut dyn FnMut(SocketEvent),
) {
    let fail = |on_event: &mut dyn FnMut(SocketEvent), message: String| {
        on_event(SocketEvent::Error(message));
        on_event(SocketEvent::Close {
            code: 1006,
            reason: String::new(),
            clean: false,
        });
    };
    let secure = match url.scheme() {
        "wss" => true,
        "ws" => false,
        other => return fail(on_event, format!("not a WebSocket scheme: {other}")),
    };
    let stream = match connect(url, secure) {
        Ok(stream) => stream,
        Err(message) => return fail(on_event, message),
    };
    use tungstenite::client::IntoClientRequest;
    let mut request = match url.as_str().into_client_request() {
        Ok(request) => request,
        Err(error) => return fail(on_event, error.to_string()),
    };
    let headers = request.headers_mut();
    let mut set = |name: &'static str, value: &str| {
        if let Ok(value) = tungstenite::http::HeaderValue::from_str(value) {
            headers.insert(name, value);
        }
    };
    set("Origin", origin);
    set("User-Agent", crate::loader::USER_AGENT);
    if !protocols.is_empty() {
        set("Sec-WebSocket-Protocol", &protocols.join(", "));
    }
    if let Some(cookie) = cookie {
        set("Cookie", cookie);
    }
    let (mut socket, response) = match tungstenite::client(request, stream) {
        Ok(connected) => connected,
        Err(error) => return fail(on_event, error.to_string()),
    };
    let protocol = response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    on_event(SocketEvent::Open { protocol });
    // From here on a read that finds nothing gives the page's messages a turn.
    socket.get_ref().tcp().set_read_timeout(Some(POLL)).ok();

    loop {
        loop {
            match commands() {
                Next::Command(SocketCommand::Text(text)) => {
                    let _ = socket.send(tungstenite::Message::text(text));
                }
                Next::Command(SocketCommand::Binary(bytes)) => {
                    let _ = socket.send(tungstenite::Message::binary(bytes));
                }
                Next::Command(SocketCommand::Close(code, reason)) => {
                    let _ = socket.close(Some(tungstenite::protocol::CloseFrame {
                        code: code.into(),
                        reason: reason.into(),
                    }));
                }
                Next::Empty => break,
                // The page is gone: say goodbye and stop.
                Next::Gone => {
                    let _ = socket.close(None);
                    let _ = socket.flush();
                    return;
                }
            }
        }
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => on_event(SocketEvent::Text(text.to_string())),
            Ok(tungstenite::Message::Binary(bytes)) => {
                on_event(SocketEvent::Binary(bytes.to_vec()))
            }
            Ok(tungstenite::Message::Close(frame)) => {
                let (code, reason) = frame
                    .map(|frame| (u16::from(frame.code), frame.reason.to_string()))
                    .unwrap_or((1005, String::new()));
                // Answered by the library; the close completes on a later read.
                let _ = socket.flush();
                on_event(SocketEvent::Close {
                    code,
                    reason,
                    clean: true,
                });
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error)) if is_timeout(&error) => {
                let _ = socket.flush();
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                on_event(SocketEvent::Close {
                    code: 1000,
                    reason: String::new(),
                    clean: true,
                });
                return;
            }
            Err(error) => return fail(on_event, error.to_string()),
        }
    }
}

/// What one connection of an event stream came to.
enum StreamEnd {
    /// The page closed it.
    Closed,
    /// The server ended it or it broke: reconnect after this long.
    Reconnect(Duration),
    /// The server refused it for good.
    Failed,
}

/// Hold an event stream open to `url`, reconnecting as the server asks,
/// until the page closes it or the server refuses it.
pub fn run_event_stream(
    url: &url::Url,
    cookie: Option<&str>,
    commands: &mut dyn FnMut() -> Next,
    on_event: &mut dyn FnMut(SocketEvent),
) {
    let mut last_id = String::new();
    let mut retry = Duration::from_secs(3);
    for attempt in 0..1000 {
        match event_stream_once(
            url,
            cookie,
            &mut last_id,
            &mut retry,
            commands,
            on_event,
            attempt,
        ) {
            StreamEnd::Closed => return,
            StreamEnd::Failed => {
                on_event(SocketEvent::Close {
                    code: 0,
                    reason: String::new(),
                    clean: false,
                });
                return;
            }
            StreamEnd::Reconnect(wait) => {
                on_event(SocketEvent::Error("reconnecting".to_owned()));
                let deadline = std::time::Instant::now() + wait;
                while std::time::Instant::now() < deadline {
                    if page_closed(commands) {
                        return;
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }
}

fn event_stream_once(
    url: &url::Url,
    cookie: Option<&str>,
    last_id: &mut String,
    retry: &mut Duration,
    commands: &mut dyn FnMut() -> Next,
    on_event: &mut dyn FnMut(SocketEvent),
    attempt: u32,
) -> StreamEnd {
    let secure = match url.scheme() {
        "https" => true,
        "http" => false,
        _ => return StreamEnd::Failed,
    };
    let Ok(mut stream) = connect(url, secure) else {
        return if attempt == 0 {
            StreamEnd::Failed
        } else {
            StreamEnd::Reconnect(*retry)
        };
    };
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().to_owned(),
    };
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let mut head = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\nUser-Agent: {}\r\nConnection: keep-alive\r\n",
        crate::loader::USER_AGENT
    );
    if !last_id.is_empty() {
        head.push_str(&format!("Last-Event-ID: {last_id}\r\n"));
    }
    if let Some(cookie) = cookie {
        head.push_str(&format!("Cookie: {cookie}\r\n"));
    }
    head.push_str("\r\n");
    if stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.flush())
        .is_err()
    {
        return StreamEnd::Reconnect(*retry);
    }
    stream.tcp().set_read_timeout(Some(POLL)).ok();
    let mut reader = BufReader::new(stream);

    // The response head.
    let mut raw = Vec::new();
    loop {
        if page_closed(commands) {
            return StreamEnd::Closed;
        }
        let mut line = Vec::new();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => return StreamEnd::Reconnect(*retry),
            Ok(_) => {
                raw.extend_from_slice(&line);
                if line == b"\r\n" || line == b"\n" {
                    break;
                }
                if raw.len() > 64 * 1024 {
                    return StreamEnd::Failed;
                }
            }
            Err(error) if is_timeout(&error) => {
                raw.extend_from_slice(&line);
            }
            Err(_) => return StreamEnd::Reconnect(*retry),
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);
    if !matches!(response.parse(&raw), Ok(httparse::Status::Complete(_))) {
        return StreamEnd::Failed;
    }
    let header = |name: &str| {
        response
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(|header| String::from_utf8_lossy(header.value).to_ascii_lowercase())
    };
    let chunked = header("transfer-encoding").is_some_and(|value| value.contains("chunked"));
    let event_stream =
        header("content-type").is_some_and(|value| value.starts_with("text/event-stream"));
    if response.code != Some(200) || !event_stream {
        return StreamEnd::Failed;
    }
    on_event(SocketEvent::Open {
        protocol: String::new(),
    });

    let mut body = Body {
        reader,
        chunked,
        remaining: 0,
    };
    let mut parser = EventParser::default();
    let mut pending = Vec::new();
    loop {
        if page_closed(commands) {
            return StreamEnd::Closed;
        }
        let mut buffer = [0u8; 8192];
        match body.read(&mut buffer) {
            Ok(0) => return StreamEnd::Reconnect(*retry),
            Ok(count) => {
                pending.extend_from_slice(&buffer[..count]);
                // Whole lines only: a multi-byte character may be split.
                while let Some(end) = pending.iter().position(|&byte| byte == b'\n') {
                    let line: Vec<u8> = pending.drain(..=end).collect();
                    let text = String::from_utf8_lossy(&line);
                    for event in parser.line(text.trim_end_matches(['\n', '\r'])) {
                        match event {
                            Parsed::Event { kind, data, id } => {
                                on_event(SocketEvent::Event { kind, data, id });
                            }
                            Parsed::Retry(millis) => *retry = Duration::from_millis(millis),
                        }
                    }
                    *last_id = parser.last_id.clone();
                }
            }
            Err(error) if is_timeout(&error) => {}
            Err(_) => return StreamEnd::Reconnect(*retry),
        }
    }
}

fn page_closed(commands: &mut dyn FnMut() -> Next) -> bool {
    loop {
        match commands() {
            Next::Command(SocketCommand::Close(..)) | Next::Gone => return true,
            Next::Command(_) => {}
            Next::Empty => return false,
        }
    }
}

/// A response body, de-chunked when it was sent in chunks.
struct Body<R: BufRead> {
    reader: R,
    chunked: bool,
    /// Bytes left in the current chunk.
    remaining: usize,
}

impl<R: BufRead> Read for Body<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.chunked {
            return self.reader.read(buf);
        }
        if self.remaining == 0 {
            let mut size = String::new();
            while size.trim().is_empty() {
                size.clear();
                if self.reader.read_line(&mut size)? == 0 {
                    return Ok(0);
                }
            }
            let digits = size.trim().split(';').next().unwrap_or_default();
            self.remaining = usize::from_str_radix(digits, 16)
                .map_err(|_| std::io::Error::other("a bad chunk size"))?;
            if self.remaining == 0 {
                return Ok(0);
            }
        }
        let wanted = buf.len().min(self.remaining);
        let count = self.reader.read(&mut buf[..wanted])?;
        self.remaining -= count;
        Ok(count)
    }
}

/// What a line of an event stream completed.
#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Event {
        kind: String,
        data: String,
        id: String,
    },
    Retry(u64),
}

/// The event stream interpretation (HTML §9.2.6), a line at a time.
#[derive(Default)]
struct EventParser {
    kind: String,
    data: String,
    last_id: String,
}

impl EventParser {
    fn line(&mut self, line: &str) -> Vec<Parsed> {
        if line.is_empty() {
            if self.data.is_empty() {
                self.kind.clear();
                return Vec::new();
            }
            let mut data = std::mem::take(&mut self.data);
            if data.ends_with('\n') {
                data.pop();
            }
            let kind = std::mem::take(&mut self.kind);
            return vec![Parsed::Event {
                kind: if kind.is_empty() {
                    "message".to_owned()
                } else {
                    kind
                },
                data,
                id: self.last_id.clone(),
            }];
        }
        if line.starts_with(':') {
            return Vec::new();
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.kind = value.to_owned(),
            "data" => {
                self.data.push_str(value);
                self.data.push('\n');
            }
            "id" if !value.contains('\0') => self.last_id = value.to_owned(),
            "retry" => {
                if let Ok(millis) = value.parse::<u64>() {
                    return vec![Parsed::Retry(millis)];
                }
            }
            _ => {}
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_stream_is_read_a_line_at_a_time() {
        let mut parser = EventParser::default();
        let mut events = Vec::new();
        for line in [
            ": comment",
            "event: tick",
            "data: one",
            "data: two",
            "id: 7",
            "",
            "data:x",
            "",
            "retry: 50",
        ] {
            events.extend(parser.line(line));
        }
        assert_eq!(
            events,
            vec![
                Parsed::Event {
                    kind: "tick".into(),
                    data: "one\ntwo".into(),
                    id: "7".into()
                },
                Parsed::Event {
                    kind: "message".into(),
                    data: "x".into(),
                    id: "7".into()
                },
                Parsed::Retry(50),
            ]
        );
    }

    #[test]
    fn a_chunked_body_comes_out_whole() {
        let wire = b"4\r\ndata\r\n3;ext=1\r\n: x\r\n0\r\n\r\n";
        let mut body = Body {
            reader: BufReader::new(&wire[..]),
            chunked: true,
            remaining: 0,
        };
        let mut out = String::new();
        body.read_to_string(&mut out).unwrap();
        assert_eq!(out, "data: x");
    }
}
