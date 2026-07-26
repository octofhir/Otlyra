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
    /// The page this request was made from, if a page made it.
    ///
    /// `None` is nobody: a typed address, a bookmark, a session restored. This is
    /// what `SameSite` is decided against, and it is the caller's to answer
    /// because only the caller knows — a loader looking at a URL cannot tell a
    /// link followed from a picture fetched.
    pub initiator: Option<Url>,
    /// Whether this is a top-level navigation — something whose result the reader
    /// will be looking at — rather than a resource inside a page.
    ///
    /// The other half of the `SameSite=Lax` question. Whether the method is a
    /// safe one is *not* asked here: it is [`LoadRequest::body`], which the
    /// loader reads itself and which a redirect can change on the way.
    pub navigation: bool,
    /// What the cache is allowed to do about this one.
    pub cache: CacheMode,
}

/// What a request is allowed to take from the cache.
///
/// The reader's own instruction, which is what makes this the caller's to say
/// rather than something the loader works out. A reload means *check*, and a
/// reload holding shift means *do not even look* — and a browser where those two
/// do the same thing as an ordinary click is a browser whose reload button
/// appears not to work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CacheMode {
    /// Answer from the cache where it may, ask where it must, fetch otherwise.
    #[default]
    Default,
    /// Never serve without asking the server, even where the stored copy is
    /// still fresh. What ⌘R means: the reader is saying they think it changed.
    Revalidate,
    /// Do not look at the cache at all. What ⌘⇧R means. The answer is still
    /// stored, because the point is to get a new copy rather than to stop having
    /// one.
    Bypass,
}

impl LoadRequest {
    /// A request for `url`, caused by nobody.
    pub fn new(url: Url) -> Self {
        Self {
            url,
            body: None,
            initiator: None,
            navigation: false,
            cache: CacheMode::Default,
        }
    }

    /// A request that sends `body` to `url`.
    pub fn post(url: Url, body: Body) -> Self {
        Self {
            body: Some(body),
            ..Self::new(url)
        }
    }

    /// The same request, caused by a page at `initiator`.
    pub fn from(mut self, initiator: Url) -> Self {
        self.initiator = Some(initiator);
        self
    }

    /// The same request, marked as a top-level navigation.
    pub fn navigating(mut self) -> Self {
        self.navigation = true;
        self
    }

    /// The same request, with what the cache may do about it.
    pub fn caching(mut self, cache: CacheMode) -> Self {
        self.cache = cache;
        self
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

    /// A redirect pointed somewhere that is not an address.
    #[error("{url} redirected to something that is not an address: {location}")]
    BadRedirect {
        /// The URL that answered with the redirect.
        url: String,
        /// What its `Location` said, as far as it can be shown.
        location: String,
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

/// The `Date` a response carried, if it carried a readable one.
///
/// Read with the cookie date reader, which takes the three HTTP formats and the
/// several shapes servers send that are none of them.
fn header_date(headers: &[(String, String)]) -> Option<std::time::SystemTime> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("date"))
        .and_then(|(_, value)| crate::cookie::date::parse(value))
}

/// How long a response had already spent in caches upstream.
///
/// Zero where there is no `Age`, which is the specification's own default and is
/// the right one: a response that says nothing has been nowhere.
fn header_age(headers: &[(String, String)]) -> std::time::Duration {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("age"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .map_or(std::time::Duration::ZERO, std::time::Duration::from_secs)
}

/// Where a redirect points, and whether the request keeps its body getting there.
struct Hop {
    /// The address, already resolved against the one that answered.
    to: Url,
    /// Whether the next request is the same request. `false` turns a `POST` into
    /// a `GET` of where it was pointed.
    keeps_the_body: bool,
}

/// Identifies us to servers. Deliberately honest rather than imitating a browser
/// whose behaviour we do not yet have.
const USER_AGENT: &str = concat!("Otlyra/", env!("CARGO_PKG_VERSION"));

/// The process's network stack: one HTTP client.
///
/// One client for the whole process, not one per request — a fresh client throws
/// away the connection pool, the DNS cache and the TLS session cache, which is
/// most of what makes the second request to a host fast.
///
/// No runtime of its own unless something asks it to block. [`Loader::fetch`] is
/// what the browser uses, on the runtime the browser already has; the shell's
/// one-shot `--url` mode has no runtime at all and [`Loader::fetch_blocking`] builds
/// it one, once, the first time it is called. A runtime built here unconditionally
/// was a second reactor sitting inside every fetch, which is what the fetch pool
/// used to block on.
pub struct Loader {
    client: reqwest::Client,
    blocking: std::sync::OnceLock<tokio::runtime::Runtime>,
    limits: Limits,
    jar: Option<SharedJar>,
    cache: Option<SharedCache>,
}

/// The one cache, shared by everything that fetches and everything that shows a
/// reader what has been kept.
///
/// A `std::sync::Mutex` and never held across an `await`, for the reason
/// [`SharedJar`] is one: what is done under it is a hash lookup and a memcpy, and
/// an async mutex would be a scheduling point with nothing to schedule.
pub type SharedCache = std::sync::Arc<std::sync::Mutex<crate::cache::Cache>>;

/// The one jar, shared by everything that sends a request and everything that
/// shows the reader what is kept.
///
/// A `std::sync::Mutex` rather than tokio's, and never held across an `await`:
/// the two things done under it — asking for a header, taking one — are a walk
/// over a few hundred cookies with no waiting in them, and an async mutex would
/// be a scheduling point where there is nothing to schedule.
pub type SharedJar = std::sync::Arc<std::sync::Mutex<crate::cookie::Jar>>;

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

        // The client follows nothing. Every hop is ours, because a redirect is
        // where several of a browser's rules apply and a client that walks the
        // chain by itself applies none of them: a `Set-Cookie` on the way is
        // invisible, the `Cookie` for the hop after it is never asked for, and a
        // `Location` naming a `file:` URL is followed rather than refused. The
        // limit that was this line's argument is enforced in [`Loader::fetch`].
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(limits.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| startup(Box::new(error)))?;

        Ok(Self {
            client,
            blocking: std::sync::OnceLock::new(),
            limits,
            jar: None,
            cache: None,
        })
    }

