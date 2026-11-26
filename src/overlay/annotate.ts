/**
 * Annotation model and canvas rendering for the selection overlay.
 *
 * The frozen screenshot lives in a native view behind the page; this layer
 * only ever draws the strokes the user adds. Everything is stored in
 * selection-local CSS coordinates, so moving the selection carries the
 * annotations along, and re-rendering at any size clips them cleanly.
 *
 * Two stacked canvases keep drawing cheap: `committed` holds the finished
 * ops (plus the mosaic base pixels), `live` holds only the op in progress.
 * On confirm the ops are replayed into an offscreen canvas at the crop's
 * exact pixel size — that PNG rides to Rust and is composited over the crop,
 * so the pixels the user drew are the pixels that get copied or saved.
 */

export type AnnoTool = 'pen' | 'highlighter' | 'arrow' | 'line' | 'rect' | 'ellipse' | 'text' | 'mosaic';

export type AnnoOp =
  | { kind: 'pen'; color: string; width: number; points: [number, number][] }
  | { kind: 'highlighter'; color: string; width: number; points: [number, number][] }
  | { kind: 'arrow' | 'line' | 'rect' | 'ellipse'; color: string; width: number; x1: number; y1: number; x2: number; y2: number }
  | { kind: 'text'; color: string; size: number; x: number; y: number; text: string }
  | { kind: 'mosaic'; x1: number; y1: number; x2: number; y2: number };

/** Swatches shown in the palette, in display order. */
export const COLOR_PRESETS = ['#ef4444', '#f97316', '#facc15', '#22c55e', '#3b82f6', '#a855f7', '#111827', '#ffffff'];

/** Stroke widths in CSS px per preset slot (细/中/粗). */
export const WIDTH_PRESETS = [2, 4, 7];

/** Font size in CSS px per preset slot; text follows the stroke size. */
export const TEXT_SIZES = [16, 22, 30];

/** Mosaic block size in CSS px. */
const MOSAIC_BLOCK = 8;

const HIGHLIGHT_ALPHA = 0.35;
/** Minimum pointer travel (CSS px) before another pen point is recorded. */
const POINT_STEP = 1.5;
const ARROW_HEAD_MIN = 10;
const ARROW_HEAD_MAX = 26;
const FONT_FAMILY = "-apple-system, BlinkMacSystemFont, 'PingFang SC', 'Helvetica Neue', sans-serif";

/** The tool draws shapes (click-drag) instead of freehand strokes. */
export function isShapeTool(tool: AnnoTool): boolean {
  return tool === 'arrow' || tool === 'line' || tool === 'rect' || tool === 'ellipse' || tool === 'mosaic';
}

export class Annotator {
  /** Finished ops, in drawing order. */
  private ops: AnnoOp[] = [];
  private redoStack: AnnoOp[] = [];
  /** The op being drawn right now (rendered onto the live canvas). */
  private current: AnnoOp | null = null;
  private scale = 1;
  /** Mosaic base pixels (the crop), anchored at the selection's top-left. */
  private base: { image: HTMLImageElement; cssW: number; cssH: number } | null = null;
  /** The base pre-downscaled for pixelation. */
  private mosaicSource: HTMLCanvasElement | null = null;

  constructor(private committed: HTMLCanvasElement, private live: HTMLCanvasElement) {}

  get hasBase(): boolean {
    return this.base !== null;
  }

  get opCount(): number {
    return this.ops.length;
  }

  get redoCount(): number {
    return this.redoStack.length;
  }

  get isEmpty(): boolean {
    return this.ops.length === 0;
  }

  setScale(scale: number) {
    if (scale > 0) this.scale = scale;
  }

  /** Refit both canvases to the selection's CSS size and replay the ops. */
  resize(cssW: number, cssH: number) {
    const w = Math.max(1, Math.round(cssW * this.scale));
    const h = Math.max(1, Math.round(cssH * this.scale));
    for (const canvas of [this.committed, this.live]) {
      if (canvas.width !== w) canvas.width = w;
      if (canvas.height !== h) canvas.height = h;
      canvas.style.width = `${cssW}px`;
      canvas.style.height = `${cssH}px`;
    }
    this.renderCommitted();
  }

  /** Start the op `tool` draws at selection-local `(x, y)`. */
  begin(tool: AnnoTool, color: string, width: number, x: number, y: number) {
    if (tool === 'text') return;
    this.current =
      tool === 'pen' || tool === 'highlighter'
        ? { kind: tool, color, width, points: [[x, y]] }
        : tool === 'mosaic'
          ? { kind: 'mosaic', x1: x, y1: y, x2: x, y2: y }
          : { kind: tool, color, width, x1: x, y1: y, x2: x, y2: y };
    this.renderLive();
  }

