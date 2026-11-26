/**
 * 取景布局：把任意尺寸的源画面放进指定比例的显示框。
 *
 * 语义与旧 egui 版 `src-tauri/src/ratio.rs` 的 `ratio::layout` 一致：
 *   fit     完整显示，框内留黑边（等比缩放）
 *   fill    等比放大到填满比例框，超出部分居中裁切
 *   stretch 拉伸变形填满比例框
 *
 * 纯函数、零依赖、不碰 DOM，可直接在 `node --test` 下测试。
 *
 * @typedef {{x:number,y:number,w:number,h:number}} Rect
 * @typedef {{box:Rect,dest:Rect,uv:Rect}} Layout
 */

/** 预设比例（顺序与 egui 版一致，最后一项是「自定义」）。 */
export const PRESETS = [
  { id: 'source', label: '跟随源', ratio: null },
  { id: '16:9', label: '16:9', ratio: [16, 9] },
  { id: '9:16', label: '9:16', ratio: [9, 16] },
  { id: '4:3', label: '4:3', ratio: [4, 3] },
  { id: '3:4', label: '3:4', ratio: [3, 4] },
  { id: '3:2', label: '3:2', ratio: [3, 2] },
  { id: '2:3', label: '2:3', ratio: [2, 3] },
  { id: '21:9', label: '21:9', ratio: [21, 9] },
  { id: '1:1', label: '1:1', ratio: [1, 1] },
  { id: 'custom', label: '自定义…', ratio: null, custom: true },
];

/** 显示模式（与 egui 版同样的三段选择）。 */
export const MODES = [
  { id: 'fit', label: '适应', hint: '完整画面，比例框内留黑边' },
  { id: 'fill', label: '裁切', hint: '画面裁切填满比例框' },
  { id: 'stretch', label: '拉伸', hint: '画面拉伸变形填满比例框' },
];

/** 完整采样区域（不裁切）。 */
export const FULL_UV = Object.freeze({ x: 0, y: 0, w: 1, h: 1 });

const MODE_IDS = new Set(MODES.map((m) => m.id));

function finite(value, fallback) {
  const n = Number(value);
  return Number.isFinite(n) ? n : fallback;
}

function positive(value, fallback) {
  const n = Number(value);
  return Number.isFinite(n) && n > 0 ? n : fallback;
}

/**
 * 规范化比例输入，非法值返回 null（调用方回退到「跟随源」）。
 * 接受 [w, h] / {w, h} / {width, height} 三种写法。
 * @param {unknown} ratio
 * @returns {[number, number] | null}
 */
export function normalizeRatio(ratio) {
  let pair = null;
  if (Array.isArray(ratio)) pair = [ratio[0], ratio[1]];
  else if (ratio && typeof ratio === 'object') {
    pair = [ratio.w ?? ratio.width, ratio.h ?? ratio.height];
  }
  if (!pair) return null;
  const w = Number(pair[0]);
  const h = Number(pair[1]);
  if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0) return null;
  return [w, h];
}

/**
 * 把下拉选择（预设 id / 自定义宽高）解析成比例。
 * 自定义非法时返回 null，布局保持「跟随源」。
 * @param {string} selection
 * @param {string|number} customW
 * @param {string|number} customH
 * @returns {[number, number] | null}
 */
export function resolveRatio(selection, customW, customH) {
  const preset = PRESETS.find((p) => p.id === selection);
  if (preset && preset.custom) {
    return normalizeRatio([
      Number(String(customW ?? '').trim().replace(',', '.')),
      Number(String(customH ?? '').trim().replace(',', '.')),
    ]);
  }
  return normalizeRatio(preset ? preset.ratio : null);
}

/**
 * 计算取景布局。
 *
 * @param {number} srcW 源画面宽（像素）
 * @param {number} srcH 源画面高（像素）
 * @param {number} availW 可用区域宽（CSS 像素）
 * @param {number} availH 可用区域高（CSS 像素）
 * @param {[number, number] | number | null} ratio 目标比例；null/非法 = 跟随源
 * @param {'fit'|'fill'|'stretch'} mode 显示模式
 * @returns {Layout} box=比例框，dest=实际绘制矩形，uv=归一化采样区域
 */
export function computeLayout(srcW, srcH, availW, availH, ratio = null, mode = 'fit') {
  const sw = Math.max(1, positive(srcW, 1));
  const sh = Math.max(1, positive(srcH, 1));
  const aw = Math.max(1, positive(availW, 1));
  const ah = Math.max(1, positive(availH, 1));

  const r = normalizeRatio(ratio);
  const target = r ? r[0] / r[1] : sw / sh;

  // 比例框：可用区域内能放下的最大 target 比例矩形
  const wider = aw / ah > target;
  const boxW = wider ? ah * target : aw;
  const boxH = wider ? ah : aw / target;
  const box = { x: (aw - boxW) / 2, y: (ah - boxH) / 2, w: boxW, h: boxH };

  const kind = MODE_IDS.has(mode) ? mode : 'fit';

  if (kind === 'stretch') {
    return { box, dest: { ...box }, uv: { ...FULL_UV } };
  }

  if (kind === 'fill') {
    const scale = Math.max(boxW / sw, boxH / sh);
    const visW = Math.min(boxW / scale, sw);
    const visH = Math.min(boxH / scale, sh);
    const u0 = (sw - visW) / 2 / sw;
    const v0 = (sh - visH) / 2 / sh;
    return {
      box,
      dest: { ...box },
      uv: { x: u0, y: v0, w: visW / sw, h: visH / sh },
    };
  }

  // fit
  const scale = Math.min(boxW / sw, boxH / sh);
  const dw = sw * scale;
  const dh = sh * scale;
  return {
    box,
    dest: { x: (aw - dw) / 2, y: (ah - dh) / 2, w: dw, h: dh },
    uv: { ...FULL_UV },
  };
}

/**
 * 水平镜像采样区域（预览与拍照共用同一套镜像逻辑）。
 * @param {Rect} uv
 * @returns {Rect}
 */
export function mirrorUv(uv) {
  return {
    x: 1 - finite(uv.x, 0) - finite(uv.w, 1),
    y: finite(uv.y, 0),
    w: finite(uv.w, 1),
    h: finite(uv.h, 1),
  };
}

/**
 * 把指定比例的沙盒矩形描述成便于调试/断言的字符串。
 * @param {Layout} layout
 */
export function describeLayout(layout) {
  const f = (n) => Math.round(n * 10) / 10;
  return `box ${f(layout.box.w)}×${f(layout.box.h)} @${f(layout.box.x)},${f(layout.box.y)} · dest ${f(layout.dest.w)}×${f(layout.dest.h)} · uv ${f(layout.uv.w)}×${f(layout.uv.h)}`;
}
