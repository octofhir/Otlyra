//! The jar as a file, and back.
//!
//! Only the cookies that outlive the process are written. A session cookie is one
//! the reader is signed in with *now*, and writing it down would turn closing the
//! browser into something that no longer ends a session — which is the one thing
//! a person expects it to do.
//!
//! # The format
//!
//! One cookie per line, tab-separated, the same shape the bookmarks use and for
//! the same reasons: a list has no `key = value` spelling, and a file a person can
//! read and repair by hand beats one they cannot. The columns are
//!
//! ```text
//! domain  host-only  path  secure  http-only  same-site  expires  created  name  value
//! ```
//!
//! with the two instants as seconds since the epoch. A name or a value may hold a
//! tab — nothing in `Set-Cookie` forbids one — so both are percent-encoded, along
//! with the `%` that makes that reversible. Everything else is a fixed vocabulary
//! or a number.
//!
//! A line that does not parse is skipped with a warning. One corrupt line must not
//! cost a person every session they are signed in to, and there is nothing to
//! recover it from.
//!
//! Nothing here writes a file. Where the file lives is the shell's business, the
//! way it is for the bookmarks and the preferences.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};

use super::jar::{Capacity, Cookie, Jar};
use super::parse::SameSite;

/// What has to be escaped so a line can be split on tabs again.
///
/// The controls, which carry the tab and the newline, and `%` itself — without
/// which the escaping could not be undone.
const ESCAPED: &AsciiSet = &CONTROLS.add(b'%');

/// The columns, in order, as the file's own first line.
const HEADER: &str = "# Otlyra's cookies: domain\thost-only\tpath\tsecure\thttp-only\tsame-site\texpires\tcreated\tname\tvalue\n";

/// The cookies that outlive the process, as the file spells them.
///
/// Expired ones are left out rather than written and dropped on the way back in:
/// a file is also what a person looks at to see what a browser is keeping about
/// them, and listing what it has already stopped sending would be a lie in the
/// direction that matters.
pub fn to_text(jar: &Jar, now: SystemTime) -> String {
    let mut text = String::from(HEADER);
    for cookie in jar.all() {
        if !cookie.is_persistent() || cookie.is_expired(now) {
            continue;
        }
        let expires = cookie.expires.map(seconds_since_epoch).unwrap_or_default();
        text.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            cookie.domain,
            flag(cookie.host_only),
            cookie.path,
            flag(cookie.secure),
            flag(cookie.http_only),
            same_site_name(cookie.same_site),
            expires,
            seconds_since_epoch(cookie.created),
            utf8_percent_encode(&cookie.name, ESCAPED),
            utf8_percent_encode(&cookie.value, ESCAPED),
        ));
    }
    text
}

/// A jar as the file left it, with `capacity`.
///
/// Never fails. A line that cannot be read is skipped and warned about; a file
/// that is nonsense is an empty jar, because refusing to start over a cookie file
/// would be refusing to start.
pub fn from_text(text: &str, capacity: Capacity, now: SystemTime) -> Jar {
    let mut jar = Jar::with_capacity(capacity);
    for (number, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        match read_line(line) {
            // An expiry that has passed since the file was written is a cookie
            // that is simply gone, and not worth a word.
            Some(cookie) if cookie.is_expired(now) => {}
            Some(cookie) => jar.put(cookie),
            None => tracing::warn!(line = number + 1, "skipping an unreadable cookie"),
        }
    }
    jar
}

fn read_line(line: &str) -> Option<Cookie> {
    // Ten columns, and the last split off last so a value holding a tab — which
    // it may, escaped or not — cannot make the line look like eleven.
    let mut columns = line.splitn(10, '\t');
    let domain = columns.next()?.to_owned();
    let host_only = read_flag(columns.next()?)?;
    let path = columns.next()?.to_owned();
    let secure = read_flag(columns.next()?)?;
    let http_only = read_flag(columns.next()?)?;
    let same_site = read_same_site(columns.next()?)?;
    let expires = columns.next()?.parse::<i64>().ok().map(instant)?;
    let created = instant(columns.next()?.parse::<i64>().ok()?);
    let name = unescape(columns.next()?)?;
    let value = unescape(columns.next()?)?;

    if domain.is_empty() || !path.starts_with('/') || (name.is_empty() && value.is_empty()) {
        return None;
    }

    Some(Cookie {
        name,
        value,
        domain,
        path,
        expires: Some(expires),
        secure,
        http_only,
        same_site,
        host_only,
        created,
        // Nothing has been sent yet this run, so what was read is what was last
        // used. Otherwise every cookie from the file would look equally stale and
        // eviction would have nothing to order them by.
        last_access: created,
    })
}

fn flag(set: bool) -> &'static str {
    if set { "yes" } else { "no" }
}

fn read_flag(written: &str) -> Option<bool> {
    match written {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn same_site_name(same_site: SameSite) -> &'static str {
    match same_site {
        SameSite::Strict => "strict",
        SameSite::Lax => "lax",
        SameSite::None => "none",
    }
}

fn read_same_site(written: &str) -> Option<SameSite> {
    match written {
        "strict" => Some(SameSite::Strict),
        "lax" => Some(SameSite::Lax),
        "none" => Some(SameSite::None),
        _ => None,
    }
}

fn unescape(written: &str) -> Option<String> {
    percent_decode_str(written)
        .decode_utf8()
        .ok()
        .map(Into::into)
}

/// Seconds since the epoch, negative before it.
fn seconds_since_epoch(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => since.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    }
}

