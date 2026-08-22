## Parent

Part of the plan at `/home/vscode/.claude/plans/dazzling-honking-pixel.md` (Part 2, modal half).

## What to build

Move the video mode / frame rate / resolution / mouse mode dropdowns and
the Save button out of the top bar (added in task #2) and into a proper
settings modal, opened by a new gear icon in the bar:

- Gear icon in the top bar opens a centered modal overlay containing those
  controls.
- Clicking the backdrop closes the modal.
- While the modal is open, the bar's auto-hide-on-outside-click (from task
  #2) is suppressed.
- Save button is disabled by default, and only enabled when a dropdown
  value actually differs from `appliedSettings` (the server-confirmed
  baseline from task #1) — including re-disabling itself if a value is
  flipped back to its original.
- Add one new inline SVG icon: the settings gear.

Full implementation detail is in Part 2 of the plan file above (the
`#settings-modal` sections of the DOM restructure / styling / behavior) —
read it before starting.

## Acceptance criteria

- [ ] Clicking the gear icon opens the settings modal; clicking the
      backdrop closes it.
- [ ] Opening the modal keeps the bar open (auto-hide suppressed) until the
      modal is closed.
- [ ] Save starts disabled, becomes enabled only when a dropdown value
      differs from the server-confirmed settings, and re-disables when
      flipped back to the original value.
- [ ] Save still shows the "Saving…" loading state from task #1 and only
      confirms success once the server responds.
- [ ] `cargo nextest run` still passes (40 tests).
- [ ] `e2e/browser-test.sh` still passes (update its selectors to open the
      bar and the settings modal before reaching these controls).

## Blocked by

- #1 (Settings only apply on Save) — needs `appliedSettings` to exist as
  the dirty-check baseline.
- #2 (Full-screen video with auto-hiding top bar) — needs the bar to exist
  as a place to put the gear icon.
