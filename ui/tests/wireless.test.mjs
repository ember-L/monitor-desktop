import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { createContext, runInContext } from 'node:vm';

const phonePage = readFileSync(new URL('../../src-tauri/src/phone_page.html', import.meta.url), 'utf8');
const script = phonePage.match(/<script>([\s\S]*?)<\/script>/)[1];
const app = readFileSync(new URL('../app.js', import.meta.url), 'utf8')
  .replace(/^import .*$/gm, '').replace(/\bboot\(\);\s*$/, '');

function phone({ width = 1920, height = 1080, resolution = 960, rate = 24 } = {}) {
  const video = { videoWidth: width, videoHeight: height, readyState: 2 };
  const elements = { video, res: { value: resolution }, fps: { value: rate } };
  const drawing = {
    save() {}, restore() {}, drawImage() {},
    getImageData: () => ({ data: new Uint8Array([255, 255, 255, 255]) }),
  };
  return createContext({
    document: {
      getElementById: id => elements[id] ??= { style: {} },
      createElement: () => ({
        getContext: () => drawing,
        toBlob(done, type, quality) { done({ width: this.width, height: this.height, type, quality }); },
      }),
      addEventListener() {},
    },
    performance, location: { search: '' }, navigator: { userAgent: '' }, URLSearchParams,
    setInterval() {}, window: { addEventListener() {} },
  });
}

for (const [width, height, resolution, expected] of [
  [1920, 1080, 960, [960, 540]],
  [1080, 1920, 1280, [720, 1280]],
  [640, 480, 1280, [640, 480]],
]) {
  test(`wireless ${resolution} caps ${width}×${height} without stretching`, async () => {
    const context = phone({ width, height, resolution });
    runInContext(script, context);
    const frame = await runInContext('grab()', context);
    assert.deepEqual([frame.width, frame.height], expected);
  });
}

test('wireless quality reacts to ACK latency before the pipeline stalls', () => {
  const context = phone({ rate: 24 });
  runInContext(script, context);
  assert.ok(runInContext('sentFrames = 1; pendingSends.push({ n: 1, at: performance.now() - 100 }); onAck(1); quality', context) < 0.8);
});

test('desktop starts JPEG display decode without waiting for the black-frame probe', async () => {
  let releaseProbe;
  const context = createContext({
    window: {}, Blob, URL, setTimeout, clearTimeout,
    probe: new Promise(resolve => { releaseProbe = resolve; }),
  });
  runInContext(app, context);
  runInContext(`
    let decodeStarted = false;
    dom.viewImgB = { decode() { decodeStarted = true; return Promise.resolve(); } };
    dom.viewImg = {};
    frameLum = () => probe;
    paint = () => {};
    queueFrame(new Blob(['jpeg'], { type: 'image/jpeg' }));
  `, context);
  assert.equal(runInContext('decodeStarted', context), true);
  assert.equal(runInContext('state.hasFrame', context), false);
  releaseProbe(120);
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(runInContext('state.hasFrame', context), true);
  URL.revokeObjectURL(runInContext('dom.viewImgB.src', context));
});

test('established ACK stream stops at three outstanding frames even when ACKs stall', async () => {
  const context = phone();
  let now = 0;
  context.setTimeout = context.clearTimeout = () => {};
  context.performance = { now: () => now };
  runInContext(script, context);
  runInContext(`
    let pushed = 0, resume;
    sleep = () => new Promise(resolve => { resume = resolve; });
    ws = { readyState: 1, bufferedAmount: 0, send() { pushed++; } };
    sending = true;
    startLoop();
  `, context);
  const settle = () => new Promise(resolve => setImmediate(resolve));
  await settle();
  const step = async ms => {
    now += ms;
    runInContext('resume()', context);
    await settle();
  };

  runInContext('onAck(1)', context);
  for (let i = 0; i < 3; i++) await step(42);
  assert.equal(runInContext('pushed', context), 4);
  await step(1300);
  assert.equal(runInContext('pushed', context), 4);
  runInContext('onAck(3)', context);
  await step(42);
  assert.equal(runInContext('pushed', context), 5);
  runInContext('sending = false; resume()', context);
});

test('desktop keeps the last good frame if JPEG load fires before decode fails', async () => {
  const context = createContext({ window: {}, Blob, URL, setTimeout, clearTimeout });
  runInContext(app, context);
  runInContext(`
    let rejectDecode;
    state.hasFrame = true;
    dom.viewImg = { src: 'last-good.jpg' };
    dom.viewImgB = { naturalWidth: 1280, decode() { return new Promise((_, reject) => { rejectDecode = reject; }); } };
    frameLum = async () => 120;
    paint = () => {};
    queueFrame(new Blob(['jpeg'], { type: 'image/jpeg' }));
    dom.viewImgB.onload?.();
  `, context);
  await new Promise(resolve => setImmediate(resolve));
  try {
    assert.equal(runInContext('frontImage().src', context), 'last-good.jpg');
  } finally {
    runInContext('rejectDecode()', context);
    await new Promise(resolve => setImmediate(resolve));
    URL.revokeObjectURL(runInContext('dom.viewImgB.src', context));
  }
  assert.equal(runInContext('frontImage().src', context), 'last-good.jpg');
  assert.equal(runInContext('state.decodeFailures', context), 1);
});

