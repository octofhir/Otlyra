//! Where the preferences are kept between runs.
//!
//! # The format
//!
//! `key = value`, one per line, `#` to the end of a line is a comment. That is a
//! subset of TOML and deliberately not the whole of it: this file holds a handful
//! of scalars, it is written and read by this program, and a parser for tables and
//! arrays would be a parser for shapes nothing here can produce. A line that
//! does not fit the subset is skipped with a warning rather than refused — a
//! preferences file is not worth failing to start over, and a person who has
//! hand-edited one wants the rest of their settings back.
//!
//! # Where
//!
//! The platform's own configuration directory, worked out from the environment
//! rather than taken from a crate: it is three cases, they are stable, and a
//! dependency for them would be one more thing to keep current for as long as
//! this program exists.

use std::path::PathBuf;

use crate::settings::{Appearance, OnStart, Settings};

/// What this program's directory is called inside the platform's.
const FOLDER: &str = "Otlyra";
/// And the file inside that.
const FILE: &str = "preferences.toml";

/// The directory the browser keeps its own files in, if the platform will say.
///
/// Derived from the preferences' path rather than worked out a second time, so the
/// `OTLYRA_CONFIG_DIR` override covers everything the browser saves — a test that
/// redirected the preferences and not the bookmarks would still be writing into the
/// developer's home directory.
pub fn directory() -> Option<PathBuf> {
    path()?.parent().map(std::path::Path::to_path_buf)
}

/// Where the preferences live, if the platform will say.
pub fn path() -> Option<PathBuf> {
    // An escape hatch, and the reason it exists is worth stating: without it a
    // test that saves a preference writes the *developer's* preferences, and the
    // next test that starts a browser reads them back. That is a test suite whose
    // result depends on what the last run happened to click, which is how a
    // machine ends up being the only one where anything passes.
    if let Some(directory) = std::env::var_os("OTLYRA_CONFIG_DIR") {
        return Some(PathBuf::from(directory).join(FILE));
    }
    platform_path()
}

/// Where the platform keeps a program's configuration, ignoring the override.
fn platform_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let directory = if cfg!(target_os = "macos") {
        home?.join("Library").join("Application Support")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)?
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".config")))?
    };
    Some(directory.join(FOLDER).join(FILE))
}

/// Read the preferences, falling back to the defaults for anything missing.
///
/// Never fails. A file that is not there is a browser that has not been
/// configured, and a file that cannot be read is a warning and the defaults —
/// refusing to start over a preferences file would be refusing to start over
/// something the reader can live without.
pub fn load() -> Settings {
    let Some(path) = path() else {
        return Settings::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        // Not a warning: a browser that has never been configured has no file,
        // and saying so on every launch would be noise about the ordinary case.
        return Settings::default();
    };
    from_text(&text)
}

/// Write them, if the platform will let us.
///
/// Failure is a warning and nothing else. A preference that could not be saved
/// is a preference that lasts until the browser closes, which is worse than
/// saving it and better than refusing to change it.
pub fn save(settings: &Settings) {
    let Some(path) = path() else {
        return;
    };
    if let Some(directory) = path.parent()
        && let Err(error) = std::fs::create_dir_all(directory)
    {
        tracing::warn!(%error, path = %directory.display(), "could not make the preferences directory");
        return;
    }
    if let Err(error) = std::fs::write(&path, to_text(settings)) {
        tracing::warn!(%error, path = %path.display(), "could not write the preferences");
    }
}

