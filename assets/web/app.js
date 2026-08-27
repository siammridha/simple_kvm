// simple_kvm client: opens an RTCPeerConnection over plain HTTP signaling
// (POST /rtc/offer, no trickle ICE — see connect()), receives H.264 video
// as a native WebRTC track rendered into a <video> element. Keyboard and
// mouse input goes out as small binary messages on an
// unreliable/unordered data channel, and settings updates/paste text go
// out as JSON on a reliable data channel, which also carries
// device-state/settings pushes back from the server. That same reliable
// channel also carries any *later* offer/answer round trips the server
// starts (see handleRenegotiationOffer) - there's no other signaling
// channel left open once the initial connection is up.

const TAG_KEY_EVENT = 1;
const TAG_MOUSE_ABSOLUTE_MOVE = 2;
const TAG_MOUSE_RELATIVE_MOVE = 3;
const TAG_MOUSE_BUTTONS = 4;

const statusEl = document.getElementById('status');
const videoEl = document.getElementById('video-el');
const inputSurface = document.getElementById('video-surface');
const topbarHandle = document.getElementById('topbar-handle');
const topbar = document.getElementById('topbar');
const settingsButton = document.getElementById('settings-button');
const mouseToggleButton = document.getElementById('mouse-toggle-button');
const settingsPanel = document.getElementById('settings-panel');
const pasteButton = document.getElementById('paste-button');
const pastePanel = document.getElementById('paste-panel');
const frameRateSelect = document.getElementById('frame-rate');
const resolutionSelect = document.getElementById('resolution');
const mouseModeSelect = document.getElementById('mouse-mode');
const pasteText = document.getElementById('paste-text');
const saveSettings = document.getElementById('save-settings');
const saveSettingsStatus = document.getElementById('save-settings-status');
const versionEl = document.getElementById('version');

let pc = null;
let controlChannel = null;
let inputChannel = null;
// Whether the capture card / CH9329 are actually connected right now, per
// the server's device_state/hid_state pushes - gates which half of a Save
// gets sent, since there's nothing meaningful to save for a device that
// isn't there.
let captureAvailable = false;
let hidAvailable = false;
// Settings actually confirmed by the server (last `settings` push, on
// connect and after a Save is applied) - never written optimistically on a
// dropdown change or Save click, so live reads of this always reflect what
// the server is actually doing.
let appliedSettings = { width: undefined, height: undefined, fps: undefined, mouse_mode: 'absolute' };
let savePending = false;
// Whether the RTCPeerConnection has actually reached 'connected'. Starts
// false so the bar stays forced open from page load until the connection
// comes up - see openTopbar()/closeTopbar() and the connectionstatechange
// handler in connect().
let rtcConnected = false;
// What the capture card supports (resolutions, and frame rates per
// resolution) - from the last `device_state` push. Needed so switching the
// resolution dropdown *before* Save repopulates the frame-rate dropdown
// from the right list.
let deviceState = emptyDeviceState();
// Whether the settings panel is open - suppresses the topbar's
// outside-click auto-hide (see wireTopbar()) while it's up.
let settingsOpen = false;
// Latest mouse position/movement since the last flush, sent out at most
// once per video frame period instead of on every native mousemove event -
// sending on every event once flooded the CH9329's serial link with enough
// traffic to crash-reboot the target machine. See scheduleMouseMoveFlush().
let pendingAbsoluteMove = null;
let pendingRelativeDx = 0;
let pendingRelativeDy = 0;
let lastMouseMoveButtons = 0;
// Local, unsaved toggle - when off, no mouse event (move, click, or scroll)
// is sent to the target at all. Keyboard input is unaffected.
let mouseEnabled = true;
// Whether the paste flyout is open - suppresses the topbar's outside-click
// auto-hide the same way settingsOpen does (see wireTopbar()).
let pasteOpen = false;
// Whether the pointer is over the topbar - suppresses auto-hide while
// hovering, same as settingsOpen/pasteOpen (see wireTopbar()).
let topbarHovered = false;

function setStatus(text, isError) {
  statusEl.textContent = text;
  statusEl.classList.toggle('error', Boolean(isError));
}

let autoHideTimer = null;

