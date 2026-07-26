//! The one cookie jar, and the file it survives in.
//!
//! The jar itself is `otlyra_net::cookie` — the rules, the matching, what a site
//! is entitled to keep. This is the browser's half of it: one jar shared between
//! the loader that fills it and the surfaces that show it, and the file it is read
//! from and written back to.
//!
//! Two jars, in the sense that matters. A session cookie is one the reader is
//! signed in with *now*: it lives in memory and dies with the process, which is
//! what makes closing the browser end a session. A persistent one has an expiry
//! the server asked for, and that is what goes to disk.
//!
//! # The file is sealed
//!
//! This one is not written in the clear, and it is the only store here that is
//! not. A cookie file is somebody's signed-in sessions: in plain text it is
//! readable by every process running as that person, by anything that later reads
//! the disk, and by whatever a backup copies it into. Chrome and Firefox both
//! seal theirs, and the reasoning — including what it does *not* protect against
//! — is in [`crate::secret`].
//!
//! With no key there is no file. A platform with nowhere safe to keep one keeps
//! cookies in memory for the run and says so, because writing them in the clear
//! instead would be the browser deciding, on a person's behalf, that their
//! sessions are worth less than the convenience.
//!
//! # When it is written
//!
//! Not on every change, and not on a timer. The jar keeps a revision that moves
//! only when the *persistent* set does, and [`CookieStore::flush`] writes only
//! when it has moved since the last write. A site resetting a session cookie on
//! every response — which is most of them — therefore costs no disk at all, and a
//! sign-in costs one write. Through a temporary file and a rename, like the
//! bookmarks: a crash mid-write cannot leave half a jar where the whole one was.
//!
//! Reading the file is the shell's job, the same rule the bookmarks, the
//! preferences and the system clipboard already follow. A browser that read it in
//! its constructor would mean every test reading and writing the cookies of
//! whoever ran them — and cookies are the one store where that would be somebody's
//! signed-in session.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use otlyra_gfx::DisplayList;
use otlyra_net::SharedJar;
use otlyra_net::cookie::{Capacity, Cookie, Jar, store};
use otlyra_platform::{Key, Modifiers};
use otlyra_text::TextEngine;

use crate::widget::controls::{self, Elide, Elided, Emphasis};
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Child, Cx, Described, Event, Flex, Focus, FocusId, Gap, Insets, Label,
    Overflow, Padding, Rect, Scroll, Size, Stack, fill_rounded,
};

/// What the cookie file is called inside the browser's own directory.
///
/// Not `.tsv`: what is in it is tab-separated and what is *on the disk* is a
/// sealed blob, and a name that promised text would be the wrong thing to tell
/// somebody looking at their own directory.
const FILE: &str = "cookies.dat";

/// What the file was called while it was written in the clear.
///
/// Read once, moved into the sealed one, and then removed — leaving it behind
/// would keep a plain copy of the very thing this change exists to stop being
/// plain.
const PLAIN_FILE: &str = "cookies.tsv";

/// What the sealed file is bound to, so it cannot be handed over where another
/// of the browser's sealed files was expected.
const PURPOSE: &[u8] = b"cookies";

/// The jar, and where it is kept.
pub struct CookieStore {
    jar: SharedJar,
    /// Where to write. `None` in a test, on a platform with nowhere to keep a
    /// key, and whenever the keychain refuses — which turns persistence off
    /// rather than turning cookies off.
    file: Option<PathBuf>,
    /// What the file is sealed with. Present exactly when `file` is: a store that
    /// could not get a key does not get a path either, so there is no state in
    /// which something could be written unsealed.
    key: Option<crate::secret::Key>,
    /// The revision last written, so a flush with nothing to say costs nothing.
    written: u64,
}

impl Default for CookieStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl CookieStore {
    /// A jar that keeps nothing between runs.
    ///
    /// What a test, a screenshot and every headless mode get, so none of them can
    /// read or overwrite a person's session.
    pub fn in_memory() -> Self {
        Self {
            jar: Arc::new(Mutex::new(Jar::new())),
            file: None,
            key: None,
            written: 0,
        }
    }

