//! The page's own requests: what its script asked for with `fetch` or
//! `XMLHttpRequest`, fetched here and answered to it.
//!
//! The isolate never touches the network. A request it makes waits in the
//! runner until the pump takes it, goes through the same
//! [`crate::fetcher::Fetcher`] as everything else the page loads — its lanes,
//! the page's scheme policy, the browser's cookies and cache — as
//! [`ResourceKind::Fetch`], and comes back to the runner, which settles the
//! promise or fires the events that were waiting on it.
//!
//! # Contents
//! - [`PageWorld::fetch_page_requests`] — take and send what every tab asked.
//! - [`PageWorld::page_response_arrived`] — one answer, back to its page.
//! - [`readable`] — whether a response is one the page may read.
//!
//! # Invariants
//! - An answer is delivered only to the document that asked: a tab that has
//!   moved on, or closed, gets nothing.
//! - A cross-origin response reaches script only when it says the page may
//!   read it (`Access-Control-Allow-Origin`); anything else is a network
//!   error, as CORS makes it (Fetch §3.2).
//! - Only `GET`, `HEAD` and `POST` are sent; the transport has no others yet,
//!   and any other method is refused rather than sent as one of them.

use otlyra_html::{PageRequest, PageResponse};

use super::PageWorld;
use crate::browser::TabId;
use crate::fetcher::{Fetched, ResourceKind};

/// A request the page made, in flight.
pub(super) struct PageFetch {
    tab: TabId,
    /// The slot generation of the document that asked.
    generation: u64,
    /// The page's own number for it.
    id: u64,
    /// The origin of the document that asked, which a cross-origin answer
    /// has to name to be read.
    origin: url::Origin,
    /// Whether a `<script>` element asked: its answer is not the page's to
    /// read, only to run, so no CORS check applies.
    script: bool,
}

impl PageFetch {
    /// Whether this was asked for by `tab`'s page.
    pub(super) fn of(&self, tab: TabId) -> bool {
        self.tab == tab
    }
}

impl PageWorld {
    /// Send what each tab's script asked for since the last pump.
    pub(super) fn fetch_page_requests(&mut self) {
        let tabs: Vec<TabId> = self.slots.keys().copied().collect();
        for tab in tabs {
            let requests = self
                .with_page_runner(tab, |runner, _| runner.take_page_requests())
                .unwrap_or_default();
            for request in requests {
                self.send_page_request(tab, request);
            }
        }
    }

    fn send_page_request(&mut self, tab: TabId, request: PageRequest) {
        let Some(slot) = self.slots.get(&tab) else {
            return;
        };
        let generation = slot.generation;
        let Some(page) = slot.page.as_ref() else {
            return;
        };
        let document = page.url().to_string();
        let origin = page.url().origin();
        let refuse = |world: &mut Self, why: &str| {
            world.with_page_runner(tab, |runner, document| {
                runner.deliver_page_response(document, request.id, Err(why.to_owned()));
            });
        };
        let Some(address) = url::Url::parse(&request.url)
            .ok()
            .filter(|url| Self::may_reach(&document, url))
        else {
            refuse(self, "the page may not reach this address");
            return;
        };
        let body = match request.method.as_str() {
            "GET" | "HEAD" => None,
            "POST" => {
                let content_type = request
                    .headers
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                    .map_or_else(
                        || "text/plain;charset=UTF-8".to_owned(),
                        |(_, value)| value.clone(),
                    );
                Some(crate::fetcher::Body {
                    content_type,
                    bytes: request.body.clone().unwrap_or_default(),
                })
            }
            other => {
                tracing::warn!(target: "page.script", method = other, url = %address, "a request method the transport cannot send");
                refuse(self, "this request method is not supported");
                return;
            }
        };
        // What the transport writes itself is not the page's to set.
        let headers = request
            .headers
            .into_iter()
            .filter(|(name, _)| {
                !matches!(
                    name.to_ascii_lowercase().as_str(),
                    "content-type" | "content-length" | "cookie" | "host" | "origin" | "referer"
                )
            })
            .collect();
        let fetch = self.fetcher.fetch_with_headers(
            tab.0,
            address.as_str(),
            if request.script {
                ResourceKind::Script
            } else {
                ResourceKind::Fetch
            },
            body,
            otlyra_net::CacheMode::Default,
            headers,
        );
        self.page_fetches.insert(
            fetch,
            PageFetch {
                tab,
                generation,
                id: request.id,
                origin,
                script: request.script,
            },
        );
    }

    /// A request a page made came back. Answers whether anything changed.
    pub(super) fn page_response_arrived(&mut self, fetch: PageFetch, fetched: Fetched) -> bool {
        let current = self
            .slots
            .get(&fetch.tab)
            .is_some_and(|slot| slot.generation == fetch.generation);
        if !current {
            return false;
        }
        let response = fetched.result.and_then(|loaded| {
            let response = PageResponse {
                status: loaded.status.unwrap_or(200),
                url: loaded.final_url,
                headers: loaded
                    .response_headers
                    .into_iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value))
                    .collect(),
                body: loaded.bytes,
            };
            if !fetch.script {
                readable(&fetch.origin, &response)?;
            }
            Ok(response)
        });
        if let Err(error) = &response {
            tracing::debug!(target: "page.script", %error, "a page's request failed");
        }
        self.with_page_runner(fetch.tab, |runner, document| {
            runner.deliver_page_response(document, fetch.id, response);
        })
        .is_some()
    }
}

/// Whether a page at `origin` may read `response` (Fetch §4.9, the CORS
/// check, as far as a request with no preflight goes): its own origin's
/// answers always, another's only when it names this origin or `*`.
pub(super) fn readable(origin: &url::Origin, response: &PageResponse) -> Result<(), String> {
    let Ok(from) = url::Url::parse(&response.url) else {
        return Err("a response from no address".to_owned());
    };
    if from.origin() == *origin || from.scheme() == "data" {
        return Ok(());
    }
    let allowed = response
        .headers
        .iter()
        .find(|(name, _)| name == "access-control-allow-origin")
        .map(|(_, value)| value.trim());
    match allowed {
        Some("*") => Ok(()),
        Some(value) if value == origin.ascii_serialization() => Ok(()),
        _ => Err(format!(
            "{} may not be read from {}: it does not allow this origin",
            response.url,
            origin.ascii_serialization()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(url: &str, headers: &[(&str, &str)]) -> PageResponse {
        PageResponse {
            status: 200,
            url: url.to_owned(),
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
            body: Vec::new(),
        }
    }

    #[test]
    fn a_page_reads_its_own_origin_and_what_another_allows_it() {
        let origin = url::Url::parse("https://site.test/page").unwrap().origin();
        assert!(readable(&origin, &response("https://site.test/api", &[])).is_ok());
        assert!(readable(&origin, &response("https://other.test/api", &[])).is_err());
        assert!(
            readable(
                &origin,
                &response(
                    "https://other.test/api",
                    &[("access-control-allow-origin", "*")]
                )
            )
            .is_ok()
        );
        assert!(
            readable(
                &origin,
                &response(
                    "https://other.test/api",
                    &[("access-control-allow-origin", "https://site.test")]
                )
            )
            .is_ok()
        );
        assert!(
            readable(
                &origin,
                &response(
                    "https://other.test/api",
                    &[("access-control-allow-origin", "https://evil.test")]
                )
            )
            .is_err()
        );
    }
}