// (Re)starts the 5s auto-hide countdown, but only when the bar is actually
// open, connected, and neither the settings panel nor the paste flyout is
// up - same gating as the outside-click auto-hide in wireTopbar().
function armAutoHide() {
  clearAutoHide();
  if (!topbar.classList.contains('open') || !rtcConnected || settingsOpen || pasteOpen || topbarHovered) return;
  autoHideTimer = setTimeout(closeTopbar, 5000);
}

function clearAutoHide() {
  if (autoHideTimer !== null) {
    clearTimeout(autoHideTimer);
    autoHideTimer = null;
  }
}

function openTopbar() {
  topbar.classList.add('open');
  armAutoHide();
}

function closeTopbar() {
  topbar.classList.remove('open');
  clearAutoHide();
}

function openSettingsPanel() {
  closePastePanel();
  settingsPanel.classList.add('open');
  settingsOpen = true;
  clearAutoHide();
}

function closeSettingsPanel() {
  settingsPanel.classList.remove('open');
  settingsOpen = false;
  armAutoHide();
}

function openPastePanel() {
  closeSettingsPanel();
  pastePanel.classList.add('open');
  pasteOpen = true;
  clearAutoHide();
  pasteText.focus();
}

function closePastePanel() {
  pastePanel.classList.remove('open');
  pasteOpen = false;
  armAutoHide();
}

function emptyDeviceState() {
  return { resolutions: [], default_resolution: null, frame_rates: [] };
}

function sameResolution(a, b) {
  return a && b && a.width === b.width && a.height === b.height;
}

function currentResolutionSelection() {
  const [width, height] = resolutionSelect.value.split('x').map(Number);
  return Number.isNaN(width) || Number.isNaN(height) ? null : { width, height };
}

// Repopulates the resolution dropdown from the capture card's list
// (preferring `preferredResolution` if it's still valid, else the card's
// default), then repopulates the frame-rate dropdown to match whichever
// resolution ends up selected. Called when the server confirms a
// `device_state`/`settings` push.
function refreshCaptureOptions(preferredResolution, preferredFps) {
  const resolution = deviceState.resolutions.some((r) => sameResolution(r, preferredResolution)) ? preferredResolution : deviceState.default_resolution;
  populateResolutions(deviceState.resolutions, resolution);
  refreshFrameRatesForSelection(preferredFps);
}

// Repopulates the frame-rate dropdown from the currently-selected
// resolution. Called whenever the resolution dropdown changes (live,
// before Save) as well as from refreshCaptureOptions above.
function refreshFrameRatesForSelection(preferredFps) {
  const resolution = currentResolutionSelection();
  const entry = deviceState.frame_rates.find((fr) => sameResolution(fr.resolution, resolution));
  const rates = entry ? entry.rates : [];
  const fps = preferredFps !== undefined && rates.includes(preferredFps) ? preferredFps : undefined;
  populateFrameRates(rates, fps);
}

function populateResolutions(resolutions, defaultResolution) {
  resolutionSelect.innerHTML = '';
  if (resolutions.length === 0) {
    const opt = document.createElement('option');
    opt.textContent = 'no video device';
    opt.disabled = true;
    resolutionSelect.appendChild(opt);
    resolutionSelect.disabled = true;
    return;
  }
  resolutionSelect.disabled = false;
  for (const { width, height } of resolutions) {
    const opt = document.createElement('option');
    opt.value = `${width}x${height}`;
    opt.textContent = `${width}x${height}`;
    resolutionSelect.appendChild(opt);
  }
  if (defaultResolution) {
    resolutionSelect.value = `${defaultResolution.width}x${defaultResolution.height}`;
  }
}

function populateFrameRates(frameRates, currentFps) {
  frameRateSelect.innerHTML = '';
  if (frameRates.length === 0) {
    const opt = document.createElement('option');
    opt.textContent = 'no video device';
    opt.disabled = true;
    frameRateSelect.appendChild(opt);
    return;
  }
  for (const fps of frameRates) {
    const opt = document.createElement('option');
    opt.value = fps;
    opt.textContent = `${fps} fps`;
    frameRateSelect.appendChild(opt);
  }
  if (currentFps !== undefined) {
    frameRateSelect.value = currentFps;
  }
}

