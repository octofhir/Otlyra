//! The jar: what a site is allowed to keep, and what goes back to it.
//!
//! Everything hard about cookies is here, and none of it is the parsing. A cookie
//! is a string a server asks a browser to repeat, and the only thing standing
//! between that and a reader's session being readable by whoever asks is the set
//! of rules about *who* it is repeated to. Every one of them is a near-miss away
//! from being wrong: a domain match that is a suffix match hands
//! `example.com`'s cookies to `evil-example.com`, a path match that is a prefix
//! match hands `/foo`'s to `/foobar`, and no public-suffix rule at all lets one
//! site under `.co.uk` write a cookie every site under `.co.uk` sends back.
//!
//! The clock is a parameter and never read from the machine. That is not for
//! testing alone, though it is what lets a test state an instant instead of
//! sleeping: a request and the cookies chosen for it should be decided against
//! one instant, not against however many `SystemTime::now()` calls the code
//! happens to make.

use std::time::{Duration, SystemTime};

use url::{Host, Url};

use super::parse::{SameSite, SetCookie, Unreadable};
use super::suffix;

/// The longest a cookie may outlive the moment it was set.
///
/// Four hundred days, which is the specification's cap and what browsers
/// enforce. A server asking for ten years gets this instead: an expiry is a claim
/// by the party that benefits from it, and a jar with no ceiling is one that
/// keeps a tracking identifier until the disk is replaced.
pub const MAX_LIFETIME: Duration = Duration::from_secs(400 * 24 * 60 * 60);

/// How much a jar will hold.
///
/// A cap is not tidiness. Every cookie a domain sets is sent back on every later
/// request to it, so an uncapped jar is a way for one site to make a reader's own
/// connection slow, and for one site to fill a reader's disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capacity {
    /// Cookies one domain may keep. The specification asks for at least 50;
    /// this is what the browsers settled on.
    pub per_domain: usize,
    /// Cookies in all.
    pub total: usize,
}

impl Default for Capacity {
    fn default() -> Self {
        Self {
            per_domain: 180,
            total: 3300,
        }
    }
}

/// How a request came to be made, which is the whole of what `SameSite` asks.
///
/// The caller decides this, because only the caller knows what caused the
/// request. Getting it wrong in the generous direction is a cross-site request
/// that arrives already signed in, which is the attack `SameSite` exists for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// The page that caused this request is on the same site, or there is no page
    /// because a person typed the address or opened a bookmark.
    SameSite,
    /// A top-level navigation to this site from another one — a link followed, a
    /// redirect arrived at — *by a method that changes nothing*. A cross-site
    /// form post is [`Context::CrossSite`] however top-level it is; that is the
    /// distinction `Lax` was drawn at.
    CrossSiteNavigation,
    /// Anything else another site's page caused: a subresource, a form post, a
    /// redirect chain begun elsewhere.
    CrossSite,
}

impl Context {
    /// How a request to `target` relates to the site that caused it.
    ///
    /// `initiator` is the page that asked, and `None` means nobody did — a typed
    /// address, a bookmark, a session restored. There is no other site involved
    /// then, so the request is same-site with wherever it is going, which is what
    /// makes a bookmark to a bank open it signed in.
    ///
    /// `top_level_and_safe` is the `Lax` line: a navigation the reader can see the
    /// result of, made by a method that changes nothing. A cross-site form post is
    /// top-level and is not safe, and is exactly what `Lax` was drawn to stop.
    pub fn of(target: &Url, initiator: Option<&Url>, top_level_and_safe: bool) -> Self {
        match initiator {
            None => Self::SameSite,
            Some(from) if same_site(from, target) => Self::SameSite,
            Some(_) if top_level_and_safe => Self::CrossSiteNavigation,
            Some(_) => Self::CrossSite,
        }
    }
}

/// Whether two addresses belong to the same site.
///
/// The same registrable domain, reached over the same scheme. The second half is
/// *schemeful* same-site, and it is not pedantry: without it a page served over
/// plain `http` — which anyone on the path can write — counts as the same site as
/// the `https` one and can cause its cookies to be sent.
///
/// A host with no registrable domain is its own site, which is what makes
/// `localhost` and a bare address behave.
pub fn same_site(one: &Url, other: &Url) -> bool {
    let (Some(here), Some(there)) = (one.host_str(), other.host_str()) else {
        return false;
    };
    one.scheme() == other.scheme() && site_of(here) == site_of(there)
}

/// The site a host belongs to: its registrable domain, or itself where it has
/// none.
///
/// An address is its own site and is never taken apart. Reading `127.0.0.1` as a
/// name gives `0.1` — a suffix two other machines on the network share — so a
/// registrable domain is exactly the wrong question to ask of one.
fn site_of(host: &str) -> &str {
    if is_address(host) {
        return host;
    }
    suffix::registrable_domain(host).unwrap_or(host)
}

/// One cookie as it is kept.
///
/// Every field here is the resolved one: the domain is what the cookie goes back
/// to rather than what the attribute said, the path is the request's own where no
/// attribute was written, and the expiry is an instant rather than a duration.
/// Nothing downstream has to redo the storage model to read this.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cookie {
    /// The name. May be empty, which is a cookie sent back as a bare value.
    pub name: String,
    /// The value, as the server wrote it.
    pub value: String,
    /// The host this goes back to, canonical: lowercase, punycode, no leading
    /// dot. With [`Cookie::host_only`] set it is the only host; without, it and
    /// everything under it.
    pub domain: String,
    /// The path prefix this goes back to.
    pub path: String,
    /// When it stops being sent. `None` is a session cookie, which dies with the
    /// process rather than at a time.
    pub expires: Option<SystemTime>,
    /// Only over a connection nobody can read.
    pub secure: bool,
    /// Not reachable from script. Nothing here runs script, so this is carried to
    /// be honoured when something does — and to be shown to a reader now.
    pub http_only: bool,
    /// Whether it goes out on a request another site caused.
    pub same_site: SameSite,
    /// Whether it goes only to the exact host that set it, which is what a cookie
    /// with no `Domain` attribute means.
    pub host_only: bool,
    /// When it was first set. Kept across a replacement, because it orders the
    /// header and a site resetting a cookie should not jump the queue.
    pub created: SystemTime,
    /// When it was last sent, which is what decides who is evicted first.
    pub last_access: SystemTime,
}

