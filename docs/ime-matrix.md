# IME test matrix

Input methods are the one part of text input that cannot be tested automatically:
the interesting behaviour lives in the OS input method, and reproducing it means
driving real key events into a real window. So this is a manual matrix, run and
recorded rather than asserted.

It starts now, at M7, and not at M14 with events. Every winit 0.30.x patch since
0.30.7 fixes IME somewhere, and `main` carries unreleased macOS fixes for IME
locking on and for a panic on emoji preedit. A regression found six milestones
after it lands is a regression nobody can bisect.

## How to run it

1. `cargo run` and click the address bar.
2. For each input method below, type the sample and note what happens at each
   step.

Record the result in the table with the date and the winit version. A row that
has not been run this sprint is a row that says nothing.

## What to check, per input method

| # | Step | Expected |
|---|---|---|
| 1 | Type the first syllable | Preedit text appears, underlined, at the caret |
| 2 | Keep typing | Preedit grows; the surrounding text does not move |
| 3 | Candidate window | Opens next to the caret, not at the window origin |
| 4 | Select a candidate | Preedit is replaced, commit lands in the field |
| 5 | Escape mid-preedit | Preedit disappears; nothing is committed |
| 6 | Backspace mid-preedit | Deletes within the preedit, not the committed text |
| 7 | Click away mid-preedit | Preedit commits or clears; nothing is left drawn |
| 8 | Emoji picker (`Cmd+Ctrl+Space`) | Inserts, and does not panic |

## Matrix

| Input method | Sample | Date | winit | Result |
|---|---|---|---|---|
| macOS Kotoeri (Japanese) | にほんご | — | — | **not yet run** |
| macOS Pinyin (Simplified) | 中文 | — | — | **not yet run** |
| macOS ABC (dead keys) | `é` via Option+E, E | — | — | **not yet run** |
| macOS emoji picker | 🙂 | — | — | **not yet run** |
| Linux ibus (from M11) | — | — | — | not applicable yet |
| Windows MS-IME (from M11) | — | — | — | not applicable yet |

## Known state

The address bar takes `TextInput` events, which are what the platform reports
*after* the input method has had its say — so committed text already works for
every method that commits through the normal path. What is **not** implemented is
preedit: there is no underlined in-progress text and no candidate-window
positioning, because winit's IME API is one this milestone did not wire up. Until
it is, steps 1 through 7 are expected to show the commit only, and the matrix
records that rather than pretending otherwise.
