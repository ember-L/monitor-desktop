import test from 'node:test';
import assert from 'node:assert/strict';

import {
  FrameReader,
  parseMjpegStream,
  parseHeader,
  findHeaderEnd,
  findJpegStart,
  findJpegEnd,
  isJpegStart,
} from '../mjpeg.js';

/* ------------------------------------------------------------------ */
/* 测试用的 MJPEG 流构造                                                */
/* ------------------------------------------------------------------ */

const CRLF = '\r\n';

/** 最小「合法」JPEG：SOI + 载荷 + EOI。 */
function jpeg(payload = [0x11, 0x22, 0x33]) {
  return Uint8Array.from([0xff, 0xd8, ...payload, 0xff, 0xd9]);
}

function concat(...chunks) {
  const total = chunks.reduce((sum, chunk) => sum + chunk.length, 0);
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.length;
  }
  return out;
}

function ascii(text) {
  return Uint8Array.from(Array.from(text, (ch) => ch.charCodeAt(0) & 0xff));
}

/** 造一个 multipart 分段（与 docs/CONTRACT.md §3 一致）。 */
function part(frame, { withLength = true, eol = true } = {}) {
  const header =
    '--frame' + CRLF +
    'Content-Type: image/jpeg' + CRLF +
    (withLength ? 'Content-Length: ' + frame.length + CRLF : '') +
    CRLF;
  return concat(ascii(header), frame, eol ? Uint8Array.from([13, 10]) : new Uint8Array(0));
}

/** 按固定大小切片，模拟网络分片。 */
function sliceEvery(bytes, size) {
  const out = [];
  for (let i = 0; i < bytes.length; i += size) out.push(bytes.subarray(i, i + size));
  return out;
}

function feed(chunks, reader = new FrameReader()) {
  const frames = [];
  for (const chunk of chunks) frames.push(...reader.push(chunk));
  return { frames, reader };
}

/* ------------------------------------------------------------------ */
/* 基础工具函数                                                        */
/* ------------------------------------------------------------------ */

test('findHeaderEnd 找到 CRLFCRLF 且不越界', () => {
  const bytes = ascii('Content-Length: 3' + CRLF + CRLF);
  assert.equal(findHeaderEnd(bytes), bytes.length - 4);
  assert.equal(findHeaderEnd(ascii('no delimiter here')), -1);
  assert.equal(findHeaderEnd(ascii('\r\n\r')), -1);
});

test('parseHeader 解析 Content-Length', () => {
  assert.deepEqual(parseHeader('Content-Type: image/jpeg' + CRLF + 'Content-Length: 1234'), {
    length: 1234,
    looksLikeHeader: true,
  });
  assert.equal(parseHeader('content-length:7').length, 7);
  assert.equal(parseHeader('Content-Type: image/jpeg').length, null);
  assert.equal(parseHeader('Content-Type: image/jpeg').looksLikeHeader, true);
  assert.equal(parseHeader('random noise').looksLikeHeader, false);
  assert.equal(parseHeader('').looksLikeHeader, false);
  assert.equal(parseHeader('').length, null);
});

test('JPEG 标记探测', () => {
  const frame = jpeg([1, 2, 3]);
  assert.ok(isJpegStart(frame));
  assert.equal(isJpegStart(Uint8Array.from([1, 2])), false);
  assert.equal(findJpegStart(concat(ascii('xx'), frame)), 2);
  assert.equal(findJpegEnd(frame), frame.length - 2);
  assert.equal(findJpegEnd(Uint8Array.from([0xff, 0xd8, 0x01])), -1);
});

/* ------------------------------------------------------------------ */
/* 单帧 / 多帧                                                          */
/* ------------------------------------------------------------------ */

test('单块完整流：拆出一帧且字节完全一致', () => {
  const frame = jpeg([0xaa, 0xbb, 0xcc, 0xdd, 0xee]);
  const { frames, reader } = feed([part(frame)]);
  assert.equal(frames.length, 1);
  assert.deepEqual([...frames[0]], [...frame]);
  assert.equal(reader.stats.frames, 1);
  assert.equal(reader.stats.bytes, frame.length);
  assert.equal(reader.stats.badFrames, 0);
});

test('头部与载荷分离：帧内不含 HTTP 头文本', () => {
  const frame = jpeg([0x01, 0x02, 0x03]);
  const { frames } = feed([part(frame)]);
  assert.equal(frames.length, 1);
  const text = Array.from(frames[0], (b) => String.fromCharCode(b)).join('');
  assert.ok(!text.includes('Content-Type'));
  assert.ok(!text.includes('Content-Length'));
  assert.ok(!text.includes('--frame'));
  assert.equal(frames[0].length, frame.length);
  assert.equal(frames[0][0], 0xff);
  assert.equal(frames[0][1], 0xd8);
});

