//! The browser's own pages, and the stores behind them.
//!
//! Preferences, history, bookmarks, cookies, the cache and downloads are each a
//! surface that reports what the reader did over a store that outlives the tab
//! showing it. What the browser does about those reports — and the native
//! dialogues two of them open — is kept together, apart from the documents a tab
//! loads.

use crate::downloads;
use crate::settings;

use super::{Browser, SURFACE_CHROME};

impl Browser {
    /// Save the preferences if the surface has changed one.
    ///
    /// Compared rather than announced, because every change already goes through
    /// one place — `Settings::apply` — and a second signal saying *and this one
    /// was worth saving* would be a second thing to keep in step with the first.
    pub(super) fn save_preferences_if_changed(&mut self, before: &settings::Settings) {
        if self.settings.settings.persisted_eq(before) {
            return;
        }
        // The appearance is a preference like the rest, so the one place that
        // notices a preference changing is the one place the palette follows it.
        self.apply_theme();
        crate::preferences::save(&self.settings.settings);
    }

    /// Answer what the settings surface reported and could not do itself.
    ///
    /// Two things: leaving the surface, which is a tab's business, and putting a
    /// native dialogue on the screen, which is the platform's. Everything else the
    /// surface has already applied to the preferences by the time this runs.
    pub(super) fn handle_settings_action(&mut self, action: &settings::Action) {
        // Unconditional, and before the match: the surface has already written
        // every change into the preferences, and the jar is the one piece of
        // state a preference reaches that is not read back out of them on use.
        // Doing it here rather than under the one action that needs it is what
        // stops the next such preference from being forgotten.
        self.sync_cookie_policy();
        match action {
            settings::Action::Close => self.close_system_page(),
            settings::Action::ChooseDownloadDirectory => {
                let current = self.settings.settings.download_directory();
                let Some(chosen) = choose_download_directory(current.as_deref()) else {
                    return;
                };
                // Through `apply`, like every other change, and saved here because
                // the caller's own before/after comparison already ran — the
                // dialogue was up while it did.
                self.settings
                    .settings
                    .apply(settings::Action::SetDownloadDirectory(
                        chosen.to_string_lossy().into_owned(),
                    ));
                crate::preferences::save(&self.settings.settings);
            }
            _ => {}
        }
    }

    /// Leave the browser page being shown: back if there is a back, a blank
    /// tab if there is not.
    ///
    /// Leave the settings when the surface says it is done with them.
    ///
    /// *Done* is *back*, now that the settings are a history entry like any
    /// other: it returns to whatever the tab was showing before, at the scroll
    /// position it was left at. With nothing behind it — the settings opened in
    /// a fresh tab — the tab is emptied instead, because there is nowhere to
    /// return to and staying would make the button do nothing.
    fn close_system_page(&mut self) {
        if self.tabs[self.active].can_go_back() {
            self.go_back();
            return;
        }
        let tab = &mut self.tabs[self.active];
        tab.system = None;
        tab.url = String::new();
        tab.title = "New tab".to_owned();
        self.ui.address.clear();
        self.ui.bookmark = crate::ui::Bookmarked::Impossible;
        self.ui.focus_address();
        self.activate_surface(SURFACE_CHROME);
    }

    /// Act on what the history surface reported.
    pub(super) fn handle_history_action(&mut self, action: crate::history::Action) {
        match action {
            crate::history::Action::Open(url) => self.navigate_from(&url, false),
            crate::history::Action::Clear => self.history.clear(),
            crate::history::Action::Close => self.close_system_page(),
            crate::history::Action::None
            | crate::history::Action::Focus(_)
            | crate::history::Action::SearchHit(_) => {}
        }
    }

    /// Keep the page the reader is on, or stop keeping it.
    ///
    /// One command for both, because ⌘D is one key and a reader pressing it twice
    /// means *undo that*. What it did is reported in the log rather than on the
    /// page: the native menu bar is built once at startup and cannot yet be
    /// relabelled, so the browser's own menu — rebuilt every frame — is where the
    /// state is said out loud.
    pub(super) fn toggle_bookmark(&mut self) {
        let tab = &self.tabs[self.active];
        let url = tab.url.clone();
        if url.trim().is_empty() {
            // A blank tab is not a page, and a bookmark that opens nowhere is
            // worse than no bookmark.
            return;
        }
        let title = if tab.title.trim().is_empty() {
            url.clone()
        } else {
            tab.title.clone()
        };
        let kept = self.bookmarks.toggle(url.clone(), title);
        tracing::info!(%url, kept, "bookmark toggled");
        self.ui.bookmark = if kept {
            crate::ui::Bookmarked::Yes
        } else {
            crate::ui::Bookmarked::No
        };
        self.accessibility_dirty = true;
    }