impl Cookie {
    /// Whether this cookie has stopped being one.
    pub fn is_expired(&self, now: SystemTime) -> bool {
        self.expires.is_some_and(|expires| expires <= now)
    }

    /// The site this belongs to: its registrable domain, or its domain where
    /// there is no registrable one. What a page listing cookies groups by.
    pub fn site(&self) -> &str {
        site_of(&self.domain)
    }

    /// Whether this cookie outlives the process.
    ///
    /// A cookie with no expiry is a session cookie: it is sent for as long as the
    /// browser is open and is never written down. The split that decides what
    /// goes on disk.
    pub fn is_persistent(&self) -> bool {
        self.expires.is_some()
    }

    /// What identifies this cookie for the purpose of replacing it.
    fn identity(&self) -> (&str, &str, &str, bool) {
        (&self.name, &self.domain, &self.path, self.host_only)
    }
}

/// Why a `Set-Cookie` was not kept.
///
/// Stated rather than swallowed: every one of these is a thing a site did that a
/// person debugging their own site needs to see, and every one is a rule an
/// inspector should be able to name.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// The line itself could not be read.
    #[error("the Set-Cookie line could not be read: {0:?}")]
    Unreadable(Unreadable),

    /// The response came from something with no host to attach a cookie to.
    #[error("a response with no host cannot set a cookie")]
    NoHost,

    /// `Domain` named a registry rather than a registrant — `.co.uk`, `.com`,
    /// `.github.io`. The rule that stops a supercookie.
    #[error("Domain={0} is a public suffix, which no site may set a cookie for")]
    PublicSuffix(String),

    /// `Domain` named a host that is neither this one nor a parent of it. The
    /// rule that keeps `evil-example.com` from writing `example.com`'s cookies,
    /// and it is not a suffix comparison.
    #[error("Domain={domain} does not cover {host}")]
    NotOurDomain {
        /// What the attribute said.
        domain: String,
        /// Where the response actually came from.
        host: String,
    },

    /// `Secure` was asked for over a connection that is not.
    #[error("a Secure cookie cannot be set over {0}")]
    InsecureRequest(String),

    /// `SameSite=None` without `Secure`. A cookie that goes out on every
    /// cross-site request has to at least be one nobody on the path can read.
    #[error("SameSite=None requires Secure")]
    CrossSiteWithoutSecure,

    /// A `__Secure-` or `__Host-` name whose promise the cookie does not keep.
    /// The prefix is the promise: it is in the name, so it survives being
    /// repeated by anything that only sees names and values.
    #[error("the {0} prefix promises more than this cookie keeps")]
    BrokenPrefix(&'static str),
}

/// A jar of cookies.
///
/// The store is a flat list walked once per request. That is the honest shape for
/// what this holds — a few hundred cookies, and a request that has to sort them
/// anyway — and an index by domain is a thing to add when a profile asks for it
/// rather than because a list looks slow written down.
#[derive(Clone, Debug, Default)]
pub struct Jar {
    cookies: Vec<Cookie>,
    capacity: Capacity,
    /// Bumped whenever what would be written down changes.
    ///
    /// Only the persistent cookies count. A session cookie never reaches a disk,
    /// so a site that resets one on every response — which is most of them — must
    /// not be able to make the browser write a file on every response.
    kept_revision: u64,
}

impl Jar {
    /// An empty jar with the default capacity.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty jar that will hold what `capacity` says.
    pub fn with_capacity(capacity: Capacity) -> Self {
        Self {
            cookies: Vec::new(),
            capacity,
            kept_revision: 0,
        }
    }

    /// A number that changes whenever the cookies that outlive the process do.
    ///
    /// What a store on disk compares against to decide whether it has anything to
    /// write. Not a count and not a hash: only that it differs is meaningful.
    pub fn kept_revision(&self) -> u64 {
        self.kept_revision
    }

    /// Note that the persistent set changed, if it did.
    fn kept_changed(&mut self, before: u64) {
        let after = self.cookies.iter().filter(|c| c.is_persistent()).count() as u64;
        if after != before {
            self.kept_revision += 1;
        }
    }

    /// How many cookies here outlive the process.
    fn kept_count(&self) -> u64 {
        self.cookies.iter().filter(|c| c.is_persistent()).count() as u64
    }

    /// What this jar will hold.
    pub fn capacity(&self) -> Capacity {
        self.capacity
    }

    /// Everything kept, in no particular order. What a page listing cookies
    /// reads.
    pub fn all(&self) -> &[Cookie] {
        &self.cookies
    }

    /// How many cookies are kept.
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// Whether nothing is kept.
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Throw everything away.
    pub fn clear(&mut self) {
        let before = self.kept_count();
        self.cookies.clear();
        self.kept_changed(before);
    }

    /// Throw away everything belonging to one site, named by its registrable
    /// domain. Answers how many went.
    pub fn clear_site(&mut self, site: &str) -> usize {
        let kept = self.kept_count();
        let before = self.cookies.len();
        self.cookies.retain(|cookie| cookie.site() != site);
        self.kept_changed(kept);
        before - self.cookies.len()
    }

    /// Drop what has expired. Answers how many went.
    pub fn purge_expired(&mut self, now: SystemTime) -> usize {
        let kept = self.kept_count();
        let before = self.cookies.len();
        self.cookies.retain(|cookie| !cookie.is_expired(now));
        self.kept_changed(kept);
        before - self.cookies.len()
    }

    /// Drop every session cookie — the ones with no expiry, which is what
    /// closing the browser does to them.
    pub fn clear_session_cookies(&mut self) -> usize {
        let before = self.cookies.len();
        self.cookies.retain(Cookie::is_persistent);
        before - self.cookies.len()
    }

    /// Take one `Set-Cookie` header value that arrived on a response from `url`.
    ///
    /// This is the storage model, and the order of the checks is the
    /// specification's: what the line says is read first, then whether the site
    /// that sent it was entitled to say it.
    pub fn set(&mut self, url: &Url, line: &str, now: SystemTime) -> Result<(), Refused> {
        let parsed = SetCookie::parse(line).map_err(Refused::Unreadable)?;
        self.store(url, parsed, now)
    }

