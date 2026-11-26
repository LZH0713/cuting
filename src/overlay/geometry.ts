/**
 * 选区几何的纯函数：拖拽换算、窗口吸附、裁剪像素换算。
 * 全部无副作用、不碰 DOM，方便单元测试（Rust 侧 `Frame::to_pixels`
 * 是 cropPixelBounds 的对照实现，两边必须保持一致）。
 */

import type { Rect, StartPayload } from '../lib/ipc';

/** 选区的最小边长（CSS px）。 */
export const MIN_SELECTION = 2;

export const clamp = (v: number, lo: number, hi: number) => Math.min(Math.max(v, lo), hi);

export type Drag = {
  kind: 'nw' | 'n' | 'ne' | 'e' | 'se' | 's' | 'sw' | 'w' | 'new' | 'move';
  startX: number;
  startY: number;
  origin: Rect;
  moved: boolean;
};

/** 把指针拖到 `(x, y)` 时选区应有的矩形（已按屏幕边界钳制）。 */
export function applyDrag(d: Drag, x: number, y: number, width: number, height: number): Rect {
  const o = d.origin;
  if (d.kind === 'new') {
    const x1 = clamp(d.startX, 0, width);
    const y1 = clamp(d.startY, 0, height);
    const x2 = clamp(x, 0, width);
    const y2 = clamp(y, 0, height);
    return { x: Math.min(x1, x2), y: Math.min(y1, y2), w: Math.abs(x2 - x1), h: Math.abs(y2 - y1) };
  }
  const dx = x - d.startX;
  const dy = y - d.startY;
  if (d.kind === 'move') {
    return { x: clamp(o.x + dx, 0, width - o.w), y: clamp(o.y + dy, 0, height - o.h), w: o.w, h: o.h };
  }
  let { x: left, y: top, w, h } = o;
  const right = o.x + o.w;
  const bottom = o.y + o.h;
  if (d.kind.includes('w')) {
    left = clamp(o.x + dx, 0, right - MIN_SELECTION);
    w = right - left;
  }
  if (d.kind.includes('e')) {
    w = clamp(right + dx - o.x, MIN_SELECTION, width - o.x);
  }
  if (d.kind.includes('n')) {
    top = clamp(o.y + dy, 0, bottom - MIN_SELECTION);
    h = bottom - top;
  }
  if (d.kind.includes('s')) {
    h = clamp(bottom + dy - o.y, MIN_SELECTION, height - o.y);
  }
  return { x: left, y: top, w, h };
}

/** 指针落在哪个窗口轮廓上（`windows` 按前台优先排序，取第一个命中的）。 */
export function windowAt(x: number, y: number, p: StartPayload): Rect | null {
  for (const w of p.windows) {
    if (w.rect.x <= x && x < w.rect.x + w.rect.w && w.rect.y <= y && y < w.rect.y + w.rect.h) {
      return { ...w.rect };
    }
  }
  return null;
}

/**
 * 选区对应的显示器物理像素裁剪框。与 Rust `Frame::to_pixels` 保持一致：
 * 物理坐标取整；负坐标与越界部分在 CSS 空间先钳制（对整数边界两者等价）。
 */
export function cropPixelBounds(
  sel: Rect,
  scale: number,
  displayW: number,
  displayH: number,
): { x0: number; y0: number; x1: number; y1: number } {
  return {
    x0: Math.round(Math.max(0, sel.x) * scale),
    y0: Math.round(Math.max(0, sel.y) * scale),
    x1: Math.round(Math.min(displayW, sel.x + sel.w) * scale),
    y1: Math.round(Math.min(displayH, sel.y + sel.h) * scale),
  };
}
