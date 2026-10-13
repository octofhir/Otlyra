//! The page's DOM glue, compiled before the browser is.
//!
//! The engine takes an extension's script half as source *and* the bytecode a
//! product build made from that exact source — `otter_runtime::ExtensionJs` —
//! so an isolate links a verified module at startup instead of parsing one. This
//! is that product build for `src/dom/dom.js`: compile it, verify the encoding
//! round-trips, and write the three files `romp!` in `src/dom/node.rs` includes.
//!
//! A failure here stops the build. A browser whose `window` and `document`
//! glue did not compile is not a browser with less JavaScript; it is one whose
//! every page script fails on its first line.

use std::path::Path;

/// Where diagnostics and stack frames say the glue came from.
const SPECIFIER: &str = "<bootstrap:otlyra-dom>";

/// The glue's source, relative to this file.
const SOURCE_PATH: &str = "src/dom/dom.js";

/// The globals the glue defines, in the order it defines them.
///
/// The engine installs these as the extension's names; a global the script
/// creates and this list leaves out is one the isolate does not know it owns.
const DEFINES: &[&str] = &[
    "window",
    "self",
    "top",
    "parent",
    "document",
    "matchMedia",
    "requestAnimationFrame",
    "cancelAnimationFrame",
    "addEventListener",
    "removeEventListener",
    "dispatchEvent",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "__otlyraFlushDeferred",
    "location",
];

/// Bounds on what the glue may grow to, so that a mistake in it is a build
/// error rather than a startup cost nobody measured.
const MAX_SOURCE_BYTES: usize = 1024 * 1024;
const MAX_BYTECODE_BYTES: usize = 4 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={SOURCE_PATH}");

    let out = std::env::var_os("OUT_DIR").ok_or("OUT_DIR is required")?;
    let out = Path::new(&out);
    let text = std::fs::read_to_string(SOURCE_PATH)?;
    if text.len() > MAX_SOURCE_BYTES {
        return Err(format!("{SOURCE_PATH} exceeds {MAX_SOURCE_BYTES} bytes").into());
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
