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
  -> CPU Skia raster
  -> full RGBA readback
  -> full wgpu texture upload
  -> swapchain blit
```

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

The reference CI runner has the labels `self-hosted`, `macOS`, `ARM64`, and
`otlyra-performance`. `tools/startup-benchmark.py` and
`.github/workflows/startup-performance.yml` record raw samples and enforce:

- process to visible: p50 <= 50 ms, p95 <= 100 ms;
- process to first complete chrome frame: p50 <= 100 ms, p95 <= 150 ms.

Do not weaken these budgets to accept the current implementation.

## Priority 1 — Finish startup isolation

### 1. Attribute the remaining startup time

- [ ] Add machine-readable milestones for:
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
- [ ] Aggregate stage p50/p95 in `tools/startup-benchmark.py`.
- [ ] Keep the report stable and preserve every raw sample.
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

- [ ] Separate chrome build, text shaping, Skia raster, readback, upload, and
  present timings.
- [ ] Reuse raster readback and upload buffers.
- [ ] Avoid rasterizing before the presenter is ready.
- [ ] Decide from profiles whether the next step is direct Skia GPU rendering,
  Skia/wgpu interop, or retained CPU tiles.
- [ ] Ensure the first chrome frame does not initialize Otter or wait for a
  page/network service.

**Exit:** the strict startup workflow passes on the reference runner.

## Priority 2 — Retained scene layers and damage

- [ ] Change browser painting to publish persistent scene layers instead of
  replaying one flattened display list.
- [ ] Store unchanged display lists behind `Arc<DisplayList>`.
- [ ] Add stable layer identity and epochs for:
  - tab strip;
  - toolbar/omnibox;
  - page viewport;
  - inspector or side panel;
  - popup surfaces;
  - transient overlays, drag images, and toasts.
- [ ] Keep device scale, translation, clip, opacity, and simple animation in
  layer properties rather than cloning transformed display items.
- [ ] Rasterize and upload only damaged regions or tiles.
- [ ] Add cache/build/upload counters and tests proving unrelated input leaves
  unchanged layers untouched.
- [ ] Add backend-object caches only when profiles show conversion cost.

Performance targets:

- no-op input: no frame and no heap allocation;
- hover input-to-present p95 <= 8.33 ms;
- cached chrome reconcile/layout/display-list update p95 <= 1 ms;
- unchanged page: no raster, readback, upload, or accessibility rebuild;
- caret: at most two small paint invalidations per second while idle.

**Exit:** toolbar hover or caret movement does not rasterize or upload the page.

## Priority 3 — Finish persistent UI migration

- [ ] Migrate settings, history, about pages, menus, and remaining system
  surfaces from full-tree cache misses to persistent boundaries.
- [ ] Replace coarse inspector body invalidation with keyed/virtualized
  tree/table/list children where profiles justify it.
- [ ] Remove the short-lived widget adapter after the final surface migrates.
- [ ] Update `docs/interface.md`: model state remains external, but persistent
  render nodes may retain identity, geometry, focus/capture membership,
  semantics, animation progress, and render caches.

**Exit:** no browser-owned surface depends on rebuilding its complete widget
tree for an unrelated visual change.

## Priority 4 — Surfaces, focus, and popups

- [ ] Introduce `UiSurfaceId` and multiple UI roots.
- [ ] Add focus scopes and deterministic traversal across root and popup
  surfaces.
- [ ] Add pointer capture and a complete drag lifecycle.
- [ ] Route IME, clipboard, accessibility, and keyboard input through the
  focused surface.
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

Add per-stage startup milestones to the JSON probe and benchmark summary. Use
the resulting p50/p95 attribution to select exactly one next optimization.
Do not begin retained tiles, direct GPU rendering, or service deferral based on
intuition alone.
