//! The render thread: where a window's frames are rasterized and presented.
//!
//! The window's thread builds a frame — it asks the painter for its layers — and
//! hands it here. This thread owns everything that touches pixels or the GPU:
//! the retained surface and its layer history, the presenter and its swapchain,
//! and the Ganesh surface of the direct GPU path. Rasterizing a heavy page and
//! waiting for the display to take a drawable therefore happen while the window's
//! thread goes on reading input, which is the whole of the point: before this, a
//! frame that took a third of a second to rasterize was a third of a second in
//! which no key, click or wheel notch was even looked at.
//!
//! One frame is in flight at a time. The window's thread does not build the next
//! frame until this one reports, so a slow frame is followed by one frame built
//! from everything that changed meanwhile, not by a queue of stale ones. And
//! because presenting blocks on the display once the swapchain is full, the
//! report arriving *is* the display's pace: an animation that asks for the next
//! frame whenever the last one lands runs at the display's rate, whatever that is.
//!
//! Nothing here names winit. The thread is told how to wake the loop with a
//! closure, which keeps the wall in `event_loop.rs` where it is.

use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::compositor::{SceneCompositor, Upload};
use crate::present::{DirectFrame, GpuContext, Presented, Presenter, PresenterSeed};
use crate::{PlatformError, RendererBackend, Scene, StartupTrace, Viewport};

/// What a frame is made of, as the window's thread built it.
pub(crate) enum FrameContent {
    /// Retained layers, rasterized here against the layer history.
    Scene(Scene),
    /// A whole frame the painter drew itself, already RGBA8.
    ///
    /// For a painter that has not adopted layers. Its pixels replace everything
    /// the layer history knew about the surface.
    Pixels(Vec<u8>),
}

/// One frame to draw.
pub(crate) struct FrameJob {
    pub content: FrameContent,
    pub viewport: Viewport,
    /// The startup trace, on the first frame only.
    pub trace: Option<StartupTrace>,
}

/// What one frame cost and how it ended.
pub(crate) struct FrameReport {
    pub outcome: Presented,
    pub viewport: Viewport,
    /// Device pixels re-rasterized for this frame.
    pub rasterized_pixels: u64,
    /// Bytes copied to the GPU for this frame, counted only when it presented.
    pub uploaded_bytes: u64,
    /// The trace the frame was submitted with, handed back for the milestones
    /// that belong to the window's thread.
    pub trace: Option<StartupTrace>,
}

/// What the render thread tells the window's thread.
pub(crate) enum RenderReport {
    /// The GPU device exists and frames can be submitted.
    Ready,
    /// A submitted frame is finished, one way or another.
    Frame(FrameReport),
    /// Something failed that no retry will fix.
    Failed(PlatformError),
}

enum Command {
    Frame(FrameJob),
    Shutdown,
}

/// The window's handle on its render thread.
///
/// Dropping it stops the thread and waits for it, so everything the thread holds
/// — the swapchain among it, and with it a handle on the window — is gone before
/// the window is.
pub(crate) struct RenderThread {
    commands: mpsc::Sender<Command>,
    reports: mpsc::Receiver<RenderReport>,
    handle: Option<JoinHandle<()>>,
}

impl RenderThread {
    /// Start the thread. It creates the GPU device from `seed` first — which is
    /// the slow part of startup, and is why this happens off the window's
    /// thread — and reports [`RenderReport::Ready`] once frames can be drawn.
    ///
    /// `wake` is called after every report is posted, to wake the window's loop.
    pub(crate) fn spawn(
        seed: PresenterSeed,
        viewport: Viewport,
        backend: RendererBackend,
        wake: Box<dyn Fn() + Send>,
    ) -> std::io::Result<Self> {
        let (commands, inbox) = mpsc::channel::<Command>();
        let (outbox, reports) = mpsc::channel::<RenderReport>();
        let handle = std::thread::Builder::new()
            .name("otlyra-render".to_owned())
            .spawn(move || {
                let report = |message| {
                    if outbox.send(message).is_ok() {
                        wake();
                    }
                };
                let presenter = match Presenter::new(seed, viewport) {
                    Ok(presenter) => presenter,
                    Err(error) => {
                        report(RenderReport::Failed(error.into()));
                        return;
                    }
                };
                report(RenderReport::Ready);

                let mut renderer = Renderer {
                    direct: None,
                    gpu: None,
                    presenter,
                    compositor: SceneCompositor::new(),
                    backend,
                };
                while let Ok(Command::Frame(job)) = inbox.recv() {
                    report(match renderer.draw(job) {
                        Ok(frame) => RenderReport::Frame(frame),
                        Err(error) => RenderReport::Failed(error),
                    });
                }
                // `renderer` drops here, on the thread that made its GPU objects:
                // the Ganesh surface first, then the presenter it wraps.
            })?;
        Ok(Self {
            commands,
            reports,
            handle: Some(handle),
        })
    }

