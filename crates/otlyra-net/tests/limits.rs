//! The limits, against a server that misbehaves on purpose.
//!
//! The server is thirty lines of `std::net` rather than a real HTTP stack: these
//! tests need a socket that lies about `Content-Length` and redirects forever, and
//! a well-behaved server is exactly what will not do that.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use otlyra_net::{Limits, LoadRequest, Loader, NetError};

/// Serve `responses` in order, one per connection, then stop. Returns the base URL.
fn serve(responses: Vec<Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();

    thread::spawn(move || {
        let mut responses = responses.into_iter().cycle();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            read_request(&mut stream);
            let response = responses.next().expect("cycle never ends");
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });

    format!("http://127.0.0.1:{port}")
}

/// Drain the request headers so the client's write completes.
fn read_request(stream: &mut TcpStream) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        if line == "\r\n" {
            break;
        }
        line.clear();
    }
}

fn response(head: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = head.as_bytes().to_vec();
    bytes.extend_from_slice(body);
    bytes
}

fn loader(limits: Limits) -> Loader {
    otlyra_net::install_crypto_provider();
    Loader::with_limits(limits).expect("loader")
}

fn fetch(loader: &Loader, url: &str) -> Result<otlyra_net::LoadedResource, NetError> {
    let url = otlyra_net::normalize(url).expect("url");
    loader.fetch_blocking(LoadRequest::new(url))
}

const SMALL: Limits = Limits {
    max_body_bytes: 64,
    max_redirects: 3,
    timeout: std::time::Duration::from_secs(5),
};

#[test]
fn a_body_within_the_limit_arrives_intact() {
    let base = serve(vec![response(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: 13\r\nConnection: close\r\n\r\n",
        b"<p>hello</p>\n",
    )]);

    let resource = fetch(&loader(SMALL), &base).expect("fetch");
    assert_eq!(resource.status, 200);
    assert_eq!(resource.decode_text(), "<p>hello</p>\n");
    assert_eq!(resource.charset().as_deref(), Some("utf-8"));
}

#[test]
fn a_declared_length_over_the_limit_is_refused_before_the_body() {
    let base = serve(vec![response(
        "HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n",
        &vec![b'x'; 1024],
    )]);

    let error = fetch(&loader(SMALL), &base).expect_err("should refuse");
    assert!(
        matches!(error, NetError::BodyTooLarge { limit, .. } if limit == SMALL.max_body_bytes),
        "unexpected error: {error}"
    );
}

/// The interesting case: no `Content-Length` at all, so the cap can only come from
/// counting the bytes as they arrive.
#[test]
fn an_undeclared_body_over_the_limit_is_refused_while_streaming() {
    let base = serve(vec![response(
        "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n",
        &vec![b'x'; 4096],
    )]);

    let error = fetch(&loader(SMALL), &base).expect_err("should refuse");
    assert!(
        matches!(error, NetError::BodyTooLarge { limit, .. } if limit == SMALL.max_body_bytes),
        "unexpected error: {error}"
    );
}

#[test]
fn an_endless_redirect_chain_stops_at_the_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            read_request(&mut stream);
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.flush();
        }
    });

    let error =
        fetch(&loader(SMALL), &format!("http://127.0.0.1:{port}/")).expect_err("should stop");
    assert!(
        matches!(error, NetError::TooManyRedirects { limit, .. } if limit == SMALL.max_redirects),
        "unexpected error: {error}"
    );
}

#[test]
fn a_connection_closed_before_the_response_is_a_transport_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            read_request(&mut stream);
            drop(stream);
        }
    });

    let error =
        fetch(&loader(SMALL), &format!("http://127.0.0.1:{port}/")).expect_err("should fail");
    assert!(
        matches!(error, NetError::Transport { .. }),
        "unexpected error: {error}"
    );
}
