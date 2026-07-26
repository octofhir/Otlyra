//! What may be kept, for how long, and when it has to be asked about again.
//!
//! All of it is the servers' own answer. A cache that decided for itself how long
//! a page stays good would be a cache that shows a stale price, an old balance or
//! yesterday's article, and there is no heuristic that makes that acceptable — so
//! nothing here guesses where a server has spoken, and where it has not the
//! guesses are the specification's own and are bounded.
//!
//! RFC 9111 is the whole of it. Two things about it are worth stating up front
//! because they decide the shape of everything below:
//!
//! - **This is a private cache**, one reader's own. `private` is therefore
//!   storable here and `s-maxage` is not ours to read; both are the opposite of
//!   what a proxy would do with them.
//! - **Age is not elapsed time.** A response may have sat in somebody else's cache
//!   before it reached us, and `Age` is how long. A cache that starts the clock at
//!   the moment the bytes arrive serves things it should have revalidated.

use std::time::{Duration, SystemTime};

use crate::cookie::date;

/// The longest a heuristic may claim a response is good for.
///
/// A day. The specification sets no ceiling and browsers all set one: the
/// heuristic exists so a server that said nothing still gets some benefit, and
/// without a cap a file with an old `Last-Modified` and no `Cache-Control` would
/// be kept for years by arithmetic nobody chose.
pub const MAX_HEURISTIC: Duration = Duration::from_secs(24 * 60 * 60);

/// What fraction of a resource's age is taken as how long it stays good, when
/// nothing says otherwise. The specification's own suggestion.
const HEURISTIC_FRACTION: f64 = 0.1;

/// What a `Cache-Control` header asked for.
///
/// Only the directives that change what this cache does. One it does not know is
/// ignored, which is what the specification asks for and is what keeps a new
/// directive from making an old cache wrong.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Directives {
    /// Do not keep this at all — not on disk, not in memory.
    pub no_store: bool,
    /// Keep it, but never hand it over without asking the server first.
    ///
    /// Not *do not cache*, which is what the name has always suggested and has
    /// never meant. The one that means that is [`Directives::no_store`].
    pub no_cache: bool,
    /// Once stale, it may not be served at all, however badly the network is
    /// going.
    pub must_revalidate: bool,
    /// It will not change while it is fresh, so a reload need not ask.
    pub immutable: bool,
    /// How long it stays good, in seconds.
    pub max_age: Option<u64>,
    /// How long past that it may still be served while a revalidation is in
    /// flight.
    pub stale_while_revalidate: Option<u64>,
}

impl Directives {
    /// Read every `Cache-Control` header on a message.
    ///
    /// Several are one, comma-separated, and a directive repeated is not an
    /// error: the strictest wins, because two answers to *may I keep this* have
    /// only one safe resolution.
    pub fn parse(values: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        let mut out = Self::default();
        for value in values {
            for directive in value.as_ref().split(',') {
                let (name, argument) = match directive.split_once('=') {
                    Some((name, argument)) => (name.trim(), Some(argument.trim())),
                    None => (directive.trim(), None),
                };
                // A quoted argument is legal — `max-age="60"` — and the quotes are
                // not part of the number.
                let seconds = || argument?.trim_matches('"').trim().parse::<u64>().ok();
                match name {
                    name if name.eq_ignore_ascii_case("no-store") => out.no_store = true,
                    name if name.eq_ignore_ascii_case("no-cache") => out.no_cache = true,
                    name if name.eq_ignore_ascii_case("must-revalidate") => {
                        out.must_revalidate = true;
                    }
                    // A proxy's version of the same instruction. We are not a
                    // proxy, but a server that said it to proxies said something
                    // about the resource, and treating it as `must-revalidate`
                    // here is what browsers do.
                    name if name.eq_ignore_ascii_case("proxy-revalidate") => {
                        out.must_revalidate = true;
                    }
                    name if name.eq_ignore_ascii_case("immutable") => out.immutable = true,
                    name if name.eq_ignore_ascii_case("max-age") => {
                        // The smallest wins where it is said twice, for the same
                        // reason the strictest flag does.
                        let read = seconds();
                        out.max_age = match (out.max_age, read) {
                            (Some(had), Some(now)) => Some(had.min(now)),
                            (had, now) => had.or(now),
                        };
                    }
                    name if name.eq_ignore_ascii_case("stale-while-revalidate") => {
                        out.stale_while_revalidate = seconds();
                    }
                    // `private` is what a *shared* cache must not keep, and this
                    // one is a single reader's own. `s-maxage` is likewise a
                    // shared cache's number and not ours to read. Both are
                    // deliberately no-ops rather than oversights.
                    _ => {}
                }
            }
        }
        out
    }
}