/// The preferences as the file spells them.
pub fn to_text(settings: &Settings) -> String {
    format!(
        "# Otlyra's preferences. Written by the browser; safe to edit by hand.\n\
         on_start = \"{}\"\n\
         home = \"{}\"\n\
         load_images = {}\n\
         run_scripts = {}\n\
         do_not_track = {}\n\
         block_third_party_cookies = {}\n\
         restore_tabs = {}\n\
         appearance = \"{}\"\n\
         text_scale = {}\n\
         download_ask = {}\n\
         download_directory = \"{}\"\n{}",
        match settings.on_start {
            OnStart::Blank => "blank",
            OnStart::Home => "home",
            OnStart::Restore => "restore",
        },
        // Escaped, because an address may contain a quote and a file that cannot
        // be read back is a file that was not saved.
        settings
            .home
            .text()
            .replace('\\', "\\\\")
            .replace('"', "\\\""),
        settings.load_images,
        settings.run_scripts,
        settings.do_not_track,
        settings.block_third_party_cookies,
        settings.restore_tabs,
        match settings.appearance {
            Appearance::Light => "light",
            Appearance::Dark => "dark",
            Appearance::System => "system",
        },
        settings.text_scale,
        settings.download_ask,
        // A path may hold a quote or a backslash — a Windows one holds nothing
        // but backslashes — so it is escaped exactly as the home address is.
        settings
            .download_directory
            .replace('\\', "\\\\")
            .replace('"', "\\\""),
        // One line per site, sorted, so a file written twice from the same
        // preferences is the same file — a diff of a preferences file should be
        // what changed and not what a map felt like ordering itself as.
        settings
            .zoom
            .iter()
            .map(|(origin, factor)| {
                format!(
                    "zoom.\"{}\" = {factor}\n",
                    origin.replace('\\', "\\\\").replace('"', "\\\"")
                )
            })
            .collect::<String>(),
    )
}

