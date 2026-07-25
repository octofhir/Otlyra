//! `about:downloads` and the session's completed downloads.
//!
//! A response carrying `Content-Disposition: attachment` is not a document. The
//! browser keeps its bytes here instead of feeding them to the HTML parser, and
//! this surface shows the result without depending on the document engine.
//!
//! Each retained attachment can then be written through the native Save As
//! dialog. Automatic saving can use the same store once a download directory is
//! a persisted browser preference.

use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use otlyra_gfx::DisplayList;
use otlyra_platform::{Key, Modifiers, Waker};
use otlyra_text::TextEngine;

use crate::widget::controls::{self, Elide, Elided, Emphasis};
use crate::widget::theme::Theme;
use crate::widget::{
    Align, Background, Child, Cx, Described, Event, Flex, Focus, FocusId, Gap, Insets, Label,
    Overflow, Padding, Rect, Scroll, Size, Stack, fill_rounded,
};

/// The most attachment data retained by one browser session.
///
/// The network layer caps one response at 32 MiB. Four maximum-sized downloads
/// are enough to make the page useful without allowing repeated navigations to
/// grow the process forever.
const BYTE_BUDGET: usize = 128 * 1024 * 1024;

/// Stable identity of one completed attachment.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DownloadId(u64);

/// One attachment that completed.
#[derive(Debug, PartialEq, Eq)]
pub struct Download {
    id: DownloadId,
    filename: String,
    url: String,
    content_type: Option<String>,
    bytes: Arc<[u8]>,
    saved_to: Option<String>,
    saving_to: Option<String>,
    save_error: Option<String>,
}

impl Download {
    /// The identity actions from the surface use.
    pub fn id(&self) -> DownloadId {
        self.id
    }

    /// The safe leaf name reported by the response or inferred from its URL.
    pub fn filename(&self) -> &str {
        &self.filename
    }

    /// Where the attachment came from, after redirects.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The transport's declared media type.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The completed payload.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// A cheap owned handle for an asynchronous writer.
    pub(crate) fn payload(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }

    /// Where the payload was last saved during this session.
    pub fn saved_to(&self) -> Option<&str> {
        self.saved_to.as_deref()
    }

    /// Where a write is currently going.
    pub fn saving_to(&self) -> Option<&str> {
        self.saving_to.as_deref()
    }

    /// Why the last write failed.
    pub fn save_error(&self) -> Option<&str> {
        self.save_error.as_deref()
    }
}

/// Completed attachments owned by the browser rather than by any one tab.
#[derive(Default)]
pub struct DownloadStore {
    entries: Vec<Download>,
    bytes: usize,
    revision: u64,
    next_id: u64,
}

impl DownloadStore {
    /// Keep a completed attachment, evicting the oldest retained payloads if
    /// the session budget would otherwise be exceeded.
    pub fn record(
        &mut self,
        filename: impl Into<String>,
        url: impl Into<String>,
        content_type: Option<String>,
        bytes: Vec<u8>,
    ) {
        let incoming = bytes.len();
        while self.bytes + incoming > BYTE_BUDGET && !self.entries.is_empty() {
            let removed = self.entries.remove(0);
            self.bytes -= removed.bytes.len();
        }

        // The network limit is below the store budget in production. Keeping
        // this guard makes the store safe when used with a custom Loader.
        if incoming > BYTE_BUDGET {
            return;
        }

        self.bytes += incoming;
        let id = DownloadId(self.next_id);
        self.next_id += 1;
        self.entries.push(Download {
            id,
            filename: filename.into(),
            url: url.into(),
            content_type,
            bytes: bytes.into(),
            saved_to: None,
            saving_to: None,
            save_error: None,
        });
        self.revision += 1;
    }

    /// Forget all completed attachments and release their payloads.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.revision += 1;
    }

    /// Completed downloads, newest first.
    pub fn downloads(&self) -> impl Iterator<Item = &Download> {
        self.entries.iter().rev()
    }

    /// Find a completed attachment by its stable identity.
    pub fn get(&self, id: DownloadId) -> Option<&Download> {
        self.entries.iter().find(|download| download.id == id)
    }

    /// Remember where an attachment was saved so the page can say so.
    pub fn mark_saved(&mut self, id: DownloadId, path: impl Into<String>) {
        let Some(download) = self.entries.iter_mut().find(|download| download.id == id) else {
            return;
        };
        download.saved_to = Some(path.into());
        download.saving_to = None;
        download.save_error = None;
        self.revision += 1;
    }

    /// Put a row into its in-progress state before the writer starts.
    pub fn mark_saving(&mut self, id: DownloadId, path: impl Into<String>) {
        let Some(download) = self.entries.iter_mut().find(|download| download.id == id) else {
            return;
        };
        download.saving_to = Some(path.into());
        download.save_error = None;
        self.revision += 1;
    }

    /// Report a failed write without throwing away the attachment.
    pub fn mark_save_failed(&mut self, id: DownloadId, error: impl Into<String>) {
        let Some(download) = self.entries.iter_mut().find(|download| download.id == id) else {
            return;
        };
        download.saving_to = None;
        download.save_error = Some(error.into());
        self.revision += 1;
    }

    /// A number that changes whenever the store does.
    pub fn revision(&self) -> u64 {
        self.revision
    }
}