/// The clocks a stored response is judged against.
///
/// Recorded when it arrives, because two of the four cannot be worked out later:
/// how long the request was out for, and what the server thought the time was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Times {
    /// When the request went out.
    pub requested: SystemTime,
    /// When the response came back.
    pub received: SystemTime,
    /// The `Date` header, or when it arrived if the server sent none.
    pub date: SystemTime,
    /// The `Age` header, which is how long it had already spent in caches
    /// upstream.
    pub age: Duration,
}

impl Times {
    /// How old the response is now.
    ///
    /// RFC 9111's own calculation, and the part people leave out is the reason it
    /// exists: `Age` says the response was already old when it arrived, and the
    /// round trip added to that. A cache that measures from the moment the bytes
    /// landed serves things it should have asked about.
    pub fn age_at(&self, now: SystemTime) -> Duration {
        let since = |later: SystemTime, earlier: SystemTime| {
            later.duration_since(earlier).unwrap_or(Duration::ZERO)
        };
        // What the response looks like from its own `Date`, and what it says about
        // itself. A clock that disagrees with the server's makes the first of
        // these nonsense, which is why the larger is taken.
        let apparent = since(self.received, self.date);
        let corrected = self.age + since(self.received, self.requested);
        apparent.max(corrected) + since(now, self.received)
    }
}

/// How long a response stays good, and on whose say-so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifetime {
    /// The server said so, in `Cache-Control: max-age` or in `Expires`.
    Stated(Duration),
    /// Nobody said, and this is the specification's guess from how long ago it
    /// last changed — capped at [`MAX_HEURISTIC`].
    Guessed(Duration),
    /// Nothing to go on: no freshness, and nothing to guess from.
    Unknown,
}

impl Lifetime {
    /// How long it is, whichever kind it is.
    pub fn duration(self) -> Duration {
        match self {
            Self::Stated(duration) | Self::Guessed(duration) => duration,
            Self::Unknown => Duration::ZERO,
        }
    }
}

/// How long a response is good for.
///
/// `max-age` first, then `Expires`, then a guess from `Last-Modified`. That order
/// is the specification's and it matters: a server sending both means the newer
/// one, and `Expires` was the only one there used to be.
pub fn lifetime(
    directives: Directives,
    expires: Option<&str>,
    last_modified: Option<&str>,
    times: Times,
) -> Lifetime {
    if let Some(seconds) = directives.max_age {
        return Lifetime::Stated(Duration::from_secs(seconds));
    }
    if let Some(written) = expires {
        // An `Expires` that cannot be read is one that has passed. The
        // specification says so outright, and it is the safe reading of a header
        // whose most common malformed value is the literal `0`.
        let when = date::parse(written);
        return Lifetime::Stated(match when {
            Some(when) => when.duration_since(times.date).unwrap_or(Duration::ZERO),
            None => Duration::ZERO,
        });
    }
    // The guess, and only where there is something to guess from. A tenth of how
    // long it had already gone unchanged when we asked.
    if let Some(when) = last_modified.and_then(date::parse)
        && let Ok(unchanged_for) = times.date.duration_since(when)
    {
        let guess = unchanged_for.mul_f64(HEURISTIC_FRACTION).min(MAX_HEURISTIC);
        return Lifetime::Guessed(guess);
    }
    Lifetime::Unknown
}

