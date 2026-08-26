# simple_kvm

A small KVM-over-USB tool for a Dell Wyse 3040 running Alpine Linux. It sits
next to a target computer, reads its screen through a capture card, and
sends keyboard/mouse input to it through a CH9329 HID adapter — controlled
entirely from a web page.

## Hardware

- **CH9329 + CH340 UART/TTL Serial Port to USB Connecting Wire** - the
  CH340 side (a plain USB-serial adapter with TX/RX/GND leads, wired to the
  CH9329 board's UART pins) plugs into the Wyse 3040 and shows up as
  `/dev/ttyUSB*`. The CH9329's own USB connector plugs into the **target**
  computer, where it enumerates as a USB keyboard/mouse. Getting this
  backwards (CH9329's USB side into the Wyse 3040) makes the Wyse 3040
  itself show up as a keyboard/mouse instead of controlling anything - if
  `/dev/ttyUSB*` never appears, check which connector is plugged in where.
- **1080P Capture Card, USB 3.0 to HDMI** - plugs into the target
  computer's HDMI output and the Wyse 3040's USB port, and shows up as
  `/dev/video*`.

Both devices are recognized by the Linux kernel automatically, no drivers
to install.

## What it does

Run the binary on the Wyse 3040 and it serves a page at `http://<device
ip>:3000` with:

- **Live video**, streamed over WebRTC as a real H.264 video track,
  software-encoded and decoded natively by the browser into a `<video>`
  element. The Wyse 3040's Atom CPU is genuinely weak for real-time
  software video encoding - expect this to be choppy at higher
  resolutions/frame rates. The encoder inserts a keyframe every 60 frames
  so a browser that (re)connects mid-stream has something to start
  decoding from, and also produces one immediately whenever a connected
  browser's own decoder asks for one (standard WebRTC keyframe-request
  feedback) - without either, only the very first frame of a capture
  session would ever be a keyframe, and joining any later would leave the
  video stuck.
- **Resolution dropdown**, populated from whatever the capture card
  actually reports supporting (queried at startup) - not a hardcoded list.
- **Frame rate dropdown**, populated from whatever the capture card
  actually reports supporting for the currently-selected resolution
  (queried at startup, like the resolution dropdown) - not a hardcoded
  list, since real hardware supports different rates at different
  resolutions. Switching the resolution dropdown (even before clicking
  Save) repopulates this list to match, so it's never possible to pick a
  rate the card can't actually do at that resolution. Sent to the capture
  card via V4L2's frame-interval negotiation; the card is still free to
  negotiate a different rate than requested (a mismatch is logged, not
  shown on the page), but picking through the page only ever offers rates
  the card itself reported.
- **Bitrate dropdown**, a fixed set of steps from 500 Kbps up to 5 Mbps.
  Unlike the resolution/frame rate dropdowns, this isn't queried from
  hardware. The full range has been confirmed clean in manual testing -
  5 Mbps at both 1080p@10fps and 720p@25fps - see
  `docs/gpu-encoding-investigation.md`. The server enforces a hard 5 Mbps
  ceiling itself (clamping anything higher) regardless of what the dropdown
  offers, since the settings message could in principle be hand-crafted
  with any value.
- **Mouse movement, clicks, and scroll wheel**, absolute or relative mode,
  switched via **Save settings**. Absolute mode positions the cursor
  exactly where you point in the video; on the CH9329 hardware this repo
  was built against, clicks and scroll wheel only work through its
  *relative* HID report, so absolute mode sends position via the absolute
  report and clicks/scroll via a zero-motion relative report - invisible
  from the browser, just how `ch9329::writer` talks to this chip. Mouse
  movement used to be sent on every native `mousemove` event, which was
  fast enough to crash-reboot the Wyse 3040 (a power/brownout issue), so
  it's now throttled to send at most once per video frame - matching
  whatever fps is currently configured, and re-sampling automatically if
  fps changes.
- **A mouse on/off toggle** - the cursor icon next to the gear icon turns
  all mouse forwarding (movement, clicks, scroll) on or off. It's a local,
  browser-only switch (nothing is saved or sent to the server); keyboard
  input keeps working either way. Useful for typing without stray clicks
  landing on the target.
- **A paste box** - pasting into it sends the text to the target right away
  as simulated keystrokes, then clears the box (US QWERTY only; there's no
  OS-level clipboard access over a HID-only link).
- **An auto-hiding controls bar** - the bar with resolution/frame rate/mouse
  mode/paste controls and status tucks itself away 5 seconds after it's
  opened while a browser is connected and nothing else is going on, and
  reopens on tap/click, or on its own if the connection drops. Hovering
  over the bar pauses the countdown so it won't close out from under the
  pointer; it resumes once the pointer leaves.
- **No login.** Anyone who can reach port 3000 has full control. This is
  meant for a trusted LAN, not the open internet.

Video capture and encoding only run while at least one browser is
connected - with nobody watching, no CPU/power is spent on capture at all.
The moment a browser connects, capture starts at whatever resolution/fps
the settings say; the moment the last browser disconnects, it stops. A
second browser connecting while one is already active doesn't restart
anything. Every actual start/stop of a capture pass is logged as `video
encoding started`/`video encoding stopped`.

The server runs fine with no capture card or CH9329 attached - the page
still loads, the frame rate/resolution dropdowns are disabled and
reflect "no video device," the mouse mode dropdown is disabled too, and
keyboard/mouse input is silently dropped instead of the service failing
to start. Useful
for development without the hardware plugged in. This also covers either
device disconnecting after the service has already started, and both
recover on their own once reconnected, no restart needed: the CH9329
silently drops input while it's gone and reconnects as soon as it's
plugged back in (noticed immediately, not just on the next key or click -
see below); the capture card pauses video the same way and resumes
streaming once it's replugged. The page picks this up live, too - the
resolution dropdown and "no video device" status update immediately on an
already-open tab, and the relevant dropdowns enable/disable to match, not
just on the next load or reconnect.

Both the capture card's and the CH9329's reconnects are noticed
immediately: the server listens directly on the Linux kernel's own
device-change broadcast (`NETLINK_KOBJECT_UEVENT`, the same channel udev
listens on), rather than going through udev itself - the Wyse 3040 image
this runs on has no udev daemon (it uses the simpler `mdev` instead), so
udev's own notifications never fire there. Listening straight to the
kernel works regardless of what (if anything) is managing devices. For the
capture card, a slow, infrequent poll still runs alongside it as a safety
net in case that listener can't be opened; the CH9329 side has no such
fallback, so if its listener fails to open, its reconnects are only
noticed on the next real keystroke or click instead of immediately.

**Dropdown changes only take effect when you click Save settings.**
Changing frame rate, resolution, bitrate, or mouse mode does nothing on its own -
picking a new value just moves the dropdown. Clicking **Save
settings** sends one message over the WebRTC control channel that both
applies the new settings live and writes them to the settings file on
disk, together, in one step. If you reload the page (or open a second
tab) without saving, the dropdowns show whatever the server is actually
using right now, not your unsaved picks - and if the service restarts
(e.g. after a reboot) without a save having happened first, it comes back
up with whatever was last saved, or the capture card's own defaults if
nothing ever was.

Save only includes the settings for hardware that's actually connected:
resolution/frame rate/bitrate are sent only if the capture card is
plugged in, and mouse mode only if the CH9329 is - there's nothing
meaningful to save for a device that isn't there. If only one is
connected, Save updates just that half and leaves the other as it was.

### No TLS to set up

The page is served over plain HTTP - no certificate to provide, no
warning to click through. The video/input connection (WebRTC) still gets
encrypted end to end: WebRTC's DTLS-SRTP is mandatory and automatic,
generating a fresh self-signed certificate per connection that's verified
via a fingerprint exchanged during signaling, not checked against any
certificate authority. There's nothing for an operator to provide, rotate,
or configure. See [docs/transport-comparison.md](docs/transport-comparison.md)
for why this replaced an earlier WebTransport-based design.

## How it's built

- `.github/workflows/build.yml` - on a pushed version tag (`v*`), builds
  the release binary in a native x86_64 Alpine container (matching the
  Wyse 3040 exactly, so it's a normal build, not a cross-compile) and
  publishes it as a GitHub Release.
- `deploy/install.sh` - run on the device; downloads the latest release
  binary from GitHub and sets it up as an OpenRC service that starts on
  boot.

Building needs a few extra Alpine packages beyond a bare Rust toolchain:
`clang-dev` and `linux-headers` (the `v4l` crate generates V4L2 bindings
with `bindgen` at build time) and `libva-dev` (the GPU H.264 encoder's
`bindgen`-based build script, and linking against libva). Both the
devcontainer and the release workflow already install these.

**Fast local iteration, without a tag/release for every change:** the
devcontainer can cross-compile a debug binary directly, for testing
against the real device without waiting on CI:

```sh
apk add --no-cache zig cargo-zigbuild
rustup target add x86_64-unknown-linux-musl
RUSTC_BOOTSTRAP=1 cargo zigbuild --release --target x86_64-unknown-linux-musl
```

`.cargo/config.toml`'s `[unstable]`/`[host]` section (needing
`RUSTC_BOOTSTRAP=1` since it's not stabilized) is what makes this work:
without it, `bindgen`'s own build script fails to cross-compile with a
"dynamic loading not supported" error, since Cargo doesn't apply
`[target]` rustflags to host-compiled build scripts. Copy the resulting
`target/x86_64-unknown-linux-musl/release/simple_kvm` to the device (e.g.
via `scp` to `/usr/local/bin/simple_kvm.new`, then `chmod 755` and `mv`
over the running binary - the rename avoids "Text file busy") and `rc-service
simple_kvm restart`.

`test-on-device.sh`, in the repo root, automates all of the above (build,
copy, install, restart) over password-authenticated SSH:

```sh
./test-on-device.sh
```

This file holds the device's IP and root password directly, so it's listed
in `.gitignore` and never committed. It won't exist in a fresh clone - copy
it back in (or recreate it) if it's missing.

## Target platform

- **Device:** Dell Wyse 3040 (Intel Atom x5-Z8350, x86_64)
- **OS:** Alpine Linux

## Installing on the device

Run this on the Wyse 3040 itself, as root:

```sh
wget -qO- https://raw.githubusercontent.com/siammridha/simple_kvm/main/deploy/install.sh | sh
```

This downloads the latest release binary and sets it up as an OpenRC
service (`simple_kvm`) that starts on boot and is already running once the
script finishes.

Check it's running:

```sh
rc-service simple_kvm status
cat /var/log/simple_kvm.log
```

Then open `http://<device ip>:3000` from a browser on the same network -
no certificate warning to click through, see [No TLS to
set up](#no-tls-to-set-up).

**Updating later:** push a new version tag on GitHub, then re-run the same
`wget ... | sh` command on the device - it always grabs the latest release.
The page, `app.js`, and `style.css` are all served with `Cache-Control:
no-store`, so a browser tab reloaded after an update always gets the new
version instead of quietly running old page code against the new server.

**Why the service waits 30 seconds after boot before starting:** on the
actual Wyse 3040 this was built and tested against, opening the capture
card right as it finishes USB enumeration at boot reliably hard-crashes
the machine (confirmed by repeated testing - starting the service at boot
crashed it every time; starting the exact same binary the exact same way
once the system had been up a while never did). The installed service
waits 30 seconds before starting to avoid that window. If you hit boot
crashes on different hardware, try increasing the delay in
`/etc/init.d/simple_kvm`'s `start_pre()`.

The CH9329 has shown the same crash-on-connect behavior, so the binary
itself also waits before opening the serial port (`SERIAL_OPEN_DELAY_SECS`,
default 30 seconds) — separately from the capture card's boot delay above,
so it applies any time the service starts, not just at boot.

### Configuration

All optional, set as environment variables (e.g. in `/etc/init.d/simple_kvm`
if you need to change one):

| Variable | Default | Meaning |
|---|---|---|
| `SERIAL_PATH` | `/dev/ttyUSB0` | CH9329/CH340 serial device |
| `SERIAL_OPEN_DELAY_SECS` | `30` | How long to wait before opening the CH9329 serial port, for the same reason as the capture card's boot delay below — set to `0` to disable. |
| `VIDEO_PATH` | `/dev/video0` | Capture card device |
| `HTTP_PORT` | `3000` | Port for the page and WebRTC signaling (`POST /rtc/offer`) |
| `SETTINGS_PATH` | `/etc/simple_kvm-settings.json` | Where frame rate/resolution/bitrate/mouse mode are written when you click **Save settings** on the page, and read back on the next startup. |
| `RUST_LOG` | `info,simple_kvm::rtc::session=debug,simple_kvm::ch9329::writer=debug` | Standard `tracing` log filter. By default, every keystroke/click is already logged at `debug` - no configuration needed first - since each log line includes how long that event took to queue/write, which is useful for tracking down input lag. A command that takes more than 50ms logs at `warn` regardless. Setting `RUST_LOG` yourself (e.g. `RUST_LOG=debug` for everything, or `RUST_LOG=warn` to quiet the per-keystroke lines) fully overrides the default above. |

Each browser tab's video/input connection (WebRTC) picks its own UDP port
automatically - there's no fixed port to configure for it, and nothing to
open in a firewall today, since `deploy/install.sh` doesn't set one up.

## Releasing a new version

1. Push this repo to GitHub, with Actions enabled.
2. Bump `version` in `Cargo.toml` to match the tag you're about to push
   (the server logs this version at startup, so it needs to stay in sync).
3. Push a version tag, e.g.:
   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```
   This runs the `build` workflow and publishes a GitHub Release with the
   `simple_kvm` binary attached.

## Development

```sh
cargo build
cargo nextest run
```

`e2e/browser-test.sh` drives the actual page with `agent-browser` against
the container's system Chromium. No real capture card or CH9329 is
needed: the capture card stays in its soft "no device" state (no
`v4l2loopback` support in the container to fake one), while the CH9329 is
faked with `socat` (a linked PTY pair - the app just needs something to
open at `SERIAL_PATH`, real hardware or not), which is enough to exercise
the mouse-mode half of Save. The capture half of that same scoping logic
is covered by Rust tests in `src/rtc/session.rs` instead, since
it can't be exercised without real capture hardware. This script needs
`socat` installed; it's a layer on top of the Rust tests, not a
replacement for them.
