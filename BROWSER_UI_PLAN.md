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

The 2026-10-14 [browser rendering/threading audit](docs/browser-rendering-research.md)
compares Chromium, Firefox and WebKit with the current call paths. Its ownership
baseline exposed page work on native UI. Windowed startup now creates the
browser model, Otter, DOM, style and layout on a content owner; native chrome
consumes immutable scenes. Headless drivers keep their synchronous API.

## Standing constraints

- Do not rebuild the scheduling, retained-tree, damage, or surface foundations:
  `FrameRequest`, coalesced redraws, deadlines, keyed render nodes, retained
  tab/toolbar/inspector/popup boundaries, per-layer damage, `UiSurfaceId`
  routing, and focus scopes all exist and are covered by tests.
- Nor the focus scopes, the popup lifecycle or the shared dismissal rules, which
  are in place and documented in `docs/interface.md`. The browser menu and the
  context menu are the two consumers they were written from; the find bar is what
  taught them that a popup may be a *mode* rather than a choice.
- A display list holds glyph identifiers and not the string they were shaped
  from, so **what a surface paints cannot be read back out of one**. The
  accessibility tree holds only controls and the outline snapshots hold only
  geometry, so a golden PNG is the only thing that catches text appearing where
  it should not.
- Zed/GPUI and the Chromium compositor are design references, not dependencies:
  keep stable identity plus a GPU scene, not the crate.
- The CPU backend stays the screenshot, deterministic-test and fallback path
  whatever the GPU work decides.
- Attach AccessKit before first visibility. Do not block the callback after
  `set_visible(true)` to make the visibility milestone appear earlier. Do not
  rasterize before the presenter is ready.
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

The frame-wait follow-up distribution measured on 2026-10-14 UTC, Apple M1, macOS 26.5,
1024x768 at 2x, release, `about:about`, isolated profile and `--no-secrets`:
three warmups, then 40 GPU / 20 CPU fresh processes per version, with
before/after order alternating against the `12c954b` platform runtime rebuilt
with the same Otter Rust/manifests as the candidate. Both binary hashes and that
source digest are in `target/async-frames-build-pair.json`. Later Otter changes
are verified separately; the distributions refer to that fixed build pair.
The driver cache is warm;
these are not cold-shader measurements. Load differs between series: compare
within each row, not across runs or against an older local distribution.

| Startup, p50 / p95 | Before | Current |
|---|---:|---:|
| GPU process → visible | 164.43 / 183.39 ms | 165.34 / 176.29 ms |
| GPU process → first frame report handled | 249.20 / 274.42 ms | 252.73 / 268.16 ms |
| CPU process → visible | 163.17 / 171.52 ms | 156.60 / 168.58 ms |
| CPU process → first frame report handled | 244.58 / 265.53 ms | 241.73 / 255.94 ms |

Browser construction is ~1 ms. Explicit phase marks corrected the earlier
attribution: Metal backend construction is ~0.01 ms, device/swapchain/pipeline
setup ~5 ms, and delivery of its Ready report to the UI takes ~45–60 ms under
local load. The old ~44 ms "backend discovery" interval mostly measured native
event dispatch. Chrome construction is ~7–8 ms and GPU raster ~24–25 ms.
The strict startup budgets still fail. Raw matched distributions are
`target/startup-async-frames-gpu-40.json` and
`target/startup-async-frames-cpu.json`; each includes its control distribution
and both binary hashes. GPU paired first-frame delta p50 is -0.44 ms: startup
is essentially unchanged; the change removes UI waiting during rendering.
The 20-pair pilot had a higher candidate GPU p95, so the larger GPU series was
run before accepting the startup result. Do not mix their quantiles.
The subsequent content-owner distribution and retained-damage measurements are
in the [rendering audit](docs/browser-rendering-research.md). Its native UI frame
construction excludes page work moved to content; the original intervals above
describe the earlier ownership boundary.

- [ ] Re-run the 20-sample local distribution after every startup change.
- [x] Attribute native event-loop construction, icon decode/installation, menu
  validation/object creation/installation, and hidden-window creation with
  independent startup marks. Keep worker overlap separate from adjacent stages.
- [ ] Resolve the internal AppKit/dynamic-initialization stacks behind those
  intervals; do not move their cost behind the visibility mark.
- [x] Recover an initial coalesced redraw once after render readiness if native
  startup has delivered no frame. Keep its owed frame, avoid retrying started or
  hidden work, and leave later redraw/damage scheduling unchanged.
