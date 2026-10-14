# The interface layer

The browser's own surfaces — the toolbar, the tab strip, the inspector, and the
pages it serves about itself — are drawn by a widget layer of our own, in
`crates/otlyra-app/src/widget/`. This is what binds a change to it.

The direction of dependency is the thing to keep: a widget never reaches into
layout to get its geometry, and the engine never learns what a toolbar is.

## Invariants

These nine are why the layer is worth having. A change that breaks one of them
is a change that needs an argument, not a patch.

1. **Geometry is computed once**, by `Widget::place`, stored on the widget, and
   read by both drawing and hit testing. Neither recomputes anything, so a
   control cannot be drawn in one place and clicked in another. `Overflow` and
   `Placed` are how a frame reports geometry back to whoever needs it, rather
   than that caller deriving it a second way.

2. **Controls hold no browser model state.** Each is a view of a value its
   caller owns. Persistent render nodes may retain identity, geometry,
   measurement, focus/capture membership, semantics, animation progress, and
   render caches; they may not become a second copy of a URL, preference,
   document, tab, or navigation state. Hover and press are questions with
   answers — where the pointer is and whether it is down are known when the
   frame is drawn — not flags to keep up to date.

3. **Widgets paint into a `DisplayList`**, never a live canvas, so the interface
   stays backend-independent and snapshot-testable through `RecordingPainter` on
   the same seam the page uses.

4. **The layer knows nothing of the browser.** `Widget<A>` is generic over the
   action; `ui.rs` reports `UiAction`, `settings.rs` its own, and neither enum
   appears in `widget/`. The inspector reports *what to set* and never sets it,
   because it does not hold the document.

5. **One of each thing.** No second toolkit, no second text stack, no second
   rasterizer, no second event model — for the toolbar, for the settings, for
   the devtools panels, for anything. A browser already *is* a rendering engine
   and a hit tester; a second one of either does not add capability, it adds a
   seam where two answers to the same question have to be kept agreeing, and it
   is one more thing to keep alive at every future change. When something is
   missing, the answer is to improve what we own or to write the small piece we
   lack — both of which stay ours. `deny.toml` lists the crates this rules out,
   so a dependency that breaks it fails CI rather than a review.

6. **One active surface.** Chrome, the page, browser-owned pages, the inspector,
   and future popups are independent roots with stable `UiSurfaceId`s. Exactly
   one owns keyboard, text/IME, clipboard editing, and accessibility focus.
   Input is never offered down a list until some stale root happens to consume
   it. A pointer press, accessibility action, or explicit command changes the
   active root and blurs roots that remain visible in the background.

7. **A drag names its control, not its rectangle.** A press that starts a drag
   is answered by *the press began inside my rectangle* — one fact, no state —
   and that is right for everything that stays where it was drawn. A control
   that *moves* while dragged takes the pointer instead (`Button::capture`,
   `Cx::take_pointer`), so the rest of the drag reaches it wherever it has got
   to, and nothing else answers a move that is not theirs. A drag starts only
   past a threshold, so a click with an unsteady hand stays a click, and Escape
   takes it back.

8. **What the pointer rests on is named, after a pause.** The delay is
   scheduled through the same frame deadline the caret blinks on — the loop is
   woken once, at the earlier of the two — and the name is the control's own,
   the string a screen reader is given, so what is shown and what is said cannot
   drift. It is not a popup: it takes no focus, catches no press, and goes when
   the pointer moves off, when anything is pressed, or when a key is struck.

9. **A popup owns the pointer, and usually the keyboard, until it is
   dismissed.** One popup at a time per surface. A menu owns the keyboard: it
   opens a traversal scope its rows claim their focus ids in, so Tab reaches
   those rows and wraps there instead of walking to controls behind the sheet.
   A list under a field the reader is still typing into owns neither the
   keyboard nor the whole window: it opens no scope, the arrows walk its rows
   without taking one, and it has no sheet — so a press outside reaches what it
   landed on and the list goes away on the way through. Four ways out, and all four are one
   code path: a press outside it, Escape, the keyboard being named elsewhere,
   and the thing it belongs to going away — a navigation, a resize, another
   root becoming the active one. Escape puts the keyboard back where it was;
   the others do not, because a ring appearing after a click answers a question
   nobody asked.

## Before a commit

