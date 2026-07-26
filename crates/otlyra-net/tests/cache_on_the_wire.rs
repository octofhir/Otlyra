//! The cache attached to the loader, against a real socket.
//!
//! Every one of these is asserted on *what the server saw*: the whole point of a
//! cache is the request that was not made, and only the server can say whether it
//! was.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{respond, serve};
use otlyra_net::cache::Cache;
use otlyra_net::{Limits, LoadRequest, Loader, SharedCache};

const LIMITS: Limits = Limits {
    max_body_bytes: 64 * 1024,
    max_redirects: 5,
    timeout: Duration::from_secs(5),
};

fn cached() -> (Loader, SharedCache) {
    otlyra_net::install_crypto_provider();
    let cache: SharedCache = Arc::new(Mutex::new(Cache::new()));
    let loader = Loader::with_limits(LIMITS)
        .expect("loader")
        .with_cache(Arc::clone(&cache));
    (loader, cache)
}

fn get(loader: &Loader, address: &str) -> otlyra_net::LoadedResource {
    let url = otlyra_net::normalize(address).expect("url");
    loader.fetch_blocking(LoadRequest::new(url)).expect("fetch")
}

/// The point of the exercise: the second request is not made at all.
#[test]
fn a_fresh_response_is_answered_without_a_request() {
    let server = serve(|_| {
        respond(
            "200 OK",
            &["Cache-Control: max-age=3600", "Content-Type: text/plain"],
            "hello",
        )
    });

    let (loader, cache) = cached();
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "hello");
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "hello");
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "hello");

    assert_eq!(server.paths(), ["/a"], "asked once, answered three times");
    assert_eq!(
        cache.lock().expect("not poisoned").counts(),
        (2, 0),
        "two hits and no misses after the first, which was neither"
    );
    // And what comes back out says everything the response said.
    let again = get(&loader, &server.url("/a"));
    assert_eq!(again.content_type.as_deref(), Some("text/plain"));
    assert_eq!(again.status, 200);
    assert_eq!(again.final_url, server.url("/a"));
}

/// Stale, with something to ask with: one conditional request, and the body is
/// not sent again.
#[test]
fn a_stale_response_is_asked_about_and_the_body_is_not_resent() {
    let server = serve(|request| {
        if request.header("if-none-match") == Some("\"v1\"") {
            return respond("304 Not Modified", &["Cache-Control: max-age=3600"], "");
        }
        respond(
            "200 OK",
            &["Cache-Control: max-age=0", "ETag: \"v1\""],
            "the body",
        )
    });

    let (loader, _cache) = cached();
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "the body");
    // Stale immediately, so the second asks — and gets the body from the cache.
    let second = get(&loader, &server.url("/a"));
    assert_eq!(second.decode_text(), "the body");
    assert_eq!(second.status, 200, "a 304 is not what the caller is handed");

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].header("if-none-match"),
        None,
        "nothing to ask with yet"
    );
    assert_eq!(requests[1].header("if-none-match"), Some("\"v1\""));

    // The 304 said `max-age=3600`, so the third asks nothing at all.
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "the body");
    assert_eq!(server.paths().len(), 2, "the 304 made it fresh again");
}

/// A `Last-Modified` is asked with too, where that is all there is.
#[test]
fn a_last_modified_is_asked_with() {
    let server = serve(|request| {
        if request.header("if-modified-since").is_some() {
            return respond("304 Not Modified", &["Cache-Control: max-age=3600"], "");
        }
        respond(
            "200 OK",
            &[
                "Cache-Control: max-age=0",
                "Last-Modified: Sun, 06 Nov 1994 08:49:37 GMT",
            ],
            "the body",
        )
    });

    let (loader, _cache) = cached();
    get(&loader, &server.url("/a"));
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "the body");
    assert_eq!(
        server.requests()[1].header("if-modified-since"),
        Some("Sun, 06 Nov 1994 08:49:37 GMT")
    );
}

/// `no-store` means it. Nothing is kept, so every request is made.
#[test]
fn no_store_is_fetched_every_time() {
    let server = serve(|_| respond("200 OK", &["Cache-Control: no-store"], "fresh"));

    let (loader, cache) = cached();
    for _ in 0..3 {
        assert_eq!(get(&loader, &server.url("/a")).decode_text(), "fresh");
    }
    assert_eq!(server.paths().len(), 3);
    assert!(cache.lock().expect("not poisoned").is_empty());
}

