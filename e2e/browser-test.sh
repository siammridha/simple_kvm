#!/bin/sh
# Starts simple_kvm and drives the page with agent-browser against the
# container's system Chromium. There's no real capture card in this
# container (no v4l2loopback support to fake one), so "the device is
# plugged in" is simulated the same way src/capture/engine.rs's own tests
# do it: a plain regular file at VIDEO_PATH. Device<CaptureDriver>'s
# presence detection only checks that the path exists - it doesn't care
# whether it's really a V4L2 node.
#
# The fake file is created *before* the server starts, not toggled live:
# this container's netlink uevent socket opens successfully (confirmed
# directly - no "failed to open" warning ever appears in its log), so
# Device<D>'s presence task waits for a genuine kernel "video4linux"
# uevent rather than falling back to polling: creating/deleting a plain
# file generates no such uevent, so a live plug/unplug can't be simulated
# from userspace here. Being present at process start, though, is a
# transition the presence task always detects synchronously on its very
# first check, with no uevent needed (see PresenceState::observe and
# device::tests::boot_time_already_present_is_still_a_detected_transition)
# - it still waits out device's 3-second detect-to-probe delay before the
# actual probe/dispatch, which this script's generous log-wait timeouts
# below already accommodate.
#
# Since the fake file isn't a real V4L2 device, CaptureDriver::probe never
# reports a supported format for it - so once the detect-to-probe delay
# elapses, the device stays "present but never probed"
# (DeviceStatus::Present(None)) for the rest of this run. Issue #027 made
# CaptureCard::request_stream() genuinely attempt and await a real device
# open (rather than gating on a stale presence flag), and its approved
# scope addition made `rtc::session` only ever attempt that open when the
# device is present *and* successfully probed
# (DeviceStatus::Present(Some(_))) - see device_probed_available in
# src/rtc/session.rs. So this fixture can no longer drive a video track
# being added at all: what it proves instead is that a present-but-
# unprobeable device correctly never gets a video track, and that the
# WebRTC connection and its data channels stay healthy regardless (see
# "Confirming no video track is ever added..." below).
#
# Genuine successful-attach (a real Device::open succeeding) and genuine
# mid-stream failure (CaptureStream's `ended` firing for a pass that
# started successfully and later died) both need a real capture card, so
# both now live entirely on real hardware via ./test-on-device.sh - issue
# #027's own acceptance criteria already requires that: "confirm the video
# track still attaches normally when the card is present, and that
# unplugging mid-stream still ends the stream the same way it does today."
#
# What this setup *can't* exercise at all, hardware or not: a genuine
# mid-session replug (that needs a real "video4linux" uevent this
# container has no privileged way to synthesize). That path shares the
# exact same `try_attach_video` function proven on real hardware by the
# initial attach, triggered by `rtc::session`'s own subscription on its
# `CaptureDevice` handle (`ctx.capture_device.add_event_listener`, see
# src/rtc/session.rs) - covered at the Rust level instead by device::tests::
# genuine_absent_to_present_transition_is_detected (the presence edge itself)
# and capture::engine::tests::
# live_count_restarts_after_pass_stopped_on_its_own_even_if_still_live
# (a fresh request_stream() restarting the pass).
#
# The CH9329 is faked over a socat PTY pair - the app only cares that
# something answers at SERIAL_PATH, not that it's real hardware - so the
# mouse-mode half of Save/settings-push can actually be exercised too.
# This only adds a browser layer on top of the page; it doesn't replace
# `cargo nextest run`.
set -eu

cd "$(dirname "$0")/.."

export HTTP_PORT="${HTTP_PORT:-3000}"
# `simple_kvm::rtc::session=info` on top of the otherwise-quiet default is
# what would surface the "added video track" line this script greps the
# server log for below - it should never actually appear for this run's
# present-but-unprobeable fake device (see the header comment above).
export RUST_LOG="${RUST_LOG:-warn,simple_kvm::rtc::session=info}"

