//! Turning what a person typed into a URL we are willing to fetch.

use url::Url;

use crate::loader::NetError;

/// Schemes a browser will navigate to.
///
/// `file` is here and not in [`FETCHABLE`]: it is a scheme the browser can show
/// and the network stack cannot fetch, and keeping the two lists apart is what
/// stops one from being mistaken for the other.
const NAVIGABLE: [&str; 3] = ["http", "https", "file"];

/// Schemes this crate will fetch over the network.
const FETCHABLE: [&str; 2] = ["http", "https"];

/// Resolve user input to an absolute URL.
///
/// A bare `google.com` becomes `https://google.com/`, never `http://`: guessing
/// the insecure scheme is how a typed hostname turns into a cleartext request that
/// anyone on the path can rewrite. An explicit `http://` is still honoured, because
/// then it is the caller's decision and not ours.
pub fn normalize(input: &str) -> Result<Url, NetError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(NetError::EmptyUrl);
    }

    let parse = |candidate: &str| {
        Url::parse(candidate).map_err(|source| NetError::InvalidUrl {
            input: input.to_owned(),
            source,
        })
    };

    // `://` is what distinguishes a spelled-out scheme from a bare `host:port`.
    // `localhost:8080` parses as a URL whose scheme is `localhost`, so trusting the
    // parser alone would refuse a perfectly ordinary address.
    let parsed = if input.contains("://") {
        parse(input)?
    } else {
        match Url::parse(input) {
            Ok(url) if NAVIGABLE.contains(&url.scheme()) => url,
            _ => parse(&format!("https://{input}"))?,
        }
    };

    if !NAVIGABLE.contains(&parsed.scheme()) {
        return Err(NetError::UnsupportedScheme {
            scheme: parsed.scheme().to_owned(),
        });
    }
    // A `file:` URL has no host, and that is not the same as a missing one.
    if parsed.scheme() != "file" && parsed.host().is_none() {
        return Err(NetError::MissingHost {
            url: parsed.to_string(),
        });
    }

    Ok(parsed)
}

/// Whether this crate can fetch `url` over the network.
///
/// A `file:` URL is navigable and not fetchable: the browser can show one, and
/// the thing that reads it is the filesystem, not an HTTP client. Asking the
/// client anyway would get a confusing transport error instead of a clear refusal.
pub fn is_fetchable(url: &Url) -> bool {
    FETCHABLE.contains(&url.scheme())
}

/// Whether a document at `from` may navigate to `to`.
///
/// The rule this enforces is §14's: `file:` is reachable from the address bar and
/// from a `file:` document's own relative links, and **never** from a document
/// fetched over the network. A page from the internet that can open
/// `file:///etc/passwd` — or, worse, follow a redirect into one — is the oldest
/// browser vulnerability there is.
pub fn may_navigate(from: Option<&str>, to: &Url) -> bool {
    if to.scheme() != "file" {
        return true;
    }
    match from {
        // Typed, or opened from the command line. The user is allowed to ask.
        None | Some("") => true,
        Some(from) => Url::parse(from).is_ok_and(|url| url.scheme() == "file"),
    }
}

/// Resolve `href` against the document it appeared in.
///
/// A page's links are mostly relative, and a relative link is meaningless without
/// the address it was found at — which is why this takes the base rather than
/// guessing one. Anything that is already absolute passes through unchanged.
pub fn resolve(base: &str, href: &str) -> Option<String> {
    let base = Url::parse(base).ok()?;
    base.join(href).ok().map(|url| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str) -> String {
        normalize(input).expect("should normalize").to_string()
    }

    #[test]
    fn a_bare_host_becomes_https() {
        assert_eq!(ok("google.com"), "https://google.com/");
        assert_eq!(ok("  example.com  "), "https://example.com/");
        assert_eq!(ok("example.com/a/b?c=d"), "https://example.com/a/b?c=d");
    }

    #[test]
    fn an_explicit_scheme_is_kept() {
        assert_eq!(ok("http://example.com/"), "http://example.com/");
        assert_eq!(ok("https://example.com/"), "https://example.com/");
    }

    #[test]
    fn a_host_with_a_port_is_not_mistaken_for_a_scheme() {
        assert_eq!(ok("localhost:8080"), "https://localhost:8080/");
        assert_eq!(ok("127.0.0.1:3000/x"), "https://127.0.0.1:3000/x");
    }

    #[test]
    fn other_schemes_are_refused() {
        assert!(matches!(
            normalize("ftp://example.com/x"),
            Err(NetError::UnsupportedScheme { scheme }) if scheme == "ftp"
        ));
        // `data:` has no `//`, so it is refused as unparseable rather than as an
        // unsupported scheme. Either way it does not become a navigation.
        assert!(normalize("data:text/html,x").is_err());
        assert!(matches!(
            normalize("javascript://alert(1)"),
            Err(NetError::UnsupportedScheme { scheme }) if scheme == "javascript"
        ));
    }

    #[test]
    fn a_file_url_is_navigable_and_keeps_its_path() {
        let url = normalize("file:///tmp/page.html").expect("a file url");
        assert_eq!(url.scheme(), "file");
        assert_eq!(url.path(), "/tmp/page.html");
    }

    /// The rule that keeps the web out of the filesystem.
    #[test]
    fn only_the_user_and_a_local_page_may_reach_a_file_url() {
        let target = normalize("file:///tmp/page.html").expect("a file url");

        assert!(may_navigate(None, &target), "typed into the address bar");
        assert!(
            may_navigate(Some("file:///tmp/index.html"), &target),
            "a local page's own link"
        );
        assert!(
            !may_navigate(Some("https://example.com/"), &target),
            "a page from the internet must never reach the filesystem"
        );
        assert!(
            !may_navigate(Some("http://example.com/"), &target),
            "nor over plain http"
        );
    }

    #[test]
    fn navigating_to_the_web_is_never_restricted() {
        let target = normalize("https://example.com/").expect("a url");
        assert!(may_navigate(Some("file:///tmp/page.html"), &target));
        assert!(may_navigate(Some("https://other.example/"), &target));
        assert!(may_navigate(None, &target));
    }

    #[test]
    fn a_relative_link_resolves_against_the_page_it_was_found_on() {
        let base = "https://example.com/docs/guide.html";
        assert_eq!(
            resolve(base, "intro.html").as_deref(),
            Some("https://example.com/docs/intro.html")
        );
        assert_eq!(
            resolve(base, "/index.html").as_deref(),
            Some("https://example.com/index.html")
        );
        assert_eq!(
            resolve(base, "../other/page").as_deref(),
            Some("https://example.com/other/page")
        );
        assert_eq!(
            resolve(base, "#section").as_deref(),
            Some("https://example.com/docs/guide.html#section")
        );
    }

    #[test]
    fn an_absolute_link_passes_through() {
        assert_eq!(
            resolve("https://example.com/", "https://other.example/x").as_deref(),
            Some("https://other.example/x")
        );
    }

    #[test]
    fn a_link_on_a_page_with_no_address_resolves_to_nothing() {
        assert_eq!(resolve("", "page.html"), None);
    }

    #[test]
    fn input_without_a_host_is_refused() {
        assert!(matches!(normalize("   "), Err(NetError::EmptyUrl)));
        assert!(normalize("https://").is_err());
    }
}
