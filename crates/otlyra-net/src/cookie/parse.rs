//! What one `Set-Cookie` line says, before the request it arrived on is asked
//! about it.
//!
//! Reading the line and accepting the cookie are two jobs and are kept apart.
//! This one is pure: it takes a string and answers with what was written, with no
//! clock, no request and no jar. Whether the site was allowed to write it is
//! [`super::jar`]'s question, and it is the one with all the rules in it.

use std::time::SystemTime;

use super::date;

/// Whether a cookie goes out on a request that another site caused.
///
/// The defence against a form on one site making a request to another that
/// arrives already signed in. `Lax` is the default when the attribute is absent
/// or unreadable — the modern one, and the reason a `SameSite` nobody wrote is
/// not the same as no rule at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SameSite {
    /// Only on a request the site itself made.
    Strict,
    /// That, and on a top-level navigation *to* the site — following a link, or
    /// typing the address. A request for a subresource still goes without it.
    #[default]
    Lax,
    /// On any request at all, which is only allowed alongside `Secure`.
    None,
}

/// The largest a name and value together may be, in bytes.
///
/// The specification's number. A cap is not tidiness: a response can name as many
/// cookies as it likes and every one of them is sent back on every later request,
/// so an uncapped jar is a way to make a reader's own connection unusable.
pub const MAX_NAME_AND_VALUE: usize = 4096;

/// The largest one attribute's value may be, in bytes. Also the specification's.
pub const MAX_ATTRIBUTE_VALUE: usize = 1024;

/// One `Set-Cookie` line, read but not yet accepted.
///
/// `Max-Age` is kept as it was written rather than as an instant, because turning
/// it into one needs a clock and this file deliberately has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetCookie {
    /// The name. May be empty, which is a cookie sent back as a bare value.
    pub name: String,
    /// The value, exactly as written — not decoded. Percent-encoding, base64 and
    /// the rest are conventions between a site and itself.
    pub value: String,
    /// The `Domain` attribute, lowercased and with any leading `.` removed.
    pub domain: Option<String>,
    /// The `Path` attribute, if one was written and began with `/`.
    pub path: Option<String>,
    /// `Max-Age` in seconds, which may be zero or negative — that is how a cookie
    /// is deleted.
    pub max_age: Option<i64>,
    /// `Expires`, if it was written and could be read.
    pub expires: Option<SystemTime>,
    /// `Secure`: only over a connection nobody can read.
    pub secure: bool,
    /// `HttpOnly`: not reachable from script. Nothing here runs script, so this
    /// is carried to be honoured later and to be shown to a reader now.
    pub http_only: bool,
    /// `SameSite`, or [`SameSite::Lax`] where none was written.
    pub same_site: SameSite,
}

/// Why a line was not read at all.
///
/// Separate from the reasons a *jar* refuses a cookie, because these are about
/// the line and those are about the site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// The line held a control character. A response that can inject a newline
    /// into a header can inject a header, so this is refused rather than trimmed.
    ControlCharacter,
    /// There was no `=`, so there is no cookie — only an attribute list with
    /// nothing to attach it to.
    NoNameValuePair,
    /// The name and the value were both empty.
    Nameless,
    /// The name and value together are over [`MAX_NAME_AND_VALUE`].
    TooLarge,
}

/// Strip the spaces and tabs from both ends.
///
/// The specification's WSP, which is those two and not the rest of what
/// [`str::trim`] would take: a vertical tab in a cookie value is a value with a
/// vertical tab in it.
fn trim(input: &str) -> &str {
    input.trim_matches([' ', '\t'])
}

/// Split at the first `=`, the way every part of this grammar does.
fn split_once_eq(input: &str) -> Option<(&str, &str)> {
    input.split_once('=')
}

