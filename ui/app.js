/*
 * MONITOR 取景器 —— 前端主控。
 *
 * 纯静态 ES 模块，无构建步骤：
 *   - 全部后端能力走 window.__TAURI__（withGlobalTauri: true）
 *   - 画面走 fetch + ReadableStream 的 MJPEG 解析（见 mjpeg.js）
 *   - 布局/比例换算见 layout.js，格式化见 format.js
 */

import { computeLayout, PRESETS, MODES, resolveRatio } from './layout.js';
import { formatClock, formatSecs, formatFps, formatResolution, formatBytes, shortPath, friendlyError, parseRatioInput } from './format.js';
import { parseMjpegStream } from './mjpeg.js';

/* ------------------------------------------------------------------ */
/* 基础状态                                                            */
/* ------------------------------------------------------------------ */

const tauri = window.__TAURI__ || null;

const dom = {};

const state = {
  info: null,
  devices: { av: [], android: [], ffmpegOk: true, adbOk: true },
  deviceError: '',
  source: null,
  gen: 0,
  status: 'idle',
  statusDetail: '',
  frame: { width: 0, height: 0, fps: 0 },
  ratioId: 'source',
  ratio: null,
  customW: '16',
  customH: '9',
  mode: 'fit',
  mirror: false,
  grid: true,
  recording: false,
  recStartedAt: 0,
  recSecs: 0,
  recName: '',
  captures: [],
  capturesDir: '',
  activeCapture: null,
  lastBitmap: null,
  layout: null,
  pendingFrame: null,
  decoding: false,
  streamAbort: null,
  streamToken: 0,
  reconnectTimer: 0,
  streamFailures: 0,
  decodeFailures: 0,
  deleteArmed: '',
  busy: { devices: false, photo: false, record: false, pair: false, viewer: false },
  wireless: { open: false, connected: false, url: '' },
  timers: { clock: 0, rec: 0, refresh: 0, ratio: 0, deleteArm: 0 },
};

/* ------------------------------------------------------------------ */
/* Tauri 桥                                                            */
/* ------------------------------------------------------------------ */

function hasBridge() {
  return !!(tauri && tauri.core && typeof tauri.core.invoke === 'function');
}

function invoke(command, args) {
  if (!hasBridge()) {
    return Promise.reject(new Error('请通过 MONITOR 桌面应用打开取景器'));
  }
  return args === undefined ? tauri.core.invoke(command) : tauri.core.invoke(command, args);
}

function listen(eventName, handler) {
  if (!tauri || !tauri.event || typeof tauri.event.listen !== 'function') return Promise.resolve(function () {});
  return tauri.event.listen(eventName, function (event) { handler(event && event.payload); }).catch(function () {
    return function () {};
  });
}

/* ------------------------------------------------------------------ */
/* DOM 辅助                                                            */
/* ------------------------------------------------------------------ */

