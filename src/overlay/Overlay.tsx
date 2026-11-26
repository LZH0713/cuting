import { useEffect, useRef, useState } from 'react';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { call, type Rect, type StartPayload } from '../lib/ipc';
import { Annotator, COLOR_PRESETS, TEXT_SIZES, WIDTH_PRESETS, type AnnoTool } from './annotate';
import { applyDrag, clamp, cropPixelBounds, MIN_SELECTION as MIN, windowAt, type Drag } from './geometry';

type Dir = Exclude<Drag['kind'], 'new' | 'move'>;
type Action = 'copy' | 'save' | 'ocr' | 'translate';
type Phase = 'select' | 'toolbar' | 'busy';

const HANDLE_DIRS: Dir[] = ['nw', 'n', 'ne', 'e', 'se', 's', 'sw', 'w'];
const ANNO_TOOLS: AnnoTool[] = ['pen', 'highlighter', 'arrow', 'line', 'rect', 'ellipse', 'text', 'mosaic'];
const ANNO_TITLES: Record<AnnoTool, string> = {
  pen: '画笔',
  highlighter: '荧光笔',
  arrow: '箭头',
  line: '直线',
  rect: '矩形',
  ellipse: '椭圆',
  text: '文字',
  mosaic: '马赛克',
};

/**
 * 手绘工具的光标：铅笔 / 荧光笔造型（热点在笔尖），其余工具用系统光标。
 * WebKit 不支持 SVG data-URL 光标（静默退回 fallback 十字），所以用运行时
 * canvas 画一份 PNG——PNG 光标在 WKWebView 上是支持的。
 */
const canvasCursor = (hot: [number, number], draw: (ctx: CanvasRenderingContext2D) => void): string => {
  const canvas = document.createElement('canvas');
  canvas.width = 18;
  canvas.height = 18;
  const ctx = canvas.getContext('2d');
  if (!ctx) return 'crosshair';
  ctx.lineWidth = 1.2;
  ctx.lineJoin = 'round';
  ctx.strokeStyle = 'black';
  draw(ctx);
  return `url(${canvas.toDataURL('image/png')}) ${hot[0]} ${hot[1]}, crosshair`;
};

const PEN_CURSOR = canvasCursor([2, 16], (ctx) => {
  ctx.fillStyle = 'white';
  ctx.beginPath();
  ctx.moveTo(1.5, 16.5); // 笔尖
  ctx.lineTo(3, 12);
  ctx.lineTo(13, 2); // 笔杆上沿
  ctx.lineTo(16, 5); // 杆顶
  ctx.lineTo(6, 15); // 笔杆下沿
  ctx.closePath();
  ctx.fill();
  ctx.stroke();
  ctx.beginPath(); // 笔尖分界线
  ctx.moveTo(3, 12);
  ctx.lineTo(6, 15);
  ctx.stroke();
});
const HIGHLIGHTER_CURSOR = canvasCursor([3, 15], (ctx) => {
  ctx.fillStyle = '#facc15';
  ctx.beginPath();
  ctx.moveTo(3, 13.5);
  ctx.lineTo(9.5, 4.5); // 斜切笔头
  ctx.lineTo(13, 8);
  ctx.lineTo(6.5, 14);
  ctx.closePath();
  ctx.fill();
  ctx.stroke();
  ctx.beginPath(); // 底部划线
  ctx.moveTo(2.5, 16);
  ctx.lineTo(15.5, 16);
  ctx.lineWidth = 1.4;
  ctx.stroke();
});
const MOSAIC_CURSOR = canvasCursor([8, 8], (ctx) => {
  ctx.fillStyle = 'white';
  ctx.fillRect(0.6, 0.6, 14.8, 14.8);
  ctx.strokeRect(0.6, 0.6, 14.8, 14.8);
  ctx.strokeStyle = '#999';
  ctx.lineWidth = 1;
  for (const at of [5.3, 10.6]) {
    ctx.beginPath();
    ctx.moveTo(at, 1);
    ctx.lineTo(at, 16);
    ctx.moveTo(1, at);
    ctx.lineTo(16, at);
    ctx.stroke();
  }
});
const ANNO_CURSORS: Record<AnnoTool, string> = {
  pen: PEN_CURSOR,
  highlighter: HIGHLIGHTER_CURSOR,
  arrow: 'crosshair',
  line: 'crosshair',
  rect: 'crosshair',
  ellipse: 'crosshair',
  text: 'text',
  mosaic: MOSAIC_CURSOR,
};

const inside = (r: Rect, x: number, y: number) => x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;