    /// Hand a frame over. `false` means the thread is gone, which it only is after
    /// it has reported a failure.
    pub(crate) fn submit(&self, job: FrameJob) -> bool {
        self.commands.send(Command::Frame(job)).is_ok()
    }

    /// The next report, if one has arrived.
    pub(crate) fn try_report(&self) -> Option<RenderReport> {
        self.reports.try_recv().ok()
    }

    /// The next report, waiting at most `timeout` for it.
    pub(crate) fn wait_report(&self, timeout: Duration) -> Option<RenderReport> {
        self.reports.recv_timeout(timeout).ok()
    }
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            tracing::error!("the render thread panicked");
        }
    }
}

/// Everything the render thread owns.
///
/// Field order is the drop order, and the drop order is a contract: the Ganesh
/// surface wraps a texture of the presenter's device, and the context draws on
/// the presenter's device and queue, so both go first.
struct Renderer {
    /// The GPU path's retained surface. Created, used and dropped only on this
    /// thread, as the context it draws through is.
    direct: Option<DirectFrame>,
    /// The Ganesh context every GPU surface of this window shares, made the first
    /// time one is needed and kept across resizes.
    gpu: Option<GpuContext>,
    presenter: Presenter,
    /// The retained CPU surface and the layer history both backends plan from.
    compositor: SceneCompositor,
    backend: RendererBackend,
}

impl Renderer {
    fn draw(&mut self, job: FrameJob) -> Result<FrameReport, PlatformError> {
        let FrameJob {
            content,
            viewport,
            trace,
        } = job;
        match content {
            FrameContent::Scene(scene) if self.backend == RendererBackend::Gpu => {
                match self.prepare_direct(viewport) {
                    Ok(reallocated) => self.composite_direct(scene, viewport, trace, reallocated),
                    // A GPU the rasterizer cannot draw on is a window drawn on
                    // the CPU, not a window that stops drawing: from this frame
                    // on, and for the rest of the window's life.
                    Err(error) => {
                        tracing::warn!(%error, "drawing on the GPU failed; drawing on the CPU instead");
                        self.backend = RendererBackend::Cpu;
                        self.direct = None;
                        self.gpu = None;
                        self.compositor.discard();
                        self.composite(scene, viewport, trace)
                    }
                }
            }
            FrameContent::Scene(scene) => self.composite(scene, viewport, trace),
            FrameContent::Pixels(pixels) => self.present_whole(pixels, viewport, trace),
        }
    }

    /// Make sure there is a GPU surface of the viewport's size, and answer
    /// whether it is a new one — which holds no pixels, so the frame is whole.
    fn prepare_direct(&mut self, viewport: Viewport) -> Result<bool, crate::present::PresentError> {
        self.presenter.resize(viewport);
        let current = self
            .direct
            .as_ref()
            .is_some_and(|frame| frame.size() == (viewport.width, viewport.height));
        if current {
            return Ok(false);
        }
        if self.gpu.is_none() {
            self.gpu = Some(self.presenter.gpu_context()?);
        }
        let context = self.gpu.as_ref().expect("made just now");
        let previous = self.direct.take();
        self.direct = Some(self.presenter.create_direct(context, viewport, previous)?);
        Ok(true)
    }