- [ ] Reproduce the rare initial native-frame stall with controlled fault
  injection and verify recovery end to end. Baseline/deadline-only releases
  stalled; the new-events path completed 100 starts without exercising the
  recovery branch. Keep this distinct from unit coverage and startup speed.
- [ ] Profile the Ready-delivery gap in native launch/window event dispatch.
  Backend construction is already negligible; an extra bootstrap worker and
  draining messages before event-loop wait did not improve startup.
- [ ] Profile the new worker posting/wake and UI-receipt intervals with native
  traces. `first_present_complete` retains UI-accounting semantics; the separate
  `render_present_scheduled` mark is not OS presentation feedback either.
- [ ] Validate live resize visual quality under heavy raster. First-frame/resize
  submissions no longer wait synchronously: the last presented image remains
  while input continues and one latest-viewport successor is owed. Native resize
  may temporarily stretch the old image. The controlled-worker test checks input
  and coalescing at 1x/2x and rejects restoration of the 100 ms wait.
- [ ] Profile first-frame shader/font work independently of chrome list
  construction. Add overlap only when it improves distributions; an extra
  speculative `compose` before GPU readiness did not.
- [ ] Measure normal saved-profile loading separately from the isolated
  distribution, including preferences, cookies, history and update services.
- [ ] Keep page accessibility, resource services and Otter creation out of the
  first chrome frame; defer further construction only when it has a measured
  cost. System fonts are discovered on a worker and shared by deferred engines.
- [ ] Audit font file opening and first shaping, beyond collection discovery.
- [ ] Reuse raster readback and upload buffers.

**Exit:** the strict startup workflow passes on the reference runner.

## Page/UI isolation — prerequisite for live responsiveness