impl SetCookie {
    /// Read one `Set-Cookie` header value.
    ///
    /// The header's *value* — `name=value; Path=/` — not the whole line. A
    /// response carrying several sets several headers, and each is read on its
    /// own; two cookies are never written into one header, whatever a comma in a
    /// date might suggest.
    pub fn parse(line: &str) -> Result<Self, Unreadable> {
        // Before anything is split: a control character means the response's
        // headers cannot be trusted to be the headers the server meant to send.
        // Tab is not one — it is legal whitespace inside a header value.
        if line
            .bytes()
            .any(|byte| (byte <= 0x08) || (0x0A..=0x1F).contains(&byte) || byte == 0x7F)
        {
            return Err(Unreadable::ControlCharacter);
        }

        let (pair, attributes) = match line.split_once(';') {
            Some((pair, attributes)) => (pair, attributes),
            None => (line, ""),
        };

        let (name, value) = split_once_eq(pair).ok_or(Unreadable::NoNameValuePair)?;
        let (name, value) = (trim(name), trim(value));
        // A nameless cookie with a value is legal and in use — it goes back as a
        // bare value — but a cookie that is neither a name nor a value is nothing
        // at all.
        if name.is_empty() && value.is_empty() {
            return Err(Unreadable::Nameless);
        }
        if name.len() + value.len() > MAX_NAME_AND_VALUE {
            return Err(Unreadable::TooLarge);
        }

        let mut cookie = Self {
            name: name.to_owned(),
            value: value.to_owned(),
            domain: None,
            path: None,
            max_age: None,
            expires: None,
            secure: false,
            http_only: false,
            same_site: SameSite::default(),
        };

        for attribute in attributes.split(';') {
            // An attribute with no `=` is one whose presence is the whole of it:
            // `Secure`, `HttpOnly`.
            let (name, value) = match split_once_eq(attribute) {
                Some((name, value)) => (trim(name), trim(value)),
                None => (trim(attribute), ""),
            };
            if name.is_empty() || value.len() > MAX_ATTRIBUTE_VALUE {
                continue;
            }
            // Written twice, the last one is the one that counts — which is what
            // falling through and assigning gives, and is the specification's
            // rule.
            match name {
                name if name.eq_ignore_ascii_case("expires") => {
                    // A date that cannot be read is an attribute that was not
                    // written, leaving a session cookie rather than no cookie.
                    if let Some(when) = date::parse(value) {
                        cookie.expires = Some(when);
                    }
                }
                name if name.eq_ignore_ascii_case("max-age") => {
                    cookie.max_age = read_max_age(value).or(cookie.max_age);
                }
                name if name.eq_ignore_ascii_case("domain") => {
                    if !value.is_empty() {
                        // A leading dot is how this was written for twenty years
                        // and means nothing: `.example.com` and `example.com` are
                        // the same attribute.
                        let domain = value.strip_prefix('.').unwrap_or(value);
                        cookie.domain = Some(domain.to_ascii_lowercase());
                    }
                }
                name if name.eq_ignore_ascii_case("path") => {
                    // A path that is not absolute is not a path. The cookie keeps
                    // the one its request implies instead, which is why this
                    // leaves the field alone rather than clearing it.
                    if value.starts_with('/') {
                        cookie.path = Some(value.to_owned());
                    }
                }
                name if name.eq_ignore_ascii_case("secure") => cookie.secure = true,
                name if name.eq_ignore_ascii_case("httponly") => cookie.http_only = true,
                name if name.eq_ignore_ascii_case("samesite") => {
                    cookie.same_site = match value {
                        value if value.eq_ignore_ascii_case("strict") => SameSite::Strict,
                        value if value.eq_ignore_ascii_case("lax") => SameSite::Lax,
                        value if value.eq_ignore_ascii_case("none") => SameSite::None,
                        // A value nobody here knows is the default, not an error:
                        // a browser that dropped the cookie over a misspelling
                        // would sign the reader out over one.
                        _ => SameSite::default(),
                    };
                }
                // An attribute nothing reads is carried by the response and not
                // by us. Ignoring it is what the specification asks for and what
                // keeps a new one from breaking an old browser.
                _ => {}
            }
        }

        Ok(cookie)
    }
}

