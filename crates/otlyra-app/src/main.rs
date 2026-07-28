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
use otlyra_platform::{RendererBackend, StartupTrace, Viewport, WindowConfig};

/// Evaluated by the first statement in `main` and carried to the platform loop,
/// so startup milestones share one process-level origin.
static PROCESS_STARTED: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);

/// The one startup trace, measured from [`PROCESS_STARTED`]. The bootstrap marks
/// its early milestones here; `window_config` hands the same handle to the loop,
/// which records the rest and folds them into the startup report.
static STARTUP_TRACE: std::sync::LazyLock<StartupTrace> =
    std::sync::LazyLock::new(|| StartupTrace::new(*PROCESS_STARTED));

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

    /// Draw the page this much larger than its own pixels.
    ///
    /// Not the scale factor and not the text size: a zoom makes the CSS pixel
    /// itself larger for the page and leaves the browser's own interface alone,
    /// so a zoomed page reflows into the fewer pixels the window now holds
    /// rather than being magnified.
    #[arg(long, default_value_t = 1.0)]
    zoom: f32,

    /// Write the frame's display list to this path as JSON, then exit.
    ///
    /// Needs neither a GPU nor a rasterizer, so it is the cheapest way to answer
    /// "what did the browser decide to draw?".
    #[arg(long, value_name = "PATH")]
    dump_display_list: Option<PathBuf>,

    /// Draw the page alone, with none of the browser's own interface.
    ///
    /// For a screenshot that is going to be compared with one from another browser:
    /// the page has to start at the top of the picture.
    #[arg(long)]
    no_interface: bool,

    /// Touch nothing this machine keeps: no cookie file, no bookmarks, no
    /// keychain.
    ///
    /// A run that renders a page and exits has no business holding somebody's
    /// signed-in sessions, and asking the keychain for the key to them puts a
    /// dialog in front of whoever started it — on macOS, once per rebuilt
    /// binary, because the keychain decides by code signature and a fresh build
    /// has a fresh one.
    ///
    /// `--screenshot` never persists anything either way, and `--no-interface`
    /// implies this: with the browser's own interface hidden there is no way to
    /// see or add a bookmark, so keeping them is cost with nothing on the other
    /// side of it.
    #[arg(long)]
    no_secrets: bool,

    /// Which rasterizer to use.
    #[arg(long, value_enum, default_value_t = Renderer::Skia)]
    renderer: Renderer,

    /// Which palette to answer `prefers-color-scheme` with.
    ///
    /// A window follows the system and needs no flag; a screenshot has no window
    /// to be told by one, and a reference browser rendering the same page follows
    /// the system it is on — so a comparison has to be able to name the palette.
    #[arg(long, value_enum, value_name = "SCHEME")]
    color_scheme: Option<Palette>,

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

    /// Open the inspector on the page, docked under it.
    ///
    /// Useful with `--screenshot`: the panel is a thing worth looking at, and it
    /// cannot be opened from a command line otherwise.
    #[arg(long)]
    inspector: bool,

    /// Choose the element drawn at `X,Y` and show it in the inspector.
    ///
    /// The point is in logical pixels from the top left of the window, the same
    /// coordinates a click is in. Implies `--inspector`.
    #[arg(long, value_name = "X,Y", value_parser = point)]
    inspect_at: Option<(f64, f64)>,

    /// Which of the inspector's panes to show. Implies `--inspector`.
    #[arg(long, value_enum, value_name = "PANE")]
    inspect_pane: Option<InspectorPane>,

    /// Answer WebDriver BiDi on a port of the loopback, and drive nothing else.
    ///
    /// `0` asks the system for a free port, which is printed on stdout as a
    /// `ws://` address for whatever started this to read.
    #[arg(long, value_name = "PORT", num_args = 0..=1, default_missing_value = "9222")]
    bidi: Option<u16>,

    /// Drive the browser's whole window over the protocol, not just the page.
    ///
    /// With `--bidi` or `--mcp`. Input is delivered the way a window delivers it
    /// and a picture is the composited window — tab strip, toolbar, page and
    /// inspector, in window coordinates — which is what it takes to check the
    /// browser's own interface from outside it. Without it a driver gets the page
    /// alone, which is what a page automation client expects.
    #[arg(long)]
    window: bool,

    /// Answer the Model Context Protocol on stdin and stdout, and drive nothing
    /// else.
    ///
    /// For an agent, which has no client library but does have a list of tools.
    /// Every tool is one command against the same browser the protocol drives.
    #[arg(long)]
    mcp: bool,

    /// Write startup timings after the first presented frame, then exit.
    ///
    /// Used by the performance runner. The file is JSON and is written only
    /// after presentation succeeds, never merely after paint begins.
    #[arg(long, value_name = "PATH")]
    startup_report: Option<PathBuf>,

    /// Rasterize the retained window scene directly into a wgpu Metal texture.
    ///
    /// Experimental macOS measurement path. CPU Skia remains the default and
    /// continues to serve screenshots and deterministic tests.
    #[arg(long)]
    direct_gpu: bool,
}

