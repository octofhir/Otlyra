//! The names nobody may set a cookie for.
//!
//! A cookie's `Domain` attribute lets a host widen a cookie to its parent, which
//! is how `login.example.com` signs `www.example.com` in. Left unbounded that is
//! also how any site under `.co.uk` sets a cookie every other site under `.co.uk`
//! sends back — the supercookie. What stops it is knowing where the registry ends
//! and a registrant begins, and that is not derivable: `com` is a registry and so
//! is `co.uk`, while `co.com` is somebody's domain. It is a list, and there is one
//! list.
//!
//! The list is [vendored][crate::cookie::suffix] rather than taken from a crate.
//! The reasoning is in `BROWSER_RESEARCH_PLAN.md`; the short of it is that the
//! list changes every week and a crate hides its vintage inside a version number,
//! while a file in the tree carries the date it was pulled and refreshes without
//! anybody's release.

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::OnceLock;

/// The list as published, byte for byte.
///
/// Pulled from <https://publicsuffix.org/list/public_suffix_list.dat> and from
/// nowhere else — the file says so itself, and mirrors are not guaranteed to be
/// the same list. Refresh it by fetching that address again; [`version`] is what
/// says how old the copy in hand is.
const LIST: &str = include_str!("../../data/public_suffix_list.dat");

/// The three kinds of rule, each in its own table.
///
/// Split by kind rather than tagged in one table because the question asked of
/// each is different: two are asked about the candidate itself, and the wildcard
/// is asked about the candidate's *parent*.
#[derive(Default)]
struct Rules {
    /// `com`, `co.uk`: this name is itself a public suffix.
    normal: HashSet<Cow<'static, str>>,
    /// The part after `*.`, so `ck` for the rule `*.ck`: every direct child of
    /// this name is a public suffix.
    wildcard: HashSet<Cow<'static, str>>,
    /// The part after `!`, so `www.ck` for `!www.ck`: this name is registrable
    /// despite a wildcard above it.
    exception: HashSet<Cow<'static, str>>,
}

/// The parsed list, built once.
///
/// Lazily, and deliberately: sixteen thousand lines is a few milliseconds of
/// hashing, and the startup path must not spend them. Nothing asks this anything
/// until a response carries a `Set-Cookie` or a request is about to be sent, both
/// of which are long after the first frame.
fn rules() -> &'static Rules {
    static RULES: OnceLock<Rules> = OnceLock::new();
    RULES.get_or_init(|| {
        let mut rules = Rules::default();
        // A rule may be written in the script it is read in — `公司.cn`, `рф` —
        // and a host reaches us in punycode, so the rule is converted rather than
        // the host: it happens once here instead of on every lookup.
        let ascii = |name: &'static str| -> Option<Cow<'static, str>> {
            if name.is_ascii() {
                Some(Cow::Borrowed(name))
            } else {
                idna::domain_to_ascii(name).ok().map(Cow::Owned)
            }
        };
        for line in LIST.lines() {
            let line = line.trim();
            // The file's own comments are `//`, and a blank line separates
            // registries. Neither is a rule.
            if line.is_empty() || line.starts_with("//") {
                continue;
            }
            let (table, name) = if let Some(rest) = line.strip_prefix('!') {
                (&mut rules.exception, rest)
            } else if let Some(rest) = line.strip_prefix("*.") {
                (&mut rules.wildcard, rest)
            } else {
                (&mut rules.normal, line)
            };
            if let Some(name) = ascii(name) {
                table.insert(name);
            }
        }
        rules
    })
}

/// What the vendored copy of the list says about itself.
///
/// The `VERSION:` line, which is the date it was published. A stale list is a
/// quiet failure — a registry that appeared last month is not one we refuse a
/// cookie for — so the vintage is readable rather than implied.
pub fn version() -> &'static str {
    LIST.lines()
        .take_while(|line| line.starts_with("//") || line.trim().is_empty())
        .find_map(|line| line.trim_start_matches('/').trim().strip_prefix("VERSION:"))
        .map(str::trim)
        .unwrap_or("unknown")
}

