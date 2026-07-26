//! The redirect chain, now that it is ours.
//!
//! The client follows nothing, so every one of these is a rule this crate applies
//! and none of them were reachable while `reqwest` walked the chain internally.

mod common;

use common::{respond, serve};
use otlyra_net::{Body, Limits, LoadRequest, Loader, NetError};

const LIMITS: Limits = Limits {
    max_body_bytes: 64 * 1024,
    max_redirects: 3,
    timeout: std::time::Duration::from_secs(5),
};

fn loader() -> Loader {
    otlyra_net::install_crypto_provider();
    Loader::with_limits(LIMITS).expect("loader")
}

fn get(address: &str) -> Result<otlyra_net::LoadedResource, NetError> {
    let url = otlyra_net::normalize(address).expect("url");
    loader().fetch_blocking(LoadRequest::new(url))
}

fn post(address: &str, bytes: &[u8]) -> Result<otlyra_net::LoadedResource, NetError> {
    let url = otlyra_net::normalize(address).expect("url");
    loader().fetch_blocking(LoadRequest::post(
        url,
        Body {
            content_type: "application/x-www-form-urlencoded".to_owned(),
            bytes: bytes.to_vec(),
        },
    ))
}

/// The chain is walked, and every hop is a request the server saw.
#[test]
fn a_chain_is_followed_to_its_end() {
    let server = serve(|request| match request.path.as_str() {
        "/one" => respond("302 Found", &["Location: /two"], ""),
        "/two" => respond("302 Found", &["Location: /three"], ""),
        _ => respond("200 OK", &["Content-Type: text/plain"], "arrived"),
    });

    let resource = get(&server.url("/one")).expect("fetch");
    assert_eq!(resource.status, 200);
    assert_eq!(resource.decode_text(), "arrived");
    assert_eq!(resource.final_url, server.url("/three"));
    assert_eq!(server.paths(), ["/one", "/two", "/three"]);
}

/// A `Location` is usually relative, and is resolved against the address that
/// answered rather than against the one the chain started at.
#[test]
fn a_location_is_resolved_against_the_hop_that_sent_it() {
    let server = serve(|request| match request.path.as_str() {
        "/app/one" => respond("302 Found", &["Location: two"], ""),
        "/app/two" => respond("302 Found", &["Location: /root"], ""),
        "/root" => respond("302 Found", &["Location: {base}/absolute"], ""),
        _ => respond("200 OK", &[], "arrived"),
    });

    let resource = get(&server.url("/app/one")).expect("fetch");
    assert_eq!(resource.final_url, server.url("/absolute"));
    assert_eq!(
        server.paths(),
        ["/app/one", "/app/two", "/root", "/absolute"]
    );
}

/// What each code does to the method is what browsers do, not what the
/// specification first said. A body re-sent to an address that did not ask for it
/// is a second submission.
#[test]
fn a_redirected_post_becomes_a_get_except_where_it_must_not() {
    for status in ["301 Moved Permanently", "302 Found", "303 See Other"] {
        let owned = status.to_owned();
        let server = serve(move |request| match request.path.as_str() {
            "/submit" => respond(&owned, &["Location: /done"], ""),
            _ => respond("200 OK", &[], "arrived"),
        });

        post(&server.url("/submit"), b"name=value").expect("fetch");
        let requests = server.requests();
        assert_eq!(requests[0].method, "POST", "{status}");
        assert_eq!(requests[0].body, b"name=value", "{status}");
        assert_eq!(requests[1].method, "GET", "{status} should not resubmit");
        assert!(requests[1].body.is_empty(), "{status}");
        assert_eq!(
            requests[1].header("content-type"),
            None,
            "{status} should not describe a body it is not sending"
        );
    }

    // And the two codes that exist to say *no, really, send it again*.
    for status in ["307 Temporary Redirect", "308 Permanent Redirect"] {
        let owned = status.to_owned();
        let server = serve(move |request| match request.path.as_str() {
            "/submit" => respond(&owned, &["Location: /done"], ""),
            _ => respond("200 OK", &[], "arrived"),
        });

        post(&server.url("/submit"), b"name=value").expect("fetch");
        let requests = server.requests();
        assert_eq!(requests[1].method, "POST", "{status} keeps the method");
        assert_eq!(requests[1].body, b"name=value", "{status} replays the body");
    }
}

