//! The clipboard, behind a seam a test can reach.
//!
//! The same lesson the preferences taught: a constructor that touches the
//! machine makes every test depend on the machine. A browser in a test copies
//! into memory; the binary swaps in the system pasteboard at startup.

/// Where cut and copy put text, and where paste takes it from.
pub trait Clipboard {
    /// What is on the clipboard, if it is text.
    fn read(&mut self) -> Option<String>;
    /// Put `text` on the clipboard.
    fn write(&mut self, text: String);
}

/// A clipboard in memory: the default, and what every test copies into.
#[derive(Default)]
pub struct InMemory(Option<String>);

impl Clipboard for InMemory {
    fn read(&mut self) -> Option<String> {
        self.0.clone()
    }

    fn write(&mut self, text: String) {
        self.0 = Some(text);
    }
}

/// The platform's clipboard.
///
/// Absent rather than fatal when the platform refuses one — a headless session
/// has no pasteboard, and a browser that cannot copy still browses.
pub struct System(Option<arboard::Clipboard>);

impl System {
    /// Connect to the system clipboard, or to nothing if the platform has none.
    pub fn new() -> Self {
        Self(arboard::Clipboard::new().ok())
    }
}

impl Default for System {
    fn default() -> Self {
        Self::new()
    }
}

impl Clipboard for System {
    fn read(&mut self) -> Option<String> {
        self.0.as_mut()?.get_text().ok()
    }

    fn write(&mut self, text: String) {
        if let Some(clipboard) = self.0.as_mut() {
            let _ = clipboard.set_text(text);
        }
    }
}
