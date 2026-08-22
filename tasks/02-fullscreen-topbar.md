## Parent

Part of the plan at `/home/vscode/.claude/plans/dazzling-honking-pixel.md` (Part 2, bar mechanics half).

## What to build

Make the video fill 100% of the screen, and move the existing controls
(status, version, video mode, frame rate, resolution, mouse mode, Save,
paste box) off the main layout and into a slim top bar that's hidden by
default:

- A small always-visible arrow-tab handle at the top of the screen.
  Clicking it slides the bar down.
- Clicking anywhere else on the screen slides the bar back up — but only
  while a WebRTC connection is actually up. While disconnected (including
  before the first connection completes), the bar stays forced open.
- For this task, the bar's *contents* stay as they are today (all controls
  inline in the bar, not yet split into a modal/flyout — that's tasks #3
  and #4). The point of this task is the full-screen video + slide-down
  mechanic + connection-aware auto-hide, not the modal/flyout split.
- Add one new inline SVG icon: the down-chevron for the handle.

Full implementation detail (exact structure, CSS approach, line numbers) is
in Part 2 of the plan file above (the DOM restructure / styling / behavior
sections that describe `#topbar-handle` and `#topbar`) — read it before
starting, but note this task only covers the bar mechanics, not the
settings-modal or paste-flyout split described there (those are separate
tasks).

## Acceptance criteria

- [ ] The video fills the entire viewport.
- [ ] The bar is hidden by default, with only the arrow handle visible.
- [ ] Clicking the handle slides the bar down; clicking elsewhere on the
      screen slides it back up.
- [ ] While disconnected (including on initial page load before connecting),
      the bar stays open and does not auto-hide.
- [ ] All existing controls (status, version, video mode, frame rate,
      resolution, mouse mode, Save, paste box) still work exactly as before,
      just relocated into the bar.
- [ ] `cargo nextest run` still passes (40 tests).
- [ ] `e2e/browser-test.sh` still passes (update its selectors if it needs
      to open the bar first to reach the controls).

## Blocked by

None — can start immediately. (Note: touches the same files as task #1 —
coordinate so it starts only once #1 has landed, to avoid merge conflicts.)
