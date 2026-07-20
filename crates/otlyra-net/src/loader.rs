//! The shared client, and the fetch itself.

use std::time::Duration;

use url::Url;

use crate::limits::Limits;

/// A request to load one resource.
///
/// Owned, `Send`, and free of anything that knows what the bytes mean. When the
/// loader moves onto its own thread this type is the message that crosses.
#[derive(Clone, Debug)]
pub struct LoadRequest {
    /// The absolute URL to fetch. Produce it with [`crate::normalize`].
    pub url: Url,
}

impl LoadRequest {
    /// A request for `url`.
    pub fn new(url: Url) -> Self {
        Self { url }
    }
}

/// One fully-received response.
///
/// Bytes plus the little the transport knows about them. Interpreting them —
/// sniffing, parsing, deciding they are a document at all — happens elsewhere.
#[derive(Clone, Debug)]
pub struct LoadedResource {
    /// The URL the response actually came from, after any redirects.
    pub final_url: String,
    /// HTTP status.
    pub status: u16,
    /// The raw `Content-Type` header, if the server sent one.
    pub content_type: Option<String>,
    /// The body, decompressed but otherwise untouched.
    pub body: Vec<u8>,
}

impl LoadedResource {
    /// The `charset` parameter of `Content-Type`, lowercased, if there is one.
    pub fn charset(&self) -> Option<String> {
        charset_of(self.content_type.as_deref()?)
    }

    /// Decode the body to text.
    ///
    /// The charset comes from `Content-Type`; an absent, unrecognized or bogus
    /// label falls back to UTF-8. This is deliberately *not* the HTML encoding
    /// algorithm — that one also reads the BOM, prescans the first 1024 bytes for
    /// a `<meta>`, and applies the WHATWG overrides, and it belongs in the HTML
    /// parser, which is the only place that knows the bytes are HTML.
    pub fn decode_text(&self) -> String {
        let encoding = self
            .charset()
            .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
            .unwrap_or(encoding_rs::UTF_8);
        let (text, _actual, _had_errors) = encoding.decode(&self.body);
        text.into_owned()
    }
}

/// Extract the `charset` parameter from a `Content-Type` header value.
fn charset_of(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        let value = value.trim().trim_matches('"').trim();
        (!value.is_empty()).then(|| value.to_ascii_lowercase())
    })
}

/// What can go wrong between a typed URL and bytes in hand.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    /// The input was blank.
    #[error("no URL given")]
    EmptyUrl,

    /// The input is not a URL, even after assuming `https://`.
    #[error("not a URL: {input}")]
    InvalidUrl {
        /// What the caller passed in.
        input: String,
        /// The parser's complaint.
        #[source]
        source: url::ParseError,
    },

    /// A scheme this crate does not fetch.
    #[error("cannot fetch {scheme}: URLs (only http and https)")]
    UnsupportedScheme {
        /// The scheme we refused.
        scheme: String,
    },

    /// A URL with no host to connect to.
    #[error("no host in {url}")]
    MissingHost {
        /// The offending URL.
        url: String,
    },

    /// The tokio runtime or the HTTP client could not be built.
    #[error("could not start the network stack: {source}")]
    Startup {
        /// The underlying failure.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// The request outlived its budget.
    #[error("{url} timed out after {}s", timeout.as_secs())]
    Timeout {
        /// The URL we were fetching.
        url: String,
        /// The budget it exceeded.
        timeout: Duration,
    },

    /// The redirect chain never ended.
    #[error("{url} redirected more than {limit} times")]
    TooManyRedirects {
        /// The URL we started from.
        url: String,
        /// The cap that was hit.
        limit: usize,
    },

    /// The body is, or claims to be, larger than we will hold.
    #[error("{url} body exceeds the {limit} byte limit")]
    BodyTooLarge {
        /// The URL we were fetching.
        url: String,
        /// The cap that was hit.
        limit: u64,
    },

    /// Anything else the transport reported: DNS, connection, TLS, protocol.
    #[error("could not fetch {url}: {source}")]
    Transport {
        /// The URL we were fetching.
        url: String,
        /// The underlying failure.
        #[source]
        source: reqwest::Error,
    },
}

/// Identifies us to servers. Deliberately honest rather than imitating a browser
/// whose behaviour we do not yet have.
const USER_AGENT: &str = concat!("Otlyra/", env!("CARGO_PKG_VERSION"));

/// The process's network stack: one HTTP client, one runtime.
///
/// One client for the whole process, not one per request — a fresh client throws
/// away the connection pool, the DNS cache and the TLS session cache, which is
/// most of what makes the second request to a host fast.
pub struct Loader {
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    limits: Limits,
}

impl Loader {
    /// A loader with the document limits.
    pub fn new() -> Result<Self, NetError> {
        Self::with_limits(Limits::DOCUMENT)
    }

    /// A loader with explicit limits.
    ///
    /// The limits belong to the loader rather than to each request because the
    /// redirect policy and the timeout are properties of the client.
    pub fn with_limits(limits: Limits) -> Result<Self, NetError> {
        let startup =
            |source: Box<dyn std::error::Error + Send + Sync>| NetError::Startup { source };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| startup(Box::new(error)))?;

        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(limits.timeout)
            .redirect(reqwest::redirect::Policy::limited(limits.max_redirects))
            .build()
            .map_err(|error| startup(Box::new(error)))?;