function $(id) {
  return document.getElementById(id);
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

function setText(node, text) {
  if (node) node.textContent = text === undefined || text === null ? '' : String(text);
}

function setHidden(node, hidden) {
  if (!node) return;
  if (hidden) node.setAttribute('hidden', '');
  else node.removeAttribute('hidden');
}

/* ------------------------------------------------------------------ */
/* 状态胶囊 / 空态 / HUD                                                */
/* ------------------------------------------------------------------ */

const STATUS_TEXT = {
  idle: '未连接',
  connecting: '连接中…',
  live: '实时',
  error: '错误',
};

function setStatus(status, detail) {
  state.status = STATUS_TEXT[status] ? status : 'idle';
  state.statusDetail = detail ? String(detail) : '';
  applyStatus();
}

function applyStatus() {
  const pill = dom.statusPill;
  if (pill) {
    pill.setAttribute('data-state', state.status);
    setText(dom.statusText, STATUS_TEXT[state.status] || '未连接');
    pill.setAttribute('title', state.statusDetail || STATUS_TEXT[state.status] || '');
  }

  const busy = state.status === 'connecting';
  if (dom.stage) dom.stage.classList.toggle('is-busy', busy);
  if (dom.stageEmpty) setHidden(dom.stageEmpty, state.status === 'live' && !!state.lastBitmap);

  let title = '还没有画面';
  let detail = '在左侧选择一个设备开始取景。';
  if (state.status === 'connecting') {
    title = '正在连接…';
    detail = state.statusDetail || '正在等待设备画面，请稍候。';
  } else if (state.status === 'error') {
    title = '画面中断';
    detail = state.statusDetail || '请重新选择设备，或点击「刷新」重新枚举。';
  } else if (state.status === 'live' && !state.lastBitmap) {
    title = '等待第一帧…';
    detail = '设备已连接，正在接收画面。';
  }
  setText(dom.stageEmptyTitle, title);
  setText(dom.stageEmptyDetail, detail);

  const live = state.status === 'live';
  if (dom.btnPhoto) dom.btnPhoto.disabled = !live;
  if (dom.btnRecord) dom.btnRecord.disabled = !live && !state.recording;
  updateRecordingUi();
}

function updateHud() {
  setText(dom.hudClock, formatClock(new Date()));
  setText(dom.hudRes, formatResolution(state.frame.width, state.frame.height));
  setText(dom.hudFps, formatFps(state.frame.fps));
  if (dom.deviceName) {
    setText(dom.deviceName, state.source ? state.source.label : '未选择设备');
  }
  if (dom.ratioValue) {
    setText(dom.ratioValue, ratioLabel());
  }
}

function ratioLabel() {
  const preset = PRESETS.find(function (item) { return item.id === state.ratioId; });
  if (state.ratioId === 'custom') {
    const parsed = parseRatioInput(state.customW, state.customH);
    return parsed ? parsed[0] + ':' + parsed[1] : '自定义';
  }
  return preset ? preset.label : '跟随源';
}

/* ------------------------------------------------------------------ */
/* 画布绘制                                                            */
/* ------------------------------------------------------------------ */

function canvasBox() {
  const canvas = dom.view;
  if (!canvas) return { width: 1, height: 1 };
  const rect = canvas.getBoundingClientRect();
  return { width: Math.max(1, rect.width), height: Math.max(1, rect.height) };
}

function sourceSize() {
  const bitmap = state.lastBitmap;
  const width = state.frame.width > 0 ? state.frame.width : bitmap ? bitmap.width : 16;
  const height = state.frame.height > 0 ? state.frame.height : bitmap ? bitmap.height : 9;
  return { width: Math.max(1, width), height: Math.max(1, height) };
}

function paint() {
  const canvas = dom.view;
  if (!canvas) return;
  const box = canvasBox();
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const pixelWidth = Math.max(1, Math.round(box.width * dpr));
  const pixelHeight = Math.max(1, Math.round(box.height * dpr));
  if (canvas.width !== pixelWidth) canvas.width = pixelWidth;
  if (canvas.height !== pixelHeight) canvas.height = pixelHeight;

  const ctx = canvas.getContext('2d');
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.fillStyle = '#000';
  ctx.fillRect(0, 0, box.width, box.height);

  const source = sourceSize();
  const layout = computeLayout(source.width, source.height, box.width, box.height, state.ratio, state.mode);
  state.layout = layout;

  const bitmap = state.lastBitmap;
  if (bitmap) {
    const bitmapWidth = bitmap.width || source.width;
    const bitmapHeight = bitmap.height || source.height;
    const sx = layout.uv.x * bitmapWidth;
    const sy = layout.uv.y * bitmapHeight;
    const sw = Math.max(1, layout.uv.w * bitmapWidth);
    const sh = Math.max(1, layout.uv.h * bitmapHeight);
    if (state.mirror) {
      ctx.save();
      ctx.translate(layout.dest.x + layout.dest.w, layout.dest.y);
      ctx.scale(-1, 1);
      ctx.drawImage(bitmap, sx, sy, sw, sh, 0, 0, layout.dest.w, layout.dest.h);
      ctx.restore();
    } else {
      ctx.drawImage(bitmap, sx, sy, sw, sh, layout.dest.x, layout.dest.y, layout.dest.w, layout.dest.h);
    }
  }

  layoutOverlays(layout, box.width, box.height, !!bitmap);
}

function layoutOverlays(layout, width, height, hasFrame) {
  if (dom.hud) {
    dom.hud.style.left = Math.round(layout.dest.x + 10) + 'px';
    dom.hud.style.top = Math.round(layout.dest.y + 10) + 'px';
    dom.hud.classList.toggle('is-dim', !hasFrame);
  }
  if (dom.recBadge) {
    dom.recBadge.style.top = Math.round(layout.dest.y + 10) + 'px';
    dom.recBadge.style.right = Math.round(Math.max(0, width - (layout.dest.x + layout.dest.w)) + 10) + 'px';
    dom.recBadge.classList.toggle('is-dim', !hasFrame);
  }
  if (dom.grid) {
    dom.grid.style.left = layout.dest.x + 'px';
    dom.grid.style.top = layout.dest.y + 'px';
    dom.grid.style.width = layout.dest.w + 'px';
    dom.grid.style.height = layout.dest.h + 'px';
    dom.grid.classList.toggle('is-on', state.grid && hasFrame);
  }
}

function closeBitmap() {
  const bitmap = state.lastBitmap;
  state.lastBitmap = null;
  if (bitmap && typeof bitmap.close === 'function') {
    try { bitmap.close(); } catch (err) { /* 已释放 */ }
  }
}

function resetFrameState() {
  state.pendingFrame = null;
  state.decodeFailures = 0;
  closeBitmap();
  state.frame = { width: 0, height: 0, fps: 0 };
  updateHud();
  paint();
}

/* ------------------------------------------------------------------ */
/* MJPEG 客户端                                                        */
/* ------------------------------------------------------------------ */

function stopStream() {
  if (state.streamAbort) {
    try { state.streamAbort.abort(); } catch (err) { /* 已断开 */ }
    state.streamAbort = null;
  }
  state.streamToken += 1;
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = 0;
}

function connectStream() {
  if (!state.source) return;
  if (!state.info || !state.info.streamUrl) return;
  stopStream();
  const token = state.streamToken;
  const controller = new AbortController();
  state.streamAbort = controller;
  const base = state.info.streamUrl;
  const url = base + (base.indexOf('?') >= 0 ? '&' : '?') + 't=' + Date.now();

  fetch(url, { cache: 'no-store', signal: controller.signal })
    .then(function (response) {
      if (!response.ok) throw new Error('画面服务返回 ' + response.status);
      return parseMjpegStream(response, { onFrame: queueFrame, signal: controller.signal });
    })
    .catch(function (err) {
      if (controller.signal.aborted) return;
      if (err && err.name === 'AbortError') return;
      state.streamFailures += 1;
      if (state.streamFailures === 6 && state.status === 'live') {
        toast('画面流断开，正在自动重连…', 'error');
      }
    })
    .finally(function () {
      if (token !== state.streamToken) return;
      state.streamAbort = null;
      scheduleReconnect(token);
    });
}

function scheduleReconnect(token) {
  if (!state.source) return;
  clearTimeout(state.reconnectTimer);
  state.reconnectTimer = setTimeout(function () {
    if (token !== state.streamToken) return;
    if (!state.source) return;
    connectStream();
  }, 300);
}

function queueFrame(bytes) {
  if (!bytes || !bytes.length) return;
  state.pendingFrame = bytes;
  if (!state.decoding) pumpFrames();
}

function pumpFrames() {
  state.decoding = true;
  (async function () {
    try {
      while (state.pendingFrame) {
        const bytes = state.pendingFrame;
        state.pendingFrame = null;
        const bitmap = await decodeFrame(bytes);
        if (!bitmap) {
          state.decodeFailures += 1;
          continue;
        }
        state.decodeFailures = 0;
        state.streamFailures = 0;
        closeBitmap();
        state.lastBitmap = bitmap;
        if (state.status === 'connecting') setStatus('live');
        paint();
        if (dom.stageEmpty) setHidden(dom.stageEmpty, true);
      }
    } finally {
      state.decoding = false;
    }
  })();
}

async function decodeFrame(bytes) {
  const blob = new Blob([bytes], { type: 'image/jpeg' });
  if (typeof window.createImageBitmap === 'function') {
    try {
      return await window.createImageBitmap(blob);
    } catch (err) {
      /* 解码失败：跳过这一帧 */
    }
  }
  return await new Promise(function (resolve) {
    const url = URL.createObjectURL(blob);
    const image = new Image();
    image.onload = function () {
      URL.revokeObjectURL(url);
      resolve(image);
    };
    image.onerror = function () {
      URL.revokeObjectURL(url);
      resolve(null);
    };
    image.src = url;
  });
}

/* ------------------------------------------------------------------ */
/* 设备面板                                                            */
/* ------------------------------------------------------------------ */

function deviceGroup(title) {
  const group = el('div', 'device-group');
  group.append(el('p', 'device-group-title', title));
  return group;
}

function deviceButton(item) {
  const button = el('button', 'device-item' + (item.kind === 'wireless' ? ' device-wireless' : ''));
  button.type = 'button';
  button.dataset.kind = item.kind;
  button.dataset.id = item.id;
  button.append(el('span', 'device-dot'), el('span', 'device-label', item.label));
  if (item.meta) button.append(el('span', 'device-meta', item.meta));
  const active = !item.wireless && state.source && state.source.kind === item.kind && state.source.id === item.id;
  button.setAttribute('aria-pressed', active ? 'true' : 'false');
  if (item.disabled) button.disabled = true;
  button.addEventListener('click', function () {
    if (item.wireless) openWireless();
    else selectSource(item.kind, item.id, item.label);
  });
  return button;
}

function renderDevices() {
  if (!dom.deviceList) return;
  dom.deviceList.textContent = '';

  const av = (state.devices.av || []).map(function (camera) {
    return { kind: 'av', id: String(camera.index), label: camera.name || '摄像头 ' + camera.index, meta: '' };
  });
  const ios = [];
  const android = [];
  (state.devices.android || []).forEach(function (device) {
    const model = device.model || 'Android 设备';
    const entry = {
      kind: 'android',
      id: device.serial || '',
      label: model,
      meta: device.serial ? '…' + device.serial.slice(-4) : '',
      disabled: !device.serial,
    };
    if (/iphone|ipad|ios|apple/i.test(model)) ios.push(entry);
    else android.push(entry);
  });

  const avGroup = deviceGroup('本机摄像头');
  if (av.length === 0) avGroup.append(el('p', 'device-empty', '没有检测到摄像头'));
  av.forEach(function (item) { avGroup.append(deviceButton(item)); });
  dom.deviceList.append(avGroup);

  const iosGroup = deviceGroup('iPhone / iPad');
  if (ios.length === 0) iosGroup.append(el('p', 'device-empty', '未连接（USB）'));
  ios.forEach(function (item) { iosGroup.append(deviceButton(item)); });
  dom.deviceList.append(iosGroup);

  const androidGroup = deviceGroup('Android');
  if (android.length === 0) androidGroup.append(el('p', 'device-empty', '未连接（USB 调试）'));
  android.forEach(function (item) { androidGroup.append(deviceButton(item)); });
  dom.deviceList.append(androidGroup);

  const wirelessGroup = deviceGroup('无线');
  wirelessGroup.append(deviceButton({
    kind: 'wireless',
    id: 'phone',
    label: '无线（扫码）',
    meta: 'QR',
    wireless: true,
  }));
  dom.deviceList.append(wirelessGroup);

  if (state.deviceError) {
    dom.deviceList.append(el('p', 'device-empty', state.deviceError));
  }

  if (dom.deviceHint) {
    const hints = [];
    if (state.devices.ffmpegOk === false) hints.push('未检测到 ffmpeg，录像可能不可用。');
    if (state.devices.adbOk === false) hints.push('未检测到 adb，Android 设备可能无法连接。');
    setText(dom.deviceHint, hints.join(' '));
    setHidden(dom.deviceHint, hints.length === 0);
    if (hints.length) dom.deviceHint.setAttribute('data-tone', 'warn');
    else dom.deviceHint.removeAttribute('data-tone');
  }

  updateHud();
}

async function refreshDevices(options) {
  const opts = options || {};
  if (state.busy.devices) return;
  state.busy.devices = true;
  if (dom.btnRefresh && !opts.silent) dom.btnRefresh.disabled = true;
  try {
    const devices = await invoke('list_devices');
    state.devices = {
      av: (devices && devices.av) || [],
      android: (devices && devices.android) || [],
      ffmpegOk: !devices || devices.ffmpegOk !== false,
      adbOk: !devices || devices.adbOk !== false,
    };
    state.deviceError = '';
  } catch (err) {
    state.deviceError = '枚举设备失败：' + friendlyError(err, '请稍后重试');
    if (!opts.silent) toast(friendlyError(err, '枚举设备失败'), 'error');
  } finally {
    state.busy.devices = false;
    if (dom.btnRefresh) dom.btnRefresh.disabled = false;
    renderDevices();
  }
}

/* ------------------------------------------------------------------ */
/* 切换画面源                                                          */
/* ------------------------------------------------------------------ */

async function releaseSource() {
  stopStream();
  resetFrameState();
  const previous = state.source;
  state.source = null;
  state.gen = 0;
  state.recording = false;
  state.recStartedAt = 0;
  state.recSecs = 0;
  updateRecordingUi();
  updateHud();
  if (previous) {
    try { await invoke('stop_source'); } catch (err) { /* 已经停止 */ }
  }
  setStatus('idle');
  renderDevices();
}

async function selectSource(kind, id, label) {
  const same = state.source && state.source.kind === kind && state.source.id === id;
  if (same && (state.status === 'live' || state.status === 'connecting')) return;
  if (state.recording) await stopRecording();
  await releaseSource();
  state.source = { kind: kind, id: id, label: label || '设备' };
  state.gen = 0;
  state.streamFailures = 0;
  setStatus('connecting', '正在启动 ' + (label || '设备') + '…');
  updateHud();
  renderDevices();
  try {
    const result = await invoke('start_source', { kind: kind, id: id });
    if (!state.source || state.source.kind !== kind || state.source.id !== id) return;
    state.gen = result && result.gen ? result.gen : 0;
    connectStream();
  } catch (err) {
    if (!state.source) return;
    setStatus('error', friendlyError(err, '无法打开这个设备'));
    toast(friendlyError(err, '无法打开这个设备'), 'error');
  }
}

/* ------------------------------------------------------------------ */
/* 拍照 / 录像                                                          */
/* ------------------------------------------------------------------ */

function flash() {
  if (!dom.flash) return;
  dom.flash.classList.remove('is-on');
  void dom.flash.offsetWidth;
  dom.flash.classList.add('is-on');
  setTimeout(function () {
    if (dom.flash) dom.flash.classList.remove('is-on');
  }, 320);
}

async function takePhoto() {
  if (state.busy.photo) return;
  if (state.status !== 'live') {
    toast(state.lastBitmap ? '画面还没有就绪，请稍后再试' : '没有可拍摄的画面', 'error');
    return;
  }
  state.busy.photo = true;
  try {
    const result = await invoke('take_photo', { mirror: state.mirror });
    flash();
    addCapture({
      name: result && result.name ? result.name : 'photo-' + Date.now() + '.jpg',
      kind: 'photo',
      path: result && result.path ? result.path : '',
      url: result && result.url ? result.url : '',
      thumbUrl: result && result.thumbUrl ? result.thumbUrl : '',
      at: Date.now(),
    });
    toast('已拍照', 'info');
  } catch (err) {
    toast(friendlyError(err, '拍照失败'), 'error');
  } finally {
    state.busy.photo = false;
  }
}

async function startRecording() {
  if (state.recording || state.busy.record) return;
  if (state.status !== 'live') {
    toast(state.lastBitmap ? '画面还没有就绪，请稍后再试' : '没有可录制的画面', 'error');
    return;
  }
  state.busy.record = true;
  try {
    const result = await invoke('start_recording');
    state.recording = true;
    state.recName = result && result.name ? result.name : '';
    state.recStartedAt = Date.now();
    state.recSecs = 0;
    updateRecordingUi();
  } catch (err) {
    toast(friendlyError(err, '无法开始录像'), 'error');
  } finally {
    state.busy.record = false;
  }
}

async function stopRecording() {
  if (!state.recording || state.busy.record) return;
  state.busy.record = true;
  try {
    const result = await invoke('stop_recording');
    const name = result && result.name ? result.name : state.recName;
    const secs = result && Number.isFinite(Number(result.secs)) ? Number(result.secs) : state.recSecs;
    const path = result && result.path ? result.path : '';
    const thumbUrl = result && result.thumbUrl ? result.thumbUrl : '';
    state.recording = false;
    state.recStartedAt = 0;
    state.recSecs = 0;
    updateRecordingUi();
    if (result && result.error) {
      toast('录像已停止，但文件可能不完整：' + friendlyError(result.error, '未知原因'), 'error');
    } else {
      toast('录像已保存', 'info');
    }
    if (name) {
      addCapture({
        name: name,
        kind: 'video',
        path: path,
        url: '',
        thumbUrl: thumbUrl,
        secs: secs,
        at: Date.now(),
      });
    }
  } catch (err) {
    toast(friendlyError(err, '停止录像失败'), 'error');
  } finally {
    state.busy.record = false;
  }
}

function toggleRecording() {
  if (state.recording) stopRecording();
  else startRecording();
}

function updateRecordingUi() {
  const recording = state.recording;
  if (dom.recTimer) setHidden(dom.recTimer, !recording);
  if (dom.recBadge) setHidden(dom.recBadge, !recording);
  if (dom.btnRecord) {
    dom.btnRecord.classList.toggle('is-recording', recording);
    dom.btnRecord.setAttribute('aria-pressed', recording ? 'true' : 'false');
    dom.btnRecord.disabled = !recording && state.status !== 'live';
  }
  setText(dom.btnRecordLabel, recording ? '停止录像' : '录像');
  setText(dom.recTime, formatSecs(state.recSecs));
  setText(dom.recBadgeTime, formatSecs(state.recSecs));
}

/* ------------------------------------------------------------------ */
/* 胶片条 / 查看器                                                      */
/* ------------------------------------------------------------------ */

function addCapture(item) {
  state.captures.unshift(item);
  if (state.captures.length > 240) state.captures.length = 240;
  renderFilm();
}

function renderFilm() {
  if (!dom.filmList) return;
  dom.filmList.textContent = '';
  setText(dom.filmCount, String(state.captures.length));
  if (dom.filmEmpty) setHidden(dom.filmEmpty, state.captures.length > 0);

  state.captures.forEach(function (item) {
    const shot = el('button', 'shot');
    shot.type = 'button';
    shot.title = item.name + (item.kind === 'video' ? '（录像）' : '（照片）');
    shot.dataset.name = item.name;
    if (state.activeCapture && state.activeCapture.name === item.name) shot.classList.add('is-active');

    const wrap = el('div', 'shot-thumb-wrap');
    const thumb = item.thumbUrl || (item.kind === 'photo' ? item.url : '');
    if (thumb) {
      const image = new Image();
      image.className = 'shot-thumb';
      image.alt = '';
      image.loading = 'lazy';
      image.src = thumb;
      image.addEventListener('error', function () {
        image.remove();
        wrap.append(el('div', 'viewer-missing', item.kind === 'video' ? 'MP4' : 'JPG'));
      });
      wrap.append(image);
    } else {
      wrap.append(el('div', 'viewer-missing', item.kind === 'video' ? 'MP4' : 'JPG'));
    }
    const kind = el('span', 'shot-kind', item.kind === 'video' ? 'REC' : 'JPG');
    kind.dataset.kind = item.kind;
    wrap.append(kind);

    const meta = el('div', 'shot-meta');
    meta.append(el('span', 'shot-clock', formatClock(new Date(item.at || Date.now()))));
    if (item.kind === 'video') meta.append(el('span', 'shot-duration', formatSecs(item.secs || 0)));

    shot.append(wrap, meta);
    shot.addEventListener('click', function () { openViewer(item); });
    dom.filmList.append(shot);
  });
  dom.filmList.scrollLeft = 0;
}

function capturePath(item) {
  if (!item) return '';
  if (item.path) return item.path;
  if (state.capturesDir) return state.capturesDir.replace(/[/]+$/, '') + '/' + item.name;
  return item.name;
}

function mediaUrl(name) {
  if (!name || !state.info || !state.info.streamUrl) return '';
  try {
    const origin = new URL(state.info.streamUrl).origin;
    return origin + '/media/' + encodeURIComponent(name);
  } catch (err) {
    return '';
  }
}

function openViewer(item) {
  if (!item || !dom.viewer) return;
  state.activeCapture = item;
  setText(dom.viewerTitle, item.kind === 'video' ? '查看录像' : '查看照片');
  if (dom.viewerStage) {
    dom.viewerStage.textContent = '';
    const url = item.kind === 'video' ? mediaUrl(item.name) : item.url || mediaUrl(item.name);
    if (!url) {
      dom.viewerStage.append(el('p', 'viewer-missing', '找不到这个文件的位置'));
    } else if (item.kind === 'video') {
      const video = document.createElement('video');
      video.controls = true;
      video.playsInline = true;
      video.src = url;
      if (item.thumbUrl) video.poster = item.thumbUrl;
      video.addEventListener('error', function () {
        dom.viewerStage.textContent = '';
        dom.viewerStage.append(el('p', 'viewer-missing', '视频无法播放，可以尝试用「打开」播放'));
      });
      dom.viewerStage.append(video);
    } else {
      const image = new Image();
      image.alt = item.name;
      image.src = url;
      image.addEventListener('error', function () {
        dom.viewerStage.textContent = '';
        dom.viewerStage.append(el('p', 'viewer-missing', '图片无法显示，可以尝试用「打开」查看'));
      });
      dom.viewerStage.append(image);
    }
  }
  if (dom.viewerMeta) {
    const parts = [item.name];
    if (item.kind === 'video') parts.push('时长 ' + formatSecs(item.secs || 0));
    parts.push(formatClock(new Date(item.at || Date.now())));
    if (item.size) parts.push(formatBytes(item.size));
    setText(dom.viewerMeta, parts.join(' · '));
  }
  setHidden(dom.viewer, false);
  resetDeleteConfirm();
  renderFilm();
  if (dom.btnViewerDelete) dom.btnViewerDelete.disabled = false;
}

function resetDeleteConfirm() {
  state.deleteArmed = '';
  if (dom.btnViewerDelete) {
    setText(dom.btnViewerDelete, '删除');
    dom.btnViewerDelete.classList.remove('is-armed');
  }
}

function closeViewer() {
  if (!dom.viewer) return;
  resetDeleteConfirm();
  const video = dom.viewerStage ? dom.viewerStage.querySelector('video') : null;
  if (video) {
    try { video.pause(); } catch (err) { /* 忽略 */ }
    video.removeAttribute('src');
    video.load();
  }
  if (dom.viewerStage) dom.viewerStage.textContent = '';
  setHidden(dom.viewer, true);
  state.activeCapture = null;
  renderFilm();
}

async function viewerOpen(reveal) {
  const item = state.activeCapture;
  if (!item) return;
  if (state.busy.viewer) return;
  state.busy.viewer = true;
  try {
    await invoke('open_path', { path: capturePath(item), reveal: !!reveal });
  } catch (err) {
    toast(friendlyError(err, '无法打开文件'), 'error');
  } finally {
    state.busy.viewer = false;
  }
}

async function viewerDelete() {
  const item = state.activeCapture;
  if (!item) return;
  if (state.busy.viewer) return;
  if (state.deleteArmed !== item.name) {
    state.deleteArmed = item.name;
    if (dom.btnViewerDelete) {
      setText(dom.btnViewerDelete, '确认删除');
      dom.btnViewerDelete.classList.add('is-armed');
    }
    toast('再点一次「确认删除」就会删除这个文件', 'info');
    clearTimeout(state.timers.deleteArm);
    state.timers.deleteArm = setTimeout(function () {
      if (state.deleteArmed === item.name) resetDeleteConfirm();
    }, 4000);
    return;
  }
  resetDeleteConfirm();
  state.busy.viewer = true;
  if (dom.btnViewerDelete) dom.btnViewerDelete.disabled = true;
  try {
    await invoke('delete_capture', { name: item.name });
    state.captures = state.captures.filter(function (one) { return one.name !== item.name; });
    closeViewer();
    toast('已删除', 'info');
  } catch (err) {
    toast(friendlyError(err, '删除失败'), 'error');
  } finally {
    state.busy.viewer = false;
    if (dom.btnViewerDelete) dom.btnViewerDelete.disabled = false;
  }
}

/* ------------------------------------------------------------------ */
/* 比例 / 模式                                                          */
/* ------------------------------------------------------------------ */

function buildRatioOptions() {
  if (!dom.ratioSelect) return;
  dom.ratioSelect.textContent = '';
  PRESETS.forEach(function (preset) {
    const option = document.createElement('option');
    option.value = preset.id;
    option.textContent = preset.label;
    dom.ratioSelect.append(option);
  });
  dom.ratioSelect.value = state.ratioId;
  applyModeButtons();
}

function applyModeButtons() {
  const buttons = document.querySelectorAll('#mode-group .seg');
  buttons.forEach(function (button) {
    const active = button.dataset.mode === state.mode;
    button.setAttribute('aria-pressed', active ? 'true' : 'false');
  });
  const mode = MODES.find(function (item) { return item.id === state.mode; });
  setText(dom.modeHint, mode ? mode.hint : '');
}

function applyRatio(selection) {
  if (dom.ratioError) setHidden(dom.ratioError, true);
  if (dom.ratioError) setText(dom.ratioError, '');
  const customVisible = selection === 'custom';
  setHidden(dom.customRatio, !customVisible);

  if (customVisible) {
    const parsed = parseRatioInput(state.customW, state.customH);
    if (!parsed) {
      if (dom.ratioError) {
        setText(dom.ratioError, '比例需要是正数，比如 16 和 9；当前输入无效，画面保持上一次的比例。');
        setHidden(dom.ratioError, false);
      }
      return;
    }
    state.ratio = parsed;
  } else {
    state.ratio = resolveRatio(selection, state.customW, state.customH);
  }
  state.ratioId = selection;
  updateHud();
  paint();
}

function scheduleRatioApply() {
  clearTimeout(state.timers.ratio);
  state.timers.ratio = setTimeout(function () { applyRatio('custom'); }, 240);
}

/* ------------------------------------------------------------------ */
/* 无线（扫码）                                                         */
/* ------------------------------------------------------------------ */

async function openWireless() {
  if (!dom.wirelessModal) return;
  state.wireless.open = true;
  setHidden(dom.wirelessModal, false);
  renderPair(null);
  if (state.busy.pair) return;
  state.busy.pair = true;
  try {
    const info = await invoke('pair_info');
    state.wireless.url = info && info.url ? info.url : '';
    renderPair(info);
  } catch (err) {
    renderPair({ available: false, hint: friendlyError(err, '无线服务启动失败') });
  } finally {
    state.busy.pair = false;
  }
}

function renderPair(info) {
  if (dom.wirelessQr) {
    dom.wirelessQr.textContent = '';
    const qr = info && info.qrUrl ? info.qrUrl : '';
    if (qr) {
      const image = new Image();
      image.alt = '扫码连接地址的二维码';
      image.src = qr;
      image.addEventListener('error', function () {
        dom.wirelessQr.textContent = '';
        dom.wirelessQr.append(el('p', 'qr-placeholder', '二维码加载失败，可以手动输入地址'));
      });
      dom.wirelessQr.append(image);
    } else {
      dom.wirelessQr.append(el('p', 'qr-placeholder', info ? '无法生成二维码' : '正在准备二维码…'));
    }
  }
  const url = info && info.url ? info.url : '';
  if (dom.wirelessUrl) setText(dom.wirelessUrl, url || '—');
  if (dom.btnCopyUrl) dom.btnCopyUrl.disabled = !url;

  const connected = state.wireless.connected;
  let statusText = '等待手机连接…';
  let tone = '';
  if (connected) {
    statusText = '已连接' + (state.wireless.name ? '：' + state.wireless.name : '');
    tone = 'live';
  } else if (info && info.available === false) {
    statusText = '无线服务暂不可用';
    tone = 'error';
  } else if (!info) {
    statusText = '正在准备…';
  }
  if (dom.wirelessStatus) {
    setText(dom.wirelessStatus, statusText);
    if (tone) dom.wirelessStatus.setAttribute('data-tone', tone);
    else dom.wirelessStatus.removeAttribute('data-tone');
  }
  if (dom.wirelessHint) {
    let hint = '手机与电脑连同一个 Wi‑Fi；首次打开需要在手机上信任本地证书。';
    if (info && info.available === false && info.hint) hint = info.hint;
    else if (info && info.hint && !connected) hint = info.hint;
    setText(dom.wirelessHint, hint);
  }
}

async function closeWireless() {
  if (!dom.wirelessModal) return;
  const wasConnected = state.wireless.connected;
  setHidden(dom.wirelessModal, true);
  state.wireless.open = false;
  if (!wasConnected) {
    try { await invoke('pair_stop'); } catch (err) { /* 服务已停止 */ }
  }
}

async function copyWirelessUrl() {
  const url = state.wireless.url;
  if (!url) return;
  let ok = false;
  try {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(url);
      ok = true;
    }
  } catch (err) {
    ok = false;
  }
  if (!ok) {
    try {
      const input = document.createElement('textarea');
      input.value = url;
      input.setAttribute('readonly', '');
      input.style.position = 'fixed';
      input.style.opacity = '0';
      document.body.append(input);
      input.select();
      ok = document.execCommand('copy');
      input.remove();
    } catch (err) {
      ok = false;
    }
  }
  toast(ok ? '地址已复制' : '复制失败，请手动选择地址', ok ? 'info' : 'error');
}

