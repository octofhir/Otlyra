# Skia ↔ wgpu texture interop on macOS/Metal

Investigated 2026-07-20 on Apple M1, macOS 25.5, `skia-safe 0.99`, `wgpu 30`.

**Question.** Can Skia render directly into a wgpu-owned texture, with no per-frame
copy, or must Skia rasterize separately and wgpu only present the result?

**Decision: Skia rasterizes into its own raster surface; wgpu uploads it once per
frame and blits it to the swapchain.** Zero-copy is deferred, not ruled out.

## Zero-copy is reachable

Every piece exists:

| Step | API |
|---|---|
| Get wgpu's `MTLDevice` / `MTLCommandQueue` | `wgpu::Device::as_hal::<hal::api::Metal>()` |
| Get a wgpu texture's `MTLTexture` | `wgpu::Texture::as_hal::<hal::api::Metal>()` |
| Wrap them for Ganesh | `skia_safe::gpu::mtl::BackendContext::new(device, queue)` (unsafe) |
| Build the context | `gpu::direct_contexts::make_metal` |
| Wrap the texture | `gpu::backend_textures::make_mtl` → `gpu::surfaces::wrap_backend_texture` |

## Why it is not what we ship

1. **Version coupling on the fastest-moving dependency we have.** The handles are
   raw Objective-C pointers whose types come from `wgpu-hal`'s `metal` crate. Using
   them means pinning `wgpu-hal` and `metal` to exactly what `wgpu` resolved,
   re-pinned every twelve weeks against a dependency with no LTS.
2. **Unsafe FFI across two runtimes on the critical path.** Reference-counting and
   lifetime rules on both the Ganesh and the wgpu side, in week one, in the code
   every frame goes through.
3. **Cross-queue synchronization.** Ganesh submits on its own `MTLCommandQueue`;
   wgpu's submission must be ordered after it. That is a correctness problem, not a
   plumbing problem.

## What the copy actually costs

Release build, tight loop, 60 iterations, median of the run:

| Surface | Rasterize | Read back | Bytes |
|---|---|---|---|
| 1600×1200 | 0.26 ms | 0.32 ms | 7.7 MB |
| 2560×1600 | 0.67 ms | 1.01 ms | 16.4 MB |
| 3456×2234 | 2.37 ms | 1.72 ms | 30.9 MB |

Read back plus upload is roughly 1–2 ms at a typical Retina viewport, against an
8 ms present budget. Real, but a fraction of the budget, and it shrinks to near
nothing once damage tracking means only dirty tiles are copied.

## Revisit when

The present budget is actually threatened by the copy — not before. `PaintTarget`
means the swap is confined to one backend, so this stays a scheduling decision.
