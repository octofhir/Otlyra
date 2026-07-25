# Otlyra Browser UI — Remaining Work

This plan covers browser-owned UI only: tabs, toolbar, omnibox, menus, popups,
settings, history, downloads, inspector, permission prompts, and other chrome.
Web-page UI remains HTML/CSS/DOM/JavaScript running in Otter.

Flutter is an architectural reference, not a dependency. Otlyra keeps one Rust
event model, text stack, display-list format, rasterizer, and compositor.

## Current baseline

Do not rebuild the completed scheduling and persistent-tree foundations.
`FrameRequest`, coalesced redraws, deadlines, idle/no-op behavior, work counters,
stable keyed render nodes, retained tab/toolbar/inspector boundaries, and
incremental semantics already exist in the current working tree.

The current frame path is still expensive:

```text
widget/page display lists
  -> CPU Skia raster of the damage rectangle
  -> RGBA readback of that rectangle
  -> wgpu sub-rectangle texture upload
  -> full-surface swapchain blit
```

### Renderer research decision

Zed/GPUI is a design reference, not a dependency and not a renderer we can
drop in. GPUI rebuilds a short-lived element description each frame but keeps
state behind stable element/entity identities; its scene is a typed, sorted
batch of quads, paths, shadows, glyph sprites, images, and surfaces. On macOS
Zed's backend renders that scene directly with Metal rather than through wgpu.
The useful contract is stable identity plus a GPU scene, not the GPUI crate.

Chromium reaches the same boundary from the browser side: paint produces
display items/layers, invalidation names what changed, and the compositor can
remain responsive without asking the document/main thread to repaint.

Otlyra therefore keeps its own widget, display-list, text, and event contracts:

- retain stable UI nodes and scene-layer identities;
- keep scale, translation, clip, opacity, and animation in compositor state;
- make damage precise enough that caret and text input never repaint a page;
- add a direct GPU implementation behind the existing seven-operation
  `PaintTarget` seam, instead of replacing the UI framework;
- retain the CPU backend for screenshots, deterministic tests, unsupported
  GPUs, and backend comparison.

Vello is the closest existing Rust renderer to Otlyra's `kurbo`/`peniko` scene
vocabulary and renders directly to a wgpu texture, but it is not a blind
dependency choice. As of 2026-07-25 Vello 0.9 uses wgpu 29 while Otlyra uses
wgpu 30, and Vello still documents blur/filter and glyph-cache work as
incomplete. A backend spike must first prove version integration, all seven
paint operations, pixel parity, startup cost, and p95 frame improvement.

Primary references:

- [GPUI architecture](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md)
  and [scene primitives](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/scene.rs);