/* ------------------------------------------------------------------ */
/* Toast                                                              */
/* ------------------------------------------------------------------ */

function toast(text, level) {
  if (!dom.toasts || !text) return;
  const node = el('div', 'toast', text);
  node.dataset.level = level === 'error' ? 'error' : 'info';
  dom.toasts.append(node);
  while (dom.toasts.children.length > 4) dom.toasts.firstElementChild.remove();
  setTimeout(function () {
    node.classList.add('is-leaving');
    setTimeout(function () { node.remove(); }, 240);
  }, level === 'error' ? 5200 : 4200);
}

/* ------------------------------------------------------------------ */
/* 后端事件                                                            */
/* ------------------------------------------------------------------ */

function onSourceState(payload) {
  if (!payload) return;
  const next = payload.state;
  const gen = Number(payload.gen) || 0;
  if (next === 'starting') {
    if (state.source) setStatus('connecting', payload.detail || '正在启动设备…');
    return;
  }
  if (next === 'error') {
    stopStream();
    resetFrameState();
    if (state.source) setStatus('error', payload.detail || friendlyError(payload.detail, '设备出错了'));
    return;
  }
  if (next === 'stopped') {
    if (state.source) {
      state.source = null;
      stopStream();
      resetFrameState();
      setStatus('idle');
      renderDevices();
      updateHud();
    }
    return;
  }
  if (next === 'live') {
    if (!state.source) return;
    const changed = gen > 0 && gen !== state.gen;
    if (changed) {
      state.gen = gen;
      resetFrameState();
    }
    setStatus('live');
    if (changed || !state.streamAbort) connectStream();
  }
}

