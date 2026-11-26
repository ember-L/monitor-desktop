import test from 'node:test';
import assert from 'node:assert/strict';

import {
  PRESETS,
  MODES,
  computeLayout,
  normalizeRatio,
  resolveRatio,
  mirrorUv,
  describeLayout,
} from '../layout.js';

const near = (actual, expected, epsilon = 1e-6) =>
  Math.abs(actual - expected) <= epsilon;

test('跟随源：等比缩放并居中，不裁切', () => {
  const layout = computeLayout(1920, 1080, 800, 600, null, 'fit');
  assert.ok(near(layout.dest.w, 800));
  assert.ok(near(layout.dest.h, 450));
  assert.ok(near(layout.dest.x, 0));
  assert.ok(near(layout.dest.y, 75));
  assert.deepEqual(layout.uv, { x: 0, y: 0, w: 1, h: 1 });
  // 居中等比
  assert.ok(near(layout.dest.x + layout.dest.w / 2, 400));
  assert.ok(near(layout.dest.y + layout.dest.h / 2, 300));
  assert.ok(near(layout.dest.w / layout.dest.h, 16 / 9, 1e-9));
  // 比例框与画面同比例
  assert.ok(near(layout.box.w, 800));
  assert.ok(near(layout.box.h, 450));
});

test('跟随源：竖屏画面在横框内左右留黑边', () => {
  const layout = computeLayout(1080, 1920, 800, 600, null, 'fit');
  assert.ok(near(layout.dest.h, 600));
  assert.ok(near(layout.dest.w, 337.5));
  assert.ok(near(layout.dest.x, 231.25));
  assert.ok(near(layout.dest.y, 0));
  assert.equal(layout.uv.w, 1);
  assert.equal(layout.uv.h, 1);
});

test('16:9 裁切：竖屏源上下被裁，uv.width ≈ 1', () => {
  const layout = computeLayout(1080, 2400, 800, 600, [16, 9], 'fill');
  assert.ok(near(layout.box.w / layout.box.h, 16 / 9, 1e-9));
  assert.ok(near(layout.dest.w, layout.box.w));
  assert.ok(near(layout.dest.h, layout.box.h));
  assert.ok(near(layout.uv.w, 1, 1e-9));
  assert.ok(layout.uv.h < 0.5);
  // 上下对称裁切，采样区域垂直居中
  assert.ok(layout.uv.y > 0);
  assert.ok(near(layout.uv.y + layout.uv.h / 2, 0.5, 1e-9));
  assert.ok(near(layout.uv.y, (1 - layout.uv.h) / 2, 1e-9));
});

test('拉伸：填满整个比例框且采样完整', () => {
  const layout = computeLayout(640, 480, 800, 600, [1, 1], 'stretch');
  assert.ok(near(layout.dest.w, 600));
  assert.ok(near(layout.dest.h, 600));
  assert.ok(near(layout.dest.x, 100));
  assert.ok(near(layout.dest.y, 0));
  assert.deepEqual(layout.uv, { x: 0, y: 0, w: 1, h: 1 });
  assert.deepEqual(layout.dest, layout.box);
});

test('适应：4:3 源放进 21:9 框，左右留黑边', () => {
  const layout = computeLayout(640, 480, 800, 600, [21, 9], 'fit');
  assert.ok(near(layout.box.w / layout.box.h, 21 / 9, 1e-9));
  assert.ok(near(layout.box.w, 800));
  // 完整画面：等比缩放后比比例框窄，左右出现黑边
  assert.ok(layout.dest.w < layout.box.w);
  assert.ok(layout.dest.x > 0);
  assert.ok(near(layout.dest.x + layout.dest.w / 2, 400));
  assert.ok(near(layout.dest.w / layout.dest.h, 4 / 3, 1e-9));
  assert.deepEqual(layout.uv, { x: 0, y: 0, w: 1, h: 1 });
});

test('裁切：16:9 源放进 9:16 框，左右被裁且垂直完整', () => {
  const layout = computeLayout(1920, 1080, 800, 600, [9, 16], 'fill');
  assert.ok(near(layout.box.w / layout.box.h, 9 / 16, 1e-9));
  assert.ok(near(layout.uv.h, 1, 1e-9));
  assert.ok(layout.uv.w < 0.4);
  assert.ok(near(layout.uv.x + layout.uv.w / 2, 0.5, 1e-9));
  assert.ok(near(layout.dest.w, layout.box.w));
  assert.ok(near(layout.dest.h, layout.box.h));
});