```
cargo test --workspace
cargo clippy --workspace --all-targets     # empty
cargo fmt --all
cargo run -p otlyra-app --example interface -- /tmp/ui
```

Look at the snapshots at scale factor 1 **and** 2. Tests are on behaviour, not
pixels. A press is tested against the previous frame: draw, then press. Where a
cache is added, add `builds()` beside it. Goldens are redrawn deliberately with
`OTLYRA_UPDATE_GOLDEN=1`; snapshots are `cargo insta`.

## Font discovery and startup

The browser constructs a deferred `TextEngine` and starts system font discovery
on one worker while AppKit creates the window. The page world shares the result
through a deferred fork. Each engine takes its own mutable collection on first
use, so page `@font-face` registrations stay isolated. A first shape can wait
for discovery; worker creation failure falls back to discovery on first use.

Settings, history and the inspector fork their painting engine on first draw.
Their first input therefore uses the fonts layout used, without rediscovering
system fonts on the UI thread. `TextEngine::new` remains eager for callers that
need it; isolated measurement engines still use only the vendored family.

On macOS the Dock icon retains its full-resolution encoded representation and
transparent margins, with a logical canvas capped at 512 points. This bounds
AppKit's icon installation work before visibility. The startup trace separates
image creation from installation.

Metal's layer is attached to the hidden window after AccessKit, before first
visibility. `SurfaceTarget::DisplayAndWindow` owns both handles through the
window; adapter/device, swapchain and shader setup stay on the render thread.
Painting starts only after its `Ready` report. Other platforms retain their
display-bound backend discovery worker. When an embedder supplies a native
menu, macOS skips winit's temporary default menu; an embedder without one keeps
the standard menu.

Startup distributions use an isolated empty profile and `--no-secrets`, with
three warmups and at least 20 fresh processes. GPU is the default;
`tools/startup-benchmark.py --cpu-raster` measures CPU. Reports record the binary
SHA-256 as well as the source revision. Keep profile hydration and cold driver
cache measurements separate, and alternate before/after binaries when system
load drifts. The strict budgets in `BROWSER_UI_PLAN.md` are unchanged.

`--baseline-binary /path/to/previous/otlyra` performs that alternation within
one benchmark, with separate temporary profiles and matched process pairs.
The candidate stays at the report root; `comparison.baseline` records the
control distribution and hash, and `comparison.paired_delta_ms` records
candidate minus baseline. `--check` still enforces the candidate's budgets.

Milestone gaps are elapsed intervals, not exclusive work attribution. `phases`
uses explicit start/end marks for backend discovery, device setup, delivery of
`Ready` to the UI, chrome construction, GPU surface preparation and raster.
For example, a backend created in 0.01 ms can wait tens of milliseconds for
native event dispatch; that wait must not be called GPU initialization.
First frames and resizes are submitted asynchronously, just like steady frames.
The previous presented image remains until the new frame is scheduled; a native
resize can temporarily stretch it, but never waits up to 100 ms for the worker.
Only one frame is in flight; events arriving meanwhile owe one successor built
for the latest viewport and state. A controlled-worker regression checks input
delivery and coalescing at 1x and 2x, and rejects the former 100 ms wait.

`render_present_scheduled` marks the render thread returning from
`Queue::present`; `render_report_posted` marks the posting attempt before the
channel send, and `first_frame_report_received` marks UI handling. Ready has
corresponding posting and wake-call marks. Worker wake calls can return after
the UI handles the message; missing late marks are valid in the first-frame
report. These milestones split render work, message delivery and UI accounting.
`first_present_complete` and the existing startup budgets retain their previous
UI-accounting meaning. None of these marks is OS presentation feedback.

The opt-in `window_startup` test runs actual GPU/CPU windows, checks the first
presented frame and ensures the Metal layer precedes visibility. It runs on the
desktop performance runner; offscreen compositor tests cannot check that API
boundary.

## Window content ownership

Normal window startup uses `browser::WindowBrowser`. Its factory constructs
`Browser`, `PageWorld` and all script worlds on `otlyra-content`; live isolates
and documents never cross threads. Native UI owns `BrowserUi`, its stored hit
geometry, an independent text-engine fork over the shared system-font seed,
and the system clipboard. The existing render worker owns rasterization and
presentation. Screenshots, MCP and BiDi retain the local synchronous Browser.