/// The result of one asynchronous file write.
pub struct SaveResult {
    /// Which row asked for it.
    pub id: DownloadId,
    /// The chosen destination.
    pub path: std::path::PathBuf,
    /// Success, or the operating system's reason.
    pub result: Result<(), String>,
}

/// Writes completed attachments without blocking the browser thread.
pub struct DownloadWriter {
    results: Receiver<SaveResult>,
    sender: Sender<SaveResult>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl Default for DownloadWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadWriter {
    /// A writer using the process-wide Tokio I/O runtime.
    pub fn new() -> Self {
        let (sender, results) = channel();
        Self {
            results,
            sender,
            waker: Arc::new(Mutex::new(None)),
        }
    }

    /// Wake the browser loop when a write finishes.
    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(waker);
        }
    }

    /// Start writing `bytes` and return immediately.
    pub fn save(&self, id: DownloadId, path: std::path::PathBuf, bytes: Arc<[u8]>) {
        let sender = self.sender.clone();
        let waker = Arc::clone(&self.waker);
        crate::io::shared().spawn(async move {
            let result = tokio::fs::write(&path, bytes.as_ref())
                .await
                .map_err(|error| error.to_string());
            let _ = sender.send(SaveResult { id, path, result });
            if let Some(waker) = waker.lock().ok().and_then(|slot| slot.clone()) {
                waker.wake();
            }
        });
    }

    /// Everything that finished since the browser last checked.
    pub fn poll(&self) -> Vec<SaveResult> {
        let mut finished = Vec::new();
        while let Ok(result) = self.results.try_recv() {
            finished.push(result);
        }
        finished
    }
}

/// Return the attachment's safe leaf filename when the headers say this
/// response is a download.
pub fn attachment_filename(headers: &[(String, String)], final_url: &str) -> Option<String> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-disposition"))
        .map(|(_, value)| value)?;
    let fields = disposition_fields(value);
    if !fields
        .first()
        .is_some_and(|field| field.trim().eq_ignore_ascii_case("attachment"))
    {
        return None;
    }

    let mut plain = None;
    let mut encoded = None;
    for field in fields.iter().skip(1) {
        let Some((name, value)) = field.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = unquote(value.trim());
        if name.eq_ignore_ascii_case("filename*") {
            encoded = decode_extended_filename(&value);
        } else if name.eq_ignore_ascii_case("filename") {
            plain = Some(value);
        }
    }

    encoded
        .or(plain)
        .and_then(|name| safe_leaf(&name))
        .or_else(|| filename_from_url(final_url))
        .or_else(|| Some("download".to_owned()))
}

/// Split a disposition at semicolons outside quoted strings.
fn disposition_fields(value: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        match character {
            '\\' if quoted && !escaped => escaped = true,
            '"' if !escaped => quoted = !quoted,
            ';' if !quoted => {
                fields.push(value[start..index].trim().to_owned());
                start = index + 1;
            }
            _ => escaped = false,
        }
    }
    fields.push(value[start..].trim().to_owned());
    fields
}

fn unquote(value: &str) -> String {
    let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };
    let mut result = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            result.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            result.push(character);
        }
    }
    if escaped {
        result.push('\\');
    }
    result
}

fn decode_extended_filename(value: &str) -> Option<String> {
    let mut parts = value.splitn(3, '\'');
    let charset = parts.next()?;
    let _language = parts.next()?;
    let encoded = parts.next()?;
    if !charset.is_empty() && !charset.eq_ignore_ascii_case("utf-8") {
        return None;
    }
    percent_decode(encoded)
}

fn percent_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '%' {
            let high = characters.next()?.to_digit(16)?;
            let low = characters.next()?.to_digit(16)?;
            bytes.push(((high << 4) | low) as u8);
        } else {
            let mut encoded = [0; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
        }
    }
    String::from_utf8(bytes).ok()
}

fn safe_leaf(filename: &str) -> Option<String> {
    let leaf = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    let leaf = leaf.trim().trim_matches('.').trim();
    (!leaf.is_empty()).then(|| leaf.to_owned())
}

fn filename_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let leaf = path.rsplit('/').find(|part| !part.is_empty())?;
    safe_leaf(&percent_decode(leaf).unwrap_or_else(|| leaf.to_owned()))
}