    /// The same loader, sending and storing cookies through `jar`.
    ///
    /// Optional, and off by default: a loader with no jar sends no `Cookie` and
    /// keeps no `Set-Cookie`, which is what a one-shot `--url` fetch and most of
    /// this crate's own tests want. A browser gives it one.
    pub fn with_jar(mut self, jar: SharedJar) -> Self {
        self.jar = Some(jar);
        self
    }

    /// The jar this loader sends from, if it has one.
    pub fn jar(&self) -> Option<&SharedJar> {
        self.jar.as_ref()
    }

    /// The same loader, answering out of `cache` where it can.
    ///
    /// Optional and off by default, like the jar: a one-shot `--url` fetch and
    /// most of this crate's tests want the network every time, and a test that
    /// silently answered out of a cache would be a test of the cache.
    pub fn with_cache(mut self, cache: SharedCache) -> Self {
        self.cache = Some(cache);
        self
    }

    /// The cache this loader answers out of, if it has one.
    pub fn cache(&self) -> Option<&SharedCache> {
        self.cache.as_ref()
    }

    /// The limits this loader enforces.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Fetch one resource, blocking the calling thread until it is complete.
    ///
    /// **This blocks, and that is why it is named so.** It exists for the shell's
    /// one-shot `--url` mode, which has no event loop and nothing else to do. The
    /// window's own thread must never reach it: a main loop that blocks on the
    /// network is a main loop that has stopped painting and stopped answering the
    /// keyboard. The browser calls [`Loader::fetch`] on its own runtime instead.
    ///
    /// Calling this from inside a Tokio runtime is a panic, as it should be — it is
    /// exactly the mistake the name is warning about.
    pub fn fetch_blocking(&self, request: LoadRequest) -> Result<LoadedResource, NetError> {
        let span = tracing::info_span!("resource_load", url = %request.url);
        let _entered = span.enter();
        if self.blocking.get().is_none() {
            let built = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| NetError::Startup {
                    source: Box::new(error),
                })?;
            let _ = self.blocking.set(built);
        }
        let runtime = self.blocking.get().expect("the runtime was just built");
        runtime.block_on(self.fetch(request))
    }

    /// Fetch one resource on the caller's runtime, following redirects.
    ///
    /// The whole transport: `data:` read out of the address, scheme policy, the
    /// redirect chain, the declared and the arriving body caps, and the headers as
    /// sent and as received. Nothing here knows what the bytes mean.
    ///
    /// **The chain is walked here rather than by the client**, which is what makes
    /// a hop a place rules can be applied. A client following redirects internally
    /// hands back only the last response: what a server set on the way is gone,
    /// what should have been sent on the hop after it was never asked for, and a
    /// `Location` naming a scheme this crate does not fetch is followed rather
    /// than refused. A sign-in is almost always a redirect, so none of that is an
    /// edge case.
    pub async fn fetch(&self, request: LoadRequest) -> Result<LoadedResource, NetError> {
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

        let started_at = request.url.to_string();
        let mut url = request.url;
        let mut body = request.body;

        // One instant for the whole chain. A cookie that expires between the first
        // hop and the last is one the chain was decided against inconsistently,
        // and asking the clock four times is how that happens.
        let now = std::time::SystemTime::now();
        // What the cache is asked about, and answers under: the address the
        // reader asked for rather than the one the chain ends at. A later request
        // is made for the first of those and never for the second, so an entry
        // under the last hop is an entry nothing looks up — and the response it
        // holds already says where it came from.
        let key = started_at.clone();
        // The headers a `Vary` could name that are not the same on every request
        // this client makes. `Cookie` is the one: `User-Agent`, `Accept` and
        // `Accept-Encoding` are constants here, so an entry stored under them
        // matches every later request by construction.
        let varying: Vec<(String, String)> = self
            .cookie_header(
                &url,
                crate::cookie::Context::of(
                    &url,
                    request.initiator.as_ref(),
                    request.navigation && body.is_none(),
                ),
                now,
            )
            .map(|value| vec![("cookie".to_owned(), value)])
            .unwrap_or_default();

        // Answered without a request at all where the stored copy is still good,
        // and with a conditional one where it is not. Only a `GET`: a request with
        // a body changes something, and nothing here would reuse the answer.
        let mut conditions: Vec<(&'static str, String)> = Vec::new();
        if body.is_none()
            && request.cache != CacheMode::Bypass
            && let Some(cache) = self.cache.as_ref()
        {
            let mut cache = cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match cache.look_up(&key, &varying, now) {
                // A fresh entry is the answer — unless the reader has just said
                // they think it changed, which is what a reload is. Then it is
                // still worth having: the server may say nothing changed, and a
                // reload that costs a header instead of a body is the difference
                // between a page that reappears and a page that loads again.
                Some((stored, crate::cache::Use::Fresh)) if request.cache == CacheMode::Default => {
                    tracing::debug!(%key, "answered from the cache");
                    return Ok(stored.as_resource());
                }
                // A validator belongs to the address that sent it. Where the
                // entry was stored under one address and answered by another —
                // a redirect chain, which is kept whole under the address the
                // reader asked for — its `ETag` is the last hop's and asking the
                // first hop about it would be asking one resource whether it
                // matches another's. A server that said yes would hand back the
                // wrong body, so nothing is asked and the chain runs again.
                Some((stored, _)) if stored.final_url == key => {
                    conditions = stored.conditions();
                }
                Some(_) => {}
                None => {}
            }
        }

        // `SameSite` across a chain is the whole chain's answer, not each hop's:
        // once a redirect has left the initiator's site, everything after it was
        // caused by that departure however same-site the last two hops look. A
        // request nobody initiated never leaves, which is what makes a bookmark to
        // a bank open it signed in.
        let mut still_same_site = true;

        for hop in 0.. {
            if hop > self.limits.max_redirects {
                return Err(NetError::TooManyRedirects {
                    url: started_at,
                    limit: self.limits.max_redirects,
                });
            }

            still_same_site &= request
                .initiator
                .as_ref()
                .is_none_or(|from| crate::cookie::same_site(from, &url));
            let context = if still_same_site {
                crate::cookie::Context::SameSite
            } else if request.navigation && body.is_none() {
                // Top-level, and by a method that changes nothing — which is where
                // `Lax` was drawn. A cross-site form post is top-level and is not
                // safe, and a 303 that drops the body makes the hop after it safe,
                // which is what browsers do as well.
                crate::cookie::Context::CrossSiteNavigation
            } else {
                crate::cookie::Context::CrossSite
            };

            // The conditions go on the first hop only: they are about the entry
            // stored under the address the reader asked for, and a redirect is a
            // different resource with different validators.
            let asking = if hop == 0 { conditions.as_slice() } else { &[] };
            let (response, request_headers) =
                self.send(&url, body.clone(), context, now, asking).await?;
            self.take_cookies(&response, &url, context, now);

            // The server saying nothing changed. The body was never sent, so the
            // one that answers is the one already held.
            if response.status().as_u16() == 304
                && let Some(cache) = self.cache.as_ref()
            {
                let headers = headers_to_pairs(response.headers());
                let times = crate::cache::Times {
                    requested: now,
                    received: std::time::SystemTime::now(),
                    date: header_date(&headers).unwrap_or(now),
                    age: header_age(&headers),
                };
                let mut cache = cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if cache.refresh(&key, &headers, times) {
                    tracing::debug!(%key, "the server said nothing changed");
                    if let Some((stored, _)) = cache.look_up(&key, &varying, now) {
                        return Ok(stored.as_resource());
                    }
                }
            }

            match Self::redirect_from(&response, &url)? {
                // Not a redirect, or one with nowhere to go — which a server does
                // send, and which every browser renders as an ordinary response.
                None => {
                    let resource = self.receive(response, url, request_headers).await?;
                    if body.is_none() {
                        self.keep(&key, &varying, &resource, now);
                    }
                    return Ok(resource);
                }
                Some(next) => {
                    // The chain is only ever http and https. A `Location` naming
                    // anything else — `file:` above all — is refused here, because
                    // a page from the internet reaching the filesystem through a
                    // redirect is the oldest browser vulnerability there is.
                    if !crate::is_fetchable(&next.to) {
                        return Err(NetError::UnsupportedScheme {
                            scheme: next.to.scheme().to_owned(),
                        });
                    }
                    if !next.keeps_the_body {
                        body = None;
                    }
                    url = next.to;
                }
            }
        }
        // `0..` is not empty, so the loop returns or diverges.
        unreachable!("the redirect loop returns from inside")
    }

    /// One hop: build it, send it, and say what was actually put on it.
    ///
    /// Built rather than sent in one call, so the headers the client is about to
    /// write can be read back and shown: an inspector's *Request* pane is the
    /// headers we actually sent, and this is where they become knowable.
    async fn send(
        &self,
        url: &Url,
        body: Option<Body>,
        context: crate::cookie::Context,
        now: std::time::SystemTime,
        conditions: &[(&'static str, String)],
    ) -> Result<(reqwest::Response, Vec<(String, String)>), NetError> {
        let shown = url.to_string();
        let built = match body {
            // A body is held in memory rather than streamed, which is what lets it
            // be replayed: a redirect that keeps the method — 307, 308 — has to
            // send the same bytes again, and a body that could only be read once
            // would arrive empty the second time.
            Some(body) => self
                .client
                .post(url.clone())
                .header(reqwest::header::CONTENT_TYPE, body.content_type)
                .body(body.bytes),
            None => self.client.get(url.clone()),
        }
        .build()
        .map_err(|error| self.classify(error, &shown))?;
        let mut built = built;
        // Asked for and written here rather than left to the client, so the header
        // is on the request the inspector reads back and so the jar sees each hop.
        // The guard is taken and dropped before anything is awaited.
        if let Some(header) = self.cookie_header(url, context, now)
            && let Ok(value) = reqwest::header::HeaderValue::from_str(&header)
        {
            built.headers_mut().insert(reqwest::header::COOKIE, value);
        }
        for (name, written) in conditions {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                && let Ok(value) = reqwest::header::HeaderValue::from_str(written)
            {
                built.headers_mut().insert(name, value);
            }
        }
        let built = built;
        let request_headers = headers_to_pairs(built.headers());

        let response = self
            .client
            .execute(built)
            .await
            .map_err(|error| self.classify(error, &shown))?;
        Ok((response, request_headers))
    }

    /// Read a response that is the end of the chain.
    async fn receive(
        &self,
        response: reqwest::Response,
        url: Url,
        request_headers: Vec<(String, String)>,
    ) -> Result<LoadedResource, NetError> {
        let shown = url.to_string();
        let limit = self.limits.max_body_bytes;

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
            return Err(NetError::BodyTooLarge { url: shown, limit });
        }

        // And again as it arrives, because `Content-Length` is a claim by the same
        // server that is sending the bytes, and may be absent or false.
        let mut body: Vec<u8> = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.classify(error, &shown))?
        {
            if body.len() as u64 + chunk.len() as u64 > limit {
                return Err(NetError::BodyTooLarge { url: shown, limit });
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

    /// The `Cookie` header for one hop, or `None` when there is nothing to send.
    fn cookie_header(
        &self,
        url: &Url,
        context: crate::cookie::Context,
        now: std::time::SystemTime,
    ) -> Option<String> {
        let jar = self.jar.as_ref()?;
        // A jar poisoned by a panic somewhere else must not stop every later fetch
        // from having cookies: the data behind it is a list of cookies, not an
        // invariant a panic can have broken halfway.
        let mut jar = jar.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        jar.header_for(url, context, now)
    }

    /// Take whatever a response set, one `Set-Cookie` at a time.
    ///
    /// Every hop, including the redirects — which is the point of walking the
    /// chain here. A sign-in sets its session on the hop that redirects, and a
    /// client that handed back only the last response never showed it to anyone.
    fn take_cookies(
        &self,
        response: &reqwest::Response,
        url: &Url,
        context: crate::cookie::Context,
        now: std::time::SystemTime,
    ) {
        let Some(jar) = self.jar.as_ref() else {
            return;
        };
        let lines: Vec<&str> = response
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            // A header value is bytes and a cookie may be written in some legacy
            // encoding. One that is not text is dropped rather than mangled: a
            // name or a value read wrong is a cookie that goes back wrong, which
            // is worse than one that never arrived.
            .filter_map(|value| value.to_str().ok())
            .collect();
        if lines.is_empty() {
            return;
        }

        let mut jar = jar.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for line in lines {
            if let Err(refused) = jar.set_in(url, line, context, now) {
                // Named rather than swallowed: this is what a person debugging
                // their own site needs to see, and what the inspector will show.
                tracing::debug!(%url, %refused, "a cookie was refused");
            }
        }
    }

    /// Keep a response, if the rules say it may be kept.
    fn keep(
        &self,
        key: &str,
        varying: &[(String, String)],
        resource: &LoadedResource,
        requested: std::time::SystemTime,
    ) {
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        let received = std::time::SystemTime::now();
        let times = crate::cache::Times {
            requested,
            received,
            date: header_date(&resource.response_headers).unwrap_or(received),
            age: header_age(&resource.response_headers),
        };
        let directives = crate::cache::Directives::parse(
            resource
                .response_headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
                .map(|(_, value)| value.as_str()),
        );
        let header = |name: &str| {
            resource
                .response_headers
                .iter()
                .find(|(sent, _)| sent.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let lifetime = crate::cache::lifetime(
            directives,
            header("expires"),
            header("last-modified"),
            times,
        );
        let stored = crate::cache::Stored {
            status: resource.status,
            headers: resource.response_headers.clone(),
            body: resource.body.clone(),
            final_url: resource.final_url.clone(),
            directives,
            lifetime,
            times,
            varied: Vec::new(),
            varies_on_everything: false,
        };
        let mut cache = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.store(key, "GET", stored, varying);
    }

    /// Where a response says to go next, and what to carry there.
    ///
    /// `None` when the response is the end of the chain — either it is not a
    /// redirect, or it is one with no `Location`, which servers do send and which
    /// every browser renders as an ordinary response rather than as an error.
    fn redirect_from(response: &reqwest::Response, from: &Url) -> Result<Option<Hop>, NetError> {
        let status = response.status().as_u16();
        // What each code does to the method is not what the specification first
        // said, and browsers are what servers are written against. 303 was always
        // *go and GET this instead*. 301 and 302 are supposed to preserve the
        // method and never did: a body re-sent to an address that did not ask for
        // it is a second submission, so a redirected POST becomes a GET of where
        // it was pointed. 307 and 308 exist precisely to say *no, really, send it
        // again*.
        let keeps_the_body = match status {
            301..=303 => false,
            307 | 308 => true,
            _ => return Ok(None),
        };

        let Some(location) = response.headers().get(reqwest::header::LOCATION) else {
            return Ok(None);
        };
        let bad = || NetError::BadRedirect {
            url: from.to_string(),
            location: String::from_utf8_lossy(location.as_bytes()).into_owned(),
        };
        // Relative against the address that answered, which is the common form:
        // `Location: /login` is most of the redirects on the web.
        let to = from
            .join(location.to_str().map_err(|_| bad())?)
            .map_err(|_| bad())?;
        Ok(Some(Hop { to, keeps_the_body }))
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