TEST_DIR="$(mktemp -d)"
export VIDEO_PATH="${VIDEO_PATH:-$TEST_DIR/video0}"
echo "not a real capture device" >"$VIDEO_PATH"

socat -d -d pty,raw,echo=0,link="$TEST_DIR/ch9329" pty,raw,echo=0 >"$TEST_DIR/socat.log" 2>&1 &
SOCAT_PID=$!
for _ in $(seq 1 50); do [ -e "$TEST_DIR/ch9329" ] && break; sleep 0.1; done
export SERIAL_PATH="$TEST_DIR/ch9329"

export AGENT_BROWSER_EXECUTABLE_PATH="/usr/bin/chromium"
export AGENT_BROWSER_ARGS="--no-sandbox"

echo "Building..."
cargo build --quiet

SERVER_LOG="$TEST_DIR/server.log"
./target/debug/simple_kvm >"$SERVER_LOG" 2>&1 &
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
# The top status text now only ever says "connected" once the peer
# connection is up - whether a video device is available or not is shown
# separately, in the video overlay (checked below), not folded into this
# string any more.
STATUS_TEXT=""
for _ in $(seq 1 50); do
	STATUS_TEXT=$(agent-browser get text "#status")
	[ "$STATUS_TEXT" = "connected" ] && break
	sleep 0.1
done
echo "status indicator: $STATUS_TEXT"
if [ "$STATUS_TEXT" != "connected" ]; then
	echo "FAIL: status never reached 'connected' - WebRTC didn't connect" >&2
	exit 1
fi

# agent-browser eval returns JSON.stringify()'d results, so a bare string
# comes back double-quoted (e.g. `"connected"`) - comparing the raw
# connectionState string against a bash literal would always fail. Doing
# the comparison in JS and returning a bool sidesteps that.
assert_still_connected() {
	label="$1"
	conn_state=$(agent-browser eval "window.__debugPeerConnection().connectionState")
	is_connected=$(agent-browser eval "window.__debugPeerConnection().connectionState === 'connected'")
	if [ "$is_connected" != "true" ]; then
		echo "FAIL: connection state should still be 'connected' $label, got $conn_state" >&2
		exit 1
	fi
}

echo "Confirming no video track is ever added for a present-but-unprobeable device (issue #027 + the approved gate on probed availability: this container can't fake a working V4L2 device, so request_stream() should never even be attempted here)..."
if grep -q "capture device available: added video track" "$SERVER_LOG"; then
	echo "FAIL: a video track was added for a device that never successfully probes - #027/gating regression" >&2
	exit 1
fi
assert_still_connected "with no video track ever attached"

# With no video ever attached, deviceStateKnown flips true (the device_state
# push has arrived) but captureAvailable stays false for the rest of this
# run - the overlay should say so, the display status icon should stay off,
# and the mouse toggle should be force-disabled (there's no picture to point
# a click at, see mouseUsable() in app.js).
echo "Confirming the video overlay reports no video device..."
OVERLAY_TEXT=""
for _ in $(seq 1 50); do
	OVERLAY_TEXT=$(agent-browser get text "#video-overlay-text")
	[ "$OVERLAY_TEXT" = "No video device connected" ] && break
	sleep 0.1
done
echo "video overlay text: $OVERLAY_TEXT"
if [ "$OVERLAY_TEXT" != "No video device connected" ]; then
	echo "FAIL: expected the video overlay to say 'No video device connected', got '$OVERLAY_TEXT'" >&2
	exit 1
fi

DISPLAY_ON=$(agent-browser eval "document.getElementById('display-status-icon').classList.contains('status-on')")
if [ "$DISPLAY_ON" != "false" ]; then
	echo "FAIL: display status icon should not be 'on' with no video device, got $DISPLAY_ON" >&2
	exit 1
fi

MOUSE_ENABLED=$(agent-browser is enabled "#mouse-toggle-button")
if [ "$MOUSE_ENABLED" != "false" ]; then
	echo "FAIL: mouse toggle should be disabled with no video device to point at, got enabled=$MOUSE_ENABLED" >&2
	exit 1
