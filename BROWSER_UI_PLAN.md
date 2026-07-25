# Otlyra Browser UI — Remaining Work

This plan covers browser-owned UI only: tabs, toolbar, omnibox, menus, popups,
settings, history, downloads, inspector, permission prompts, and other chrome.
Web-page UI remains HTML/CSS/DOM/JavaScript running in Otter.

It is a list of work, not a record of it. What has landed is in the code, in
`docs/interface.md`, and in the history; nothing is kept here to be admired.
An item stays only while something is left to do about it, and carries the
constraint a person picking it up would otherwise have to rediscover.

Flutter is an architectural reference, not a dependency. Otlyra keeps one Rust
event model, text stack, display-list format, rasterizer, and compositor.

## Standing constraints

- Do not rebuild the scheduling, retained-tree, damage, or surface foundations:
  `FrameRequest`, coalesced redraws, deadlines, keyed render nodes, retained
  tab/toolbar/inspector/popup boundaries, per-layer damage, `UiSurfaceId`
  routing, and focus scopes all exist and are covered by tests.
- Zed/GPUI and the Chromium compositor are design references, not dependencies:
  keep stable identity plus a GPU scene, not the crate.
- The CPU backend stays the screenshot, deterministic-test and fallback path
  whatever the GPU work decides.
- Startup budgets, enforced by `.github/workflows/startup-performance.yml` on a
  runner labelled `self-hosted`, `macOS`, `ARM64`, `otlyra-performance`:
  process to visible p50 ≤ 50 ms / p95 ≤ 100 ms; process to first complete
  chrome frame p50 ≤ 100 ms / p95 ≤ 150 ms. Do not weaken them to accept an
  implementation.
- Interactive budgets: no-op input allocates nothing and draws no frame; hover
  input-to-present p95 ≤ 8.33 ms; cached chrome reconcile/layout/display-list
  update p95 ≤ 1 ms; an unchanged page rasterizes, reads back, uploads and
  rebuilds accessibility never; a caret costs at most two small invalidations a
  second while idle.

## Priority 1 — Finish startup isolation

The measured distribution, 20 samples on Apple M1, macOS 26.5, 1024x768 at 2x:
process → visible p50 321.53 ms / p95 368.95 ms; process → first presented
frame p50 494.62 ms / p95 564.74 ms. The largest remaining stage is first paint
(`visibility_requested -> chrome_raster_complete`, ~114 ms p50: chrome build,
text shaping, CPU raster).

- [ ] Re-run the 20-sample local distribution after every startup change.
- [ ] Split browser construction into a minimal chrome bootstrap and deferred
  services.
- [ ] Defer inspector, history, downloads, update checks, navigation services,
  page accessibility, and Otter creation until first use. (Text engines are
  already deferred; each `TextEngine::new` re-enumerates system fonts, which was
  the largest measured stage.)
- [ ] Audit preference loading, native menu installation, Dock icon decoding,
  font discovery, dynamic initialization, and synchronous filesystem work.
- [ ] Cache or bundle decoded startup assets where measurement justifies it.
- [ ] Keep AccessKit attached before first visibility where the platform
  requires it; do not fake the visibility milestone by blocking the event-loop
  callback after `set_visible(true)`.
- [ ] Reuse raster readback and upload buffers.
- [ ] Avoid rasterizing before the presenter is ready.
- [ ] Decide from profiles whether the next step is direct Skia GPU rendering,
  Skia/wgpu interop, or retained CPU tiles.
- [ ] Ensure the first chrome frame does not initialize Otter or wait for a
  page/network service.

**Exit:** the strict startup workflow passes on the reference runner.

## Priority 2 — Retained scene layers and damage

- [ ] Keep device scale, translation, clip, opacity, and simple animation in
  layer properties rather than cloning transformed display items. Blocked on a
  gfx change: `SkiaPainter::push_layer` resets the canvas matrix to identity, so
  a canvas base-transform is wiped inside layers and scale is still baked per
  item.
- [ ] Tighten the highlight layer's rectangle from the whole content area to the
  chosen box. Blocked on a prerequisite: the overlay draws labels and, for a
  grid, track lines and numbers that reach outside the box, and there is no
  bounds computation for a display list — glyph runs carry no extent — so any
  rectangle short of the content area clips exactly what a person is looking at.
  Add `DisplayList::bounds` first; this is then the union of what was drawn.