/// `no-cache` is the other one: kept, and asked about every time.
#[test]
fn no_cache_is_kept_and_asked_about() {
    let server = serve(|request| {
        if request.header("if-none-match") == Some("\"v1\"") {
            return respond("304 Not Modified", &["Cache-Control: no-cache"], "");
        }
        respond(
            "200 OK",
            &["Cache-Control: no-cache", "ETag: \"v1\""],
            "the body",
        )
    });

    let (loader, cache) = cached();
    for _ in 0..3 {
        assert_eq!(get(&loader, &server.url("/a")).decode_text(), "the body");
    }
    assert_eq!(server.paths().len(), 3, "asked every time");
    assert_eq!(
        server.requests()[2].header("if-none-match"),
        Some("\"v1\""),
        "and asked conditionally, so the body crossed once"
    );
    assert_eq!(
        cache.lock().expect("not poisoned").len(),
        1,
        "and it is kept"
    );
}

/// A loader with no cache fetches everything, which is what the one-shot `--url`
/// mode and most of this crate's tests want.
#[test]
fn a_loader_without_a_cache_fetches_every_time() {
    let server = serve(|_| respond("200 OK", &["Cache-Control: max-age=3600"], "hello"));

    otlyra_net::install_crypto_provider();
    let loader = Loader::with_limits(LIMITS).expect("loader");
    get(&loader, &server.url("/a"));
    get(&loader, &server.url("/a"));
    assert_eq!(server.paths().len(), 2);
    assert!(loader.cache().is_none());
}

/// A request with a body changes something, so its answer is not reused and does
/// not unseat what is stored.
#[test]
fn a_post_is_neither_answered_nor_stored_from_the_cache() {
    let server = serve(|request| {
        respond(
            "200 OK",
            &["Cache-Control: max-age=3600"],
            &format!("{} {}", request.method, request.path),
        )
    });

    let (loader, cache) = cached();
    let url = otlyra_net::normalize(&server.url("/a")).expect("url");
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "GET /a");

    let posted = loader
        .fetch_blocking(LoadRequest::post(
            url,
            otlyra_net::Body {
                content_type: "text/plain".to_owned(),
                bytes: b"x".to_vec(),
            },
        ))
        .expect("fetch");
    assert_eq!(posted.decode_text(), "POST /a", "the network answered it");
    assert_eq!(server.paths().len(), 2);
    assert_eq!(
        cache.lock().expect("not poisoned").len(),
        1,
        "and the GET is still there"
    );
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "GET /a");
    assert_eq!(server.paths().len(), 2, "still from the cache");
}

/// The whole chain is stored under the address the reader asked for, because that
/// is the address a later request is made for.
#[test]
fn a_redirect_chain_is_answered_from_where_it_started() {
    let server = serve(|request| match request.path.as_str() {
        "/one" => respond("302 Found", &["Location: /two"], ""),
        _ => respond("200 OK", &["Cache-Control: max-age=3600"], "arrived"),
    });

    let (loader, _cache) = cached();
    let first = get(&loader, &server.url("/one"));
    assert_eq!(first.decode_text(), "arrived");
    assert_eq!(first.final_url, server.url("/two"));

    let second = get(&loader, &server.url("/one"));
    assert_eq!(second.decode_text(), "arrived");
    assert_eq!(
        second.final_url,
        server.url("/two"),
        "and it still says where it came from"
    );
    assert_eq!(server.paths(), ["/one", "/two"], "the chain ran once");
}

/// A `Vary` the request does not match is a request that goes to the network.
#[test]
fn vary_sends_a_request_the_entry_does_not_answer() {
    let server = serve(|_| {
        respond(
            "200 OK",
            &["Cache-Control: max-age=3600", "Vary: Cookie"],
            "body",
        )
    });

    let (matching, _cache) = cached();
    get(&matching, &server.url("/a"));
    get(&matching, &server.url("/a"));
    assert_eq!(
        server.paths().len(),
        1,
        "no cookie either time, so it matches"
    );

    // And `Vary: *`, which says the answer depends on something not in the
    // headers at all and is therefore never reused.
    let never = serve(|_| {
        respond(
            "200 OK",
            &["Cache-Control: max-age=3600", "Vary: *"],
            "body",
        )
    });
    let (starred, _cache) = cached();
    get(&starred, &never.url("/a"));
    get(&starred, &never.url("/a"));
    assert_eq!(never.paths().len(), 2);
}

