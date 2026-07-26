//! The browser's menu bar.
//!
//! Two halves: [`Command`], which is what the browser can be asked to do, and
//! [`menu_bar`], which is where those commands appear and what key presses reach
//! them. Keeping them separate means a command can be invoked from a menu, a
//! shortcut, a toolbar button or a test without three definitions of what it means.

use otlyra_platform::{Menu, MenuBar, MenuEntry, MenuId, SystemItem};

/// Something the browser can be asked to do.
///
/// Every variant is a command a user can name. Anything that is not a user-facing
/// action does not belong here.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Command {
    /// Open a new tab.
    NewTab,
    /// Open a new window.
    NewWindow,
    /// Open a file from disk.
    OpenFile,
    /// Close the current tab.
    CloseTab,
    /// Reload the current page.
    Reload,
    /// Reload, bypassing the cache.
    ReloadIgnoringCache,
    /// Stop loading.
    Stop,
    /// Go back in session history.
    Back,
    /// Go forward in session history.
    Forward,
    /// Go to the home page.
    Home,
    /// Open the browser settings.
    Settings,
    /// Open the complete browsing history.
    ShowHistory,
    /// Open completed downloads.
    ShowDownloads,
    /// Keep the page the reader is on, or stop keeping it.
    ToggleBookmark,
    /// Open what the reader kept.
    ShowBookmarks,
    /// Increase the page zoom.
    ZoomIn,
    /// Decrease the page zoom.
    ZoomOut,
    /// Reset the page zoom.
    ActualSize,
    /// Show the page's source.
    ViewSource,
    /// Toggle the developer tools.
    ToggleDevTools,
    /// Cut what is selected.
    Cut,
    /// Copy it.
    Copy,
    /// Paste over it.
    Paste,
    /// Select everything the keyboard is in.
    SelectAll,
}

impl Command {
    /// Every command, in a stable order.
    pub const ALL: &'static [Self] = &[
        Self::NewTab,
        Self::NewWindow,
        Self::OpenFile,
        Self::CloseTab,
        Self::Reload,
        Self::ReloadIgnoringCache,
        Self::Stop,
        Self::Back,
        Self::Forward,
        Self::Home,
        Self::Settings,
        Self::ShowHistory,
        Self::ShowDownloads,
        Self::ToggleBookmark,
        Self::ShowBookmarks,
        Self::ZoomIn,
        Self::ZoomOut,
        Self::ActualSize,
        Self::ViewSource,
        Self::ToggleDevTools,
        Self::Cut,
        Self::Copy,
        Self::Paste,
        Self::SelectAll,
    ];

    /// The id this command travels under across the platform boundary.
    ///
    /// Derived from the position in [`Command::ALL`], so adding a command in the
    /// middle renumbers the rest. That is fine: ids are never persisted, only used
    /// within one run.
    pub fn id(self) -> MenuId {
        let index = Self::ALL
            .iter()
            .position(|command| *command == self)
            .expect("every command is listed in Command::ALL");
        MenuId(index as u32)
    }

    /// Recover a command from the id the platform reported.
    pub fn from_id(id: MenuId) -> Option<Self> {
        Self::ALL.get(id.0 as usize).copied()
    }

    /// Whether the command is implemented yet.
    ///
    /// The ones that are not are shown greyed out rather than hidden: the menu is
    /// then the shape of the browser, which is more honest than an empty menu bar
    /// and more discoverable than nothing at all.
    pub fn is_available(self) -> bool {
        matches!(
            self,
            Self::NewTab
                | Self::CloseTab
                | Self::Reload
                | Self::ReloadIgnoringCache
                | Self::Stop
                | Self::Back
                | Self::Forward
                | Self::Home
                | Self::Settings
                | Self::ShowHistory
                | Self::ShowDownloads
                | Self::ToggleBookmark
                | Self::ShowBookmarks
                | Self::ToggleDevTools
                | Self::ZoomIn
                | Self::ZoomOut
                | Self::ActualSize
                | Self::Cut
                | Self::Copy
                | Self::Paste
                | Self::SelectAll
        )
    }
}

