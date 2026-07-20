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
    fn input_without_a_host_is_refused() {
        assert!(matches!(normalize("   "), Err(NetError::EmptyUrl)));
        assert!(normalize("https://").is_err());
    }
}
