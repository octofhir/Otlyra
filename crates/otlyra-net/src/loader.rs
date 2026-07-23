//! The shared client, and the fetch itself.

use std::time::Duration;

use url::Url;

use crate::limits::Limits;

/// Bytes a request carries, and what it says they are.
///
/// Built entirely above this crate — a form's entry list is HTML's business, and
/// nothing here knows what a form is. What arrives is bytes and a media type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    /// The `Content-Type` to send them under.
    pub content_type: String,
    /// The bytes themselves.
    pub bytes: Vec<u8>,
}

/// A request to load one resource.
///
/// Owned, `Send`, and free of anything that knows what the bytes mean. When the
/// loader moves onto its own thread this type is the message that crosses.
#[derive(Clone, Debug)]
pub struct LoadRequest {
    /// The absolute URL to fetch. Produce it with [`crate::normalize`].
    pub url: Url,
    /// What to send with it, if anything. A request with a body is a `POST` and
    /// one without is a `GET`: those are the two a page without a script can ask
    /// for, so the body is the method rather than a second field that could
    /// disagree with it.
    pub body: Option<Body>,
}

impl LoadRequest {
    /// A request for `url`.
    pub fn new(url: Url) -> Self {
        Self { url, body: None }
    }

    /// A request that sends `body` to `url`.
    pub fn post(url: Url, body: Body) -> Self {
        Self {
            url,
            body: Some(body),
        }
    }

    /// The method this request is made with.
    pub fn method(&self) -> &'static str {
        if self.body.is_some() { "POST" } else { "GET" }
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
    /// Whether the server sent `X-Content-Type-Options: nosniff`, which is it
    /// saying that what it declared is what it means.
    pub nosniff: bool,
    /// The headers actually put on the request, name and value, in the order the
    /// client wrote them. What an inspector shows under *Request*: the ones we
    /// sent, not a plausible list of ones we might have.
    pub request_headers: Vec<(String, String)>,
    /// Every header the response carried, name and value, in the order it sent
    /// them. A header seen twice is listed twice, because that is what arrived.
    pub response_headers: Vec<(String, String)>,
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

/// A header map as name/value pairs a person can read.
///
/// A value that is not valid UTF-8 — which a header may be — is shown as the
/// bytes it is rather than dropped: an inspector that hid a header because it
/// could not spell it would be hiding exactly the odd one worth seeing.
fn headers_to_pairs(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_owned(),
                value
                    .to_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|_| String::from_utf8_lossy(value.as_bytes()).into_owned()),
            )
        })
        .collect()
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
    /// one-shot `--url` mode, and for the fetch threads, which have nothing else to
    /// do while it runs. The window's own thread must never reach it: a main loop
    /// that blocks on the network is a main loop that has stopped painting and
    /// stopped answering the keyboard. Everything crossing this signature is owned,
    /// so moving the transport further away is a change of transport rather than a
    /// change of design.
    pub fn fetch_blocking(&self, request: LoadRequest) -> Result<LoadedResource, NetError> {
        let span = tracing::info_span!("resource_load", url = %request.url);
        let _entered = span.enter();
        self.runtime.block_on(self.fetch(request))
    }

    async fn fetch(&self, request: LoadRequest) -> Result<LoadedResource, NetError> {
        // A `data:` URL is not a request at all: the resource is written into the
        // address, and reading it is decoding rather than fetching. Answered here
        // so that everything upstream — a picture, a stylesheet, a font — takes one
        // route to its bytes.
        if let Some((kind, body)) = crate::read_data_url(&request.url) {
            return Ok(LoadedResource {
                final_url: request.url.to_string(),
                status: 200,
                content_type: Some(kind),
                nosniff: false,
                request_headers: Vec::new(),
                response_headers: Vec::new(),
                body,
            });
        }

        if !crate::is_fetchable(&request.url) {
            return Err(NetError::UnsupportedScheme {
                scheme: request.url.scheme().to_owned(),
            });
        }

        let url = request.url.to_string();
        let limit = self.limits.max_body_bytes;

        // Built rather than sent in one call, so the headers the client is about
        // to write can be read back and shown: an inspector's *Request* pane is
        // the headers we actually sent, and this is where they become knowable.
        let built = match request.body {
            // A body is held in memory rather than streamed, which is what lets the
            // client replay it: a redirect that keeps the method — 307, 308 — has to
            // send the same bytes again, and a body it could only read once would
            // arrive empty the second time.
            Some(body) => self
                .client
                .post(request.url)
                .header(reqwest::header::CONTENT_TYPE, body.content_type)
                .body(body.bytes),
            None => self.client.get(request.url),
        }
        .build()
        .map_err(|error| self.classify(error, &url))?;
        let request_headers = headers_to_pairs(built.headers());

        let response = self
            .client
            .execute(built)
            .await
            .map_err(|error| self.classify(error, &url))?;

        let status = response.status().as_u16();
        let response_headers = headers_to_pairs(response.headers());
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let nosniff = response
            .headers()
            .get(reqwest::header::X_CONTENT_TYPE_OPTIONS)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("nosniff"));

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
            nosniff,
            request_headers,
            response_headers,
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
            nosniff: false,
            request_headers: Vec::new(),
            response_headers: Vec::new(),
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