test('自定义比例生效（2:1）', () => {
  const layout = computeLayout(1920, 1080, 800, 600, normalizeRatio([2, 1]), 'fill');
  assert.ok(near(layout.dest.w / layout.dest.h, 2, 1e-9));
});

test('非法比例回退到跟随源，且不产生 NaN', () => {
  for (const bad of [null, undefined, [0, 9], [-4, 3], [NaN, 3], [Infinity, 2], 'x', {}]) {
    const layout = computeLayout(1920, 1080, 800, 600, bad, 'fit');
    for (const rect of [layout.box, layout.dest, layout.uv]) {
      for (const value of [rect.x, rect.y, rect.w, rect.h]) {
        assert.ok(Number.isFinite(value), '布局出现非法数值');
      }
    }
    assert.ok(near(layout.dest.w, 800));
    assert.ok(near(layout.dest.h, 450));
  }
});

test('零尺寸输入不会崩，尺寸下限为 1', () => {
  const layout = computeLayout(0, 0, 0, 0, null, 'fit');
  for (const rect of [layout.box, layout.dest, layout.uv]) {
    for (const value of [rect.x, rect.y, rect.w, rect.h]) assert.ok(Number.isFinite(value));
  }
  const stretched = computeLayout(-10, -10, -5, -5, [16, 9], 'stretch');
  assert.ok(stretched.dest.w > 0 && stretched.dest.h > 0);
});

test('未知模式按适应处理', () => {
  const bogus = computeLayout(1920, 1080, 800, 600, null, 'bogus');
  const fit = computeLayout(1920, 1080, 800, 600, null, 'fit');
  assert.deepEqual(bogus.dest, fit.dest);
  assert.deepEqual(bogus.uv, fit.uv);
});

test('mirrorUv 水平翻转采样区域', () => {
  const flipped = mirrorUv({ x: 0.1, y: 0.2, w: 0.5, h: 0.6 });
  assert.ok(near(flipped.x, 0.4));
  assert.ok(near(flipped.y, 0.2));
  assert.ok(near(flipped.w, 0.5));
  assert.ok(near(flipped.h, 0.6));
  const back = mirrorUv(flipped);
  assert.ok(near(back.x, 0.1));
});

test('resolveRatio 支持预设 / 自定义 / 非法输入', () => {
  assert.deepEqual(resolveRatio('16:9'), [16, 9]);
  assert.deepEqual(resolveRatio('21:9'), [21, 9]);
  assert.equal(resolveRatio('source'), null);
  assert.deepEqual(resolveRatio('custom', '4', '3'), [4, 3]);
  assert.deepEqual(resolveRatio('custom', ' 16 ', '9'), [16, 9]);
  assert.deepEqual(resolveRatio('custom', '16,5', '9'), [16.5, 9]);
  assert.equal(resolveRatio('custom', '0', '9'), null);
  assert.equal(resolveRatio('custom', 'abc', '9'), null);
  assert.equal(resolveRatio('没有这个预设'), null);
});

test('PRESETS 顺序与旧 egui 版一致', () => {
  assert.deepEqual(
    PRESETS.map((preset) => preset.label),
    ['跟随源', '16:9', '9:16', '4:3', '3:4', '3:2', '2:3', '21:9', '1:1', '自定义…'],
  );
  assert.equal(PRESETS.filter((preset) => preset.custom).length, 1);
  assert.deepEqual(MODES.map((mode) => mode.id), ['fit', 'fill', 'stretch']);
  assert.ok(MODES.every((mode) => mode.label && mode.hint));
});

test('窗口最小尺寸下仍有合理布局（860×560 内的取景区）', () => {
  // 左侧面板 + 内边距后大约剩下 600×390 的取景区
  const layout = computeLayout(1920, 1080, 600, 390, [16, 9], 'fit');
  assert.ok(layout.dest.w <= 600 + 1e-9);
  assert.ok(layout.dest.h <= 390 + 1e-9);
  assert.ok(near(layout.dest.w / layout.dest.h, 16 / 9, 1e-9));
  assert.ok(layout.dest.x >= 0 && layout.dest.y >= 0);
  assert.equal(typeof describeLayout(layout), 'string');
});
