//! What the reader has selected in the page's text.
//!
//! A place in the text rather than a rectangle on the screen, so that it means
//! the same words once the page has been laid out again. Its own module because it
//! is counted in the page's runs where a field's selection is counted in what a
//! control holds, and a focused field is the one case in which these answer for
//! the field instead.

use otlyra_layout::FragmentTree;

use super::PageScene;

impl PageScene {
    /// Begin a selection at a point in the window, and answer whether there is
    /// any text there to select.
    ///
    /// The page coordinates are the window's plus wherever the reader has scrolled
    /// to, which is the one conversion between what a pointer reports and what a
    /// page is laid out in.
    pub fn select_from(&mut self, x: f32, y: f32, top: f32) -> bool {
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };
        let point = (x, y - top + self.scroll);
        match otlyra_layout::selection::position_at(tree, point.0, point.1) {
            Some(position) => {
                self.set_selection(Some(otlyra_layout::Selection::at(position)));
                true
            }
            None => {
                self.set_selection(None);
                false
            }
        }
    }

    /// Take the selection to a point: what a drag does after the press.
    pub fn select_to(&mut self, x: f32, y: f32, top: f32) {
        let Some(mut selection) = self.selection else {
            return;
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return;
        };
        let point = (x, y - top + self.scroll);
        if let Some(position) = otlyra_layout::selection::position_at(tree, point.0, point.1) {
            selection.focus = position;
            self.set_selection(Some(selection));
        }
    }

    /// Take the word under a point, which is what a second click means.
    pub fn select_word_at(&mut self, x: f32, y: f32, top: f32) -> bool {
        self.select_expanded(x, y, top, otlyra_layout::selection::word_at)
    }

    /// And the block it is in, which is what a third means.
    pub fn select_paragraph_at(&mut self, x: f32, y: f32, top: f32) -> bool {
        self.select_expanded(x, y, top, otlyra_layout::selection::paragraph_at)
    }

    fn select_expanded(
        &mut self,
        x: f32,
        y: f32,
        top: f32,
        expand: fn(&FragmentTree, otlyra_layout::TextPosition) -> otlyra_layout::Selection,
    ) -> bool {
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };
        let point = (x, y - top + self.scroll);
        match otlyra_layout::selection::position_at(tree, point.0, point.1) {
            Some(position) => {
                let selection = expand(tree, position);
                self.set_selection(Some(selection));
                !selection.is_empty()
            }
            None => {
                self.set_selection(None);
                false
            }
        }
    }

    /// Select the whole page.
    pub fn select_all(&mut self) -> bool {
        // A field with the focus is what "everything" means: selecting the page
        // behind it is not what a reader who is typing into one asked for.
        if let Some(node) = self.focused_field() {
            let value_len = self.field_text(node).len();
            self.field_anchor = Some(0);
            self.caret = value_len;
            self.restart_caret();
            return true;
        }
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };
        match otlyra_layout::selection::all(tree) {
            Some(selection) if !selection.is_empty() => {
                self.set_selection(Some(selection));
                true
            }
            _ => false,
        }
    }

    /// Move the far end of the selection one step, and answer whether anything
    /// moved.
    ///
    /// Extending keeps where it started and moves where it is going; not
    /// extending takes both ends to the same place, which is a caret rather than
    /// a selection and is what an arrow key means with nothing held down.
    pub fn move_selection(&mut self, motion: otlyra_layout::Motion, extend: bool) -> bool {
        let Some(selection) = self.selection else {
            return false;
        };
        let Some((_, tree)) = self.layout.as_ref() else {
            return false;
        };
        let focus = otlyra_layout::selection::moved(tree, selection.focus, motion);
        let moved = otlyra_layout::Selection {
            anchor: if extend { selection.anchor } else { focus },
            focus,
        };
        if moved == selection {
            return false;
        }
        self.set_selection(Some(moved));
        true
    }

    /// Drop the selection, which is what a press somewhere else means.
    pub fn clear_selection(&mut self) {
        self.set_selection(None);
    }

    /// What is selected, as text, or `None` when nothing is.
    pub fn selected_text(&self) -> Option<String> {
        if let Some((node, from, to)) = self.field_selection() {
            return Some(self.field_text(node)[from..to].to_owned());
        }
        let selection = self.selection.filter(|selection| !selection.is_empty())?;
        let (_, tree) = self.layout.as_ref()?;
        let text = otlyra_layout::selection::text(tree, selection);
        (!text.is_empty()).then_some(text)
    }

    /// Whether anything is selected.
    pub fn has_selection(&self) -> bool {
        if self.field_selection().is_some() {
            return true;
        }
        self.selection
            .is_some_and(|selection| !selection.is_empty())
    }

    pub(super) fn set_selection(&mut self, selection: Option<otlyra_layout::Selection>) {
        if self.selection == selection {
            return;
        }
        self.selection = selection;
        self.damage.add(otlyra_layout::Damage::PAINT);
    }
}