  /** Extend the live op to selection-local `(x, y)`. */
  extend(x: number, y: number) {
    const op = this.current;
    if (!op) return;
    if (op.kind === 'pen' || op.kind === 'highlighter') {
      const last = op.points[op.points.length - 1];
      if (Math.abs(x - last[0]) + Math.abs(y - last[1]) < POINT_STEP) return;
      op.points.push([x, y]);
    } else if (op.kind !== 'text') {
      op.x2 = x;
      op.y2 = y;
    }
    this.renderLive();
  }

  /** Finish the live op (no-op while no drag is going on). */
  commit() {
    const op = this.current;
    if (!op) return;
    this.current = null;
    this.live.getContext('2d')!.clearRect(0, 0, this.live.width, this.live.height);
    // A click on a shape (no drag) draws nothing; a lone pen tap leaves a dot.
    const tiny =
      (op.kind === 'rect' || op.kind === 'ellipse' || op.kind === 'mosaic' || op.kind === 'arrow' || op.kind === 'line') &&
      Math.abs(op.x2 - op.x1) + Math.abs(op.y2 - op.y1) < 2;
    if (!tiny) {
      this.ops.push(op);
      this.redoStack = [];
      this.renderCommitted();
    }
  }

  /** Place committed text with its top-left at selection-local `(x, y)`. */
  addText(x: number, y: number, text: string, color: string, size: number) {
    if (!text.trim()) return;
    this.ops.push({ kind: 'text', color, size, x, y, text });
    this.redoStack = [];
    this.renderCommitted();
  }

  undo() {
    const op = this.ops.pop();
    if (!op) return;
    this.redoStack.push(op);
    this.renderCommitted();
  }

  redo() {
    const op = this.redoStack.pop();
    if (!op) return;
    this.ops.push(op);
    this.renderCommitted();
  }

  /** Drop everything (selection cleared). */
  clear() {
    this.ops = [];
    this.redoStack = [];
    this.current = null;
    this.base = null;
    this.mosaicSource = null;
    this.renderCommitted();
    this.live.getContext('2d')!.clearRect(0, 0, this.live.width, this.live.height);
  }

  /**
   * Load the crop pixels for the mosaic tool. They land under the ops, so
   * afterwards the exported layer is a complete image, not a transparent one.
   */
  loadBase(url: string, cssW: number, cssH: number): Promise<void> {
    return new Promise((resolve, reject) => {
      const image = new Image();
      image.onload = () => {
        this.base = { image, cssW, cssH };
        // Pre-downscale: one mosaic-source pixel becomes one visible block.
        const bw = Math.max(1, Math.round(cssW / MOSAIC_BLOCK));
        const bh = Math.max(1, Math.round(cssH / MOSAIC_BLOCK));
        const source = document.createElement('canvas');
        source.width = bw;
        source.height = bh;
        const ctx = source.getContext('2d')!;
        ctx.imageSmoothingQuality = 'low';
        ctx.drawImage(image, 0, 0, bw, bh);
        this.mosaicSource = source;
        this.renderCommitted();
        resolve();
      };
      image.onerror = () => reject(new Error('标注底图加载失败'));
      image.src = url;
    });
  }

  /**
   * Replay every op into an offscreen canvas of the crop's exact pixel size.
   * Ops live in selection-local CSS coordinates and the crop starts at the
   * selection's origin (annotate mode rejects off-display selections), so a
   * pure scale maps them onto the crop — no extra translation wanted.
   * Returns bare base64 PNG data (no `data:` prefix), or null when empty.
   */
  exportBase64(targetW: number, targetH: number): string | null {
    if (this.ops.length === 0) return null;
    const canvas = document.createElement('canvas');
    canvas.width = Math.max(1, targetW);
    canvas.height = Math.max(1, targetH);
    const ctx = canvas.getContext('2d');
    if (!ctx) return null;
    ctx.setTransform(this.scale, 0, 0, this.scale, 0, 0);
    this.renderAll(ctx);
    const url = canvas.toDataURL('image/png');
    return url.slice(url.indexOf(',') + 1);
  }

  // ------------------------------------------------------------- rendering

  private renderCommitted() {
    const ctx = this.committed.getContext('2d')!;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, this.committed.width, this.committed.height);
    ctx.setTransform(this.scale, 0, 0, this.scale, 0, 0);
    this.renderAll(ctx);
  }

  private renderLive() {
    const ctx = this.live.getContext('2d')!;
    ctx.setTransform(1, 0, 0, 1, 0, 0);
    ctx.clearRect(0, 0, this.live.width, this.live.height);
    if (!this.current) return;
    ctx.setTransform(this.scale, 0, 0, this.scale, 0, 0);
    drawOp(ctx, this.current, this.mosaicSource);
  }

  private renderAll(ctx: CanvasRenderingContext2D) {
    if (this.base) {
      // Mosaic pixels are anchored to the selection's top-left at the size
      // they were fetched at; later resizes keep them put.
      ctx.drawImage(this.base.image, 0, 0, this.base.cssW, this.base.cssH);
    }
    for (const op of this.ops) drawOp(ctx, op, this.mosaicSource);
  }
}