        Ok(Self {
            client,
            runtime,
            limits,
        })
    }

    /// The limits this loader enforces.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Fetch one resource, blocking the calling thread until it is complete.
    ///
    /// **This blocks, and that is why it is named so.** It exists for the shell's
    /// one-shot `--url` mode, where there is nothing else for the thread to do.
    /// The browser's real path is the opposite: the loader runs on its own runtime
    /// and the event loop only ever `try_recv`s owned messages from it, because a
    /// main loop that blocks on the network is a main loop that stops painting and
    /// stops responding to input. That arrives with navigation, at M9; everything
    /// crossing this signature is already owned so that it is a change of transport
    /// and not a change of design.
    pub fn fetch_blocking(&self, request: LoadRequest) -> Result<LoadedResource, NetError> {
        let span = tracing::info_span!("resource_load", url = %request.url);
        let _entered = span.enter();
        self.runtime.block_on(self.fetch(request))
    }

    async fn fetch(&self, request: LoadRequest) -> Result<LoadedResource, NetError> {
        if !crate::is_fetchable(&request.url) {
            return Err(NetError::UnsupportedScheme {
                scheme: request.url.scheme().to_owned(),
            });
        }

        let url = request.url.to_string();
        let limit = self.limits.max_body_bytes;

        let response = self
            .client
            .get(request.url)
            .send()
            .await
            .map_err(|error| self.classify(error, &url))?;

        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);

        // Before the body, not after: a declared length over the cap is a request we
        // decline to make memory available for.
        if let Some(declared) = response.content_length()
            && declared > limit
        {
            return Err(NetError::BodyTooLarge { url, limit });
        }

        // And again as it arrives, because `Content-Length` is a claim by the same
        // server that is sending the bytes, and may be absent or false.
        let mut body: Vec<u8> = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.classify(error, &url))?
        {
            if body.len() as u64 + chunk.len() as u64 > limit {
                return Err(NetError::BodyTooLarge { url, limit });
            }
            body.extend_from_slice(&chunk);
        }

        tracing::debug!(status, bytes = body.len(), "resource loaded");

        Ok(LoadedResource {
            final_url,
            status,
            content_type,
            body,
        })
    }

    /// Give a transport failure the name the user needs to hear.
    fn classify(&self, error: reqwest::Error, url: &str) -> NetError {
        let url = url.to_owned();
        if error.is_timeout() {
            NetError::Timeout {
                url,
                timeout: self.limits.timeout,
            }
        } else if error.is_redirect() {
            NetError::TooManyRedirects {
                url,
                limit: self.limits.max_redirects,
            }
        } else {
            NetError::Transport { url, source: error }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resource(content_type: Option<&str>, body: &[u8]) -> LoadedResource {
        LoadedResource {
            final_url: "https://example.com/".to_owned(),
            status: 200,
            content_type: content_type.map(str::to_owned),
            body: body.to_vec(),
        }
    }

    #[test]
    fn charset_is_read_out_of_content_type() {
        assert_eq!(
            charset_of("text/html; charset=UTF-8").as_deref(),
            Some("utf-8")
        );
        assert_eq!(
            charset_of("text/html;charset=\"windows-1251\"").as_deref(),
            Some("windows-1251")
        );
        assert_eq!(
            charset_of("text/html; boundary=x; charset = iso-8859-1").as_deref(),
            Some("iso-8859-1")
        );
        assert_eq!(charset_of("text/html").as_deref(), None);
        assert_eq!(charset_of("text/html; charset=").as_deref(), None);
    }

    #[test]
    fn a_declared_charset_decodes_legacy_bytes() {
        // "Привет" in windows-1251.
        let bytes = [0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        let decoded = resource(Some("text/html; charset=windows-1251"), &bytes).decode_text();
        assert_eq!(decoded, "Привет");
    }

    #[test]
    fn an_absent_or_unknown_charset_falls_back_to_utf8() {
        assert_eq!(resource(None, "héllo".as_bytes()).decode_text(), "héllo");
        assert_eq!(
            resource(Some("text/html; charset=nonsense"), "héllo".as_bytes()).decode_text(),
            "héllo"
        );
    }

    #[test]
    fn invalid_bytes_decode_to_replacement_characters_rather_than_failing() {
        assert_eq!(
            resource(None, &[0xE0, 0x80]).decode_text(),
            "\u{fffd}\u{fffd}"
        );
    }

    /// `decode` sniffs a BOM, which outranks the declared charset. That is the
    /// WHATWG rule, and it is the one thing the transport-level decode shares with
    /// the full HTML algorithm.
    #[test]
    fn a_bom_wins_over_the_declared_charset() {
        let bytes = [0xEF, 0xBB, 0xBF, b'h', b'i'];
        assert_eq!(
            resource(Some("text/html; charset=windows-1251"), &bytes).decode_text(),
            "hi"
        );
    }
}