/// What may be done with a stored response right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Use {
    /// Hand it over. Nothing goes to the network.
    Fresh,
    /// Ask the server whether it has changed, and hand it over if it says no.
    Revalidate,
    /// It cannot be used at all; fetch it as if nothing were stored.
    Refetch,
}

/// Whether a stored response may be used, and how.
///
/// `has_validator` is whether there is an `ETag` or a `Last-Modified` to ask
/// with. Without one there is no such thing as revalidating: the only way to find
/// out is to fetch it again, and saying `Revalidate` would be saying a request
/// this cache cannot make.
pub fn use_of(
    directives: Directives,
    lifetime: Lifetime,
    has_validator: bool,
    times: Times,
    now: SystemTime,
) -> Use {
    let ask = if has_validator {
        Use::Revalidate
    } else {
        Use::Refetch
    };
    // `no-cache` is *ask every time*, not *do not keep*. The stored copy is still
    // worth having: a server that answers "not changed" saves the body, which on a
    // picture is the whole of the cost.
    if directives.no_cache {
        return ask;
    }
    if times.age_at(now) < lifetime.duration() {
        return Use::Fresh;
    }
    ask
}

/// Whether a response may be kept at all.
///
/// The question asked once, when it arrives. A `no-store` is refused here and
/// never written anywhere — which is the point of it, and the reason this is not
/// folded into [`use_of`], where it would have been asked of something already on
/// a disk.
pub fn may_store(
    method: &str,
    status: u16,
    directives: Directives,
    lifetime: Lifetime,
    has_validator: bool,
) -> bool {
    // Only what a browser fetches without changing anything. A `POST` may be
    // cached in principle and is not worth the rules it costs: nothing here would
    // reuse one.
    if !method.eq_ignore_ascii_case("GET") {
        return false;
    }
    if directives.no_store {
        return false;
    }
    // Something to go on: a stated lifetime, a validator to ask with, or a status
    // the specification says may be guessed about. A response with none of the
    // three is one this cache could only ever refetch, so keeping it is spending
    // memory to learn nothing.
    match lifetime {
        Lifetime::Stated(_) | Lifetime::Guessed(_) => true,
        Lifetime::Unknown => has_validator || heuristically_cacheable(status),
    }
}

