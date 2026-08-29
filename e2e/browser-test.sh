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
# below already accommodate. Once dispatched, this is enough to drive a
# real CaptureCard::request_stream() success and prove the session's
# add-track + renegotiation path end to end. The fake file also isn't a
# real V4L2 device, so the capture pass started for it fails
# immediately once it runs (CaptureDriver::probe never reports a
# supported format) - CaptureStream's `ended` event fires from that as it
# would from a genuine mid-session unplug, proving the remove-track half
# too, all without needing any uevent at all.
#
# What this setup *can't* exercise: a genuine mid-session replug (that
# needs a real "video4linux" uevent this container has no privileged way
# to synthesize). That path shares the exact same `try_attach_video`
# function already proven above by the initial attach, triggered by
# `CaptureCard::add_event_listener`'s presence forwarding - covered at
# the Rust level instead by device::tests::
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
# what surfaces the "added video track"/"removed video track" lines this
# script greps the server log for below.
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
# "no video device found" is the correct status here, not a failure - the
# fake file at VIDEO_PATH isn't a real V4L2 device, so once device's
# detect-to-probe delay elapses and CaptureDriver::probe actually runs
# against it (see the header comment above), the probe itself fails and
# DeviceState.available stays false for this whole run - even though the
# presence-gated video-track path below still fires, since that only
# needs the device to be *present*, not probed successfully. Either
# string proves the WebRTC connection itself succeeded
# (offer/answer exchanged, data channels open), which is what this step is
# actually checking - negotiate() no longer attaches a video track up
# front either way (see src/rtc/mod.rs).
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

# Waits (bounded) for the server's log to contain at least $2 occurrences
# of $1 - `rtc::session::handle` logs exactly these lines from
# `try_attach_video`/`remove_video_track` (see src/rtc/session.rs), so
# this is a direct check that the real add/remove-track code path ran, not
# an inference from browser-visible WebRTC object state (a recvonly
# transceiver's receiver/track exists from the initial offer regardless of
# whether the server ever attaches anything to it, so receiver count can't
# tell the two states apart).
wait_for_log_count() {
	pattern="$1"
	want="$2"
	got=0
	for _ in $(seq 1 100); do
		got=$(grep -c "$pattern" "$SERVER_LOG" 2>/dev/null || true)
		[ -z "$got" ] && got=0
		[ "$got" -ge "$want" ] && return 0
		sleep 0.2
	done
	echo "FAIL: server log never reached $want occurrence(s) of '$pattern' (got $got)" >&2
	echo "--- server log tail ---" >&2
	tail -n 40 "$SERVER_LOG" >&2
	exit 1
}

echo "Waiting for the server to add this session's video track (fake device was already present at startup)..."
wait_for_log_count "capture device available: added video track" 1
echo "server added a video track for this session"
assert_still_connected "after adding video"

# The fake device file isn't a real V4L2 device, so CaptureDriver::probe
# never reports a supported format for it - the capture pass started for it
# fails immediately (see src/capture/mod.rs's run_one_pass and
# src/capture/engine.rs's own "ended_fires_exactly_once_on_unrecoverable_
# pass_failure" test, which exercises the exact same fake-file setup).
# CaptureStream's `ended` event fires from that, same as it would for a
# genuine mid-session unplug - this is what's being proven here: the
# session removes its video track and renegotiates on its own, with no
# further action from this script and no page reload.
echo "Waiting for the video track to be removed again (simulated capture failure/unplug)..."
wait_for_log_count "capture device unavailable: removed video track" 1
assert_still_connected "after losing video"

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

echo "PASS: page loaded, dropdowns present, WebRTC connected with no TLS anywhere, video track added then removed live via a real presence-gated capture stream, Save settings applied in memory (no settings file), no uncaught JS errors."