    /// Keep bookmarks between runs, reading what the last one left.
    ///
    /// Called by the shell for a browser a person is using, beside the system
    /// clipboard and for the same reason: a window means a person, and a person
    /// expects what they kept to still be there tomorrow. Every headless mode — a
    /// screenshot, an automation session, a test — keeps the in-memory store, so
    /// none of them can read or overwrite that person's bookmarks.
    pub fn persist_bookmarks(&mut self) {
        self.bookmarks = crate::bookmarks::BookmarkStore::persisted();
        self.sync_address();
    }

    /// Keep the cookies the last run left, and write every change from now on.
    ///
    /// The same rule as the bookmarks and for a sharper reason: the file is
    /// somebody's signed-in sessions, so every headless mode — a screenshot, an
    /// automation session, a test — keeps the in-memory jar and none of them can
    /// read or overwrite it.
    ///
    /// The jar handed to the loader is unchanged by this; only what is in it and
    /// where it is written are.
    pub fn persist_cookies(&mut self) {
        self.cookies.persist();
    }

    /// The jar, to hand to a loader before the browser is built.
    pub fn cookies(&self) -> &crate::cookies::CookieStore {
        &self.cookies
    }

    /// The jar, to list and to empty.
    pub fn cookies_mut(&mut self) -> &mut crate::cookies::CookieStore {
        &mut self.cookies
    }

    /// Use `cache` as this browser's HTTP cache.
    ///
    /// The same shape as the jar, and for the same reason: the loader is built
    /// before the browser and both have to hold the one cache.
    pub fn set_cache(&mut self, cache: otlyra_net::SharedCache) {
        self.cache = Some(cache);
    }

    /// What has already been fetched, for a surface that lists it or empties it.
    pub fn cache(&self) -> Option<&otlyra_net::SharedCache> {
        self.cache.as_ref()
    }

    /// Use `store` as this browser's jar.
    ///
    /// For a shell that made the jar first because the loader needed it — which is
    /// the ordinary case, since a loader is built before the browser that holds it.
    pub fn set_cookie_store(&mut self, store: crate::cookies::CookieStore) {
        self.cookies = store;
        self.sync_cookie_policy();
    }

    /// Tell the jar what the reader's switch says.
    ///
    /// The jar holds the answer rather than the loader, because the loader is
    /// built once and the switch moves — and because the surfaces have to be able
    /// to show what is in force.
    pub(super) fn sync_cookie_policy(&mut self) {
        let accepts = !self.settings.settings.block_third_party_cookies;
        self.cookies
            .with(|jar| jar.set_accepts_third_party(accepts));
    }

    /// Whether the page the reader is on is one they kept.
    pub fn is_bookmarked(&self) -> bool {
        self.bookmarks.contains(&self.tabs[self.active].url)
    }

    /// Act on what the bookmarks surface reported.
    pub(super) fn handle_bookmarks_action(&mut self, action: crate::bookmarks::Action) {
        match action {
            crate::bookmarks::Action::Open(url) => self.navigate_from(&url, false),
            crate::bookmarks::Action::Remove(url) => {
                self.bookmarks.remove(&url);
            }
            crate::bookmarks::Action::Clear => self.bookmarks.clear(),
            crate::bookmarks::Action::Close => self.close_system_page(),
            crate::bookmarks::Action::None => {}
        }
    }

    /// Act on what the cache surface reported.
    pub(super) fn handle_cache_action(&mut self, action: crate::cache::Action) {
        if action == crate::cache::Action::Close {
            self.close_system_page();
            return;
        }
        let Some(cache) = self.cache.as_ref() else {
            return;
        };
        let mut held = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match action {
            crate::cache::Action::ClearSite(site) => {
                held.clear_site(&site);
            }
            crate::cache::Action::Clear => held.clear(),
            crate::cache::Action::Close | crate::cache::Action::None => {}
        }
    }

