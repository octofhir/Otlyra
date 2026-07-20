//! The `otlyra` binary.
//!
//! Two modes, one renderer: open a window, or render one frame to a PNG and exit.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use otlyra_app::scene::DemoScene;
use otlyra_app::{observability, run_window, write_screenshot};
use otlyra_platform::{Viewport, WindowConfig};

/// An experimental browser.
#[derive(Debug, Parser)]
#[command(name = "otlyra", version, about)]
struct Cli {
    /// Render a single frame to this path as a PNG, then exit.
    ///
    /// Needs no display server, so this is what CI's image tests drive.
    #[arg(long, value_name = "PATH")]
    screenshot: Option<PathBuf>,

    /// Viewport width in logical pixels.
    #[arg(long, default_value_t = 1024)]
    width: u32,

    /// Viewport height in logical pixels.
    #[arg(long, default_value_t = 768)]
    height: u32,

    /// Device pixels per logical pixel. Only meaningful with `--screenshot`; a real
    /// window takes its scale factor from the OS.
    #[arg(long, default_value_t = 2.0)]
    scale_factor: f64,
}

fn main() -> ExitCode {
    observability::init();
    let cli = Cli::parse();

    let mut scene = DemoScene::new();

    let result = match cli.screenshot.as_deref() {
        Some(path) => {
            let viewport = Viewport::new(
                (f64::from(cli.width) * cli.scale_factor).round() as u32,
                (f64::from(cli.height) * cli.scale_factor).round() as u32,
                cli.scale_factor,
            );
            write_screenshot(&mut scene, viewport, path)
        }
        None => run_window(
            WindowConfig {
                title: "Otlyra".to_owned(),
                logical_size: (f64::from(cli.width), f64::from(cli.height)),
            },
            &mut scene,
        ),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("otlyra: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}