UI receives owned tab views without navigation-history copies, retained scenes,
cursor/focus metadata and accessibility trees. Pending tab intents are applied
to the read model immediately and acknowledged by sequence. Commands target
stable tab IDs; document input and document-specific actions carry navigation
generations. Consecutive pointer moves coalesce without crossing keys/buttons;
viewport requests and scene publications each keep one pending value. Font,
DOM, script and application work runs outside the shared mailbox lock.

Clipboard reads happen on UI when paste is requested. Content copy/cut writes
are posted back to UI with acknowledgements; queued paste preserves an earlier
unacknowledged copy. File, Save As and directory dialogs stay on UI; selected
file bytes are read on content, and old-document answers are rejected. Editable
page fields now share cut/copy/paste between both painter modes, including UTF-8
and normalized multiline textarea paste. Address/find focus requests claim
input before their first frame and are cleared on submit/blur.

Chrome composes over the latest ready content while its owner is busy. Empty
or pending tabs use the existing blank-page painter, and cached content is
rescaled once per list/scale pair during DPI changes. The owner sleeps on its
inbox or the next content deadline; it does not poll. Closing UI signals the
owner without joining a running script turn.

Partial content damage survives scene coalescing. Overwritten mailbox results
and received UI scenes accumulate the union of unacknowledged rectangles;
unknown damage dominates, while an unchanged epoch adds no new damage.
`Painter::frame_complete` confirms the last submitted frame before its successor
is composed. The native render report and the offscreen FramePump use that same
boundary. Only successful presentation clears cached damage with matching tab,
viewport, layer, epoch and rectangle; newer cached content keeps its damage.
Dropped/occluded frames keep pending damage and the renderer discards its retained
baseline. Tab activation, layer changes, viewport changes and cached DPI scaling
keep full damage where a partial rectangle cannot describe the transition.
The implementation retains one rectangle per cached layer, not a frame history.

Window accessibility uses incremental `TreeUpdate`s. Native chrome descriptions
are cached by their rendered build; changed controls alone are published while
the retained document Arc stays unchanged. Focus-only changes contain no nodes,
and unchanged wakes/hover publish no update. Content keeps equal semantic
snapshots on the same Arc within one tab/navigation generation. Pending
navigation exposes an empty document until the current owner reply arrives.
The platform merges updates with the same AccessKit consumer used by its
adapters; a screen reader attaching later receives a complete current tree,
including the latest focus and excluding removed descendants.

Text fields now describe their actual focus IDs even when empty. Chrome and
system-page focus is resolved within its owning surface before their flat
descriptions are joined, avoiding collisions between local IDs. Toolbar semantic
invalidation includes bookmark labels, loading-state transitions and changes to
the tab-strip focus prefix. Shared datalist options have one accessible parent:
the focused/open field, or the first rendered editable field. Taking a suggestion
focuses and updates that same field, preserving a valid tree when lists are shared.

`content_owner_ready`, `first_content_scene_ready`,
`first_content_scene_received` and `first_content_scene_composed` separate
construction, publication, receipt and use in a native frame. Receipt intervals
include time while AppKit initializes; they are not exclusive IPC work. The
About startup smoke requires real content in the first eligible startup frame.
Early chrome-only frames remain interactive. Each render job captures
`Painter::startup_ready`; an early frame cannot satisfy the startup report even
if content arrives before its completion is handled. The first eligible frame
carries the startup build/raster/report marks, and `early_chrome_present_complete`
records the earlier UI accounting separately. Strict budgets remain unchanged.

This is one content owner per window: pages, system tabs and inspector work can
still wait for another page's turn. Native live BiDi attachment, hidden-page
scheduling, interruptible content/raster work,
incremental content accessibility generation and durable nonblocking shutdown persistence
remain follow-ups. Independent compositor scrolling and display-driven frame
deadlines remain separate from preserving partial damage.

## Things that cost a stage to learn

**A control with no words in it has no name.** Every icon button was reported to
a screen reader as a button called nothing, because a button takes its name from
the label inside it and a mark is not a label. `icon_button` now *requires* a
name, so the compiler asks rather than a reviewer. `Named` is the same gap in
another shape: a switch and a slider hold no words either, and the row they sit
in is what knows their name.

