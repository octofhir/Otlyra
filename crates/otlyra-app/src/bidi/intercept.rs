//! Requests a driver has asked to be stopped before they are sent.
//!
//! # What it is for
//!
//! Three things, and they are the reason interception is the one part of the
//! protocol people ask for by name. A request can be **answered from nothing**,
//! so a page can be shown a server that does not exist and a state that takes a
//! broken backend to reach can be reached without one. It can be **stopped**,
//! which is what blocking a tracker or a font is. And it can be **sent somewhere
//! else**, which is how a page is pointed at a fixture.
//!
//! # Why this could not exist until navigation stopped blocking
//!
//! A held request is answered by a *later command*. While `browsingContext.navigate`
//! waited inside its own dispatch, the thread that would have read that command
//! was the thread sitting in the wait — so the load waited for a command that
//! could not arrive, and the browser stopped. Parking a navigation instead of
//! waiting for it is what makes this possible at all, and it is why the two were
//! built in that order.
//!
//! # What is here
//!
//! The request phase, whole: `beforeRequestSent`, and the four commands that end
//! it. The response phase is not, and says so rather than accepting an intercept
//! it would never report: pausing after the headers and before the body would
//! mean a loader that hands back a response in two pieces, and ours returns one
//! finished resource. Nothing here pretends otherwise.

use serde_json::Value;

use super::Error;

/// One standing request to hold things.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intercept {
    /// What a driver calls it, and what it removes it by.
    pub id: String,
    /// Which addresses it is about. Empty means every one of them.
    pub patterns: Vec<Pattern>,
}

/// Which addresses an intercept is about.
///
/// The specification's two shapes. A whole URL is the exact case — *this file,
/// nothing else* — and the parts are what an intercept is usually written with,
/// because *everything from this host* is the useful question and spelling out
/// every path is not a way to ask it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// One address, matched whole.
    Whole(String),
    /// Named parts, each matched exactly, and each optional.
    Parts {
        /// `https`, without the colon.
        protocol: Option<String>,
        /// The host, without the port.
        hostname: Option<String>,
        /// The port, as it is written.
        port: Option<String>,
        /// The path, leading slash and all.
        pathname: Option<String>,
        /// The query, without the `?`.
        search: Option<String>,
    },
}

impl Pattern {
    /// Read one out of what a client sent.
    pub fn parse(value: &Value) -> Result<Self, Error> {
        let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
        match value.get("type").and_then(Value::as_str) {
            Some("string") => {
                let pattern = value
                    .get("pattern")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Error::invalid("a string url pattern needs a pattern"))?;
                Ok(Self::Whole(pattern.to_owned()))
            }
            Some("pattern") => {
                let parts = Self::Parts {
                    protocol: text("protocol"),
                    hostname: text("hostname"),
                    port: text("port"),
                    pathname: text("pathname"),
                    search: text("search"),
                };
                if parts == Self::empty_parts() {
                    // Every part absent matches every address, which is what an
                    // intercept with no patterns already means — and is almost
                    // never what somebody who wrote out a pattern object meant.
                    return Err(Error::invalid(
                        "a pattern with no parts matches everything: send no patterns instead",
                    ));
                }
                Ok(parts)
            }
            Some(other) => Err(Error::invalid(format!(
                "{other:?} is not a url pattern type: it is string or pattern"
            ))),
            None => Err(Error::invalid("a url pattern needs a type")),
        }
    }

    fn empty_parts() -> Self {
        Self::Parts {
            protocol: None,
            hostname: None,
            port: None,
            pathname: None,
            search: None,
        }
    }

    /// Whether this pattern is about `url`.
    #[must_use]
    pub fn matches(&self, url: &str) -> bool {
        match self {
            Self::Whole(wanted) => wanted == url,
            Self::Parts {
                protocol,
                hostname,
                port,
                pathname,
                search,
            } => {
                // An address that will not parse matches nothing rather than
                // everything: an intercept is a thing that *stops* requests, and
                // one that swallowed whatever it could not read would be a
                // browser that stops loading and cannot say why.
                let Ok(parsed) = url::Url::parse(url) else {
                    return false;
                };
                let same = |wanted: &Option<String>, found: Option<&str>| match wanted {
                    None => true,
                    Some(wanted) => found == Some(wanted.as_str()),
                };
                same(protocol, Some(parsed.scheme()))
                    && same(hostname, parsed.host_str())
                    && same(port, parsed.port().map(|port| port.to_string()).as_deref())
                    && same(pathname, Some(parsed.path()))
                    && same(search, parsed.query())
            }
        }
    }
}

