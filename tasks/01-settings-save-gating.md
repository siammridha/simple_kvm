## Parent

Part of the plan at `/home/vscode/.claude/plans/dazzling-honking-pixel.md` (Part 1).

## What to build

Two settings dropdowns currently have a live effect before Save is clicked,
even though the project's rule is "a dropdown does nothing until Save":

- Mouse mode: the click/wheel handlers read the dropdown's live value
  directly, so flipping it changes which input message type is sent on the
  very next click/scroll, before Save.
- Video mode: a `change` listener on the dropdown immediately swaps which
  element is visible (`<video>` vs `<canvas>`), so it can show the wrong
  element for the actually-running stream.

Fix both by introducing a single `appliedSettings` object in `app.js` that
holds whatever the server has actually confirmed (`video_mode`, `width`,
`height`, `fps`, `mouse_mode`). It's only ever written from the server's
`settings` push (on connect, and after a Save is applied) — never
optimistically on click. The two live-read call sites read from
`appliedSettings` instead of the dropdown's DOM value.

Also add a loading state to the Save button: disable it and show "Saving…"
right after it's clicked, and only clear that (showing "Saved") once the
server's confirmation `settings` message actually arrives. If the control
channel closes while a save is pending, clear the loading state without
claiming success.

Full implementation detail (exact line numbers, code snippets) is in Part 1
of the plan file above — read it before starting.

## Acceptance criteria

- [ ] Changing the mouse-mode dropdown without clicking Save does not
      change which message type is sent on the next click/scroll.
- [ ] Changing the video-mode dropdown without clicking Save does not
      change which element (`<video>`/`<canvas>`) is shown.
- [ ] Clicking Save shows a visible loading state, and the new values only
      take effect once the server's confirmation message arrives (not
      immediately on click).
- [ ] `cargo nextest run` still passes (40 tests).
- [ ] `e2e/browser-test.sh` still passes.

## Blocked by

None — can start immediately.
