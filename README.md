# simple_kvm

A small KVM-over-USB tool for a Dell Wyse 3040 running Alpine Linux. It sits
next to a target computer, reads its screen through a capture card, and
sends keyboard/mouse input to it through a CH9329 HID adapter — controlled
entirely from a web page. Video is encoded to H.264 on the Wyse 3040's own
GPU (Intel's `i965` VAAPI driver) and streamed to the browser over WebRTC.

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

- **Live video** - streamed over WebRTC as H.264, decoded natively in the browser's `<video>` element.
- **Resolution dropdown** - populated from what the capture card actually reports supporting.
- **Frame rate dropdown** - populated from what the card supports at the currently-selected resolution.
- **Mouse movement, clicks, and scroll wheel** - absolute or relative mode, switched via Save settings.
- **A mouse on/off toggle** - turns all mouse forwarding on or off locally in the browser; keyboard input is unaffected.
- **A paste box** - sends pasted text to the target as simulated keystrokes (US QWERTY only).
- **An auto-hiding controls bar** - tucks away after 5 seconds idle, reopens on tap/click.
- **No login** - anyone who can reach port 3000 has full control, so keep this on a trusted LAN.

Video only starts when a browser tab is actually watching, and the capture card and CH9329 can be
hot-plugged at any time - the page picks up a device connecting or disconnecting live, no restart
or reload needed. Settings changes (resolution, frame rate, mouse mode) apply live via **Save
settings** and are not saved to disk - they reset to defaults on restart.

### No TLS to set up

The page is served over plain HTTP - no certificate needed. The video/input connection (WebRTC)
is still encrypted end to end on its own. See
[docs/transport-comparison.md](docs/transport-comparison.md) for details.

## How it's built

- `.github/workflows/build.yml` - on a pushed version tag (`v*`), builds
  the release binary in a native x86_64 Alpine container (matching the
  Wyse 3040 exactly, so it's a normal build, not a cross-compile) and
  publishes it as a GitHub Release.
- `.github/workflows/test.yml` - on every push and pull request, runs
  `./check-architecture.sh` (first, since it needs no toolchain), then
  builds the crate and runs the full `cargo nextest run` suite. It runs on a
  plain `ubuntu-latest` runner rather than the release job's Alpine
  container, because it only has to compile and run tests - it has no
  reason to match the device's musl libc, and apt already has the
  clang/libva/linux headers the build needs. `e2e/browser-test.sh` is not
  run in CI; see the comment at the top of the workflow for why.
- `deploy/install.sh` - run on the device; downloads the latest release
  binary from GitHub and sets it up as an OpenRC service that starts on
  boot.

Building needs a few extra Alpine packages beyond a bare Rust toolchain:
`clang-dev` and `linux-headers` (the `v4l` crate generates V4L2 bindings
with `bindgen` at build time) and `libva-dev` (the GPU H.264 encoder's
`bindgen`-based build script, and linking against libva). Both the
devcontainer and the release workflow already install these; the test
workflow installs the Debian equivalents (`clang`, `libclang-dev`,
`linux-libc-dev`, `libva-dev`) with apt.

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
copy, install, restart) over SSH (the device accepts root login with no
key and no password). Set `DEVICE_IP` to your device's LAN address:

```sh
DEVICE_IP=192.168.1.50 ./test-on-device.sh
```

## Target platform

- **Device:** Dell Wyse 3040 (Intel Atom x5-Z8350, x86_64)
- **OS:** Alpine Linux

## Installing on the device

Run this on the Wyse 3040 itself, as root:

```sh
wget -qO- https://raw.githubusercontent.com/siammridha/simple_kvm/main/deploy/install.sh | sh
```

This first checks the device has what the binary needs to run - root, an
x86_64 CPU, Alpine's `apk`, a GPU render device at `/dev/dri/renderD*`, and
the VAAPI runtime + Intel `i965` driver (`libva`/`libva-intel-driver`,
installing them from Alpine's `community` repo if missing) - since the
H.264 encoder needs the GPU and has no CPU fallback: if VAAPI setup fails,
the binary refuses to start rather than silently falling back to software
encoding. It also reports (without blocking on) whether the capture card
and CH9329 are currently plugged in, since both are hot-pluggable and the
app runs fine without either attached yet. Once the checks pass, it
downloads the latest release binary and sets it up as an OpenRC service
(`simple_kvm`) that starts on boot and is already running once the script
finishes.

This device's GPU needs the older `i965` driver, not the newer
`intel-media-driver`. That driver's own auto-generated H.264 headers are
broken, so the encoder builds them by hand instead of trusting the driver -
see [docs/gpu-encoding-investigation.md](docs/gpu-encoding-investigation.md)
for the full story.

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

**Known issue:** opening the capture card too soon after boot has crashed this hardware in testing.
The binary now waits until a browser actually connects before opening the capture card or CH9329,
which should avoid this, but it hasn't been confirmed on a real reboot yet.

### Configuration

All optional, set as environment variables (e.g. in `/etc/conf.d/simple_kvm`,
which OpenRC sources automatically before starting the service, if you need
to change one):

| Variable | Default | Meaning |
|---|---|---|
| `SERIAL_PATH` | `/dev/ttyUSB0` | CH9329/CH340 serial device |
| `VIDEO_PATH` | `/dev/video0` | Capture card device |
| `HTTP_PORT` | `3000` | Port for the page and WebRTC signaling (`POST /rtc/offer`) |
| `RUST_LOG` | `info` | Standard `tracing` log filter - `deploy/install.sh` sets this to `info` for an installed service; `test-on-device.sh` sets a more verbose filter for test deploys. |

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
./check-architecture.sh
```

`check-architecture.sh` checks the module boundaries listed in
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) section 8: no device path
outside `device/`, no import edge that isn't in the allowed table, and no
config or extra code in `main.rs`. It needs no toolchain and prints, every
run, which of the five checks it decides on its own and which still need a
human reading the code.

`e2e/browser-test.sh` drives the actual page with `agent-browser` against
the container's system Chromium. No real capture card or CH9329 is
needed: the capture card is faked as a plain regular file at `VIDEO_PATH`
(no `v4l2loopback` support in the container for a real one), which is
present but never probes as a real device - so it proves the WebRTC
connection, data channels, and UI (video overlay, status icons, Save
settings, the scroll-flip toggle) all behave correctly with no video
device, but can't exercise a real video track attaching or ending. The
CH9329 is faked with `socat` (a linked PTY pair - the app just needs
something to open at `SERIAL_PATH`, real hardware or not), which is
enough to prove the keyboard status icon and mouse-mode half of Save. A
real device attaching/ending, and a genuine mid-session replug, both need
real hardware - covered by `./test-on-device.sh` and by Rust tests in
`src/device/mod.rs` and `src/capture/engine.rs`. This script needs
`socat` installed; it's a layer on top of the Rust tests, not a
replacement for them.
