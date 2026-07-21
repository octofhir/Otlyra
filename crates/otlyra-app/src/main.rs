//! The `otlyra` binary.
//!
//! Two modes, one renderer: open a window, or render one frame to a PNG and exit.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use otlyra_app::browser::Browser;
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

    /// Print the box tree instead of opening a window, then exit.
    ///
    /// The box tree is what the DOM becomes once the user-agent stylesheet has had
    /// its say: `display: none` gone, anonymous blocks inserted, one style per box.
    #[arg(long)]
    dump_boxes: bool,

    /// Print the laid-out fragment tree instead of opening a window, then exit.
    ///
    /// Geometry, in logical pixels, at `--width` by `--height`.
    #[arg(long)]
    dump_fragments: bool,

    /// Print which elements a selector matches, then exit.
    ///
    /// Answers the question the cascade will ask of every rule, one selector at a
    /// time: `--dump-selectors 'ul > li:first-child'`.
    #[arg(long, value_name = "SELECTOR")]
    dump_selectors: Option<String>,

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

    if cli.dump_dom.is_some()
        || cli.dump_source
        || cli.dump_boxes
        || cli.dump_fragments
        || cli.dump_selectors.is_some()
    {
        eprintln!("otlyra: --dump-dom, --dump-boxes and --dump-source need a --url or a --file");
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
        // The golden image is the demo scene: it is the one frame whose every pixel
        // is ours, with no system font and no network in it.
        Some(path) => write_screenshot(&mut scene, viewport, path),
        None => {
            let mut browser = Browser::new(NetLoader::default());
            run_window(window_config(&cli), &mut browser)
        }
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
    // One of the browser's own pages fetches nothing and parses nothing, so
    // every step below it — the encoding decision, the tree, the dumps — has
    // no input to work on. It goes straight to a window.
    if let Source::Url(input) = &source
        && let Some(page) = otlyra_app::ui::SystemPage::from_url(input)
    {
        let mut browser = Browser::new(NetLoader::default());
        browser.open_system(page);
        return match cli.screenshot.as_deref() {
            Some(path) => Ok(write_screenshot(&mut browser, cli.viewport(), path)?),
            None => Ok(run_window(window_config(cli), &mut browser)?),
        };
    }

    // The dumps want the bytes here, on the way past; a window does not — it
    // fetches for itself, off its own thread, and fetching twice would be a second
    // request for the same page and a second parse of it.
    let wants_bytes = cli.dump_source
        || cli.dump_dom.is_some()
        || cli.dump_boxes
        || cli.dump_fragments
        || cli.dump_selectors.is_some();
    if !wants_bytes {
        let mut browser = Browser::new(NetLoader::default());
        browser.navigate(&match &source {
            Source::Url(url) => url.clone(),
            Source::File(path) => path.display().to_string(),
        });
        return match cli.screenshot.as_deref() {
            Some(path) => {
                browser.wait_for_load(LOAD_TIMEOUT);
                Ok(write_screenshot(&mut browser, cli.viewport(), path)?)
            }
            None => Ok(run_window(window_config(cli), &mut browser)?),
        };
    }

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

    if let Some(selector) = cli.dump_selectors.as_deref() {
        let matched = otlyra_css::stylo_dom::select(&parsed.document, selector)
            .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
        eprintln!("{} elements match {selector:?}", matched.len());
        for id in matched {
            let element = parsed.document.node(id).element().expect("an element");
            print!("<{}", element.name.local);
            for attr in &element.attrs {
                print!(" {}=\"{}\"", attr.name.local, attr.value);
            }
            println!(">");
        }
        return Ok(());
    }

    if cli.dump_boxes {
        let tree = styled_boxes(&parsed.document, cli.width, cli.height);
        eprintln!("{} boxes", tree.len());
        print!("{}", otlyra_layout::dump::serialize(&tree));
        return Ok(());
    }

    if cli.dump_fragments {
        let boxes = styled_boxes(&parsed.document, cli.width, cli.height);
        let mut text = otlyra_text::TextEngine::new();
        let fragments = otlyra_layout::layout(
            &boxes,
            &mut text,
            otlyra_layout::Viewport {
                width: cli.width as f32,
                height: cli.height as f32,
            },
        );
        print!("{}", otlyra_layout::dump::serialize_fragments(&fragments));
        return Ok(());
    }

    // A document was named on the command line, and nothing asked for a dump: open
    // the browser with it already loaded.
    let mut browser = Browser::new(NetLoader::default());
    browser.navigate(&match &source {
        Source::Url(url) => url.clone(),
        Source::File(path) => path.display().to_string(),
    });

    match cli.screenshot.as_deref() {
        Some(path) => {
            // A screenshot has one frame to get right and no event loop to be woken
            // by, so this is the one place that waits for a load.
            browser.wait_for_load(LOAD_TIMEOUT);
            write_screenshot(&mut browser, cli.viewport(), path)?;
        }
        None => run_window(window_config(cli), &mut browser)?,
    }
    Ok(())
}

