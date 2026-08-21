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

function setStatus(text, isError) {
  statusEl.textContent = text;
  statusEl.classList.toggle('error', Boolean(isError));
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
  const showVideoEl = videoModeSelect.value === 'h264';
  videoEl.style.display = showVideoEl ? '' : 'none';
  canvas.style.display = showVideoEl ? 'none' : '';
}

async function connect() {
  const { version } = window.SERVER_CONFIG;
  versionEl.textContent = `v${version}`;

  setStatus('connecting…');
  const pc = new RTCPeerConnection();

  pc.addEventListener('connectionstatechange', () => {
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
    if (!msg.available) {
      setStatus('no video device found', true);
    } else if (statusEl.textContent === 'no video device found') {
      setStatus('connected');
    }
  } else if (msg.type === 'hid_state') {
    hidAvailable = msg.available;
  } else if (msg.type === 'settings') {
    videoModeSelect.value = msg.capture.video_mode;
    frameRateSelect.value = msg.capture.fps;
    resolutionSelect.value = `${msg.capture.resolution.width}x${msg.capture.resolution.height}`;
    mouseModeSelect.value = msg.mouse_mode;
    updateVideoElementVisibility();
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
    if (mouseModeSelect.value === 'absolute') {
      sendMouseButtons(buttonMask(e.buttons), wheel);
    } else {
      sendMouseRelativeMove(buttonMask(e.buttons), 0, 0, wheel);
    }
  });

  pasteSend.addEventListener('click', () => {
    sendControl({ type: 'paste', text: pasteText.value });
    pasteText.value = '';
  });

  videoModeSelect.addEventListener('change', updateVideoElementVisibility);

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
    saveSettingsStatus.textContent = 'Saved';
    setTimeout(() => { saveSettingsStatus.textContent = ''; }, 2000);
  });

  function handleMouseButtons(e) {
    if (mouseModeSelect.value === 'absolute') {
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

if (typeof RTCPeerConnection === 'undefined') {
  setStatus('this browser has no WebRTC support', true);
} else {
  connect().catch((err) => {
    console.error(err);
    setStatus('failed to connect: ' + err.message, true);
  });
}
