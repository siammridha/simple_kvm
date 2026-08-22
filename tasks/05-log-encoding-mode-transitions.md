## Parent

Part of the plan at `/home/vscode/.claude/plans/dazzling-honking-pixel.md` (Part 3).

## What to build

Log a line whenever the actual video encoding switches between H.264 and
MJPEG (in either direction) — e.g. "video encoding stopped" for the old
mode followed by "video encoding started" for the new mode, at the exact
point the capture loop restarts with a changed `video_mode`. This should
only fire on an actual mode change (not on every settings save, e.g. a
resolution/fps-only change stays silent), and only once capture has
actually started at least once (no spurious log on startup).

Full implementation detail (exact location in `CaptureManager::run`, the
comparison logic) is in Part 3 of the plan file above — read it before
starting.

## Acceptance criteria

- [ ] Saving a video-mode change from mjpeg to h264 logs a
      "video encoding stopped" (mode=Mjpeg) line followed by a
      "video encoding started" (mode=H264) line.
- [ ] Saving a change to the same video mode (e.g. just fps) does not log
      either line.
- [ ] Switching back to mjpeg logs the reverse pair of lines.
- [ ] `cargo nextest run` still passes (40 tests).

## Blocked by

None — can start immediately. Touches only `src/capture/mod.rs`, no
overlap with the frontend tasks (#1-#4).
