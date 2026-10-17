//! Module sources: asked for on a page's behalf, and handed back to it.
//!
//! A page's script world says which module addresses it needs — the entries
//! of its module scripts, what they import, and the targets of its
//! `import()` calls — and this is where they are fetched. Each goes through
//! the same [`crate::fetcher::Fetcher`] as every other subresource, with its
//! concurrency and the page's scheme policy, as [`ResourceKind::Module`].
//!
//! # Contents
//! - [`PageWorld::request_modules`] — take what a tab's runner needs and ask.
//! - [`PageWorld::fetch_module_imports`] — the same for every tab, each pump.
//! - [`PageWorld::module_arrived`] — one module fetched after the load.
//! - [`module_response`] — a response checked the way a module must be.
//!
//! # Invariants
//! - While a load is under way its module fetches are part of what it waits
//!   for (`PendingLoad::outstanding`), so the window's `load` waits for the
//!   module graphs; after it, they are kept in `PageWorld::module_fetches`.
//! - An address is asked for once per document: the runner hands each out
//!   once, and one the preload scan already asked for is not asked again.
//! - A module is delivered only to the document that asked for it.
//! - A response is a module only with a success status and a JavaScript MIME
//!   type; anything else is delivered as a failure, which the page sees as
//!   `error` on the script element or a rejected `import()`.
//! - No more than [`MODULE_LIMIT`] modules are fetched for one document.
//!
//! # See also
//! - `otlyra_script::page` — what the runner does with a delivery.

use otlyra_dom::Document;
use otlyra_html::{ModuleResponse, ScriptRunner};

use super::load::PendingResource;
use super::{PageSlot, PageWorld, after_script_turn};
use crate::browser::TabId;
use crate::fetcher::{Fetched, Loaded, ResourceKind};

/// How many module sources one document may fetch. A graph is often a few
/// dozen files and a large application a few hundred; past this, a page is
/// generating imports rather than shipping them.
pub(super) const MODULE_LIMIT: usize = 1024;

/// A module fetched for a document after its load.
pub(super) struct ModuleFetch {
    tab: TabId,
    /// The slot generation of the document that asked.
    generation: u64,
    /// The address as the page's module map knows it.
    url: String,
}

impl ModuleFetch {
    /// Carry a request `tab`'s document made while it loaded over to the
    /// generation the finished load gives the same document.
    pub(super) fn rebase(&mut self, tab: TabId, from: u64, to: u64) {
        if self.tab == tab && self.generation == from {
            self.generation = to;
        }
    }

    /// Whether this was asked for by `tab`'s page.
    pub(super) fn of(&self, tab: TabId) -> bool {
        self.tab == tab
    }
}

impl PageWorld {
    /// Lend `tab`'s script runner and document to `run`: the parser's while
    /// the parse is stopped, the load's after the parse, the page's after the
    /// load.
    pub(super) fn with_page_runner<R>(
        &mut self,
        tab: TabId,
        run: impl FnOnce(&mut dyn ScriptRunner, &mut Document) -> R,
    ) -> Option<R> {
        let PageSlot {
            page,
            scripts,
            pending,
            ..
        } = self.slots.get_mut(&tab)?;
        let page = page.as_mut()?;
        let loading = pending.is_some();
        let runner: &mut dyn ScriptRunner = match pending.as_mut() {
            Some(pending) => match pending.parse.as_mut() {
                Some(parser) => parser.runner_mut()?,
                None => pending.runner.as_deref_mut()?,
            },
            None => scripts.as_deref_mut()?,
        };
        let mut answer = None;
        page.with_document(|document| answer = Some(run(&mut *runner, document)));
        if loading {
            if runner.take_mutated() {
                page.document_changed();
            }
        } else {
            after_script_turn(page, runner);
        }
        answer
    }