// Reflects device_state/hid_state availability onto the controls
// themselves - Save already silently drops whichever half isn't
// available (see the click handler below), this just makes that visible.
function updateSettingsAvailability() {
  frameRateSelect.disabled = !captureAvailable;
  resolutionSelect.disabled = !captureAvailable || resolutionSelect.options.length === 0;
  mouseModeSelect.disabled = !hidAvailable;
}

// True if a dropdown's current value differs from appliedSettings (the
// server-confirmed baseline) for whichever half of settings is actually
// available - a device that isn't present has nothing meaningful to save,
// so its dropdowns (which may hold placeholder values) don't count.
function settingsAreDirty() {
  if (captureAvailable) {
    if (Number(frameRateSelect.value) !== appliedSettings.fps) return true;
    const [width, height] = resolutionSelect.value.split('x').map(Number);
    if (width !== appliedSettings.width || height !== appliedSettings.height) return true;
  }
  if (hidAvailable) {
    if (mouseModeSelect.value !== appliedSettings.mouse_mode) return true;
  }
  return false;
}

function updateSaveButtonState() {
  saveSettings.disabled = savePending || !settingsAreDirty();
}

// Bar handle/outside-click wiring - independent of the WebRTC connection
// itself, so the handle works even before/if connect() ever succeeds.
function wireTopbar() {
  topbarHandle.addEventListener('click', () => {
    if (topbar.classList.contains('open')) {
      closeTopbar();
    } else {
      openTopbar();
    }
  });

  document.addEventListener('click', (e) => {
    if (!rtcConnected) return;
    if (settingsOpen || pasteOpen) return;
    if (topbar.contains(e.target) || topbarHandle.contains(e.target) || settingsPanel.contains(e.target) || pastePanel.contains(e.target)) return;
    closeTopbar();
  });

  topbar.addEventListener('mouseenter', () => {
    topbarHovered = true;
    clearAutoHide();
  });

  topbar.addEventListener('mouseleave', () => {
    topbarHovered = false;
    armAutoHide();
  });
}

// Settings panel wiring - gear icon toggles it, a document-level click
// outside the panel/button closes it (same pattern as the paste flyout),
// and each dropdown's change recomputes whether Save should be enabled.
function wireSettingsPanel() {
  settingsButton.addEventListener('click', () => {
    if (settingsOpen) {
      closeSettingsPanel();
    } else {
      openSettingsPanel();
    }
  });

  document.addEventListener('click', (e) => {
    if (!settingsOpen) return;
    if (settingsPanel.contains(e.target) || settingsButton.contains(e.target)) return;
    closeSettingsPanel();
  });

  // Registered before the generic updateSaveButtonState loop below, so the
  // fps list is already repopulated for the newly-selected resolution by
  // the time the dirty-check reads it.
  resolutionSelect.addEventListener('change', () => {
    refreshFrameRatesForSelection(Number(frameRateSelect.value));
  });

  for (const el of [frameRateSelect, resolutionSelect, mouseModeSelect]) {
    el.addEventListener('change', updateSaveButtonState);
  }
}

function onMouseMove(e) {
  if (appliedSettings.mouse_mode === 'absolute') {
    const rect = inputSurface.getBoundingClientRect();
    pendingAbsoluteMove = { xFrac: (e.clientX - rect.left) / rect.width, yFrac: (e.clientY - rect.top) / rect.height };
  } else {
    pendingRelativeDx += e.movementX;
    pendingRelativeDy += e.movementY;
    lastMouseMoveButtons = buttonMask(e.buttons);
  }
}

// Removing the listener outright when the mouse is off (rather than
// leaving it attached and no-op'ing inside) means the highest-frequency
// input event just isn't handled at all while disabled. Written as
// remove-then-conditionally-add so it's idempotent regardless of whether
// it's called from wireInput()'s initial setup or a later toggle.
function updateMouseMoveListener() {
  inputSurface.removeEventListener('mousemove', onMouseMove);
  if (mouseEnabled) inputSurface.addEventListener('mousemove', onMouseMove);
}

