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

    /// Fetch this URL and show the page in a window.
    ///
    /// A bare host is assumed to be `https`. With `--dump-dom` or `--dump-source`
    /// the page goes to the terminal instead and no window opens.
    #[arg(long, value_name = "URL")]
    url: Option<String>,

    /// Open a local HTML file instead of fetching one.
    #[arg(long, value_name = "PATH", conflicts_with = "url")]
    file: Option<PathBuf>,

    /// Print the parsed tree instead of opening a window, then exit.
    ///
    /// Takes a file, or nothing at all when `--url` or `--file` supplies the bytes.
    /// The output is the html5lib-tests format, so what you read is exactly what the
    /// conformance suite compares against.
    #[arg(long, value_name = "PATH", num_args = 0..=1)]
    dump_dom: Option<Option<PathBuf>>,

    /// Print the document's source instead of opening a window, then exit.
    #[arg(long)]
    dump_source: bool,
}

impl Cli {
    /// The viewport `--screenshot` and `--dump-display-list` render at.
    fn viewport(&self) -> Viewport {
        Viewport::new(
            (f64::from(self.width) * self.scale_factor).round() as u32,
            (f64::from(self.height) * self.scale_factor).round() as u32,
            self.scale_factor,
        )
    }
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

    // Was a document named? `--dump-dom PATH` names one too.
    let document = cli
        .url
        .clone()
        .map(Source::Url)
        .or_else(|| cli.file.clone().map(Source::File))
        .or_else(|| cli.dump_dom.clone().flatten().map(Source::File));

    if let Some(source) = document {
        return match open_document(source, &cli) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("otlyra: {error}");
                ExitCode::FAILURE
            }
        };
    }

    if cli.dump_dom.is_some() || cli.dump_source {
        eprintln!("otlyra: --dump-dom and --dump-source need a --url or a --file");
        return ExitCode::FAILURE;
    }

    let mut scene = DemoScene::new();
    let viewport = cli.viewport();

    if let Some(path) = cli.dump_display_list.as_deref() {
        return match dump_display_list(&mut scene, viewport, path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("otlyra: {error}");
                ExitCode::FAILURE
            }
        };
    }

    if cli.renderer == Renderer::Record {
        let list = scene.build_display_list(viewport);
        let mut painter = otlyra_gfx::RecordingPainter::new();
        otlyra_gfx::render(&list, &mut painter);
        for op in painter.ops() {
            println!("{op:?}");
        }
        return ExitCode::SUCCESS;
    }

    let result = match cli.screenshot.as_deref() {
        Some(path) => write_screenshot(&mut scene, viewport, path),
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

/// Where a document's bytes come from.
#[derive(Clone, Debug)]
enum Source {
    /// Over the network.
    Url(String),
    /// Off the disk.
    File(PathBuf),
}

/// The whole pipeline we have, end to end: bytes, encoding, tree, and either a
/// window or a dump of what we found on the way.
///
/// The fetch blocks before the window opens. That is wrong and it is temporary —
/// the event loop must never wait on the network — and it is why `fetch_blocking`
/// is spelled the way it is. Navigation over a channel is M9.
fn open_document(source: Source, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let (bytes, transport_charset) = match &source {
        Source::Url(input) => {
            let resource = fetch(input)?;
            eprintln!(
                "{} {} ({} bytes)",
                resource.status,
                resource.final_url,
                resource.body.len()
            );
            let charset = resource.charset();
            (resource.body, charset)
        }
        Source::File(path) => (std::fs::read(path)?, None),
    };

    if cli.dump_source {
        let decision = otlyra_html::determine(&bytes, transport_charset.as_deref());
        let (text, _actual, _errors) = decision.encoding.decode(&bytes);
        print!("{text}");
        return Ok(());
    }

    let parsed = otlyra_html::parse(&bytes, transport_charset.as_deref());
    eprintln!(
        "encoding {} ({:?}), {} nodes",
        parsed.encoding.encoding.name(),
        parsed.encoding.source,
        parsed.document.len()
    );

    if cli.dump_dom.is_some() {
        print!("{}", otlyra_dom::dump::serialize(&parsed.document));
        return Ok(());
    }

    let title = otlyra_app::page::title_of(&parsed.document);
    let mut page = otlyra_app::page::PageScene::new(&parsed.document);
    eprintln!("{} blocks of text", page.blocks().len());

    match cli.screenshot.as_deref() {
        Some(path) => write_screenshot(&mut page, cli.viewport(), path)?,
        None => run_window(
            WindowConfig {
                title: match (&title, &source) {
                    (Some(title), _) => format!("{title} — Otlyra"),
                    (None, Source::Url(url)) => format!("{url} — Otlyra"),
                    (None, Source::File(path)) => format!("{} — Otlyra", path.display()),
                },
                logical_size: (f64::from(cli.width), f64::from(cli.height)),
                menu_bar: menu_bar(),
                icon: Some(otlyra_app::ICON),
            },
            &mut page,
        )?,
    }
    Ok(())
}

/// Fetch one URL.
///
/// The crypto provider is installed here, in `main`, and nowhere else: rustls picks
/// one implicitly only while exactly one is reachable, and a dependency that drags
/// in a second turns that into a panic at the first HTTPS request. Naming ours makes
/// the choice ours.
fn fetch(input: &str) -> Result<otlyra_net::LoadedResource, otlyra_net::NetError> {
    otlyra_net::install_crypto_provider();

    let url = otlyra_net::normalize(input)?;
    let loader = otlyra_net::Loader::new()?;
    loader.fetch_blocking(otlyra_net::LoadRequest::new(url))
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
