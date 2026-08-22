// simple_kvm client: opens an RTCPeerConnection over plain HTTP signaling
// (POST /rtc/offer, no trickle ICE — see connect()), receives H.264 video
// as a native WebRTC track rendered into a <video> element, and receives
// MJPEG frames as blobs on a data channel drawn onto a <canvas>. Keyboard
// and mouse input goes out as small binary messages on an
// unreliable/unordered data channel, and settings updates/paste text go
// out as JSON on a reliable data channel, which also carries
// device-state/settings pushes back from the server.

const TAG_KEY_EVENT = 1;
const TAG_MOUSE_RELATIVE_MOVE = 3;
const TAG_MOUSE_BUTTONS = 4;

const statusEl = document.getElementById('status');
const canvas = document.getElementById('video');
const ctx2d = canvas.getContext('2d');
const videoEl = document.getElementById('video-el');
const inputSurface = document.getElementById('video-surface');
const topbarHandle = document.getElementById('topbar-handle');
const topbar = document.getElementById('topbar');
const settingsButton = document.getElementById('settings-button');
const settingsModal = document.getElementById('settings-modal');
const pasteButton = document.getElementById('paste-button');
const pastePanel = document.getElementById('paste-panel');
const videoModeSelect = document.getElementById('video-mode');
const frameRateSelect = document.getElementById('frame-rate');
const resolutionSelect = document.getElementById('resolution');
const mouseModeSelect = document.getElementById('mouse-mode');
const pasteText = document.getElementById('paste-text');
const pasteSend = document.getElementById('paste-send');
const saveSettings = document.getElementById('save-settings');
const saveSettingsStatus = document.getElementById('save-settings-status');
const versionEl = document.getElementById('version');

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
let appliedSettings = { video_mode: 'mjpeg', width: undefined, height: undefined, fps: undefined, mouse_mode: 'absolute' };
let savePending = false;
// Whether the RTCPeerConnection has actually reached 'connected'. Starts
// false so the bar stays forced open from page load until the connection
// comes up - see openTopbar()/closeTopbar() and the connectionstatechange
// handler in connect().
let rtcConnected = false;
// Whether the settings modal is open - suppresses the topbar's
// outside-click auto-hide (see wireTopbar()) while it's up.
let settingsOpen = false;
// Whether the paste flyout is open - suppresses the topbar's outside-click
// auto-hide the same way settingsOpen does (see wireTopbar()).
let pasteOpen = false;

function setStatus(text, isError) {
  statusEl.textContent = text;
  statusEl.classList.toggle('error', Boolean(isError));
}

function openTopbar() {
  topbar.classList.add('open');
}

function closeTopbar() {
  topbar.classList.remove('open');
}

function openSettingsModal() {
  settingsModal.classList.add('open');
  settingsOpen = true;
}

function closeSettingsModal() {
  settingsModal.classList.remove('open');
  settingsOpen = false;
}

function openPastePanel() {
  pastePanel.classList.add('open');
  pasteOpen = true;
}

function closePastePanel() {
  pastePanel.classList.remove('open');
  pasteOpen = false;
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

function updateVideoElementVisibility() {
  const showVideoEl = appliedSettings.video_mode === 'h264';
  videoEl.style.display = showVideoEl ? '' : 'none';
  canvas.style.display = showVideoEl ? 'none' : '';
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
  videoModeSelect.disabled = !captureAvailable;
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
    if (videoModeSelect.value !== appliedSettings.video_mode) return true;
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
    openTopbar();
  });

  document.addEventListener('click', (e) => {
    if (!rtcConnected) return;
    if (settingsOpen || pasteOpen) return;
    if (topbar.contains(e.target) || topbarHandle.contains(e.target) || settingsModal.contains(e.target) || pastePanel.contains(e.target)) return;
    closeTopbar();
  });
}

// Settings modal wiring - gear icon toggles it, backdrop click closes it,
// and each dropdown's change recomputes whether Save should be enabled.
function wireSettingsModal() {
  settingsButton.addEventListener('click', () => {
    if (settingsOpen) {
      closeSettingsModal();
    } else {
      openSettingsModal();
    }
  });

  settingsModal.addEventListener('click', (e) => {
    if (e.target === settingsModal) {
      closeSettingsModal();
    }
  });

  for (const el of [videoModeSelect, frameRateSelect, resolutionSelect, mouseModeSelect]) {
    el.addEventListener('change', updateSaveButtonState);
  }
}

// Paste flyout wiring - paste icon toggles it, and (since it's a small
// fixed panel rather than a full-screen backdrop like the settings modal)
// a document-level click outside the panel/button closes it.
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
  const pc = new RTCPeerConnection();

  pc.addEventListener('connectionstatechange', () => {
    if (pc.connectionState === 'connected') {
      // Auto-hide is only allowed once we're actually connected - see
      // wireTopbar()'s outside-click listener.
      rtcConnected = true;
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

  const mjpegChannel = pc.createDataChannel('mjpeg', { ordered: false, maxRetransmits: 0 });
  mjpegChannel.binaryType = 'arraybuffer';
  mjpegChannel.addEventListener('message', (event) => {
    renderJpeg(event.data).catch((err) => console.error('failed to render MJPEG frame', err));
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
  updateVideoElementVisibility();
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
    populateResolutions(msg.resolutions, msg.default_resolution);
    populateFrameRates(msg.frame_rates, undefined);
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
      video_mode: msg.capture.video_mode,
      width: msg.capture.resolution.width,
      height: msg.capture.resolution.height,
      fps: msg.capture.fps,
      mouse_mode: msg.mouse_mode,
    };
    videoModeSelect.value = msg.capture.video_mode;
    frameRateSelect.value = msg.capture.fps;
    resolutionSelect.value = `${msg.capture.resolution.width}x${msg.capture.resolution.height}`;
    mouseModeSelect.value = msg.mouse_mode;
    updateVideoElementVisibility();
    if (savePending) {
      savePending = false;
      saveSettingsStatus.textContent = 'Saved';
      setTimeout(() => { saveSettingsStatus.textContent = ''; }, 2000);
    }
    // Dropdowns now match appliedSettings (either from this being the
    // confirmation of a Save, or an unrelated push) - recompute so Save
    // re-disables itself rather than staying force-enabled.
    updateSaveButtonState();
  }
}

async function renderJpeg(payload) {
  const bitmap = await createImageBitmap(new Blob([payload], { type: 'image/jpeg' }));
  if (canvas.width !== bitmap.width || canvas.height !== bitmap.height) {
    canvas.width = bitmap.width;
    canvas.height = bitmap.height;
  }
  ctx2d.drawImage(bitmap, 0, 0);
  bitmap.close();
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
  inputSurface.addEventListener('contextmenu', (e) => e.preventDefault());

  inputSurface.addEventListener('wheel', (e) => {
    e.preventDefault();
    const wheel = clampWheel(-e.deltaY);
    if (appliedSettings.mouse_mode === 'absolute') {
      sendMouseButtons(buttonMask(e.buttons), wheel);
    } else {
      sendMouseRelativeMove(buttonMask(e.buttons), 0, 0, wheel);
    }
  });

  pasteSend.addEventListener('click', () => {
    sendControl({ type: 'paste', text: pasteText.value });
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
      message.capture = { video_mode: videoModeSelect.value, width, height, fps: Number(frameRateSelect.value) };
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

wireTopbar();
wireSettingsModal();
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