Follow the [threading audit's ordered implementation](docs/browser-rendering-research.md#ordered-implementation-and-acceptance).
This work crosses the browser/page boundary rather than replacing UI foundations.

- [x] Move windowed page execution behind a content owner created on its own
  thread: Otter, DOM, parsing, style, layout, timers and rAF. Publish retained
  scenes, tab/navigation/viewport metadata, accessibility trees and inspector
  output. Keep page hit testing and synchronous DOM semantics inside content.
  Native chrome owns its input, geometry and font-engine fork; ordered tab
  intents project immediately over the owner's read model.
- [x] Remove persistent-cookie sealing/file writes from the UI pump while
  keeping serialized atomic persistence; acknowledge only successful writes,
  retry failures and flush pending revisions at shutdown. One coalescing writer
  runs key lookup, serialization, sealing and replacement. Native close sends
  shutdown immediately; the CLI joins content only after its event loop returns.
- [x] Own fetch futures and cancel/drain them on content's owner before final
  cookie persistence. Close the permit queue before aborting, suppress teardown
  wakes and preserve independent pools on the shared runtime. Blocking test
  loaders and started decode jobs can still finish off-thread.
- [x] Own accepted download-save tasks and finish them on content shutdown,
  including exit before native surface confirmation. Keep native Drop
  nonblocking; collect terminal success/failure before final cookie persistence.
- [ ] Resolve other started blocking I/O at process shutdown. Fetch cancellation
  cannot interrupt a running `spawn_blocking` loader/decode job.
- [x] Reflect active-tab visibility in `document.hidden` / `visibilityState`,
  including the first script and paused parsers; dispatch `visibilitychange`
  on content's owner. Retain hidden rAF callbacks until activation and exclude
  inactive slots from frame execution and Vsync requests.
- [x] Propagate confirmed native window occlusion/minimization to content's
  visibility. Stop hidden native raster/Vsync, preserve unpresented damage and
  restore at the latest viewport. Unfocused visible windows remain visible.
- [x] Defer hidden-window style/layout, scene preparation and document
  accessibility generation while continuing timers and loads. Publish metadata
  without replacing retained scenes; restore with full damage at the latest
  viewport. Geometry-bound input explicitly prepares deferred content.
- [x] Suppress identical hidden metadata publications and native UI wakes while
  preserving tab/load/title/viewport changes and ordered command acknowledgements.
  Retain metadata and system descriptions behind immutable shared handles; timer
  deadlines stay on content's scheduler without waking hidden native UI.
- [ ] Define background timer budgets/exemptions. Timers currently continue
  normally; changed hidden metadata still publishes.
- [x] Yield timer work between complete callbacks and microtasks, retaining
  unrun deadlines. Use a four-ms page slice and an eight-ms soft shared pass,
  resuming at the next slot so busy tabs cannot monopolize every pass. Enforce
  elapsed settle budgets even when older deadlines remain due. A long callback
  can overrun these scheduling deadlines; its watchdog is unchanged.
- [x] Align `performance.now()`, `timeOrigin` and rAF timestamps to each
  document's navigation origin on a shared monotonic 100-microsecond grid.
  Include fetch wait, timestamp the first deferred frame, preserve the origin
  across encoding resets and retain legacy runner/bare-host contracts.
- [x] Supply window CSS dimensions and device scale before the first script;
  update them on resize, zoom and inspector docking, including paused parsers
  and encoding resets. Retain per-tab zoom and dispatch resize with current
  metrics on content's owner; a pure DPR change does not resize the CSS viewport.
- [x] Confirm the native surface's viewport/DPI before the first page script.
  Native content waits after the chrome metadata handoff; `SurfaceReady`
  establishes actual metrics before ready loads are pumped. Preserve queued
  UI input and allow shutdown while waiting. Known-viewport/headless owners
  retain immediate execution.
- [x] Respect accessibility action intent on native chrome and all system pages:
  Focus does not activate controls, Activate follows the click path, range steps
  use existing adjustments and accumulate before redraw. Advertise only supported
  actions on enabled focusable controls, including native slider adjustments.
- [ ] Give dynamic accessibility controls stable semantic identities instead of
  layout positions, and reject stale native/queued requests after a target retires.
  Focus ids are positional too; substituting them for indices is insufficient.
- [x] Bind retained tab select/close actions to TabId, preserving their targets
  through model reorder and ignoring retired ids. Include system-page kind in
  queued content contexts and accessibility retirement; about-page transitions
  can retain a prior navigation id.
- [x] Bind native content menus to their tab/document/system-page context.
  Retire them on owner dismissal or replacement, acknowledge requests with no
  offered menu, and reject stale queued commands even if a replacement opens
  another menu. Preserve the browser menu's existing lifecycle.
- [ ] Add eligible compositor scrolling against retained page output, with
  cancellable input, nested handoff and script-scroll reconciliation preserved.
- [ ] Extend the existing bounded frame scheduler with display deadlines and
  active/pending scene preparation; do not queue obsolete frames or share a live
  GPU context across threads.
- [ ] Measure omnibox/tab responsiveness during finite busy JS, slow layout,
  slow raster and live resize, separately from empty-profile startup.
  Finite real JS and 1x/2x CPU compositor coverage exist; native input-to-present,
  large-document layout and active desktop resize remain to measure.
- [x] Preserve incremental damage across coalesced content publications and
  acknowledged rendered frames. Mailbox/UI replacements union pending damage;
  successful render reports clear only matching cached layer versions. Unknown
  damage and tab/viewport/layer transitions retain conservative full redraws.
  1x/2x field edits, disjoint skipped updates, late completion and dropped frames
  have pixel-parity coverage; CPU raster comparisons exclude native presentation.
- [x] Avoid copying the full retained document accessibility tree when only
  native chrome changes. Publish changed window controls and focus; retain the
  complete tree for late AccessKit activation. Clear old document nodes during
  pending navigation and scope focus before joining chrome/system descriptions.
- [ ] Make content accessibility generation incremental after DOM/layout
  changes. The owner still builds a full snapshot for semantic invalidations;
  equal snapshots retain their Arc, but generation/comparison still walks it.

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

- [ ] Put the inspector in the traversal order. Chrome and page hand the
  keyboard to each other at either end and `tabindex` is read; the panel is
  still reachable only by pressing into it. It needs a stated place in the
  round trip — after the page, before the chrome — and its own edge detection.
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
  history, downloads, bookmarks, cookies and inspector surfaces do not.
- [ ] Put plain text in the accessibility tree. Only controls are described, so
  a heading, a hint and an empty state are invisible to a screen reader on every
  browser-owned page — `about:cookies` says *No site is keeping anything here*
  and nothing hears it. `Named::instead_of_its_own` is how a row renames the
  control inside it; what is missing is a `Described` for a label that is not a
  control at all.
- [ ] Build the surfaces that should be on the popup contract and are not built
  at all: a dropdown, and permission prompts (which need something that asks for
  a permission first). Omnibox suggestions are on it, and are what taught the
  contract that a popup need not own the keyboard: theirs stays in the field,
  the arrows walk the rows, and there is no sheet, so a press outside reaches
  what it landed on rather than being swallowed by a dismissal.

**Exit:** every popup uses one event/focus/semantics contract and can leave the
browser window when required.

## Next large feature — not chosen yet

The two the last one was picked over, either of which is the obvious next:

- **The half-drawn controls.** `range`, `progress`, `meter`, `date`, `time`,
  `color` and `file` are a shape and no behaviour. A page that asks for a slider
  gets something a person cannot move.
- **The HTTP cache on disk.** Built, attached, and driven by the two reload
  modes; it lives for the life of the process. Surviving a restart is what would
  make a *cold start* fast rather than a second navigation, and it is a different
  problem: a format, a size budget measured against a real disk, and an answer to
  two windows writing at once. Not sealed, unlike the cookie file — Chrome and
  Firefox both keep their caches in the clear, and the thing in it is content the
  reader has already seen rather than a credential. Say so out loud when it
  lands, because "the cookies are sealed and the cache is not" is a sentence
  somebody will otherwise have to reconstruct.

What the finished ones left behind, which is work and not history:

- [ ] Hold the reader's place across a *resize*, not only across a zoom.
  `PageScene::hold_the_reader_s_place` records a position in the text and puts
  the page back at it after the next layout; the zoom uses it and a resize does
  not, so dragging a window edge still throws the reader down the page. Every
  relayout wants this, not only the two that have it.
- [ ] Send a `Referer`, under a policy. Nothing sends one, and a redirect hop is
  where a policy would be applied — `Loader`'s chain is the place, now that the
  chain is ours.
- [ ] Show a refused cookie in the inspector. `otlyra_net::cookie::Refused` names
  which rule dropped it, which is exactly what a person debugging their own site
  needs and nothing currently displays.

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

## GPU renderer follow-up

The window already uses the direct GPU path by default, with automatic CPU
fallback; `--cpu-raster` forces the fallback. Screenshots and deterministic
tests stay on CPU. The acceptance corpus, seam test, Skia references at 1x/2x,
Metal replay and retained compositor connection exist. What is left:

- [ ] Settle 1x parity or state an explicit parity policy: 2x passes at 0.011%
  differing pixels; 1x misses the 0.2% allowance at 0.355%, concentrated on
  antialiased path and image edges. Do not weaken the gate silently.
- [ ] Profile first-frame shader/font work with a cold driver cache separately
  from fresh-process startup with a warm driver cache. Keep the strict startup
  gate. A CPU-first-frame/GPU switch needs measured evidence before adding a
  second bootstrap path.
- [ ] Measure live input-to-present p95 for cached omnibox, caret, scroll, and
  path/image-heavy pages. The warm corpus improves p50 (1.47 → 0.76 ms at 1x,
  6.01 → 0.83 ms at 2x) but does not establish live interaction latency.
- [ ] Exercise device-loss fallback in the window and preserve retained damage
  through the switch to CPU.

## Required invariants

- Browser/page models own navigation, tabs, settings, documents, and other
  application state.
- UI runtime state must not become a second copy of browser model state.
- Core widgets remain generic over typed actions and know nothing about the
  browser.
- Paint and hit testing use the same stored geometry.
- Page layout never depends on chrome layout.
- The UI thread never blocks on page JavaScript, network, or filesystem work.
  Windowed page execution/layout and cookie flushes now run on content. Initial
  saved-profile setup still precedes the window handoff; native modal dialogs
  run on UI, and file bytes are read by content after the dialog closes.
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
python3 -m unittest discover -s tools -p 'test_*.py'
```

On a visible macOS desktop, also run
`cargo test -p otlyra-app --test window_startup -- --ignored`. This checks real
GPU/CPU window startup and is enabled in the desktop performance workflow.

A UI change is also checked where a person would see it: `interface_golden`,
`surface_snapshot`, and `window_interaction`, which drives the real windowed
compositor and asserts on composited pixels. A new surface is run live —
`cargo run -p otlyra-app --example interface`, or `--url … --screenshot` — and
the picture looked at, not only the tests.

Performance changes include before/after distributions. Visual changes include
reviewed 1x/2x artifacts. New caches expose counters and have a regression test
showing unrelated state does not invalidate them.
