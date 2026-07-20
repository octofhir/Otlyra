//! The html5lib tree-construction suite, run against our DOM.
//!
//! The data files are vendored under `tests/data/html5lib` — see the README there
//! for where they came from. The expectations are the suite's own, so the only
//! thing this file contributes is reading the format and comparing strings.
//!
//! Failures are held in `expectations.txt` and checked **both ways**: an unexpected
//! failure fails the build, and so does a test that starts passing while still
//! listed. A one-way ledger rots into a list of things nobody has looked at since.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use otlyra_dom::dump;

/// One `#data` block.
#[derive(Debug)]
struct TestCase {
    file: String,
    index: usize,
    data: String,
    expected: String,
    /// Fragment-parsing cases name a context element. We do not parse fragments
    /// yet, so those are skipped rather than failed.
    fragment_context: Option<String>,
    /// Cases that only apply with scripting off. Ours parses with scripting on,
    /// which is what a browser does.
    script_off: bool,
}

fn parse_dat(file: &str, contents: &str) -> Vec<TestCase> {
    const HEADERS: [&str; 7] = [
        "#data",
        "#errors",
        "#new-errors",
        "#document-fragment",
        "#script-off",
        "#script-on",
        "#document",
    ];

    let mut cases = Vec::new();
    let mut current: Option<TestCase> = None;
    let mut section = "";

    for line in contents.split('\n') {
        if HEADERS.contains(&line) {
            if line == "#data" {
                if let Some(case) = current.take() {
                    cases.push(finish(case));
                }
                current = Some(TestCase {
                    file: file.to_owned(),
                    index: cases.len(),
                    data: String::new(),
                    expected: String::new(),
                    fragment_context: None,
                    script_off: false,
                });
            }
            if line == "#script-off"
                && let Some(case) = current.as_mut()
            {
                case.script_off = true;
            }
            section = line;
            continue;
        }

        let Some(case) = current.as_mut() else {
            continue;
        };
        match section {
            "#data" => {
                let _ = writeln!(case.data, "{line}");
            }
            "#document" => {
                let _ = writeln!(case.expected, "{line}");
            }
            "#document-fragment" => case.fragment_context = Some(line.trim().to_owned()),
            _ => {}
        }
    }

    if let Some(case) = current.take() {
        cases.push(finish(case));
    }
    cases
}

/// Drop the trailing newline the format adds to `#data`, and the blank line that
/// separates one test from the next.
fn finish(mut case: TestCase) -> TestCase {
    if case.data.ends_with('\n') {
        case.data.pop();
    }
    while case.expected.ends_with("\n\n") {
        case.expected.pop();
    }
    case
}

fn data_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/html5lib"))
}

/// The identifier a failure is recorded under.
fn name(case: &TestCase) -> String {
    format!("{}:{}", case.file, case.index)
}

fn expectations() -> BTreeSet<String> {
    let path = data_dir().join("expectations.txt");
    let contents = std::fs::read_to_string(path).unwrap_or_default();
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split_whitespace().next().unwrap_or(line).to_owned())
        .collect()
}

#[test]
fn tree_construction() {
    let known_failures = expectations();
    let mut failed = Vec::new();
    let mut passed = 0usize;
    let mut skipped = 0usize;
    let mut report = String::new();

    let mut files: Vec<_> = std::fs::read_dir(data_dir())
        .expect("test data directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "dat"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .dat files found");

    for path in files {
        let file = path
            .file_stem()
            .expect("file stem")
            .to_string_lossy()
            .into_owned();
        let contents = std::fs::read_to_string(&path).expect("readable test data");

        for case in parse_dat(&file, &contents) {
            if case.fragment_context.is_some() || case.script_off {
                skipped += 1;
                continue;
            }

            assert!(
                !case.expected.is_empty(),
                "{}: empty expectation means the .dat file was misread",
                name(&case)
            );

            let parsed = otlyra_html::parse(case.data.as_bytes(), Some("utf-8"));
            let actual = dump::serialize(&parsed.document);
            let id = name(&case);

            match (actual == case.expected, known_failures.contains(&id)) {
                (true, false) => passed += 1,
                (false, true) => passed += 0,
                (true, true) => {
                    let _ = writeln!(
                        report,
                        "{id} passes but is listed as a known failure; remove it from expectations.txt"
                    );
                    failed.push(id);
                }
                (false, false) => {
                    let _ = writeln!(
                        report,
                        "\n{id}\n  input:    {:?}\n  expected:\n{}  actual:\n{}",
                        case.data, case.expected, actual
                    );
                    failed.push(id);
                }
            }
        }
    }

    assert!(
        failed.is_empty(),
        "{} of {} tree-construction cases disagree ({skipped} skipped)\n{report}\nfailing ids:\n{}",
        failed.len(),
        passed + failed.len(),
        failed.join("\n")
    );

    eprintln!("tree construction: {passed} passed, {skipped} skipped");
}
