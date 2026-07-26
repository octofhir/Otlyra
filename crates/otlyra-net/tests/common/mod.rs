//! A server that says exactly what a test needs it to say, and remembers what it
//! was asked.
//!
//! Sixty lines of `std::net` rather than an HTTP stack, for the same reason the
//! limits tests have their own: these tests need a server that redirects in a
//! circle, sets a cookie on a hop nobody sees the body of, and reports the
//! headers it actually received. A well-behaved server is what will not do that.
//!
//! One request per connection, answered inline, so the order a test asserts on is
//! the order the client made them in.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// One request as it arrived.
#[derive(Clone, Debug)]
pub struct Request {
    /// `GET`, `POST`.
    pub method: String,
    /// The path and query, as written on the request line.
    pub path: String,
    /// Header names lowercased, values as sent, in arrival order. A header sent
    /// twice is here twice.
    pub headers: Vec<(String, String)>,
    /// The body, where `Content-Length` said there was one.
    pub body: Vec<u8>,
}

impl Request {
    /// The first value of `name`, if it was sent.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(sent, _)| sent == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A running server and the record of what it was asked.
pub struct Server {
    /// `http://127.0.0.1:port`, with no trailing slash.
    pub base: String,
    seen: Arc<Mutex<Vec<Request>>>,
}

impl Server {
    /// The address of `path` on this server.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Every request received, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.seen.lock().expect("not poisoned").clone()
    }

    /// The paths asked for, in order — the shape of a redirect chain.
    pub fn paths(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .map(|request| request.path)
            .collect()
    }
}

/// Start a server that answers each request with `reply`.
///
/// The reply is the whole response, head and body, with `\r\n` line endings. Two
/// conveniences, because both are needed in every second test: `{base}` in the
/// reply is replaced with this server's own address, so an absolute `Location`
/// can be written before the port is known, and a `Connection: close` header is
/// appended when the reply does not carry one.
pub fn serve(reply: impl Fn(&Request) -> String + Send + Sync + 'static) -> Server {
    serve_bytes(move |request| reply(request).into_bytes())
}

/// The same, for a test whose response is not text.
///
/// A header value may be any bytes, and a browser meets ones that are not UTF-8;
/// a reply written as a `String` cannot produce one.
pub fn serve_bytes(reply: impl Fn(&Request) -> Vec<u8> + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let base = format!("http://127.0.0.1:{port}");
    let seen: Arc<Mutex<Vec<Request>>> = Arc::new(Mutex::new(Vec::new()));

    let recorded = Arc::clone(&seen);
    let answering = base.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let answer = replace_base(reply(&request), answering.as_bytes());
            recorded.lock().expect("not poisoned").push(request);
            let _ = stream.write_all(&answer);
            let _ = stream.flush();
        }
    });

    Server { base, seen }
}

/// A response head and body, with the lengths and the close a client expects.
pub fn respond(status: &str, headers: &[&str], body: &str) -> String {
    let mut head = format!("HTTP/1.1 {status}\r\n");
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    head.push_str("Connection: close\r\n\r\n");
    head.push_str(body);
    head
}

/// Substitute `{base}` with the server's own address, on bytes rather than on a
/// string, so a reply carrying a header value that is not UTF-8 still gets it.
fn replace_base(answer: Vec<u8>, base: &[u8]) -> Vec<u8> {
    const MARKER: &[u8] = b"{base}";
    let mut out = Vec::with_capacity(answer.len());
    let mut rest = answer.as_slice();
    while let Some(at) = rest
        .windows(MARKER.len())
        .position(|window| window == MARKER)
    {
        out.extend_from_slice(&rest[..at]);
        out.extend_from_slice(base);
        rest = &rest[at + MARKER.len()..];
    }
    out.extend_from_slice(rest);
    out
}

fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);

    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();

    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((name, value)) = header.trim_end().split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }

    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }

    Some(Request {
        method,
        path,
        headers,
        body,
    })
}
