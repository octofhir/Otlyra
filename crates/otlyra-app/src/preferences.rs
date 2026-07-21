//! Where the preferences are kept between runs.
//!
//! # The format
//!
//! `key = value`, one per line, `#` to the end of a line is a comment. That is a
//! subset of TOML and deliberately not the whole of it: this file holds seven
//! scalars, it is written and read by this program, and a parser for tables and
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

use crate::settings::{OnStart, Settings};

/// What this program's directory is called inside the platform's.
const FOLDER: &str = "Otlyra";
/// And the file inside that.
const FILE: &str = "preferences.toml";

/// Where the preferences live, if the platform will say.
pub fn path() -> Option<PathBuf> {
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
         restore_tabs = {}\n\
         text_scale = {}\n",
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
        settings.restore_tabs,
        settings.text_scale,
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
            "restore_tabs" => settings.restore_tabs = flag().unwrap_or(settings.restore_tabs),
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
        settings.home = crate::ui::TextField::new("https://example.org/start");

        let read = from_text(&to_text(&settings));
        assert_eq!(read.load_images, settings.load_images);
        assert_eq!(read.do_not_track, settings.do_not_track);
        assert_eq!(read.text_scale, 125.0);
        assert_eq!(read.on_start, OnStart::Home);
        assert_eq!(read.home.text(), "https://example.org/start");
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
        let path = path().expect("a home directory in a test environment");
        assert!(
            path.ends_with(std::path::Path::new(FOLDER).join(FILE)),
            "{path:?}"
        );
    }
}