/// Build the menu bar.
pub fn menu_bar() -> MenuBar {
    MenuBar::new()
        // The application menu. Its title is replaced by the process name on macOS,
        // so what is written here only matters on platforms that show it.
        .with(Menu::new(
            "Otlyra",
            vec![
                MenuEntry::System(SystemItem::About),
                MenuEntry::Separator,
                entry(Command::Settings, "Settings…", Some("CmdOrCtrl+Comma")),
                MenuEntry::Separator,
                MenuEntry::System(SystemItem::Services),
                MenuEntry::Separator,
                MenuEntry::System(SystemItem::Hide),
                MenuEntry::System(SystemItem::HideOthers),
                MenuEntry::System(SystemItem::ShowAll),
                MenuEntry::Separator,
                MenuEntry::System(SystemItem::Quit),
            ],
        ))
        .with(Menu::new(
            "File",
            vec![
                entry(Command::NewTab, "New Tab", Some("CmdOrCtrl+KeyT")),
                entry(Command::NewWindow, "New Window", Some("CmdOrCtrl+KeyN")),
                MenuEntry::Separator,
                entry(Command::OpenFile, "Open File…", Some("CmdOrCtrl+KeyO")),
                MenuEntry::Separator,
                entry(Command::CloseTab, "Close Tab", Some("CmdOrCtrl+KeyW")),
                MenuEntry::System(SystemItem::CloseWindow),
            ],
        ))
        .with(Menu::new(
            "Edit",
            vec![
                MenuEntry::System(SystemItem::Undo),
                MenuEntry::System(SystemItem::Redo),
                MenuEntry::Separator,
                // Ours rather than the platform's four. A predefined Cut, Copy,
                // Paste or Select All on macOS *is* the keystroke: the item owns
                // ⌘C and sends `copy:` down the responder chain, where nothing of
                // ours answers — so the key never arrives at the window and
                // copying out of the address field did nothing at all, however
                // right everything downstream of the keystroke was.
                entry(Command::Cut, "Cut", Some("CmdOrCtrl+KeyX")),
                entry(Command::Copy, "Copy", Some("CmdOrCtrl+KeyC")),
                entry(Command::Paste, "Paste", Some("CmdOrCtrl+KeyV")),
                entry(Command::SelectAll, "Select All", Some("CmdOrCtrl+KeyA")),
            ],
        ))
        .with(Menu::new(
            "View",
            vec![
                entry(Command::Reload, "Reload", Some("CmdOrCtrl+KeyR")),
                entry(
                    Command::ReloadIgnoringCache,
                    "Force Reload",
                    Some("CmdOrCtrl+Shift+KeyR"),
                ),
                entry(Command::Stop, "Stop", Some("CmdOrCtrl+Period")),
                MenuEntry::Separator,
                entry(Command::ActualSize, "Actual Size", Some("CmdOrCtrl+Digit0")),
                entry(Command::ZoomIn, "Zoom In", Some("CmdOrCtrl+Equal")),
                entry(Command::ZoomOut, "Zoom Out", Some("CmdOrCtrl+Minus")),
                MenuEntry::Separator,
                MenuEntry::System(SystemItem::Fullscreen),
            ],
        ))
        .with(Menu::new(
            "History",
            vec![
                entry(Command::Back, "Back", Some("CmdOrCtrl+BracketLeft")),
                entry(Command::Forward, "Forward", Some("CmdOrCtrl+BracketRight")),
                MenuEntry::Separator,
                entry(Command::Home, "Home", Some("CmdOrCtrl+Shift+KeyH")),
                entry(
                    Command::ShowHistory,
                    "Show All History",
                    Some("CmdOrCtrl+KeyY"),
                ),
                entry(
                    Command::ShowDownloads,
                    "Show Downloads",
                    Some("CmdOrCtrl+Shift+KeyJ"),
                ),
            ],
        ))
        // Between History and Develop, where every browser puts it.
        .with(Menu::new(
            "Bookmarks",
            vec![
                // One item for both directions. The label says what pressing it on
                // a page nobody kept does, which is the common case; the item
                // cannot yet be relabelled while the browser runs, because the
                // native bar is built once at startup. The browser's own menu is
                // rebuilt every frame and says which of the two it will do.
                entry(
                    Command::ToggleBookmark,
                    "Bookmark This Page",
                    Some("CmdOrCtrl+KeyD"),
                ),
                MenuEntry::Separator,
                entry(
                    Command::ShowBookmarks,
                    "Show Bookmarks",
                    Some("CmdOrCtrl+Alt+KeyB"),
                ),
            ],
        ))
        .with(Menu::new(
            "Develop",
            vec![
                entry(Command::ViewSource, "View Source", Some("CmdOrCtrl+KeyU")),
                entry(
                    Command::ToggleDevTools,
                    "Developer Tools",
                    Some("CmdOrCtrl+Alt+KeyI"),
                ),
            ],
        ))
        .with(Menu::new(
            "Window",
            vec![
                MenuEntry::System(SystemItem::Minimize),
                MenuEntry::System(SystemItem::Maximize),
                MenuEntry::Separator,
                MenuEntry::System(SystemItem::BringAllToFront),
            ],
        ))
}

