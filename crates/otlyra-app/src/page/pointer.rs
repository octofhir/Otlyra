//! The pointer: what is under it, and what a press, a move and a release do.
//!
//! Every answer is read from the last frame's hit-test targets, because a press
//! lands on what the reader was looking at rather than on what would be built now
//! — which is also why the reverse question, where a box was drawn, is answered
//! here. Its own module because the walk a press takes — from a point to a box,
//! from the box to its element, from the element to the control it acts on — is
//! one route, and everything that takes it is here.

use otlyra_dom::NodeId;
use otlyra_layout::BoxId;

use super::PageScene;
use super::field::word_around;

impl PageScene {
    /// The topmost box at `point`, in window logical coordinates.
    ///
    /// Reads the last frame's targets: a click lands on what the user was looking
    /// at, which is the frame that was on screen, not the one that would be built
    /// now.
    pub fn box_at(&self, x: f64, y: f64) -> Option<BoxId> {
        let point = otlyra_gfx::kurbo::Point::new(x, y);
        self.targets
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, id)| *id)
    }

    /// The element the pointer is over, walking out of the box tree to the nearest
    /// node that has one.
    ///
    /// Text inside a button belongs to an anonymous box, and the button is two
    /// boxes above it — so a press on the word "Send" has to find the button the
    /// same way a click on a link finds the link.
    fn node_at(&self, x: f64, y: f64) -> Option<NodeId> {
        let mut current = self.box_at(x, y);
        while let Some(id) = current {
            let node = self.boxes.get(id)?;
            if let Some(node) = node.node {
                return Some(node);
            }
            current = node.parent;
        }
        None
    }

    /// The control a press at this point acts on, if any.
    ///
    /// A `<label>` acts on the control it names, which is what makes the words
    /// beside a checkbox tick it — and it is the label's own activation behaviour
    /// rather than a special case in the hit test.
    fn control_at(&self, x: f64, y: f64) -> Option<NodeId> {
        let mut current = self.node_at(x, y);
        while let Some(node) = current {
            if otlyra_dom::form::Control::of(&self.document, node).is_some() {
                return Some(node);
            }
            if let Some(labelled) = otlyra_dom::form::labeled_control(&self.document, node) {
                return Some(labelled);
            }
            current = self.document.get(node).and_then(|inner| inner.parent);
        }
        None
    }

    /// Whether a press here lands on a control rather than on the page.
    pub fn control_under(&self, x: f64, y: f64) -> bool {
        self.control_at(x, y).is_some()
    }

    /// The pointer moved to this point. Returns whether the page has to be drawn
    /// again.
    pub fn pointer_moved(&mut self, x: f64, y: f64) -> bool {
        // A slider being dragged owns the pointer until it is let go, wherever the
        // pointer wanders.
        if let Some(node) = self.sliding {
            return self.slide_to(node, x);
        }
        // A drag inside a field takes the letters it passes, wherever the pointer
        // wanders — the same rule a drag across the page follows.
        if self.field_dragging
            && let Some(node) = self.focused_field()
            && let Some(box_id) = self.boxes.box_for(node)
            && let Some((_, fragments)) = self.layout.as_ref()
            && let Some(offset) =
                otlyra_layout::selection::offset_in(fragments, box_id, x as f32, y as f32)
        {
            let value_len = self.field_text(node).len();
            self.caret = offset.min(value_len);
            self.restart_caret();
            return true;
        }
        let hover = self.node_at(x, y);
        self.set_interaction(otlyra_css::state::Interaction {
            hover,
            ..self.interaction
        })
    }

    /// The pointer left the page.
    pub fn pointer_left(&mut self) -> bool {
        self.set_interaction(otlyra_css::state::Interaction {
            hover: None,
            active: None,
            ..self.interaction
        })
    }

    /// The pointer went down. Returns whether the page has to be drawn again.
    ///
    /// The focus moves on the press rather than on the release, which is what every
    /// platform does and what makes a click-and-drag inside a field select rather
    /// than move the focus somewhere else halfway through.
    pub fn pointer_pressed(&mut self, x: f64, y: f64) -> bool {
        self.pointer_pressed_times(x, y, 1)
    }

    /// The same, told how many presses in a row this is.
    ///
    /// A second press takes the word under it and a third the whole of what the
    /// field holds, which is what a second and a third press mean everywhere else.
    pub fn pointer_pressed_times(&mut self, x: f64, y: f64, clicks: u32) -> bool {
        self.keyboard = false;
        let target = self.control_at(x, y);

        // A press on an option belongs to the list it is in, and the list belongs
        // to the control it hangs off: pressing "Beta" is pressing the drop-down.
        let mut open = self.interaction.open;
        let mut took = false;
        // A suggestion belongs to the field it was offered to, which is whichever
        // field is showing the list: a `<datalist>` can be named by any number of
        // them.
        let suggested =
            target.and_then(|node| otlyra_dom::form::suggested_control(&self.document, node, open));
        let owning = target
            .and_then(|node| otlyra_dom::form::owning_select(&self.document, node))
            .or(suggested);
        if let Some(node) = target {
            match otlyra_dom::form::Control::of(&self.document, node) {
                Some(otlyra_dom::form::Control::Select)
                    if otlyra_dom::form::is_mutable(&self.document, node) =>
                {
                    // A press on the control itself opens the list, and a second
                    // one puts it away again.
                    open = (open != Some(node)).then_some(node);
                }
                Some(otlyra_dom::form::Control::Option) => {
                    if let Some(select) = otlyra_dom::form::owning_select(&self.document, node)
                        && open == Some(select)
                        && !otlyra_dom::form::is_disabled(&self.document, node)
                    {
                        self.choose_option(select, node);
                        open = None;
                    } else if let Some(field) = suggested {
                        self.take_suggestion(field, node);
                        took = true;
                        open = None;
                    }
                }
                // A press on a slider takes hold of it and puts it where the
                // pointer is, which is what a press anywhere on a track means.
                Some(_) if self.slider(node) => {
                    open = None;
                    self.sliding = Some(node);
                }
                // A press in a field that has suggestions shows them, the same way
                // a press on a drop-down shows its options.
                Some(_)
                    if otlyra_dom::form::takes_suggestions(&self.document, node)
                        && otlyra_dom::form::is_mutable(&self.document, node)
                        && !otlyra_dom::form::suggestions_of(&self.document, node).is_empty() =>
                {
                    open = (open != Some(node)).then_some(node);
                }
                // A press on anything else puts an open list away, which is what
                // pressing away from a menu means everywhere.
                _ => open = None,
            }
        } else {
            open = None;
        }

        let target = owning.or(target);
        let focus = target.filter(|&node| self.is_focusable(node));
        if let Some(node) = focus {
            // Between the two letters the pointer is between, and at the end of
            // what is there when the press landed past it — which is what a click
            // into a field means anywhere else. Only when the field is showing what
            // it holds: a placeholder is not the value, and an offset into one is
            // not an offset into the other.
            let value = self.field_text(node);
            let landed = (!value.is_empty() && !took)
                .then(|| {
                    let box_id = self.boxes.box_for(node)?;
                    let (_, fragments) = self.layout.as_ref()?;
                    otlyra_layout::selection::offset_in(fragments, box_id, x as f32, y as f32)
                })
                .flatten();
            self.caret = landed.unwrap_or(value.len()).min(value.len());
            self.field_anchor = Some(self.caret);
            // A press in a date field takes hold of the part it landed in rather
            // than putting a caret between two digits.
            if self.segments_of(node).is_some() {
                let index = self.segment_at(node, self.caret);
                self.hold_segment(node, index);
                return self.set_interaction(otlyra_css::state::Interaction {
                    active: target,
                    focus,
                    focus_visible: true,
                    open: None,
                    suggestion: None,
                    ..self.interaction
                });
            }
            // A press that took a suggestion landed on the list rather than in the
            // field, so what follows it is not a drag across the field's letters.
            self.field_dragging = !took;
            self.restart_caret();

            match clicks % 3 {
                2 => {
                    let (from, to) = word_around(&value, self.caret);
                    self.field_anchor = Some(from);
                    self.caret = to;
                    self.field_dragging = false;
                }
                0 if clicks > 0 => {
                    self.field_anchor = Some(0);
                    self.caret = value.len();
                    self.field_dragging = false;
                }
                _ => {}
            }
        }
        // A ring after a press only where the reader is going to type: that is the
        // one case the specification says to show it whatever the pointer did.
        let focus_visible = focus.is_some_and(|node| {
            otlyra_dom::form::Control::of(&self.document, node)
                .is_some_and(otlyra_dom::form::Control::is_text_entry)
        });
        let mut changed = self.set_interaction(otlyra_css::state::Interaction {
            active: target,
            focus,
            focus_visible,
            open,
            // Nothing is walked to until the arrows walk to it: a list that has
            // just been opened, or closed, marks no suggestion.
            suggestion: None,
            ..self.interaction
        });
        // After the interaction rather than before it: where the thumb goes is
        // measured against the box laid out for the control, and a press that also
        // moved the focus has not changed that box.
        if let Some(node) = self.sliding {
            changed |= self.slide_to(node, x);
        }
        changed
    }

    /// The pointer came up. Runs the activation behaviour if it came up over what
    /// it went down on.
    pub fn pointer_released(&mut self, x: f64, y: f64) -> bool {
        self.field_dragging = false;
        self.sliding = None;
        let pressed = self.interaction.active;
        let over = self.control_at(x, y);
        let mut changed = self.set_interaction(otlyra_css::state::Interaction {
            active: None,
            ..self.interaction
        });
        if let Some(node) = pressed
            && over == pressed
        {
            changed |= self.activate(node);
        }
        changed
    }

    /// The `href` of the link at `point`, if there is one.
    ///
    /// Walks up the box tree, because the text inside `<a><b>text</b></a>` belongs
    /// to the `<b>` and the link is two boxes above it.
    pub fn link_at(&self, x: f64, y: f64) -> Option<String> {
        let mut current = self.box_at(x, y);
        while let Some(id) = current {
            let node = self.boxes.get(id)?;
            if node.tag.as_ref().is_some_and(|tag| tag.as_ref() == "a")
                && let Some(href) = node.node.and_then(|node| self.attribute(node, "href"))
            {
                return Some(href);
            }
            current = node.parent;
        }
        None
    }

    /// Where a box was drawn on the last frame, if it was.
    pub fn rect_of(&self, id: BoxId) -> Option<otlyra_layout::Rect> {
        self.targets
            .iter()
            .find(|(_, target)| *target == id)
            .map(|(rect, _)| {
                otlyra_layout::Rect::new(
                    rect.x0 as f32,
                    rect.y0 as f32,
                    rect.width() as f32,
                    rect.height() as f32,
                )
            })
    }

    /// The `href` of a box, if it is a link with one.
    pub fn href_of(&self, id: BoxId) -> Option<String> {
        let node = self.boxes.get(id)?;
        if node.tag.as_ref().is_none_or(|tag| tag.as_ref() != "a") {
            return None;
        }
        self.attribute(node.node?, "href")
    }
}