function Icon({ name }: {
  name:
    | 'copy' | 'save' | 'ocr' | 'translate' | 'redo' | 'close' | 'scroll'
    | 'pen' | 'highlighter' | 'arrow' | 'line' | 'rect' | 'ellipse' | 'text' | 'mosaic' | 'undo';
}) {
  const paths: Record<typeof name, string> = {
    copy: 'M5.5 5.5h7a1 1 0 0 1 1 1v7a1 1 0 0 1-1 1h-7a1 1 0 0 1-1-1v-7a1 1 0 0 1 1-1Zm-3 6v-8a1 1 0 0 1 1-1h8',
    save: 'M8 2.5v8.5m0 0 3-3m-3 3-3-3M2.5 11.5v1.5a1 1 0 0 0 1 1h9a1 1 0 0 0 1-1v-1.5',
    ocr: 'M5.5 2.5h-2a1 1 0 0 0-1 1v2m10-3h-2m3 0v3m0 5v2a1 1 0 0 1-1 1h-2m-7 0h-2a1 1 0 0 1-1-1v-2m0-3v-3m3 3h6m-6 2.5h6m-6 2.5h3.5',
    translate: 'M3 4.5h6M6 3v1.5m1.5 0c-.5 2.5-2.3 4.6-4.5 5.8m1.2-3.6c.8 1.6 2.3 3 4.3 3.6M9 13l2.4-6h.6L14.4 13m-4.6-2h4.4',
    redo: 'M13 8a5 5 0 1 1-1.5-3.5M13 2.5V5h-2.5',
    close: 'M4 4l8 8M12 4l-8 8',
    scroll: 'M2.5 3.5h11M2.5 6.5h8M2.5 9.5h5M10.5 5.5v7m0 0 2-2m-2 2-2-2',
    pen: 'M3 13l.9-3.2 7-7a1.3 1.3 0 0 1 1.8 0l.5.5a1.3 1.3 0 0 1 0 1.8l-7 7L3 13Z',
    highlighter: 'M8.8 2.8l4.4 4.4-5.6 5.6-4.1.9.9-4.1 4.4-6.8ZM3 13.5h10',
    arrow: 'M3 13 13 3M13 3H7.5M13 3v5.5',
    line: 'M3 13 13 3',
    rect: 'M3.5 4.5h9a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1h-9a1 1 0 0 1-1-1v-5a1 1 0 0 1 1-1Z',
    ellipse: 'M8 4.5c3 0 5.5 1.6 5.5 3.5S11 11.5 8 11.5 2.5 9.9 2.5 8 5 4.5 8 4.5Z',
    text: 'M3 3.5h10M8 3.5v9M5.5 12.5h5',
    mosaic: 'M3 3h4.5v4.5H3zM8.5 3H13v4.5H8.5zM3 8.5h4.5V13H3zM8.5 8.5H13V13H8.5z',
    undo: 'M3 8a5 5 0 1 0 1.5-3.5M3 2.5V5h2.5',
  };
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d={paths[name]} />
    </svg>
  );
}

/**
 * Selection overlay for one display. The frozen screenshot is a native view
 * behind this transparent page; the magnifier is a native view in front of it
 * (Rust tracks the cursor), so this page only draws selection UI and — on top
 * of the frozen pixels — the annotation layer (see ./annotate.ts).
 *
 * Selection geometry lives in refs and is painted straight to the DOM: it
 * changes on every pointer move, which is far too hot for React state.
 */
