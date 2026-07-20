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

Two categories are skipped rather than run, and the runner counts them:

- `#document-fragment` cases, until fragment parsing exists (it needs
  `innerHTML`, which needs script).
- `#script-off` cases. We parse with scripting enabled, which is what a browser
  does; the same tests appear in their scripting-on form.