function drawOp(ctx: CanvasRenderingContext2D, op: AnnoOp, mosaicSource: HTMLCanvasElement | null) {
  if (op.kind === 'pen' || op.kind === 'highlighter') {
    strokeFreehand(ctx, op.points, op.color, op.width, op.kind === 'highlighter' ? HIGHLIGHT_ALPHA : 1);
    return;
  }
  if (op.kind === 'text') {
    ctx.save();
    ctx.fillStyle = op.color;
    ctx.font = `${op.size}px ${FONT_FAMILY}`;
    ctx.textBaseline = 'top';
    const lineHeight = op.size * 1.3;
    op.text.split('\n').forEach((line, i) => ctx.fillText(line, op.x, op.y + i * lineHeight));
    ctx.restore();
    return;
  }
  const x = Math.min(op.x1, op.x2);
  const y = Math.min(op.y1, op.y2);
  const w = Math.abs(op.x2 - op.x1);
  const h = Math.abs(op.y2 - op.y1);
  ctx.save();
  if (op.kind !== 'mosaic') {
    ctx.strokeStyle = op.color;
    ctx.lineWidth = op.width;
  }
  ctx.lineCap = 'round';
  ctx.lineJoin = 'round';
  ctx.beginPath();
  if (op.kind === 'line') {
    ctx.moveTo(op.x1, op.y1);
    ctx.lineTo(op.x2, op.y2);
    ctx.stroke();
  } else if (op.kind === 'arrow') {
    ctx.moveTo(op.x1, op.y1);
    ctx.lineTo(op.x2, op.y2);
    ctx.stroke();
    const angle = Math.atan2(op.y2 - op.y1, op.x2 - op.x1);
    const head = Math.min(ARROW_HEAD_MAX, Math.max(ARROW_HEAD_MIN, op.width * 3));
    for (const spread of [Math.PI * 0.78, -Math.PI * 0.78]) {
      ctx.moveTo(op.x2, op.y2);
      ctx.lineTo(op.x2 + Math.cos(angle + spread) * head, op.y2 + Math.sin(angle + spread) * head);
    }
    ctx.stroke();
  } else if (op.kind === 'rect') {
    ctx.strokeRect(x, y, w, h);
  } else if (op.kind === 'ellipse') {
    ctx.ellipse(x + w / 2, y + h / 2, w / 2, h / 2, 0, 0, Math.PI * 2);
    ctx.stroke();
  } else if (op.kind === 'mosaic') {
    ctx.imageSmoothingEnabled = false;
    if (mosaicSource && w > 0.5 && h > 0.5) {
      const sx = Math.max(0, x / MOSAIC_BLOCK);
      const sy = Math.max(0, y / MOSAIC_BLOCK);
      const sw = Math.min(w / MOSAIC_BLOCK, mosaicSource.width - sx);
      const sh = Math.min(h / MOSAIC_BLOCK, mosaicSource.height - sy);
      if (sw > 0 && sh > 0) ctx.drawImage(mosaicSource, sx, sy, sw, sh, x, y, w, h);
    }
  }
  ctx.restore();
}

function strokeFreehand(
  ctx: CanvasRenderingContext2D,
  points: [number, number][],
  color: string,
  width: number,
  alpha: number,
) {
  if (points.length === 0) return;
  ctx.save();
  ctx.globalAlpha = alpha;
  ctx.strokeStyle = color;
  ctx.lineWidth = width;
  ctx.lineCap = 'round';
  ctx.lineJoin = 'round';
  if (points.length === 1) {
    // A tap leaves a dot.
    ctx.fillStyle = color;
    ctx.beginPath();
    ctx.arc(points[0][0], points[0][1], width / 2, 0, Math.PI * 2);
    ctx.fill();
    ctx.restore();
    return;
  }
  ctx.beginPath();
  ctx.moveTo(points[0][0], points[0][1]);
  // Quadratic midpoints turn the raw pointer samples into a smooth curve.
  for (let i = 1; i < points.length - 1; i++) {
    const [x1, y1] = points[i];
    const [x2, y2] = points[i + 1];
    ctx.quadraticCurveTo(x1, y1, (x1 + x2) / 2, (y1 + y2) / 2);
  }
  const last = points[points.length - 1];
  ctx.lineTo(last[0], last[1]);
  ctx.stroke();
  ctx.restore();
}
