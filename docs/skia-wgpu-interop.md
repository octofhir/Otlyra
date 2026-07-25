# Skia ↔ wgpu texture interop on macOS/Metal

Investigated 2026-07-20 on Apple M1, macOS 25.5, `skia-safe 0.99`, `wgpu 30`.

**Question.** Can Skia render directly into a wgpu-owned texture, with no per-frame
copy, or must Skia rasterize separately and wgpu only present the result?

**Live-path decision: keep the retained CPU surface until the bounded Metal
candidate clears parity and startup gates.** Zero-copy now has an executable
acceptance spike; it is no longer only a paper design.

## Zero-copy is reachable

Every piece exists:

| Step | API |
|---|---|
| Get wgpu's `MTLDevice` / `MTLCommandQueue` | `wgpu::Device::as_hal` / `wgpu::Queue::as_hal` |
| Get a wgpu texture's `MTLTexture` | `wgpu::Texture::as_hal::<hal::api::Metal>()` |
| Wrap them for Ganesh | `skia_safe::gpu::mtl::BackendContext::new(device, queue)` (unsafe) |
| Build the context | `gpu::direct_contexts::make_metal` |
| Wrap the texture | `gpu::backend_textures::make_mtl` → `gpu::surfaces::wrap_backend_texture` |

## What the acceptance spike proved

`backend-corpus` can wrap a wgpu-owned `Rgba8UnormSrgb` texture in Ganesh and replay
the exact same seven-operation `PaintTarget` corpus used by the CPU backend.
Crucially, Skia receives wgpu's own `MTLCommandQueue`, not a second queue. A
Skia flush committed before later wgpu work is therefore ordered by Metal's
single queue; no cross-queue semaphore is needed for this path.

The timed GPU replay includes `flush_submit_and_sync_cpu`, but excludes the PNG
readback used only for comparison. It performs no CPU raster, CPU readback, or
texture upload:

| 640×400 logical sRGB corpus | CPU Skia p50/p95 | Metal p50/p95 |
|---|---:|---:|
| 1x | 1.47 / 1.78 ms | 0.76 / 3.84 ms |
| 2x | 6.01 / 8.92 ms | 0.83 / 3.42 ms |

These are one warm 100-frame run, not a stable CI distribution. GPU p50 is
consistently lower in repeated runs; its small-corpus p95 still has outliers and
must be measured in the real browser scenarios.

The current pixel gate passes at 2x: 0.011% of pixels differ, with a worst
channel difference of 35. At 1x it does not yet pass: 0.355% differ against a
0.2% allowance. The marked differences follow antialiased path and image edges,
not missing primitives, but the threshold is not weakened without a deliberate
GPU-vs-raster policy.

Cold cost is not settled. With a warm driver cache, headless wgpu adapter/device
creation took 15–17 ms, wrapping took at most 0.45 ms, and the first corpus frame
took 86–92 ms. The first uncached live `--direct-gpu` frame spent about 635 ms
between `gpu_ready` and `chrome_raster_complete`.

## Live opt-in

`otlyra --direct-gpu` now keeps a thread-bound Ganesh surface beside the
presenter. It is created only on the event-loop thread after the wgpu device has
arrived from its startup worker; no `SkSurface` or `GrDirectContext` is marked
`Send` or crosses a thread. The existing `SceneCompositor` remains the sole
owner of layer history and damage planning. CPU offscreen rendering, frame-pump
tests, screenshots, and the default window path are unchanged.

Three warmups followed by 20 real process/window samples at 2048×1536
produced:

| Live `about:about`, p50/p95 | CPU default | `--direct-gpu` |
|---|---:|---:|
| visible → first presented frame | 227.99 / 260.88 ms | 250.79 / 298.02 ms |
| process → first presented frame | 616.90 / 661.81 ms | 632.07 / 779.97 ms |
| raster stage p50 | 144.67 ms | 110.14 ms |
| readback + upload/present p50 | 8.51 ms | 2.31 ms |

The direct frame reports zero uploaded bytes. This is evidence that the live
texture ownership and queue ordering work, not a default-on performance win.
GPU raster itself is faster and removes the copy, but unlike CPU raster it
cannot start before `gpu_ready`; the CPU path overlaps its first paint with GPU
startup. The direct distribution is therefore slower overall and had one
1.68-second process-to-frame outlier. Both paths miss the startup budget.

## Why it is not the live default yet

1. **Version coupling on the fastest-moving dependency we have.** The handles are
   raw Objective-C pointers whose types come from `wgpu-hal`'s `metal` crate. Using
   them means pinning `wgpu-hal` and `metal` to exactly what `wgpu` resolved,
   re-pinned every twelve weeks against a dependency with no LTS.
2. **Unsafe FFI across two runtimes on the critical path.** Reference-counting and
   lifetime rules on both the Ganesh and the wgpu side, in week one, in the code
   every frame goes through.
3. **The 1x parity gate is still red.** It is a small antialiasing boundary, but
   a renderer is not adopted by describing its difference as probably harmless.
4. **The opt-in path still needs real interaction distributions and device-loss
   fallback.** Resize and damage clipping are integrated; screenshots and tests
   deliberately remain CPU. A lost direct device must select CPU automatically
   before this can be a default.
5. **Uncached startup is too expensive.** First-frame shader work must happen
   after visibility or be demonstrably hidden by work already required. The
   likely shape is CPU first frame followed by GPU warm-and-switch, unless device
   readiness moves early enough to beat the overlapped CPU path.

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

## Adoption gate

Keep the spike outside the live default until:

- 1x and 2x have an explicit, automated parity policy;
- the opt-in retained wgpu surface passes cached omnibox, caret, scroll, and
  path/image-heavy page distributions;
- first-visible startup stays within the existing budget on a cold driver
  cache;
- screenshots and deterministic tests continue through CPU Skia;
- device loss or unsupported platforms select the CPU backend without changing
  browser/UI code.