    /// Put a cookie in that has already been through the storage model.
    ///
    /// **This applies none of the rules.** It exists for one caller: a jar being
    /// read back from the file it was written to, where every cookie was checked
    /// when the site first set it and rechecking would need the request that is
    /// long gone. It is not a way to accept a cookie — [`Jar::set`] is — and
    /// anything reaching this from a network is a bug.
    pub fn put(&mut self, cookie: Cookie) {
        if cookie.is_persistent() {
            self.kept_revision += 1;
        }
        self.cookies.push(cookie);
    }

    /// Take a cookie that has already been read. Split out so a caller holding a
    /// parsed line — a persistent jar being loaded, a test — does not go back
    /// through the string.
    pub fn store(&mut self, url: &Url, parsed: SetCookie, now: SystemTime) -> Result<(), Refused> {
        let host = url.host_str().ok_or(Refused::NoHost)?.to_ascii_lowercase();

        // Where it goes back to. A `Domain` attribute widens the cookie to a
        // parent, and the two rules below are what bound how far.
        let (domain, host_only) = match parsed.domain.as_deref() {
            Some(domain) => {
                // A name a registry hands out is not a name anybody may write a
                // cookie for — except the registry's own host writing its own
                // cookie, which is the one case the specification carves out and
                // turns into a host-only cookie.
                if suffix::is_public_suffix(domain) {
                    if domain == host {
                        (host.clone(), true)
                    } else {
                        return Err(Refused::PublicSuffix(domain.to_owned()));
                    }
                } else if is_address(&host) {
                    // An address has no parent to widen to. Naming itself is
                    // allowed and means host-only; naming anything else is not.
                    if domain == host {
                        (host.clone(), true)
                    } else {
                        return Err(Refused::NotOurDomain {
                            domain: domain.to_owned(),
                            host,
                        });
                    }
                } else if domain_match(&host, domain) {
                    (domain.to_owned(), false)
                } else {
                    return Err(Refused::NotOurDomain {
                        domain: domain.to_owned(),
                        host,
                    });
                }
            }
            // No attribute is not "everything": it is this host and no other.
            None => (host.clone(), true),
        };

        let path = parsed.path.clone().unwrap_or_else(|| default_path(url));
        let secure_request = is_secure(url);

        if parsed.secure && !secure_request {
            return Err(Refused::InsecureRequest(url.scheme().to_owned()));
        }
        // A cookie that goes out on every cross-site request is one an attacker
        // can cause to be sent; it has to at least be one nobody on the path can
        // read.
        if parsed.same_site == SameSite::None && !parsed.secure {
            return Err(Refused::CrossSiteWithoutSecure);
        }

        // The two prefixes that carry their own rule. They are in the *name*, so
        // the promise survives every layer that only ever sees a name and a
        // value — which is the point of putting it there. Matched case-sensitively,
        // as the specification writes them.
        if parsed.name.starts_with("__Secure-") && !parsed.secure {
            return Err(Refused::BrokenPrefix("__Secure-"));
        }
        if parsed.name.starts_with("__Host-")
            && !(parsed.secure && host_only && parsed.domain.is_none() && path == "/")
        {
            return Err(Refused::BrokenPrefix("__Host-"));
        }

        // `Max-Age` outranks `Expires` wherever the two appear, whatever order
        // they were written in — and either is capped, because a lifetime is a
        // claim by the party that gains from it.
        let ceiling = now.checked_add(MAX_LIFETIME);
        let expires = match parsed.max_age {
            Some(seconds) if seconds <= 0 => Some(SystemTime::UNIX_EPOCH),
            Some(seconds) => now
                .checked_add(Duration::from_secs(seconds as u64))
                .filter(|when| ceiling.is_none_or(|ceiling| *when <= ceiling))
                .or(ceiling),
            None => parsed
                .expires
                .map(|when| match ceiling {
                    Some(ceiling) => when.min(ceiling),
                    None => when,
                })
                .or(None),
        };

        let mut cookie = Cookie {
            name: parsed.name,
            value: parsed.value,
            domain,
            path,
            expires,
            secure: parsed.secure,
            http_only: parsed.http_only,
            same_site: parsed.same_site,
            host_only,
            created: now,
            last_access: now,
        };

        // Replacing one keeps its creation time: the header is ordered by it, and
        // a site refreshing a cookie every request would otherwise walk itself to
        // the front of the queue.
        let mut touched_the_kept = false;
        if let Some(index) = self
            .cookies
            .iter()
            .position(|kept| kept.identity() == cookie.identity())
        {
            cookie.created = self.cookies[index].created;
            touched_the_kept = self.cookies[index].is_persistent();
            self.cookies.remove(index);
        }

        // An expiry in the past is how a server deletes a cookie: the old one is
        // gone by now, and the new one is not kept.
        if !cookie.is_expired(now) {
            touched_the_kept |= cookie.is_persistent();
            self.cookies.push(cookie);
            self.make_room(now);
        }
        // Counting is not enough here: a cookie replaced by one with a different
        // value leaves the count alone and still has to reach the disk.
        if touched_the_kept {
            self.kept_revision += 1;
        }

        Ok(())
    }

    /// Which cookies go on a request to `url`, in the order they belong in the
    /// header.
    ///
    /// Nothing here filters on `HttpOnly`: that attribute says a cookie is not
    /// reachable from script, and this is the request path, which is exactly
    /// where an `HttpOnly` cookie is supposed to go.
    pub fn matching(&self, url: &Url, context: Context, now: SystemTime) -> Vec<&Cookie> {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return Vec::new();
        };
        let path = url.path();
        let secure = is_secure(url);