/// Read them back, keeping the default for anything the file does not settle.
pub fn from_text(text: &str) -> Settings {
    let mut settings = Settings::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            tracing::warn!(line, "a preferences line that is not `key = value`");
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        let text = || {
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .map(|value| value.replace("\\\"", "\"").replace("\\\\", "\\"))
        };
        let flag = || value.parse::<bool>().ok();

        match key {
            key if key.starts_with("zoom.") => {
                // `zoom."example.com" = 1.25`. The site is in the key rather
                // than in a table because the file is read a line at a time and
                // a line that carries both is a line that can be read on its
                // own.
                let origin = key["zoom.".len()..]
                    .trim()
                    .strip_prefix('"')
                    .and_then(|origin| origin.strip_suffix('"'))
                    .map(|origin| origin.replace("\\\"", "\"").replace("\\\\", "\\"));
                match (origin, value.parse::<f32>()) {
                    (Some(origin), Ok(factor)) if !origin.is_empty() && factor > 0.0 => {
                        settings.zoom.insert(origin, factor);
                    }
                    _ => tracing::warn!(line, "a zoom line that names no site or no factor"),
                }
            }
            "on_start" => {
                settings.on_start = match text().as_deref() {
                    Some("home") => OnStart::Home,
                    Some("restore") => OnStart::Restore,
                    Some("blank") => OnStart::Blank,
                    _ => {
                        tracing::warn!(value, "an on_start nobody has heard of");
                        settings.on_start
                    }
                };
            }
            "home" => {
                if let Some(home) = text() {
                    settings.home = crate::ui::TextField::new(home);
                }
            }
            "load_images" => settings.load_images = flag().unwrap_or(settings.load_images),
            "run_scripts" => settings.run_scripts = flag().unwrap_or(settings.run_scripts),
            "do_not_track" => settings.do_not_track = flag().unwrap_or(settings.do_not_track),
            "block_third_party_cookies" => {
                settings.block_third_party_cookies =
                    flag().unwrap_or(settings.block_third_party_cookies);
            }
            "restore_tabs" => settings.restore_tabs = flag().unwrap_or(settings.restore_tabs),
            "appearance" => {
                settings.appearance = match text().as_deref() {
                    Some("light") => Appearance::Light,
                    Some("dark") => Appearance::Dark,
                    Some("system") => Appearance::System,
                    _ => {
                        tracing::warn!(value, "an appearance nobody has heard of");
                        settings.appearance
                    }
                };
            }
            "download_ask" => settings.download_ask = flag().unwrap_or(settings.download_ask),
            "download_directory" => {
                if let Some(directory) = text() {
                    settings.download_directory = directory;
                }
            }
            "text_scale" => {
                if let Ok(scale) = value.parse::<f64>() {
                    // Clamped to what the control can express: a file saying
                    // 10000 would be a browser nobody can read and no way back
                    // to one, since the slider could not reach the value to
                    // change it.
                    settings.text_scale = scale.clamp(50.0, 200.0);
                }
            }
            _ => tracing::warn!(key, "a preference nobody has heard of"),
        }
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_written_is_what_is_read_back() {
        let mut settings = Settings::default();
        settings.apply(crate::settings::Action::ToggleImages);
        settings.apply(crate::settings::Action::ToggleDoNotTrack);
        settings.apply(crate::settings::Action::SetTextScale(125.0));
        settings.apply(crate::settings::Action::SetOnStart(OnStart::Home));
        settings.apply(crate::settings::Action::SetAppearance(Appearance::Dark));
        settings.home = crate::ui::TextField::new("https://example.org/start");

        let read = from_text(&to_text(&settings));
        assert_eq!(read.load_images, settings.load_images);
        assert_eq!(read.do_not_track, settings.do_not_track);
        assert_eq!(
            read.block_third_party_cookies,
            settings.block_third_party_cookies
        );
        assert_eq!(read.text_scale, 125.0);
        assert_eq!(read.on_start, OnStart::Home);
        assert_eq!(read.appearance, Appearance::Dark);
        assert_eq!(read.home.text(), "https://example.org/start");
    }

    /// Where downloads go outlives the run that chose it — a preference that had
    /// to be set again on every launch would not be a preference.
    #[test]
    fn where_downloads_go_survives_the_round_trip() {
        let mut settings = Settings::default();
        settings.apply(crate::settings::Action::ToggleDownloadAsk);
        settings.apply(crate::settings::Action::SetDownloadDirectory(
            r"C:\Users\Ada\Downloads".to_owned(),
        ));

        let read = from_text(&to_text(&settings));
        assert!(!read.download_ask);
        // Backslashes and all: a Windows path is nothing else.
        assert_eq!(read.download_directory, r"C:\Users\Ada\Downloads");
    }

    #[test]
    fn an_address_with_a_quote_in_it_survives_the_round_trip() {
        let mut settings = Settings::default();
        settings.home = crate::ui::TextField::new(r#"https://example.org/?q="x"\y"#);
        assert_eq!(
            from_text(&to_text(&settings)).home.text(),
            r#"https://example.org/?q="x"\y"#
        );
    }

    #[test]
    fn a_file_that_makes_no_sense_gives_the_defaults_rather_than_nothing() {
        // Hand-edited badly, or written by a version that had other ideas. What
        // it does settle is kept; the rest is the default, and none of it is a
        // reason to refuse to start.
        let read = from_text(
            "this is not a preferences file\n\
             load_images = no\n\
             do_not_track = true\n\
             wallpaper = \"blue\"\n",
        );
        let defaults = Settings::default();
        assert_eq!(read.load_images, defaults.load_images, "`no` is not a bool");
        assert!(read.do_not_track, "and the line that did parse took effect");
    }

    #[test]
    fn a_comment_is_not_a_preference() {
        let read = from_text("# load_images = false\nload_images = false # and this\n");
        assert!(!read.load_images);
    }

    #[test]
    fn a_text_size_from_a_file_stays_within_what_the_control_can_undo() {
        // Otherwise a hand-edited file could leave a browser nobody can read and
        // no way back, because the slider could not reach the value to change it.
        assert_eq!(from_text("text_scale = 10000").text_scale, 200.0);
        assert_eq!(from_text("text_scale = 1").text_scale, 50.0);
    }

    #[test]
    fn the_file_goes_where_the_platform_keeps_such_things() {
        // `platform_path` rather than `path`, deliberately: `path` answers the
        // override when one is set, and another test in this binary sets it. A
        // test that read `path` would pass or fail on the order they ran in.
        let path = platform_path().expect("a home directory in a test environment");
        assert!(
            path.ends_with(std::path::Path::new(FOLDER).join(FILE)),
            "{path:?}"
        );
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::*;

    /// A zoom per site survives being written and read back, and a site left at
    /// its own size leaves nothing behind.
    #[test]
    fn zooms_round_trip_through_the_file() {
        let mut settings = Settings::default();
        settings.zoom.insert("https://example.com".to_owned(), 1.25);
        settings
            .zoom
            .insert("http://other.example".to_owned(), 0.75);

        let text = to_text(&settings);
        assert!(
            text.contains("zoom.\"https://example.com\" = 1.25"),
            "{text}"
        );

        let read = from_text(&text);
        assert_eq!(read.zoom, settings.zoom);

        // Written twice from the same preferences, the file is the same file: a
        // diff of one should be what changed rather than what a map felt like
        // ordering itself as.
        assert_eq!(to_text(&read), text);

        // A line naming no site or no factor is dropped rather than taken.
        let broken = from_text("zoom.\"\" = 1.5\nzoom.\"a.example\" = nonsense\n");
        assert!(broken.zoom.is_empty());
    }
}