    /// Attach the file: read what the last run kept into this jar, and write every
    /// change from now on.
    ///
    /// **The jar itself is not replaced.** A loader was handed this jar before the
    /// shell decided whether to persist it, and swapping it here would leave the
    /// loader filling a jar nobody reads — the bug that shape of wiring always
    /// produces. What is read is put into the jar that already exists.
    ///
    /// Never fails. A file that is not there is a browser nobody has been signed
    /// in with; a file that cannot be read is a warning and an empty jar, because
    /// refusing to start over a cookie file would be refusing to start.
    pub fn persist(&mut self) {
        let Some(file) = file_path(FILE) else {
            tracing::warn!("nowhere to keep cookies; sessions will not survive this run");
            return;
        };
        // The key first. Without one there is no file: a store that fell back to
        // plain text here would be the whole point of the exercise undone at the
        // one moment nobody is watching.
        let Some(key) = crate::secret::Key::from_keychain() else {
            tracing::warn!("no key to seal cookies with; sessions will not survive this run");
            return;
        };

        let now = SystemTime::now();
        let text = match std::fs::read(&file) {
            // A file that is not there is not a warning: a browser nobody has
            // signed in with has none, and saying so on every launch would be
            // noise about the ordinary case.
            Err(_) => String::new(),
            Ok(sealed) => match key.open(PURPOSE, &sealed) {
                Some(plain) => String::from_utf8(plain).unwrap_or_default(),
                None if crate::secret::is_sealed(&sealed) => {
                    // Sealed, and not with this key — a keychain that was reset, a
                    // file from another account. Not repairable and not a reason
                    // to refuse to start; the sessions in it are simply gone.
                    tracing::warn!("the cookie file was sealed with another key; starting empty");
                    String::new()
                }
                None => {
                    tracing::warn!("the cookie file is not one of ours; starting empty");
                    String::new()
                }
            },
        };

        let read = store::from_text(&text, Capacity::default(), now);
        let revision = self.with(|jar| {
            for cookie in read.all() {
                jar.put(cookie.clone());
            }
            jar.kept_revision()
        });
        // What was read is already what is on disk, so the first flush has nothing
        // to do.
        self.written = revision;
        self.file = Some(file);
        self.key = Some(key);
        self.take_over_from_a_plain_file();
    }

    /// Move what an older build wrote in the clear into the sealed file, and then
    /// be rid of the plain one.
    ///
    /// The order matters and is the whole of this function: read, seal, write,
    /// and only then remove. A removal before a successful write would lose the
    /// sessions; leaving the plain file behind would keep a readable copy of
    /// exactly what was just sealed.
    fn take_over_from_a_plain_file(&mut self) {
        let Some(plain) = file_path(PLAIN_FILE) else {
            return;
        };
        let Ok(text) = std::fs::read_to_string(&plain) else {
            return;
        };
        let read = store::from_text(&text, Capacity::default(), SystemTime::now());
        tracing::info!(
            cookies = read.len(),
            "sealing what an earlier build kept in the clear"
        );
        self.with(|jar| {
            for cookie in read.all() {
                jar.put(cookie.clone());
            }
        });
        if !self.flush() {
            tracing::warn!("could not seal them; the plain file is left where it is");
            return;
        }
        if let Err(error) = std::fs::remove_file(&plain) {
            tracing::warn!(%error, path = %plain.display(), "the plain cookie file is still there");
        }
    }

    /// A store sealed with `key` and written to `file`, for a test that must
    /// touch neither the machine's keychain nor a person's own directory.
    #[cfg(test)]
    fn sealed_at(file: PathBuf, key: crate::secret::Key) -> Self {
        Self {
            jar: Arc::new(Mutex::new(Jar::new())),
            file: Some(file),
            key: Some(key),
            written: 0,
        }
    }

    /// Read `file` back into this store, the way [`CookieStore::persist`] does
    /// once it has a key.
    #[cfg(test)]
    fn reopen(&mut self) {
        let (Some(file), Some(key)) = (self.file.clone(), self.key.as_ref()) else {
            return;
        };
        let sealed = std::fs::read(&file).unwrap_or_default();
        let text = key
            .open(PURPOSE, &sealed)
            .and_then(|plain| String::from_utf8(plain).ok())
            .unwrap_or_default();
        let read = store::from_text(&text, Capacity::default(), SystemTime::now());
        self.with(|jar| {
            for cookie in read.all() {
                jar.put(cookie.clone());
            }
        });
    }

    /// The jar itself, to give to a loader.
    pub fn jar(&self) -> SharedJar {
        Arc::clone(&self.jar)
    }

