//! Enforces the crate's first invariant: no `winit` or `wgpu` type may appear in
//! `otlyra-platform`'s public API.
//!
//! The check is textual over the crate's own sources rather than semantic over
//! rustdoc JSON, because rustdoc JSON is nightly-only. Textual is enough: a leak
//! has to be spelled somewhere, and the spelling is what this catches.

use std::path::{Path, PathBuf};

const FORBIDDEN: [&str; 2] = ["winit", "wgpu"];

fn sources() -> Vec<PathBuf> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("readable source directory") {
            let path = entry.expect("readable directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Lines that declare something public. Crude, and deliberately so: it errs toward
/// flagging, and a false positive is a five-second read.
fn public_declarations(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.trim()))
        .filter(|(_, line)| line.starts_with("pub ") || line.starts_with("pub(crate) use "))
}

#[test]
fn no_windowing_or_gpu_types_in_public_declarations() {
    let mut leaks = Vec::new();

    for file in sources() {
        // `event_loop` and `present` are the modules that are *allowed* to name
        // these crates; they are private modules and re-export nothing raw.
        let source = std::fs::read_to_string(&file).expect("readable source file");
        for (line_number, line) in public_declarations(&source) {
            for forbidden in FORBIDDEN {
                if line.contains(&format!("{forbidden}::")) {
                    leaks.push(format!("{}:{line_number}: {line}", file.display()));
                }
            }
        }
    }

    assert!(
        leaks.is_empty(),
        "windowing/gpu types leaked into the public API:\n{}",
        leaks.join("\n")
    );
}

/// The private modules that own the dependency must stay private. If `mod present`
/// or `mod event_loop` ever becomes `pub mod`, every type inside becomes reachable
/// and the test above stops being sufficient.
#[test]
fn the_modules_owning_winit_and_wgpu_stay_private() {
    let lib = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("lib.rs"),
    )
    .expect("readable lib.rs");

    for module in ["event_loop", "present"] {
        assert!(
            lib.contains(&format!("mod {module};")),
            "expected a private `mod {module};` declaration"
        );
        assert!(
            !lib.contains(&format!("pub mod {module};")),
            "`{module}` must stay private; it owns the winit/wgpu dependency"
        );
    }
}
