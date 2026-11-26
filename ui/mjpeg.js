/**
 * MJPEG-over-fetch 解析器。
 *
 * WebKit 不支持把 `<img src="/stream">` 当 multipart 流播放，所以前端必须自己
 * 用 `fetch` + `ReadableStream` 拆帧。帧格式见 `docs/CONTRACT.md` §3：
 *
 *   --frame\r\n
 *   Content-Type: image/jpeg\r\n
 *   Content-Length: <N>\r\n
 *   \r\n
 *   <N 字节 JPEG>\r\n
 *
 * 容错要求：
 *   1. 头部与载荷用 \r\n\r\n 分隔，数据可以任意跨 chunk 分片；
 *   2. 头部没有 Content-Length 时回退到 JPEGSOI(FF D8)/EOI(FF D9) 扫描；
 *   3. 垃圾字节/坏帧跳过，不断流；
 *   4. 缓冲区有上限，坏流不会把内存吃满。
 *
 * 纯逻辑、零依赖，可在 `node --test` 下测试。
 */

const CR = 13;
const LF = 10;
const SOI_0 = 0xff;
const SOI_1 = 0xd8;
const EOI_0 = 0xff;
const EOI_1 = 0xd9;

/** 头部/载荷分隔符长度。 */
const HEADER_DELIM_LEN = 4;

function toPositive(value, fallback) {
  const n = Number(value);
  return Number.isFinite(n) && n > 0 ? Math.floor(n) : fallback;
}

/**
 * 找 `\r\n\r\n` 的下标（指向第一个 \r）。找不到返回 -1。
 * @param {Uint8Array} bytes
 * @param {number} [from]
 */
export function findHeaderEnd(bytes, from = 0) {
  for (let i = Math.max(0, from); i + HEADER_DELIM_LEN <= bytes.length; i++) {
    if (
      bytes[i] === CR &&
      bytes[i + 1] === LF &&
      bytes[i + 2] === CR &&
      bytes[i + 3] === LF
    ) {
      return i;
    }
  }
  return -1;
}

/**
 * 解析一个 MJPEG 分段头部。
 * @param {string} text 头部原文（不含结尾空行）
 * @returns {{length: number|null, looksLikeHeader: boolean}}
 */
export function parseHeader(text) {
  const raw = String(text ?? '');
  const match = /content-length\s*:\s*(\d+)/i.exec(raw);
  const size = match ? Number(match[1]) : NaN;
  const length = Number.isFinite(size) && size > 0 ? size : null;
  const looksLikeHeader =
    /content-type|content-length/i.test(raw) || /(^|[\r\n])--/.test(raw) || /^--/.test(raw.trimStart());
  return { length, looksLikeHeader };
}

/** 从 from 起找 JPEG SOI（FF D8）。 */
export function findJpegStart(bytes, from = 0) {
  for (let i = Math.max(0, from); i + 1 < bytes.length; i++) {
    if (bytes[i] === SOI_0 && bytes[i + 1] === SOI_1) return i;
  }
  return -1;
}

/**
 * 从 from 起找 JPEG EOI（FF D9），返回 FF 的下标；找不到返回 -1。
 * @param {Uint8Array} bytes
 * @param {number} [from]
 */
export function findJpegEnd(bytes, from = 2) {
  for (let i = Math.max(0, from); i + 1 < bytes.length; i++) {
    if (bytes[i] === EOI_0 && bytes[i + 1] === EOI_1) return i;
  }
  return -1;
}

/** 是否以 JPEG SOI 开头。 */
export function isJpegStart(bytes) {
  return !!bytes && bytes.length >= 2 && bytes[0] === SOI_0 && bytes[1] === SOI_1;
}

/** 头部是纯 ASCII，逐字节解码即可。 */
function latin1(bytes, from, to) {
  let out = '';
  for (let i = from; i < to; i++) out += String.fromCharCode(bytes[i]);
  return out;
}

/**
 * 增量帧读取器：把 fetch 的 chunk 喂进来，吐出完整的 JPEG 字节。
 *
 * @example
 * const reader = new FrameReader();
 * for await (const chunk of stream) {
 *   for (const jpeg of reader.push(chunk)) draw(jpeg);
 * }
 */