test('Windows checks the decoded preview for black without decoding the JPEG twice', async () => {
  const pixels = new Uint8Array([0, 0, 0, 255]);
  let extraDecodes = 0;
  const context = createContext({
    window: {
      navigator: { userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/140.0.0.0' },
      createImageBitmap: async () => { extraDecodes++; return {}; },
    },
    document: { createElement: () => ({ getContext: () => ({
      clearRect() {}, drawImage() {}, getImageData: () => ({ data: pixels }),
    }) }) },
    Blob, URL, setTimeout, clearTimeout,
  });
  runInContext(app, context);
  runInContext(`
    state.hasFrame = true;
    dom.viewImg = { src: 'last-good.jpg' };
    dom.viewImgB = { decode: async () => {} };
    paint = () => {};
    const jpeg = new Blob(['jpeg'], { type: 'image/jpeg' });
  `, context);
  assert.equal(await runInContext('showFrame(jpeg)', context), 'skipped');
  assert.equal(runInContext('frontImage().src', context), 'last-good.jpg');
  for (let n = 0; n < 7; n++) assert.equal(await runInContext('showFrame(jpeg)', context), 'skipped');
  assert.equal(await runInContext('showFrame(jpeg)', context), 'shown');
  assert.equal(runInContext('state.darkFrames', context), 8);
  assert.equal(extraDecodes, 0);
  URL.revokeObjectURL(runInContext('frontImage().src', context));
});

function fakeNetwork(context) {
  const timers = new Map(), sockets = [];
  let now = 0, id = 0;
  Object.assign(context, {
    Blob, AbortController,
    setTimeout(fn, ms) { timers.set(++id, { fn, at: now + ms }); return id; },
    clearTimeout(id) { timers.delete(id); },
    performance: { now: () => now },
    WebSocket: class {
      readyState = 0;
      bufferedAmount = 0;
      messages = [];
      constructor() { sockets.push(this); }
      send(message) { this.messages.push(message); }
      close() { this.readyState = 3; this.onclose?.(); }
      open() { this.readyState = 1; this.onopen?.(); }
      receive(data) { this.onmessage?.({ data }); }
    },
  });
  return { sockets, async advance(ms) {
    const end = now + ms;
    for (;;) {
      const next = [...timers].sort(([, a], [, b]) => a.at - b.at)[0];
      if (!next || next[1].at > end) break;
      timers.delete(next[0]);
      now = next[1].at;
      next[1].fn();
      await new Promise(resolve => setImmediate(resolve));
    }
    now = end;
    await new Promise(resolve => setImmediate(resolve));
  } };
}

const settleNetwork = () => new Promise(resolve => setImmediate(resolve));

test('phone times out a stuck handshake and an open socket without ACKs, then recovers via POST and re-probes WSS', async () => {
  const context = phone();
  const { sockets, advance } = fakeNetwork(context);
  let posts = 0;
  context.fetch = async () => { posts++; return { ok: true, status: 204 }; };
  runInContext(script, context);
  runInContext('sending = true; connect()', context);
  await advance(5200);
  assert.equal(sockets[0].readyState, 3);
  assert.equal(sockets.length, 2);
  const second = sockets[1];
  second.open();
  assert.equal(second.messages.length, 1, 'no video before pairing is acknowledged');
  second.receive('{"type":"hello-ok"}');
  await settleNetwork();
  await advance(4500);
  assert.equal(second.messages.length, 4, 'hello plus at most three unconfirmed frames');
  assert.ok(posts > 0);
  await advance(21000);
  assert.equal(sockets.length, 3);
  const probing = posts;
  await advance(200);
  assert.ok(posts > probing, 'POST stays active during the WSS probe');
  sockets[2].open();
  sockets[2].receive('{"type":"hello-ok"}');
  await advance(100);
  assert.ok(sockets[2].messages.length > 1, 'video switches back after pairing');
  sockets[2].receive('{"type":"ack","n":1}');
  assert.equal(runInContext('badge.textContent', context), '已连接');
  runInContext('pauseSending()', context);
  const before = posts;
  await advance(30000);
  assert.equal(sockets[2].readyState, 3);
  assert.equal(sockets.length, 3);
  assert.equal(posts, before);
});

test('phone stops retrying when pairing is rejected by WSS or POST', async () => {
  for (const mode of ['ws', 'post']) {
    const context = phone();
    const { sockets, advance } = fakeNetwork(context);
    let posts = 0;
    context.fetch = async () => { posts++; return { status: 403, ok: false }; };
    runInContext(script, context);
    runInContext(`sending = true; ${mode === 'ws' ? 'connect()' : 'transport = "post"; startLoop()'}`, context);
    if (mode === 'ws') {
      sockets[0].open();
      sockets[0].receive('{"type":"error","message":"配对码无效"}');
    }
    await settleNetwork();
    assert.equal(runInContext('sending', context), false);
    assert.match(runInContext('$("err").innerHTML', context), /重新扫描/);
    await advance(30000);
    assert.equal(sockets.length, mode === 'ws' ? 1 : 0);
    assert.equal(posts, mode === 'post' ? 1 : 0);
  }
});

test('desktop preview waits for decode before asking for the latest frame and uses HTTP after socket failure', async () => {
  const context = createContext({ window: {} });
  const { sockets, advance } = fakeNetwork(context);
  let polls = 0;
  context.fetch = async () => { polls++; return { status: 404 }; };
  runInContext(app, context);
  runInContext(`
    let finishDecode;
    showFrame = () => new Promise(resolve => { finishDecode = resolve; });
    applyStatus = () => {};
    state.info = { streamUrl: 'http://127.0.0.1:9999/stream', previewUrl: 'ws://127.0.0.1:9999/preview?t=test' };
    state.source = { kind: 'wireless' };
    connectStream(); connectStream();
  `, context);
  assert.equal(sockets.length, 1);
  sockets[0].open();
  sockets[0].receive(new Blob(['jpeg']));
  assert.deepEqual(sockets[0].messages, ['next']);
  runInContext('finishDecode("shown"); state.hasFrame = true', context);
  await settleNetwork();
  assert.deepEqual(sockets[0].messages, ['next', 'next']);
  sockets[0].close();
  await advance(500);
  assert.ok(polls > 0);
  assert.equal(runInContext('state.hasFrame', context), true);
  runInContext('stopStream()', context);
  const before = polls;
  await advance(30000);
  assert.equal(polls, before);
  assert.equal(sockets.length, 1);
});

test('phone cancels an in-flight POST on pause and never runs a stale retry', async () => {
  const context = phone();
  const { sockets, advance } = fakeNetwork(context);
  const signals = [];
  context.fetch = (_, { signal }) => {
    signals.push(signal);
    return new Promise((_, reject) => signal.addEventListener('abort', () => reject(signal.reason)));
  };
  runInContext(script, context);
  runInContext('sending = true; transport = "post"; startLoop()', context);
  await settleNetwork();
  await advance(3999);
  assert.equal(signals[0].aborted, false);
  await advance(501);
  assert.equal(signals[0].aborted, true, 'hung POST times out before a fresh frame is retried');
  assert.equal(signals.length, 2);
  runInContext('pauseSending()', context);
  await advance(30000);
  assert.equal(signals[1].aborted, true);
  assert.equal(signals.length, 2);
  assert.equal(sockets.length, 0);
});

test('desktop aborts HTTP on source switch and discards its late result', async () => {
  const context = createContext({ window: {} });
  const { sockets, advance } = fakeNetwork(context);
  let reply, signal;
  context.fetch = (_, options) => {
    signal = options.signal;
    return new Promise(resolve => { reply = resolve; });
  };
  runInContext(app, context);
  runInContext(`
    let frames = 0;
    queueFrame = () => { frames++; };
    applyStatus = () => {};
    state.info = { streamUrl: 'http://127.0.0.1:9999/stream', previewUrl: 'ws://127.0.0.1:9999/preview' };
    state.source = { kind: 'wireless' };
    connectStream();
  `, context);
  await advance(3000); // WebSocket never opens (blocked by WebView or stuck in handshake).
  assert.equal(sockets[0].readyState, 3);
  assert.ok(signal);
  runInContext('stopStream(); state.source = { kind: "av" }; connectStream()', context);
  assert.ok(signal.aborted);
  reply({ ok: true, status: 200, headers: new Map(), blob: async () => new Blob(['old-frame']) });
  await settleNetwork();
  assert.equal(runInContext('frames', context), 0);
  assert.equal(sockets.length, 2);
});

test('desktop keeps serving snapshots while probing WebSocket, then cancels polling after the first response', async () => {
  const context = createContext({ window: {} });
  const { sockets, advance } = fakeNetwork(context);
  let polls = 0;
  context.fetch = async () => { polls++; return { status: 404 }; };
  runInContext(app, context);
  runInContext(`
    applyStatus = () => {};
    queueFrame = () => {};
    state.source = { kind: 'wireless' };
    state.info = { streamUrl: 'http://127.0.0.1:9999/stream', previewUrl: 'ws://127.0.0.1:9999/preview' };
    connectStream();
  `, context);
  sockets[0].close();
  await advance(20100);
  assert.equal(sockets.length, 2);
  const before = polls;
  await advance(480);
  assert.ok(polls > before, 'a slow handshake must not interrupt fallback');
  sockets[1].open();
  const afterOpen = polls;
  sockets[1].receive(new Blob(['jpeg']));
  await advance(480);
  assert.equal(polls, afterOpen);
  assert.deepEqual(sockets[1].messages, ['next', 'next']);
  runInContext('stopStream()', context);
});
