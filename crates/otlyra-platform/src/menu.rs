//! The native menu bar.
//!
//! Menus are real OS menus — `NSMenu` on macOS — not shapes we draw. Drawing them
//! ourselves would lose the system menu bar position, VoiceOver, the Services menu,
//! system-wide keyboard shortcut handling and the user's own shortcut overrides.
//! None of that is worth reimplementing, and most of it cannot be.
//!
//! As with winit and wgpu, no `muda` type reaches this crate's public API: menus
//! are described with the vocabulary below and built into native objects inside.

use std::str::FromStr;

/// Identifies a menu item the embedder defined.
///
/// Opaque to this crate. The app decides what the number means.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MenuId(pub u32);

/// A menu item whose behaviour the operating system provides.
///
/// These are not merely items with a default action: the OS expects to own them.
/// `Services` is populated by the system, `About` and `Hide` participate in
/// application-level conventions, and `Quit` performs the standard termination
/// sequence. Reimplementing them produces a menu that looks right and behaves
/// subtly wrong.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SystemItem {
    /// The standard about panel.
    About,
    /// The system-populated Services submenu.
    Services,
    /// Hide this application.
    Hide,
    /// Hide every other application.
    HideOthers,
    /// Show all applications.
    ShowAll,
    /// Quit, following the platform's termination sequence.
    Quit,
    /// Close the focused window.
    CloseWindow,
    /// Minimize the focused window.
    Minimize,
    /// Zoom the focused window.
    Maximize,
    /// Toggle full screen.
    Fullscreen,
    /// Bring every window of this application forward.
    BringAllToFront,
    /// Standard editing commands, which the OS routes to the focused text control.
    Undo,
    /// See [`SystemItem::Undo`].
    Redo,
    /// See [`SystemItem::Undo`].
    Cut,
    /// See [`SystemItem::Undo`].
    Copy,
    /// See [`SystemItem::Undo`].
    Paste,
    /// See [`SystemItem::Undo`].
    SelectAll,
}

/// One entry in a menu.
#[derive(Clone, Debug, PartialEq)]
pub enum MenuEntry {
    /// An item the embedder handles, reported as [`PlatformEvent::MenuCommand`].
    ///
    /// [`PlatformEvent::MenuCommand`]: crate::PlatformEvent::MenuCommand
    Item {
        /// Reported back when the item is chosen.
        id: MenuId,
        /// Text shown to the user.
        label: String,
        /// Keyboard shortcut, in the conventional spelling: `"CmdOrCtrl+T"`,
        /// `"CmdOrCtrl+Shift+R"`, `"F11"`. `None` for no shortcut.
        accelerator: Option<String>,
        /// Whether the item can be chosen.
        enabled: bool,
    },
    /// A separator line.
    Separator,
    /// An item the operating system implements.
    System(SystemItem),
}

impl MenuEntry {
    /// An enabled item with a shortcut.
    pub fn item(id: MenuId, label: impl Into<String>, accelerator: &str) -> Self {
        Self::Item {
            id,
            label: label.into(),
            accelerator: Some(accelerator.to_owned()),
            enabled: true,
        }
    }

    /// An enabled item with no shortcut.
    pub fn plain(id: MenuId, label: impl Into<String>) -> Self {
        Self::Item {
            id,
            label: label.into(),
            accelerator: None,
            enabled: true,
        }
    }

    /// A disabled item, for a command that exists but is not available yet.
    ///
    /// Preferable to hiding it: a menu whose entries appear and disappear is harder
    /// to learn than one whose entries grey out.
    pub fn disabled(id: MenuId, label: impl Into<String>, accelerator: Option<&str>) -> Self {
        Self::Item {
            id,
            label: label.into(),
            accelerator: accelerator.map(str::to_owned),
            enabled: false,
        }
    }
}

/// One top-level menu.
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    /// Title in the menu bar.
    pub title: String,
    /// Entries, in order.
    pub entries: Vec<MenuEntry>,
}

