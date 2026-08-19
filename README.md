# simple_kvm

A small KVM-over-USB tool for a Dell Wyse 3040 running Alpine Linux. It sits
next to a target computer and lets you see its screen and control its
keyboard/mouse remotely.

## Hardware

- **CH9329 + CH340 UART/TTL Serial Port to USB Connecting Wire** - plugs
  into the target computer's USB port and acts as a keyboard/mouse (HID).
  The Wyse 3040 talks to it over serial to send keystrokes and mouse moves.
- **1080P Capture Card, USB 3.0 to HDMI** - plugs into the target
  computer's HDMI output and the Wyse 3040's USB port, so the Wyse 3040 can
  read the target's screen as a video source.

Both devices are recognized by the Linux kernel automatically (no extra
drivers to install) and show up as `/dev/ttyUSB*` (the CH9329/CH340 serial
adapter) and `/dev/video*` (the capture card) once plugged in.

## Current state

This repository currently has a minimal Rust binary (`src/main.rs`) plus
the build and deploy tooling described below. It doesn't yet read the
serial adapter or the capture card - that's the next piece of work.

## How it's built

- `.github/workflows/build.yml` - on a pushed version tag (`v*`), builds
  the release binary in a native x86_64 Alpine container (matching the
  Wyse 3040 exactly, so it's a normal build, not a cross-compile) and
  publishes it as a GitHub Release.
- `deploy/install.sh` - run on the device; downloads the latest release
  binary from GitHub and sets it up as an OpenRC service that starts on
  boot.

## Target platform

- **Device:** Dell Wyse 3040 (Intel Atom x5-Z8350, x86_64)
- **OS:** Alpine Linux

## Installing on the device

Run this on the Wyse 3040 itself, as root:

```sh
wget -qO- https://raw.githubusercontent.com/username/project_name/master/deploy/install.sh | sh
```

This downloads the latest release binary and sets it up as an OpenRC
service (`simple_kvm`) that starts on boot and is already running once the
script finishes.

Check it's running:

```sh
rc-service simple_kvm status
cat /var/log/simple_kvm.log
```

**Updating later:** push a new version tag on GitHub, then re-run the same
`wget ... | sh` command on the device - it always grabs the latest release.

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