impl Cli {
    /// The preferences to start from: what is saved, with what the command line
    /// overrides applied on top.
    fn settings(&self) -> otlyra_app::settings::Settings {
        let mut settings = otlyra_app::preferences::load();
        if let Some(palette) = self.color_scheme {
            settings.appearance = match palette {
                Palette::Light => otlyra_app::settings::Appearance::Light,
                Palette::Dark => otlyra_app::settings::Appearance::Dark,
            };
        }
        settings
    }

    /// The protocol session this command line asks for: the whole window, or the
    /// page alone.
    fn session(&self, browser: Browser) -> otlyra_app::bidi::Session {
        let viewport = (self.width, self.height);
        match self.window {
            true => otlyra_app::bidi::Session::windowed(browser, viewport),
            false => otlyra_app::bidi::Session::new(browser, viewport),
        }
    }

    /// The viewport `--screenshot` and `--dump-display-list` render at.
    fn viewport(&self) -> Viewport {
        Viewport::new(
            (f64::from(self.width) * self.scale_factor).round() as u32,
            (f64::from(self.height) * self.scale_factor).round() as u32,
            self.scale_factor,
        )
    }
}

/// The palette `--color-scheme` names.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Palette {
    /// Dark on light.
    Light,
    /// Light on dark.
    Dark,
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

/// Answer the protocol until the process is stopped.
///
/// No window: a driver asks for frames and gets them offscreen, so this needs
/// neither an event loop nor a compositor. Attaching to a window a person is
/// looking at is a later transport onto the same vocabulary.
fn serve_bidi(port: u16, cli: &Cli) -> Result<(), Box<dyn std::error::Error>> {
    let server = otlyra_app::bidi::listen(port)?;
    // On stdout rather than through the log: whatever started this reads one
    // line to learn where to connect, and the log belongs to a person.
    println!("{}", server.url());
    std::io::Write::flush(&mut std::io::stdout())?;
    tracing::info!(address = %server.address(), "answering WebDriver BiDi");

    let browser = browser(cli.settings());
    let mut session = cli.session(browser);
    loop {
        server.serve_one(&mut session)?;
    }
}