/// The statuses a cache may keep without being told it may.
///
/// The specification's own list. What is not on it — a `500`, a `503` — is a
/// server having a bad moment, and a cache that kept those would go on serving
/// the bad moment after it had passed.
fn heuristically_cacheable(status: u16) -> bool {
    matches!(
        status,
        200 | 203 | 204 | 206 | 300 | 301 | 308 | 404 | 405 | 410 | 414 | 501
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// A response that arrived just now, with no time spent anywhere else.
    fn just_now() -> Times {
        Times {
            requested: now(),
            received: now(),
            date: now(),
            age: Duration::ZERO,
        }
    }

    fn parse(value: &str) -> Directives {
        Directives::parse([value])
    }

    #[test]
    fn the_directives_that_change_what_is_done_are_read() {
        let all = parse("no-store, no-cache, must-revalidate, immutable, max-age=60");
        assert!(all.no_store && all.no_cache && all.must_revalidate && all.immutable);
        assert_eq!(all.max_age, Some(60));

        assert_eq!(parse("max-age=\"120\"").max_age, Some(120), "quoted");
        assert_eq!(parse("MAX-AGE=30").max_age, Some(30), "case");
        assert_eq!(parse("max-age=").max_age, None);
        assert_eq!(parse("max-age=soon").max_age, None);
        assert_eq!(
            parse("stale-while-revalidate=5").stale_while_revalidate,
            Some(5)
        );
    }

    /// A directive nobody here knows is ignored rather than refused, which is what
    /// stops a new one from making an old cache wrong.
    #[test]
    fn an_unknown_directive_changes_nothing() {
        let read = parse("no-store, some-future-thing=7, no-cache");
        assert_eq!(read, parse("no-store, no-cache"));
    }

    /// `private` and `s-maxage` are a shared cache's business. This one is a
    /// single reader's, and reading them would be caching less than it may.
    #[test]
    fn a_shared_caches_directives_are_not_ours() {
        assert_eq!(parse("private"), Directives::default());
        assert_eq!(parse("s-maxage=60").max_age, None);
        // And `private, max-age=60` is sixty seconds here, not nothing.
        assert_eq!(parse("private, max-age=60").max_age, Some(60));
    }

    /// Said twice, the strictest wins: there is only one safe way to resolve two
    /// answers to *may I keep this*.
    #[test]
    fn the_strictest_of_two_answers_wins() {
        let split = Directives::parse(["max-age=600", "max-age=60, no-cache"]);
        assert_eq!(split.max_age, Some(60));
        assert!(split.no_cache);
    }

    /// The part of the age calculation people leave out. A response that spent
    /// half an hour in somebody else's cache is half an hour old on arrival.
    #[test]
    fn age_counts_the_time_spent_in_caches_upstream() {
        let times = Times {
            requested: now() - Duration::from_secs(2),
            received: now(),
            date: now(),
            age: HOUR,
        };
        assert_eq!(times.age_at(now()), HOUR + Duration::from_secs(2));
        // And it goes on ageing where it sits.
        assert_eq!(
            times.age_at(now() + HOUR),
            HOUR + HOUR + Duration::from_secs(2)
        );
    }

    /// A server whose clock disagrees with ours must not be able to make a
    /// response look younger than it is.
    #[test]
    fn a_servers_clock_cannot_make_a_response_young() {
        // The server thinks it is an hour later than it is, so the response looks
        // as though it came from the future.
        let times = Times {
            requested: now(),
            received: now(),
            date: now() + HOUR,
            age: Duration::ZERO,
        };
        assert_eq!(times.age_at(now()), Duration::ZERO, "and not negative");

        // And a `Date` an hour in the past is an hour of age, whatever `Age` says.
        let old = Times {
            date: now() - HOUR,
            ..just_now()
        };
        assert_eq!(old.age_at(now()), HOUR);
    }

    #[test]
    fn max_age_outranks_expires_which_outranks_the_guess() {
        let stated = lifetime(
            parse("max-age=60"),
            Some("Sun, 06 Nov 2094 08:49:37 GMT"),
            Some("Sun, 06 Nov 1994 08:49:37 GMT"),
            just_now(),
        );
        assert_eq!(stated, Lifetime::Stated(Duration::from_secs(60)));

        // No `max-age`: `Expires`, measured from the server's own `Date`.
        let expires = lifetime(
            Directives::default(),
            Some(&format_http_date(now() + HOUR)),
            None,
            just_now(),
        );
        assert_eq!(expires, Lifetime::Stated(HOUR));
    }

    /// An `Expires` that cannot be read is one that has passed. Its most common
    /// malformed value is the literal `0`, and a cache that ignored it instead
    /// would keep exactly what a server was trying not to have kept.
    #[test]
    fn an_unreadable_expires_has_already_passed() {
        for written in ["0", "", "-1", "not a date"] {
            assert_eq!(
                lifetime(Directives::default(), Some(written), None, just_now()),
                Lifetime::Stated(Duration::ZERO),
                "{written:?}"
            );
        }
    }

    /// A tenth of how long it had gone unchanged, and never more than a day.
    #[test]
    fn the_guess_is_a_tenth_of_its_age_and_is_capped() {
        let ten_hours_old = format_http_date(now() - HOUR * 10);
        assert_eq!(
            lifetime(
                Directives::default(),
                None,
                Some(&ten_hours_old),
                just_now()
            ),
            Lifetime::Guessed(HOUR)
        );

        // A file untouched for five years would be kept for six months by the
        // arithmetic alone. A day is as far as a guess goes.
        let ancient = format_http_date(now() - Duration::from_secs(5 * 365 * 24 * 3600));
        assert_eq!(
            lifetime(Directives::default(), None, Some(&ancient), just_now()),
            Lifetime::Guessed(MAX_HEURISTIC)
        );

        // And with nothing to guess from there is no guess.
        assert_eq!(
            lifetime(Directives::default(), None, None, just_now()),
            Lifetime::Unknown
        );
    }

    #[test]
    fn a_fresh_response_is_handed_over_and_a_stale_one_is_asked_about() {
        let fresh = Lifetime::Stated(HOUR);
        assert_eq!(
            use_of(Directives::default(), fresh, true, just_now(), now()),
            Use::Fresh
        );
        assert_eq!(
            use_of(
                Directives::default(),
                fresh,
                true,
                just_now(),
                now() + HOUR + Duration::from_secs(1)
            ),
            Use::Revalidate
        );
    }

    /// Stale with nothing to ask with is not *ask*: it is *fetch it again*.
    /// Saying otherwise would be naming a request this cache cannot make.
    #[test]
    fn stale_without_a_validator_is_a_fresh_fetch() {
        assert_eq!(
            use_of(
                Directives::default(),
                Lifetime::Stated(Duration::ZERO),
                false,
                just_now(),
                now()
            ),
            Use::Refetch
        );
    }

    /// `no-cache` has never meant *do not cache*. It means *ask every time* — and
    /// the stored copy is still worth having, because a server answering "not
    /// changed" saves the body.
    #[test]
    fn no_cache_keeps_it_and_asks_every_time() {
        let directives = parse("no-cache, max-age=3600");
        assert_eq!(
            use_of(directives, Lifetime::Stated(HOUR), true, just_now(), now()),
            Use::Revalidate,
            "fresh by the clock and asked about anyway"
        );
        assert!(may_store(
            "GET",
            200,
            directives,
            Lifetime::Stated(HOUR),
            true
        ));
    }

    /// `no-store` is the one that means it.
    #[test]
    fn no_store_is_never_kept() {
        assert!(!may_store(
            "GET",
            200,
            parse("no-store"),
            Lifetime::Stated(HOUR),
            true
        ));
        assert!(!may_store(
            "GET",
            200,
            parse("no-store, max-age=3600"),
            Lifetime::Stated(HOUR),
            true
        ));
    }

    #[test]
    fn only_what_is_worth_keeping_is_kept() {
        let none = Directives::default();
        // Nothing to go on at all, on a status nobody may guess about.
        assert!(!may_store("GET", 500, none, Lifetime::Unknown, false));
        // The same, with something to ask with.
        assert!(may_store("GET", 500, none, Lifetime::Unknown, true));
        // A status the specification says may be guessed about.
        assert!(may_store("GET", 404, none, Lifetime::Unknown, false));
        assert!(may_store("GET", 301, none, Lifetime::Unknown, false));
        // A server having a bad moment is not something to go on serving.
        assert!(!may_store("GET", 503, none, Lifetime::Unknown, false));
        // And nothing but a `GET`.
        assert!(!may_store("POST", 200, none, Lifetime::Stated(HOUR), true));
    }

    /// An HTTP date, for a test that needs to hand one to a parser.
    fn format_http_date(time: SystemTime) -> String {
        let seconds = time
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs() as i64;
        let days = seconds.div_euclid(86_400);
        let rest = seconds.rem_euclid(86_400);
        // The inverse of the civil-from-days the cookie date parser uses.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if month <= 2 { year + 1 } else { year };
        const MONTHS: [&str; 12] = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ];
        format!(
            "Thu, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
            MONTHS[(month - 1) as usize],
            rest / 3600,
            (rest % 3600) / 60,
            rest % 60
        )
    }

    /// The helper the tests lean on has to be right, or every date case is
    /// meaningless: it is checked against the parser it feeds.
    #[test]
    fn the_test_date_helper_round_trips() {
        for offset in [0, 1, 86_399, 86_400, 1_700_000_000, 2_500_000_000] {
            let time = SystemTime::UNIX_EPOCH + Duration::from_secs(offset);
            let written = format_http_date(time);
            assert_eq!(date::parse(&written), Some(time), "{written}");
        }
    }
}
