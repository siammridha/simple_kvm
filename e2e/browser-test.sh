#!/bin/sh
# Starts simple_kvm (pointed at nonexistent hardware paths, so it runs in
# its soft "no device" state — no capture card/CH9329 needed) and drives
# the page with agent-browser against the container's system Chromium.
# This only adds a browser layer on top of the page; it doesn't replace
# `cargo nextest run`.
set -eu

cd "$(dirname "$0")/.."

export HTTP_PORT="${HTTP_PORT:-3000}"
export WEBTRANSPORT_PORT="${WEBTRANSPORT_PORT:-4433}"
export SERIAL_PATH="${SERIAL_PATH:-/dev/nonexistent-ttyUSB}"
export VIDEO_PATH="${VIDEO_PATH:-/dev/nonexistent-video}"
export RUST_LOG="${RUST_LOG:-warn}"

CERT_DIR="$(mktemp -d)"
# TLS_CERT_PATH/TLS_KEY_PATH are required (the server no longer generates
# its own certificate) - a throwaway self-signed pair is enough for this
# test, since AGENT_BROWSER_IGNORE_HTTPS_ERRORS below makes the browser
# trust it without a manual "accept the risk" step.
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
STATUS_TEXT=""
for _ in $(seq 1 50); do
	STATUS_TEXT=$(agent-browser get text "#status")
	[ "$STATUS_TEXT" = "connected" ] && break
	sleep 0.1
done
echo "status indicator: $STATUS_TEXT"
if [ "$STATUS_TEXT" != "connected" ]; then
	echo "FAIL: status never reached 'connected' - WebTransport didn't connect without cert-hash pinning" >&2
	exit 1
fi

echo "Changing frame rate and clicking Save settings..."
agent-browser select "#frame-rate" 25
agent-browser click "#save-settings"
sleep 0.5

if ! grep -q '"fps": 25' "$CERT_DIR/settings.json" 2>/dev/null; then
	echo "FAIL: settings.json doesn't show fps: 25 after clicking Save" >&2
	cat "$CERT_DIR/settings.json" 2>&1 >&2 || echo "(file doesn't exist)" >&2
	exit 1
fi

echo "Reloading to confirm the change was applied and persisted (not just picked in the dropdown)..."
agent-browser reload
agent-browser wait --load load
STATUS_TEXT=""
for _ in $(seq 1 50); do
	STATUS_TEXT=$(agent-browser get text "#status")
	[ "$STATUS_TEXT" = "connected" ] && break
	sleep 0.1
done
if [ "$STATUS_TEXT" != "connected" ]; then
	echo "FAIL: status never reached 'connected' after reload" >&2
	exit 1
fi

FRAME_RATE=$(agent-browser get value "#frame-rate")
echo "frame rate after reload: $FRAME_RATE"
if [ "$FRAME_RATE" != "25" ]; then
	echo "FAIL: expected frame rate 25 to have persisted through Save + reload, got '$FRAME_RATE'" >&2
	exit 1
fi

PAGE_ERRORS=$(agent-browser errors)
if [ -n "$PAGE_ERRORS" ]; then
	echo "FAIL: uncaught page errors:" >&2
	echo "$PAGE_ERRORS" >&2
	exit 1
fi

echo "PASS: page loaded, dropdowns present, WebTransport connected without cert-hash pinning, Save settings applied and persisted, no uncaught JS errors."