        let mut chosen: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|cookie| cookie.goes_to(&host, path, secure, context, now))
            .collect();

        // The specification's order: the most specific path first, and among
        // equals the one set first. A server reading only the first of a repeated
        // name gets the one it would get anywhere else.
        chosen.sort_by(|left, right| {
            right
                .path
                .len()
                .cmp(&left.path.len())
                .then_with(|| left.created.cmp(&right.created))
        });
        chosen
    }

    /// The `Cookie` header for a request to `url`, or `None` when there is
    /// nothing to send — which is a request with no header at all, not one with
    /// an empty one.
    ///
    /// Takes the jar mutably because sending a cookie is using it, and what was
    /// used last is what decides who is evicted when the jar is full.
    pub fn header_for(&mut self, url: &Url, context: Context, now: SystemTime) -> Option<String> {
        let host = url.host_str().map(str::to_ascii_lowercase)?;
        let path = url.path().to_owned();
        let secure = is_secure(url);

        for cookie in &mut self.cookies {
            if cookie.goes_to(&host, &path, secure, context, now) {
                cookie.last_access = now;
            }
        }

        let chosen = self.matching(url, context, now);
        if chosen.is_empty() {
            return None;
        }
        let mut header = String::new();
        for cookie in chosen {
            if !header.is_empty() {
                header.push_str("; ");
            }
            // A nameless cookie goes back as a bare value, with no `=` — which is
            // what it was set as and the only way it round-trips.
            if !cookie.name.is_empty() {
                header.push_str(&cookie.name);
                header.push('=');
            }
            header.push_str(&cookie.value);
        }
        Some(header)
    }

    /// Bring the jar back inside its capacity: what has expired goes first, then
    /// whatever was used longest ago.
    fn make_room(&mut self, now: SystemTime) {
        self.purge_expired(now);

        // Per domain before in total, because one noisy domain must not evict
        // every other site's session.
        if let Some(domain) = self.cookies.last().map(|cookie| cookie.domain.clone()) {
            while self
                .cookies
                .iter()
                .filter(|cookie| cookie.domain == domain)
                .count()
                > self.capacity.per_domain
            {
                let Some(index) = self.least_recently_used(|cookie| cookie.domain == domain) else {
                    break;
                };
                self.cookies.remove(index);
            }
        }

        while self.cookies.len() > self.capacity.total {
            let Some(index) = self.least_recently_used(|_| true) else {
                break;
            };
            self.cookies.remove(index);
        }
    }

    /// The index of the cookie among those `wanted` that was sent longest ago.
    fn least_recently_used(&self, wanted: impl Fn(&Cookie) -> bool) -> Option<usize> {
        self.cookies
            .iter()
            .enumerate()
            .filter(|(_, cookie)| wanted(cookie))
            .min_by_key(|(_, cookie)| cookie.last_access)
            .map(|(index, _)| index)
    }
}

impl Cookie {
    /// Whether this cookie goes on a request to `host` at `path`.
    fn goes_to(
        &self,
        host: &str,
        path: &str,
        secure: bool,
        context: Context,
        now: SystemTime,
    ) -> bool {
        if self.is_expired(now) {
            return false;
        }
        let reaches_host = if self.host_only {
            self.domain == host
        } else {
            domain_match(host, &self.domain)
        };
        if !reaches_host || !path_match(path, &self.path) {
            return false;
        }
        if self.secure && !secure {
            return false;
        }
        match context {
            Context::SameSite => true,
            Context::CrossSiteNavigation => self.same_site != SameSite::Strict,
            Context::CrossSite => self.same_site == SameSite::None,
        }
    }
}

/// Whether a request to `host` carries a cookie kept for `domain`.
///
/// **This is not a suffix comparison, and the difference is the bug.**
/// `example.com` covers `www.example.com` because the character before the match
/// is the dot that makes it a whole label; it does not cover `evil-example.com`,
/// where that character is a letter. One byte, and it is the whole rule.
///
/// An address matches only itself: `1.2.3.4` must not be covered by `2.3.4`,
/// which reads as a parent domain and is not one.
fn domain_match(host: &str, domain: &str) -> bool {
    if host == domain {
        return true;
    }
    host.len() > domain.len()
        && !is_address(host)
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

/// Whether a request for `request` carries a cookie kept for `cookie`.
///
/// **This is not a prefix comparison either.** `/foo` covers `/foo/bar` and does
/// not cover `/foobar`: either the cookie's own path ends at a boundary, or the
/// next character of the request's is one.
///
/// Compared byte for byte, with no case folding and no decoding of escapes: a
/// path is a path, and two spellings of one are two paths.
fn path_match(request: &str, cookie: &str) -> bool {
    if request == cookie {
        return true;
    }
    request.starts_with(cookie)
        && (cookie.ends_with('/') || request.as_bytes()[cookie.len()] == b'/')
}

/// The path a cookie takes when its `Set-Cookie` named none: the directory the
/// response came from.
///
/// Everything up to the last `/` and not including it, so a cookie set by
/// `/app/login` covers `/app` and everything under it. A response from the root
/// gives `/`.
fn default_path(url: &Url) -> String {
    let path = url.path();
    if !path.starts_with('/') {
        return "/".to_owned();
    }
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(last) => path[..last].to_owned(),
    }
}

