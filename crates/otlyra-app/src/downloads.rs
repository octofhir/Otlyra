//! `about:downloads` and the session's completed downloads.
//!
//! A response carrying `Content-Disposition: attachment` is not a document. The
//! browser keeps its bytes here instead of feeding them to the HTML parser, and
//! this surface shows the result without depending on the document engine.
//!
//! A retained attachment reaches the disk two ways, and the preference decides
//! which: the native Save As dialog names one exact path, or the download
//! directory takes it under a name that is not already spoken for. Both go
//! through one writer, and the writer's rules are the interesting part:
//!
//! - the bytes land in a `<name>.otlyra-part` beside the destination and are
//!   renamed onto it only once they are all there, so a half-written file never
//!   looks like a finished one — and the rename is within one directory, so it is
//!   the atomic operation the platform promises rather than a copy;
//! - that part file is also the *reservation*. Two downloads of `report.csv`
//!   arriving together cannot both pick `report (1).csv`, because claiming a name
//!   means creating its part file exclusively, and the loser moves on to the next
//!   number. A check for whether the final name is free cannot do that on its own;
//! - a failed write leaves nothing behind and says why, and the row offers to try
//!   again.

use std::path::{Path, PathBuf};
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
    /// The exact path the last attempt was aimed at, when a person named one.
    ///
    /// `None` means it went to the download directory, and trying again asks the
    /// preference afresh rather than reusing a directory the reader may have
    /// changed *because* the write failed.
    chosen: Option<PathBuf>,
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

    /// Where trying again should write, given what the last attempt aimed at.
    pub(crate) fn retry_destination(&self, directory: PathBuf) -> Destination {
        match self.chosen.clone() {
            Some(path) => Destination::Exact(path),
            None => Destination::Into {
                directory,
                filename: self.filename.clone(),
            },
        }
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
    /// Returns the identity the row was given, so an automatic save can start on
    /// it without searching the list for what was just added.
    pub fn record(
        &mut self,
        filename: impl Into<String>,
        url: impl Into<String>,
        content_type: Option<String>,
        bytes: Vec<u8>,
    ) -> Option<DownloadId> {
        let incoming = bytes.len();
        while self.bytes + incoming > BYTE_BUDGET && !self.entries.is_empty() {
            let removed = self.entries.remove(0);
            self.bytes -= removed.bytes.len();
        }

        // The network limit is below the store budget in production. Keeping
        // this guard makes the store safe when used with a custom Loader.
        if incoming > BYTE_BUDGET {
            return None;
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
            chosen: None,
        });
        self.revision += 1;
        Some(id)
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
    ///
    /// `destination` is what the row says while the write runs, and `chosen` is
    /// the exact path to try again at — `None` for a write into the download
    /// directory, whose final name the writer has not settled yet.
    pub fn mark_saving(
        &mut self,
        id: DownloadId,
        destination: impl Into<String>,
        chosen: Option<PathBuf>,
    ) {
        let Some(download) = self.entries.iter_mut().find(|download| download.id == id) else {
            return;
        };
        download.saving_to = Some(destination.into());
        download.chosen = chosen;
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
    /// Where it ended up, or the reason it did not.
    ///
    /// The path is in the answer rather than beside it because a write into the
    /// download directory does not know its own name until it has claimed one:
    /// the reader asked for a directory, and which of `report.csv` and
    /// `report (1).csv` they got is something only the writer can report.
    pub result: Result<PathBuf, String>,
}

/// Where a completed attachment should be written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// This exact path, replacing whatever is there.
    ///
    /// What the Save As dialog promised: the platform's own dialogue has already
    /// asked about replacing a file, so asking again here would be asking twice.
    Exact(PathBuf),
    /// Inside this directory, under `filename` or the first free number after it.
    Into {
        /// The directory to write into, created if it is not there.
        directory: PathBuf,
        /// The name to aim for. Reduced to a leaf again before it is used, so a
        /// server that answered `../../etc/passwd` cannot name a path.
        filename: String,
    },
}

/// How many numbered copies of one name the writer will try before giving up.
///
/// A bound rather than a loop, because the alternative is a directory that
/// somehow answers "taken" forever and a browser that stops responding while it
/// asks. A thousand copies of one file is someone else's problem to explain.
const MOST_COLLISIONS: usize = 999;

/// What an unfinished download is called while it is being written.
const PART_SUFFIX: &str = ".otlyra-part";

