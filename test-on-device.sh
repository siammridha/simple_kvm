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

echo "Building release binary for x86_64-unknown-linux-musl..."
RUSTC_BOOTSTRAP=1 cargo zigbuild --release --target x86_64-unknown-linux-musl

BINARY="target/x86_64-unknown-linux-musl/release/simple_kvm"
SSH="ssh -o StrictHostKeyChecking=accept-new root@$DEVICE_IP"
SCP="scp -o StrictHostKeyChecking=accept-new"

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
