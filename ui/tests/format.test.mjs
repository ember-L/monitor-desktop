import test from 'node:test';
import assert from 'node:assert/strict';

import {
  pad2,
  clamp,
  formatClock,
  formatSecs,
  formatDuration,
  formatFps,
  formatResolution,
  formatBytes,
  parseRatioInput,
  shortPath,
  friendlyError,
} from '../format.js';

test('pad2 补零', () => {
  assert.equal(pad2(0), '00');
  assert.equal(pad2(7), '07');
  assert.equal(pad2(123), '123');
  assert.equal(pad2(-5), '05');
  assert.equal(pad2('9'), '09');
});

test('clamp 夹取并处理非法值', () => {
  assert.equal(clamp(5, 0, 10), 5);
  assert.equal(clamp(-1, 0, 10), 0);
  assert.equal(clamp(99, 0, 10), 10);
  assert.equal(clamp(NaN, 3, 9), 3);
});

test('formatClock 输出 HH:MM:SS', () => {
  const date = new Date(2026, 0, 2, 3, 4, 5);
  assert.equal(formatClock(date), '03:04:05');
  assert.equal(formatClock(new Date(2026, 10, 9, 23, 59, 59)), '23:59:59');
  assert.equal(formatClock('not-a-date'), '--:--:--');
});

test('formatSecs 在一小时内用 MM:SS，超过用 HH:MM:SS', () => {
  assert.equal(formatSecs(0), '00:00');
  assert.equal(formatSecs(9.9), '00:09');
  assert.equal(formatSecs(61), '01:01');
  assert.equal(formatSecs(3599), '59:59');
  assert.equal(formatSecs(3661), '01:01:01');
  assert.equal(formatSecs(-8), '00:00');
  assert.equal(formatSecs(undefined), '00:00');
});

test('formatDuration 生成胶片条短标签', () => {
  assert.equal(formatDuration(0), '0s');
  assert.equal(formatDuration(12.4), '12s');
  assert.equal(formatDuration(125), '02:05');
});

test('formatFps / formatResolution 未知值有占位符', () => {
  assert.equal(formatFps(30), '30 FPS');
  assert.equal(formatFps(0), '— FPS');
  assert.equal(formatResolution(1920, 1080), '1920 × 1080');
  assert.equal(formatResolution(0, 0), '— × —');
});

test('formatBytes 人类可读', () => {
  assert.equal(formatBytes(512), '512 B');
  assert.equal(formatBytes(2048), '2 KB');
  assert.equal(formatBytes(5 * 1024 * 1024), '5.0 MB');
});

test('parseRatioInput 只接受合理正数', () => {
  assert.deepEqual(parseRatioInput('16', '9'), [16, 9]);
  assert.deepEqual(parseRatioInput(' 4 ', '3'), [4, 3]);
  assert.deepEqual(parseRatioInput('16,5', '9'), [16.5, 9]);
  assert.equal(parseRatioInput('0', '9'), null);
  assert.equal(parseRatioInput('-16', '9'), null);
  assert.equal(parseRatioInput('abc', '9'), null);
  assert.equal(parseRatioInput('', ''), null);
  assert.equal(parseRatioInput('400', '9'), null);
});

test('shortPath 保留文件名', () => {
  assert.equal(shortPath('/tmp/a.jpg'), '/tmp/a.jpg');
  const long = '/Users/someone/Movies/MONITOR/captures/VID_20260101_120000.mp4';
  const cut = shortPath(long, 30);
  assert.ok(cut.length <= 30);
  assert.ok(cut.endsWith('VID_20260101_120000.mp4'));
});

test('friendlyError 把后端字符串转成友好提示', () => {
  assert.equal(friendlyError('没有可拍摄的画面'), '没有可拍摄的画面');
  assert.equal(friendlyError(new Error('设备被占用')), '设备被占用');
  assert.equal(friendlyError(null, '操作失败'), '操作失败');
  assert.match(friendlyError('ffmpeg exited 1', '失败'), /brew install ffmpeg/);
  // 已经带建议的报错不再重复追加
  const once = friendlyError('没有找到 ffmpeg，请先安装', '失败');
  assert.equal(once, '没有找到 ffmpeg，请先安装');
});