function onFrameInfo(payload) {
  if (!payload) return;
  const gen = Number(payload.gen) || 0;
  if (gen > 0 && state.gen > 0 && gen !== state.gen) return;
  state.frame = {
    width: Math.max(0, Math.round(Number(payload.width) || 0)),
    height: Math.max(0, Math.round(Number(payload.height) || 0)),
    fps: Math.max(0, Number(payload.fps) || 0),
  };
  updateHud();
  paint();
}

function onRecordTick(payload) {
  if (!payload) return;
  const secs = Math.max(0, Number(payload.secs) || 0);
  state.recSecs = secs;
  if (secs > 0) state.recStartedAt = Date.now() - secs * 1000;
  updateRecordingUi();
}

function onWireless(payload) {
  if (!payload) return;
  state.wireless.connected = !!payload.connected;
  state.wireless.name = payload.name || '';
  if (payload.hint && dom.wirelessHint && state.wireless.open) setText(dom.wirelessHint, payload.hint);
  if (dom.wirelessStatus && state.wireless.open) renderPair({ url: state.wireless.url, available: true });
  if (!payload.connected) return;
  toast('手机已连接：' + (payload.name || '无线设备'), 'info');
  const label = payload.name ? '无线 · ' + payload.name : '无线设备';
  if (!state.source || state.source.kind !== 'wireless') {
    selectSource('wireless', 'phone', label);
  }
  if (state.wireless.open) {
    setTimeout(function () {
      if (state.wireless.open) setHidden(dom.wirelessModal, true);
      state.wireless.open = false;
    }, 900);
  }
}