impl Menu {
    /// A menu with `title` and `entries`.
    pub fn new(title: impl Into<String>, entries: Vec<MenuEntry>) -> Self {
        Self {
            title: title.into(),
            entries,
        }
    }
}

/// The whole menu bar.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuBar {
    /// Top-level menus, left to right.
    pub menus: Vec<Menu>,
}

impl MenuBar {
    /// A menu bar with no menus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a menu.
    pub fn with(mut self, menu: Menu) -> Self {
        self.menus.push(menu);
        self
    }

    /// Every accelerator string in the bar, for validation.
    pub fn accelerators(&self) -> impl Iterator<Item = &str> {
        self.menus
            .iter()
            .flat_map(|menu| &menu.entries)
            .filter_map(|entry| match entry {
                MenuEntry::Item {
                    accelerator: Some(accelerator),
                    ..
                } => Some(accelerator.as_str()),
                _ => None,
            })
    }

    /// Every embedder-defined id in the bar, for duplicate checking.
    pub fn ids(&self) -> impl Iterator<Item = MenuId> {
        self.menus
            .iter()
            .flat_map(|menu| &menu.entries)
            .filter_map(|entry| match entry {
                MenuEntry::Item { id, .. } => Some(*id),
                _ => None,
            })
    }

    /// Check that every accelerator parses and no id repeats.
    ///
    /// Menus are built from string shortcuts, so a typo would otherwise become a
    /// silently missing shortcut at runtime. Calling this in a test turns that into
    /// a build failure.
    pub fn validate(&self) -> Result<(), MenuError> {
        for accelerator in self.accelerators() {
            muda::accelerator::Accelerator::from_str(accelerator)
                .map_err(|_| MenuError::Accelerator(accelerator.to_owned()))?;
        }

        let mut seen = std::collections::BTreeSet::new();
        for id in self.ids() {
            if !seen.insert(id) {
                return Err(MenuError::DuplicateId(id));
            }
        }
        Ok(())
    }
}

/// Why a menu bar could not be built.
#[derive(Debug, thiserror::Error)]
pub enum MenuError {
    /// An accelerator string could not be parsed.
    #[error("`{0}` is not a valid accelerator")]
    Accelerator(String),
    /// Two items share an id, so activations would be ambiguous.
    #[error("menu id {0:?} is used more than once")]
    DuplicateId(MenuId),
    /// The platform refused to build the menu.
    #[error("the platform rejected the menu: {0}")]
    Platform(String),
}

/// Builds the native menu and keeps it alive.
///
/// The menu must outlive the application: dropping it removes the menu bar.
pub(crate) struct NativeMenu {
    #[allow(
        dead_code,
        reason = "held to keep the native menu alive for the app's lifetime"
    )]
    menu: muda::Menu,
}

impl NativeMenu {
    /// Build `bar` and install it as the application menu.
    pub(crate) fn install(bar: &MenuBar) -> Result<Self, MenuError> {
        bar.validate()?;

        let menu = muda::Menu::new();
        for section in &bar.menus {
            let submenu = muda::Submenu::new(&section.title, true);
            for entry in &section.entries {
                append(&submenu, entry)?;
            }
            menu.append(&submenu)
                .map_err(|error| MenuError::Platform(error.to_string()))?;
        }

        #[cfg(target_os = "macos")]
        menu.init_for_nsapp();

        Ok(Self { menu })
    }
}