fn main() -> ExitCode {
    let _ = *PROCESS_STARTED;
    STARTUP_TRACE.mark("main_entered");
    observability::init();
    let cli = Cli::parse();
    STARTUP_TRACE.mark("cli_parsed");

    // Answering the protocol is a mode of its own: a driver navigates, so a
    // document named on the command line would be a page the first command
    // replaces.
    // Nothing was named on the command line, so what happens is what the
    // preferences say happens.
    if cli.url.is_none() && cli.file.is_none() && !cli.mcp && cli.bidi.is_none() {
        let settings = cli.settings();
        STARTUP_TRACE.mark("preferences_ready");
        let mut browser = browser(settings);
        STARTUP_TRACE.mark("browser_ready");
        let start = browser.settings_on_start();
        if start == otlyra_app::settings::OnStart::Home {
            browser.go_home();
        }
        return match cli.screenshot.as_deref() {
            Some(path) => match screenshot(&mut browser, &cli, path) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("otlyra: {error}");
                    ExitCode::FAILURE
                }
            },
            None => {
                open_inspector(&mut browser, &cli);
                match run_windowed(&cli, &mut browser) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => {
                        eprintln!("otlyra: {error}");
                        ExitCode::FAILURE
                    }
                }
            }
        };
    }

    if cli.mcp {
        // stdout is the wire from here on. Everything this program says about
        // itself already goes to stderr, which is what makes that safe.
        let browser = browser(cli.settings());
        let mut session = cli.session(browser);
        let input = std::io::BufReader::new(std::io::stdin().lock());
        return match otlyra_app::mcp::serve(&mut session, input, std::io::stdout().lock()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("otlyra: {error}");
                ExitCode::FAILURE
            }
        };
    }

    if let Some(port) = cli.bidi {
        return match serve_bidi(port, &cli) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("otlyra: {error}");
                ExitCode::FAILURE
            }
        };
    }

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
            let mut browser = browser(cli.settings());
            run_windowed(&cli, &mut browser)
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
        let settings = cli.settings();
        STARTUP_TRACE.mark("preferences_ready");
        let mut browser = browser(settings);
        STARTUP_TRACE.mark("browser_ready");
        browser.open_system(page);
        return match cli.screenshot.as_deref() {
            Some(path) => Ok(write_screenshot(&mut browser, cli.viewport(), path)?),
            None => Ok(run_windowed(cli, &mut browser)?),
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
        let mut browser = browser(cli.settings());
        if cli.no_interface {
            browser.hide_interface();
        }
        // Before the navigation, because a page picks between the pictures it
        // offers while it loads and the load can finish before the first frame.
        browser.set_viewport(cli.viewport());
        browser.set_zoom(cli.zoom);
        browser.navigate(&match &source {
            Source::Url(url) => url.clone(),
            Source::File(path) => path.display().to_string(),
        });
        return match cli.screenshot.as_deref() {
            Some(path) => screenshot(&mut browser, cli, path),
            None => {
                open_inspector(&mut browser, cli);
                Ok(run_windowed(cli, &mut browser)?)
            }
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
        let mut boxes = styled_boxes(&parsed.document, cli.width, cli.height);
        let mut text = otlyra_text::TextEngine::new();
        let fragments = otlyra_layout::layout(
            &mut boxes,
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
    let settings = cli.settings();
    STARTUP_TRACE.mark("preferences_ready");
    let mut browser = browser(settings);
    STARTUP_TRACE.mark("browser_ready");
    browser.set_viewport(cli.viewport());
    browser.set_zoom(cli.zoom);
    browser.navigate(&match &source {
        Source::Url(url) => url.clone(),
        Source::File(path) => path.display().to_string(),
    });

    match cli.screenshot.as_deref() {
        Some(path) => screenshot(&mut browser, cli, path)?,
        None => {
            open_inspector(&mut browser, cli);
            run_windowed(cli, &mut browser)?;
        }
    }
    Ok(())
}

/// Open the inspector if the command line asked for it.
fn open_inspector(browser: &mut Browser, cli: &Cli) {
    if cli.inspector || cli.inspect_at.is_some() || cli.inspect_pane.is_some() {
        browser.inspector_mut().open = true;
    }
    if let Some(pane) = cli.inspect_pane {
        use otlyra_app::inspector::{Pane, Sidebar};
        // Styles and Layout are sidebars inside Elements rather than panes of
        // their own — the same shape every other browser's devtools have — so
        // naming one on the command line chooses the pane *and* the sidebar.
        let (pane, sidebar) = match pane {
            InspectorPane::Elements => (Pane::Elements, Sidebar::Node),
            InspectorPane::Rules => (Pane::Elements, Sidebar::Rules),
            InspectorPane::Styles => (Pane::Elements, Sidebar::Styles),
            InspectorPane::Layout => (Pane::Elements, Sidebar::Layout),
            InspectorPane::Console => (Pane::Console, Sidebar::Node),
            InspectorPane::Network => (Pane::Network, Sidebar::Node),
            InspectorPane::Accessibility => (Pane::Accessibility, Sidebar::Node),
        };
        browser.inspector_mut().pane = pane;
        browser.inspector_mut().sidebar = sidebar;
    }
}

/// The panes, as the command line spells them.
///
/// A copy of the enum the panel owns rather than the enum itself: deriving
/// clap's traits on it would put a command-line vocabulary into a module that
/// has no command line, and there are three of them.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum InspectorPane {
    Elements,
    Rules,
    Styles,
    Layout,
    Console,
    Network,
    Accessibility,
}

/// Write one screenshot, having settled whatever the command line asked to see.
///
/// A screenshot has one frame to get right and no event loop to be woken by, so
/// this is the one place that waits for a load. Choosing an element needs a
/// frame of its own first: the hit test reads what the last frame drew, which is
/// the same rule a click follows.
fn screenshot(
    browser: &mut Browser,
    cli: &Cli,
    path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    browser.wait_for_load(LOAD_TIMEOUT);
    browser.prepare_frame(cli.viewport(), LOAD_TIMEOUT);
    open_inspector(browser, cli);
    if let Some((x, y)) = cli.inspect_at {
        write_screenshot(browser, cli.viewport(), path)?;
        browser.inspect_at(x, y);
    }
    write_screenshot(browser, cli.viewport(), path)?;
    Ok(())
}

/// A point written as `X,Y`, for the flag that names one.
fn point(text: &str) -> Result<(f64, f64), String> {
    let (x, y) = text
        .split_once(',')
        .ok_or_else(|| format!("expected X,Y and got {text:?}"))?;
    let parse = |value: &str| {
        value
            .trim()
            .parse::<f64>()
            .map_err(|error| format!("{value:?} is not a number: {error}"))
    };
    Ok((parse(x)?, parse(y)?))
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
        startup_origin: *PROCESS_STARTED,
        startup_report: cli.startup_report.clone(),
        startup_trace: Some(STARTUP_TRACE.clone()),
        renderer: if cli.direct_gpu {
            RendererBackend::DirectGpu
        } else {
            RendererBackend::Cpu
        },
    }
}