export class FrameReader {
  /**
   * @param {{maxHeader?:number, maxFrame?:number, maxBuffer?:number}} [options]
   */
  constructor(options = {}) {
    this.maxHeader = toPositive(options.maxHeader, 64 * 1024);
    this.maxFrame = toPositive(options.maxFrame, 24 * 1024 * 1024);
    this.maxBuffer = toPositive(options.maxBuffer, 32 * 1024 * 1024);
    this.reset();
  }

  /** 清空缓冲与状态（重连时调用）。 */
  reset() {
    /** @type {Uint8Array} */
    this.buffer = new Uint8Array(0);
    /** 'header' 找头部 | 'length' 按 Content-Length 收载荷 | 'scan' 按 SOI/EOI 扫描 */
    this.mode = 'header';
    /** 当前帧还需要的字节数 */
    this.need = 0;
    this.stats = {
      frames: 0, // 成功拆出的帧
      bytes: 0, // 有效载荷字节
      skippedBytes: 0, // 跳过的垃圾字节
      badFrames: 0, // 头部正常但载荷不是 JPEG 的帧
    };
  }

  /** 缓冲区里还没被消费的字节数。 */
  get pending() {
    return this.buffer.length;
  }

  /**
   * 追加一段字节，返回这次能凑出的完整帧。
   * @param {Uint8Array|ArrayBuffer|Array<number>} chunk
   * @returns {Uint8Array[]} 完整 JPEG 帧（顺序与流一致）
   */
  push(chunk) {
    if (!chunk) return [];
    const bytes =
      chunk instanceof Uint8Array
        ? chunk
        : Array.isArray(chunk)
          ? Uint8Array.from(chunk)
          : new Uint8Array(chunk);
    if (!bytes.length) return [];
    const merged = new Uint8Array(this.buffer.length + bytes.length);
    merged.set(this.buffer, 0);
    merged.set(bytes, this.buffer.length);
    this.buffer = merged;
    return this.drain();
  }

  /** 丢掉提前攒下的一小段头部残留（重连时防止旧数据串帧）。 */
  flushPending() {
    if (this.mode === 'header' && this.buffer.length < this.maxHeader) this.buffer = new Uint8Array(0);
  }

  /**
   * 尽可能多地消费缓冲区，返回新帧。
   * @returns {Uint8Array[]}
   */
  drain() {
    const frames = [];
    // 每次循环都至少丢弃/前进若干字节，上限只是防御性保险
    for (let guard = 0; guard < 100000; guard++) {
      if (this.mode === 'length') {
        if (this.buffer.length < this.need) break;
        const candidate = this.buffer.subarray(0, this.need);
        if (!isJpegStart(candidate)) {
          // Content-Length 对上了但载荷不是 JPEG：丢掉 1 字节重新对齐
          this.stats.badFrames += 1;
          this.buffer = this.buffer.subarray(1);
          this.need = 0;
          this.mode = 'header';
          continue;
        }
        this.emit(frames, candidate);
        this.buffer = this.buffer.subarray(this.need);
        this.need = 0;
        this.mode = 'header';
        this.skipCrlf();
        continue;
      }

      if (this.mode === 'scan') {
        if (this.buffer.length < 2) break;
        const start = findJpegStart(this.buffer);
        if (start < 0) {
          // 没有 SOI：只留 1 个字节等下一块（可能是被切开的 SOI），其余都是垃圾
          this.stats.skippedBytes += Math.max(0, this.buffer.length - 1);
          this.trimTo(1);
          break;
        }
        if (start > 0) this.trimTo(this.buffer.length - start);
        const end = findJpegEnd(this.buffer, 2);
        if (end < 0) {
          if (this.buffer.length > this.maxFrame) {
            // 一直没有 EOI：判定为坏数据，整段丢弃并回到找头部
            this.stats.skippedBytes += this.buffer.length;
            this.buffer = new Uint8Array(0);
            this.mode = 'header';
            break;
          }
          break;
        }
        this.emit(frames, this.buffer.subarray(0, end + 2));
        this.buffer = this.buffer.subarray(end + 2);
        this.mode = 'header';
        this.skipCrlf();
        continue;
      }

      // mode === 'header'
      const delimiter = findHeaderEnd(this.buffer);
      if (delimiter < 0) {
        if (this.buffer.length > this.maxHeader) {
          // 头部迟迟不完整：可能对方根本没发头部（裸 JPEG）
          const soi = findJpegStart(this.buffer);
          if (soi >= 0) {
            this.trimTo(this.buffer.length - soi);
            this.mode = 'scan';
            continue;
          }
          // 纯垃圾：只留尾巴，避免缓冲无限增长
          this.trimTo(Math.min(this.buffer.length, 8));
        }
        break;
      }

      if (delimiter > this.maxHeader) {
        // 超长「头部」一定是垃圾
        this.stats.skippedBytes += delimiter + HEADER_DELIM_LEN;
        this.trimTo(this.buffer.length - (delimiter + HEADER_DELIM_LEN));
        continue;
      }

      const header = parseHeader(latin1(this.buffer, 0, delimiter));
      if (!header.looksLikeHeader) {
        // 分隔符前面不是 MJPEG 头：跳过这段垃圾
        this.stats.skippedBytes += delimiter + HEADER_DELIM_LEN;
        this.trimTo(this.buffer.length - (delimiter + HEADER_DELIM_LEN));
        continue;
      }

      this.trimTo(this.buffer.length - (delimiter + HEADER_DELIM_LEN));
      if (header.length) {
        if (header.length > this.maxFrame) {
          this.stats.badFrames += 1;
          this.mode = 'header';
          continue;
        }
        this.need = header.length;
        this.mode = 'length';
      } else {
        // 没有 Content-Length：回退到 SOI/EOI 扫描
        this.mode = 'scan';
      }
    }

    if (this.buffer.length > this.maxBuffer) {
      this.stats.skippedBytes += this.buffer.length - this.maxHeader;
      this.trimTo(Math.min(this.buffer.length, this.maxHeader));
    }
    return frames;
  }