    /// Rasterize the layers into the retained CPU surface, read back only what
    /// changed, and upload that.
    fn composite(
        &mut self,
        scene: Scene,
        viewport: Viewport,
        trace: Option<StartupTrace>,
    ) -> Result<FrameReport, PlatformError> {
        let composited = self.compositor.rasterize(&scene, viewport)?;
        let rasterized_pixels = composited.damage.pixels(viewport);
        if let Some(trace) = trace.as_ref() {
            trace.mark("chrome_raster_complete");
        }

        self.presenter.resize(viewport);
        let _present = tracing::info_span!("present", mode = "composite").entered();
        let (outcome, bytes) = match composited.upload {
            Upload::Unchanged => (self.presenter.reblit()?, 0),
            Upload::Full(pixels) => {
                if let Some(trace) = trace.as_ref() {
                    trace.mark("readback_complete");
                }
                let bytes = pixels.len() as u64;
                (
                    self.presenter
                        .present(&pixels, viewport.width, viewport.height)?,
                    bytes,
                )
            }
            Upload::Region { pixels, rect } => {
                if let Some(trace) = trace.as_ref() {
                    trace.mark("readback_complete");
                }
                let bytes = pixels.len() as u64;
                // A fresh staging texture cannot take a partial upload; it says
                // so as a dropped frame, and the next frame is whole.
                (
                    self.presenter
                        .present_rect(&pixels, viewport.width, viewport.height, rect)?,
                    bytes,
                )
            }
        };

        // The layers are remembered only once they reached the screen: a
        // dropped frame must be recomposed, and forcing the next one whole is
        // the safe way to do that.
        if outcome == Presented::Frame {
            self.compositor.commit(&scene);
        } else {
            self.compositor.discard();
        }
        Ok(FrameReport {
            outcome,
            viewport,
            rasterized_pixels,
            uploaded_bytes: if outcome == Presented::Frame {
                bytes
            } else {
                0
            },
            trace,
        })
    }

    /// The GPU path: rasterize into the presenter's retained wgpu texture and
    /// sample it straight into the swapchain — no read-back, no upload.
    fn composite_direct(
        &mut self,
        scene: Scene,
        viewport: Viewport,
        trace: Option<StartupTrace>,
        reallocated: bool,
    ) -> Result<FrameReport, PlatformError> {
        let damage = self.compositor.plan(&scene, reallocated);
        let direct = self.direct.as_mut().expect("direct frame prepared");
        direct.rasterize(&scene, &damage);
        if let Some(trace) = trace.as_ref() {
            trace.mark("chrome_raster_complete");
            // This backend has no readback; the milestone records that the stage
            // took no time, and keeps the startup report's schema.
            trace.mark("readback_complete");
        }

        let _present = tracing::info_span!("present", mode = "gpu").entered();
        let outcome = self.presenter.present_direct(direct)?;
        if outcome == Presented::Frame {
            self.compositor.commit(&scene);
        } else {
            self.compositor.discard();
        }
        Ok(FrameReport {
            outcome,
            viewport,
            rasterized_pixels: damage.pixels(viewport),
            uploaded_bytes: 0,
            trace,
        })
    }

    /// Upload a whole frame the painter drew itself.
    fn present_whole(
        &mut self,
        pixels: Vec<u8>,
        viewport: Viewport,
        trace: Option<StartupTrace>,
    ) -> Result<FrameReport, PlatformError> {
        // What is on the surface now is not what the layer history describes.
        self.compositor.discard();
        let _present = tracing::info_span!("present", mode = "blit").entered();
        if let Some(trace) = trace.as_ref() {
            trace.mark("readback_complete");
        }
        self.presenter.resize(viewport);
        let bytes = pixels.len() as u64;
        let outcome = self
            .presenter
            .present(&pixels, viewport.width, viewport.height)?;
        Ok(FrameReport {
            outcome,
            viewport,
            rasterized_pixels: u64::from(viewport.width) * u64::from(viewport.height),
            uploaded_bytes: if outcome == Presented::Frame {
                bytes
            } else {
                0
            },
            trace,
        })
    }
}