/// Open the window, with the system clipboard attached first.
///
/// The one place the real pasteboard enters: a window means a person, and a
/// person expects ⌘C to reach the clipboard the rest of their machine uses.
/// Every headless mode keeps the in-memory default.
fn run_windowed(cli: &Cli, browser: &mut Browser) -> Result<(), otlyra_app::AppError> {
    browser.set_clipboard(Box::new(otlyra_app::clipboard::System::new()));
    if cli.no_secrets || cli.no_interface {
        // A window opened to look at one page. It gets the in-memory stores, so
        // it can neither read nor overwrite what a real session kept — and it
        // never asks the keychain, which is the difference between a rebuild
        // that opens and one that opens a dialog first.
        tracing::info!("keeping nothing from this run: no cookie file, no bookmarks, no keychain");
        return run_window(window_config(cli), browser);
    }
    // The bookmarks the last run kept: a window means a person, and what they
    // chose to keep outlives the process.
    browser.persist_bookmarks();
    // And the sessions the last run was signed in with, for the same reason and
    // one degree more so: this file is what makes closing the browser not mean
    // signing out of everything.
    browser.persist_cookies();
    run_window(window_config(cli), browser)
}

/// The real loader: `otlyra-net` over HTTP, the filesystem for a `file:` URL.
///
/// One client behind a `OnceLock`, shared by every outstanding fetch: the client
/// owns the connection pool, so one per request would be several pools for no gain.
/// Built on first use rather than at startup, so opening a browser costs no TLS
/// setup until something is fetched.
struct NetLoader {
    loader: std::sync::OnceLock<otlyra_net::Loader>,
    /// The jar this loader fills and sends from, shared with the browser that
    /// shows it. Handed in rather than made here, because the loader is built
    /// before the browser and both have to end up holding the same one.
    jar: otlyra_net::SharedJar,
    /// What has already been fetched, shared for the same reason.
    cache: otlyra_net::SharedCache,
}

/// A browser and the loader that fills its jar.
///
/// The jar is made first because the loader needs it, and the browser is given
/// the store afterwards. In memory until a shell asks for a file: see
/// `Browser::persist_cookies`.
fn browser(settings: otlyra_app::settings::Settings) -> Browser {
    let cookies = otlyra_app::cookies::CookieStore::in_memory();
    let cache = browser_cache();
    let loader = NetLoader {
        loader: std::sync::OnceLock::new(),
        jar: cookies.jar(),
        cache: std::sync::Arc::clone(&cache),
    };
    let mut browser = Browser::with_async_loader(loader, settings);
    browser.set_cookie_store(cookies);
    browser.set_cache(cache);
    browser
}

/// The one HTTP cache, made here because the loader needs it before the browser
/// exists and the two have to hold the same one.
///
/// Two tiers. The one in memory answers the second request for the same picture
/// on the same page; the one on disk answers the thing a person actually notices,
/// which is that the site they open every morning was fetched from nothing every
/// morning. Where it is written is decided here rather than in the library, for
/// the same reason the cookie file is: a crate that chose a directory on somebody's
/// disk because it was linked would be a crate doing something nobody asked for.
///
/// With nowhere to write — a platform that will not say where a program's files
/// go, or a directory that cannot be made — the memory tier is the whole of it,
/// and the browser says so rather than failing to start over a cache.
fn browser_cache() -> otlyra_net::SharedCache {
    let cache: otlyra_net::SharedCache =
        std::sync::Arc::new(std::sync::Mutex::new(otlyra_net::cache::Cache::new()));
    let Some(directory) =
        otlyra_app::preferences::directory().map(|directory| directory.join(CACHE_FOLDER))
    else {
        tracing::warn!("nowhere to keep a cache between runs");
        return cache;
    };

    // Opening it reads the headers of every file in there, which for a cache of a
    // few thousand entries is a few thousand short reads — and the startup budget
    // is measured to the first frame, which fetches nothing. So the browser starts
    // and the disk tier arrives when it arrives. Anything fetched before then
    // missed a cache, which costs a request and is never a wrong answer.
    let attaching = std::sync::Arc::clone(&cache);
    if let Err(error) = std::thread::Builder::new()
        .name("otlyra-cache-open".to_owned())
        .spawn(
            move || match otlyra_net::cache::Disk::open(&directory, DISK_CACHE_BYTES) {
                Ok(disk) => {
                    tracing::info!(
                        at = %directory.display(),
                        entries = disk.len(),
                        bytes = disk.bytes(),
                        "the cache on disk was opened"
                    );
                    attaching
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .attach_disk(disk);
                }
                Err(error) => {
                    tracing::warn!(at = %directory.display(), %error, "no cache on disk this run");
                }
            },
        )
    {
        tracing::warn!(%error, "no thread to open the cache on; this run keeps nothing");
    }
    cache
}