fn entry(command: Command, label: &str, accelerator: Option<&str>) -> MenuEntry {
    if command.is_available() {
        match accelerator {
            Some(accelerator) => MenuEntry::item(command.id(), label, accelerator),
            None => MenuEntry::plain(command.id(), label),
        }
    } else {
        MenuEntry::disabled(command.id(), label, accelerator)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accelerators are strings, so a typo would otherwise be a silently missing
    /// shortcut at runtime. This turns it into a failing build.
    #[test]
    fn the_menu_bar_is_well_formed() {
        menu_bar()
            .validate()
            .expect("every accelerator parses and ids are unique");
    }

    #[test]
    fn command_ids_round_trip() {
        for command in Command::ALL {
            assert_eq!(Command::from_id(command.id()), Some(*command));
        }
    }

    /// Cut, Copy, Paste and Select All are ours, not the platform's.
    ///
    /// A predefined one on macOS *is* the keystroke: the item owns ⌘C and sends
    /// `copy:` down the responder chain, where nothing of ours answers. The key
    /// never reaches the window, so copying out of the address field did nothing
    /// — and every test passed, because a test delivers the key press itself.
    #[test]
    fn the_editing_commands_are_ours_so_their_keys_reach_the_window() {
        let bar = format!("{:?}", menu_bar());
        for name in ["Cut", "Copy", "Paste", "Select All"] {
            assert!(bar.contains(name), "{name} is not on the menu: {bar}");
        }
        for taken in ["Cut)", "Copy)", "Paste)", "SelectAll)"] {
            assert!(
                !bar.contains(&format!("System({taken}")),
                "{taken} is still the platform's, so its key never arrives"
            );
        }
    }

    #[test]
    fn unknown_ids_do_not_resolve() {
        assert_eq!(Command::from_id(MenuId(u32::MAX)), None);
    }

    #[test]
    fn implemented_browser_surfaces_are_enabled() {
        for command in [
            Command::Home,
            Command::Settings,
            Command::ShowHistory,
            Command::ShowDownloads,
            Command::ToggleDevTools,
        ] {
            assert!(command.is_available(), "{command:?} should be clickable");
        }
    }

    /// Every command must be reachable from the menu, or it is a command no user
    /// can invoke.
    #[test]
    fn every_command_appears_in_the_menu_bar() {
        let bar = menu_bar();
        let present: std::collections::BTreeSet<MenuId> = bar.ids().collect();

        for command in Command::ALL {
            assert!(
                present.contains(&command.id()),
                "{command:?} is not in any menu"
            );
        }
    }

    #[test]
    fn the_bar_has_the_menus_a_browser_is_expected_to_have() {
        let bar = menu_bar();
        let titles: Vec<&str> = bar.menus.iter().map(|menu| menu.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Otlyra",
                "File",
                "Edit",
                "View",
                "History",
                "Bookmarks",
                "Develop",
                "Window"
            ]
        );
    }
}
