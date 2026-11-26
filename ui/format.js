/**
 * 展示层格式化工具：时间码、计时、比例输入校验等。
 * 纯函数、零依赖、不碰 DOM。
 */

/** 两位数补零。 */
export function pad2(value) {
  const n = Math.trunc(Math.abs(Number(value) || 0));
  return String(n).padStart(2, '0');
}

/** 数值夹取，非法值回退到下限。 */
export function clamp(value, min, max) {
  const v = Number(value);
  if (!Number.isFinite(v)) return min;
  const lo = Number.isFinite(min) ? min : -Infinity;
  const hi = Number.isFinite(max) ? max : Infinity;
  return Math.min(hi, Math.max(lo, v));
}

/**
 * 本地时间 -> HH:MM:SS。
 * @param {Date|number|string} [date]
 */
export function formatClock(date = new Date()) {
  const d = date instanceof Date ? date : new Date(date);
  if (Number.isNaN(d.getTime())) return '--:--:--';
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}:${pad2(d.getSeconds())}`;
}

/**
 * 秒数 -> 计时文本。不足一小时用 MM:SS，超过用 HH:MM:SS。
 * @param {number} secs
 */
export function formatSecs(secs) {
  const total = Math.max(0, Math.floor(Number(secs) || 0));
  const s = total % 60;
  const m = Math.floor(total / 60) % 60;
  const h = Math.floor(total / 3600);
  return h > 0 ? `${pad2(h)}:${pad2(m)}:${pad2(s)}` : `${pad2(m)}:${pad2(s)}`;
}

/**
 * 秒数 -> 胶片条上的短时长标签（如 12s / 3:20）。
 * @param {number} secs
 */
export function formatDuration(secs) {
  const total = Math.max(0, Math.round(Number(secs) || 0));
  if (total < 60) return `${total}s`;
  return formatSecs(total);
}

/** 帧率文本，0 时用占位符。 */
export function formatFps(fps) {
  const n = Math.round(Number(fps) || 0);
  return n > 0 ? `${n} FPS` : '— FPS';
}

/** 分辨率文本，未知时用占位符。 */
export function formatResolution(width, height) {
  const w = Math.round(Number(width) || 0);
  const h = Math.round(Number(height) || 0);
  return w > 0 && h > 0 ? `${w} × ${h}` : '— × —';
}

/** 字节数 -> 人类可读文本。 */
export function formatBytes(bytes) {
  const n = Number(bytes) || 0;
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${Math.round(n / 1024)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

/**
 * 校验自定义比例输入。非法返回 null（界面保持上一次有效布局并给出提示）。
 * 允许 "16" / "16.0" / " 16 " / "16,5"（逗号当小数点）。
 * @param {string|number} width
 * @param {string|number} height
 * @returns {[number, number] | null}
 */
export function parseRatioInput(width, height) {
  const toNumber = (v) => {
    if (typeof v === 'number') return v;
    const text = String(v ?? '').trim().replace(',', '.');
    if (!text) return NaN;
    const n = Number(text);
    return Number.isFinite(n) ? n : NaN;
  };
  const w = toNumber(width);
  const h = toNumber(height);
  if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0) return null;
  if (w > 100 || h > 100) return null;
  return [w, h];
}

/**
 * 缩短路径，保留结尾的文件/目录名，便于在窄面板里展示。
 * @param {string} path
 * @param {number} [max]
 */
export function shortPath(path, max = 34) {
  const text = String(path ?? '');
  if (!text) return '';
  if (text.length <= max) return text;
  const keep = Math.max(6, max - 1);
  const tail = text.slice(text.length - keep);
  const cut = tail.indexOf('/');
  return '…' + (cut > 0 ? tail.slice(cut) : tail);
}

/**
 * 后端抛出的错误（契约里是字符串）-> 一句给用户看的中文提示。
 * @param {unknown} error
 * @param {string} fallback
 */
export function friendlyError(error, fallback = '操作失败，请重试') {
  let text = '';
  if (typeof error === 'string') text = error;
  else if (error && typeof error === 'object') {
    text = String(error.message ?? error.error ?? error.detail ?? '');
  }
  text = text.trim();
  if (!text) return fallback;
  // 一些底层报错带上 ffmpeg 的英文原文，补一句可执行的建议
  if (/ffmpeg/i.test(text) && !/brew|安装|install/i.test(text)) {
    text += '（若未安装 ffmpeg，可执行 brew install ffmpeg）';
  }
  return text;
}
