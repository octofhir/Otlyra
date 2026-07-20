//! Printing a box tree, for `--dump-boxes` and for snapshots.

use std::fmt::Write as _;

use otlyra_css::Display;

use crate::box_tree::{BoxId, BoxKind, BoxTree};

/// Serialize the whole tree.
pub fn serialize(tree: &BoxTree) -> String {
    let mut out = String::new();
    write_box(tree, tree.root(), 0, &mut out);
    out
}

fn write_box(tree: &BoxTree, id: BoxId, depth: usize, out: &mut String) {
    let Some(node) = tree.get(id) else { return };
    let indent = "  ".repeat(depth);

    match &node.kind {
        BoxKind::Text(text) => {
            let _ = writeln!(out, "{indent}TEXT {:?}", text.to_string());
        }
        kind => {
            let name = match kind {
                BoxKind::Block => "BLOCK",
                BoxKind::Inline => "INLINE",
                BoxKind::Text(_) => unreachable!("handled above"),
            };
            let tag = match (&node.tag, node.parent) {
                (Some(tag), _) => tag.to_string(),
                // The initial containing block: the viewport's own box, which no
                // element generated and nothing was fixed up to produce.
                (None, None) => "(initial containing block)".to_owned(),
                (None, Some(_)) => "(anonymous)".to_owned(),
            };
            let _ = write!(out, "{indent}{name} {tag}");
            if node.style.display == Display::Block && node.style.font_weight >= 700 {
                let _ = write!(out, " bold");
            }
            let _ = writeln!(out, " font={}px", node.style.font_size);
        }
    }

    for &child in &node.children {
        write_box(tree, child, depth + 1, out);
    }
}

/// Serialize a fragment tree: geometry first, because geometry is what layout is
/// judged on.
pub fn serialize_fragments(tree: &crate::fragment::FragmentTree) -> String {
    let mut out = String::new();
    write_fragment(&tree.root, 0, &mut out);
    out
}

fn write_fragment(fragment: &crate::fragment::Fragment, depth: usize, out: &mut String) {
    use crate::fragment::FragmentKind;

    let indent = "  ".repeat(depth);
    let rect = fragment.rect;
    let kind = match &fragment.kind {
        FragmentKind::Box => "BOX".to_owned(),
        FragmentKind::Line => "LINE".to_owned(),
        FragmentKind::Text(run) => format!("TEXT {} glyphs", run.glyphs.len()),
    };
    let _ = writeln!(
        out,
        "{indent}{kind} at ({:.1}, {:.1}) size {:.1}x{:.1}",
        rect.x, rect.y, rect.width, rect.height
    );

    for child in &fragment.children {
        write_fragment(child, depth + 1, out);
    }
}
