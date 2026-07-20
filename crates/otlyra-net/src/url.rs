//! Turning what a person typed into a URL we are willing to fetch.

use url::Url;

use crate::loader::NetError;

/// Schemes this crate will fetch.
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
            Ok(url) if FETCHABLE.contains(&url.scheme()) => url,
            _ => parse(&format!("https://{input}"))?,
        }
    };

    if !FETCHABLE.contains(&parsed.scheme()) {
        return Err(NetError::UnsupportedScheme {
            scheme: parsed.scheme().to_owned(),
        });
    }
    if parsed.host().is_none() {
        return Err(NetError::MissingHost {
            url: parsed.to_string(),
        });
    }

    Ok(parsed)
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
        assert!(matches!(
            normalize("file:///etc/passwd"),
            Err(NetError::UnsupportedScheme { scheme }) if scheme == "file"
        ));
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
