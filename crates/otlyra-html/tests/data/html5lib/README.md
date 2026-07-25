# html5lib tree-construction data

Vendored, not fetched: a conformance suite that changes underneath the build is
not a conformance suite.

- Source: <https://github.com/html5lib/html5lib-tests>, commit
  `9329e64694e7835d0dcff9811e22856ef6ad16f9`, directory `tree-construction/`.
  That commit is the last one before the suite moved to web-platform-tests; its
  successor, `224991e`, deletes the directory.
- Licence: MIT, in `LICENSE` beside this file.
- Format: documented in the suite's own `tree-construction/README.md`.

`expectations.txt` records cases we knowingly fail. It is checked in both
directions — an unlisted failure and a listed pass both fail the build.

The whole `tree-construction/` directory is imported — 57 files, 1787 cases — not
a subset. A conformance suite you have chosen the easy parts of measures nothing.

One category is skipped rather than run, and the runner counts it: `#script-off`
cases, because we parse with scripting enabled, which is what a browser does. The
same documents appear in their scripting-on form.