/// The public suffix of `host`: the tail of it that belongs to a registry.
///
/// `host` must already be canonical — lowercased, and in punycode if it is an
/// IDN, which is what [`url::Url::host_str`] gives.
///
/// The algorithm is the list's own. Every candidate is a suffix of `host`
/// starting at a label boundary, so the scan walks byte offsets into the string
/// it was handed and allocates nothing. Candidates are tried longest first,
/// because the rule with the most labels is the one that prevails; at each one an
/// exception is asked before the rules it excepts, because an exception outranks
/// everything.
///
/// A host matching no rule at all falls to the implied `*`, under which the last
/// label is the suffix — which is why `example.invalid` is registrable and
/// `invalid` is not.
pub fn public_suffix(host: &str) -> &str {
    let rules = rules();
    let mut candidate = host;
    loop {
        let parent = candidate.split_once('.').map(|(_, parent)| parent);

        // `!www.ck` says `www.ck` is registrable, so the suffix is what is left
        // after its own leftmost label — that is the whole point of writing it.
        if rules.exception.contains(candidate) {
            return parent.unwrap_or("");
        }
        if rules.normal.contains(candidate) {
            return candidate;
        }
        // `*.ck` says every direct child of `ck` is a public suffix, so what has
        // to be in the table is the candidate's parent rather than the candidate.
        if let Some(parent) = parent
            && rules.wildcard.contains(parent)
        {
            return candidate;
        }

        match parent {
            Some(parent) => candidate = parent,
            None => break,
        }
    }
    host.rsplit_once('.').map_or(host, |(_, last)| last)
}

/// Whether `host` is a name a registry hands out rather than one a registrant
/// holds — and therefore one no cookie may name.
pub fn is_public_suffix(host: &str) -> bool {
    public_suffix(host) == host
}

