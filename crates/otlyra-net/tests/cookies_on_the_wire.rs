//! The jar attached to the loader, against a real socket.
//!
//! The one that matters is [`a_cookie_set_on_a_redirect_is_kept_and_sent`]: it is
//! the shape of every sign-in on the web, and it is exactly what a client
//! following its own redirects cannot do.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{respond, serve};
use otlyra_net::cookie::Jar;
use otlyra_net::{Body, Limits, LoadRequest, Loader, SharedJar};

const LIMITS: Limits = Limits {
    max_body_bytes: 64 * 1024,
    max_redirects: 5,
    timeout: Duration::from_secs(5),
};

/// A loader and the jar it shares, so a test can look in the jar afterwards.
fn loader() -> (Loader, SharedJar) {
    otlyra_net::install_crypto_provider();
    let jar: SharedJar = Arc::new(Mutex::new(Jar::new()));
    let loader = Loader::with_limits(LIMITS)
        .expect("loader")
        .with_jar(Arc::clone(&jar));
    (loader, jar)
}

fn url(address: &str) -> url::Url {
    otlyra_net::normalize(address).expect("url")
}

/// What the jar holds, as `name=value`, sorted so a test can state it.
fn held(jar: &SharedJar) -> Vec<String> {
    let jar = jar.lock().expect("not poisoned");
    let mut names: Vec<String> = jar
        .all()
        .iter()
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect();
    names.sort();
    names
}

/// The whole reason the chain is ours. A sign-in sets its session on the hop that
/// redirects, and the browser has to both keep it and send it on the hop after.
#[test]
fn a_cookie_set_on_a_redirect_is_kept_and_sent() {
    let server = serve(|request| match request.path.as_str() {
        "/login" => respond(
            "302 Found",
            &["Set-Cookie: session=abc; Path=/", "Location: /home"],
            "",
        ),
        _ => respond("200 OK", &[], "signed in"),
    });

    let (loader, jar) = loader();
    let resource = loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/login"))))
        .expect("fetch");
    assert_eq!(resource.decode_text(), "signed in");

    let requests = server.requests();
    assert_eq!(requests[0].header("cookie"), None, "nothing to send yet");
    assert_eq!(
        requests[1].header("cookie"),
        Some("session=abc"),
        "the cookie the redirect set travels on the hop after it"
    );
    assert_eq!(held(&jar), ["session=abc"]);
}

/// And it is still there on the next fetch, which is what staying signed in is.
#[test]
fn a_cookie_kept_is_sent_on_the_next_request() {
    let server = serve(|request| match request.path.as_str() {
        "/set" => respond("200 OK", &["Set-Cookie: a=1; Path=/"], ""),
        _ => respond("200 OK", &[], "hello"),
    });

    let (loader, _jar) = loader();
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/set"))))
        .expect("fetch");
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/other"))))
        .expect("fetch");

    assert_eq!(server.requests()[1].header("cookie"), Some("a=1"));
}

/// Several `Set-Cookie` headers on one response are several cookies. They are
/// never comma-joined, whatever a date in one of them looks like.
#[test]
fn every_set_cookie_header_on_a_response_is_taken() {
    let server = serve(|_| {
        respond(
            "200 OK",
            &[
                "Set-Cookie: a=1; Path=/",
                "Set-Cookie: b=2; Path=/; Expires=Sun, 06 Nov 2094 08:49:37 GMT",
                "Set-Cookie: c=3; Path=/",
            ],
            "",
        )
    });

    let (loader, jar) = loader();
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/"))))
        .expect("fetch");
    assert_eq!(held(&jar), ["a=1", "b=2", "c=3"]);
}

/// A loader with no jar sends nothing and keeps nothing, which is what the
/// one-shot `--url` fetch and most tests want.
#[test]
fn a_loader_without_a_jar_has_no_cookies() {
    let server = serve(|_| respond("200 OK", &["Set-Cookie: a=1; Path=/"], ""));

    otlyra_net::install_crypto_provider();
    let loader = Loader::with_limits(LIMITS).expect("loader");
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/"))))
        .expect("fetch");
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/again"))))
        .expect("fetch");

    assert_eq!(server.requests()[1].header("cookie"), None);
    assert!(loader.jar().is_none());
}

/// A cookie the site was not entitled to set does not travel, and the response it
/// arrived on is still a response.
#[test]
fn a_refused_cookie_costs_only_itself() {
    let server = serve(|request| match request.path.as_str() {
        "/" => respond(
            "200 OK",
            &[
                "Set-Cookie: good=1; Path=/",
                "Set-Cookie: stolen=2; Path=/; Domain=example.com",
                "Set-Cookie: naked=3; Path=/; SameSite=None",
            ],
            "here",
        ),
        _ => respond("200 OK", &[], ""),
    });

    let (loader, jar) = loader();
    let resource = loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/"))))
        .expect("fetch");
    assert_eq!(resource.decode_text(), "here");
    assert_eq!(held(&jar), ["good=1"]);
}

