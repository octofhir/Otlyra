//! `CharacterData`, `Text` and `Comment`: nodes that are strings.
//!
//! Every offset and count here is in UTF-16 code units, because that is how
//! the platform counts a string (DOM §4.10): `"é".length` is one and `"😀"`'s
//! is two, and a page that splits a text node at `str.length / 2` means code
//! units. The arena holds UTF-8, so each method converts on the way in and out.

use otlyra_dom::DocumentMutator;
use otter_macros::js_class;
use otter_runtime::marshal::JsError;

use super::identity::JsNode;
use super::node::{CharacterDataRef, CommentRef, NodeRef, TextRef, illegal_constructor};
use super::tree::{self, Kind};
use super::{with_document, with_document_mut};

/// What a method throws for an offset past the end of the data.
fn index_size(offset: u32, length: usize) -> JsError {
    tree::exception(
        "IndexSizeError",
        format!("offset {offset} is past the end of data {length} code units long"),
    )
}

#[js_class(name = "CharacterData", feature = WEB, extends = NodeRef)]
impl CharacterDataRef {
    #[constructor]
    fn js_new() -> Result<CharacterDataRef, JsError> {
        Err(illegal_constructor("CharacterData"))
    }

    #[getter(name = "data")]
    fn js_data(&self) -> Result<String, JsError> {
        self.units().map(|units| tree::from_utf16(&units))
    }

    #[setter(name = "data")]
    fn js_set_data(&mut self, value: Option<String>) -> Result<(), JsError> {
        self.write(&value.unwrap_or_default())
    }

    #[getter(name = "length")]
    fn js_length(&self) -> Result<u32, JsError> {
        self.units()
            .map(|units| u32::try_from(units.len()).unwrap_or(u32::MAX))
    }

    #[method(name = "substringData", length = 2)]
    fn js_substring_data(&self, offset: u32, count: u32) -> Result<String, JsError> {
        let units = self.units()?;
        let (start, end) = range(offset, count, units.len())?;
        Ok(tree::from_utf16(&units[start..end]))
    }

    #[method(name = "appendData", length = 1)]
    fn js_append_data(&self, data: String) -> Result<(), JsError> {
        let mut units = self.units()?;
        units.extend(data.encode_utf16());
        self.write(&tree::from_utf16(&units))
    }

    #[method(name = "insertData", length = 2)]
    fn js_insert_data(&self, offset: u32, data: String) -> Result<(), JsError> {
        self.js_replace_data(offset, 0, data)
    }

    #[method(name = "deleteData", length = 2)]
    fn js_delete_data(&self, offset: u32, count: u32) -> Result<(), JsError> {
        self.js_replace_data(offset, count, String::new())
    }

    /// DOM §4.10 "replace data", which the four editing methods all are.
    #[method(name = "replaceData", length = 3)]
    fn js_replace_data(&self, offset: u32, count: u32, data: String) -> Result<(), JsError> {
        let mut units = self.units()?;
        let (start, end) = range(offset, count, units.len())?;
        units.splice(start..end, data.encode_utf16());
        self.write(&tree::from_utf16(&units))
    }
}

/// The code units `count` from `offset` covers, clamped at the end the way
/// the specification clamps it — and an error if `offset` is past it.
fn range(offset: u32, count: u32, length: usize) -> Result<(usize, usize), JsError> {
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    if start > length {
        return Err(index_size(offset, length));
    }
    let end = start
        .saturating_add(usize::try_from(count).unwrap_or(usize::MAX))
        .min(length);
    Ok((start, end))
}

impl CharacterDataRef {
    /// The data, in UTF-16.
    fn units(&self) -> Result<Vec<u16>, JsError> {
        let NodeRef { doc, id } = self.node;
        with_document(doc, |document| {
            tree::character_data(document, id).map(tree::utf16)
        })?
        .ok_or_else(tree::gone)
    }

    /// Replace the data.
    fn write(&self, data: &str) -> Result<(), JsError> {
        let NodeRef { doc, id } = self.node;
        with_document_mut(doc, |document| {
            DocumentMutator::new(document).set_character_data(id, data.into());
        })
    }
}

// ---------------------------------------------------------------- Text

#[js_class(name = "Text", feature = WEB, extends = CharacterDataRef)]
impl TextRef {
    #[constructor]
    fn js_new() -> Result<TextRef, JsError> {
        Err(illegal_constructor("Text"))
    }

    /// DOM §4.11 "split a Text node": everything from `offset` moves into a new
    /// text node, which follows this one if this one has a parent.
    #[method(name = "splitText", length = 1)]
    fn js_split_text(&self, offset: u32) -> Result<JsNode, JsError> {
        let NodeRef { doc, id } = self.data.node;
        let units = self.data.units()?;
        let start = usize::try_from(offset).unwrap_or(usize::MAX);
        if start > units.len() {
            return Err(index_size(offset, units.len()));
        }
        let (kept, moved) = units.split_at(start);
        let (kept, moved) = (tree::from_utf16(kept), tree::from_utf16(moved));
        let created = with_document_mut(doc, |document| {
            let next = document.get(id).and_then(|node| node.next_sibling());
            let parent = tree::parent(document, id);
            let mut mutator = DocumentMutator::new(document);
            let created = mutator.create(otlyra_dom::NodeData::Text(moved.as_str().into()));
            if let Some(parent) = parent {
                match next {
                    Some(next) => mutator.insert_before(next, created),
                    None => mutator.append(parent, created),
                };
            }
            mutator.set_character_data(id, kept.as_str().into());
            created
        })?;
        Ok(JsNode::new(doc, created))
    }

    /// The text of this node and every text node next to it, run together
    /// (DOM §4.11 "contiguous Text nodes").
    #[getter(name = "wholeText")]
    fn js_whole_text(&self) -> Result<String, JsError> {
        let NodeRef { doc, id } = self.data.node;
        with_document(doc, |document| {
            let is_text = |node| tree::kind(document, node) == Some(Kind::Text);
            let mut first = id;
            while let Some(previous) = document.get(first).and_then(|node| node.prev_sibling()) {
                if !is_text(previous) {
                    break;
                }
                first = previous;
            }
            let mut out = String::new();
            let mut current = Some(first);
            while let Some(node) = current.filter(|node| is_text(*node)) {
                out.push_str(tree::character_data(document, node).unwrap_or_default());
                current = document.get(node).and_then(|inner| inner.next_sibling());
            }
            out
        })
    }
}

// ------------------------------------------------------------- Comment

#[js_class(name = "Comment", feature = WEB, extends = CharacterDataRef)]
impl CommentRef {
    #[constructor]
    fn js_new() -> Result<CommentRef, JsError> {
        Err(illegal_constructor("Comment"))
    }
}