// Mouse enable/disable toggle - purely local, nothing sent to the server.
// Turning it off also drops any queued movement so re-enabling doesn't
// flush a stale position/delta from before it was switched off.
function wireMouseToggle() {
  mouseToggleButton.addEventListener('click', () => {
    mouseEnabled = !mouseEnabled;
    updateMouseMoveListener();
    if (!mouseEnabled) {
      pendingAbsoluteMove = null;
      pendingRelativeDx = 0;
      pendingRelativeDy = 0;
    }
    mouseToggleButton.setAttribute('aria-pressed', String(!mouseEnabled));
    mouseToggleButton.setAttribute('aria-label', mouseEnabled ? 'Disable mouse' : 'Enable mouse');
  });
}

// Paste flyout wiring - paste icon toggles it, and a document-level click
// outside the panel/button closes it (same pattern as the settings panel).
function wirePastePanel() {
  pasteButton.addEventListener('click', () => {
    if (pasteOpen) {
      closePastePanel();
    } else {
      openPastePanel();
    }
  });

  document.addEventListener('click', (e) => {
    if (!pasteOpen) return;
    if (pastePanel.contains(e.target) || pasteButton.contains(e.target)) return;
    closePastePanel();
  });
}

async function connect() {
  const { version } = window.SERVER_CONFIG;
  versionEl.textContent = `v${version}`;

  setStatus('connecting…');
  updateSettingsAvailability();
  pc = new RTCPeerConnection();

  pc.addEventListener('connectionstatechange', () => {
    if (pc.connectionState === 'connected') {
      // Auto-hide is only allowed once we're actually connected - see
      // wireTopbar()'s outside-click listener.
      rtcConnected = true;
      armAutoHide();
    } else if (pc.connectionState === 'failed' || pc.connectionState === 'closed' || pc.connectionState === 'disconnected') {
      // Force the bar back open any time we're not actively connected, same
      // as the pre-connect state below - so status/version stay visible.
      rtcConnected = false;
      openTopbar();
    }
    if (pc.connectionState === 'failed' || pc.connectionState === 'closed') {
      setStatus('disconnected - reload the page to reconnect', true);
    } else if (pc.connectionState === 'disconnected') {
      setStatus('connection lost - reload the page to reconnect', true);
    }
  });

  pc.addTransceiver('video', { direction: 'recvonly' });
  pc.addEventListener('track', (event) => {
    videoEl.srcObject = event.streams[0] ?? new MediaStream([event.track]);
  });

  inputChannel = pc.createDataChannel('input', { ordered: false, maxRetransmits: 0 });

  controlChannel = pc.createDataChannel('control');
  controlChannel.binaryType = 'arraybuffer';
  controlChannel.addEventListener('message', (event) => {
    handleServerMessage(JSON.parse(new TextDecoder().decode(event.data)));
  });
  controlChannel.addEventListener('close', () => {
    // Connection dropped mid-save - clear the loading state without
    // claiming success, since no confirmation is coming.
    if (savePending) {
      savePending = false;
      saveSettingsStatus.textContent = '';
      updateSaveButtonState();
    }
  });

  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  await waitForIceGatheringComplete(pc);

  const response = await fetch('/rtc/offer', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ sdp: pc.localDescription.sdp }),
  });
  if (!response.ok) {
    throw new Error(`signaling failed: HTTP ${response.status}`);
  }
  const { sdp } = await response.json();
  await pc.setRemoteDescription({ type: 'answer', sdp });

  setStatus('connected');
  wireInput();
}

function waitForIceGatheringComplete(pc) {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    pc.addEventListener('icegatheringstatechange', function onChange() {
      if (pc.iceGatheringState === 'complete') {
        pc.removeEventListener('icegatheringstatechange', onChange);
        resolve();
      }
    });
  });
}

