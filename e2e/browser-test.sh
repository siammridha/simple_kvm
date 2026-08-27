#!/bin/sh
# Starts simple_kvm and drives the page with agent-browser against the
# container's system Chromium. The capture card is never present (no
# v4l2loopback support in this container - see the CH9329 note below), so
# capture stays in its soft "no device" state. The CH9329 is faked over a
# socat PTY pair - the app only cares that something answers at
# SERIAL_PATH, not that it's real hardware - so the mouse-mode half of
# Save/settings-push can actually be exercised, while the capture half of
# that same logic is covered by src/rtc/session.rs's Rust tests instead.
# Also exercises issue #005's renegotiation mechanism (add/remove the
# video track after the connection is already up) via a debug-only hook -
# see the "Debug trigger" steps below. This only adds a browser layer on
# top of the page; it doesn't replace `cargo nextest run`.
set -eu

cd "$(dirname "$0")/.."

export HTTP_PORT="${HTTP_PORT:-3000}"
export VIDEO_PATH="${VIDEO_PATH:-/dev/nonexistent-video}"
export RUST_LOG="${RUST_LOG:-warn}"

TEST_DIR="$(mktemp -d)"
socat -d -d pty,raw,echo=0,link="$TEST_DIR/ch9329" pty,raw,echo=0 >"$TEST_DIR/socat.log" 2>&1 &
SOCAT_PID=$!
for _ in $(seq 1 50); do [ -e "$TEST_DIR/ch9329" ] && break; sleep 0.1; done
export SERIAL_PATH="$TEST_DIR/ch9329"
export SERIAL_OPEN_DELAY_SECS=0

export AGENT_BROWSER_EXECUTABLE_PATH="/usr/bin/chromium"
export AGENT_BROWSER_ARGS="--no-sandbox"

echo "Building..."
cargo build --quiet

./target/debug/simple_kvm &
SERVER_PID=$!

cleanup() {
	kill "$SERVER_PID" 2>/dev/null || true
	kill "$SOCAT_PID" 2>/dev/null || true
	agent-browser close >/dev/null 2>&1 || true
	rm -rf "$TEST_DIR"
}
trap cleanup EXIT

echo "Waiting for the server to come up..."
for _ in $(seq 1 50); do
	if curl -sf "http://localhost:$HTTP_PORT/" >/dev/null 2>&1; then
		break
	fi
	sleep 0.1
done

echo "Loading the page..."
agent-browser open "http://localhost:$HTTP_PORT/"
agent-browser wait --load load

DROPDOWN_COUNT=$(agent-browser get count select)
echo "dropdowns found: $DROPDOWN_COUNT"
if [ "$DROPDOWN_COUNT" -ne 3 ]; then
	echo "FAIL: expected 3 dropdowns (frame rate, resolution, mouse mode), got $DROPDOWN_COUNT" >&2
	exit 1
fi

echo "Waiting for the WebRTC connection..."
# "no video device found" is the correct status here, not a failure - this
# test has no capture card, and the device-state push (sent as soon as the
# control channel opens) correctly overrides the page's initial "connected"
# text to say so. Either string proves the WebRTC connection itself
# succeeded (offer/answer exchanged, data channels open), which is what
# this step is actually checking.
STATUS_TEXT=""
for _ in $(seq 1 50); do
	STATUS_TEXT=$(agent-browser get text "#status")
	{ [ "$STATUS_TEXT" = "connected" ] || [ "$STATUS_TEXT" = "no video device found" ]; } && break
	sleep 0.1
done
echo "status indicator: $STATUS_TEXT"
if [ "$STATUS_TEXT" != "connected" ] && [ "$STATUS_TEXT" != "no video device found" ]; then
	echo "FAIL: status never reached 'connected' or 'no video device found' - WebRTC didn't connect" >&2
	exit 1
fi

