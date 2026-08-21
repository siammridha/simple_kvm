// simple_kvm client: reads bootstrap data (WebTransport port, version) from
// window.SERVER_CONFIG, embedded server-side into the page itself, opens a
// WebTransport session, streams video frames from server-opened
// unidirectional streams onto the canvas, and relays keyboard/mouse input
// back as small binary datagrams. One bidirectional stream carries settings
// updates and paste text as JSON-lines, and receives device-state/settings
// pushes from the server the same way.

const TAG_KEY_EVENT = 1;
const TAG_MOUSE_RELATIVE_MOVE = 3;
const TAG_MOUSE_BUTTONS = 4;

const statusEl = document.getElementById('status');
const canvas = document.getElementById('video');
const ctx2d = canvas.getContext('2d');
const videoModeSelect = document.getElementById('video-mode');
const frameRateSelect = document.getElementById('frame-rate');
const resolutionSelect = document.getElementById('resolution');
const mouseModeSelect = document.getElementById('mouse-mode');
const pasteText = document.getElementById('paste-text');
const pasteSend = document.getElementById('paste-send');
const saveSettings = document.getElementById('save-settings');
const saveSettingsStatus = document.getElementById('save-settings-status');
const versionEl = document.getElementById('version');

let controlWriter = null;
let datagramWriter = null;
let videoDecoder = null;
let decoderConfigured = false;
// Whether the capture card / CH9329 are actually connected right now, per
// the server's device_state/hid_state pushes - gates which half of a Save
// gets sent, since there's nothing meaningful to save for a device that
// isn't there.
let captureAvailable = false;
let hidAvailable = false;
// A fresh decoder (or one that just errored) can't decode a delta frame
// until it's seen a keyframe - feeding it one throws and, per WebCodecs,
// permanently closes the decoder. Dropping delta frames until the next
// real keyframe avoids that instead of erroring on every frame forever.
let awaitingKeyframe = true;

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

async function connect() {
  const { webtransportPort, version, certHash } = window.SERVER_CONFIG;
  versionEl.textContent = `v${version}`;

  const url = `https://${location.hostname}:${webtransportPort}/kvm`;

  setStatus('connecting…');
  const transport = new WebTransport(url, {
    serverCertificateHashes: [{ algorithm: 'sha-256', value: new Uint8Array(certHash) }],
    allowPooling: false,
  });

  transport.closed
    .then(() => {
      setStatus('disconnected - reload the page to reconnect', true);
    })
    .catch(() => {
      setStatus('connection lost - reload the page to reconnect', true);
    });

  await transport.ready;
  setStatus('connected');

  datagramWriter = transport.datagrams.writable.getWriter();

  const controlStream = await transport.createBidirectionalStream();
  controlWriter = controlStream.writable.getWriter();
  readControlReplies(controlStream.readable);

  readVideoStreams(transport);
  wireInput();
}

async function readControlReplies(readable) {
  const reader = readable.getReader();
  const decoder = new TextDecoder();
  let buf = '';
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let newlineIndex;
      while ((newlineIndex = buf.indexOf('\n')) !== -1) {
        const line = buf.slice(0, newlineIndex);
        buf = buf.slice(newlineIndex + 1);
        if (line) handleServerMessage(JSON.parse(line));
      }
    }
  } catch {
    // Session ending; transport.closed's handler above already updates the status.
  }
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
  }
}

async function readVideoStreams(transport) {
  const reader = transport.incomingUnidirectionalStreams.getReader();
  while (true) {
    const { value: stream, done } = await reader.read();
    if (done) break;
    handleVideoStream(stream).catch((err) => console.error('video stream error', err));
  }
}

