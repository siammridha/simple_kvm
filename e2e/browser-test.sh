#!/bin/sh
# Starts simple_kvm and drives the page with agent-browser against the
# container's system Chromium. The capture card is never present (no
# v4l2loopback support in this container - see the CH9329 note below), so
# capture stays in its soft "no device" state. The CH9329 is faked over a
# socat PTY pair - the app only cares that something answers at
# SERIAL_PATH, not that it's real hardware - so the mouse-mode half of
# Save/settings-push can actually be exercised, while the capture half of
# that same logic is covered by src/webtransport/session.rs's Rust tests
# instead. This only adds a browser layer on top of the page; it doesn't
# replace `cargo nextest run`.
set -eu

cd "$(dirname "$0")/.."

export HTTP_PORT="${HTTP_PORT:-3000}"
export WEBTRANSPORT_PORT="${WEBTRANSPORT_PORT:-4433}"
export VIDEO_PATH="${VIDEO_PATH:-/dev/nonexistent-video}"
export RUST_LOG="${RUST_LOG:-warn}"

CERT_DIR="$(mktemp -d)"
socat -d -d pty,raw,echo=0,link="$CERT_DIR/ch9329" pty,raw,echo=0 >"$CERT_DIR/socat.log" 2>&1 &
SOCAT_PID=$!
for _ in $(seq 1 50); do [ -e "$CERT_DIR/ch9329" ] && break; sleep 0.1; done
export SERIAL_PATH="$CERT_DIR/ch9329"
export SERIAL_OPEN_DELAY_SECS=0
# TLS_CERT_PATH/TLS_KEY_PATH are required (the server no longer generates
# its own certificate) - a throwaway self-signed pair is enough for this
# test. AGENT_BROWSER_IGNORE_HTTPS_ERRORS below covers the page's plain
# HTTPS load; the WebTransport connection itself is pinned to this cert's
# hash (serverCertificateHashes), which is why it's a 1-day EC key - that's
# the validity window and key type the pinning check requires.
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
	-keyout "$CERT_DIR/key.pem" -out "$CERT_DIR/cert.pem" -days 1 \
	-subj "/CN=localhost" -addext "subjectAltName=DNS:localhost" >/dev/null 2>&1
export TLS_CERT_PATH="$CERT_DIR/cert.pem"
export TLS_KEY_PATH="$CERT_DIR/key.pem"
# The default SETTINGS_PATH (/etc/simple_kvm-settings.json) usually isn't
# writable by a non-root test user - point at the temp dir instead so the
# Save-settings persistence check below exercises a real file write, not
# just the in-memory value surviving a page reload.
export SETTINGS_PATH="$CERT_DIR/settings.json"

export AGENT_BROWSER_EXECUTABLE_PATH="/usr/bin/chromium"
export AGENT_BROWSER_ARGS="--no-sandbox"
# The page is served over HTTPS with a self-signed cert (required for
# browsers to expose the WebTransport API outside a localhost origin) -
# this lets agent-browser load it, and open the WebTransport connection,
# without a manual "accept the risk" step.
export AGENT_BROWSER_IGNORE_HTTPS_ERRORS=1

echo "Building..."
cargo build --quiet

./target/debug/simple_kvm &
SERVER_PID=$!

cleanup() {
	kill "$SERVER_PID" 2>/dev/null || true
	kill "$SOCAT_PID" 2>/dev/null || true
	agent-browser close >/dev/null 2>&1 || true
	rm -rf "$CERT_DIR"
}
trap cleanup EXIT

echo "Waiting for the server to come up..."
for _ in $(seq 1 50); do
	if curl -skf "https://localhost:$HTTP_PORT/" >/dev/null 2>&1; then
		break
	fi
	sleep 0.1
done

echo "Loading the page..."
agent-browser open "https://localhost:$HTTP_PORT/"
agent-browser wait --load load

DROPDOWN_COUNT=$(agent-browser get count select)
echo "dropdowns found: $DROPDOWN_COUNT"
if [ "$DROPDOWN_COUNT" -ne 4 ]; then
	echo "FAIL: expected 4 dropdowns (video mode, frame rate, resolution, mouse mode), got $DROPDOWN_COUNT" >&2
	exit 1
fi

echo "Waiting for the WebTransport connection..."
# "no video device found" is the correct status here, not a failure - this
# test has no capture card, and the device-state push (sent as soon as the
# control stream opens) correctly overrides the page's initial "connected"
# text to say so. Either string proves the WebTransport connection itself
# succeeded, which is what this step is actually checking.
STATUS_TEXT=""
for _ in $(seq 1 50); do
	STATUS_TEXT=$(agent-browser get text "#status")
	{ [ "$STATUS_TEXT" = "connected" ] || [ "$STATUS_TEXT" = "no video device found" ]; } && break
	sleep 0.1
done
echo "status indicator: $STATUS_TEXT"
if [ "$STATUS_TEXT" != "connected" ] && [ "$STATUS_TEXT" != "no video device found" ]; then
	echo "FAIL: status never reached 'connected' or 'no video device found' - WebTransport didn't connect with cert-hash pinning" >&2
	exit 1
fi

# The CH9329 writer only checks whether its port is still present when it
# actually handles a command - a keypress is the trigger that makes it
# notice the socat-faked device and mark HID as connected, same as it
# would notice a real CH9329 on the next keystroke after being plugged in.
echo "Sending a keypress so the server notices the faked CH9329..."
agent-browser click "#video"
agent-browser press "a"
sleep 0.5

echo "Changing mouse mode and clicking Save settings..."
agent-browser select "#mouse-mode" relative
agent-browser click "#save-settings"
sleep 0.5

if ! grep -q '"mouse_mode": "relative"' "$CERT_DIR/settings.json" 2>/dev/null; then
	echo "FAIL: settings.json doesn't show mouse_mode: relative after clicking Save" >&2
	cat "$CERT_DIR/settings.json" 2>&1 >&2 || echo "(file doesn't exist)" >&2
	exit 1
fi

echo "Reloading to confirm the change was applied and persisted (not just picked in the dropdown)..."
agent-browser reload
agent-browser wait --load load
STATUS_TEXT=""
for _ in $(seq 1 50); do
	STATUS_TEXT=$(agent-browser get text "#status")
	{ [ "$STATUS_TEXT" = "connected" ] || [ "$STATUS_TEXT" = "no video device found" ]; } && break
	sleep 0.1
done
if [ "$STATUS_TEXT" != "connected" ] && [ "$STATUS_TEXT" != "no video device found" ]; then
	echo "FAIL: status never reached 'connected' or 'no video device found' after reload" >&2
	exit 1
fi

MOUSE_MODE=$(agent-browser get value "#mouse-mode")
echo "mouse mode after reload: $MOUSE_MODE"
if [ "$MOUSE_MODE" != "relative" ]; then
	echo "FAIL: expected mouse mode 'relative' to have persisted through Save + reload, got '$MOUSE_MODE'" >&2
	exit 1
fi

PAGE_ERRORS=$(agent-browser errors)
if [ -n "$PAGE_ERRORS" ]; then
	echo "FAIL: uncaught page errors:" >&2
	echo "$PAGE_ERRORS" >&2
	exit 1
fi

echo "PASS: page loaded, dropdowns present, WebTransport connected with cert-hash pinning, Save settings applied and persisted, no uncaught JS errors."