/// A validator belongs to the address that sent it.
///
/// A redirect chain is kept whole under the address the reader asked for, so the
/// `ETag` on the entry is the *last* hop's. Asking the first hop whether it
/// matches it is asking one resource about another's, and a server that answered
/// yes would hand back the wrong body.
#[test]
fn a_validator_is_not_asked_of_the_wrong_address() {
    let server = serve(|request| match request.path.as_str() {
        "/one" => respond("302 Found", &["Location: /two"], ""),
        _ => respond(
            "200 OK",
            &["Cache-Control: max-age=0", "ETag: \"two\""],
            "arrived",
        ),
    });

    let (loader, _cache) = cached();
    get(&loader, &server.url("/one"));
    get(&loader, &server.url("/one"));

    let requests = server.requests();
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        ["/one", "/two", "/one", "/two"]
    );
    for request in &requests {
        assert_eq!(
            request.header("if-none-match"),
            None,
            "{} was asked about an ETag that is not its",
            request.path
        );
    }

    // And where the address that was stored is the address that answered, it *is*
    // asked with its own — which is what makes the rule above a rule rather than
    // a cache that never revalidates.
    let direct = serve(|_| {
        respond(
            "200 OK",
            &["Cache-Control: max-age=0", "ETag: \"mine\""],
            "body",
        )
    });
    let (straight, _cache) = cached();
    get(&straight, &direct.url("/a"));
    get(&straight, &direct.url("/a"));
    assert_eq!(
        direct.requests()[1].header("if-none-match"),
        Some("\"mine\"")
    );
}

/// `Age` says the response was already old when it arrived. A cache that starts
/// its clock at the moment the bytes land serves things it should have asked
/// about — including on a `304`, which is where it is easiest to forget.
#[test]
fn a_response_that_arrives_old_is_old() {
    let server = serve(|_| {
        respond(
            "200 OK",
            &["Cache-Control: max-age=60", "Age: 3600", "ETag: \"v1\""],
            "body",
        )
    });

    let (loader, _cache) = cached();
    get(&loader, &server.url("/a"));
    // An hour old on arrival against a minute of freshness: the second request is
    // made, and it is made conditionally.
    get(&loader, &server.url("/a"));
    let requests = server.requests();
    assert_eq!(requests.len(), 2, "it was stale before it landed");
    assert_eq!(requests[1].header("if-none-match"), Some("\"v1\""));

    // And a `304` that says it is old again leaves it old, so the third asks too.
    let ageing = serve(|request| {
        if request.header("if-none-match") == Some("\"v1\"") {
            return respond(
                "304 Not Modified",
                &["Cache-Control: max-age=60", "Age: 3600"],
                "",
            );
        }
        respond(
            "200 OK",
            &["Cache-Control: max-age=60", "Age: 3600", "ETag: \"v1\""],
            "body",
        )
    });
    let (revalidating, _cache) = cached();
    for _ in 0..3 {
        assert_eq!(get(&revalidating, &ageing.url("/a")).decode_text(), "body");
    }
    assert_eq!(
        ageing.paths().len(),
        3,
        "a 304 that arrives an hour old is an hour old"
    );
}

/// An address that answered directly and now redirects.
///
/// The entry was stored with that address's own `ETag`, so the revalidation
/// rightly carries it — and the hop it is redirected to is a different resource
/// that must not be asked the same question.
#[test]
fn a_condition_does_not_follow_a_redirect_that_appeared_later() {
    let moved = Arc::new(Mutex::new(false));
    let server = serve({
        let moved = Arc::clone(&moved);
        move |request| {
            let has_moved = *moved.lock().expect("not poisoned");
            match request.path.as_str() {
                "/a" if has_moved => respond("302 Found", &["Location: /b"], ""),
                "/a" => respond(
                    "200 OK",
                    &["Cache-Control: max-age=0", "ETag: \"here\""],
                    "here",
                ),
                _ => respond("200 OK", &["Cache-Control: max-age=0"], "moved"),
            }
        }
    });

    let (loader, _cache) = cached();
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "here");
    *moved.lock().expect("not poisoned") = true;
    assert_eq!(get(&loader, &server.url("/a")).decode_text(), "moved");

    let requests = server.requests();
    assert_eq!(
        requests.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
        ["/a", "/a", "/b"]
    );
    assert_eq!(
        requests[1].header("if-none-match"),
        Some("\"here\""),
        "the address that was stored is asked with its own"
    );
    assert_eq!(
        requests[2].header("if-none-match"),
        None,
        "and what it redirected to is not"
    );
}
