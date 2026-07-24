//! GPU presentation: upload the rasterized frame and blit it to the swapchain.

use std::sync::Arc;

use crate::Viewport;

/// Format of the staging texture. `*Srgb` so the sampler decodes and the swapchain
/// re-encodes; the rasterizer hands us sRGB-encoded premultiplied RGBA8.
const STAGING_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

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
/// available on the event-loop thread and then moved to the GPU startup worker.
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

impl Presenter {
    pub(crate) fn instance<W>(window: Arc<W>) -> PresenterInstance
    where
        W: raw_window_handle::HasDisplayHandle + std::fmt::Debug + Send + Sync + 'static,
    {
        PresenterInstance(wgpu::Instance::new(
            wgpu::InstanceDescriptor::new_with_display_handle(Box::new(window)),
        ))
    }

    pub(crate) fn prepare<W>(
        instance: PresenterInstance,
        window: Arc<W>,
    ) -> Result<PresenterSeed, PresentError>
    where
        W: wgpu::WindowHandle + raw_window_handle::HasDisplayHandle + std::fmt::Debug + 'static,
    {
        let PresenterInstance(instance) = instance;
        let surface = instance.create_surface(wgpu::SurfaceTarget::Window(Box::new(window)))?;
        Ok(PresenterSeed { instance, surface })
    }

    pub(crate) fn new(seed: PresenterSeed, viewport: Viewport) -> Result<Self, PresentError> {
        let PresenterSeed { instance, surface } = seed;
        // Adapter and device creation happen once on the startup worker. The
        // window event loop and the frame path never await them.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))?;

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

    fn ensure_staging(&mut self, width: u32, height: u32) {
        let current = self
            .staging
            .as_ref()
            .is_some_and(|staging| staging.width == width && staging.height == height);
        if current {
            return;
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
    }

    /// Upload `pixels` (tightly packed premultiplied RGBA8) and present it.
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

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            // A lost or outdated swapchain is normal while a window is being
            // created, resized or moved between displays. Reconfigure and tell the
            // caller the frame never happened, so it can ask for another one — the
            // loop blocks in `Wait`, so a dropped frame nobody re-requests is a
            // window that stays black until the user happens to poke it.
            wgpu::CurrentSurfaceTexture::Lost | wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                tracing::debug!("swapchain lost or outdated; frame dropped");
                return Ok(Presented::Dropped);
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                tracing::debug!("swapchain timed out; frame dropped");
                return Ok(Presented::Dropped);
            }
            // The window is hidden. Asking again would spin against a window nobody
            // can see; winit wakes us when it is visible.
            wgpu::CurrentSurfaceTexture::Occluded => return Ok(Presented::Occluded),
            wgpu::CurrentSurfaceTexture::Validation => return Err(PresentError::SurfaceValidation),
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
            pass.set_bind_group(0, &staging.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }

        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        Ok(Presented::Frame)
    }
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
    #[error("the gpu surface reported a validation error")]
    SurfaceValidation,
    #[error("frame buffer is {actual} bytes, expected {expected}")]
    PixelBufferSize { expected: usize, actual: usize },
}