/// What the downloads surface reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing.
    None,
    /// Forget completed attachments.
    Clear,
    /// Choose where to save this attachment.
    Save(DownloadId),
    /// Leave the surface.
    Close,
}

const HEADER_HEIGHT: f64 = 52.0;
const CONTENT_WIDTH: f64 = 680.0;

#[derive(Clone, PartialEq)]
struct Drawn {
    rect: Rect,
    revision: u64,
    scroll: f64,
    pointer: (f64, f64),
    focus: Option<FocusId>,
}

/// The browser-owned list of completed attachments.
pub struct DownloadsSurface {
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

impl Default for DownloadsSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl DownloadsSurface {
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

    /// What the last frame drew, for accessibility.
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
            Action::Clear | Action::Save(_) | Action::Close => otlyra_platform::Cursor::Pointer,
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
        store: &DownloadStore,
        text: &mut TextEngine,
        out: &mut DisplayList,
    ) {
        let drawn = Drawn {
            rect,
            revision: store.revision(),
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

        self.builds += 1;
        let mut built = DisplayList::new();
        let theme = self.theme.clone();
        fill_rounded(&mut built, rect, theme.surface_sunken, 0.0);

        let mut cx = Cx::new(text);
        cx.pointer = self.pointer;
        cx.focus = self.focused;
        cx.theme = theme.clone();

        self.focus.begin();
        let mut root = self.build(&theme, rect.width, store, &self.focus);
        root.measure(Size::new(rect.width, rect.height), &mut cx);
        root.place(rect, &mut cx);
        root.draw(&mut cx, &mut built);
        self.scroll = self.scroll.clamp(0.0, self.overflow.get());

        self.root = Some(root);
        self.cache = Some((drawn, built));
        let (_, built) = self.cache.as_ref().expect("just stored");
        out.append(built);
    }

    fn build(
        &self,
        theme: &Theme,
        width: f64,
        store: &DownloadStore,
        focus: &Focus,
    ) -> Child<Action> {
        let mut rows: Vec<Child<Action>> = store
            .downloads()
            .map(|download| self.download_row(theme, focus, download))
            .collect();
        if rows.is_empty() {
            rows.push(Box::new(Padding::new(
                Insets::all(theme.inset * 2.0),
                Box::new(Align::centre(Box::new(Label::new(
                    "No downloads in this session.",
                    theme.font_size,
                    theme.ink_dim,
                )))),
            )));
        } else {
            rows = vec![controls::card_plain(theme, rows)];
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
                self.header(theme, focus, store.downloads().next().is_some()),
                Box::new(Scroll::new(self.scroll, Rc::clone(&self.overflow), centred)),
            ],
        ))
    }

    fn header(&self, theme: &Theme, focus: &Focus, has_downloads: bool) -> Child<Action> {
        let title: Child<Action> = Box::new(Align::left(Box::new(Label::new(
            "Downloads",
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
                                "Clear",
                                Emphasis::Danger,
                                has_downloads,
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

    fn download_row(&self, theme: &Theme, focus: &Focus, download: &Download) -> Child<Action> {
        let kind = download.content_type().unwrap_or("Unknown type");
        let (detail, may_be_long) = if let Some(path) = download.saving_to() {
            (format!("Saving to {path}…"), true)
        } else if let Some(error) = download.save_error() {
            (format!("Save failed: {error}"), true)
        } else if let Some(path) = download.saved_to() {
            (format!("Saved to {path}"), true)
        } else {
            (
                format!("{kind} · {}", format_bytes(download.bytes().len())),
                false,
            )
        };
        let name: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            download.filename().to_owned(),
            theme.font_size,
            theme.ink,
            Elide::End,
        ))));
        let source: Child<Action> = Box::new(Align::left(Box::new(Elided::new(
            download.url().to_owned(),
            theme.font_size_small,
            theme.ink_dim,
            Elide::End,
        ))));
        let detail: Child<Action> = if may_be_long {
            Box::new(Align::left(Box::new(Elided::new(
                detail,
                theme.font_size_small,
                theme.ink_dim,
                Elide::End,
            ))))
        } else {
            Box::new(Align::left(Box::new(Label::new(
                detail,
                theme.font_size_small,
                theme.ink_dim,
            ))))
        };
        let labels: Child<Action> = Box::new(Flex::new(
            1.0,
            Box::new(Stack::column(theme.gap * 0.5, vec![name, source, detail])),
        ));
        Box::new(Padding::new(
            Insets::symmetric(theme.inset, theme.gap),
            Box::new(Stack::row(
                theme.inset,
                vec![
                    labels,
                    Box::new(Align::centre(controls::button(
                        theme,
                        focus,
                        Action::Save(download.id()),
                        "Save As…",
                        Emphasis::Normal,
                        download.saving_to().is_none(),
                    ))),
                ],
            )),
        ))
    }
}