/// The part file that stands in for `destination` until the bytes are all there.
///
/// A sibling, deliberately: the finishing move is a rename, and a rename is only
/// the atomic operation the platform promises when both names are in the same
/// directory. Across filesystems it becomes a copy, which is the very thing this
/// is here to avoid.
fn part_path(destination: &Path) -> PathBuf {
    let mut name = destination.file_name().unwrap_or_default().to_os_string();
    name.push(PART_SUFFIX);
    destination.with_file_name(name)
}

/// `report.csv` numbered: `report.csv`, `report (1).csv`, `report (2).csv`.
///
/// Split at the last dot, which is what the platform means by an extension, so
/// `archive.tar.gz` becomes `archive.tar (1).gz`. Not what every browser does, and
/// the alternative is a list of compound extensions to keep current forever.
fn indexed_name(filename: &str, index: usize) -> String {
    if index == 0 {
        return filename.to_owned();
    }
    let path = Path::new(filename);
    match (path.file_stem(), path.extension()) {
        (Some(stem), Some(extension)) if !stem.is_empty() => format!(
            "{} ({index}).{}",
            stem.to_string_lossy(),
            extension.to_string_lossy()
        ),
        _ => format!("{filename} ({index})"),
    }
}

/// Where downloads go when nothing has said otherwise.
///
/// The platform's own Downloads folder, asked of the platform rather than guessed
/// at. `preferences::path` works its own directory out by hand and says why — a
/// configuration directory is three stable cases — and this one is deliberately
/// not that: on Linux it is whatever `~/.config/user-dirs.dirs` names, which may
/// be localized, relative, or missing, and on Windows it is a Known Folder the
/// reader may have moved and that no environment variable reports. `$HOME/Downloads`
/// is right on macOS and a wrong answer for anyone who has moved theirs.
///
/// `None` means the platform would not say, which is a browser that has to ask
/// before it can save anything — see [`crate::settings::Settings::asks_where_to_save`].
pub fn default_directory() -> Option<PathBuf> {
    // The same escape hatch the preferences file has, and for the same reason: a
    // person running the browser by hand must be able to keep it out of their own
    // Downloads folder. Read before the platform is asked, so it wins.
    if let Some(directory) = std::env::var_os("OTLYRA_DOWNLOAD_DIR") {
        return Some(PathBuf::from(directory));
    }
    dirs::download_dir()
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

    /// Start writing `bytes` to `destination` and return immediately.
    pub fn save(&self, id: DownloadId, destination: Destination, bytes: Arc<[u8]>) {
        let sender = self.sender.clone();
        let waker = Arc::clone(&self.waker);
        crate::io::shared().spawn(async move {
            let result = write(destination, bytes).await;
            let _ = sender.send(SaveResult { id, result });
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

/// Write `bytes` where `destination` says, and answer with where they went.
///
/// Through a part file in both cases. A reader who watches the folder while a
/// large file arrives sees `report.csv.otlyra-part` growing and then `report.csv`
/// appearing whole — never a `report.csv` that opens to half a document.
async fn write(destination: Destination, bytes: Arc<[u8]>) -> Result<PathBuf, String> {
    let (path, part) = match destination {
        Destination::Exact(path) => {
            if let Some(directory) = path.parent() {
                ensure_directory(directory).await?;
            }
            // No reservation here: the reader named this path through a dialogue
            // that already asked about replacing it, and a stale part file left by
            // a crash is not a reason to refuse the write.
            let part = part_path(&path);
            (path, part)
        }
        Destination::Into {
            directory,
            filename,
        } => {
            ensure_directory(&directory).await?;
            claim(&directory, &filename).await?
        }
    };

    let written = async {
        tokio::fs::write(&part, bytes.as_ref())
            .await
            .map_err(|error| format!("{}: {error}", part.display()))?;
        tokio::fs::rename(&part, &path)
            .await
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Ok(path)
    }
    .await;

    if written.is_err() {
        // Whatever was written is not a download and must not look like the start
        // of one. Best effort: if this fails too there is nothing further to say.
        let _ = tokio::fs::remove_file(&part).await;
    }
    written
}

/// Make `directory` if it is not there.
async fn ensure_directory(directory: &Path) -> Result<(), String> {
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|error| format!("{}: {error}", directory.display()))
}

/// Claim a free name for `filename` in `directory`, returning it and its part file.
///
/// Claiming is creating the part file exclusively, not finding a free name: two
/// downloads called `report.csv` that arrive together both see `report (1).csv`
/// free, and only one of them can create `report (1).csv.otlyra-part`. The other
/// is told the name is taken and moves on, which is a check no amount of looking
/// could have made safe.
async fn claim(directory: &Path, filename: &str) -> Result<(PathBuf, PathBuf), String> {
    // Reduced to a leaf again rather than trusted. The name reached here through
    // `attachment_filename`, which already does this — but this function joins a
    // path with a server's string, and a second guard at the join is cheaper than
    // being sure about every route into it.
    let filename = safe_leaf(filename).unwrap_or_else(|| "download".to_owned());
    let mut refused = None;
    for index in 0..=MOST_COLLISIONS {
        let candidate = directory.join(indexed_name(&filename, index));
        // An error rather than a `false` means the directory will not say, and
        // treating "cannot tell" as free is how a file gets overwritten.
        if tokio::fs::try_exists(&candidate).await.unwrap_or(true) {
            continue;
        }
        let part = part_path(&candidate);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await
        {
            Ok(_file) => return Ok((candidate, part)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                refused = Some(format!("{}: {error}", part.display()));
                break;
            }
        }
    }
    Err(refused.unwrap_or_else(|| {
        format!(
            "{}: {filename} and {MOST_COLLISIONS} numbered copies of it are all taken",
            directory.display()
        )
    }))
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
    /// Write this attachment again, wherever the failed attempt was aimed.
    Retry(DownloadId),
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
            Action::Clear | Action::Save(_) | Action::Retry(_) | Action::Close => {
                otlyra_platform::Cursor::Pointer
            }
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
        let idle = download.saving_to().is_none();
        let mut buttons: Vec<Child<Action>> = vec![labels];
        // Only after a failure, and before Save As…: a write that did not happen
        // is the one thing on the row worth pressing, and "try that again" is a
        // shorter answer than "choose somewhere else".
        if download.save_error().is_some() {
            buttons.push(Box::new(Align::centre(controls::button(
                theme,
                focus,
                Action::Retry(download.id()),
                "Retry",
                Emphasis::Primary,
                idle,
            ))));
        }
        buttons.push(Box::new(Align::centre(controls::button(
            theme,
            focus,
            Action::Save(download.id()),
            "Save As…",
            Emphasis::Normal,
            idle,
        ))));
        Box::new(Padding::new(
            Insets::symmetric(theme.inset, theme.gap),
            Box::new(Stack::row(theme.inset, buttons)),
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

    /// A directory of this test's own, gone when the test is.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time moves forward")
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("otlyra-{tag}-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("a scratch directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn payload(bytes: &[u8]) -> Arc<[u8]> {
        Arc::<[u8]>::from(bytes)
    }

    /// Wait for the next write to finish, or fail rather than hang.
    fn settled(writer: &DownloadWriter) -> SaveResult {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(saved) = writer.poll().into_iter().next() {
                return saved;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the asynchronous write never completed"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn the_writer_finishes_on_the_tokio_runtime() {
        let scratch = Scratch::new("download-writer");
        let path = scratch.path().join("saved.bin");
        let writer = DownloadWriter::new();
        writer.save(
            DownloadId(7),
            Destination::Exact(path.clone()),
            payload(b"async bytes"),
        );

        let saved = settled(&writer);
        assert_eq!(saved.id, DownloadId(7));
        assert_eq!(saved.result.as_deref().ok(), Some(path.as_path()));
        assert_eq!(
            std::fs::read(&path).expect("the saved file"),
            b"async bytes"
        );
    }

    /// The part file is a means, not a leftover: once the bytes are all there the
    /// only thing in the folder is the download.
    #[test]
    fn a_finished_write_leaves_only_the_file_it_promised() {
        let scratch = Scratch::new("download-part");
        let writer = DownloadWriter::new();
        writer.save(
            DownloadId(1),
            Destination::Into {
                directory: scratch.path().to_owned(),
                filename: "report.csv".to_owned(),
            },
            payload(b"a,b\n1,2\n"),
        );
        let saved = settled(&writer).result.expect("the write");

        assert_eq!(saved, scratch.path().join("report.csv"));
        let listed: Vec<String> = std::fs::read_dir(scratch.path())
            .expect("the scratch directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(listed, ["report.csv"]);
    }

    /// Two downloads of one name are two files, and the second is numbered rather
    /// than written over the first.
    #[test]
    fn a_name_that_is_taken_is_numbered_rather_than_replaced() {
        let scratch = Scratch::new("download-collision");
        let writer = DownloadWriter::new();
        let into = || Destination::Into {
            directory: scratch.path().to_owned(),
            filename: "report.csv".to_owned(),
        };

        writer.save(DownloadId(1), into(), payload(b"first"));
        let first = settled(&writer).result.expect("the first write");
        writer.save(DownloadId(2), into(), payload(b"second"));
        let second = settled(&writer).result.expect("the second write");

        assert_eq!(first, scratch.path().join("report.csv"));
        assert_eq!(second, scratch.path().join("report (1).csv"));
        assert_eq!(std::fs::read(&first).expect("the first file"), b"first");
        assert_eq!(std::fs::read(&second).expect("the second file"), b"second");
    }

    /// A write that cannot happen says why and leaves nothing that looks like a
    /// download in progress.
    #[test]
    fn a_failed_write_leaves_nothing_behind() {
        let scratch = Scratch::new("download-failure");
        // A file where the directory should be: `create_dir_all` cannot make one,
        // which is the same shape as a folder the reader cannot write to and is a
        // failure this test can cause on any platform.
        let blocked = scratch.path().join("not-a-directory");
        std::fs::write(&blocked, b"in the way").expect("the blocking file");

        let writer = DownloadWriter::new();
        writer.save(
            DownloadId(3),
            Destination::Into {
                directory: blocked.clone(),
                filename: "report.csv".to_owned(),
            },
            payload(b"never written"),
        );
        let error = settled(&writer).result.expect_err("the write must fail");

        assert!(
            error.contains("not-a-directory"),
            "the reason must name where it was going: {error}"
        );
        assert_eq!(
            std::fs::read(&blocked).expect("the blocking file"),
            b"in the way",
            "the failed write must not have touched anything"
        );
    }

    #[test]
    fn a_numbered_name_keeps_the_extension_where_it_can() {
        assert_eq!(indexed_name("report.csv", 0), "report.csv");
        assert_eq!(indexed_name("report.csv", 2), "report (2).csv");
        assert_eq!(indexed_name("README", 1), "README (1)");
        // The last dot is what the platform calls an extension, so a compound one
        // splits where the platform splits it rather than where a person would.
        assert_eq!(indexed_name("archive.tar.gz", 1), "archive.tar (1).gz");
        assert_eq!(indexed_name(".bashrc", 1), ".bashrc (1)");
    }

    /// Trying again goes back where the reader pointed, and asks the preference
    /// again when they never pointed anywhere.
    #[test]
    fn retrying_reuses_a_chosen_path_and_not_a_chosen_directory() {
        let mut store = DownloadStore::default();
        let id = store
            .record("one.txt", "https://example.test/one", None, vec![1])
            .expect("the download was recorded");
        let directory = PathBuf::from("/tmp/otlyra-downloads");

        store.mark_saving(
            id,
            "/elsewhere/one.txt",
            Some(PathBuf::from("/elsewhere/one.txt")),
        );
        store.mark_save_failed(id, "read-only file system");
        assert_eq!(
            store
                .get(id)
                .expect("the download")
                .retry_destination(directory.clone()),
            Destination::Exact(PathBuf::from("/elsewhere/one.txt"))
        );

        store.mark_saving(id, directory.to_string_lossy().into_owned(), None);
        store.mark_save_failed(id, "read-only file system");
        assert_eq!(
            store
                .get(id)
                .expect("the download")
                .retry_destination(directory.clone()),
            Destination::Into {
                directory,
                filename: "one.txt".to_owned(),
            }
        );
    }

    /// A row that failed offers the press that matters, and one that did not does
    /// not offer it at all.
    #[test]
    fn only_a_failed_row_offers_retry() {
        let mut store = DownloadStore::default();
        let id = store
            .record("one.txt", "https://example.test/one", None, vec![1])
            .expect("the download was recorded");
        let mut surface = DownloadsSurface::new();
        let mut text = TextEngine::new();
        let rect = Rect::new(0.0, 0.0, 900.0, 700.0);

        let reachable = |surface: &mut DownloadsSurface,
                         store: &DownloadStore,
                         text: &mut TextEngine,
                         wanted: Action| {
            let mut list = DisplayList::new();
            surface.build_display_list(rect, store, text, &mut list);
            (0..700).step_by(4).any(|y| {
                (0..900)
                    .step_by(8)
                    .any(|x| surface.action_at(f64::from(x), f64::from(y), text) == wanted)
            })
        };

        assert!(
            !reachable(&mut surface, &store, &mut text, Action::Retry(id)),
            "a download that has not failed must not offer Retry"
        );
        store.mark_save_failed(id, "read-only file system");
        assert!(
            reachable(&mut surface, &store, &mut text, Action::Retry(id)),
            "a download that failed must offer Retry"
        );
    }

    /// There is somewhere to put a download without anyone naming one.
    ///
    /// Read rather than written: the environment is process-wide and the tests in
    /// this binary run at once, so a test that set a variable would be racing every
    /// other test that reads one. The override exists for a person running the
    /// browser by hand, and is exercised there.
    #[test]
    fn the_download_folder_has_a_default() {
        assert!(default_directory().is_some_and(|path| path.ends_with("Downloads")));
    }
}