export function Overlay({ monitorId }: { monitorId: number }) {
  const [payload, setPayload] = useState<StartPayload | null>(null);
  const [busy, setBusy] = useState(false);
  const [busyLabel, setBusyLabel] = useState('正在识别…');
  const [error, setError] = useState<string | null>(null);
  const [interacting, setInteracting] = useState(false);
  const [annoTool, setAnnoTool] = useState<AnnoTool | null>(null);
  const [annoColor, setAnnoColor] = useState(COLOR_PRESETS[0]);
  const [annoWidth, setAnnoWidth] = useState(1);
  const [annoCounts, setAnnoCounts] = useState({ undo: 0, redo: 0 });
  const [textEdit, setTextEdit] = useState<{ x: number; y: number; value: string } | null>(null);

  const boxRef = useRef<HTMLDivElement>(null);
  const hoverRef = useRef<HTMLDivElement>(null);
  const sizeRef = useRef<HTMLDivElement>(null);
  const toolbarRef = useRef<HTMLDivElement>(null);
  const committedRef = useRef<HTMLCanvasElement>(null);
  const liveRef = useRef<HTMLCanvasElement>(null);
  const textRef = useRef<HTMLTextAreaElement>(null);
  const brushRingRef = useRef<HTMLDivElement>(null);

  const payloadRef = useRef<StartPayload | null>(null);
  const selRef = useRef<Rect | null>(null);
  const dragRef = useRef<Drag | null>(null);
  const phaseRef = useRef<Phase>('select');
  const busyRef = useRef(false);
  const annoRef = useRef<Annotator | null>(null);
  const annoToolRef = useRef<AnnoTool | null>(null);
  const annoColorRef = useRef(COLOR_PRESETS[0]);
  const annoWidthRef = useRef(1);
  const drawingRef = useRef(false);
  const textEditRef = useRef<{ x: number; y: number } | null>(null);

  const setBusyBoth = (next: boolean) => {
    busyRef.current = next;
    setBusy(next);
  };

  const updateCounts = () => {
    const a = annoRef.current;
    setAnnoCounts(a ? { undo: a.opCount, redo: a.redoCount } : { undo: 0, redo: 0 });
  };

  /**
   * Tell Rust which phase we're in (the native magnifier only runs in select)
   * and repaint: the toolbar's visibility depends on the phase, so it must
   * show up the moment the phase changes, not at the next pointer move.
   */
  const setPhase = (next: Phase) => {
    if (phaseRef.current === next) return;
    phaseRef.current = next;
    paint(selRef.current);
    void call('set_overlay_phase', { phase: next }).catch(() => {});
  };

  /** Re-fit the annotation canvases to a finalized selection (ops re-render
      clipped; during a resize drag they merely stretch). */
  const fitAnnotation = (sel: Rect | null) => {
    if (sel && annoRef.current) annoRef.current.resize(sel.w, sel.h);
  };

  const hideHover = () => {
    if (hoverRef.current) hoverRef.current.style.display = 'none';
  };

  /** Push the current selection (or its absence) to the DOM. */
  const paint = (r: Rect | null) => {
    const box = boxRef.current;
    if (!box) return;
    if (!r || r.w < 0.5 || r.h < 0.5) {
      box.style.display = 'none';
      if (sizeRef.current) sizeRef.current.style.display = 'none';
      if (toolbarRef.current) toolbarRef.current.style.display = 'none';
      return;
    }
    const p = payloadRef.current;
    box.style.display = 'block';
    box.style.transform = `translate(${r.x}px, ${r.y}px)`;
    box.style.width = `${r.w}px`;
    box.style.height = `${r.h}px`;

    const size = sizeRef.current;
    if (size) {
      const scale = p?.scale ?? 1;
      const px = scale !== 1 ? ` (${Math.round(r.w * scale)} × ${Math.round(r.h * scale)})` : '';
      const below = r.y + r.h + 24 < (p?.height ?? Infinity);
      size.style.display = 'block';
      size.textContent = `${Math.round(r.w)} × ${Math.round(r.h)}${px}`;
      // Sit just outside the bottom-right corner, tucking inside when the
      // selection reaches the right edge of the screen.
      const outside = r.x + r.w + 6 + size.offsetWidth < (p?.width ?? Infinity);
      const left = outside ? r.x + r.w + 6 : r.x + r.w - size.offsetWidth - 4;
      size.style.transform = `translate(${left}px, ${below ? r.y + r.h + 4 : r.y - 22}px)`;
    }

    const toolbar = toolbarRef.current;
    if (toolbar && phaseRef.current === 'toolbar' && p) {
      toolbar.style.display = 'flex';
      const tw = toolbar.offsetWidth;
      const th = toolbar.offsetHeight;
      // Hang off the bottom-right corner: a drag ends there, so the actions
      // land right under the released cursor. Flip above when there is no
      // room below, clamped to the screen.
      let left = clamp(r.x + r.w - tw, 4, Math.max(4, p.width - tw - 4));
      let top = r.y + r.h + 6;
      if (top + th > p.height - 4) top = r.y - th - 6;
      if (top < 4) top = r.y + r.h - th - 6;
      toolbar.style.transform = `translate(${left}px, ${top}px)`;
    }
  };

  const setSelection = (r: Rect | null) => {
    selRef.current = r;
    paint(r);
  };

  const reset = () => {
    payloadRef.current = null;
    selRef.current = null;
    dragRef.current = null;
    annoRef.current = null;
    annoToolRef.current = null;
    drawingRef.current = false;
    textEditRef.current = null;
    setPayload(null);
    phaseRef.current = 'select';
    setBusyBoth(false);
    setError(null);
    setInteracting(false);
    setAnnoTool(null);
    setTextEdit(null);
    setAnnoCounts({ undo: 0, redo: 0 });
    paint(null);
    hideHover();
  };

  const clearSelection = () => {
    exitAnnotate();
    annoRef.current?.clear();
    updateCounts();
    setSelection(null);
    setPhase('select');
    setInteracting(false);
  };

  // ------------------------------------------------------------- annotations

  const exitAnnotate = () => {
    if (annoToolRef.current === null) return;
    annoToolRef.current = null;
    drawingRef.current = false;
    textEditRef.current = null;
    setAnnoTool(null);
    setTextEdit(null);
    annoRef.current?.commit(); // finish a stroke that was still down
  };

  /** Open the annotation palette; drawing then happens inside the selection. */
  const enterAnnotate = () => {
    const sel = selRef.current;
    const p = payloadRef.current;
    if (!sel || !p || busyRef.current) return;
    // The annotation canvas is this display's crop; a selection running onto
    // another screen would need offsets this layer doesn't model yet.
    if (sel.x < 0 || sel.y < 0 || sel.x + sel.w > p.width + 0.5 || sel.y + sel.h > p.height + 0.5) {
      setError('标注暂不支持跨显示器选区，请在单个屏幕内框选');
      return;
    }
    if (!annoRef.current && committedRef.current && liveRef.current) {
      annoRef.current = new Annotator(committedRef.current, liveRef.current);
    }
    const anno = annoRef.current;
    if (!anno) return;
    anno.setScale(p.scale);
    anno.resize(sel.w, sel.h);
    // 画框是最常用的标注，默认选矩形而不是画笔。
    annoToolRef.current = 'rect';
    setAnnoTool('rect');
  };

  const pickTool = async (tool: AnnoTool) => {
    const anno = annoRef.current;
    const sel = selRef.current;
    if (!anno || !sel) return;
    if (tool === 'mosaic' && !anno.hasBase) {
      try {
        const url = await call<string>('region_pixels', { rect: { x: sel.x, y: sel.y, w: sel.w, h: sel.h } });
        await anno.loadBase(url, sel.w, sel.h);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
        return;
      }
    }
    annoToolRef.current = tool;
    setAnnoTool(tool);
  };

  const beginAnnoOp = (x: number, y: number) => {
    const sel = selRef.current;
    const anno = annoRef.current;
    const tool = annoToolRef.current;
    if (!sel || !anno || !tool) return;
    const lx = x - sel.x;
    const ly = y - sel.y;
    if (tool === 'text') {
      textEditRef.current = { x: lx, y: ly };
      setTextEdit({ x: lx, y: ly, value: '' });
      requestAnimationFrame(() => textRef.current?.focus());
      return;
    }
    drawingRef.current = true;
    anno.begin(tool, annoColorRef.current, WIDTH_PRESETS[annoWidthRef.current], lx, ly);
  };

  const commitTextEdit = () => {
    const edit = textEditRef.current;
    if (!edit || !textEdit) return;
    textEditRef.current = null;
    setTextEdit(null);
    annoRef.current?.addText(edit.x, edit.y, textEdit.value, annoColorRef.current, TEXT_SIZES[annoWidthRef.current]);
    updateCounts();
  };

  const cancelTextEdit = () => {
    textEditRef.current = null;
    setTextEdit(null);
  };

  /** The annotation layer as bare base64 PNG, sized exactly like the crop. */
  const exportAnnotation = (): string | null => {
    const anno = annoRef.current;
    const sel = selRef.current;
    const p = payloadRef.current;
    if (!anno || anno.isEmpty || !sel || !p) return null;
    // Replicate the crop's pixel math (Rust `Frame::to_pixels`) so the layer
    // matches the screenshot byte for byte. Strokes are selection-local, so
    // only the size is passed — the crop origin must NOT shift them.
    const { x0, y0, x1, y1 } = cropPixelBounds(sel, p.scale, p.width, p.height);
    return anno.exportBase64(x1 - x0, y1 - y0);
  };

  // ------------------------------------------------------------------ actions

  const runAction = (action: Action) => {
    const sel = selRef.current;
    if (!sel || busyRef.current) return;
    const args: Record<string, unknown> = { rect: { x: sel.x, y: sel.y, w: sel.w, h: sel.h } };
    const annotation = exportAnnotation();
    if (annotation) args.annotation = annotation;
    // Copy and save keep the overlay up until Rust has read the pixels and
    // closed the session itself; an early hide would break the crop.
    const fail = (e: unknown) => {
      setBusyBoth(false);
      setPhase('select');
      setError(e instanceof Error ? e.message : String(e));
    };
    if (action === 'copy' || action === 'save') {
      setBusyBoth(true);
      setBusyLabel(action === 'copy' ? '正在复制…' : '正在保存…');
      setPhase('busy');
      call(action === 'copy' ? 'copy_region' : 'save_region', args).catch(fail);
      return;
    }
    setBusyBoth(true);
    setBusyLabel('正在识别…');
    setPhase('busy');
    void call('log_message', { message: `ui: invoking ocr_region rect=${JSON.stringify(args.rect)} action=${action}` }).catch(() => {});
    call('ocr_region', { ...args, action })
      .then(() => call('log_message', { message: 'ui: ocr_region resolved' }).catch(() => {}))
      .catch((e) => {
        void call('log_message', { message: `ui: ocr_region failed: ${e instanceof Error ? e.message : String(e)}` }).catch(() => {});
        fail(e);
      });
  };

  /** Hand the selection over to the scroll-capture session (Rust takes over). */
  const startScroll = () => {
    const sel = selRef.current;
    const p = payloadRef.current;
    if (!sel || !p || busyRef.current) return;
    if (sel.x < 0 || sel.y < 0 || sel.x + sel.w > p.width + 0.5 || sel.y + sel.h > p.height + 0.5) {
      setError('长截图暂不支持跨显示器选区，请在单个屏幕内框选');
      return;
    }
    setBusyBoth(true);
    setBusyLabel('正在进入长截图…');
    setPhase('busy');
    // On success Rust ends the screenshot session itself, so `capture-end`
    // resets this page; only failures keep the overlay up.
    call('scroll_capture_start', { rect: { x: sel.x, y: sel.y, w: sel.w, h: sel.h } }).catch((e) => {
      setBusyBoth(false);
      setPhase('toolbar');
      setError(e instanceof Error ? e.message : String(e));
    });
  };

  // ------------------------------------------------------------------ pointer

  const localPoint = (e: React.PointerEvent | React.MouseEvent) => {
    const rect = e.currentTarget.getBoundingClientRect();
    return { x: e.clientX - rect.left, y: e.clientY - rect.top };
  };

  const applyHover = (x: number, y: number, p: StartPayload) => {
    const hover = hoverRef.current;
    if (!hover) return;
    const w = windowAt(x, y, p);
    if (!w || w.w < 8 || w.h < 8 || (selRef.current && inside(selRef.current, x, y))) {
      hover.style.display = 'none';
      return;
    }
    hover.style.display = 'block';
    hover.style.transform = `translate(${w.x}px, ${w.y}px)`;
    hover.style.width = `${w.w}px`;
    hover.style.height = `${w.h}px`;
  };

  const onPointerDown = (e: React.PointerEvent<HTMLDivElement>) => {
    // Right click steps back without needing the keyboard: leave annotation
    // mode first, then drop the selection, then cancel outright.
    if (e.button === 2) {
      if (busyRef.current) return;
      if (annoToolRef.current) exitAnnotate();
      else if (selRef.current) clearSelection();
      else void call('cancel_capture', { reason: 'right-click' }).catch(() => {});
      return;
    }
    if (e.button !== 0) return;
    const p = payloadRef.current;
    if (!p || busyRef.current) return;
    const { x, y } = localPoint(e);
    const sel = selRef.current;
    const dir = (e.target as HTMLElement).dataset?.dir as Dir | undefined;
    // While a drawing tool is active, clicks inside the selection draw.
    // Handles stay resizable; clicks outside do nothing (too easy to lose
    // annotations to a stray click otherwise).
    if (annoToolRef.current && sel && phaseRef.current === 'toolbar') {
      if (!dir && inside(sel, x, y)) {
        if (!textEditRef.current) beginAnnoOp(x, y);
        e.currentTarget.setPointerCapture(e.pointerId);
        return;
      }
      if (!dir) return;
    }
    let drag: Drag;
    if (sel && phaseRef.current === 'toolbar' && dir) {
      drag = { kind: dir, startX: x, startY: y, origin: sel, moved: false };
    } else if (sel && phaseRef.current === 'toolbar' && inside(sel, x, y)) {
      drag = { kind: 'move', startX: x, startY: y, origin: sel, moved: false };
    } else {
      setPhase('select');
      setSelection(null);
      hideHover();
      drag = { kind: 'new', startX: x, startY: y, origin: { x, y, w: 0, h: 0 }, moved: false };
    }
    dragRef.current = drag;
    setInteracting(true);
    e.currentTarget.setPointerCapture(e.pointerId);
    // Other screens sync with this drag: cleared on a fresh drag, or told the
    // current box while a handle/move drag resizes an existing selection.
    if (drag.kind === 'new') {
      void call('selection_start', { label: payloadRef.current?.label ?? '', x, y }).catch(() => {});
    } else if (sel) {
      void call('selection_move', { label: payloadRef.current?.label ?? '', x: sel.x, y: sel.y, w: sel.w, h: sel.h }).catch(() => {});
    }
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    const p = payloadRef.current;
    if (!p) return;
    const { x, y } = localPoint(e);
    // 笔刷圈：手绘工具时跟随光标，显示实际笔画粗细（仅选区内绘制有效）。
    const ring = brushRingRef.current;
    if (ring) {
      const show =
        annoToolRef.current && annoToolRef.current !== 'text' && selRef.current && inside(selRef.current, x, y);
      const size = show ? Math.max(WIDTH_PRESETS[annoWidthRef.current] + 3, 8) : 0;
      ring.style.display = size ? 'block' : 'none';
      if (size) {
        ring.style.width = `${size}px`;
        ring.style.height = `${size}px`;
        ring.style.transform = `translate(${x - size / 2}px, ${y - size / 2}px)`;
      }
    }
    if (drawingRef.current) {
      const sel = selRef.current;
      if (sel) annoRef.current?.extend(x - sel.x, y - sel.y);
      return;
    }
    const drag = dragRef.current;
    if (!drag) {
      if (!busyRef.current && phaseRef.current === 'select') applyHover(x, y, p);
      return;
    }
    hideHover();
    drag.moved = drag.moved || Math.abs(x - drag.startX) + Math.abs(y - drag.startY) > 2;
    setSelection(applyDrag(drag, x, y, p.width, p.height));
  };

  const commit = (p: StartPayload, sel: Rect) => {
    if (sel.w < MIN || sel.h < MIN) {
      setSelection(null);
      setInteracting(false);
      return;
    }
    setSelection(sel);
    if (p.mode !== 'shot') {
      // 提取/翻译：选完直接跑，不留工具条
      runAction(p.mode);
      return;
    }
    setInteracting(false);
    fitAnnotation(sel);
    setPhase('toolbar');
  };

  const onPointerUp = (e: React.PointerEvent<HTMLDivElement>) => {
    const p = payloadRef.current;
    if (drawingRef.current) {
      drawingRef.current = false;
      annoRef.current?.commit();
      updateCounts();
      return;
    }
    const drag = dragRef.current;
    if (!p || !drag) return;
    dragRef.current = null;
    const { x, y } = localPoint(e);
    const sel = applyDrag(drag, x, y, p.width, p.height);
    // Announce the final rectangle to other screens.
    void call('selection_end', { label: p.label, x: sel.x, y: sel.y, w: sel.w, h: sel.h }).catch(() => {});
    if (!drag.moved && drag.kind === 'new') {
      // 单击：贴住窗口轮廓；没有窗口就清空
      const w = windowAt(x, y, p);
      if (w && w.w >= 8 && w.h >= 8) {
        setSelection(w);
        if (p.mode !== 'shot') {
          // 提取/翻译模式下单击窗口：直接对它识别
          runAction(p.mode);
          return;
        }
        setInteracting(false);
        fitAnnotation(w);
        setPhase('toolbar');
        return;
      }
      setSelection(null);
      setInteracting(false);
      return;
    }
    commit(p, sel);
  };

  const onDoubleClick = (e: React.MouseEvent<HTMLDivElement>) => {
    const p = payloadRef.current;
    if (!p || p.mode !== 'shot' || busyRef.current) return;
    if (annoToolRef.current) return;
    const { x, y } = localPoint(e);
    const sel = selRef.current;
    if (sel && inside(sel, x, y)) runAction('copy');
  };

  const keyRef = useRef<(e: KeyboardEvent) => void>(() => {});
  keyRef.current = (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      // First Esc steps out of annotation mode, the second one cancels.
      if (!busyRef.current && annoToolRef.current) {
        exitAnnotate();
        return;
      }
      void call('cancel_capture', { reason: 'esc' }).catch(() => {});
      return;
    }
    if (busyRef.current) return;
    const p = payloadRef.current;
    if (!p) return;
    if ((e.key === 'z' || e.key === 'Z') && (e.metaKey || e.ctrlKey) && annoToolRef.current) {
      e.preventDefault();
      if (e.shiftKey) annoRef.current?.redo();
      else annoRef.current?.undo();
      updateCounts();
      return;
    }
    if (e.key === 'Enter') {
      const sel = selRef.current;
      if (sel && sel.w >= MIN && sel.h >= MIN) runAction(p.mode === 'shot' ? 'copy' : p.mode);
      return;
    }
    if (e.key === 'Delete' || e.key === 'Backspace') {
      if (selRef.current) {
        e.preventDefault();
        clearSelection();
      }
      return;
    }
    if ((e.key === 'a' || e.key === 'A') && (e.metaKey || e.ctrlKey)) {
      e.preventDefault();
      const all = { x: 1, y: 1, w: p.width - 2, h: p.height - 2 };
      setSelection(all);
      fitAnnotation(all);
      setPhase('toolbar');
    }
  };

  useEffect(() => {
    const handler = (e: KeyboardEvent) => keyRef.current(e);
    window.addEventListener('keydown', handler);
    // Heartbeat: proves the page's script engine is alive. Rust watches for a
    // fresh beat after presenting a session; a page that never beats is dead
    // and gets recreated.
    const beat = () => void call('page_heartbeat', { label: `overlay-${monitorId}` }).catch(() => {});
    beat();
    const timer = setInterval(beat, 1500);
    return () => {
      window.removeEventListener('keydown', handler);
      clearInterval(timer);
    };
  }, [monitorId]);

  useEffect(() => {
    let disposed = false;
    const unlistens: UnlistenFn[] = [];
    call<StartPayload | null>('overlay_start_payload')
      .then((p) => {
        if (!disposed && p) {
          payloadRef.current = p;
          setPayload(p);
        }
      })
      .catch(() => {});
    listen<StartPayload>('capture-start', (e) => {
      if (disposed) return;
      reset();
      payloadRef.current = e.payload;
      setPayload(e.payload);
    }).then((un) => unlistens.push(un));
    listen('capture-end', () => {
      if (!disposed) reset();
    }).then((un) => unlistens.push(un));
    // Recognition is taking unusually long: the system Vision pipeline is
    // degraded. Keep the spinner but say so, instead of a silently stuck app.
    listen('ocr-slow', () => {
      if (!disposed && busyRef.current) setBusyLabel('系统识别服务响应缓慢，仍在等待…');
    }).then((un) => unlistens.push(un));
    // Another display's page is dragging: draw this display's share of the box.
    listen<{ rect: Rect | null; active: string }>('selection-update', (e) => {
      if (disposed) return;
      const mine = payloadRef.current;
      if (!mine || e.payload.active === mine.label) return; // my own drag renders locally
      const global = e.payload.rect;
      if (!global) {
        selRef.current = null;
        paint(null);
        return;
      }
      // Intersect the desktop rect with this display, then make it display-local.
      const dx0 = Math.max(global.x, mine.x);
      const dy0 = Math.max(global.y, mine.y);
      const dx1 = Math.min(global.x + global.w, mine.x + mine.width);
      const dy1 = Math.min(global.y + global.h, mine.y + mine.height);
      if (dx1 <= dx0 || dy1 <= dy0) {
        selRef.current = null;
        paint(null);
        return;
      }
      const local: Rect = { x: dx0 - mine.x, y: dy0 - mine.y, w: dx1 - dx0, h: dy1 - dy0 };
      selRef.current = local;
      paint(local);
    }).then((un) => unlistens.push(un));
    return () => {
      disposed = true;
      unlistens.forEach((un) => un());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [monitorId]);

  const cursor = annoTool
    ? ANNO_CURSORS[annoTool]
    : payload
      ? 'crosshair'
      : 'default';

  return (
    <div
      className="overlay"
      style={{ cursor }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onPointerCancel={onPointerUp}
      onDoubleClick={onDoubleClick}
      onContextMenu={(e) => e.preventDefault()}
    >
      <div ref={hoverRef} className="hover" style={{ display: 'none' }} />
      <div ref={brushRingRef} className="brush-ring" style={{ display: 'none' }} />
      {/* 标注工具激活时，选区内的光标跟手绘工具走（.sel 的 cursor:move 是给拖框用的） */}
      <div ref={boxRef} className="sel" style={{ display: 'none', cursor: annoTool ? cursor : undefined }}>
        <canvas ref={committedRef} className="anno-canvas" />
        <canvas ref={liveRef} className="anno-canvas" />
        {HANDLE_DIRS.map((dir) => (
          <span key={dir} data-dir={dir} className={`handle handle-${dir}`} />
        ))}
        {textEdit && (
          <textarea
            ref={textRef}
            className="anno-text-input"
            style={{ left: textEdit.x, top: textEdit.y, color: annoColor, fontSize: TEXT_SIZES[annoWidth], lineHeight: 1.3 }}
            value={textEdit.value}
            placeholder="输入文字"
            onChange={(e) => setTextEdit({ ...textEdit, value: e.target.value })}
            onPointerDown={(e) => e.stopPropagation()}
            onBlur={commitTextEdit}
            onKeyDown={(e) => {
              // Typing must never reach the overlay's shortcuts (Backspace
              // would wipe the selection from under the caret).
              e.stopPropagation();
              if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                commitTextEdit();
              } else if (e.key === 'Escape' && !e.nativeEvent.isComposing) {
                e.preventDefault();
                cancelTextEdit();
              }
            }}
          />
        )}
      </div>
      <div ref={sizeRef} className="size" style={{ display: 'none' }} />
      <div
        ref={toolbarRef}
        className={annoTool ? 'toolbar with-anno' : 'toolbar'}
        style={{ display: 'none' }}
        onPointerDown={(e) => e.stopPropagation()}
      >
        {annoTool && (
          <div className="anno-palette">
            {ANNO_TOOLS.map((t) => (
              <button key={t} className={`tool${annoTool === t ? ' active' : ''}`} title={ANNO_TITLES[t]} onClick={() => void pickTool(t)}>
                <Icon name={t} />
              </button>
            ))}
            <span className="tool-sep" />
            {COLOR_PRESETS.map((c) => (
              <button
                key={c}
                className={`swatch${c === annoColor ? ' active' : ''}`}
                style={{ background: c }}
                title={`颜色 ${c}`}
                onClick={() => {
                  annoColorRef.current = c;
                  setAnnoColor(c);
                }}
              />
            ))}
            <span className="tool-sep" />
            {WIDTH_PRESETS.map((w, i) => (
              <button
                key={w}
                className={`width-dot${i === annoWidth ? ' active' : ''}`}
                title={['细', '中', '粗'][i]}
                onClick={() => {
                  annoWidthRef.current = i;
                  setAnnoWidth(i);
                }}
              >
                <span style={{ width: 4 + w * 2, height: 1 + w }} />
              </button>
            ))}
            <span className="tool-sep" />
            <button className="tool" title="撤销 (⌘Z)" disabled={annoCounts.undo === 0} onClick={() => { annoRef.current?.undo(); updateCounts(); }}>
              <Icon name="undo" />
            </button>
            <button className="tool" title="重做 (⇧⌘Z)" disabled={annoCounts.redo === 0} onClick={() => { annoRef.current?.redo(); updateCounts(); }}>
              <Icon name="redo" />
            </button>
            <button className="tool" title="退出标注 (Esc)" onClick={exitAnnotate}>
              <Icon name="close" />
            </button>
          </div>
        )}
        <div className="toolbar-row">
          <button className={`tool${annoTool ? ' active' : ''}`} title="标注" onClick={() => (annoTool ? exitAnnotate() : enterAnnotate())}>
            <Icon name="pen" />
          </button>
          <button className="tool" title="长截图（手动滚动）" onClick={startScroll}>
            <Icon name="scroll" />
          </button>
          <span className="tool-sep" />
          <button className="tool" title="保存为 PNG" onClick={() => runAction('save')}><Icon name="save" /></button>
          <button className="tool" title="提取文字" onClick={() => runAction('ocr')}><Icon name="ocr" /></button>
          <button className="tool" title="翻译" onClick={() => runAction('translate')}><Icon name="translate" /></button>
          <span className="tool-sep" />
          <button className="tool" title="重新选区 (Delete)" onClick={clearSelection}><Icon name="redo" /></button>
          <button className="tool" title="取消 (Esc)" onClick={() => void call('cancel_capture', { reason: 'close-button' }).catch(() => {})}><Icon name="close" /></button>
          {/* 最常用，放在最右：拖拽结束时光标就在这里 */}
          <button className="tool" title="复制到剪贴板 (Enter)" onClick={() => runAction('copy')}><Icon name="copy" /></button>
        </div>
      </div>
      {busy && (
        <div className="notice">
          <span className="spinner" />
          {busyLabel}
        </div>
      )}
      {error && <div className="notice notice-error">{error}</div>}
      {payload && payload.mode !== 'shot' && !busy && !error && !interacting && (
        <div className="hint">{payload.mode === 'ocr' ? '框选要提取文字的区域，Esc 取消' : '框选要翻译的区域，Esc 取消'}</div>
      )}
    </div>
  );
}