    /// Whether this store puts anything on disk.
    pub fn is_persistent(&self) -> bool {
        self.file.is_some()
    }

    /// Do something with the jar under its lock.
    ///
    /// The lock is held for the call and no longer. A jar poisoned by a panic
    /// elsewhere is still a list of cookies, so it is taken back rather than
    /// spreading the panic to everything that wanted one.
    pub fn with<T>(&self, act: impl FnOnce(&mut Jar) -> T) -> T {
        let mut jar = self
            .jar
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        act(&mut jar)
    }

    /// Write the persistent cookies down, if any of them have changed.
    ///
    /// Cheap to call often, which is the point: the caller does not have to know
    /// whether a fetch set a cookie, only that one finished.
    /// Answers whether anything reached the disk, which is what the migration
    /// needs before it removes the plain file it read from.
    pub fn flush(&mut self) -> bool {
        let (Some(file), Some(key)) = (self.file.clone(), self.key.as_ref()) else {
            return false;
        };
        let now = SystemTime::now();
        let (revision, text) = self.with(|jar| (jar.kept_revision(), store::to_text(jar, now)));
        if revision == self.written {
            return true;
        }
        let Some(sealed) = key.seal(PURPOSE, text.as_bytes()) else {
            tracing::warn!("could not seal the cookies; nothing was written");
            return false;
        };
        self.written = revision;

        if let Some(directory) = file.parent()
            && let Err(error) = std::fs::create_dir_all(directory)
        {
            tracing::warn!(%error, path = %directory.display(), "could not make the browser's directory");
            return false;
        }
        let temporary = file.with_extension("dat.writing");
        if let Err(error) = std::fs::write(&temporary, sealed) {
            tracing::warn!(%error, path = %temporary.display(), "could not write the cookies");
            return false;
        }
        if let Err(error) = std::fs::rename(&temporary, &file) {
            tracing::warn!(%error, path = %file.display(), "could not replace the cookies");
            let _ = std::fs::remove_file(&temporary);
            return false;
        }
        true
    }
}

/// Where one of the browser's own files lives, when there is anywhere.
fn file_path(name: &str) -> Option<PathBuf> {
    Some(crate::preferences::directory()?.join(name))
}

/// What the cookies surface reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing.
    None,
    /// Throw away everything one site is keeping.
    ClearSite(String),
    /// Throw away everything.
    Clear,
    /// Leave the surface.
    Close,
}

/// The height of the surface's own header, above the scrolling content.
const HEADER_HEIGHT: f64 = 52.0;
/// The widest the list is allowed to be, however wide the window is.
const CONTENT_WIDTH: f64 = 680.0;

/// Everything the surface's appearance is a function of.
#[derive(Clone, PartialEq)]
struct Drawn {
    rect: Rect,
    revision: u64,
    scroll: f64,
    pointer: (f64, f64),
    focus: Option<FocusId>,
}

/// One site, and everything it is keeping here.
struct Site {
    name: String,
    cookies: Vec<Cookie>,
}

/// The jar as a surface: who is keeping what, and a way to be rid of it.
///
/// Grouped by site rather than listed flat, because *by whom* is the question a
/// person opens this page with. A site keeping four cookies is one line of
/// concern, and forty flat rows is none.
pub struct CookiesSurface {
    /// Every colour and measurement it is drawn from.
    pub theme: Theme,
    focused: Option<FocusId>,
    focus: Focus,
    scroll: f64,
    overflow: Overflow,
    pointer: (f64, f64),
    cache: Option<(Drawn, DisplayList)>,
    builds: u64,
    root: Option<Child<Action>>,
}

impl Default for CookiesSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl CookiesSurface {
    /// An empty, unfocused surface in the default theme.
    pub fn new() -> Self {
        Self {
            theme: Theme::light(),
            focused: None,
            focus: Focus::default(),
            scroll: 0.0,
            overflow: Overflow::default(),
            pointer: (-1.0, -1.0),
            cache: None,
            builds: 0,
            root: None,
        }
    }

    /// What the last frame drew, for something that cannot see it.
    pub fn describe(&self) -> Vec<Described> {
        let mut out = Vec::new();
        if let Some(root) = self.root.as_ref() {
            root.describe(&mut out);
        }
        out
    }