test('连续多帧一次喂入，顺序不变', () => {
  const framesIn = [jpeg([1]), jpeg([2, 2]), jpeg([3, 3, 3])];
  const wire = concat(...framesIn.map((frame) => part(frame)));
  const { frames } = feed([wire]);
  assert.equal(frames.length, 3);
  framesIn.forEach((expected, index) => assert.deepEqual([...frames[index]], [...expected]));
});

test('帧之间没有尾部 CRLF 也能继续解析', () => {
  const first = jpeg([0x10]);
  const second = jpeg([0x20]);
  const wire = concat(part(first, { eol: false }), part(second, { eol: false }));
  const { frames } = feed([wire]);
  assert.equal(frames.length, 2);
  assert.deepEqual([...frames[0]], [...first]);
  assert.deepEqual([...frames[1]], [...second]);
});

/* ------------------------------------------------------------------ */
/* 分片容错                                                             */
/* ------------------------------------------------------------------ */

test('跨 chunk 分片：逐字节喂入仍能拆出完整帧', () => {
  const frame = jpeg([0xde, 0xad, 0xbe, 0xef, 0x00, 0x7f]);
  const wire = part(frame);
  const reader = new FrameReader();
  const frames = [];
  for (let i = 0; i < wire.length; i++) {
    frames.push(...reader.push(wire.subarray(i, i + 1)));
    assert.ok(reader.pending <= wire.length, '不应无限缓冲');
  }
  assert.equal(frames.length, 1);
  assert.deepEqual([...frames[0]], [...frame]);
});

test('跨 chunk 分片：任意切分长度都能还原', () => {
  const framesIn = [jpeg([1, 2, 3, 4, 5]), jpeg([6, 7, 8])];
  const wire = concat(...framesIn.map((frame) => part(frame)));
  for (const size of [1, 2, 3, 5, 7, 11, 16, 64, 4096]) {
    const { frames } = feed(sliceEvery(wire, size));
    assert.equal(frames.length, framesIn.length, '分片大小 ' + size);
    framesIn.forEach((expected, index) => {
      assert.deepEqual([...frames[index]], [...expected], '分片大小 ' + size + ' 第 ' + index + ' 帧');
    });
  }
});

test('载荷未收满时先不吐帧，收满后再吐', () => {
  const frame = jpeg(Array.from({ length: 40 }, (_, index) => index));
  const wire = part(frame);
  const split = Math.floor(wire.length / 2);
  const reader = new FrameReader();
  assert.equal(reader.push(wire.subarray(0, split)).length, 0, '半帧不应吐帧');
  assert.ok(reader.pending > 0, '半帧应留在缓冲里');
  const rest = reader.push(wire.subarray(split));
  assert.equal(rest.length, 1);
  assert.deepEqual([...rest[0]], [...frame]);
});

/* ------------------------------------------------------------------ */
/* 无 Content-Length 回退                                               */
/* ------------------------------------------------------------------ */

test('无 Content-Length：按 FF D8 / FF D9 回退扫描', () => {
  const frame = jpeg([0x31, 0x32, 0x33, 0x34]);
  const { frames, reader } = feed([part(frame, { withLength: false })]);
  assert.equal(frames.length, 1);
  assert.deepEqual([...frames[0]], [...frame]);
  assert.equal(reader.stats.frames, 1);
});

test('无 Content-Length：EOI 跨 chunk 也能恢复', () => {
  const frame = jpeg([0x41, 0x42, 0x43]);
  const wire = part(frame, { withLength: false, eol: false });
  // 让最后一个字节（EOI 的第二个字节）单独到达
  const reader = new FrameReader();
  assert.equal(reader.push(wire.subarray(0, wire.length - 1)).length, 0);
  const frames = reader.push(wire.subarray(wire.length - 1));
  assert.equal(frames.length, 1);
  assert.deepEqual([...frames[0]], [...frame]);
});

test('无 Content-Length：连续多帧回退扫描', () => {
  const framesIn = [jpeg([9, 9]), jpeg([8, 8, 8])];
  const wire = concat(...framesIn.map((frame) => part(frame, { withLength: false })));
  const { frames } = feed([wire]);
  assert.equal(frames.length, 2);
  framesIn.forEach((expected, index) => assert.deepEqual([...frames[index]], [...expected]));
});

/* ------------------------------------------------------------------ */
/* 垃圾 / 坏帧容错                                                      */
/* ------------------------------------------------------------------ */