/// What the cache directory is called inside the browser's own directory.
const CACHE_FOLDER: &str = "cache";

/// How much of somebody's disk this may take.
///
/// A quarter of a gigabyte, which is a fraction of what a shipping browser takes
/// and is enough for the pictures, stylesheets and fonts of every site a person
/// visits regularly. A cache that has to be explained to whoever finds it is too
/// large.
const DISK_CACHE_BYTES: usize = 256 * 1024 * 1024;

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
            text_scale: 1.0,
            color_scheme: otlyra_css::cascade::ColorScheme::Light,
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

impl otlyra_app::fetcher::AsyncLoader for NetLoader {
    fn fetch(
        self: std::sync::Arc<Self>,
        input: String,
        body: Option<otlyra_net::Body>,
        cache: otlyra_net::CacheMode,
        headers: Vec<(String, String)>,
    ) -> otlyra_app::fetcher::Fetching {
        Box::pin(async move {
            // A path typed into the address bar becomes the `file:` URL it names, so
            // that what the bar shows is an address and not a filename — and so that
            // a relative link on the page has something to resolve against.
            if let Some(url) = file_url(&input) {
                // A file has nothing to receive a body with, so a form aimed at one
                // is read rather than refused — which is what the references do.
                let path = url
                    .to_file_path()
                    .map_err(|()| format!("not a path: {input}"))?;
                let bytes = tokio::fs::read(&path)
                    .await
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                // A filesystem has no `Content-Type`, so the extension stands in for
                // one — which is what every browser does with a `file:` URL, and is
                // why opening a `.css` shows the stylesheet rather than rendering it.
                return Ok(otlyra_app::fetcher::Loaded {
                    bytes,
                    content_type: content_type_of(&path),
                    final_url: url.to_string(),
                    ..Default::default()
                });
            }

            otlyra_net::install_crypto_provider();
            // A `data:` URL carries its own bytes, so it is read rather than
            // resolved: normalizing it would refuse it, because it is not an address
            // anything can be navigated to and the two rules are deliberately not
            // the same one.
            let url = if input.trim_start().starts_with("data:") {
                url::Url::parse(input.trim()).map_err(|error| error.to_string())?
            } else {
                otlyra_net::normalize(&input).map_err(|error| error.to_string())?
            };
            // Built once, by whichever fetch asks first; a second that raced it
            // drops its own and uses the one that won, so there is still exactly one
            // connection pool.
            if self.loader.get().is_none() {
                let built = otlyra_net::Loader::new()
                    .map_err(|error| error.to_string())?
                    .with_jar(std::sync::Arc::clone(&self.jar))
                    .with_cache(std::sync::Arc::clone(&self.cache));
                let _ = self.loader.set(built);
            }
            let loader = self.loader.get().expect("the loader was just built");

            let request = match body {
                Some(body) => otlyra_net::LoadRequest::post(url, body),
                None => otlyra_net::LoadRequest::new(url),
            }
            .caching(cache)
            .with_headers(headers);
            // The transport on the browser's own runtime: no thread is held while
            // the server thinks, and no runtime is created to wait for it.
            let resource = loader
                .fetch(request)
                .await
                .map_err(|error| error.to_string())?;
            let charset = resource.charset();
            Ok(otlyra_app::fetcher::Loaded {
                bytes: resource.body,
                charset,
                content_type: resource.content_type,
                nosniff: resource.nosniff,
                status: Some(resource.status),
                request_headers: resource.request_headers,
                response_headers: resource.response_headers,
                final_url: resource.final_url,
                served: resource.served,
            })
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