function handleServerMessage(msg) {
  if (msg.type === 'device_state') {
    captureAvailable = msg.available;
    deviceState = { resolutions: msg.resolutions, default_resolution: msg.default_resolution, frame_rates: msg.frame_rates };
    // Preserve the currently-selected resolution/fps if the new data still
    // supports them (e.g. a hot-plug refresh) - refreshCaptureOptions falls
    // back to the card's default on its own if not.
    refreshCaptureOptions(currentResolutionSelection(), Number(frameRateSelect.value));
    updateSettingsAvailability();
    updateSaveButtonState();
    if (!msg.available) {
      setStatus('no video device found', true);
    } else if (statusEl.textContent === 'no video device found') {
      setStatus('connected');
    }
  } else if (msg.type === 'hid_state') {
    hidAvailable = msg.available;
    updateSettingsAvailability();
    updateSaveButtonState();
  } else if (msg.type === 'settings') {
    appliedSettings = {
      width: msg.capture.resolution.width,
      height: msg.capture.resolution.height,
      fps: msg.capture.fps,
      mouse_mode: msg.mouse_mode,
    };
    refreshCaptureOptions(msg.capture.resolution, msg.capture.fps);
    mouseModeSelect.value = msg.mouse_mode;
    // Drop anything queued under the old mode so a mode switch can't flush a
    // stale absolute position or relative delta under the new one.
    pendingAbsoluteMove = null;
    pendingRelativeDx = 0;
    pendingRelativeDy = 0;
    if (savePending) {
      savePending = false;
      saveSettingsStatus.textContent = 'Saved';
      setTimeout(() => { saveSettingsStatus.textContent = ''; }, 2000);
    }
    // Dropdowns now match appliedSettings (either from this being the
    // confirmation of a Save, or an unrelated push) - recompute so Save
    // re-disables itself rather than staying force-enabled.
    updateSaveButtonState();
  } else if (msg.type === 'offer') {
    handleRenegotiationOffer(msg.sdp).catch((err) => console.error('renegotiation failed:', err));
  }
}

// Applies a server-initiated renegotiation offer (see rtc::Handler::
// on_negotiation_needed) - e.g. the server just added or removed this
// session's video track. Fully separate from the initial connect() offer/
// answer: this never touches input/control, which stay open throughout.
async function handleRenegotiationOffer(sdp) {
  await pc.setRemoteDescription({ type: 'offer', sdp });
  const answer = await pc.createAnswer();
  await pc.setLocalDescription(answer);
  sendControl({ type: 'answer', sdp: pc.localDescription.sdp });
}

function sendInput(bytes) {
  if (!inputChannel || inputChannel.readyState !== 'open') return;
  try {
    inputChannel.send(bytes);
  } catch {
    // Channel not open yet, or closing - drop the event, same tolerance
    // as a dropped datagram.
  }
}

function wireInput() {
  inputSurface.addEventListener('keydown', (e) => {
    e.preventDefault();
    sendKeyEvent(e.code, true);
  });
  inputSurface.addEventListener('keyup', (e) => {
    e.preventDefault();
    sendKeyEvent(e.code, false);
  });

  inputSurface.addEventListener('mousedown', handleMouseButtons);
  inputSurface.addEventListener('mouseup', handleMouseButtons);
  inputSurface.addEventListener('contextmenu', (e) => {
    if (mouseEnabled) e.preventDefault();
  });

  updateMouseMoveListener();
  scheduleMouseMoveFlush();

  inputSurface.addEventListener('wheel', (e) => {
    if (!mouseEnabled) return;
    e.preventDefault();
    const wheel = clampWheel(-e.deltaY);
    if (appliedSettings.mouse_mode === 'absolute') {
      sendMouseButtons(buttonMask(e.buttons), wheel);
    } else {
      sendMouseRelativeMove(buttonMask(e.buttons), 0, 0, wheel);
    }
  });

  pasteText.addEventListener('paste', (e) => {
    e.preventDefault();
    sendControl({ type: 'paste', text: e.clipboardData.getData('text') });
    pasteText.value = '';
  });

  saveSettings.addEventListener('click', () => {
    // Only include the half of the settings a device is actually present
    // for - there's nothing meaningful to save for a device that isn't
    // there (e.g. the resolution dropdown holds a "no video device"
    // placeholder, not a real WxH, when there's no capture card).
    const message = { type: 'update_settings' };
    if (captureAvailable) {
      const [width, height] = resolutionSelect.value.split('x').map(Number);
      message.capture = { width, height, fps: Number(frameRateSelect.value) };
    }
    if (hidAvailable) {
      message.mouse_mode = mouseModeSelect.value;
    }
    sendControl(message);
    savePending = true;
    saveSettings.disabled = true;
    saveSettingsStatus.textContent = 'Saving…';
  });

  function handleMouseButtons(e) {
    if (!mouseEnabled) return;
    if (appliedSettings.mouse_mode === 'absolute') {
      sendMouseButtons(buttonMask(e.buttons), 0);
    } else {
      sendMouseRelativeMove(buttonMask(e.buttons), 0, 0, 0);
    }
  }
}