/// Whether a request to `url` travels somewhere a `Secure` cookie may go.
///
/// `https`, and plain `http` to this machine. The second is not a hole: a
/// connection that does not leave the machine has nobody on the path, and
/// refusing it would mean a `Secure` cookie could not be developed against
/// without a certificate. Every browser draws it here.
fn is_secure(url: &Url) -> bool {
    if matches!(url.scheme(), "https" | "wss") {
        return true;
    }
    match url.host() {
        Some(Host::Domain(name)) => name == "localhost" || name.ends_with(".localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    }
}

/// Whether a host is an address rather than a name.
///
/// An address has no parent: there is no domain above `1.2.3.4` however much
/// `2.3.4` looks like one.
fn is_address(host: &str) -> bool {
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    // `Url` writes an IPv6 host in brackets, and that is the form that reaches
    // here.
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .is_some_and(|inner| inner.parse::<std::net::Ipv6Addr>().is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed instant, so a test states a time rather than racing one.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn url(address: &str) -> Url {
        Url::parse(address).expect("a url")
    }

    /// Set a line and expect it to be kept.
    fn set(jar: &mut Jar, address: &str, line: &str) {
        jar.set(&url(address), line, now())
            .unwrap_or_else(|error| panic!("{line:?} from {address} should be kept: {error}"));
    }

    /// The header a request would carry, same-site, at `now()`.
    fn header(jar: &mut Jar, address: &str) -> Option<String> {
        jar.header_for(&url(address), Context::SameSite, now())
    }

    // --- domain matching -------------------------------------------------

    /// The near-miss this rule exists for. A suffix comparison hands
    /// `example.com`'s cookies to a host that merely ends with those characters,
    /// and the one byte that stops it is the dot.
    #[test]
    fn a_domain_match_is_not_a_suffix_match() {
        assert!(domain_match("example.com", "example.com"));
        assert!(domain_match("www.example.com", "example.com"));
        assert!(domain_match("a.b.example.com", "example.com"));

        assert!(
            !domain_match("evil-example.com", "example.com"),
            "the character before the match must be a dot"
        );
        assert!(!domain_match("notexample.com", "example.com"));
        // And it only ever widens downward.
        assert!(!domain_match("example.com", "www.example.com"));
        assert!(!domain_match("example.com.evil.test", "example.com"));
    }

    /// An address has no parent, however much a suffix of it reads like one.
    #[test]
    fn an_address_matches_only_itself() {
        assert!(domain_match("1.2.3.4", "1.2.3.4"));
        assert!(!domain_match("1.2.3.4", "2.3.4"));
        assert!(!domain_match("1.2.3.4", "4"));
        assert!(is_address("1.2.3.4"));
        assert!(is_address("[::1]"));
        assert!(!is_address("example.com"));
        assert!(!is_address("1.2.3.4.example.com"));
    }

    /// A response from `www.example.com` may widen a cookie to `example.com`,
    /// and may not reach anywhere else.
    #[test]
    fn a_site_may_widen_to_its_parent_and_no_further() {
        let mut jar = Jar::new();
        set(
            &mut jar,
            "https://www.example.com/",
            "a=1; Domain=example.com",
        );
        assert_eq!(
            header(&mut jar, "https://other.example.com/"),
            Some("a=1".into())
        );

        // Sideways, upward past the registrant, and to somebody else entirely.
        for domain in ["evil.test", "www.other.example.com", "co.uk"] {
            let error = jar
                .set(
                    &url("https://www.example.com/"),
                    &format!("b=2; Domain={domain}"),
                    now(),
                )
                .expect_err("should be refused");
            assert!(
                matches!(
                    error,
                    Refused::NotOurDomain { .. } | Refused::PublicSuffix(_)
                ),
                "Domain={domain} gave {error}"
            );
        }
    }

    /// Without a `Domain` attribute a cookie belongs to the host that set it and
    /// to nothing under it. The direction people get backwards.
    #[test]
    fn a_cookie_with_no_domain_is_host_only() {
        let mut jar = Jar::new();
        set(&mut jar, "https://example.com/", "a=1");
        assert_eq!(header(&mut jar, "https://example.com/"), Some("a=1".into()));
        assert_eq!(header(&mut jar, "https://www.example.com/"), None);
    }

    // --- the public suffix rule ------------------------------------------

    /// The rule that stops a supercookie. Without it one site under `.co.uk`
    /// writes a cookie every site under `.co.uk` sends back.
    #[test]
    fn nobody_may_set_a_cookie_for_a_registry() {
        let mut jar = Jar::new();
        for (address, domain) in [
            ("https://shop.co.uk/", "co.uk"),
            ("https://a.example.com/", "com"),
            ("https://project.github.io/", "github.io"),
        ] {
            assert!(
                matches!(
                    jar.set(&url(address), &format!("a=1; Domain={domain}"), now()),
                    Err(Refused::PublicSuffix(refused)) if refused == domain
                ),
                "Domain={domain} from {address} should be refused"
            );
        }
        assert!(jar.is_empty());
    }

    /// The one case the rule carves out: the registry's own host setting its own
    /// cookie, which becomes host-only rather than reaching its children.
    #[test]
    fn a_registry_naming_itself_gets_a_host_only_cookie() {
        let mut jar = Jar::new();
        set(&mut jar, "https://co.uk/", "a=1; Domain=co.uk");
        assert!(jar.all()[0].host_only);
        assert_eq!(header(&mut jar, "https://co.uk/"), Some("a=1".into()));
        assert_eq!(header(&mut jar, "https://shop.co.uk/"), None);
    }

    // --- path matching ---------------------------------------------------

    /// The other near-miss. `/foo` covers `/foo/bar` and does not cover
    /// `/foobar`.
    #[test]
    fn a_path_match_is_not_a_prefix_match() {
        assert!(path_match("/foo", "/foo"));
        assert!(path_match("/foo/bar", "/foo"));
        assert!(path_match("/foo/", "/foo"));
        assert!(path_match("/anything", "/"));

        assert!(!path_match("/foobar", "/foo"));
        assert!(!path_match("/foo", "/foo/bar"));
        // A cookie path already ending at a boundary needs no boundary after it.
        assert!(path_match("/foo/bar", "/foo/"));
        assert!(!path_match("/food/bar", "/foo/"));
        // Paths are bytes: two spellings are two paths.
        assert!(!path_match("/Foo/bar", "/foo"));
    }

    /// A `Set-Cookie` with no `Path` takes the directory of the response, not the
    /// response's own address.
    #[test]
    fn a_cookie_with_no_path_takes_the_directory_it_came_from() {
        assert_eq!(default_path(&url("https://x.test/app/login")), "/app");
        assert_eq!(
            default_path(&url("https://x.test/app/sub/login")),
            "/app/sub"
        );
        assert_eq!(default_path(&url("https://x.test/login")), "/");
        assert_eq!(default_path(&url("https://x.test/")), "/");
        assert_eq!(default_path(&url("https://x.test")), "/");
        // A trailing slash is a directory, and everything before it is the path.
        assert_eq!(default_path(&url("https://x.test/app/")), "/app");

        let mut jar = Jar::new();
        set(&mut jar, "https://x.test/app/login", "a=1");
        assert_eq!(
            header(&mut jar, "https://x.test/app/other"),
            Some("a=1".into())
        );
        assert_eq!(header(&mut jar, "https://x.test/"), None);
        assert_eq!(header(&mut jar, "https://x.test/apple"), None);
    }

    // --- Secure and the schemes ------------------------------------------

    #[test]
    fn a_secure_cookie_needs_a_secure_connection_both_ways() {
        let mut jar = Jar::new();
        assert!(matches!(
            jar.set(&url("http://example.com/"), "a=1; Secure", now()),
            Err(Refused::InsecureRequest(_))
        ));

        set(&mut jar, "https://example.com/", "a=1; Secure");
        assert_eq!(header(&mut jar, "https://example.com/"), Some("a=1".into()));
        assert_eq!(header(&mut jar, "http://example.com/"), None);
    }

    /// A connection that does not leave the machine has nobody on the path, so a
    /// `Secure` cookie may be developed against without a certificate.
    #[test]
    fn the_local_machine_counts_as_secure() {
        assert!(is_secure(&url("https://example.com/")));
        assert!(is_secure(&url("http://localhost:8080/")));
        assert!(is_secure(&url("http://dev.localhost/")));
        assert!(is_secure(&url("http://127.0.0.1:3000/")));
        assert!(is_secure(&url("http://[::1]/")));
        assert!(!is_secure(&url("http://example.com/")));
        assert!(!is_secure(&url("http://192.168.1.10/")));

        let mut jar = Jar::new();
        set(&mut jar, "http://localhost:8080/", "a=1; Secure");
        assert_eq!(
            header(&mut jar, "http://localhost:8080/"),
            Some("a=1".into())
        );
    }

    // --- SameSite --------------------------------------------------------

    /// The three answers, against the three ways a request comes to be made.
    #[test]
    fn same_site_decides_what_another_sites_request_carries() {
        let mut jar = Jar::new();
        set(
            &mut jar,
            "https://example.com/",
            "strict=1; SameSite=Strict",
        );
        set(&mut jar, "https://example.com/", "lax=1; SameSite=Lax");
        set(
            &mut jar,
            "https://example.com/",
            "none=1; SameSite=None; Secure",
        );

        let names = |jar: &mut Jar, context| {
            let mut names: Vec<String> = jar
                .matching(&url("https://example.com/"), context, now())
                .iter()
                .map(|cookie| cookie.name.clone())
                .collect();
            names.sort();
            names
        };

        assert_eq!(
            names(&mut jar, Context::SameSite),
            ["lax", "none", "strict"]
        );
        assert_eq!(
            names(&mut jar, Context::CrossSiteNavigation),
            ["lax", "none"]
        );
        assert_eq!(names(&mut jar, Context::CrossSite), ["none"]);
    }

    /// A cookie that goes out on every cross-site request must at least be one
    /// nobody on the path can read.
    #[test]
    fn same_site_none_without_secure_is_refused() {
        let mut jar = Jar::new();
        assert_eq!(
            jar.set(&url("https://example.com/"), "a=1; SameSite=None", now()),
            Err(Refused::CrossSiteWithoutSecure)
        );
        set(
            &mut jar,
            "https://example.com/",
            "a=1; SameSite=None; Secure",
        );
    }

    /// A cookie with no `SameSite` is `Lax`, not "anywhere". The default is the
    /// defence.
    #[test]
    fn a_cookie_with_no_same_site_is_lax() {
        let mut jar = Jar::new();
        set(&mut jar, "https://example.com/", "a=1");
        assert!(
            jar.matching(&url("https://example.com/"), Context::CrossSite, now())
                .is_empty()
        );
        assert_eq!(
            jar.matching(
                &url("https://example.com/"),
                Context::CrossSiteNavigation,
                now()
            )
            .len(),
            1
        );
    }

    // --- the name prefixes -----------------------------------------------

    /// The promise is in the name, so it survives every layer that only sees a
    /// name and a value.
    #[test]
    fn the_secure_prefix_is_enforced() {
        let mut jar = Jar::new();
        assert_eq!(
            jar.set(&url("https://example.com/"), "__Secure-a=1", now()),
            Err(Refused::BrokenPrefix("__Secure-"))
        );
        set(&mut jar, "https://example.com/", "__Secure-a=1; Secure");
    }

    /// `__Host-` promises more: this host only, the whole site, over a secure
    /// connection. Break any one of the three and the cookie is refused.
    #[test]
    fn the_host_prefix_is_enforced() {
        let mut jar = Jar::new();
        let refused = |jar: &mut Jar, address: &str, line: &str| {
            assert_eq!(
                jar.set(&url(address), line, now()),
                Err(Refused::BrokenPrefix("__Host-")),
                "{line} from {address}"
            );
        };
        let root = "https://example.com/";
        refused(&mut jar, root, "__Host-a=1; Path=/");
        refused(&mut jar, root, "__Host-a=1; Secure; Path=/app");
        refused(
            &mut jar,
            root,
            "__Host-a=1; Secure; Path=/; Domain=example.com",
        );
        // Over plain http the `Secure` attribute is refused before the prefix is
        // reached, which is the same answer arrived at one rule earlier.
        assert!(matches!(
            jar.set(
                &url("http://example.com/"),
                "__Host-a=1; Secure; Path=/",
                now()
            ),
            Err(Refused::InsecureRequest(_))
        ));
        // The path has to be written, because a response from a subdirectory
        // would otherwise take that directory as the cookie's path and the
        // prefix's promise of the whole site would be silently untrue.
        refused(
            &mut jar,
            "https://example.com/app/login",
            "__Host-a=1; Secure",
        );

        set(&mut jar, root, "__Host-a=1; Secure; Path=/");
        assert!(jar.all()[0].host_only);
        // From the root the default path is already `/`, so it need not be
        // spelled — which is the one case that reads like an exception and is not.
        set(&mut jar, root, "__Host-b=1; Secure");
    }

    // --- lifetime --------------------------------------------------------

    #[test]
    fn max_age_outranks_expires_whatever_order_they_are_written_in() {
        let mut jar = Jar::new();
        // An `Expires` long past and a `Max-Age` in the future: the cookie lives.
        set(
            &mut jar,
            "https://example.com/",
            "a=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=600",
        );
        assert_eq!(jar.all()[0].expires, Some(now() + Duration::from_secs(600)));

        // And the other way round, written in the other order.
        set(
            &mut jar,
            "https://example.com/",
            "b=1; Max-Age=-1; Expires=Sun, 06 Nov 2094 08:49:37 GMT",
        );
        assert!(
            jar.all().iter().all(|cookie| cookie.name != "b"),
            "Max-Age=-1 deletes whatever Expires said"
        );
    }

    /// How a server deletes a cookie: it sets it again with an expiry in the
    /// past. The old one has to go, and the new one must not be kept.
    #[test]
    fn an_expiry_in_the_past_removes_what_was_there() {
        let mut jar = Jar::new();
        set(&mut jar, "https://example.com/", "a=1; Max-Age=600");
        assert_eq!(jar.len(), 1);
        set(&mut jar, "https://example.com/", "a=1; Max-Age=0");
        assert!(jar.is_empty());

        set(&mut jar, "https://example.com/", "b=1; Max-Age=600");
        set(
            &mut jar,
            "https://example.com/",
            "b=1; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
        );
        assert!(jar.is_empty());
    }

    /// A lifetime is a claim by the party that gains from it, so it has a
    /// ceiling.
    #[test]
    fn a_lifetime_is_capped() {
        let mut jar = Jar::new();
        set(&mut jar, "https://example.com/", "a=1; Max-Age=99999999999");
        assert_eq!(jar.all()[0].expires, Some(now() + MAX_LIFETIME));

        set(
            &mut jar,
            "https://example.com/",
            "b=1; Expires=Sun, 06 Nov 2094 08:49:37 GMT",
        );
        let capped = jar.all().iter().find(|c| c.name == "b").expect("kept");
        assert_eq!(capped.expires, Some(now() + MAX_LIFETIME));
    }

    /// No expiry at all is a session cookie: it is sent, and it dies with the
    /// process rather than at a time.
    #[test]
    fn a_cookie_with_no_expiry_is_a_session_cookie() {
        let mut jar = Jar::new();
        set(&mut jar, "https://example.com/", "a=1");
        set(&mut jar, "https://example.com/", "b=1; Max-Age=600");
        assert_eq!(jar.all()[0].expires, None);

        assert_eq!(jar.clear_session_cookies(), 1);
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.all()[0].name, "b");
    }

    #[test]
    fn an_expired_cookie_is_neither_sent_nor_kept() {
        let mut jar = Jar::new();
        set(&mut jar, "https://example.com/", "a=1; Max-Age=600");
        let later = now() + Duration::from_secs(601);
        assert!(
            jar.matching(&url("https://example.com/"), Context::SameSite, later)
                .is_empty()
        );
        assert_eq!(jar.purge_expired(later), 1);
    }

    // --- the header ------------------------------------------------------

    /// The specification's order: the most specific path first, and among equals
    /// the one set first.
    #[test]
    fn the_header_is_ordered_by_path_then_by_age() {
        let mut jar = Jar::new();
        set(&mut jar, "https://x.test/", "root=1; Path=/");
        set(&mut jar, "https://x.test/", "deep=1; Path=/a/b");
        set(&mut jar, "https://x.test/", "mid=1; Path=/a");
        assert_eq!(
            header(&mut jar, "https://x.test/a/b/c"),
            Some("deep=1; mid=1; root=1".into())
        );

        // Two cookies on the same path come out in the order they were set —
        // and a cookie reset later keeps its place, because a replacement keeps
        // its creation time.
        let mut jar = Jar::new();
        jar.set(&url("https://x.test/"), "first=1", now())
            .expect("kept");
        jar.set(
            &url("https://x.test/"),
            "second=1",
            now() + Duration::from_secs(1),
        )
        .expect("kept");
        jar.set(
            &url("https://x.test/"),
            "first=2",
            now() + Duration::from_secs(2),
        )
        .expect("kept");
        assert_eq!(
            jar.header_for(
                &url("https://x.test/"),
                Context::SameSite,
                now() + Duration::from_secs(3)
            ),
            Some("first=2; second=1".into())
        );
    }

    /// Nothing to send is a request with no header, not a request with an empty
    /// one.
    #[test]
    fn nothing_to_send_is_no_header() {
        let mut jar = Jar::new();
        assert_eq!(header(&mut jar, "https://example.com/"), None);
        set(&mut jar, "https://example.com/", "a=1");
        assert_eq!(header(&mut jar, "https://other.test/"), None);
    }

    /// A nameless cookie goes back as a bare value — the only way it round-trips.
    #[test]
    fn a_nameless_cookie_goes_back_as_a_bare_value() {
        let mut jar = Jar::new();
        set(&mut jar, "https://x.test/", "=alone");
        assert_eq!(header(&mut jar, "https://x.test/"), Some("alone".into()));
    }

    /// A cookie replaced keeps its identity and loses its old value. The identity
    /// is name, domain, path and host-only together: change any one and it is a
    /// different cookie that coexists with the first.
    #[test]
    fn a_cookie_is_replaced_only_by_one_with_the_same_identity() {
        let mut jar = Jar::new();
        set(&mut jar, "https://www.example.com/", "a=1");
        set(&mut jar, "https://www.example.com/", "a=2");
        assert_eq!(jar.len(), 1);
        assert_eq!(jar.all()[0].value, "2");

        // Same name, different path: two cookies, and both go out.
        set(&mut jar, "https://www.example.com/", "a=3; Path=/app");
        assert_eq!(jar.len(), 2);
        // Same name, widened to the parent: a third, and it is not the same one.
        set(
            &mut jar,
            "https://www.example.com/",
            "a=4; Domain=example.com",
        );
        assert_eq!(jar.len(), 3);
        assert_eq!(
            header(&mut jar, "https://www.example.com/app/x"),
            Some("a=3; a=2; a=4".into())
        );
    }

    // --- the jar's own bounds --------------------------------------------

    /// One domain filling the jar must not evict every other site's session.
    #[test]
    fn a_domain_is_capped_before_the_jar_is() {
        let mut jar = Jar::with_capacity(Capacity {
            per_domain: 3,
            total: 10,
        });
        set(&mut jar, "https://other.test/", "keep=1");
        for index in 0..10 {
            jar.set(
                &url("https://noisy.test/"),
                &format!("c{index}=1"),
                now() + Duration::from_secs(index),
            )
            .expect("kept");
        }
        assert_eq!(
            jar.all()
                .iter()
                .filter(|c| c.domain == "noisy.test")
                .count(),
            3
        );
        assert!(
            jar.all().iter().any(|c| c.name == "keep"),
            "another site's cookie survives"
        );
        // The three left are the three most recently set, because nothing has
        // been sent and last-access starts at creation.
        let mut kept: Vec<&str> = jar
            .all()
            .iter()
            .filter(|c| c.domain == "noisy.test")
            .map(|c| c.name.as_str())
            .collect();
        kept.sort();
        assert_eq!(kept, ["c7", "c8", "c9"]);
    }

    /// Being sent is being used, and what was used longest ago is what goes.
    #[test]
    fn eviction_takes_what_was_sent_longest_ago() {
        let mut jar = Jar::with_capacity(Capacity {
            per_domain: 2,
            total: 10,
        });
        jar.set(&url("https://x.test/a"), "old=1; Path=/a", now())
            .expect("kept");
        jar.set(
            &url("https://x.test/b"),
            "new=1; Path=/b",
            now() + Duration::from_secs(1),
        )
        .expect("kept");

        // Send the older one, which makes it the more recently used.
        jar.header_for(
            &url("https://x.test/a/x"),
            Context::SameSite,
            now() + Duration::from_secs(2),
        );

        jar.set(
            &url("https://x.test/c"),
            "third=1; Path=/c",
            now() + Duration::from_secs(3),
        )
        .expect("kept");

        let names: Vec<&str> = jar.all().iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"old"), "it was used most recently");
        assert!(names.contains(&"third"));
        assert!(!names.contains(&"new"), "it was used longest ago");
    }

    // --- what a page listing cookies needs -------------------------------

    #[test]
    fn cookies_group_by_the_site_that_set_them() {
        let mut jar = Jar::new();
        set(&mut jar, "https://www.example.co.uk/", "a=1");
        set(&mut jar, "https://shop.example.co.uk/", "b=1");
        set(&mut jar, "https://other.test/", "c=1");
        assert_eq!(jar.all()[0].site(), "example.co.uk");
        assert_eq!(jar.all()[1].site(), "example.co.uk");
        assert_eq!(jar.all()[2].site(), "other.test");

        assert_eq!(jar.clear_site("example.co.uk"), 2);
        assert_eq!(jar.len(), 1);
        jar.clear();
        assert!(jar.is_empty());
    }

    /// A response from something with no host has nothing to attach a cookie to.
    #[test]
    fn a_response_with_no_host_sets_nothing() {
        let mut jar = Jar::new();
        assert_eq!(
            jar.set(&url("file:///tmp/page.html"), "a=1", now()),
            Err(Refused::NoHost)
        );
        assert!(
            jar.matching(&url("file:///tmp/page.html"), Context::SameSite, now())
                .is_empty()
        );
    }

    /// Two addresses are the same site when they share a registrable domain and a
    /// scheme. The scheme half is not pedantry: without it a page served over
    /// plain http, which anyone on the path can write, counts as the same site as
    /// the https one.
    #[test]
    fn a_site_is_a_registrable_domain_and_a_scheme() {
        let same = |one, other| same_site(&url(one), &url(other));
        assert!(same(
            "https://www.example.com/",
            "https://api.example.com/x"
        ));
        assert!(same("https://example.com/", "https://example.com:8443/"));
        assert!(!same("https://example.com/", "http://example.com/"));
        assert!(!same("https://example.com/", "https://example.org/"));
        assert!(!same("https://example.co.uk/", "https://other.co.uk/"));
        // A machine on the local network is its own site, and is never taken
        // apart: `127.0.0.1` read as a name gives `0.1`, which two other machines
        // would share.
        assert_eq!(site_of("127.0.0.1"), "127.0.0.1");
        assert_eq!(site_of("192.168.0.1"), "192.168.0.1");
        assert!(!same("http://127.0.0.1:8080/", "http://192.168.0.1:8080/"));
        assert!(same("http://127.0.0.1:8080/", "http://127.0.0.1:9090/"));
        // And so is a name with nothing above it.
        assert_eq!(site_of("localhost"), "localhost");
        assert!(!same("http://localhost:8080/", "http://127.0.0.1:8080/"));
    }

    /// A cookie kept for an address groups under that address, not under the tail
    /// of it that reads like a domain.
    #[test]
    fn a_cookie_from_an_address_belongs_to_that_address() {
        let mut jar = Jar::new();
        set(&mut jar, "http://127.0.0.1:8080/", "a=1");
        assert_eq!(jar.all()[0].site(), "127.0.0.1");
        assert_eq!(jar.clear_site("0.1"), 0);
        assert_eq!(jar.clear_site("127.0.0.1"), 1);
    }

    /// Nobody caused a typed address, so there is no other site involved and the
    /// request is same-site with wherever it goes. This is what makes a bookmark
    /// to a bank open it signed in.
    #[test]
    fn a_request_nobody_caused_is_same_site() {
        assert_eq!(
            Context::of(&url("https://bank.test/"), None, true),
            Context::SameSite
        );
        assert_eq!(
            Context::of(
                &url("https://bank.test/"),
                Some(&url("https://bank.test/app")),
                false
            ),
            Context::SameSite
        );
        assert_eq!(
            Context::of(
                &url("https://bank.test/"),
                Some(&url("https://elsewhere.test/")),
                true
            ),
            Context::CrossSiteNavigation
        );
        assert_eq!(
            Context::of(
                &url("https://bank.test/"),
                Some(&url("https://elsewhere.test/")),
                false
            ),
            Context::CrossSite
        );
    }

    /// A host is compared canonically, so the case a server wrote does not make a
    /// second cookie.
    #[test]
    fn a_host_is_matched_in_one_case() {
        let mut jar = Jar::new();
        set(&mut jar, "https://EXAMPLE.com/", "a=1");
        assert_eq!(jar.all()[0].domain, "example.com");
        assert_eq!(header(&mut jar, "https://Example.COM/"), Some("a=1".into()));
    }
}