# Issue #005: the session starts with no video track at all (see
# rtc::negotiate). window.__debugToggleVideo (assets/web/app.js) is a
# test-only hook that sends ControlMessage::DebugToggleVideo, the manual
# stand-in issue #005 uses to prove the add_track/remove_track +
# renegotiation mechanism end to end - issue #006 replaces it with a
# trigger driven by real capture-device availability. There's no real
# capture card in this container, so no actual video frames ever flow;
# what's checked here is that renegotiation itself completes cleanly -
# a receiver appears after "add", the connection stays 'connected'
# throughout, and it can be exercised twice in a row (add, remove, add).
echo "Debug trigger: adding a video track and renegotiating..."
agent-browser eval "window.__debugToggleVideo()" >/dev/null
sleep 0.5
RECEIVER_COUNT=$(agent-browser eval "window.__debugPeerConnection().getReceivers().filter(r => r.track && r.track.kind === 'video').length")
echo "video receivers after add: $RECEIVER_COUNT"
if [ "$RECEIVER_COUNT" -lt 1 ]; then
	echo "FAIL: expected a video receiver after the debug add trigger, got $RECEIVER_COUNT" >&2
	exit 1
fi
# agent-browser eval returns JSON.stringify()'d results, so a bare string
# comes back double-quoted (e.g. `"connected"`) - comparing the raw
# connectionState string against a bash literal would always fail. Doing
# the comparison in JS and returning a bool sidesteps that.
CONN_STATE=$(agent-browser eval "window.__debugPeerConnection().connectionState")
IS_CONNECTED=$(agent-browser eval "window.__debugPeerConnection().connectionState === 'connected'")
if [ "$IS_CONNECTED" != "true" ]; then
	echo "FAIL: connection state should still be 'connected' after adding video, got $CONN_STATE" >&2
	exit 1
fi

echo "Debug trigger: removing the video track and renegotiating..."
agent-browser eval "window.__debugToggleVideo()" >/dev/null
sleep 0.5
CONN_STATE=$(agent-browser eval "window.__debugPeerConnection().connectionState")
IS_CONNECTED=$(agent-browser eval "window.__debugPeerConnection().connectionState === 'connected'")
if [ "$IS_CONNECTED" != "true" ]; then
	echo "FAIL: connection state should still be 'connected' after removing video, got $CONN_STATE" >&2
	exit 1
fi

echo "Debug trigger: adding the video track again (exercising add/remove/add)..."
agent-browser eval "window.__debugToggleVideo()" >/dev/null
sleep 0.5
RECEIVER_COUNT=$(agent-browser eval "window.__debugPeerConnection().getReceivers().filter(r => r.track && r.track.kind === 'video').length")
if [ "$RECEIVER_COUNT" -lt 1 ]; then
	echo "FAIL: expected a video receiver again after the second add trigger, got $RECEIVER_COUNT" >&2
	exit 1
fi

echo "Confirming the input/control data channels survived all that renegotiation..."
DC_STATES=$(agent-browser eval "inputChannel.readyState + ',' + controlChannel.readyState")
BOTH_OPEN=$(agent-browser eval "inputChannel.readyState === 'open' && controlChannel.readyState === 'open'")
echo "input,control readyState: $DC_STATES"
if [ "$BOTH_OPEN" != "true" ]; then
	echo "FAIL: expected both data channels still 'open' after renegotiating, got $DC_STATES" >&2
	exit 1
fi

# The CH9329 writer only checks whether its port is still present when it
# actually handles a command - a keypress is the trigger that makes it
# notice the socat-faked device and mark HID as connected, same as it
# would notice a real CH9329 on the next keystroke after being plugged in.
echo "Sending a keypress so the server notices the faked CH9329..."
agent-browser click "#video-surface"
agent-browser press "a"
sleep 0.5

# The top bar slides off-screen once connected (clicking the video surface
# above just closed it, since we're connected by this point) - the handle
# re-opens it. The dropdowns/Save button now live in the settings panel,
# opened via the gear icon in the bar.
echo "Opening the top bar..."
agent-browser click "#topbar-handle"

echo "Opening the settings panel..."
agent-browser click "#settings-button"

echo "Changing mouse mode and clicking Save settings..."
agent-browser select "#mouse-mode" relative
agent-browser click "#save-settings"
sleep 0.5

echo "Reloading to confirm the change was applied in memory (not just picked in the dropdown)..."
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
	echo "FAIL: expected mouse mode 'relative' to still be in effect (in memory) through Save + reload, got '$MOUSE_MODE'" >&2
	exit 1
fi

PAGE_ERRORS=$(agent-browser errors)
if [ -n "$PAGE_ERRORS" ]; then
	echo "FAIL: uncaught page errors:" >&2
	echo "$PAGE_ERRORS" >&2
	exit 1
fi

echo "PASS: page loaded, dropdowns present, WebRTC connected with no TLS anywhere, Save settings applied in memory (no settings file), no uncaught JS errors."