    /// Act on what the cookies surface reported.
    pub(super) fn handle_cookies_action(&mut self, action: crate::cookies::Action) {
        match action {
            crate::cookies::Action::ClearSite(site) => {
                self.cookies.with(|jar| jar.clear_site(&site));
                // Immediately, not at the next fetch: a person who asked to be rid
                // of something should not have it on the disk while they read the
                // page that says it is gone.
                self.cookies.flush();
            }
            crate::cookies::Action::Clear => {
                self.cookies.with(otlyra_net::cookie::Jar::clear);
                self.cookies.flush();
            }
            crate::cookies::Action::Close => self.close_system_page(),
            crate::cookies::Action::None => {}
        }
    }

    /// Act on what the downloads surface reported.
    pub(super) fn handle_downloads_action(&mut self, action: downloads::Action) {
        match action {
            downloads::Action::Clear => self.downloads.clear(),
            downloads::Action::Save(id) => {
                let asked = self.downloads.get(id).and_then(|download| {
                    // The dialogue opens in the download folder, so the preference
                    // is where a manual save starts from as well as where an
                    // automatic one ends up.
                    choose_download_path(
                        download.filename(),
                        self.settings.settings.download_directory().as_deref(),
                    )
                    .map(|path| (path, download.payload()))
                });
                if let Some((path, bytes)) = asked {
                    self.start_save(id, downloads::Destination::Exact(path), bytes);
                }
            }
            downloads::Action::Retry(id) => {
                // The directory is read again rather than remembered: the usual
                // reason a write failed is that where it was going was wrong, and
                // the usual answer is that the reader has since changed it.
                let Some(directory) = self.settings.settings.download_directory() else {
                    return;
                };
                let retry = self
                    .downloads
                    .get(id)
                    .map(|download| (download.retry_destination(directory), download.payload()));
                if let Some((destination, bytes)) = retry {
                    self.start_save(id, destination, bytes);
                }
            }
            downloads::Action::Close => self.close_system_page(),
            downloads::Action::None => {}
        }
    }

    /// Mark a row as being written and hand the bytes to the writer.
    ///
    /// One place, because the row's in-progress state and the write starting must
    /// not be able to disagree: a row that says nothing is happening while a file
    /// is being written offers a second Save As over the top of the first.
    pub(super) fn start_save(
        &mut self,
        id: downloads::DownloadId,
        destination: downloads::Destination,
        bytes: std::sync::Arc<[u8]>,
    ) {
        match &destination {
            downloads::Destination::Exact(path) => self.downloads.mark_saving(
                id,
                path.to_string_lossy().into_owned(),
                Some(path.clone()),
            ),
            // No path yet: which name a directory write lands on is the writer's
            // answer, and the row says the folder until it comes back.
            downloads::Destination::Into { directory, .. } => {
                self.downloads
                    .mark_saving(id, directory.to_string_lossy().into_owned(), None);
            }
        }
        self.downloads_writer.save(id, destination, bytes);
    }
}

/// Ask where one completed attachment should be written.
///
/// The dialogue itself is modal by platform convention. The write starts only
/// after it closes and runs through [`downloads::DownloadWriter`], never on the
/// browser thread.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn choose_download_path(
    filename: &str,
    directory: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let mut dialogue = rfd::FileDialog::new().set_file_name(filename);
    // Only a directory that is there: a dialogue told to open somewhere that does
    // not exist opens somewhere of its own choosing on some platforms and refuses
    // on others, and neither is what the preference meant.
    if let Some(directory) = directory.filter(|directory| directory.is_dir()) {
        dialogue = dialogue.set_directory(directory);
    }
    dialogue.save_file()
}

/// The same, where the platform has no native file dialogue.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn choose_download_path(
    _filename: &str,
    _directory: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    tracing::debug!("no save dialogue on this platform; the attachment remains in the session");
    None
}

/// Ask which folder automatic downloads should go into.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn choose_download_directory(current: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let mut dialogue = rfd::FileDialog::new();
    if let Some(current) = current.filter(|current| current.is_dir()) {
        dialogue = dialogue.set_directory(current);
    }
    dialogue.pick_folder()
}

/// The same, where there is nothing to ask.
#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn choose_download_directory(_current: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    tracing::debug!("no folder dialogue on this platform; the download folder is unchanged");
    None
}
