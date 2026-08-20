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

Run the binary on the Wyse 3040 and it serves a page at `https://<device
ip>:3000` with:

- **Live video**, streamed over WebTransport (not WebRTC). Two modes,
  switchable live from a dropdown:
  - **MJPEG** (default) - if the capture card has hardware MJPEG (most
    UVC capture cards do), frames are forwarded as-is, essentially free on
    the CPU. If not, frames are JPEG-compressed in software from the raw
    feed - still much cheaper than video encoding, just not free.
  - **H.264**, software-encoded. Included because it was asked for, but
    the Wyse 3040's Atom CPU is genuinely weak for real-time software
    video encoding - expect this mode to be choppy. MJPEG is the
    practical default. The encoder inserts a keyframe every 60 frames so
    a browser that (re)connects mid-stream has something to start
    decoding from - without it, only the very first frame of a capture
    session would ever be a keyframe, and joining any later would leave
    the video stuck.
- **Resolution dropdown**, populated from whatever the capture card
  actually reports supporting (queried at startup) - not a hardcoded
  list.
- **Mouse clicks and scroll wheel**, absolute or relative mode
  switchable live; on the CH9329 hardware this repo was built against,
  clicks and scroll wheel only work through its *relative* HID report,
  so both modes send them via a zero-motion relative report - invisible
  from the browser, just how `ch9329::writer` talks to this chip. Mouse
  *movement* is not sent at all - moving the mouse over the video was
  crashing the Wyse 3040, so cursor tracking has been removed for now
  and only clicks/scroll go through.
- **A paste box** - text typed or pasted into it is sent to the target as
  simulated keystrokes (US QWERTY only; there's no OS-level clipboard
  access over a HID-only link).
- **No login.** Anyone who can reach port 3000 has full control. This is
  meant for a trusted LAN, not the open internet.

The server runs fine with no capture card or CH9329 attached - the page
still loads, dropdowns just reflect "no video device," and keyboard/mouse
input is silently dropped instead of the service failing to start. Useful
for development without the hardware plugged in. This also covers either
device disconnecting after the service has already started, and both
recover on their own once reconnected, no restart needed: the CH9329
silently drops input while it's gone and picks back up the next time a key
or click comes in; the capture card pauses video the same way and resumes
streaming once it's replugged (the page itself only picks up the "video is
back" state on its next load or reconnect, since the resolution dropdown
is filled in when the page connects, not continuously).

**Dropdown changes are saved automatically** - there's no separate "save"
step required. Changing video mode, resolution, or mouse mode applies
immediately (as before) and is also written to a small settings file on
disk, so the next time the service starts (e.g. after a reboot) it comes
back up with the same choices instead of resetting to the capture card's
defaults. If you reload the page, the dropdowns show whatever the server
is actually using right now. A **Save settings** button is there too, for
a visible confirmation that the current choices are on disk - it writes
the same file the automatic save does, so it's a manual double-check, not
a required step.

### TLS for the page and the video/input connection

Browsers only expose the `WebTransport` API on a secure context - an
`https://` page (or `http://localhost`, which doesn't apply here since
the device is reached by its LAN IP). So the page itself is served over
HTTPS, using the same certificate as the WebTransport connection.

There's no public domain for a LAN device, so by default the server
generates its own self-signed certificate. The WebTransport connection
pins it by hash (`serverCertificateHashes`), so there's no manual "accept
this certificate" step needed for that part. The page itself, being
regular HTTPS, does still need the browser to trust or accept the
self-signed cert once (a normal "your connection isn't private" warning
to click through, or add `TLS_CERT_PATH`/`TLS_KEY_PATH` pointing at a
cert your browser already trusts to skip that entirely).

Chrome caps a self-signed cert used with `serverCertificateHashes` at 14
days, so the server regenerates it every 12 days automatically; the page's
HTTPS listener picks up the new certificate immediately (no restart), and
any WebTransport session connected at that moment reconnects on its own.

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
with `bindgen` at build time) and `nasm` (speeds up the `openh264`
encoder, which matters on this CPU). Both the devcontainer and the release
workflow already install these.

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

Then open `https://<device ip>:3000` from a browser on the same network
(you'll likely need to click through a self-signed certificate warning
the first time - see [TLS for the page and the video/input
connection](#tls-for-the-page-and-the-videoinput-connection)).

**Updating later:** push a new version tag on GitHub, then re-run the same
`wget ... | sh` command on the device - it always grabs the latest release.

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
| `HTTP_PORT` | `3000` | HTTPS port for the page |
| `WEBTRANSPORT_PORT` | `4433` | UDP port for the video/input connection |
| `TLS_SAN` | `localhost` | Comma-separated subject names for the self-signed cert |
| `TLS_CERT_PATH`, `TLS_KEY_PATH` | unset | Use a specific cert/key PEM pair instead of generating a self-signed one. Set both to enable; loaded once at startup and never auto-rotated - that's on whoever manages the file pair. |
| `SETTINGS_PATH` | `/etc/simple_kvm-settings.json` | Where video mode/resolution/mouse mode are saved whenever a dropdown changes, and read back on the next startup. |

## Releasing a new version

1. Push this repo to GitHub, with Actions enabled.
2. Push a version tag, e.g.:
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
the container's system Chromium (no capture card/CH9329 needed - it runs
against the same soft "no device" state described above). It's a layer on
top of the Rust tests, not a replacement for them.
