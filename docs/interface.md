# The interface layer

The browser's own surfaces — the toolbar, the tab strip, the inspector, and the
pages it serves about itself — are drawn by a widget layer of our own, in
`crates/otlyra-app/src/widget/`. This is what binds a change to it.

The direction of dependency is the thing to keep: a widget never reaches into
layout to get its geometry, and the engine never learns what a toolbar is.

## Invariants

These five are why the layer is worth having. A change that breaks one of them
is a change that needs an argument, not a patch.

1. **Geometry is computed once**, by `Widget::place`, stored on the widget, and
   read by both drawing and hit testing. Neither recomputes anything, so a
   control cannot be drawn in one place and clicked in another. `Overflow` and
   `Placed` are how a frame reports geometry back to whoever needs it, rather
   than that caller deriving it a second way.

2. **Controls hold no state.** Each is a view of a value its caller owns; the
   tree is rebuilt from that state every frame. Hover and press are questions
   with answers — where the pointer is and whether it is down are known when the
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

## Deliberately not built

Decisions rather than omissions.

- **A popup that can leave the window.** The menu is drawn inside the window, so
  one near the bottom edge is clipped by it. Real popups need a platform window
  per popup, which is an `otlyra-platform` change with its own event routing.
  **Context menus** and **a real dropdown** both wait behind it; `segmented`
  stands in for a short list of choices meanwhile.
- **Tooltips.** Needs a timer per surface and somewhere to hang delayed events.
- **Dragging tabs to reorder**, and pinned tabs. The strip scrolls; neither of
  these is in it.
- **A find bar**, which needs text search over the fragment tree — engine work
  first.
- **A live BiDi transport** — attaching to the window a person is looking at,
  rather than driving a browser of one's own.