test('垃圾字节跳过：帧前的杂字节不影响解析', () => {
  const frame = jpeg([0x51, 0x52]);
  const junk = concat(ascii('MJPEG stream warm-up' + CRLF + CRLF), Uint8Array.from([0, 1, 2, 0xff]));
  const { frames, reader } = feed([concat(junk, part(frame))]);
  assert.equal(frames.length, 1);
  assert.deepEqual([...frames[0]], [...frame]);
  assert.ok(reader.stats.skippedBytes > 0);
});

test('垃圾字节跳过：两帧之间夹垃圾仍各取其帧', () => {
  const first = jpeg([0x61]);
  const second = jpeg([0x62]);
  const junk = concat(ascii('v' + CRLF + CRLF), Uint8Array.from([0xde, 0xad, 0xbe, 0xef]), ascii(CRLF + CRLF));
  const { frames } = feed([concat(part(first), junk, part(second))]);
  assert.equal(frames.length, 2);
  assert.deepEqual([...frames[0]], [...first]);
  assert.deepEqual([...frames[1]], [...second]);
});

test('坏载荷（不是 JPEG）被跳过，后续帧不受影响', () => {
  const bad = Uint8Array.from([0, 0, 0, 0, 0]);
  const good = jpeg([0x71, 0x72]);
  const { frames, reader } = feed([concat(part(bad), part(good))]);
  assert.equal(frames.length, 1);
  assert.deepEqual([...frames[0]], [...good]);
  assert.ok(reader.stats.badFrames >= 1);
});

test('纯垃圾流不会让缓冲无限增长', () => {
  const reader = new FrameReader();
  const frames = reader.push(new Uint8Array(200 * 1024)); // 200KB 零字节
  assert.equal(frames.length, 0);
  assert.ok(reader.pending <= 64 * 1024, '缓冲应被裁剪，实际 ' + reader.pending);
});

test('大段垃圾之后接正常帧仍能恢复', () => {
  const frame = jpeg([0x81, 0x82]);
  const junk = new Uint8Array(30 * 1024);
  junk.fill(0x41); // 全是 'A'
  const { frames } = feed([concat(junk, part(frame))]);
  assert.ok(frames.length >= 1);
  assert.deepEqual([...frames[frames.length - 1]], [...frame]);
});

/* ------------------------------------------------------------------ */
/* 流式接口                                                             */
/* ------------------------------------------------------------------ */

test('parseMjpegStream 逐帧回调（ReadableStream 分片）', async () => {
  const framesIn = [jpeg([1, 2]), jpeg([3, 4, 5]), jpeg([6])];
  const wire = concat(...framesIn.map((frame) => part(frame)));
  const stream = new ReadableStream({
    start(controller) {
      for (const chunk of sliceEvery(wire, 5)) controller.enqueue(chunk);
      controller.close();
    },
  });
  const seen = [];
  const stats = await parseMjpegStream({ body: stream }, { onFrame: (bytes) => seen.push([...bytes]) });
  assert.equal(seen.length, 3);
  framesIn.forEach((expected, index) => assert.deepEqual(seen[index], [...expected]));
  assert.equal(stats.frames, 3);
  assert.equal(stats.bytes, framesIn.reduce((sum, frame) => sum + frame.length, 0));
});

test('parseMjpegStream 在没有 body 时给出友好错误', async () => {
  await assert.rejects(parseMjpegStream({ body: null }), /不支持流式读取/);
  await assert.rejects(parseMjpegStream(null), /不支持流式读取/);
});

test('parseMjpegStream 支持 AbortSignal 取消', async () => {
  const controller = new AbortController();
  const stream = new ReadableStream({
    start(inner) {
      inner.enqueue(part(jpeg([1])));
      // 故意不关闭，模拟长连接
    },
  });
  controller.abort();
  await assert.rejects(
    parseMjpegStream({ body: stream }, { signal: controller.signal }),
    (error) => error.name === 'AbortError',
  );
});

test('reset 之后缓冲、状态与统计归零', () => {
  const frame = jpeg([1, 2, 3]);
  const reader = new FrameReader();
  reader.push(part(frame));
  reader.push(Uint8Array.from([0, 1, 2, 3]));
  assert.equal(reader.stats.frames, 1);
  reader.reset();
  assert.equal(reader.pending, 0);
  assert.equal(reader.stats.frames, 0);
  assert.equal(reader.stats.bytes, 0);
  assert.equal(reader.mode, 'header');
  const { frames } = feed([part(frame)], reader);
  assert.equal(frames.length, 1);
  assert.deepEqual([...frames[0]], [...frame]);
});
