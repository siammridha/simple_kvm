## Parent

Part of the plan at `/home/vscode/.claude/plans/dazzling-honking-pixel.md` (Part 2, paste half).

## What to build

Move the paste textarea + Send button out of the top bar (added in task
#2) and into a small flyout panel, opened by a new paste icon in the bar:

- Paste icon in the top bar toggles a small flyout panel (not a full modal
  — a slimmer panel dropping from the bar) containing the paste textarea
  and Send button.
- Clicking outside the flyout (but not necessarily elsewhere on the whole
  screen) closes just the flyout.
- While the flyout is open, the bar's auto-hide-on-outside-click (from
  task #2) is suppressed, same as the settings modal.
- Add one new inline SVG icon: the clipboard/paste icon.

Full implementation detail is in Part 2 of the plan file above (the
`#paste-panel` sections of the DOM restructure / styling / behavior) — read
it before starting.

## Acceptance criteria

- [ ] Clicking the paste icon opens the flyout without opening the
      settings modal.
- [ ] Opening the flyout keeps the bar open (auto-hide suppressed) until
      the flyout is closed.
- [ ] Pasting text and clicking Send still works exactly as before.
- [ ] `cargo nextest run` still passes (40 tests).
- [ ] `e2e/browser-test.sh` still passes (update its selectors if it
      exercises paste).

## Blocked by

- #2 (Full-screen video with auto-hiding top bar) — needs the bar to exist
  as a place to put the paste icon.