/// How long `--screenshot` waits for the page it was given.
const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How the shell configures its window.
fn window_config(cli: &Cli) -> WindowConfig {
    WindowConfig {
        title: "Otlyra".to_owned(),
        logical_size: (f64::from(cli.width), f64::from(cli.height)),
        menu_bar: menu_bar(),
        icon: Some(otlyra_app::ICON),
    }
}

/// The real loader: `otlyra-net` over HTTP, the filesystem for a `file:` URL.
///
/// One client behind a `OnceLock`, shared by every fetch thread: the client owns
/// the connection pool, so one per thread would be several pools and several
/// runtimes for no gain.
#[derive(Default)]
struct NetLoader {
    loader: std::sync::OnceLock<otlyra_net::Loader>,
}

/// The `file:` URL an input names, if it names one.
///
/// Accepts both a `file://` URL and a plain path, because both are things people
/// type; a path is resolved against the working directory, as a shell would.
/// The box tree a dump should show: the one the window would draw, cascade and all.
fn styled_boxes(
    document: &otlyra_dom::Document,
    width: u32,
    height: u32,
) -> otlyra_layout::BoxTree {
    let styles = otlyra_css::cascade::style_document(
        document,
        otlyra_css::cascade::Viewport {
            width: width as f32,
            height: height as f32,
            scale: 1.0,
        },
    );
    otlyra_layout::build_styled_box_tree(document, &styles)
}

fn file_url(input: &str) -> Option<url::Url> {
    if let Ok(url) = url::Url::parse(input)
        && url.scheme() == "file"
    {
        return Some(url);
    }

    let path = std::path::Path::new(input);
    if !path.exists() {
        return None;
    }
    let absolute = path
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default().join(path));
    url::Url::from_file_path(absolute).ok()
}

/// The type a filename claims, by its extension.
///
/// Only the ones a browser has to get right: a document, a stylesheet, a script and
/// the pictures. Anything else is left unsaid, and the bytes decide.
fn content_type_of(path: &std::path::Path) -> Option<String> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "html" | "htm" | "xhtml" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "txt" | "md" | "rs" | "toml" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => return None,
    };
    Some(mime.to_owned())
}

impl otlyra_app::fetcher::Loader for NetLoader {
    fn load(&self, input: &str) -> Result<otlyra_app::fetcher::Loaded, String> {
        // A path typed into the address bar becomes the `file:` URL it names, so
        // that what the bar shows is an address and not a filename — and so that a
        // relative link on the page has something to resolve against.
        if let Some(url) = file_url(input) {
            let path = url
                .to_file_path()
                .map_err(|()| format!("not a path: {input}"))?;
            let bytes =
                std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            // A filesystem has no `Content-Type`, so the extension stands in for
            // one — which is what every browser does with a `file:` URL, and is why
            // opening a `.css` shows the stylesheet rather than rendering it.
            return Ok(otlyra_app::fetcher::Loaded {
                bytes,
                content_type: content_type_of(&path),
                final_url: url.to_string(),
                ..Default::default()
            });
        }

        otlyra_net::install_crypto_provider();
        let url = otlyra_net::normalize(input).map_err(|error| error.to_string())?;
        // Built once, on whichever thread asks first; the rest wait for it and then
        // share it.
        if self.loader.get().is_none() {
            let built = otlyra_net::Loader::new().map_err(|error| error.to_string())?;
            let _ = self.loader.set(built);
        }
        let loader = self.loader.get().expect("the loader was just built");

        let resource = loader
            .fetch_blocking(otlyra_net::LoadRequest::new(url))
            .map_err(|error| error.to_string())?;
        let charset = resource.charset();
        Ok(otlyra_app::fetcher::Loaded {
            bytes: resource.body,
            charset,
            content_type: resource.content_type,
            nosniff: resource.nosniff,
            final_url: resource.final_url,
        })
    }
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