impl Intercept {
    /// Whether this intercept is about `url`.
    ///
    /// No patterns means every address, which is what the specification says and
    /// what a driver that wants to see everything sends.
    #[must_use]
    pub fn matches(&self, url: &str) -> bool {
        self.patterns.is_empty() || self.patterns.iter().any(|pattern| pattern.matches(url))
    }
}

/// The phases the specification has, and which of them this browser can offer.
///
/// A driver that asks for one that is not here is refused rather than quietly
/// registered: an intercept that never fires looks exactly like a page that never
/// made the request, and the driver would go looking at the page.
pub fn check_phases(value: Option<&Value>) -> Result<(), Error> {
    let phases = value
        .and_then(Value::as_array)
        .ok_or_else(|| Error::invalid("addIntercept needs phases"))?;
    if phases.is_empty() {
        return Err(Error::invalid("addIntercept needs at least one phase"));
    }
    for phase in phases {
        match phase.as_str() {
            Some("beforeRequestSent") => {}
            Some(phase @ ("responseStarted" | "authRequired")) => {
                return Err(Error::not_yet(
                    &format!("intercepting at {phase}"),
                    "a loader that hands a response back in two pieces, which ours does not",
                ));
            }
            other => {
                return Err(Error::invalid(format!(
                    "{:?} is not a phase",
                    other.unwrap_or_default()
                )));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_whole_address_matches_itself_and_nothing_near_it() {
        let pattern = Pattern::parse(&json!({
            "type": "string",
            "pattern": "https://a.example/one",
        }))
        .expect("a pattern");
        assert!(pattern.matches("https://a.example/one"));
        assert!(!pattern.matches("https://a.example/one/two"));
        assert!(!pattern.matches("https://a.example/"));
    }

    #[test]
    fn parts_left_out_are_not_asked_about() {
        // The useful shape: everything from one host, whatever the path.
        let pattern = Pattern::parse(&json!({
            "type": "pattern",
            "hostname": "ads.example",
        }))
        .expect("a pattern");
        assert!(pattern.matches("https://ads.example/a.js"));
        assert!(pattern.matches("http://ads.example/b/c?d=e"));
        assert!(!pattern.matches("https://a.example/a.js"));
    }

    #[test]
    fn every_named_part_has_to_agree() {
        let pattern = Pattern::parse(&json!({
            "type": "pattern",
            "protocol": "https",
            "hostname": "a.example",
            "pathname": "/one",
        }))
        .expect("a pattern");
        assert!(pattern.matches("https://a.example/one"));
        assert!(!pattern.matches("http://a.example/one"));
        assert!(!pattern.matches("https://a.example/two"));
    }

    #[test]
    fn an_address_that_is_not_one_matches_nothing() {
        let pattern = Pattern::parse(&json!({"type": "pattern", "hostname": "a.example"}))
            .expect("a pattern");
        // Swallowing it would be a browser that stops loading and cannot say why.
        assert!(!pattern.matches("not an address"));
    }

    #[test]
    fn an_intercept_with_no_patterns_is_about_everything() {
        let all = Intercept {
            id: "1".to_owned(),
            patterns: Vec::new(),
        };
        assert!(all.matches("https://anywhere.example/x"));
    }

    #[test]
    fn a_pattern_with_no_parts_is_refused_rather_than_matching_everything() {
        assert!(Pattern::parse(&json!({"type": "pattern"})).is_err());
    }

    #[test]
    fn a_phase_this_browser_cannot_offer_is_refused_rather_than_registered() {
        // An intercept that never fires looks exactly like a request the page
        // never made, and the driver would go looking at the page.
        let error = check_phases(Some(&json!(["responseStarted"]))).unwrap_err();
        assert_eq!(error.code, "unsupported operation");
        assert!(check_phases(Some(&json!(["beforeRequestSent"]))).is_ok());
        assert!(check_phases(Some(&json!([]))).is_err());
        assert!(check_phases(None).is_err());
    }
}