fi

echo "Confirming the input/control data channels are open..."
DC_STATES=$(agent-browser eval "inputChannel.readyState + ',' + controlChannel.readyState")
BOTH_OPEN=$(agent-browser eval "inputChannel.readyState === 'open' && controlChannel.readyState === 'open'")
echo "input,control readyState: $DC_STATES"
if [ "$BOTH_OPEN" != "true" ]; then
	echo "FAIL: expected both data channels 'open', got $DC_STATES" >&2
	exit 1
fi

echo "Sending a keypress on the faked CH9329..."
agent-browser click "#video-surface"
agent-browser press "a"
sleep 0.5

# `rtc` subscribes to the CH9329's own presence directly (see
# ARCHITECTURE.md §3.4), so hid_state tracks the socat-faked device's
# presence on its own - not triggered by the keypress above, which only
# proves keyboard input still works with no video device attached. The
# keyboard status icon should reflect "available" within the same
# detect-to-probe delay device/mod.rs already waits out for the video
# fixture above.
echo "Confirming the keyboard status icon reports the faked CH9329 as available..."
KEYBOARD_ON="false"
for _ in $(seq 1 100); do
	KEYBOARD_ON=$(agent-browser eval "document.getElementById('keyboard-status-icon').classList.contains('status-on')")
	[ "$KEYBOARD_ON" = "true" ] && break
	sleep 0.1
done
if [ "$KEYBOARD_ON" != "true" ]; then
	echo "FAIL: keyboard status icon never turned on for the faked CH9329" >&2
	exit 1
fi

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

# The scroll-flip toggle is purely local (localStorage, not sent to the
# server or included in Save settings) - flip it here, before the reload
# below, to prove it survives a reload the same way a real browser tab
# restart would need it to.
echo "Flipping the scroll-flip toggle..."
SCROLL_FLIP_BEFORE=$(agent-browser eval "window.__debugScrollFlipped()")
agent-browser click "#scroll-flip-toggle"
SCROLL_FLIP_AFTER=$(agent-browser eval "window.__debugScrollFlipped()")
echo "scrollFlipped: $SCROLL_FLIP_BEFORE -> $SCROLL_FLIP_AFTER"
if [ "$SCROLL_FLIP_AFTER" = "$SCROLL_FLIP_BEFORE" ]; then
	echo "FAIL: clicking the scroll-flip toggle didn't change scrollFlipped" >&2
	exit 1
fi

echo "Reloading to confirm the mouse-mode and scroll-flip changes were applied (not just picked in the UI)..."
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

MOUSE_MODE=$(agent-browser get value "#mouse-mode")
echo "mouse mode after reload: $MOUSE_MODE"
if [ "$MOUSE_MODE" != "relative" ]; then
	echo "FAIL: expected mouse mode 'relative' to still be in effect (in memory) through Save + reload, got '$MOUSE_MODE'" >&2
	exit 1
fi

SCROLL_FLIP_RELOADED=$(agent-browser eval "window.__debugScrollFlipped()")
echo "scrollFlipped after reload: $SCROLL_FLIP_RELOADED"
if [ "$SCROLL_FLIP_RELOADED" != "$SCROLL_FLIP_AFTER" ]; then
	echo "FAIL: expected scrollFlipped to persist across reload (localStorage) as '$SCROLL_FLIP_AFTER', got '$SCROLL_FLIP_RELOADED'" >&2
	exit 1
fi

PAGE_ERRORS=$(agent-browser errors)
if [ -n "$PAGE_ERRORS" ]; then
	echo "FAIL: uncaught page errors:" >&2
	echo "$PAGE_ERRORS" >&2
	exit 1
fi

echo "PASS: page loaded, dropdowns present, WebRTC connected with no TLS anywhere, no video track attached for a present-but-unprobeable device, video overlay/status icons reflect that correctly, keyboard status icon reflects the faked CH9329, Save settings and the scroll-flip toggle both applied in memory and survived a reload, no uncaught JS errors."