    /// Which control holds the keyboard.
    pub fn focused(&self) -> Option<FocusId> {
        self.focused
    }

    /// Relinquish the keyboard when another surface becomes active.
    pub fn blur(&mut self) {
        self.focused = None;
    }

    /// Draw from `theme` from the next frame on.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.theme = theme;
            self.cache = None;
        }
    }

    /// How many display lists this surface built rather than reused.
    pub fn builds(&self) -> u64 {
        self.builds
    }

    /// Activate the control a reader named.
    pub fn activate_described(&mut self, index: usize, text: &mut TextEngine) -> Action {
        let Some(focus) = self.describe().get(index).and_then(|node| node.focus) else {
            return Action::None;
        };
        self.focused = Some(focus);
        self.offer(&Event::Activate, text)
    }

    /// Note where the pointer is.
    pub fn pointer_moved(&mut self, x: f64, y: f64) {
        self.pointer = (x, y);
    }

    /// Press at the last reported position.
    pub fn pointer_pressed(&mut self, text: &mut TextEngine) -> Action {
        let action = self.offer(&Event::PointerPressed, text);
        if action == Action::None {
            self.focused = None;
        }
        action
    }

    /// Scroll by `delta` logical pixels, stopping at the ends.
    pub fn scroll_by(&mut self, delta: f64) {
        self.scroll = (self.scroll + delta).clamp(0.0, self.overflow.get());
    }

    /// Handle traversal and activation. `None` leaves the key for the toolbar.
    pub fn key_pressed(
        &mut self,
        key: Key,
        modifiers: Modifiers,
        text: &mut TextEngine,
    ) -> Option<Action> {
        if modifiers.is_accelerator() {
            return None;
        }
        match key {
            Key::Tab => {
                self.focused = if modifiers.shift {
                    self.focus.previous(self.focused)
                } else {
                    self.focus.next(self.focused)
                };
                Some(Action::None)
            }
            Key::Escape => match self.focused {
                Some(_) => {
                    self.focused = None;
                    Some(Action::None)
                }
                None => Some(Action::Close),
            },
            Key::Enter | Key::Character(' ') if self.focused.is_some() => {
                Some(self.offer(&Event::Activate, text))
            }
            _ => None,
        }
    }

    /// What a press at `x`, `y` would report, without applying it.
    pub fn action_at(&mut self, x: f64, y: f64, text: &mut TextEngine) -> Action {
        let pointer = self.pointer;
        self.pointer = (x, y);
        let action = self.offer(&Event::PointerPressed, text);
        self.pointer = pointer;
        action
    }

    /// What the pointer should look like at `x`, `y`.
    pub fn cursor_at(&mut self, x: f64, y: f64, text: &mut TextEngine) -> otlyra_platform::Cursor {
        match self.action_at(x, y, text) {
            Action::None => otlyra_platform::Cursor::Default,
            _ => otlyra_platform::Cursor::Pointer,
        }
    }

    fn offer(&mut self, event: &Event, text: &mut TextEngine) -> Action {
        let Some(root) = self.root.as_mut() else {
            return Action::None;
        };
        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.focus = self.focused;
        cx.theme = self.theme.clone();
        root.event(event, &mut cx).unwrap_or(Action::None)
    }

    /// Paint the page into `rect`, in window coordinates.
    pub fn build_display_list(
        &mut self,
        rect: Rect,
        store: &CookieStore,
        text: &mut TextEngine,
        out: &mut DisplayList,
    ) {
        let drawn = Drawn {
            rect,
            revision: store.with(|jar| jar.revision()),
            scroll: self.scroll,
            pointer: self.pointer,
            focus: self.focused,
        };
        if let Some((built, list)) = &self.cache
            && *built == drawn
            && self.root.is_some()
        {
            out.append(list);
            return;
        }

        // Read out from under the lock once, rather than holding it across a
        // measure, a place and a draw.
        let sites = store.with(|jar| by_site(jar.all()));

        self.builds += 1;
        let mut built = DisplayList::new();
        let theme = self.theme.clone();
        fill_rounded(&mut built, rect, theme.surface_sunken, 0.0);

        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.focus = self.focused;
        cx.theme = theme.clone();

        self.focus.begin();
        let mut root = self.build(&theme, rect.width, &sites, &self.focus);
        root.measure(Size::new(rect.width, rect.height), &mut cx);
        root.place(rect, &mut cx);
        root.draw(&mut cx, &mut built);
        self.scroll = self.scroll.clamp(0.0, self.overflow.get());

        self.root = Some(root);
        self.cache = Some((drawn, built));
        let (_, built) = self.cache.as_ref().expect("just stored");
        out.append(built);
    }

    fn build(&self, theme: &Theme, width: f64, sites: &[Site], focus: &Focus) -> Child<Action> {
        let mut rows: Vec<Child<Action>> = Vec::new();
        if sites.is_empty() {
            rows.push(Box::new(Padding::new(
                Insets::all(theme.inset * 2.0),
                Box::new(Align::centre(Box::new(Label::new(
                    "No site is keeping anything here.",
                    theme.font_size,
                    theme.ink_dim,
                )))),
            )));
        } else {
            for site in sites {
                rows.push(self.site_card(theme, focus, site));
            }
        }
        rows.push(Box::new(Gap::new(0.0, theme.inset * 2.0)));

        let column: Child<Action> = Box::new(Padding::new(
            Insets::symmetric(theme.inset * 2.0, theme.inset * 2.0),
            Box::new(Stack::column(theme.gap, rows)),
        ));
        let centred: Child<Action> = Box::new(Stack::row(
            0.0,
            vec![
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
                Box::new(crate::widget::Fixed::width(
                    CONTENT_WIDTH.min(width),
                    column,
                )),
                Box::new(Flex::new(1.0, Box::new(Gap::new(0.0, 0.0)))),
            ],
        ));

        Box::new(Stack::column(
            0.0,
            vec![
                self.header(theme, focus, !sites.is_empty()),
                Box::new(Scroll::new(self.scroll, Rc::clone(&self.overflow), centred)),
            ],
        ))
    }

    fn header(&self, theme: &Theme, focus: &Focus, has_cookies: bool) -> Child<Action> {
        let title: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            "Cookies",
            theme.font_size + 3.0,
            theme.ink,
        ))));
        Box::new(crate::widget::Fixed::height(
            HEADER_HEIGHT,
            Box::new(Background::new(
                theme.surface,
                0.0,
                Box::new(Padding::new(
                    Insets::symmetric(theme.inset * 2.0, theme.gap),
                    Box::new(Stack::row(
                        theme.gap,
                        vec![
                            Box::new(Flex::new(1.0, title)),
                            Box::new(Align::centre(controls::button(
                                theme,
                                focus,
                                Action::Clear,
                                "Remove all",
                                Emphasis::Danger,
                                has_cookies,
                            ))),
                            Box::new(Align::centre(controls::button(
                                theme,
                                focus,
                                Action::Close,
                                "Done",
                                Emphasis::Primary,
                                true,
                            ))),
                        ],
                    )),
                )),
            )),
        ))
    }

    /// One site: its name, how much it is keeping, a way to be rid of it, and
    /// then what it is actually keeping.
    fn site_card(&self, theme: &Theme, focus: &Focus, site: &Site) -> Child<Action> {
        let name: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            site.name.clone(),
            theme.font_size,
            theme.ink,
            Elide::End,
        ))));
        let count = site.cookies.len();
        let summary: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            if count == 1 {
                "1 cookie".to_owned()
            } else {
                format!("{count} cookies")
            },
            theme.font_size_small,
            theme.ink_dim,
        ))));

        let head: Child<Action> = Box::new(Stack::row(
            theme.inset,
            vec![
                Box::new(Flex::new(
                    1.0,
                    Box::new(Stack::column(theme.gap * 0.5, vec![name, summary])),
                )),
                // Named, because four buttons all reading "Remove" is what
                // something that cannot see the row hears. The name says which
                // site it is the Remove of.
                Box::new(Align::centre(Box::new(
                    crate::widget::Named::instead_of_its_own(
                        format!("Remove everything {} is keeping", site.name),
                        controls::button(
                            theme,
                            focus,
                            Action::ClearSite(site.name.clone()),
                            "Remove",
                            Emphasis::Normal,
                            true,
                        ),
                    ),
                ))),
            ],
        ));

        let mut rows: Vec<Child<Action>> = vec![head];
        rows.extend(
            site.cookies
                .iter()
                .map(|cookie| self.cookie_row(theme, cookie)),
        );
        controls::card_plain(theme, rows)
    }

    /// One cookie: what it is called, where it goes back to, and what it says
    /// about itself.
    ///
    /// The value is deliberately not shown. It is the thing a session *is*, so a
    /// page that put it on screen would be a page that leaks it to anyone reading
    /// over a shoulder or watching a shared screen — and it tells a person
    /// nothing, because it is a server's opaque string.
    fn cookie_row(&self, theme: &Theme, cookie: &Cookie) -> Child<Action> {
        let name: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            cookie.name.clone(),
            theme.font_size_small,
            theme.ink,
            Elide::End,
        ))));
        let detail: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            describe(cookie),
            theme.font_size_small,
            theme.ink_dim,
            Elide::End,
        ))));
        Box::new(Padding::new(
            Insets::symmetric(0.0, theme.gap * 0.5),
            Box::new(Stack::row(
                theme.inset,
                vec![
                    Box::new(crate::widget::Fixed::width(160.0, name)),
                    Box::new(Flex::new(1.0, detail)),
                ],
            )),
        ))
    }
}