- [ ] Add stable layer identity and epochs for transient overlays, drag images,
  and toasts. (Tab strip, toolbar, page, inspector, popups and the highlight
  already have them.)
- [ ] Add backend-object caches only when profiles show conversion cost.

**Still worth having, and not blocking:**

- [ ] A window session at 2x, so a device-scale bug is visible to the same tests
  that drive the window today.
- [ ] A live BiDi transport onto the window a person is actually looking at.

## Priority 3 — Finish persistent UI migration

- [ ] Migrate settings, history, about pages, menus, and remaining system
  surfaces from full-tree cache misses to persistent boundaries.
- [ ] Replace coarse inspector body invalidation with keyed/virtualized
  tree/table/list children where profiles justify it.
- [ ] Remove the short-lived widget adapter after the final surface migrates.

**Exit:** no browser-owned surface depends on rebuilding its complete widget
tree for an unrelated visual change.

## Priority 4 — Surfaces, focus, and popups

Focus scopes, the popup lifecycle and the shared dismissal rules are in place
and are documented in `docs/interface.md`; the browser menu and the context menu
are the two consumers they were written from.

- [ ] Traversal *between* roots: Tab from the last chrome control into the page
  and on into the inspector, in a stated order, with the active surface
  following it.
- [ ] Extend pointer capture past the chrome surface. The contract exists —
  `CaptureId`, `Button::capture`, `Cx::take_pointer`, threshold, move, drop,
  cancel — and dragging a tab to reorder it is its consumer. What is left is
  the page's own drags (selection, scrollbars) and the inspector's splitter,
  which still answer *the press began inside my rectangle* and are correct only
  because nothing they drag moves.
- [ ] Implement platform popup windows for a popup that must leave the browser
  window. The in-window backend covers everything that fits, flipping at an edge
  rather than clipping; a long dropdown near the bottom edge is what needs the
  platform window.
- [ ] Give the other surfaces tooltips. The chrome names what the pointer rests
  on, scheduled through the frame deadline the caret already uses; the settings,
  history, downloads and inspector surfaces do not.
- [ ] Build the surfaces that should be on the popup contract and are not built
  at all: a dropdown, and permission prompts (which need something that asks for
  a permission first). Omnibox suggestions are on it, and are what taught the
  contract that a popup need not own the keyboard: theirs stays in the field,
  the arrows walk the rows, and there is no sheet, so a press outside reaches
  what it landed on rather than being swallowed by a dismissal.

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

## Direct-GPU renderer spike

Bounded, outside the live default path, behind `--direct-gpu`. The acceptance
corpus, the seam test, the Skia references at 1x and 2x, the Metal replay and
the connection to the retained compositor all exist. What is left:

- [ ] Settle 1x parity or state an explicit parity policy: 2x passes at 0.011%
  differing pixels; 1x misses the 0.2% allowance at 0.355%, concentrated on
  antialiased path and image edges. Do not weaken the gate silently.
- [ ] Fix cold startup before considering default-on. Steady-state improves
  (corpus p50 1.47 → 0.76 ms at 1x, 6.01 → 0.83 ms at 2x; readback and upload
  become free), but the CPU path's overlap with GPU initialization is lost:
  visible-to-frame p50/p95 regresses 227.99/260.88 → 250.79/298.02 ms, and
  process-to-frame 616.90/661.81 → 632.07/779.97 ms. Test a CPU-first-frame then
  GPU warm/switch design.
- [ ] Adopt only if it removes CPU readback/upload and improves measured p95
  without regressing parity or startup. Keep the retained-damage CPU path as
  the fallback either way.

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

A UI change is also checked where a person would see it: `interface_golden`,
`surface_snapshot`, and `window_interaction`, which drives the real windowed
compositor and asserts on composited pixels. A new surface is run live —
`cargo run -p otlyra-app --example interface`, or `--url … --screenshot` — and
the picture looked at, not only the tests.

Performance changes include before/after distributions. Visual changes include
reviewed 1x/2x artifacts. New caches expose counters and have a regression test
showing unrelated state does not invalidate them.