    /// Ask for the module sources `tab`'s page needs and has not asked for.
    pub(super) fn request_modules(&mut self, tab: TabId) {
        let fetches = self
            .with_page_runner(tab, |runner, _| runner.take_module_fetches())
            .unwrap_or_default();
        if fetches.is_empty() {
            return;
        }
        let Some(slot) = self.slots.get(&tab) else {
            return;
        };
        let generation = slot.generation;
        let Some(page) = slot.page.as_ref() else {
            return;
        };
        let document = page.url().to_string();
        let base = page.base_url().clone();
        let mut refused = Vec::new();
        for url in fetches {
            let Some(slot) = self.slots.get_mut(&tab) else {
                return;
            };
            if let Some(pending) = slot.pending.as_ref()
                && pending.modules_asked.contains(&url)
            {
                // The preload scan's request is already on its way.
                continue;
            }
            if slot.modules_fetched >= MODULE_LIMIT {
                tracing::warn!(target: "page.script", %url, "a module past the limit is not fetched");
                refused.push((url, Err("too many modules".to_owned())));
                continue;
            }
            let Some(address) = Self::subresource_url(&document, &base, &url) else {
                refused.push((url, Err("the page may not reach this address".to_owned())));
                continue;
            };
            slot.modules_fetched += 1;
            let id = self
                .fetcher
                .request_for(tab.0, &address, ResourceKind::Module);
            let slot = self.slots.get_mut(&tab).expect("looked up above");
            match slot.pending.as_mut() {
                Some(pending) => {
                    pending.modules_asked.insert(url.clone());
                    pending
                        .outstanding
                        .entry(id)
                        .or_default()
                        .push(PendingResource::Module(url));
                }
                None => {
                    self.module_fetches.insert(
                        id,
                        ModuleFetch {
                            tab,
                            generation,
                            url,
                        },
                    );
                }
            }
        }
        if !refused.is_empty() {
            self.deliver_modules(tab, refused);
        }
    }

    /// Hand arrived modules to `tab`'s page, then ask for what they import.
    pub(super) fn deliver_modules(
        &mut self,
        tab: TabId,
        arrived: Vec<(String, Result<ModuleResponse, String>)>,
    ) {
        if arrived.is_empty() {
            return;
        }
        self.with_page_runner(tab, |runner, document| {
            for (url, response) in arrived {
                runner.deliver_module(document, &url, response);
            }
        });
        self.request_modules(tab);
    }

    /// The `import()` calls and module scripts of every tab that asked for
    /// something since the last pump.
    pub(super) fn fetch_module_imports(&mut self) {
        let tabs: Vec<TabId> = self.slots.keys().copied().collect();
        for tab in tabs {
            self.request_modules(tab);
        }
    }

    /// A module fetched after its document's load came back.
    pub(super) fn module_arrived(&mut self, fetch: ModuleFetch, fetched: Fetched) -> bool {
        let current = self
            .slots
            .get(&fetch.tab)
            .is_some_and(|slot| slot.generation == fetch.generation && slot.pending.is_none());
        if !current {
            return false;
        }
        let response = module_response(fetched.result.as_ref().map_err(Clone::clone));
        self.deliver_modules(fetch.tab, vec![(fetch.url, response)]);
        true
    }
}

/// A fetched response as a module: its source, or why it is not one.
pub(super) fn module_response(result: Result<&Loaded, String>) -> Result<ModuleResponse, String> {
    let loaded = result?;
    if let Some(status) = loaded.status
        && !(200..300).contains(&status)
    {
        return Err(format!("HTTP {status}"));
    }
    let content_type = loaded.content_type.clone();
    let final_url = loaded.final_url.clone();
    if !content_type
        .as_deref()
        .is_some_and(otlyra_html::is_javascript_mime)
    {
        return Err(format!(
            "a module must be served as JavaScript, not {}",
            content_type.as_deref().unwrap_or("an unknown type")
        ));
    }
    // A module is UTF-8, whatever its response claims.
    let text = String::from_utf8_lossy(&loaded.bytes).into_owned();
    let text = text
        .strip_prefix('\u{feff}')
        .map(str::to_owned)
        .unwrap_or(text);
    Ok(ModuleResponse {
        final_url,
        content_type,
        text,
    })
}
