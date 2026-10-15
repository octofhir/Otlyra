//! GPU presentation: upload the rasterized frame and blit it to the swapchain.

use crate::{Scene, Viewport, compositor::Damage};
use otlyra_gfx::PaintTarget as _;

/// Format of the staging texture. `*Srgb` so the sampler decodes and the swapchain
/// re-encodes; the rasterizer hands us sRGB-encoded premultiplied RGBA8.
const STAGING_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Format of the texture Skia draws the window into on the GPU: the same bytes as
/// the staging texture, stored plain and sampled through an `*Srgb` view.
const DIRECT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const BLIT_SHADER: &str = r#"
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    var out: VertexOutput;
    let x = f32(i32(index & 1u) * 4 - 1);
    let y = f32(i32(index >> 1u) * 4 - 1);
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

@group(0) @binding(0) var frame_texture: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(frame_texture, frame_sampler, in.uv);
}
"#;

pub(crate) struct Presenter {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    staging: Option<Staging>,
}

/// The platform-bound part of presentation, created while the window handle is
/// available on the event-loop thread and then moved to the render thread, which
/// makes the device from it.
pub(crate) struct PresenterSeed {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
}

/// A backend instance created without a window handle on the startup worker.
pub(crate) struct PresenterInstance(wgpu::Instance);

struct Staging {
    texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    width: u32,
    height: u32,
}

/// The Ganesh context a window's surfaces share, where there is one.
#[cfg(target_os = "macos")]
pub(crate) type GpuContext = otlyra_gfx::GpuContext;

/// No GPU rasterizer on this platform yet: a window draws on the CPU.
#[cfg(not(target_os = "macos"))]
pub(crate) struct GpuContext;

/// A retained wgpu texture that Ganesh draws into in place.
///
/// Field order is the lifetime contract: the Skia surface drops before the bind
/// group, and both drop before the wgpu texture whose `MTLTexture` it wraps.
#[cfg(target_os = "macos")]
pub(crate) struct DirectFrame {
    painter: otlyra_gfx::SkiaPainter,
    bind_group: wgpu::BindGroup,
    _texture: wgpu::Texture,
    width: u32,
    height: u32,
}

#[cfg(not(target_os = "macos"))]
pub(crate) struct DirectFrame;