/// A page from the internet reaching the filesystem through a redirect is the
/// oldest browser vulnerability there is.
#[test]
fn a_redirect_out_of_http_is_refused() {
    let server = serve(|_| respond("302 Found", &["Location: file:///etc/passwd"], ""));

    let error = get(&server.url("/")).expect_err("should refuse");
    assert!(
        matches!(error, NetError::UnsupportedScheme { ref scheme } if scheme == "file"),
        "unexpected error: {error}"
    );
    assert_eq!(server.paths().len(), 1, "the second hop is never made");
}

/// A redirect code with nowhere to go is an ordinary response, which is what
/// servers send and what every browser renders.
#[test]
fn a_redirect_without_a_location_is_just_a_response() {
    let server = serve(|_| respond("302 Found", &["Content-Type: text/html"], "<p>here</p>"));

    let resource = get(&server.url("/")).expect("fetch");
    assert_eq!(resource.status, 302);
    assert_eq!(resource.decode_text(), "<p>here</p>");
    assert_eq!(server.paths(), ["/"]);
}

/// A `Location` that is not text is a broken response, and says so rather than
/// being followed to somewhere unexpected. Header values are bytes, and a server
/// writing a path in some legacy encoding sends bytes no address can be read out
/// of.
#[test]
fn a_location_that_is_not_an_address_is_named_as_one() {
    let server = common::serve_bytes(|_| {
        let mut answer = b"HTTP/1.1 302 Found\r\nLocation: /".to_vec();
        answer.extend_from_slice(&[0xFF, 0xFE]);
        answer.extend_from_slice(b"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        answer
    });

    let error = get(&server.url("/")).expect_err("should refuse");
    assert!(
        matches!(error, NetError::BadRedirect { .. }),
        "unexpected error: {error}"
    );
    assert_eq!(server.paths().len(), 1, "the second hop is never made");
}

/// The limit that `Policy::limited` used to enforce, enforced here, and counting
/// hops rather than requests.
#[test]
fn a_chain_stops_at_the_limit() {
    let server = serve(|_| respond("302 Found", &["Location: /next"], ""));

    let error = get(&server.url("/")).expect_err("should stop");
    assert!(
        matches!(error, NetError::TooManyRedirects { limit, .. } if limit == LIMITS.max_redirects),
        "unexpected error: {error}"
    );
    assert_eq!(
        server.paths().len(),
        LIMITS.max_redirects + 1,
        "the first request plus the redirects it was allowed"
    );
}

/// A chain exactly at the limit still arrives.
#[test]
fn a_chain_at_the_limit_arrives() {
    let server = serve(|request| match request.path.as_str() {
        "/1" => respond("302 Found", &["Location: /2"], ""),
        "/2" => respond("302 Found", &["Location: /3"], ""),
        "/3" => respond("302 Found", &["Location: /4"], ""),
        _ => respond("200 OK", &[], "arrived"),
    });

    let resource = get(&server.url("/1")).expect("fetch");
    assert_eq!(resource.decode_text(), "arrived");
    assert_eq!(server.paths(), ["/1", "/2", "/3", "/4"]);
}

/// The headers reported are the ones put on the request that answered, not the
/// ones put on the request that started the chain — which is what an inspector's
/// *Request* pane means.
///
/// A redirected `POST` is what shows the difference: the first hop describes a
/// body and the last one does not.
#[test]
fn the_headers_reported_are_the_last_hops() {
    let dropped = serve(|request| match request.path.as_str() {
        "/submit" => respond("303 See Other", &["Location: /done"], ""),
        _ => respond("200 OK", &[], "arrived"),
    });
    let resource = post(&dropped.url("/submit"), b"name=value").expect("fetch");
    assert!(
        !resource
            .request_headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type")),
        "the last hop sent no body, so it described none: {:?}",
        resource.request_headers
    );

    // And where the body *is* replayed, the last hop describes it — same chain,
    // one different status code.
    let kept = serve(|request| match request.path.as_str() {
        "/submit" => respond("307 Temporary Redirect", &["Location: /done"], ""),
        _ => respond("200 OK", &[], "arrived"),
    });
    let resource = post(&kept.url("/submit"), b"name=value").expect("fetch");
    assert!(
        resource
            .request_headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type")),
        "the last hop sent the body again: {:?}",
        resource.request_headers
    );
}
