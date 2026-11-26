import { describe, expect, it } from 'vitest';

import type { Rect, StartPayload } from '../lib/ipc';
import { applyDrag, clamp, cropPixelBounds, MIN_SELECTION, windowAt, type Drag } from './geometry';

const rect = (x: number, y: number, w: number, h: number): Rect => ({ x, y, w, h });

const drag = (kind: Drag['kind'], startX: number, startY: number, origin: Rect): Drag => ({
  kind,
  startX,
  startY,
  origin,
  moved: true,
});

describe('clamp', () => {
  it('keeps values inside the range', () => {
    expect(clamp(5, 0, 10)).toBe(5);
    expect(clamp(-1, 0, 10)).toBe(0);
    expect(clamp(11, 0, 10)).toBe(10);
  });
});

describe('applyDrag — 新建选区', () => {
  it('把反向拖拽归一化为正矩形', () => {
    // 从右下往左上拖：起点 (100,100)，拖到 (40,30)
    const r = applyDrag(drag('new', 100, 100, rect(0, 0, 0, 0)), 40, 30, 1000, 800);
    expect(r).toEqual(rect(40, 30, 60, 70));
  });

  it('拖出屏幕的部分被钳制在屏幕内', () => {
    const r = applyDrag(drag('new', 900, 700, rect(0, 0, 0, 0)), 1200, 900, 1000, 800);
    expect(r).toEqual(rect(900, 700, 100, 100));
  });

  it('起点本身也会被钳制（起点在屏幕外）', () => {
    const r = applyDrag(drag('new', -50, -50, rect(0, 0, 0, 0)), 60, 80, 1000, 800);
    expect(r).toEqual(rect(0, 0, 60, 80));
  });
});

describe('applyDrag — 移动', () => {
  it('整体平移并钳制在屏幕内', () => {
    const r = applyDrag(drag('move', 100, 100, rect(200, 200, 300, 200)), 60, 40, 1000, 800);
    expect(r).toEqual(rect(160, 140, 300, 200));
  });

  it('不会把选框推出右/下边界', () => {
    const r = applyDrag(drag('move', 0, 0, rect(700, 600, 300, 200)), 500, 400, 1000, 800);
    expect(r).toEqual(rect(700, 600, 300, 200));
  });
});

describe('applyDrag — 缩放', () => {
  it('东向拉伸', () => {
    const r = applyDrag(drag('e', 500, 0, rect(100, 100, 400, 200)), 700, 0, 1000, 800);
    expect(r).toEqual(rect(100, 100, 600, 200));
  });

  it('西向收缩时右边界不动，且不小于最小宽度', () => {
    // 拖过头（越过右边界），钳制到 right - MIN
    const r = applyDrag(drag('w', 100, 0, rect(100, 100, 400, 200)), 600, 0, 1000, 800);
    expect(r).toEqual(rect(498, 100, 2, 200));
    expect(MIN_SELECTION).toBe(2);
  });

  it('北向拉伸时下边界不动，可翻转为负方向', () => {
    const r = applyDrag(drag('n', 0, 200, rect(100, 200, 300, 100)), 0, 60, 1000, 800);
    expect(r).toEqual(rect(100, 60, 300, 240));
  });

  it('南向拉伸不会超出屏幕', () => {
    const r = applyDrag(drag('s', 0, 700, rect(100, 600, 300, 100)), 0, 900, 1000, 800);
    expect(r).toEqual(rect(100, 600, 300, 200));
  });
});

describe('windowAt — 窗口吸附', () => {
  const payload = {
    windows: [
      { id: 1, layer: 0, pid: 1, rect: rect(0, 0, 400, 300) },
      { id: 2, layer: 0, pid: 2, rect: rect(200, 100, 400, 300) },
    ],
  } as unknown as StartPayload;

  it('返回按 z 序第一个命中的窗口', () => {
    // (300,200) 在两个窗口内，取靠前的 id=1
    expect(windowAt(300, 200, payload)?.x).toBe(0);
    // (500,300) 只在第二个窗口内
    expect(windowAt(500, 300, payload)?.x).toBe(200);
  });

  it('命中窗口返回其拷贝而不是引用', () => {
    const r = windowAt(10, 10, payload)!;
    r.w = 999;
    expect(payload.windows[0].rect.w).toBe(400);
  });

  it('没有命中时返回 null', () => {
    expect(windowAt(900, 700, payload)).toBeNull();
  });
});

describe('cropPixelBounds — 与 Rust Frame::to_pixels 保持一致', () => {
  it('普通选区按 scale 取整', () => {
    expect(cropPixelBounds(rect(10, 20, 300, 200), 2, 1512, 982)).toEqual({ x0: 20, y0: 40, x1: 620, y1: 440 });
  });

  it('负坐标钳制到 0（annotate 模式其实不允许，只是兜底）', () => {
    expect(cropPixelBounds(rect(-5, -5, 20, 20), 2, 100, 100)).toEqual({ x0: 0, y0: 0, x1: 30, y1: 30 });
  });

  it('越界部分钳制到显示器物理尺寸', () => {
    expect(cropPixelBounds(rect(90, 90, 50, 50), 2, 100, 100)).toEqual({ x0: 180, y0: 180, x1: 200, y1: 200 });
  });

  it('小数坐标取整（正值时 JS Math.round 与 Rust f64::round 结果一致）', () => {
    expect(cropPixelBounds(rect(10.5, 0, 3.5, 10), 2, 100, 100)).toEqual({ x0: 21, y0: 0, x1: 28, y1: 20 });
  });
});
