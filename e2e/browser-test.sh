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

export AGENT_BROWSER_EXECUTABLE_PATH="/usr/bin/chromium"
export AGENT_BROWSER_ARGS="--no-sandbox"
# The page is served over HTTPS with a self-signed cert (required for
# browsers to expose the WebTransport API outside a localhost origin) -
# this lets agent-browser load it without a manual "accept the risk" step.
export AGENT_BROWSER_IGNORE_HTTPS_ERRORS=1

echo "Building..."
cargo build --quiet

./target/debug/simple_kvm &
SERVER_PID=$!

cleanup() {
	kill "$SERVER_PID" 2>/dev/null || true
	agent-browser close >/dev/null 2>&1 || true
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
if [ "$DROPDOWN_COUNT" -ne 3 ]; then
	echo "FAIL: expected 3 dropdowns (video mode, resolution, mouse mode), got $DROPDOWN_COUNT" >&2
	exit 1
fi

STATUS_TEXT=$(agent-browser get text "#status")
echo "status indicator: $STATUS_TEXT"
if [ -z "$STATUS_TEXT" ]; then
	echo "FAIL: status indicator is empty" >&2
	exit 1
fi

PAGE_ERRORS=$(agent-browser errors)
if [ -n "$PAGE_ERRORS" ]; then
	echo "FAIL: uncaught page errors:" >&2
	echo "$PAGE_ERRORS" >&2
	exit 1
fi

echo "PASS: page loaded, dropdowns present, status indicator set, no uncaught JS errors."
