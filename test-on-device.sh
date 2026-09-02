#!/bin/sh
# Build a debug-fast release binary and push it straight to the real device,
# for testing changes without waiting on a tagged release. Run from the repo
# root:
#
#   ./test-on-device.sh
#
# The device accepts root login with no key and no password (PermitEmptyPasswords),
# so no credentials are needed here.
#
# Set DEVICE_IP to your device's LAN address before running, e.g.:
#
#   DEVICE_IP=192.168.1.50 ./test-on-device.sh
set -eu

[ -f .env ] && export $(cat .env | xargs)

: "${DEVICE_IP:?Set DEVICE_IP to your device's LAN address, e.g. DEVICE_IP=192.168.1.50 ./test-on-device.sh}"

if ! command -v cargo-zigbuild >/dev/null 2>&1; then
	echo "Installing cargo-zigbuild..."
	cargo install cargo-zigbuild
fi
rustup target add x86_64-unknown-linux-musl >/dev/null

SSH="ssh -o StrictHostKeyChecking=accept-new root@$DEVICE_IP"
SCP="scp -o StrictHostKeyChecking=accept-new"

# `libva-rs`'s build script links against `libva`/`libva-drm` (see its
# build.rs) - on a build host whose own arch differs from the target's (e.g.
# this aarch64 devcontainer cross-compiling to x86_64), the host's own
# libva-dev package is the wrong arch to link against, and pkg-config
# refuses to probe a foreign arch without extra cross-compile setup this
# devcontainer doesn't have. The device itself already has real x86_64
# libva headers and .so files (it has to, to run the encoder) - so fetch
# them from there once and point the build straight at them via
# LIBVA_RS_H_PATH/LIBVA_RS_LIB_PATH (which build.rs checks before ever
# reaching for pkg-config), skipping the cross pkg-config problem entirely.
# Cached under ~/.cache so a repeat run doesn't re-fetch from the device.
LIBVA_SYSROOT="$HOME/.cache/simple_kvm-libva-x86_64-linux-musl"
if [ ! -f "$LIBVA_SYSROOT/lib/libva.so" ]; then
	echo "Fetching x86_64 libva headers/libraries from $DEVICE_IP (cached at $LIBVA_SYSROOT)..."
	mkdir -p "$LIBVA_SYSROOT/include" "$LIBVA_SYSROOT/lib"
	# The whole va/ header directory, not just the ones libva-wrapper.h
	# includes directly - va.h itself pulls in several sibling headers
	# (va_version.h, va_str.h, the per-codec va_{enc,dec}_*.h, etc).
	$SCP -r "root@$DEVICE_IP:/usr/include/va" "$LIBVA_SYSROOT/include/"
	$SCP "root@$DEVICE_IP:/usr/lib/libva.so" "root@$DEVICE_IP:/usr/lib/libva-drm.so" "$LIBVA_SYSROOT/lib/"
fi
export LIBVA_RS_H_PATH="$LIBVA_SYSROOT/include"
export LIBVA_RS_LIB_PATH="$LIBVA_SYSROOT/lib"

echo "Building release binary for x86_64-unknown-linux-musl..."
RUSTC_BOOTSTRAP=1 cargo zigbuild --release --target x86_64-unknown-linux-musl

BINARY="${CARGO_TARGET_DIR:-target}/x86_64-unknown-linux-musl/release/simple_kvm"

echo "Copying $BINARY to $DEVICE_IP..."
$SCP "$BINARY" "root@$DEVICE_IP:/usr/local/bin/simple_kvm.new"

echo "Installing and restarting simple_kvm on the device..."
# mv instead of overwriting the running binary directly, to avoid a
# "Text file busy" error (same reason as deploy/install.sh). Also force
# debug logging via /etc/conf.d (OpenRC sources this automatically before
# the init script runs), so every test deploy logs everything, unlike an
# installed service (deploy/install.sh sets plain info-level there instead).
# The rtc library's own internal pipeline (rtc::peer_connection::handler) is
# turned back down to info - it logs a "bypass write" line per transport
# layer every second as part of its normal internal timer tick, which
# drowns out real debug logs.
$SSH "echo 'export RUST_LOG=debug,rtc::peer_connection::handler=info' > /etc/conf.d/simple_kvm && chmod 755 /usr/local/bin/simple_kvm.new && mv /usr/local/bin/simple_kvm.new /usr/local/bin/simple_kvm && rc-service simple_kvm restart"

echo "Done. Status:"
$SSH "rc-service simple_kvm status"

echo
echo "Open http://$DEVICE_IP:3000 in a browser to test."
echo "Logs: ssh root@$DEVICE_IP tail -f /var/log/simple_kvm.log"