function onNotice(payload) {
  if (!payload || !payload.text) return;
  toast(payload.text, payload.level === 'error' ? 'error' : 'info');
}

function bindBackend() {
  listen('source-state', onSourceState);
  listen('frame-info', onFrameInfo);
  listen('record-tick', onRecordTick);
  listen('wireless', onWireless);
  listen('notice', onNotice);
}

/* ------------------------------------------------------------------ */
/* 启动 / 事件绑定                                                      */
/* ------------------------------------------------------------------ */

function cacheDom() {
  const ids = [
    'status-pill', 'status-text', 'device-name', 'rec-timer', 'rec-time', 'btn-fullscreen',
    'btn-refresh', 'device-list', 'device-hint', 'ratio-value', 'ratio-select', 'custom-ratio',
    'ratio-w', 'ratio-h', 'ratio-error', 'mode-group', 'mode-hint', 'btn-grid', 'btn-mirror',
    'btn-photo', 'btn-record', 'btn-record-label', 'btn-open-captures', 'stage', 'view',
    'stage-grid', 'hud', 'hud-clock', 'hud-res', 'hud-fps', 'rec-badge', 'rec-badge-time',
    'stage-empty', 'stage-empty-title', 'stage-empty-detail', 'flash', 'film-list', 'film-count',
    'film-empty', 'toasts', 'wireless-modal', 'wireless-qr', 'wireless-url', 'btn-copy-url',
    'wireless-status', 'wireless-hint', 'btn-wireless-close', 'viewer', 'viewer-title',
    'viewer-stage', 'btn-viewer-close', 'viewer-meta', 'btn-viewer-open', 'btn-viewer-reveal',
    'btn-viewer-delete',
  ];
  ids.forEach(function (id) {
    const key = id.replace(/-([a-z])/g, function (_, letter) { return letter.toUpperCase(); });
    if (id.startsWith('status-pill')) {
      dom.statusPill = $(id);
      return;
    }
    if (id.startsWith('status-text')) {
      dom.statusText = $(id);
      return;
    }
    if (id.startsWith('ratio-')) {
      dom[key] = $(id);
      return;
    }
    dom[key] = $(id);
  });
  // 逐个别名，避免自动驼峰规则在个别 id 上失配
  dom.wirelessModal = $('wireless-modal');
  dom.wirelessQr = $('wireless-qr');
  dom.wirelessUrl = $('wireless-url');
  dom.wirelessStatus = $('wireless-status');
  dom.wirelessHint = $('wireless-hint');
  dom.btnWirelessClose = $('btn-wireless-close');
  dom.btnCopyUrl = $('btn-copy-url');
  dom.customRatio = $('custom-ratio');
  dom.ratioW = $('ratio-w');
  dom.ratioH = $('ratio-h');
  dom.ratioError = $('ratio-error');
  dom.ratioSelect = $('ratio-select');
  dom.stageEmpty = $('stage-empty');
  dom.stageEmptyTitle = $('stage-empty-title');
  dom.stageEmptyDetail = $('stage-empty-detail');
  dom.btnRecord = $('btn-record');
  dom.btnRecordLabel = $('btn-record-label');
  dom.recTimer = $('rec-timer');
  dom.recTime = $('rec-time');
  dom.recBadge = $('rec-badge');
  dom.recBadgeTime = $('rec-badge-time');
  dom.hudClock = $('hud-clock');
  dom.hudRes = $('hud-res');
  dom.hudFps = $('hud-fps');
  dom.stageGrid = $('stage-grid');
  dom.filmList = $('film-list');
  dom.filmEmpty = $('film-empty');
  dom.btnFullscreen = $('btn-fullscreen');
  dom.btnRefresh = $('btn-refresh');
  dom.btnGrid = $('btn-grid');
  dom.btnMirror = $('btn-mirror');
  dom.btnPhoto = $('btn-photo');
  dom.btnOpenCaptures = $('btn-open-captures');
  dom.viewerStage = $('viewer-stage');
  dom.viewerMeta = $('viewer-meta');
  dom.viewerTitle = $('viewer-title');
  dom.btnViewerOpen = $('btn-viewer-open');
  dom.btnViewerReveal = $('btn-viewer-reveal');
  dom.btnViewerDelete = $('btn-viewer-delete');
  dom.btnViewerClose = $('btn-viewer-close');
  dom.deviceList = $('device-list');
  dom.deviceHint = $('device-hint');
  dom.deviceName = $('device-name');
  dom.modeHint = $('mode-hint');
  dom.ratioValue = $('ratio-value');
  dom.toasts = $('toasts');
  dom.view = $('view');
  dom.stage = $('stage');
  dom.hud = $('hud');
  dom.grid = $('stage-grid');
  dom.viewer = $('viewer');
  dom.flash = $('flash');
  dom.modeGroup = $('mode-group');
}