/// What one cookie says about itself, in a person's words.
fn describe(cookie: &Cookie) -> String {
    let mut said = format!("{}{}", cookie.domain, cookie.path);
    if !cookie.host_only {
        said.push_str(" and below");
    }
    said.push_str(if cookie.is_persistent() {
        " · kept"
    } else {
        " · until you quit"
    });
    if cookie.secure {
        said.push_str(" · secure");
    }
    if cookie.http_only {
        said.push_str(" · not for scripts");
    }
    said
}

/// The cookies grouped by the site that set them, alphabetically, and each site's
/// own sorted by name so the list does not reshuffle as they are re-sent.
fn by_site(cookies: &[Cookie]) -> Vec<Site> {
    let mut sites: Vec<Site> = Vec::new();
    for cookie in cookies {
        let name = cookie.site();
        match sites.iter_mut().find(|site| site.name == name) {
            Some(site) => site.cookies.push(cookie.clone()),
            None => sites.push(Site {
                name: name.to_owned(),
                cookies: vec![cookie.clone()],
            }),
        }
    }
    sites.sort_by(|one, other| one.name.cmp(&other.name));
    for site in &mut sites {
        site.cookies
            .sort_by(|one, other| one.name.cmp(&other.name).then(one.path.cmp(&other.path)));
    }
    sites
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn url(address: &str) -> Url {
        Url::parse(address).expect("a url")
    }

    /// A store with nowhere to write is a working jar, not a broken one.
    #[test]
    fn a_store_with_no_file_still_keeps_cookies() {
        let mut store = CookieStore::in_memory();
        assert!(!store.is_persistent());
        store.with(|jar| {
            jar.set(&url("https://x.test/"), "a=1", SystemTime::now())
                .expect("kept");
        });
        assert_eq!(store.with(|jar| jar.len()), 1);
        // And flushing one is a no-op rather than a failure.
        store.flush();
        assert_eq!(store.with(|jar| jar.len()), 1);
    }

    /// The loader and the surfaces hold the same jar, not two copies of one.
    #[test]
    fn the_jar_handed_out_is_the_jar_kept() {
        let store = CookieStore::in_memory();
        let handed = store.jar();
        handed
            .lock()
            .expect("not poisoned")
            .set(&url("https://x.test/"), "a=1", SystemTime::now())
            .expect("kept");
        assert_eq!(store.with(|jar| jar.len()), 1);
    }

    /// A session cookie must not be able to make the browser write a file. This is
    /// the whole reason the revision counts only what is kept.
    #[test]
    fn a_session_cookie_moves_no_revision() {
        let store = CookieStore::in_memory();
        let before = store.with(|jar| jar.kept_revision());
        store.with(|jar| {
            for line in ["a=1", "b=2", "c=3"] {
                jar.set(&url("https://x.test/"), line, SystemTime::now())
                    .expect("kept");
            }
        });
        assert_eq!(store.with(|jar| jar.kept_revision()), before);

        // And one with an expiry does move it.
        store.with(|jar| {
            jar.set(
                &url("https://x.test/"),
                "d=4; Max-Age=600",
                SystemTime::now(),
            )
            .expect("kept");
        });
        assert_ne!(store.with(|jar| jar.kept_revision()), before);
    }

    /// The file on disk is sealed, and what comes back out of it is what went in.
    #[test]
    fn what_reaches_the_disk_is_sealed_and_comes_back() {
        let file = std::env::temp_dir().join("otlyra-cookie-seal-test.dat");
        let _ = std::fs::remove_file(&file);

        let mut store =
            CookieStore::sealed_at(file.clone(), crate::secret::Key::from_bytes([3u8; 32]));
        store.with(|jar| {
            jar.set(
                &url("https://bank.test/"),
                "session=s3cret; Max-Age=600",
                SystemTime::now(),
            )
            .expect("kept");
        });
        assert!(store.flush(), "it wrote");

        // What is actually on the disk holds neither the name nor the value.
        let bytes = std::fs::read(&file).expect("the file is there");
        assert!(crate::secret::is_sealed(&bytes));
        for plain in [&b"s3cret"[..], b"session", b"bank.test"] {
            assert!(
                !bytes.windows(plain.len()).any(|window| window == plain),
                "{} is in the file",
                String::from_utf8_lossy(plain)
            );
        }

        // And the same key reads it back.
        let mut reopened =
            CookieStore::sealed_at(file.clone(), crate::secret::Key::from_bytes([3u8; 32]));
        reopened.reopen();
        assert_eq!(reopened.with(|jar| jar.len()), 1);
        assert_eq!(reopened.with(|jar| jar.all()[0].value.clone()), "s3cret");

        // Another key reads nothing rather than reading nonsense.
        let mut stranger =
            CookieStore::sealed_at(file.clone(), crate::secret::Key::from_bytes([4u8; 32]));
        stranger.reopen();
        assert!(stranger.with(|jar| jar.is_empty()));

        std::fs::remove_file(&file).expect("remove the test-owned file");
    }

    /// A flush with nothing new to say writes nothing — which is what keeps a
    /// site resetting a session cookie from costing a disk write per response.
    #[test]
    fn a_flush_with_nothing_to_say_writes_nothing() {
        let file = std::env::temp_dir().join("otlyra-cookie-quiet-test.dat");
        let _ = std::fs::remove_file(&file);

        let mut store =
            CookieStore::sealed_at(file.clone(), crate::secret::Key::from_bytes([5u8; 32]));
        store.with(|jar| {
            jar.set(
                &url("https://x.test/"),
                "a=1; Max-Age=600",
                SystemTime::now(),
            )
            .expect("kept");
        });
        assert!(store.flush());
        let first = std::fs::read(&file).expect("written");

        // A session cookie, which never reaches a disk.
        store.with(|jar| {
            jar.set(&url("https://x.test/"), "b=2", SystemTime::now())
                .expect("kept");
        });
        assert!(store.flush());
        assert_eq!(
            std::fs::read(&file).expect("still there"),
            first,
            "a session cookie must not make the browser write a file"
        );

        std::fs::remove_file(&file).expect("remove the test-owned file");
    }

    // --- the surface ------------------------------------------------------

    /// A jar somebody has been browsing with.
    ///
    /// **Set in an order that is not the order they are shown in**, deliberately:
    /// a fixture whose insertion order already matches its sorted order cannot
    /// tell a sort from no sort at all.
    fn filled() -> CookieStore {
        let store = CookieStore::in_memory();
        store.with(|jar| {
            let now = SystemTime::now();
            for (address, line) in [
                ("https://tracker.test/", "id=999; Max-Age=600; Secure"),
                ("https://www.example.com/", "theme=dark; Max-Age=600"),
                ("https://api.example.com/", "token=xyz; Domain=example.com"),
                ("https://www.example.com/app/x", "session=abc; HttpOnly"),
            ] {
                jar.set(&url(address), line, now).expect("kept");
            }
        });
        store
    }

    fn drawn(surface: &mut CookiesSurface, store: &CookieStore) -> Vec<Described> {
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        surface.build_display_list(
            Rect::new(0.0, 0.0, 900.0, 600.0),
            store,
            &mut text,
            &mut list,
        );
        surface.describe()
    }

    fn words(described: &[Described]) -> String {
        described
            .iter()
            .map(|node| node.label.clone())
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// Grouped by the site that set them, alphabetically, and a site's own
    /// cookies by name — so the page does not reshuffle as they are re-sent.
    #[test]
    fn cookies_are_grouped_by_site() {
        let store = filled();
        let sites = store.with(|jar| by_site(jar.all()));
        assert_eq!(
            sites.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["example.com", "tracker.test"]
        );
        assert_eq!(
            sites[0]
                .cookies
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["session", "theme", "token"],
            "one site's own, by name"
        );
    }

    /// A cookie's value is what a session *is*, and the page does not say one.
    ///
    /// This half is what the page *tells* something that cannot see it. The other
    /// half — what it paints — is pinned by the goldens in `interface_golden.rs`,
    /// whose fixture carries values for exactly that reason: a display list holds
    /// glyph identifiers rather than the string they were shaped from, so there is
    /// nothing here to read a painted value back out of, and a picture is what
    /// catches one appearing.
    #[test]
    fn the_value_is_never_drawn() {
        let store = filled();
        let mut surface = CookiesSurface::new();
        let said = words(&drawn(&mut surface, &store));
        for secret in ["abc", "xyz", "999"] {
            assert!(!said.contains(secret), "{secret:?} is in {said:?}");
        }
        // And the page does say the things that are safe to say.
        assert!(said.contains("example.com"), "{said:?}");
        assert!(said.contains("tracker.test"), "{said:?}");
    }

    /// Every button a reader is told about is one they can press, and pressing it
    /// reports what it says.
    #[test]
    fn what_a_reader_is_told_is_what_a_reader_can_press() {
        let store = filled();
        let mut surface = CookiesSurface::new();
        let described = drawn(&mut surface, &store);
        let mut text = TextEngine::new();

        let mut reported = Vec::new();
        for (index, node) in described.iter().enumerate() {
            if node.focus.is_none() {
                continue;
            }
            let action = surface.activate_described(index, &mut text);
            if action != Action::None {
                reported.push(action);
            }
        }
        assert!(reported.contains(&Action::Clear), "{reported:?}");
        assert!(reported.contains(&Action::Close), "{reported:?}");
        assert!(
            reported.contains(&Action::ClearSite("example.com".into())),
            "{reported:?}"
        );
        assert!(
            reported.contains(&Action::ClearSite("tracker.test".into())),
            "{reported:?}"
        );

        // And each of the two Removes says which site it is the Remove of, which
        // is the whole of what something that cannot see the row has to go on.
        let names: Vec<&str> = described
            .iter()
            .filter(|node| node.label.starts_with("Remove everything"))
            .map(|node| node.label.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "Remove everything example.com is keeping",
                "Remove everything tracker.test is keeping"
            ]
        );
    }

    /// Being rid of one site is being rid of that site, and of nothing else.
    #[test]
    fn one_site_can_be_thrown_away_without_the_rest() {
        let store = filled();
        assert_eq!(store.with(|jar| jar.clear_site("tracker.test")), 1);
        assert_eq!(store.with(|jar| jar.len()), 3);
        assert_eq!(
            store
                .with(|jar| by_site(jar.all()))
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            ["example.com"]
        );
    }

    /// With nothing kept there is nothing to remove, and the button says so
    /// rather than being a press that does nothing.
    #[test]
    fn an_empty_jar_offers_nothing_to_remove() {
        let empty = CookieStore::in_memory();
        let mut surface = CookiesSurface::new();
        let described = drawn(&mut surface, &empty);
        let remove_all = described
            .iter()
            .find(|node| node.label == "Remove all")
            .expect("the button is there");
        assert!(!remove_all.enabled, "nothing to remove");

        let mut surface = CookiesSurface::new();
        let described = drawn(&mut surface, &filled());
        let remove_all = described
            .iter()
            .find(|node| node.label == "Remove all")
            .expect("the button is there");
        assert!(remove_all.enabled, "and something to remove when there is");
    }

    /// A frame nothing changed for reuses its display list. A cookie replaced by
    /// one with a different value *is* a change, which a count would miss.
    #[test]
    fn an_unchanged_surface_reuses_its_display_list() {
        let store = filled();
        let mut surface = CookiesSurface::new();
        drawn(&mut surface, &store);
        let built = surface.builds();
        drawn(&mut surface, &store);
        assert_eq!(surface.builds(), built, "nothing changed");

        store.with(|jar| {
            jar.set(
                &url("https://www.example.com/"),
                "theme=light; Max-Age=600",
                SystemTime::now(),
            )
            .expect("kept");
        });
        drawn(&mut surface, &store);
        assert_eq!(surface.builds(), built + 1, "a value changed");
    }
}
