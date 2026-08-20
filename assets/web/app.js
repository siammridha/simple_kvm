// simple_kvm client: fetches config + the current TLS cert hash, opens a
// WebTransport session pinned to that hash, streams video frames from
// server-opened unidirectional streams onto the canvas, and relays
// keyboard/mouse input back as small binary datagrams. One bidirectional
// stream carries settings changes and paste text as JSON-lines.

const TAG_KEY_EVENT = 1;
const TAG_MOUSE_RELATIVE_MOVE = 3;
const TAG_MOUSE_BUTTONS = 4;

const statusEl = document.getElementById('status');
const canvas = document.getElementById('video');
const ctx2d = canvas.getContext('2d');
const videoModeSelect = document.getElementById('video-mode');
const resolutionSelect = document.getElementById('resolution');
const mouseModeSelect = document.getElementById('mouse-mode');
const pasteText = document.getElementById('paste-text');
const pasteSend = document.getElementById('paste-send');
const saveSettings = document.getElementById('save-settings');
const saveSettingsStatus = document.getElementById('save-settings-status');

let controlWriter = null;
let datagramWriter = null;
let videoDecoder = null;
let decoderConfigured = false;
// A fresh decoder (or one that just errored) can't decode a delta frame
// until it's seen a keyframe - feeding it one throws and, per WebCodecs,
// permanently closes the decoder. Dropping delta frames until the next
// real keyframe avoids that instead of erroring on every frame forever.
let awaitingKeyframe = true;
let reconnectScheduled = false;

function setStatus(text, isError) {
  statusEl.textContent = text;
  statusEl.classList.toggle('error', Boolean(isError));
}

// Every failure path below funnels here instead of calling connect()
// directly, so a failed reconnect attempt is always caught (an uncaught
// rejection from a bare `setTimeout(connect, ...)` would otherwise surface
// as a page error on every retry) and so concurrent failures don't stack
// up multiple parallel retry loops.
function scheduleReconnect() {
  if (reconnectScheduled) return;
  reconnectScheduled = true;
  setTimeout(() => {
    reconnectScheduled = false;
    connect().catch((err) => {
      console.error(err);
      setStatus('failed to connect: ' + err.message, true);
      scheduleReconnect();
    });
  }, 1000);
}

async function loadConfig() {
  const res = await fetch('/api/config');
  return res.json();
}

async function loadCertInfo() {
  const res = await fetch('/api/cert-info');
  return res.json();
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
  const config = await loadConfig();
  populateResolutions(config.resolutions, config.default_resolution);
  videoModeSelect.value = config.video_mode;
  mouseModeSelect.value = config.mouse_mode;

  if (!config.video_available) {
    setStatus('no video device found', true);
  }

  const certInfo = await loadCertInfo();
  const hashBytes = Uint8Array.from(JSON.parse(certInfo.hash));
  const url = `https://${location.hostname}:${config.webtransport_port}/kvm`;

  setStatus('connecting…');
  const transport = new WebTransport(url, {
    serverCertificateHashes: [{ algorithm: 'sha-256', value: hashBytes }],
  });

  transport.closed
    .then(() => {
      setStatus('disconnected, reconnecting…', true);
      scheduleReconnect();
    })
    .catch(() => {
      setStatus('connection lost, reconnecting…', true);
      scheduleReconnect();
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
  // The server doesn't currently send anything back on the control
  // stream, but draining it keeps the stream from backing up.
  const reader = readable.getReader();
  try {
    while (true) {
      const { done } = await reader.read();
      if (done) break;
    }
  } catch {
    // Session ending; the top-level reconnect logic handles it.
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

  videoModeSelect.addEventListener('change', () => sendControl({ type: 'set_video_mode', mode: videoModeSelect.value }));
  resolutionSelect.addEventListener('change', () => {
    const [width, height] = resolutionSelect.value.split('x').map(Number);
    sendControl({ type: 'set_resolution', width, height });
  });
  mouseModeSelect.addEventListener('change', () => sendControl({ type: 'set_mouse_mode', mode: mouseModeSelect.value }));
  pasteSend.addEventListener('click', () => {
    sendControl({ type: 'paste', text: pasteText.value });
    pasteText.value = '';
  });

  saveSettings.addEventListener('click', async () => {
    try {
      const res = await fetch('/api/settings/save', { method: 'POST' });
      saveSettingsStatus.textContent = res.ok ? 'Saved' : 'Save failed';
    } catch (err) {
      console.error('save settings failed', err);
      saveSettingsStatus.textContent = 'Save failed';
    }
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
    scheduleReconnect();
  });
}