function isTypingTarget(target) {
  if (!target || !target.closest) return false;
  return !!target.closest('input, textarea, select, [contenteditable="true"], [contenteditable=""]');
}

function bindDom() {
  if (dom.btnRefresh) dom.btnRefresh.addEventListener('click', function () { refreshDevices(); });

  if (dom.ratioSelect) {
    dom.ratioSelect.addEventListener('change', function () {
      applyRatio(dom.ratioSelect.value);
      dom.ratioSelect.blur();
    });
  }
  if (dom.ratioW) dom.ratioW.addEventListener('input', function () { state.customW = dom.ratioW.value; scheduleRatioApply(); });
  if (dom.ratioH) dom.ratioH.addEventListener('input', function () { state.customH = dom.ratioH.value; scheduleRatioApply(); });

  if (dom.modeGroup) {
    dom.modeGroup.addEventListener('click', function (event) {
      const button = event.target.closest('.seg');
      if (!button) return;
      state.mode = button.dataset.mode || 'fit';
      applyModeButtons();
      paint();
    });
  }

  if (dom.btnGrid) {
    dom.btnGrid.addEventListener('click', function () {
      state.grid = !state.grid;
      dom.btnGrid.setAttribute('aria-pressed', state.grid ? 'true' : 'false');
      paint();
    });
  }

  if (dom.btnMirror) {
    dom.btnMirror.addEventListener('click', function () {
      state.mirror = !state.mirror;
      dom.btnMirror.setAttribute('aria-pressed', state.mirror ? 'true' : 'false');
      paint();
    });
  }

  if (dom.btnPhoto) dom.btnPhoto.addEventListener('click', function () { takePhoto(); });
  if (dom.btnRecord) dom.btnRecord.addEventListener('click', function () { toggleRecording(); });
  if (dom.btnOpenCaptures) {
    dom.btnOpenCaptures.addEventListener('click', function () {
      invoke('open_captures_dir')
        .then(function (result) {
          if (result && result.dir) {
            state.capturesDir = result.dir;
            toast('已打开拍摄目录：' + shortPath(result.dir, 40), 'info');
          } else {
            toast('已打开拍摄目录', 'info');
          }
        })
        .catch(function (err) { toast(friendlyError(err, '无法打开拍摄目录'), 'error'); });
    });
  }

  if (dom.btnFullscreen) dom.btnFullscreen.addEventListener('click', function () { toggleFullscreen(); });
  document.addEventListener('fullscreenchange', function () {
    const active = !!document.fullscreenElement;
    if (dom.btnFullscreen) dom.btnFullscreen.setAttribute('aria-pressed', active ? 'true' : 'false');
    setTimeout(paint, 60);
  });

  if (dom.btnCopyUrl) dom.btnCopyUrl.addEventListener('click', function () { copyWirelessUrl(); });
  if (dom.btnWirelessClose) dom.btnWirelessClose.addEventListener('click', function () { closeWireless(); });
  if (dom.wirelessModal) {
    dom.wirelessModal.addEventListener('click', function (event) {
      if (event.target === dom.wirelessModal) closeWireless();
    });
  }

  if (dom.btnViewerClose) dom.btnViewerClose.addEventListener('click', function () { closeViewer(); });
  if (dom.viewer) {
    dom.viewer.addEventListener('click', function (event) {
      if (event.target === dom.viewer) closeViewer();
    });
  }
  if (dom.btnViewerOpen) dom.btnViewerOpen.addEventListener('click', function () { viewerOpen(false); });
  if (dom.btnViewerReveal) dom.btnViewerReveal.addEventListener('click', function () { viewerOpen(true); });
  if (dom.btnViewerDelete) dom.btnViewerDelete.addEventListener('click', function () { viewerDelete(); });

  document.addEventListener('keydown', onKeyDown);

  window.addEventListener('resize', function () { paint(); });
  if (typeof ResizeObserver === 'function' && dom.stage) {
    const observer = new ResizeObserver(function () { paint(); });
    observer.observe(dom.stage);
  }
}