**A constructor that reads a file makes every test depend on a machine.**
`Browser::new` loaded preferences from the home directory, so a test that threw
a switch wrote the *developer's* file and the next test read it back. Two image
tests failed on one machine only. Preferences are handed in now.

**"The engine has no seam for this" is a claim, not a fact.** Four times now.
`text_scale` was said to need a cascade seam that did not exist — the seam was
there and its own comment said so. The network pane was said to have no HTTP
status — the status was one crate down, being dropped on the way up. The
accessibility pane was said to need a tree built — the tree was already being
built and handed to a screen reader. And *which rule set this value* was written
off as needing a way through `otlyra-css` that did not exist — the cascade
already hangs the chain of winning declarations off every computed style, and
reading it is not matching a second time.

**A child is measured against the whole of its parent, not against its share.**
`Stack::measure` hands every child the full available size and only `place`
divides it up — right, because a flexible child takes its size from the
leftover. But anything that *records* something during `measure` records it
against a box it will not get. Two bugs from the one cause: a list that stored
how far it could scroll came up short and could not reach its last rows, and
`Fixed` reported a pin wider than the room going, so a paragraph counted its
lines at one width and was drawn at another. Whatever `measure` records has to
be re-derived in `place`, against the rectangle actually given.

**Two independent bits do not fit in one enum.** `white-space` is *whether
spaces collapse* and *whether lines may break*, and modelling it as one
two-valued enum could spell `normal` and `pre` but never `nowrap`. The missing
half was read as the default and the site's header folded onto two lines.

**A picture's intrinsic size is the picture's own pixels.** Anything else
written there — a `width` attribute, say — takes the aspect ratio with it, and
the picture is drawn squashed on one axis. Presentational hints belong beside
the intrinsic size, not in it.

**A flex item is blockified, so it never reaches the inline branch.** Both
intrinsic-width functions fall back to taking the maximum over the children,
which is right for boxes that stack and wrong for boxes that sit in a row. A
logo beside a wordmark came out as wide as the wordmark alone.

- **Look at what you changed.** `cargo run -p otlyra-app --example interface --
  /tmp/ui` writes every state to PNG. A layout change that is not looked at is a
  layout change that is wrong at some window size.
- **Two scales, always.** The one interface bug that got furthest was invisible
  at 1× and doubled everything inside a scrolling panel at 2×.
- **A press is tested against the last frame**, and so is anything a frame
  reports — how far a list can scroll, where a tab landed, whether a field
  exists to type in. A state that acts on either has to have had a frame drawn
  first.

## Tab commands

Closing a background tab keeps the active tab's identity, keyboard surface and
address edit. Closing the active tab selects the neighbour on its right, or the
one on its left at the end of the strip; closing the last tab leaves a blank tab.

The History menu and Command/Control+Shift+T reopen the most recently closed tab.
The browser keeps metadata for at most 25 closed tabs during this session,
including their back/forward entries, history position and scroll. Reopening
loads the page again with a fresh context identity and reuses an untouched blank
tab. Documents, form edits and script heaps are released on close; this is not
session persistence across browser restarts. Window → Duplicate Tab creates an
independent copy beside the active tab, with its history and reading position.

Control+Tab and Control+Shift+Tab select the next and previous tabs and wrap at
the ends. Control+PageDown/PageUp and Command/Control+Shift+]/[ do the same.
Command/Control+1 through 8 select the corresponding tab; 9 selects the last.
These commands work from every keyboard surface, including page fields and the
inspector. Option/Alt+Enter in the address field opens the typed address or
marked suggestion in a new foreground tab.

## Deliberately not built

Decisions rather than omissions.

- **A popup that can leave the window.** Popups are drawn inside the window and
  flip back onto it at an edge rather than being clipped by it, which is enough
  for a menu and for the menu a reader asks for over the page. One that must
  *leave* the window — a long dropdown near the bottom edge — needs a platform
  window per popup, which is an `otlyra-platform` change with its own event
  routing. **A real dropdown** waits behind it; `segmented` stands in for a
  short list of choices meanwhile.
- **Pinned tabs.** The strip scrolls and its tabs can be dragged into a new
  order; pinning is not in it.
- **A live BiDi transport** — attaching to the window a person is looking at,
  rather than driving a browser of one's own.