async function handleVideoStream(stream) {
  const chunks = [];
  let total = 0;
  const reader = stream.getReader();
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    chunks.push(value);
    total += value.length;
  }
  const bytes = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.length;
  }
  if (bytes.length === 0) return;

  const kind = bytes[0];
  const payload = bytes.subarray(1);
  if (kind === 0) {
    await renderJpeg(payload);
  } else {
    renderH264(payload);
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

function naluIsKeyframe(bytes) {
  // Scans Annex-B start codes for an IDR (type 5) or SPS (type 7) NAL.
  for (let i = 0; i + 4 <= bytes.length; i++) {
    if (bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1) {
      const nalType = bytes[i + 3] & 0x1f;
      if (nalType === 5 || nalType === 7) return true;
    }
  }
  return false;
}

function resetH264Decoder() {
  if (videoDecoder && videoDecoder.state !== 'closed') {
    try {
      videoDecoder.close();
    } catch {
      // Already closing/closed - nothing to do.
    }
  }
  videoDecoder = null;
  decoderConfigured = false;
  awaitingKeyframe = true;
}

function renderH264(payload) {
  if (typeof VideoDecoder === 'undefined') {
    setStatus('this browser has no WebCodecs support for H.264', true);
    return;
  }
  if (!videoDecoder) {
    videoDecoder = new VideoDecoder({
      output: (frame) => {
        if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
        }
        ctx2d.drawImage(frame, 0, 0);
        frame.close();
      },
      error: (err) => {
        console.error('VideoDecoder error', err);
        resetH264Decoder();
      },
    });
    decoderConfigured = false;
  }
  if (!decoderConfigured) {
    videoDecoder.configure({ codec: 'avc1.42E01E', avc: { format: 'annexb' }, optimizeForLatency: true });
    decoderConfigured = true;
  }

  const isKeyframe = naluIsKeyframe(payload);
  if (awaitingKeyframe && !isKeyframe) {
    return;
  }
  awaitingKeyframe = false;

  try {
    const type = isKeyframe ? 'key' : 'delta';
    videoDecoder.decode(new EncodedVideoChunk({ type, timestamp: performance.now() * 1000, data: payload }));
  } catch (err) {
    console.error('VideoDecoder decode failed', err);
    resetH264Decoder();
  }
}

function sendDatagram(bytes) {
  datagramWriter.write(bytes).catch(() => {});
}

function wireInput() {
  canvas.addEventListener('keydown', (e) => {
    e.preventDefault();
    sendKeyEvent(e.code, true);
  });
  canvas.addEventListener('keyup', (e) => {
    e.preventDefault();
    sendKeyEvent(e.code, false);
  });

  canvas.addEventListener('mousedown', handleMouseButtons);
  canvas.addEventListener('mouseup', handleMouseButtons);
  canvas.addEventListener('contextmenu', (e) => e.preventDefault());

  canvas.addEventListener('wheel', (e) => {
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
  sendDatagram(bytes);
}

function sendMouseRelativeMove(buttons, dx, dy, wheel) {
  const bytes = new Uint8Array(5);
  bytes[0] = TAG_MOUSE_RELATIVE_MOVE;
  bytes[1] = buttons;
  bytes[2] = clampByte(dx);
  bytes[3] = clampByte(dy);
  bytes[4] = clampByte(wheel);
  sendDatagram(bytes);
}

function sendMouseButtons(buttons, wheel) {
  const bytes = new Uint8Array(3);
  bytes[0] = TAG_MOUSE_BUTTONS;
  bytes[1] = buttons;
  bytes[2] = clampByte(wheel);
  sendDatagram(bytes);
}

function clampByte(value) {
  return Math.max(-128, Math.min(127, Math.round(value))) & 0xff;
}

function sendControl(message) {
  if (!controlWriter) return;
  const bytes = new TextEncoder().encode(JSON.stringify(message) + '\n');
  controlWriter.write(bytes).catch(() => {});
}

if (typeof WebTransport === 'undefined') {
  setStatus('this browser has no WebTransport support', true);
} else {
  connect().catch((err) => {
    console.error(err);
    setStatus('failed to connect: ' + err.message, true);
  });
}