function onKeyDown(event) {
  if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey) return;

  const viewerOpenNow = dom.viewer && !dom.viewer.hasAttribute('hidden');
  const wirelessOpenNow = dom.wirelessModal && !dom.wirelessModal.hasAttribute('hidden');

  if (viewerOpenNow) {
    if (event.key === 'Escape') { event.preventDefault(); closeViewer(); }
    return;
  }
  if (wirelessOpenNow) {
    if (event.key === 'Escape') { event.preventDefault(); closeWireless(); }
    return;
  }
  if (isTypingTarget(event.target)) return;

  const key = (event.key || '').toLowerCase();
  if (event.key === ' ' || event.code === 'Space') {
    event.preventDefault();
    takePhoto();
    return;
  }
  if (key === 'g') {
    event.preventDefault();
    if (dom.btnGrid) dom.btnGrid.click();
    return;
  }
  if (key === 'm') {
    event.preventDefault();
    if (dom.btnMirror) dom.btnMirror.click();
    return;
  }
  if (key === 'r') {
    event.preventDefault();
    toggleRecording();
    return;
  }
  if (key === 'f') {
    event.preventDefault();
    toggleFullscreen();
  }
}

async function toggleFullscreen() {
  try {
    if (!document.fullscreenElement) {
      await document.documentElement.requestFullscreen();
    } else {
      await document.exitFullscreen();
    }
  } catch (err) {
    toast('无法进入全屏，可以按系统快捷键切换', 'error');
  }
}