/// `Max-Age`, which is a count of seconds and may be negative.
///
/// Strict on purpose, unlike `Expires`: the grammar is a sign and digits, there is
/// nothing in the wild that stretches it, and a lenient reader here would turn
/// `Max-Age=abc` into an expiry rather than into no attribute.
fn read_max_age(value: &str) -> Option<i64> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    // A count so large it does not fit is a cookie that outlives everything,
    // which is what saturating at the end of the range says.
    Some(value.parse().unwrap_or(if value.starts_with('-') {
        i64::MIN
    } else {
        i64::MAX
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> SetCookie {
        SetCookie::parse(line).expect("should read")
    }

    #[test]
    fn a_bare_pair_is_a_session_cookie_with_no_attributes() {
        let cookie = parse("sid=abc123");
        assert_eq!(cookie.name, "sid");
        assert_eq!(cookie.value, "abc123");
        assert_eq!(cookie.domain, None);
        assert_eq!(cookie.path, None);
        assert_eq!(cookie.max_age, None);
        assert_eq!(cookie.expires, None);
        assert!(!cookie.secure);
        assert!(!cookie.http_only);
        assert_eq!(cookie.same_site, SameSite::Lax);
    }

    #[test]
    fn every_attribute_is_read() {
        let cookie = parse(
            "sid=abc; Domain=.Example.COM; Path=/app; Max-Age=600; \
             Expires=Sun, 06 Nov 1994 08:49:37 GMT; Secure; HttpOnly; SameSite=Strict",
        );
        assert_eq!(cookie.name, "sid");
        assert_eq!(cookie.value, "abc");
        assert_eq!(cookie.domain.as_deref(), Some("example.com"));
        assert_eq!(cookie.path.as_deref(), Some("/app"));
        assert_eq!(cookie.max_age, Some(600));
        assert!(cookie.expires.is_some());
        assert!(cookie.secure);
        assert!(cookie.http_only);
        assert_eq!(cookie.same_site, SameSite::Strict);
    }

    /// Attribute names are case-insensitive and the flags have no value.
    #[test]
    fn attribute_names_are_read_in_any_case() {
        let cookie = parse("a=b; SECURE; httponly; PATH=/x; samesite=NONE");
        assert!(cookie.secure);
        assert!(cookie.http_only);
        assert_eq!(cookie.path.as_deref(), Some("/x"));
        assert_eq!(cookie.same_site, SameSite::None);
    }

    /// Spaces and tabs around a name, a value or an attribute are punctuation.
    /// Anything inside them is not.
    #[test]
    fn only_spaces_and_tabs_are_trimmed() {
        let cookie = parse("  sid \t = \t abc  ;   Path = /app  ");
        assert_eq!(cookie.name, "sid");
        assert_eq!(cookie.value, "abc");
        assert_eq!(cookie.path.as_deref(), Some("/app"));

        // A space *inside* a value is part of it.
        assert_eq!(parse("a=one two").value, "one two");
        // And so is an `=`: only the first one splits.
        assert_eq!(parse("a=b=c").value, "b=c");
    }

    /// A value is carried as written. Decoding it is between a site and itself,
    /// and a browser that guessed would corrupt the ones that are not encoded.
    #[test]
    fn a_value_is_not_decoded() {
        assert_eq!(parse("a=%20%2F").value, "%20%2F");
        assert_eq!(parse("a=\"quoted\"").value, "\"quoted\"");
    }

    /// A response that can put a newline in a header can put a header in a
    /// header. Refused whole rather than trimmed.
    #[test]
    fn a_control_character_refuses_the_whole_line() {
        assert_eq!(
            SetCookie::parse("a=b\nSet-Cookie: evil=1"),
            Err(Unreadable::ControlCharacter)
        );
        assert_eq!(
            SetCookie::parse("a=b\r\nX-Evil: 1"),
            Err(Unreadable::ControlCharacter)
        );
        assert_eq!(
            SetCookie::parse("a=b\0c"),
            Err(Unreadable::ControlCharacter)
        );
        // A tab is legal whitespace and is not one of them.
        assert_eq!(parse("a=b\tc").value, "b\tc");
    }

    /// A line with no `=` in its first field is an attribute list with nothing to
    /// attach to.
    #[test]
    fn a_line_without_a_pair_is_no_cookie() {
        assert_eq!(SetCookie::parse("sid"), Err(Unreadable::NoNameValuePair));
        assert_eq!(
            SetCookie::parse("Secure; HttpOnly"),
            Err(Unreadable::NoNameValuePair)
        );
    }

    /// A nameless cookie is legal and in use; one that is neither name nor value
    /// is not.
    #[test]
    fn a_cookie_needs_a_name_or_a_value() {
        assert_eq!(SetCookie::parse("="), Err(Unreadable::Nameless));
        assert_eq!(SetCookie::parse("  =  "), Err(Unreadable::Nameless));

        let nameless = parse("=abc");
        assert_eq!(nameless.name, "");
        assert_eq!(nameless.value, "abc");

        let valueless = parse("sid=");
        assert_eq!(valueless.name, "sid");
        assert_eq!(valueless.value, "");
    }

    #[test]
    fn a_pair_over_the_cap_is_refused() {
        let long = "x".repeat(MAX_NAME_AND_VALUE);
        assert_eq!(
            SetCookie::parse(&format!("a={long}")),
            Err(Unreadable::TooLarge)
        );
        // One byte under, with the name counted, is kept.
        let just_fits = "x".repeat(MAX_NAME_AND_VALUE - 1);
        assert_eq!(
            parse(&format!("a={just_fits}")).value.len(),
            MAX_NAME_AND_VALUE - 1
        );
    }

    /// An attribute over the cap is dropped, and the cookie is not.
    #[test]
    fn an_oversized_attribute_is_dropped_alone() {
        let long = "/".to_owned() + &"x".repeat(MAX_ATTRIBUTE_VALUE);
        let cookie = parse(&format!("a=b; Path={long}; Secure"));
        assert_eq!(cookie.path, None, "the path was too long to keep");
        assert!(cookie.secure, "and the rest of the line still counts");
    }

    /// A leading dot on `Domain` has meant nothing since RFC 6265, and the value
    /// is a hostname, so case does not signify.
    #[test]
    fn a_leading_dot_and_the_case_of_a_domain_are_dropped() {
        assert_eq!(
            parse("a=b; Domain=.EXAMPLE.com").domain.as_deref(),
            Some("example.com")
        );
        assert_eq!(
            parse("a=b; Domain=Example.Com").domain.as_deref(),
            Some("example.com")
        );
        // An empty `Domain` is not an attribute.
        assert_eq!(parse("a=b; Domain=").domain, None);
    }

    /// A path that is not absolute is not a path — the cookie falls back to the
    /// one its request implies.
    #[test]
    fn a_relative_path_is_no_path() {
        assert_eq!(parse("a=b; Path=app").path, None);
        assert_eq!(parse("a=b; Path=").path, None);
        assert_eq!(parse("a=b; Path=/").path.as_deref(), Some("/"));
        // The case of a path *does* signify: paths are compared byte for byte.
        assert_eq!(parse("a=b; Path=/App").path.as_deref(), Some("/App"));
    }

    #[test]
    fn max_age_is_digits_and_a_sign_and_nothing_else() {
        assert_eq!(read_max_age("0"), Some(0));
        assert_eq!(read_max_age("600"), Some(600));
        assert_eq!(read_max_age("-1"), Some(-1));
        assert_eq!(read_max_age(""), None);
        assert_eq!(read_max_age("-"), None);
        assert_eq!(read_max_age("6e2"), None);
        assert_eq!(read_max_age("600s"), None);
        assert_eq!(read_max_age(" 600"), None, "the caller trimmed already");
        assert_eq!(read_max_age("+600"), None);
        // Too large to hold is as long as this can say.
        assert_eq!(read_max_age(&"9".repeat(40)), Some(i64::MAX));
    }

    /// Written twice, the last one counts.
    #[test]
    fn a_repeated_attribute_takes_its_last_value() {
        let cookie = parse("a=b; Path=/one; Domain=one.test; Path=/two; Domain=two.test");
        assert_eq!(cookie.path.as_deref(), Some("/two"));
        assert_eq!(cookie.domain.as_deref(), Some("two.test"));
    }

    /// A `SameSite` nobody here knows is the default rather than a dropped
    /// cookie: signing a reader out over a misspelling is the worse failure.
    #[test]
    fn an_unknown_same_site_is_the_default() {
        assert_eq!(parse("a=b; SameSite=nonsense").same_site, SameSite::Lax);
        assert_eq!(parse("a=b; SameSite=").same_site, SameSite::Lax);
        assert_eq!(parse("a=b").same_site, SameSite::Lax);
    }

    /// An attribute nothing reads is ignored, and does not stop the ones that
    /// follow it from being read.
    #[test]
    fn an_unknown_attribute_is_stepped_over() {
        let cookie = parse("a=b; Partitioned; Priority=High; Secure");
        assert!(cookie.secure);
        assert_eq!(cookie.name, "a");
    }

    /// An `Expires` that cannot be read leaves a session cookie. Dropping the
    /// cookie instead is how a browser signs a reader out over a bad clock
    /// string.
    #[test]
    fn an_unreadable_expires_leaves_a_session_cookie() {
        let cookie = parse("a=b; Expires=not a date");
        assert_eq!(cookie.expires, None);
        assert_eq!(cookie.name, "a");
    }
}