#[cfg(target_os = "macos")]
impl DirectFrame {
    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Draw the retained scene directly into the wgpu-owned texture.
    pub(crate) fn rasterize(&mut self, scene: &Scene, damage: &Damage) {
        match damage {
            Damage::Unchanged => {}
            Damage::Full => {
                self.painter.reset();
                for layer in &scene.layers {
                    crate::compositor::draw_layer(&mut self.painter, layer);
                }
                self.painter.flush_and_submit();
            }
            Damage::Region(rect) => {
                let clip = otlyra_gfx::kurbo::Rect::new(
                    f64::from(rect.x),
                    f64::from(rect.y),
                    f64::from(rect.x + rect.width),
                    f64::from(rect.y + rect.height),
                );
                self.painter.clip_to(clip);
                self.painter.clear_rect(clip);
                for layer in &scene.layers {
                    if layer.rect.intersects(rect) {
                        crate::compositor::draw_layer(&mut self.painter, layer);
                    }
                }
                self.painter.reset_clip();
                self.painter.flush_and_submit();
            }
            Damage::Scrolled(scroll) => {
                crate::compositor::apply_scroll(&mut self.painter, scene, scroll);
                self.painter.flush_and_submit();
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl DirectFrame {
    pub(crate) fn size(&self) -> (u32, u32) {
        (0, 0)
    }

    pub(crate) fn rasterize(&mut self, _scene: &Scene, _damage: &Damage) {}
}

impl Presenter {
    /// Metal can discover its backend before a native window exists. Other
    /// platforms keep the display-bound bootstrap below (notably GLES/Wayland).
    #[cfg(target_os = "macos")]
    pub(crate) fn instance_without_window() -> PresenterInstance {
        let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
        descriptor.backends = wgpu::Backends::METAL;
        PresenterInstance(wgpu::Instance::new(descriptor))
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn instance<W>(window: W) -> PresenterInstance
    where
        W: raw_window_handle::HasDisplayHandle + std::fmt::Debug + Send + Sync + 'static,
    {
        PresenterInstance(wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(window)),
        ))
    }

    pub(crate) fn prepare<W>(
        instance: PresenterInstance,
        window: W,
    ) -> Result<PresenterSeed, PresentError>
    where
        W: wgpu::WindowHandle + raw_window_handle::HasDisplayHandle + std::fmt::Debug + 'static,
    {
        let PresenterInstance(instance) = instance;
        // A window-independent Metal instance gets both handles here, through
        // the safe owning target; no raw handle or borrowed window can escape.
        #[cfg(target_os = "macos")]
        let target = wgpu::SurfaceTarget::DisplayAndWindow(Box::new(window));
        #[cfg(not(target_os = "macos"))]
        let target = wgpu::SurfaceTarget::Window(Box::new(window));
        let surface = instance.create_surface(target)?;
        Ok(PresenterSeed { instance, surface })
    }

    pub(crate) fn new(
        seed: PresenterSeed,
        viewport: Viewport,
        trace: Option<&crate::StartupTrace>,
    ) -> Result<Self, PresentError> {
        let PresenterSeed { instance, surface } = seed;
        // Adapter and device creation happen once on the startup worker. The
        // window event loop and the frame path never await them.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))?;
        if let Some(trace) = trace {
            trace.mark("gpu_adapter_ready");
        }

        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("otlyra-device"),
                required_features: wgpu::Features::empty(),
                required_limits:
                    wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            }))?;
        if let Some(trace) = trace {
            trace.mark("gpu_device_ready");
        }

        let capabilities = surface.get_capabilities(&adapter);
        let format = capabilities
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .unwrap_or_else(|| capabilities.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: viewport.width,
            height: viewport.height,
            // Fifo blocks on vsync instead of spinning, which is what keeps the
            // idle-CPU budget honest.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: capabilities.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);
        if let Some(trace) = trace {
            trace.mark("gpu_surface_configured");
        }

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("otlyra-blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("otlyra-blit-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("otlyra-blit-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("otlyra-blit-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(format.into())],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("otlyra-blit-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let info = adapter.get_info();
        tracing::info!(adapter = %info.name, backend = ?info.backend, ?format, "gpu surface ready");
        if let Some(trace) = trace {
            trace.mark("gpu_pipeline_ready");
        }

        Ok(Self {
            surface,
            device,
            queue,
            config,
            pipeline,
            bind_group_layout,
            sampler,
            staging: None,
        })
    }

    pub(crate) fn resize(&mut self, viewport: Viewport) {
        if (self.config.width, self.config.height) == (viewport.width, viewport.height) {
            return;
        }
        self.config.width = viewport.width;
        self.config.height = viewport.height;
        self.surface.configure(&self.device, &self.config);
    }

    /// Ensure a staging texture of exactly `width` x `height` exists. Returns
    /// `true` when a fresh texture was allocated, so the caller knows the texture
    /// holds no retained pixels and a partial upload would leave it undefined.
    fn ensure_staging(&mut self, width: u32, height: u32) -> bool {
        let current = self
            .staging
            .as_ref()
            .is_some_and(|staging| staging.width == width && staging.height == height);
        if current {
            return false;
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("otlyra-frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: STAGING_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("otlyra-frame-bind-group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        self.staging = Some(Staging {
            texture,
            bind_group,
            width,
            height,
        });
        true
    }

    /// A Ganesh context on this presenter's Metal device and queue, for every
    /// surface the window will draw into.
    #[cfg(target_os = "macos")]
    pub(crate) fn gpu_context(&self) -> Result<GpuContext, PresentError> {
        // SAFETY: both guards expose objects owned by this Presenter, which the
        // render thread keeps alive longer than the context: `Renderer` drops the
        // direct frame and the context before the presenter.
        unsafe {
            let hal_device = self
                .device
                .as_hal::<wgpu::hal::api::Metal>()
                .ok_or(PresentError::DirectGpuUnsupported)?;
            let hal_queue = self
                .queue
                .as_hal::<wgpu::hal::api::Metal>()
                .ok_or(PresentError::DirectGpuUnsupported)?;
            Ok(otlyra_gfx::GpuContext::metal(
                &**hal_device.raw_device() as *const _ as *mut std::ffi::c_void,
                hal_queue.as_raw() as *const _ as *mut std::ffi::c_void,
            )?)
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn gpu_context(&self) -> Result<GpuContext, PresentError> {
        Err(PresentError::DirectGpuUnsupported)
    }

    /// Create the retained Metal texture the window draws into, in `context`,
    /// taking over whatever `previous` — the texture it replaces — had cached.
    #[cfg(target_os = "macos")]
    pub(crate) fn create_direct(
        &self,
        context: &GpuContext,
        viewport: Viewport,
        previous: Option<DirectFrame>,
    ) -> Result<DirectFrame, PresentError> {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("otlyra-direct-frame"),
            size: wgpu::Extent3d {
                width: viewport.width,
                height: viewport.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: DIRECT_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[STAGING_FORMAT],
        });
        // Skia writes the sRGB-encoded bytes the CPU rasterizer would have, into
        // a plain RGBA8 texture; the sampler reads them through an sRGB view, as
        // it reads the staging texture. An sRGB texture under Skia encoded them a
        // second time on the way in, and the window came out washed out.
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(STAGING_FORMAT),
            ..Default::default()
        });
        // wgpu does not see what Skia draws, so a texture it has never written is
        // one it zeroes before its first use — the first time the swapchain
        // sampled it, after Skia had drawn the whole window into it. Clearing it
        // here, through wgpu, is what makes it wgpu's idea of initialised.
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("otlyra-direct-frame-init"),
            });
        {
            let target = texture.create_view(&wgpu::TextureViewDescriptor::default());
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("otlyra-direct-frame-init"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue.submit([encoder.finish()]);
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("otlyra-direct-frame-bind-group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        // SAFETY: the texture is this Presenter's, made from the device the
        // context was made for, an sRGB RGBA8 render target of the viewport's
        // size. `DirectFrame` drops the painter before its texture, and the render
        // thread drops the frame before Presenter.
        let mut painter = unsafe {
            let hal_texture = texture
                .as_hal::<wgpu::hal::api::Metal>()
                .ok_or(PresentError::DirectGpuUnsupported)?;
            otlyra_gfx::SkiaPainter::wrap_metal_texture_in(
                context,
                viewport.width,
                viewport.height,
                hal_texture.raw_handle() as *const _ as *mut std::ffi::c_void,
            )?
        };
        if let Some(mut previous) = previous {
            painter.adopt_caches(&mut previous.painter);
        }
        Ok(DirectFrame {
            painter,
            bind_group,
            _texture: texture,
            width: viewport.width,
            height: viewport.height,
        })
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn create_direct(
        &self,
        _context: &GpuContext,
        _viewport: Viewport,
        _previous: Option<DirectFrame>,
    ) -> Result<DirectFrame, PresentError> {
        Err(PresentError::DirectGpuUnsupported)
    }

    /// Acquire the next swapchain texture, or say why this frame cannot happen.
    fn acquire(&mut self) -> Result<Acquired, PresentError> {
        #[cfg(feature = "fault-injection")]
        if fault::take_occluded_acquire() {
            return Ok(Acquired::Skip(Presented::Occluded));
        }
        Ok(match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => Acquired::Frame(frame),
            // A lost or outdated swapchain is normal while a window is being
            // created, resized or moved between displays. Reconfigure and tell the
            // caller the frame never happened, so it can ask for another one — the
            // loop blocks in `Wait`, so a dropped frame nobody re-requests is a
            // window that stays black until the user happens to poke it.
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                tracing::debug!("swapchain lost or outdated; frame dropped");
                Acquired::Skip(Presented::Dropped)
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                tracing::debug!("swapchain timed out; frame dropped");
                Acquired::Skip(Presented::Dropped)
            }
            // The window is hidden. Asking again would spin against a window nobody
            // can see; winit wakes us when it is visible.
            wgpu::CurrentSurfaceTexture::Occluded => Acquired::Skip(Presented::Occluded),
            wgpu::CurrentSurfaceTexture::Validation => return Err(PresentError::SurfaceValidation),
        })
    }

    /// Blit the current staging texture onto `frame` and present it.
    ///
    /// The staging texture is sampled whole, so whatever it retains from earlier
    /// frames outside a partial upload's rectangle reaches the screen unchanged —
    /// which is exactly what makes an unchanged region cost nothing to re-present.
    fn blit_with(&mut self, frame: wgpu::SurfaceTexture, bind_group: &wgpu::BindGroup) {
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("otlyra-present"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("otlyra-blit-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
    }

    fn blit(&mut self, frame: wgpu::SurfaceTexture) {
        let bind_group = self
            .staging
            .as_ref()
            .expect("staging texture ensured")
            .bind_group
            .clone();
        self.blit_with(frame, &bind_group);
    }

    /// Present the retained direct texture without CPU readback or upload.
    #[cfg(target_os = "macos")]
    pub(crate) fn present_direct(
        &mut self,
        direct: &DirectFrame,
    ) -> Result<Presented, PresentError> {
        let bind_group = direct.bind_group.clone();
        let frame = match self.acquire()? {
            Acquired::Frame(frame) => frame,
            Acquired::Skip(outcome) => return Ok(outcome),
        };
        self.blit_with(frame, &bind_group);
        Ok(Presented::Frame)
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn present_direct(
        &mut self,
        _direct: &DirectFrame,
    ) -> Result<Presented, PresentError> {
        Err(PresentError::DirectGpuUnsupported)
    }

    /// Upload the whole frame (`pixels`, tightly packed premultiplied RGBA8) and
    /// present it.
    pub(crate) fn present(
        &mut self,
        pixels: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Presented, PresentError> {
        let expected = width as usize * height as usize * 4;
        if pixels.len() != expected {
            return Err(PresentError::PixelBufferSize {
                expected,
                actual: pixels.len(),
            });
        }

        let frame = match self.acquire()? {
            Acquired::Frame(frame) => frame,
            Acquired::Skip(outcome) => return Ok(outcome),
        };

        self.ensure_staging(width, height);
        let staging = self.staging.as_ref().expect("staging texture ensured");
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &staging.texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        self.blit(frame);
        Ok(Presented::Frame)
    }

    /// Upload only `rect` of the frame and present, reusing everything the
    /// staging texture already holds outside it.
    ///
    /// `sub_pixels` is the tightly packed (`rect.width * 4` stride) premultiplied
    /// RGBA8 of `rect` alone — what [`otlyra_gfx::SkiaPainter::read_rgba8_rect`]
    /// returns for the damaged region. `full_width`/`full_height` size the staging
    /// texture.
    ///
    /// This must not be the first frame after the staging texture is created or
    /// resized: a fresh texture holds no retained pixels, so anything outside the
    /// rectangle would present as undefined. When that happens this reports
    /// `Dropped`, and the caller re-presents the whole frame.
    pub(crate) fn present_rect(
        &mut self,
        sub_pixels: &[u8],
        full_width: u32,
        full_height: u32,
        rect: DamageRect,
    ) -> Result<Presented, PresentError> {
        let expected = rect.width as usize * rect.height as usize * 4;
        if sub_pixels.len() != expected {
            return Err(PresentError::PixelBufferSize {
                expected,
                actual: sub_pixels.len(),
            });
        }
        if rect.x + rect.width > full_width || rect.y + rect.height > full_height {
            return Err(PresentError::DamageOutOfBounds {
                rect,
                full_width,
                full_height,
            });
        }

        let frame = match self.acquire()? {
            Acquired::Frame(frame) => frame,
            Acquired::Skip(outcome) => return Ok(outcome),
        };

        // A fresh texture has nothing to retain outside the rectangle, so a
        // partial upload would show garbage there. Report the frame as dropped so
        // the caller re-presents it whole.
        if self.ensure_staging(full_width, full_height) {
            tracing::debug!(
                "partial present onto a fresh staging texture; asking for a full frame"
            );
            return Ok(Presented::Dropped);
        }

        let staging = self.staging.as_ref().expect("staging texture ensured");
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &staging.texture,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: rect.x,
                    y: rect.y,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            sub_pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(rect.width * 4),
                rows_per_image: Some(rect.height),
            },
            wgpu::Extent3d {
                width: rect.width,
                height: rect.height,
                depth_or_array_layers: 1,
            },
        );

        self.blit(frame);
        Ok(Presented::Frame)
    }

    /// Present the retained staging texture again with no upload at all.
    ///
    /// For a frame the compositor was asked to draw but that changed nothing on
    /// the surface: the swapchain still needs a texture, but neither raster nor
    /// upload is owed.
    pub(crate) fn reblit(&mut self) -> Result<Presented, PresentError> {
        if self.staging.is_none() {
            return Ok(Presented::Dropped);
        }
        let frame = match self.acquire()? {
            Acquired::Frame(frame) => frame,
            Acquired::Skip(outcome) => return Ok(outcome),
        };
        self.blit(frame);
        Ok(Presented::Frame)
    }
}

/// One device-pixel rectangle uploaded by [`Presenter::present_rect`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// The outcome of acquiring a swapchain texture.
enum Acquired {
    /// A texture to render this frame into.
    Frame(wgpu::SurfaceTexture),
    /// No frame this time; the reason is the caller's return value.
    Skip(Presented),
}

/// What became of a frame handed to [`Presenter::present`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Presented {
    /// It reached the screen.
    Frame,
    /// The swapchain refused it. Painting it again should work.
    Dropped,
    /// The window is not visible. Painting it again would achieve nothing.
    Occluded,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PresentError {
    #[error("failed to create a gpu surface: {0}")]
    CreateSurface(#[from] wgpu::CreateSurfaceError),
    #[error("no suitable gpu adapter: {0}")]
    RequestAdapter(#[from] wgpu::RequestAdapterError),
    #[error("failed to acquire a gpu device: {0}")]
    RequestDevice(#[from] wgpu::RequestDeviceError),
    #[error("direct GPU rasterization is unavailable on this backend")]
    DirectGpuUnsupported,
    #[error("direct GPU rasterizer setup failed: {0}")]
    DirectGpu(#[from] otlyra_gfx::SkiaError),
    #[error("the gpu surface reported a validation error")]
    SurfaceValidation,
    #[error("frame buffer is {actual} bytes, expected {expected}")]
    PixelBufferSize { expected: usize, actual: usize },
    #[error("damage rect {rect:?} runs outside the {full_width}x{full_height} frame")]
    DamageOutOfBounds {
        rect: DamageRect,
        full_width: u32,
        full_height: u32,
    },
}

/// Native startup fault injection, compiled only with `fault-injection`.
///
/// `OTLYRA_FAULT_OCCLUDED_ACQUIRES=N` makes the first `N` swapchain acquisitions
/// report occlusion without asking the surface, which is what wgpu's Metal
/// backend reports when a newly shown window's occlusion state has not yet
/// reached `occlusionState` on the render thread.
#[cfg(feature = "fault-injection")]
mod fault {
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn remaining() -> &'static AtomicU32 {
        static REMAINING: OnceLock<AtomicU32> = OnceLock::new();
        REMAINING.get_or_init(|| {
            AtomicU32::new(
                std::env::var("OTLYRA_FAULT_OCCLUDED_ACQUIRES")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0),
            )
        })
    }

    pub(super) fn take_occluded_acquire() -> bool {
        let taken = remaining()
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if taken {
            tracing::warn!("fault injection: swapchain acquisition reported occluded");
        }
        taken
    }
}