/// `SameSite` reads the request the caller described. A page on another site
/// asking for a picture gets neither the `Strict` cookie nor the `Lax` one.
#[test]
fn same_site_reads_who_asked() {
    let server = serve(|request| match request.path.as_str() {
        "/set" => respond(
            "200 OK",
            &[
                "Set-Cookie: strict=1; Path=/; SameSite=Strict",
                "Set-Cookie: lax=2; Path=/; SameSite=Lax",
            ],
            "",
        ),
        _ => respond("200 OK", &[], ""),
    });

    let (loader, _jar) = loader();
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/set"))))
        .expect("fetch");

    // Nobody asked: a typed address, and everything goes.
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/typed"))))
        .expect("fetch");

    // Another site's page followed a link here: the `Strict` one stays behind.
    loader
        .fetch_blocking(
            LoadRequest::new(url(&server.url("/link")))
                .from(url("https://elsewhere.test/"))
                .navigating(),
        )
        .expect("fetch");

    // Another site's page asked for a picture: neither goes.
    loader
        .fetch_blocking(
            LoadRequest::new(url(&server.url("/picture"))).from(url("https://elsewhere.test/")),
        )
        .expect("fetch");

    let requests = server.requests();
    assert_eq!(requests[1].header("cookie"), Some("strict=1; lax=2"));
    assert_eq!(requests[2].header("cookie"), Some("lax=2"));
    assert_eq!(requests[3].header("cookie"), None);
}

/// A cross-site form post is top-level and is not safe, which is the line `Lax`
/// was drawn at — and it is the request `Lax` exists to keep a cookie off.
#[test]
fn a_cross_site_post_is_not_a_lax_navigation() {
    let server = serve(|request| match request.path.as_str() {
        "/set" => respond("200 OK", &["Set-Cookie: lax=1; Path=/; SameSite=Lax"], ""),
        _ => respond("200 OK", &[], ""),
    });

    let (loader, _jar) = loader();
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/set"))))
        .expect("fetch");
    loader
        .fetch_blocking(
            LoadRequest::post(
                url(&server.url("/submit")),
                Body {
                    content_type: "application/x-www-form-urlencoded".to_owned(),
                    bytes: b"x=1".to_vec(),
                },
            )
            .from(url("https://elsewhere.test/"))
            .navigating(),
        )
        .expect("fetch");

    assert_eq!(server.requests()[1].header("cookie"), None);
}

/// Once a chain has left the site that started it, everything after it is still
/// away — however same-site the last hop looks on its own.
///
/// One socket, reached under two names. `localhost` and `127.0.0.1` are the same
/// machine and are two sites, which is what makes this testable without a second
/// server and without a name resolver.
#[test]
fn a_chain_that_left_the_site_does_not_come_back_same_site() {
    let server = serve(|request| match request.path.as_str() {
        "/set" => respond(
            "200 OK",
            &["Set-Cookie: strict=1; Path=/; SameSite=Strict"],
            "",
        ),
        // Away to the other name, and straight back to this one.
        "/out" => respond("302 Found", &["Location: {alias}/back"], ""),
        "/back" => respond("302 Found", &["Location: {base}/home"], ""),
        _ => respond("200 OK", &[], "home"),
    });

    let (loader, _jar) = loader();
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/set"))))
        .expect("fetch");

    // Its own site, straight there: the strict cookie goes. Without this the test
    // below would pass on a jar that simply never sends anything.
    loader
        .fetch_blocking(
            LoadRequest::new(url(&server.url("/home")))
                .from(url(&server.url("/")))
                .navigating(),
        )
        .expect("fetch");
    assert_eq!(server.requests()[1].header("cookie"), Some("strict=1"));

    // Out through the other name and back. The last hop is `127.0.0.1` asking
    // `127.0.0.1`, which looks same-site on its own — and is not, because the
    // chain left.
    loader
        .fetch_blocking(
            LoadRequest::new(url(&server.url("/out")))
                .from(url(&server.url("/")))
                .navigating(),
        )
        .expect("fetch");

    let requests = server.requests();
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        ["/set", "/home", "/out", "/back", "/home"]
    );
    assert_eq!(
        requests.last().expect("a request").header("cookie"),
        None,
        "the chain left the site, so it is away for the rest of it"
    );
}

/// A cookie may be set on the last hop of a chain as well as on the ones before.
#[test]
fn the_last_hop_sets_cookies_too() {
    let server = serve(|request| match request.path.as_str() {
        "/one" => respond(
            "302 Found",
            &["Set-Cookie: first=1; Path=/", "Location: /two"],
            "",
        ),
        _ => respond("200 OK", &["Set-Cookie: second=2; Path=/"], "done"),
    });

    let (loader, jar) = loader();
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/one"))))
        .expect("fetch");
    assert_eq!(held(&jar), ["first=1", "second=2"]);
}

/// The header the jar produced is on the request the inspector reads back.
#[test]
fn the_cookie_header_is_among_the_headers_reported() {
    let server = serve(|request| match request.path.as_str() {
        "/set" => respond("200 OK", &["Set-Cookie: a=1; Path=/"], ""),
        _ => respond("200 OK", &[], ""),
    });

    let (loader, _jar) = loader();
    loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/set"))))
        .expect("fetch");
    let resource = loader
        .fetch_blocking(LoadRequest::new(url(&server.url("/again"))))
        .expect("fetch");

    assert!(
        resource
            .request_headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("cookie") && value == "a=1"),
        "{:?}",
        resource.request_headers
    );
}