- [Chromium GPU compositing](https://www.chromium.org/developers/design-documents/gpu-accelerated-compositing-in-chrome/)
  and [compositor-thread architecture](https://www.chromium.org/developers/design-documents/compositor-thread-architecture/);
- [Vello renderer and current limitations](https://github.com/linebender/vello).

Current release startup distribution on Apple M1, macOS 26.5, 1024x768 logical
at 2x, 20 samples after three warmups:

```text
process -> visible window:         p50 321.53 ms, p95 368.95 ms
process -> first presented frame:  p50 494.62 ms, p95 564.74 ms
visible -> first presented frame:  p50 171.59 ms, p95 193.24 ms
```

The probe now emits an ordered per-stage milestone table and the benchmark
aggregates each stage's p50/p95 duration. Deferring the inspector, history, and
settings surfaces' text engines to first interaction removed three redundant
system-font enumerations from browser construction — the largest measured stage —
cutting `preferences_ready -> browser_ready` from ~162 ms to ~82 ms p50. The
largest remaining stage is now first paint (`visibility_requested ->
chrome_raster_complete`, ~114 ms p50: chrome build, text shaping, and CPU
raster), which is the next single optimization to take.

Interactive input now has its own release gate:

```text
typing-cost, 2048x1536 device pixels at 2x, 200-paragraph page, 43 keys

                                 before       after
page-field damage p50/p95         89.8%        1.4%
page-field frame p95               4.46 ms      0.11 ms
omnibox damage p50/p95            10.2%        4.3%
omnibox frame p95                  1.48 ms      0.30 ms
full-page scroll damage p50/p95   89.8%       89.8%
full-page scroll frame p95         4.75 ms      1.97 ms
```

The page-field regression was not raw raster speed: Space performed the page
scroll default action even while an input owned the keyboard. The field then
sat offscreen, and its clipped empty damage was mistaken for unknown/full
damage. Both contracts now have HiDPI compositor regressions. Omnibox edits
likewise report the field rectangle rather than the entire chrome band.

The reference CI runner has the labels `self-hosted`, `macOS`, `ARM64`, and
`otlyra-performance`. `tools/startup-benchmark.py` and
`.github/workflows/startup-performance.yml` record raw samples and enforce:

- process to visible: p50 <= 50 ms, p95 <= 100 ms;
- process to first complete chrome frame: p50 <= 100 ms, p95 <= 150 ms.

Do not weaken these budgets to accept the current implementation.

## Priority 1 — Finish startup isolation

### 1. Attribute the remaining startup time

- [x] Add machine-readable milestones for:
  - entry into `main`;
  - CLI and preferences ready;
  - minimal browser model ready;
  - event loop resumed;
  - Dock icon ready;
  - native menu ready;
  - window created;
  - AccessKit attached;
  - window visibility requested;
  - wgpu instance ready;
  - surface attached;
  - adapter/device/pipeline ready;
  - chrome display list ready;
  - CPU raster complete;
  - readback complete;
  - upload complete;
  - first presentation complete.
- [x] Aggregate stage p50/p95 in `tools/startup-benchmark.py`.
- [x] Keep the report stable and preserve every raw sample.
- [ ] Re-run the 20-sample local distribution after every startup change.

### 2. Reduce process-to-visible

- [ ] Split browser construction into a minimal chrome bootstrap and deferred
  services.
- [ ] Defer inspector, history, downloads, update checks, navigation services,
  page accessibility, and Otter creation until first use.
- [ ] Audit preference loading, native menu installation, Dock icon decoding,
  font discovery, dynamic initialization, and synchronous filesystem work.
- [ ] Cache or bundle decoded startup assets where measurement justifies it.
- [ ] Keep AccessKit attached before first visibility where the platform
  requires it; do not fake the visibility milestone by blocking the event-loop
  callback after `set_visible(true)`.

### 3. Reduce visible-to-first-frame

- [x] Separate chrome build, text shaping, Skia raster, readback, upload, and
  present timings.
- [ ] Reuse raster readback and upload buffers.
- [ ] Avoid rasterizing before the presenter is ready.
- [ ] Decide from profiles whether the next step is direct Skia GPU rendering,
  Skia/wgpu interop, or retained CPU tiles.
- [ ] Ensure the first chrome frame does not initialize Otter or wait for a
  page/network service.

**Exit:** the strict startup workflow passes on the reference runner.

## Priority 2 — Retained scene layers and damage

- [x] Change browser painting to publish persistent scene layers instead of
  replaying one flattened display list. (`Painter::compose` seam; browser emits
  page/highlight/inspector/chrome layers; whole-surface `paint` stays the
  fallback for screenshots and `--no-interface`.)
- [x] Store unchanged display lists behind `Arc<DisplayList>`. (Page, chrome and
  inspector all hand back an `Arc` their own cache keeps, and the browser holds
  one scaled device form per layer keyed by pointer identity and scale. A frame
  that changes nothing clones no list and transforms none of them; a browser test
  asserts exactly that with `Arc::ptr_eq`, and a scale change still re-scales.)
- [~] Add stable layer identity and epochs for:
  - tab strip; — done (folded into the chrome layer)
  - toolbar/omnibox; — done (chrome layer)
  - page viewport; — done (page layer)
  - inspector or side panel; — done (inspector layer)
  - popup surfaces; — Priority 4
  - transient overlays, drag images, and toasts. — highlight layer done; the
    rest are Priority 4.
- [ ] Keep device scale, translation, clip, opacity, and simple animation in
  layer properties rather than cloning transformed display items. (Blocked on a
  gfx change: `SkiaPainter::push_layer` resets the canvas matrix to identity, so
  a canvas base-transform is wiped inside layers; scale is still baked per item.)
- [x] Rasterize and upload only damaged regions or tiles. (Compositor keeps a
  persistent surface and re-rasterizes/uploads only the union rect of moved
  layers via `read_rgba8_rect` + `present_rect`; unchanged layers keep their
  retained pixels and staging texture.)
- [x] Bound text-entry damage. Page fields redraw only their control rectangle;
  Space no longer scrolls a page while a field owns the keyboard; an offscreen
  known change advances its layer epoch without redrawing visible pixels; and
  omnibox edits redraw only the field rather than the whole chrome band.
  `typing-cost` reports damage p50/p95 as well as time, and
  `window_interaction.rs` covers the real HiDPI compositor path.
- [x] Add cache/build/upload counters and tests proving unrelated input leaves
  unchanged layers untouched. (`plan_damage` unit tests; browser epoch tests: a
  no-op frame damages nothing, a page scroll moves only the page epoch; and
  `window_interaction.rs` drives a real press on the toolbar through the
  compositor and asserts the reported damage stays inside the chrome band while
  the page's pixels do not move.)
- [ ] Tighten the highlight layer's rectangle from the whole content area to the
  chosen box. Blocked on a prerequisite rather than on effort: the overlay draws
  labels and, for a grid, track lines and line numbers that reach outside the
  box, and there is no bounds computation for a display list — glyph runs carry
  no extent — so any rectangle short of the content area is a guess that clips
  exactly what a person is looking at. Add `DisplayList::bounds` first, then this
  is the union of what the overlay actually drew.
- [ ] Add backend-object caches only when profiles show conversion cost.

Performance targets:

- no-op input: no frame and no heap allocation;
- hover input-to-present p95 <= 8.33 ms;
- cached chrome reconcile/layout/display-list update p95 <= 1 ms;
- unchanged page: no raster, readback, upload, or accessibility rebuild;
- caret: at most two small paint invalidations per second while idle.

One frame path, checked as one: `window_interaction.rs` composites a frame with
the inspector open and an element highlighted and requires it to be
pixel-for-pixel what a whole-surface `paint` of the same state draws. Without
that, every golden in the crate could be testing a picture nobody is looking at.

**Exit:** toolbar hover or caret movement does not rasterize or upload the page.
The damage mechanism meets this, and it is now confirmed against the window's own
frame path rather than against `paint`/`compose` alone: a press on the toolbar,
driven through the protocol, damages only the chrome band and leaves the page's
pixels identical.

## Verification gap — a driveable protocol for the real window — closed

The interface goldens and unit tests exercised `paint`/`compose` off the event
loop, and nothing drove the **windowed compositor** or read back what it put on
screen. That is why a focus/blur regression could pass every test and still be
visible in the running app. It no longer can.

- [x] The automation path drives the whole window. `Session::windowed`
  (`--window` with `--bidi` or `--mcp`) keeps the interface, delivers
  `input.performActions` through the window's own event path, and answers
  `otlyra:captureWindow` — `browser_window` to an agent — with the composited
  window, chrome included, plus the damage rectangle the frame redrew.
- [x] A deterministic frame pump renders composited frames to a buffer.
  `otlyra_platform::FramePump` runs the window's `compose` → damage → retained
  surface path with no winit, no wgpu and no clock: an event is delivered through
  `handle_event`, a frame is drawn only if the painter asked for one, and the
  pixels come off the same retained surface the live window blits from. The
  compositor itself moved into `otlyra-platform::compositor` so the event loop
  and the pump cannot drift apart — there is one `plan_damage`, one retained
  surface, one rasterize-the-damage path.
- [x] Click-to-blur is verified end to end in
  `crates/otlyra-app/tests/window_interaction.rs`, which asserts on the window's
  pixels rather than on a model flag.

**Resolved:** clicking outside an input did not clear the caret or the focus
ring, and the omnibox fix was not the whole story. A press on the page that did
not land on a control never reached `PageScene::pointer_pressed_times`, so the
page's `interaction.focus` — and with it the caret and the ring — survived a
press anywhere else on the document. `PageScene::blur` now answers that press,
called from the browser's press handler beside the toolbar's own blur. The
pixel test reproduced it first and passes now.

Still worth having, and not blocking:

- [ ] A window session at 2x, so a device-scale bug is visible to the same tests.
- [ ] A live BiDi transport onto the window a person is actually looking at.

## Priority 3 — Finish persistent UI migration

- [ ] Migrate settings, history, about pages, menus, and remaining system
  surfaces from full-tree cache misses to persistent boundaries.
- [ ] Replace coarse inspector body invalidation with keyed/virtualized
  tree/table/list children where profiles justify it.
- [ ] Remove the short-lived widget adapter after the final surface migrates.
- [x] Update `docs/interface.md`: model state remains external, but persistent
  render nodes may retain identity, geometry, focus/capture membership,
  semantics, animation progress, and render caches.

**Exit:** no browser-owned surface depends on rebuilding its complete widget
tree for an unrelated visual change.

## Priority 4 — Surfaces, focus, and popups

- [x] Introduce `UiSurfaceId` and multiple UI roots. Chrome, page, system page,
  and inspector have stable identities; a pointer press or explicit surface
  action selects exactly one active root.
- [ ] Add focus scopes and deterministic traversal across root and popup
  surfaces.
- [ ] Add pointer capture and a complete drag lifecycle.
- [x] Route IME/text input, clipboard editing keys, accessibility focus, and
  keyboard input through the active surface. Delivery is exclusive rather than
  an ordered "first root that consumes it" chain, so a stale inspector search
  caret cannot steal typing after the page is pressed. A window-compositor
  regression covers the handoff.
- [ ] Implement platform popup windows plus an in-window backend for tests and
  screenshots.
- [ ] Build shared dismissal rules: outside click, Escape, focus loss, and
  parent destruction.
- [ ] Add tooltip scheduling and dismissal.
- [ ] Move menus, context menus, dropdowns, omnibox suggestions, and permission
  prompts onto this contract.

**Exit:** every popup uses one event/focus/semantics contract and can leave the
browser window when required.

## Priority 5 — Design system and fast UI authoring

- [ ] Centralize semantic colors, typography, spacing, size, radius, border,
  elevation, icon, motion, density, and hit-target tokens.
- [ ] Remove raw styling values from browser components.
- [ ] Extend the `interface` example into a standalone state-matrix workbench
  that requires no DOM, network, page engine, or Otter runtime.
- [ ] Cover light, dark, high-contrast, inactive-window, 1x/2x, narrow/wide,
  keyboard, accessibility, RTL, and long-label states.
- [ ] Add dirty-root, layer, damage, hit-target, and focus overlays.
- [ ] Standardize button, text field, search field, list row, tree row, table
  cell, menu, popup, split view, scrollbar, tooltip, and toast behavior.
- [ ] Add virtual list/tree/table primitives for:
  - 10,000 history or download rows;
  - a 100,000-node inspector tree;
  - 1, 20, 100, and 500 tabs.
- [ ] Add deterministic interaction replays, goldens, and generated contact
  sheets.

**Exit:** a new browser panel is mostly composition and typed model actions,
not new input, layout, focus, paint, or accessibility infrastructure.

## Required invariants

- Browser/page models own navigation, tabs, settings, documents, and other
  application state.
- UI runtime state must not become a second copy of browser model state.
- Core widgets remain generic over typed actions and know nothing about the
  browser.
- Paint and hit testing use the same stored geometry.
- Page layout never depends on chrome layout.
- The UI thread never blocks on page JavaScript, network, or filesystem work.
- No frame is scheduled without visible dirty output or an explicit animation
  deadline.
- No unchanged display list is cloned merely to assemble a frame.
- Every interactive component has keyboard, focus, disabled, and accessible
  naming behavior.
- Add a general abstraction only when at least two real browser consumers need
  it.
- Do not add Flutter, Dart, HTML chrome, a second renderer, a second text stack,
  or a second event loop.

## Verification

Every change must include the narrow tests and measurement that prove its
claim. Before handoff:

```text
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Performance changes include before/after distributions. Visual changes include
reviewed 1x/2x artifacts. New caches expose counters and have a regression test
showing unrelated state does not invalidate them.

## Immediate next task

Build a bounded direct-GPU renderer spike outside the live default path. The
acceptance gate now exists before the candidate backend:

- [x] Add `backend-corpus`, one retained `DisplayList` covering solid and
  gradient fill, arbitrary stroke, transformed clip/layer, blend and opacity,
  sharp and blurred glyphs, direct and brush images, blur, and a non-painting
  hit-test item.
- [x] Prove in a unit test that every paintable display item reaches the
  seven-operation `PaintTarget` seam and layers stay balanced.
- [x] Generate and visually inspect deterministic Skia reference images at 1x
  and 2x. A warm release run on Apple M1 records raster-only p50/p95 of
  1.54/3.53 ms at 1x and 5.95/11.61 ms at 2x; these are a comparison baseline,
  not a stable CI budget.
- [x] Replay that exact corpus into a wgpu-owned Metal texture through the same
  `SkiaPainter`/`PaintTarget`, using wgpu's `MTLDevice` and its own
  `MTLCommandQueue`. The timed path has no CPU raster, readback, or upload.
- [x] Connect the candidate to the real retained compositor behind
  `--direct-gpu`. Ganesh remains thread-bound on the event-loop thread, shares
  the existing layer history/damage plan, redraws damaged regions in place, and
  presentation samples its wgpu-owned sRGB texture with zero upload. CPU remains
  the default, screenshot backend, deterministic test backend, and fallback.
- [~] Compare candidate pixels against the references with the existing
  `compare` example. 2x passes (0.011% differing pixels); 1x currently misses
  the 0.2% allowance with 0.355%, concentrated on antialiased path/image edges.
  Do not weaken the gate or call the candidate ready without an explicit parity
  policy.
- [ ] Measure cold initialization, first chrome frame, cached omnibox input,
  caret-only input, scroll, and a path/image-heavy page. Corpus-only steady
  p50 improves from CPU 1.47 ms to Metal 0.76 ms at 1x, and from 6.01 ms to
  0.83 ms at 2x; p95 remains noisy. A 20-sample live distribution makes
  readback/upload effectively zero and reduces raster p50 from 144.67 ms to
  110.14 ms, but loses the CPU path's overlap with GPU initialization:
  visible-to-frame p50/p95 regresses from 227.99/260.88 ms to
  250.79/298.02 ms, and process-to-frame from 616.90/661.81 ms to
  632.07/779.97 ms. Cold startup therefore remains an adoption blocker; test a
  CPU-first-frame then GPU warm/switch design before considering default-on.
- [ ] Adopt it only if it removes CPU readback/upload and improves measured p95
  without regressing parity or startup. Keep the retained-damage CPU path as
  fallback.

Priority 4's active-surface prerequisite is complete: keyboard, IME, clipboard,
and accessibility now route exclusively through the selected `UiSurfaceId`.
The next correctness work there is focus scopes/traversal and popup lifecycle.
None of this requires Otter/JavaScript integration.