  /** @param {Uint8Array[]} frames @param {Uint8Array} view */
  emit(frames, view) {
    const copy = view.slice();
    frames.push(copy);
    this.stats.frames += 1;
    this.stats.bytes += copy.length;
  }

  /** 丢掉开头 size 个字节。 */
  trimTo(size) {
    const keep = Math.max(0, Math.min(size, this.buffer.length));
    if (keep === this.buffer.length) return;
    this.buffer = this.buffer.subarray(this.buffer.length - keep);
  }

  /** 帧之间的 `\r\n`。 */
  skipCrlf() {
    while (this.buffer.length >= 2 && this.buffer[0] === CR && this.buffer[1] === LF) {
      this.buffer = this.buffer.subarray(2);
    }
  }
}

/**
 * 消费一个 fetch Response 的 body，逐帧回调。
 *
 * @param {Response|{body: ReadableStream<Uint8Array>}} response
 * @param {{
 *   onFrame?: (jpeg: Uint8Array) => void|Promise<void>,
 *   signal?: AbortSignal,
 *   maxHeader?: number,
 *   maxFrame?: number,
 * }} [options]
 * @returns {Promise<{frames:number, bytes:number, skipped:number}>}
 */
export async function parseMjpegStream(response, options = {}) {
  const body = response && response.body;
  if (!body || typeof body.getReader !== 'function') {
    throw new Error('当前环境不支持流式读取画面');
  }
  const { onFrame, signal } = options;
  const abort = () => {
    const error = new Error('画面流已取消');
    error.name = 'AbortError';
    return error;
  };
  const ensureLive = () => {
    if (signal && signal.aborted) throw abort();
  };

  const reader = body.getReader();
  const frames = new FrameReader(options);
  try {
    for (;;) {
      ensureLive();
      const { value, done } = await reader.read();
      if (done) break;
      if (!value || !value.length) continue;
      const complete = frames.push(value);
      for (const jpeg of complete) {
        ensureLive();
        if (onFrame) await onFrame(jpeg);
      }
    }
  } finally {
    // 断开/取消时释放底层连接；失败无所谓
    try {
      await reader.cancel();
    } catch (_) {
      /* 已断开 */
    }
  }
  return {
    frames: frames.stats.frames,
    bytes: frames.stats.bytes,
    skipped: frames.stats.skippedBytes,
  };
}