fn append(submenu: &muda::Submenu, entry: &MenuEntry) -> Result<(), MenuError> {
    use muda::{MenuId as MudaId, MenuItem, PredefinedMenuItem as Predefined};

    let result = match entry {
        MenuEntry::Separator => submenu.append(&Predefined::separator()),
        MenuEntry::Item {
            id,
            label,
            accelerator,
            enabled,
        } => {
            let accelerator = match accelerator {
                Some(accelerator) => Some(
                    muda::accelerator::Accelerator::from_str(accelerator)
                        .map_err(|_| MenuError::Accelerator(accelerator.clone()))?,
                ),
                None => None,
            };
            let item =
                MenuItem::with_id(MudaId::new(id.0.to_string()), label, *enabled, accelerator);
            submenu.append(&item)
        }
        MenuEntry::System(system) => match system {
            SystemItem::About => submenu.append(&Predefined::about(None, None)),
            SystemItem::Services => submenu.append(&Predefined::services(None)),
            SystemItem::Hide => submenu.append(&Predefined::hide(None)),
            SystemItem::HideOthers => submenu.append(&Predefined::hide_others(None)),
            SystemItem::ShowAll => submenu.append(&Predefined::show_all(None)),
            SystemItem::Quit => submenu.append(&Predefined::quit(None)),
            SystemItem::CloseWindow => submenu.append(&Predefined::close_window(None)),
            SystemItem::Minimize => submenu.append(&Predefined::minimize(None)),
            SystemItem::Maximize => submenu.append(&Predefined::maximize(None)),
            SystemItem::Fullscreen => submenu.append(&Predefined::fullscreen(None)),
            SystemItem::BringAllToFront => submenu.append(&Predefined::bring_all_to_front(None)),
            SystemItem::Undo => submenu.append(&Predefined::undo(None)),
            SystemItem::Redo => submenu.append(&Predefined::redo(None)),
            SystemItem::Cut => submenu.append(&Predefined::cut(None)),
            SystemItem::Copy => submenu.append(&Predefined::copy(None)),
            SystemItem::Paste => submenu.append(&Predefined::paste(None)),
            SystemItem::SelectAll => submenu.append(&Predefined::select_all(None)),
        },
    };

    result.map_err(|error| MenuError::Platform(error.to_string()))
}

/// Translate a native activation back into the embedder's id.
pub(crate) fn command_from_muda(event: &muda::MenuEvent) -> Option<MenuId> {
    event.id().0.parse().ok().map(MenuId)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar() -> MenuBar {
        MenuBar::new().with(Menu::new(
            "File",
            vec![
                MenuEntry::item(MenuId(1), "New Tab", "CmdOrCtrl+KeyT"),
                MenuEntry::Separator,
                MenuEntry::System(SystemItem::CloseWindow),
            ],
        ))
    }

    #[test]
    fn a_well_formed_bar_validates() {
        bar().validate().expect("valid");
    }

    #[test]
    fn a_bad_accelerator_is_rejected() {
        let bar = MenuBar::new().with(Menu::new(
            "File",
            vec![MenuEntry::item(MenuId(1), "New Tab", "NotAKey+???")],
        ));
        assert!(matches!(bar.validate(), Err(MenuError::Accelerator(_))));
    }

    /// Duplicate ids would make an activation ambiguous, so they are a build-time
    /// failure rather than a surprise at runtime.
    #[test]
    fn duplicate_ids_are_rejected() {
        let bar = MenuBar::new().with(Menu::new(
            "File",
            vec![
                MenuEntry::plain(MenuId(1), "One"),
                MenuEntry::plain(MenuId(1), "Two"),
            ],
        ));
        assert!(matches!(
            bar.validate(),
            Err(MenuError::DuplicateId(MenuId(1)))
        ));
    }

    #[test]
    fn only_embedder_items_contribute_ids_and_accelerators() {
        let bar = bar();
        assert_eq!(bar.ids().collect::<Vec<_>>(), [MenuId(1)]);
        assert_eq!(bar.accelerators().collect::<Vec<_>>(), ["CmdOrCtrl+KeyT"]);
    }

    #[test]
    fn disabled_items_keep_their_id_and_shortcut() {
        let entry = MenuEntry::disabled(MenuId(9), "Back", Some("CmdOrCtrl+BracketLeft"));
        let MenuEntry::Item {
            id,
            enabled,
            accelerator,
            ..
        } = entry
        else {
            panic!("expected an item");
        };
        assert_eq!(id, MenuId(9));
        assert!(!enabled);
        assert_eq!(accelerator.as_deref(), Some("CmdOrCtrl+BracketLeft"));
    }
}