function buttonMask(domButtons) {
  // DOM MouseEvent.buttons: bit0=left bit1=right bit2=middle — matches
  // ch9329::protocol::button's bit layout directly.
  return domButtons & 0x07;
}

function clampWheel(value) {
  return Math.max(-127, Math.min(127, Math.round(value / 20)));
}

function sendKeyEvent(code, pressed) {
  const codeBytes = new TextEncoder().encode(code);
  const bytes = new Uint8Array(2 + codeBytes.length);
  bytes[0] = TAG_KEY_EVENT;
  bytes[1] = pressed ? 1 : 0;
  bytes.set(codeBytes, 2);
  sendInput(bytes);
}

// Re-arms itself at the current video frame rate (falling back to 30 if
// unknown, e.g. before the server's first settings push) so the flush
// cadence tracks fps live, including across a Save that changes it.
function scheduleMouseMoveFlush() {
  const fps = appliedSettings.fps > 0 ? appliedSettings.fps : 30;
  setTimeout(() => {
    flushMouseMove();
    scheduleMouseMoveFlush();
  }, 1000 / fps);
}

function flushMouseMove() {
  if (appliedSettings.mouse_mode === 'absolute') {
    if (!pendingAbsoluteMove) return;
    sendMouseAbsoluteMove(pendingAbsoluteMove.xFrac, pendingAbsoluteMove.yFrac);
    pendingAbsoluteMove = null;
  } else {
    if (pendingRelativeDx === 0 && pendingRelativeDy === 0) return;
    sendMouseRelativeMove(lastMouseMoveButtons, clampByte(pendingRelativeDx), clampByte(pendingRelativeDy), 0);
    pendingRelativeDx = 0;
    pendingRelativeDy = 0;
  }
}

function sendMouseAbsoluteMove(xFrac, yFrac) {
  const bytes = new Uint8Array(9);
  const view = new DataView(bytes.buffer);
  bytes[0] = TAG_MOUSE_ABSOLUTE_MOVE;
  view.setFloat32(1, xFrac, true);
  view.setFloat32(5, yFrac, true);
  sendInput(bytes);
}

function sendMouseRelativeMove(buttons, dx, dy, wheel) {
  const bytes = new Uint8Array(5);
  bytes[0] = TAG_MOUSE_RELATIVE_MOVE;
  bytes[1] = buttons;
  bytes[2] = clampByte(dx);
  bytes[3] = clampByte(dy);
  bytes[4] = clampByte(wheel);
  sendInput(bytes);
}

function sendMouseButtons(buttons, wheel) {
  const bytes = new Uint8Array(3);
  bytes[0] = TAG_MOUSE_BUTTONS;
  bytes[1] = buttons;
  bytes[2] = clampByte(wheel);
  sendInput(bytes);
}

function clampByte(value) {
  return Math.max(-128, Math.min(127, Math.round(value))) & 0xff;
}

function sendControl(message) {
  if (!controlChannel || controlChannel.readyState !== 'open') return;
  try {
    controlChannel.send(JSON.stringify(message));
  } catch {
    // Channel closing - drop it.
  }
}

// Test-only hook so e2e/browser-test.sh can inspect the connection
// directly (video receiver count, connectionState) via `agent-browser
// eval` - no UI element needed for it.
window.__debugPeerConnection = () => pc;

wireTopbar();
wireSettingsPanel();
wireMouseToggle();
wirePastePanel();
// Forced open pre-connect, same as the disconnected state handled in
// connect()'s connectionstatechange listener above.
openTopbar();

if (typeof RTCPeerConnection === 'undefined') {
  setStatus('this browser has no WebRTC support', true);
} else {
  connect().catch((err) => {
    console.error(err);
    setStatus('failed to connect: ' + err.message, true);
  });
}
