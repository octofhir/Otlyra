//! The `otlyra` binary.
//!
//! Two modes, one renderer: open a window, or render one frame to a PNG and exit.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use otlyra_app::menu::menu_bar;
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

    /// Write the frame's display list to this path as JSON, then exit.
    ///
    /// Needs neither a GPU nor a rasterizer, so it is the cheapest way to answer
    /// "what did the browser decide to draw?".
    #[arg(long, value_name = "PATH")]
    dump_display_list: Option<PathBuf>,

    /// Which rasterizer to use.
    #[arg(long, value_enum, default_value_t = Renderer::Skia)]
    renderer: Renderer,

    /// Fetch this URL, print the source it returns, then exit.
    ///
    /// A bare host is assumed to be `https`. Nothing is parsed or rendered yet;
    /// this is the network stack on its own.
    #[arg(long, value_name = "URL")]
    url: Option<String>,
}

/// The rasterizer backends the `PaintTarget` seam offers.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Renderer {
    /// Skia. Produces pixels.
    Skia,
    /// Record paint operations and print them. Runs headlessly, with no GPU and no
    /// rasterization, which makes it usable anywhere.
    Record,
}

fn main() -> ExitCode {
    observability::init();
    let cli = Cli::parse();

    if let Some(url) = cli.url.as_deref() {
        return match print_source(url) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("otlyra: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let mut scene = DemoScene::new();

    let viewport = || {
        Viewport::new(
            (f64::from(cli.width) * cli.scale_factor).round() as u32,
            (f64::from(cli.height) * cli.scale_factor).round() as u32,
            cli.scale_factor,
        )
    };

    if let Some(path) = cli.dump_display_list.as_deref() {
        return match dump_display_list(&mut scene, viewport(), path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("otlyra: {error}");
                ExitCode::FAILURE
            }
        };
    }

    if cli.renderer == Renderer::Record {
        let list = scene.build_display_list(viewport());
        let mut painter = otlyra_gfx::RecordingPainter::new();
        otlyra_gfx::render(&list, &mut painter);
        for op in painter.ops() {
            println!("{op:?}");
        }
        return ExitCode::SUCCESS;
    }

    let result = match cli.screenshot.as_deref() {
        Some(path) => write_screenshot(&mut scene, viewport(), path),
        None => run_window(
            WindowConfig {
                title: "Otlyra".to_owned(),
                logical_size: (f64::from(cli.width), f64::from(cli.height)),
                menu_bar: menu_bar(),
                icon: Some(otlyra_app::ICON),
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

/// Fetch `input` and print what the server sent, decoded to text.
///
/// The crypto provider is installed here, in `main`, and nowhere else: rustls picks
/// one implicitly only while exactly one is reachable, and a dependency that drags
/// in a second turns that into a panic at the first HTTPS request. Naming ours makes
/// the choice ours.
fn print_source(input: &str) -> Result<(), otlyra_net::NetError> {
    otlyra_net::install_crypto_provider();

    let url = otlyra_net::normalize(input)?;
    let loader = otlyra_net::Loader::new()?;
    let resource = loader.fetch_blocking(otlyra_net::LoadRequest::new(url))?;

    eprintln!(
        "{} {} ({} bytes)",
        resource.status,
        resource.final_url,
        resource.body.len()
    );
    print!("{}", resource.decode_text());
    Ok(())
}

/// Serialize the frame's display list to `path`.
fn dump_display_list(
    scene: &mut DemoScene,
    viewport: Viewport,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let list = scene.build_display_list(viewport);
    let json = serde_json::to_string_pretty(&list)?;
    std::fs::write(path, json)?;
    eprintln!("wrote {} items to {}", list.len(), path.display());
    Ok(())
}
