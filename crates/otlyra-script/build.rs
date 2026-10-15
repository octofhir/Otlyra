//! The page's DOM glue, compiled before the browser is.
//!
//! The engine takes an extension's script half as source *and* the bytecode a
//! product build made from that exact source — `otter_runtime::ExtensionJs` —
//! so an isolate links a verified module at startup instead of parsing one. This
//! is that product build for the files in `src/dom/js/`: join them, in order,
//! inside one function after the generated interface table, compile the result,
//! verify the encoding round-trips, and write the three files `romp!` in
//! `src/dom/mod.rs` includes.
//!
//! A failure here stops the build. A browser whose `window` and `document`
//! glue did not compile is not a browser with less JavaScript; it is one whose
//! every page script fails on its first line.

use std::fmt::Write as _;
use std::path::Path;

#[path = "src/dom/interface_tree.rs"]
mod interface_tree;

/// Where diagnostics and stack frames say the glue came from.
const SPECIFIER: &str = "<bootstrap:otlyra-dom>";

/// The glue's files, in the order they run. Each may use what the ones before
/// it declared: they share one function scope.
const SOURCES: &[&str] = &[
    "src/dom/js/prelude.js",
    "src/dom/js/window.js",
    "src/dom/js/interfaces.js",
    "src/dom/js/collections.js",
    "src/dom/js/tree.js",
    "src/dom/js/events.js",
    "src/dom/js/reflect.js",
    "src/dom/js/style.js",
    "src/dom/js/location.js",
    "src/dom/js/platform.js",
];

/// The globals the glue defines besides the interface tree, which is added to
/// these from [`interface_tree::INTERFACE_TREE`].
const DEFINES: &[&str] = &[
    "window",
    "self",
    "top",
    "parent",
    "frames",
    "opener",
    "frameElement",
    "closed",
    "name",
    "innerWidth",
    "innerHeight",
    "devicePixelRatio",
    "onresize",
    "Window",
    "document",
    "Text",
    "Comment",
    "DocumentFragment",
    "NodeList",
    "HTMLCollection",
    "DOMTokenList",
    "Attr",
    "NamedNodeMap",
    "DOMStringMap",
    "CSSStyleDeclaration",
    "Image",
    "Audio",
    "Option",
    "addEventListener",
    "removeEventListener",
    "dispatchEvent",
    "matchMedia",
    "requestAnimationFrame",
    "cancelAnimationFrame",
    "__otlyraFlushDeferred",
    "__otlyraRunFrame",
    "__otlyraFramesPending",
    "location",
    "CustomEvent",
    "IntersectionObserver",
    "MutationObserver",
    "ResizeObserver",
];

/// Bounds on what the glue may grow to, so that a mistake in it is a build
/// error rather than a startup cost nobody measured.
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_BYTECODE_BYTES: usize = 4 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/dom/interface_tree.rs");
    for source in SOURCES {
        println!("cargo:rerun-if-changed={source}");
    }

    let out = std::env::var_os("OUT_DIR").ok_or("OUT_DIR is required")?;
    let out = Path::new(&out);

    // One function around every file, so they share a scope and nothing they
    // declare is a global unless it is defined as one.
    let mut text = String::from("(function () {\n'use strict';\n\nconst INTERFACE_TREE = [\n");
    for (name, parent) in interface_tree::INTERFACE_TREE {
        writeln!(text, "  [{name:?}, {parent:?}],")?;
    }
    text.push_str("];\n");
    for source in SOURCES {
        writeln!(text, "\n// ---- {source}\n")?;
        text.push_str(&std::fs::read_to_string(source)?);
    }
    text.push_str("\n})();\n");
    if text.len() > MAX_SOURCE_BYTES {
        return Err(format!("the DOM glue exceeds {MAX_SOURCE_BYTES} bytes").into());
    }

    let mut compiled = otter_compiler::compile_script_source_to_module(
        &text,
        otter_syntax::SourceKind::JavaScript,
        SPECIFIER,
    )?;
    // The glue iterates with the language's own iterators, whatever a page
    // later does to `Array.prototype[Symbol.iterator]`.
    compiled.bytecode.mark_primordial_iteration();
    let encoded =
        otter_bytecode::binary::encode_module_bounded(&compiled.bytecode, MAX_BYTECODE_BYTES)?;
    // The engine decodes this without verifying it again, so it is verified
    // here, once, where a failure is ours to see.
    otter_bytecode::binary::decode_module(&encoded)?;

    let names = DEFINES
        .iter()
        .copied()
        .chain(interface_tree::INTERFACE_TREE.iter().map(|(name, _)| *name))
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let descriptor = format!(
        r#"::otter_runtime::ExtensionJs {{
    source: include_str!(concat!(env!("OUT_DIR"), "/dom-bootstrap.js")),
    bytecode: include_bytes!(concat!(env!("OUT_DIR"), "/dom-bootstrap.bytecode")),
    defines: &[{names}],
}}
"#
    );
    std::fs::write(out.join("dom-bootstrap.js"), text)?;
    std::fs::write(out.join("dom-bootstrap.bytecode"), encoded)?;
    std::fs::write(out.join("dom-bootstrap.rs"), descriptor)?;
    Ok(())
}