/* ------------------------------------------------------------------ */
/* 启动流程                                                            */
/* ------------------------------------------------------------------ */

function startTimers() {
  state.timers.clock = setInterval(function () {
    setText(dom.hudClock, formatClock(new Date()));
  }, 500);

  state.timers.rec = setInterval(function () {
    if (!state.recording) return;
    if (state.recStartedAt > 0) {
      state.recSecs = Math.max(0, (Date.now() - state.recStartedAt) / 1000);
      updateRecordingUi();
    }
  }, 500);

  state.timers.refresh = setInterval(function () {
    if (document.hidden) return;
    refreshDevices({ silent: true });
  }, 10000);
}

async function loadAppInfo() {
  try {
    const info = await invoke('app_info');
    state.info = info || null;
    if (!info || !info.streamUrl) {
      toast('没有拿到画面服务地址，请在桌面应用内打开', 'error');
    }
  } catch (err) {
    state.info = null;
    setStatus('error', friendlyError(err, '无法连接取景服务'));
    toast(friendlyError(err, '无法连接取景服务'), 'error');
  }
}

async function loadCapturesDir() {
  try {
    const result = await invoke('captures_dir');
    if (result && result.dir) state.capturesDir = result.dir;
  } catch (err) {
    /* 目录信息不是必需的 */
  }
}

async function restoreRecording() {
  try {
    const status = await invoke('recording_status');
    if (status && status.recording) {
      state.recording = true;
      state.recName = status.name || '';
      state.recSecs = Math.max(0, Number(status.secs) || 0);
      state.recStartedAt = Date.now() - state.recSecs * 1000;
      updateRecordingUi();
    }
  } catch (err) {
    /* 忽略 */
  }
}

async function boot() {
  cacheDom();
  buildRatioOptions();
  bindDom();
  bindBackend();

  if (dom.btnGrid) dom.btnGrid.setAttribute('aria-pressed', state.grid ? 'true' : 'false');
  if (dom.btnMirror) dom.btnMirror.setAttribute('aria-pressed', state.mirror ? 'true' : 'false');
  renderFilm();
  setStatus('idle');
  paint();
  startTimers();

  if (!hasBridge()) {
    setStatus('error', '没有检测到桌面环境，请通过 MONITOR 应用打开。');
    renderDevices();
    return;
  }

  await loadAppInfo();
  await Promise.all([refreshDevices(), loadCapturesDir()]);
  await restoreRecording();
}

boot();