/// The registrable domain of `host`: its public suffix plus the one label a
/// registrant was given.
///
/// `None` when there is no such label — `host` is the suffix itself, or is
/// shorter than one. This is what "the site" means when two addresses are asked
/// whether they belong to the same one.
pub fn registrable_domain(host: &str) -> Option<&str> {
    let suffix = public_suffix(host);
    if host.len() <= suffix.len() {
        return None;
    }
    // The label immediately left of the suffix, and everything right of it.
    let head = &host[..host.len() - suffix.len() - 1];
    let start = head.rfind('.').map_or(0, |dot| dot + 1);
    Some(&host[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list's own test cases, from
    /// <https://github.com/publicsuffix/list/blob/master/tests/test_psl.txt>,
    /// stated as the registrable domain because that is what the file asserts.
    ///
    /// Only the rows that are about *rules* are here; the file's leading rows
    /// about `NULL` inputs and mixed case are about a caller's canonicalization,
    /// which happens before this function is reached.
    #[test]
    fn the_lists_own_cases() {
        let cases: &[(&str, Option<&str>)] = &[
            // A domain under an unlisted TLD falls to the implied `*`.
            ("example", None),
            ("example.example", Some("example.example")),
            ("b.example.example", Some("example.example")),
            ("a.b.example.example", Some("example.example")),
            // A listed TLD with no children of its own.
            ("biz", None),
            ("domain.biz", Some("domain.biz")),
            ("b.domain.biz", Some("domain.biz")),
            // A TLD with children.
            ("com", None),
            ("example.com", Some("example.com")),
            ("b.example.com", Some("example.com")),
            ("uk.com", None),
            ("example.uk.com", Some("example.uk.com")),
            ("b.example.uk.com", Some("example.uk.com")),
            ("test.ac", Some("test.ac")),
            // A TLD with only one rule, and that rule a wildcard.
            ("mm", None),
            ("c.mm", None),
            ("b.c.mm", Some("b.c.mm")),
            ("a.b.c.mm", Some("b.c.mm")),
            // The more-labels rule.
            ("jp", None),
            ("test.jp", Some("test.jp")),
            ("www.test.jp", Some("test.jp")),
            ("ac.jp", None),
            ("test.ac.jp", Some("test.ac.jp")),
            ("www.test.ac.jp", Some("test.ac.jp")),
            ("kyoto.jp", None),
            ("test.kyoto.jp", Some("test.kyoto.jp")),
            ("ide.kyoto.jp", None),
            ("b.ide.kyoto.jp", Some("b.ide.kyoto.jp")),
            ("a.b.ide.kyoto.jp", Some("b.ide.kyoto.jp")),
            ("c.kobe.jp", None),
            ("b.c.kobe.jp", Some("b.c.kobe.jp")),
            ("a.b.c.kobe.jp", Some("b.c.kobe.jp")),
            // An exception, and the wildcard it excepts.
            ("city.kobe.jp", Some("city.kobe.jp")),
            ("www.city.kobe.jp", Some("city.kobe.jp")),
            // A rule and a wildcard under the same TLD.
            ("ck", None),
            ("test.ck", None),
            ("b.test.ck", Some("b.test.ck")),
            ("a.b.test.ck", Some("b.test.ck")),
            ("www.ck", Some("www.ck")),
            ("www.www.ck", Some("www.ck")),
            // Us.
            ("us", None),
            ("test.us", Some("test.us")),
            ("www.test.us", Some("test.us")),
            ("ak.us", None),
            ("test.ak.us", Some("test.ak.us")),
            ("k12.ak.us", None),
            ("test.k12.ak.us", Some("test.k12.ak.us")),
            ("www.test.k12.ak.us", Some("test.k12.ak.us")),
        ];
        for (host, expected) in cases {
            assert_eq!(
                registrable_domain(host),
                *expected,
                "registrable domain of {host}"
            );
        }
    }

    /// The list is written in the scripts it names, and a host arrives in
    /// punycode. If the two are not made to meet, every IDN registry is invisible
    /// and a cookie can be set across all of `рф`.
    #[test]
    fn a_rule_in_another_script_is_matched_in_punycode() {
        // `公司.cn`, which the file writes in Chinese.
        assert!(is_public_suffix("xn--55qx5d.cn"));
        assert_eq!(
            registrable_domain("shop.xn--55qx5d.cn"),
            Some("shop.xn--55qx5d.cn")
        );
        // `рф`, likewise.
        assert!(is_public_suffix("xn--p1ai"));
        assert_eq!(
            registrable_domain("www.xn--80aswg.xn--p1ai"),
            Some("xn--80aswg.xn--p1ai")
        );
    }

    /// The rule that a cookie is measured against.
    #[test]
    fn a_registry_is_a_public_suffix_and_a_registrant_is_not() {
        assert!(is_public_suffix("com"));
        assert!(is_public_suffix("co.uk"));
        assert!(
            is_public_suffix("github.io"),
            "a private registry counts too"
        );
        assert!(!is_public_suffix("example.com"));
        assert!(!is_public_suffix("example.co.uk"));
        // And the pair that makes this a list rather than a rule: two names of
        // exactly the same shape, one of which hands out subdomains and one of
        // which is a company's own address. Nothing about `uk.com` and `go.com`
        // says which is which.
        assert!(is_public_suffix("uk.com"));
        assert!(!is_public_suffix("go.com"));
    }

    /// A name with nothing above it is its own suffix, and a machine on the local
    /// network has no registry at all.
    #[test]
    fn a_single_label_is_its_own_suffix() {
        assert!(is_public_suffix("localhost"));
        assert_eq!(registrable_domain("localhost"), None);
        assert_eq!(registrable_domain("dev.localhost"), Some("dev.localhost"));
        assert_eq!(public_suffix(""), "");
    }

    /// The copy in the tree says when it was pulled. A version that stopped
    /// parsing would be a list nobody could tell was stale.
    #[test]
    fn the_vendored_copy_states_its_vintage() {
        let version = version();
        assert_ne!(version, "unknown");
        assert!(
            version.starts_with("20"),
            "the version is a date, got {version:?}"
        );
    }
}