/// The instant `seconds` since the epoch names.
fn instant(seconds: i64) -> SystemTime {
    if seconds >= 0 {
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn url(address: &str) -> Url {
        Url::parse(address).expect("a url")
    }

    fn filled() -> Jar {
        let mut jar = Jar::new();
        for line in [
            "kept=1; Max-Age=600; Path=/app; HttpOnly; SameSite=Strict",
            "wide=2; Max-Age=600; Domain=example.com",
            "secure=3; Max-Age=600; Secure; SameSite=None",
            "session=4",
        ] {
            jar.set(&url("https://www.example.com/"), line, now())
                .expect("kept");
        }
        jar
    }

    /// Everything that outlives the process comes back exactly as it went, down
    /// to the flags an inspector shows and the instants eviction orders by.
    #[test]
    fn a_kept_cookie_survives_the_round_trip() {
        let jar = filled();
        let read = from_text(&to_text(&jar, now()), Capacity::default(), now());

        let mut before: Vec<&Cookie> = jar.all().iter().filter(|c| c.is_persistent()).collect();
        let mut after: Vec<&Cookie> = read.all().iter().collect();
        before.sort_by(|a, b| a.name.cmp(&b.name));
        after.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(after.len(), 3, "the session cookie is not one of them");

        for (before, after) in before.iter().zip(&after) {
            assert_eq!(before.name, after.name);
            assert_eq!(before.value, after.value);
            assert_eq!(before.domain, after.domain);
            assert_eq!(before.path, after.path);
            assert_eq!(before.expires, after.expires);
            assert_eq!(before.secure, after.secure);
            assert_eq!(before.http_only, after.http_only);
            assert_eq!(before.same_site, after.same_site);
            assert_eq!(before.host_only, after.host_only);
            assert_eq!(before.created, after.created);
        }
    }

    /// And it still goes out on the requests it belongs on, which is the only
    /// reason to have written it down.
    #[test]
    fn a_cookie_read_back_is_still_sent() {
        let read = from_text(&to_text(&filled(), now()), Capacity::default(), now());
        let mut read = read;
        assert_eq!(
            read.header_for(
                &url("https://www.example.com/app/x"),
                super::super::Context::SameSite,
                now()
            ),
            Some("kept=1; wide=2; secure=3".into())
        );
    }

    /// A session cookie is one the reader is signed in with now. Writing it down
    /// would stop closing the browser from ending a session.
    #[test]
    fn a_session_cookie_is_never_written() {
        let text = to_text(&filled(), now());
        assert!(!text.contains("session"), "{text}");
    }

    /// A cookie that has expired since the file was written is gone, not read
    /// back and dropped a moment later.
    #[test]
    fn an_expiry_that_passed_is_not_read_back() {
        let text = to_text(&filled(), now());
        let later = now() + Duration::from_secs(601);
        assert!(from_text(&text, Capacity::default(), later).is_empty());
        // And a jar written after the expiry does not list it either.
        assert_eq!(to_text(&filled(), later), HEADER);
    }

    /// A tab in a name or a value cannot be allowed to make one column into two.
    #[test]
    fn a_tab_or_a_newline_in_a_value_survives() {
        let mut jar = Jar::new();
        jar.set(
            &url("https://x.test/"),
            "od\td=one\ttwo%three; Max-Age=600",
            now(),
        )
        .expect("kept");
        assert_eq!(
            to_text(&jar, now()).lines().count(),
            2,
            "one header, one cookie"
        );

        let read = from_text(&to_text(&jar, now()), Capacity::default(), now());
        assert_eq!(read.len(), 1);
        assert_eq!(read.all()[0].name, "od\td");
        assert_eq!(read.all()[0].value, "one\ttwo%three");
    }

    /// One corrupt line must not cost every session a person is signed in to.
    #[test]
    fn an_unreadable_line_costs_only_itself() {
        let good = to_text(&filled(), now());
        let mut damaged = good.clone();
        damaged.push_str("this is not a cookie\n");
        damaged.push_str("x.test\tmaybe\t/\tno\tno\tlax\t9999999999\t0\ta\tb\n");
        damaged.push_str("\t no \t/\tno\tno\tlax\t9999999999\t0\ta\tb\n");
        damaged.push_str("x.test\tno\trelative\tno\tno\tlax\t9999999999\t0\ta\tb\n");

        let read = from_text(&damaged, Capacity::default(), now());
        assert_eq!(
            read.len(),
            3,
            "the three good ones, and none of the four bad"
        );
    }

    /// An empty file, a file of comments, and nonsense are all an empty jar
    /// rather than a browser that will not start.
    #[test]
    fn nothing_readable_is_an_empty_jar() {
        for text in ["", "\n\n", HEADER, "garbage\n"] {
            assert!(
                from_text(text, Capacity::default(), now()).is_empty(),
                "{text:?}"
            );
        }
    }
}