fn format_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    if bytes >= MIB as usize {
        format!("{:.1} MiB", bytes as f64 / MIB)
    } else if bytes >= KIB as usize {
        format!("{:.1} KiB", bytes as f64 / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_names_are_case_insensitive_and_safe() {
        let headers = vec![(
            "Content-Disposition".to_owned(),
            "Attachment; filename=\"../report.csv\"".to_owned(),
        )];
        assert_eq!(
            attachment_filename(&headers, "https://example.test/export"),
            Some("report.csv".to_owned())
        );
    }

    #[test]
    fn utf8_extended_names_win_over_plain_names() {
        let headers = vec![(
            "content-disposition".to_owned(),
            "attachment; filename=plain.txt; filename*=UTF-8''%D0%BE%D1%82%D1%87%D1%91%D1%82.txt"
                .to_owned(),
        )];
        assert_eq!(
            attachment_filename(&headers, "https://example.test/export"),
            Some("отчёт.txt".to_owned())
        );
    }

    #[test]
    fn inline_content_is_not_a_download() {
        let headers = vec![(
            "content-disposition".to_owned(),
            "inline; filename=page.html".to_owned(),
        )];
        assert_eq!(
            attachment_filename(&headers, "https://example.test/page"),
            None
        );
    }

    #[test]
    fn an_attachment_without_a_name_uses_its_url() {
        let headers = vec![("content-disposition".to_owned(), "attachment".to_owned())];
        assert_eq!(
            attachment_filename(
                &headers,
                "https://example.test/files/data%20set.bin?token=x"
            ),
            Some("data set.bin".to_owned())
        );
    }

    #[test]
    fn the_store_lists_newest_first_and_clear_releases_it() {
        let mut store = DownloadStore::default();
        store.record("one.txt", "https://example.test/one", None, vec![1]);
        store.record(
            "two.txt",
            "https://example.test/two",
            Some("text/plain".to_owned()),
            vec![2, 3],
        );
        let names = store
            .downloads()
            .map(Download::filename)
            .collect::<Vec<_>>();
        assert_eq!(names, ["two.txt", "one.txt"]);
        let newest = store.downloads().next().expect("the newest download").id();
        store.mark_saved(newest, "/tmp/two.txt");
        assert_eq!(
            store.get(newest).and_then(Download::saved_to),
            Some("/tmp/two.txt")
        );
        let before = store.revision();
        store.clear();
        assert!(store.downloads().next().is_none());
        assert!(store.revision() > before);
    }

    #[test]
    fn an_unchanged_surface_reuses_its_display_list() {
        let mut store = DownloadStore::default();
        store.record("one.txt", "https://example.test/one", None, vec![1]);
        let mut surface = DownloadsSurface::new();
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        let rect = Rect::new(0.0, 0.0, 900.0, 700.0);
        surface.build_display_list(rect, &store, &mut text, &mut list);
        surface.build_display_list(rect, &store, &mut text, &mut list);
        assert_eq!(surface.builds(), 1);
    }

    #[test]
    fn a_download_row_offers_save_as() {
        let mut store = DownloadStore::default();
        store.record("one.txt", "https://example.test/one", None, vec![1]);
        let wanted = store.downloads().next().expect("a download").id();
        let mut surface = DownloadsSurface::new();
        let mut text = TextEngine::new();
        let mut list = DisplayList::new();
        surface.build_display_list(
            Rect::new(0.0, 0.0, 900.0, 700.0),
            &store,
            &mut text,
            &mut list,
        );

        let found = (0..700).step_by(4).any(|y| {
            (0..900).step_by(8).any(|x| {
                surface.action_at(f64::from(x), f64::from(y), &mut text) == Action::Save(wanted)
            })
        });
        assert!(found, "the Save As button was not reachable");
    }

    #[test]
    fn the_writer_finishes_on_the_tokio_runtime() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time moves forward")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "otlyra-download-writer-{}-{unique}",
            std::process::id()
        ));
        let writer = DownloadWriter::new();
        writer.save(
            DownloadId(7),
            path.clone(),
            Arc::<[u8]>::from(&b"async bytes"[..]),
        );

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let saved = loop {
            if let Some(saved) = writer.poll().into_iter().next() {
                break saved;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the asynchronous write never completed"
            );
            std::thread::yield_now();
        };
        assert_eq!(saved.id, DownloadId(7));
        assert!(saved.result.is_ok(), "{:?}", saved.result);
        assert_eq!(
            std::fs::read(&path).expect("the saved file"),
            b"async bytes"
        );
        std::fs::remove_file(path).expect("remove the test-owned file");
    }
}
