import { useEffect, useRef, useState } from 'react';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { call, type Rect, type StartPayload } from '../lib/ipc';

type Dir = 'nw' | 'n' | 'ne' | 'e' | 'se' | 's' | 'sw' | 'w';
type Action = 'copy' | 'save' | 'ocr' | 'translate';
type Phase = 'select' | 'toolbar' | 'busy';
interface Drag {
  kind: Dir | 'new' | 'move';
  startX: number;
  startY: number;
  origin: Rect;
  moved: boolean;
}

const MIN = 2;
const HANDLE_DIRS: Dir[] = ['nw', 'n', 'ne', 'e', 'se', 's', 'sw', 'w'];

const clamp = (v: number, lo: number, hi: number) => Math.min(Math.max(v, lo), hi);

const inside = (r: Rect, x: number, y: number) => x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;

function applyDrag(d: Drag, x: number, y: number, width: number, height: number): Rect {
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
    left = clamp(o.x + dx, 0, right - MIN);
    w = right - left;
  }
  if (d.kind.includes('e')) {
    w = clamp(right + dx - o.x, MIN, width - o.x);
  }
  if (d.kind.includes('n')) {
    top = clamp(o.y + dy, 0, bottom - MIN);
    h = bottom - top;
  }
  if (d.kind.includes('s')) {
    h = clamp(bottom + dy - o.y, MIN, height - o.y);
  }
  return { x: left, y: top, w, h };
}

function windowAt(x: number, y: number, p: StartPayload): Rect | null {
  for (const w of p.windows) {
    if (w.rect.x <= x && x < w.rect.x + w.rect.w && w.rect.y <= y && y < w.rect.y + w.rect.h) {
      return { ...w.rect };
    }
  }
  return null;
}

function Icon({ name }: { name: 'copy' | 'save' | 'ocr' | 'translate' | 'redo' | 'close' }) {
  const paths: Record<typeof name, string> = {
    copy: 'M5.5 5.5h7a1 1 0 0 1 1 1v7a1 1 0 0 1-1 1h-7a1 1 0 0 1-1-1v-7a1 1 0 0 1 1-1Zm-3 6v-8a1 1 0 0 1 1-1h8',
    save: 'M8 2.5v8.5m0 0 3-3m-3 3-3-3M2.5 11.5v1.5a1 1 0 0 0 1 1h9a1 1 0 0 0 1-1v-1.5',
    ocr: 'M5.5 2.5h-2a1 1 0 0 0-1 1v2m10-3h-2m3 0v3m0 5v2a1 1 0 0 1-1 1h-2m-7 0h-2a1 1 0 0 1-1-1v-2m0-3v-3m3 3h6m-6 2.5h6m-6 2.5h3.5',
    translate: 'M3 4.5h6M6 3v1.5m1.5 0c-.5 2.5-2.3 4.6-4.5 5.8m1.2-3.6c.8 1.6 2.3 3 4.3 3.6M9 13l2.4-6h.6L14.4 13m-4.6-2h4.4',
    redo: 'M13 8a5 5 0 1 1-1.5-3.5M13 2.5V5h-2.5',
    close: 'M4 4l8 8M12 4l-8 8',
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
 * (Rust tracks the cursor), so this page only draws selection UI.
 *
 * Selection geometry lives in refs and is painted straight to the DOM: it
 * changes on every pointer move, which is far too hot for React state.
 */
export function Overlay({ monitorId }: { monitorId: number }) {
  const [payload, setPayload] = useState<StartPayload | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [interacting, setInteracting] = useState(false);

  const boxRef = useRef<HTMLDivElement>(null);
  const hoverRef = useRef<HTMLDivElement>(null);
  const sizeRef = useRef<HTMLDivElement>(null);
  const toolbarRef = useRef<HTMLDivElement>(null);

  const payloadRef = useRef<StartPayload | null>(null);
  const selRef = useRef<Rect | null>(null);
  const dragRef = useRef<Drag | null>(null);
  const phaseRef = useRef<Phase>('select');
  const busyRef = useRef(false);

  const setBusyBoth = (next: boolean) => {
    busyRef.current = next;
    setBusy(next);
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
    setPayload(null);
    phaseRef.current = 'select';
    setBusyBoth(false);
    setError(null);
    setInteracting(false);
    paint(null);
    hideHover();
  };

  const clearSelection = () => {
    setSelection(null);
    setPhase('select');
    setInteracting(false);
  };

  const runAction = (action: Action) => {
    const sel = selRef.current;
    if (!sel || busyRef.current) return;
    const args = { rect: { x: sel.x, y: sel.y, w: sel.w, h: sel.h } };
    // Copy and save keep the overlay up until Rust has read the pixels and
    // closed the session itself; an early hide would break the crop.
    const fail = (e: unknown) => {
      setBusyBoth(false);
      setPhase('select');
      setError(e instanceof Error ? e.message : String(e));
    };
    if (action === 'copy' || action === 'save') {
      setBusyBoth(true);
      setPhase('busy');
      call(action === 'copy' ? 'copy_region' : 'save_region', args).catch(fail);
      return;
    }
    setBusyBoth(true);
    setPhase('busy');
    void call('log_message', { message: `ui: invoking ocr_region rect=${JSON.stringify(args.rect)} action=${action}` }).catch(() => {});
    call('ocr_region', { ...args, action })
      .then(() => call('log_message', { message: 'ui: ocr_region resolved' }).catch(() => {}))
      .catch((e) => {
        void call('log_message', { message: `ui: ocr_region failed: ${e instanceof Error ? e.message : String(e)}` }).catch(() => {});
        fail(e);
      });
  };

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
    // Right click steps back without needing the keyboard: drop the selection,
    // or cancel outright when there is none.
    if (e.button === 2) {
      if (busyRef.current) return;
      if (selRef.current) clearSelection();
      else void call('cancel_capture', { reason: 'right-click' }).catch(() => {});
      return;
    }
    if (e.button !== 0) return;
    const p = payloadRef.current;
    if (!p || busyRef.current) return;
    const { x, y } = localPoint(e);
    const sel = selRef.current;
    const dir = (e.target as HTMLElement).dataset?.dir as Dir | undefined;
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
  };

  const onPointerMove = (e: React.PointerEvent<HTMLDivElement>) => {
    const p = payloadRef.current;
    if (!p) return;
    const { x, y } = localPoint(e);
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
    setPhase('toolbar');
  };

  const onPointerUp = (e: React.PointerEvent<HTMLDivElement>) => {
    const p = payloadRef.current;
    const drag = dragRef.current;
    if (!p || !drag) return;
    dragRef.current = null;
    const { x, y } = localPoint(e);
    const sel = applyDrag(drag, x, y, p.width, p.height);
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
    const { x, y } = localPoint(e);
    const sel = selRef.current;
    if (sel && inside(sel, x, y)) runAction('copy');
  };

  const keyRef = useRef<(e: KeyboardEvent) => void>(() => {});
  keyRef.current = (e) => {
    if (e.key === 'Escape') {
      e.preventDefault();
      void call('cancel_capture', { reason: 'esc' }).catch(() => {});
      return;
    }
    if (busyRef.current) return;
    const p = payloadRef.current;
    if (!p) return;
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
      setSelection({ x: 1, y: 1, w: p.width - 2, h: p.height - 2 });
      setPhase('toolbar');
    }
  };

  useEffect(() => {
    const handler = (e: KeyboardEvent) => keyRef.current(e);
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, []);

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
    return () => {
      disposed = true;
      unlistens.forEach((un) => un());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [monitorId]);

  return (
    <div
      className="overlay"
      style={{ cursor: payload ? 'crosshair' : 'default' }}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={onPointerUp}
      onDoubleClick={onDoubleClick}
      onContextMenu={(e) => e.preventDefault()}
    >
      <div ref={hoverRef} className="hover" style={{ display: 'none' }} />
      <div ref={boxRef} className="sel" style={{ display: 'none' }}>
        {HANDLE_DIRS.map((dir) => (
          <span key={dir} data-dir={dir} className={`handle handle-${dir}`} />
        ))}
      </div>
      <div ref={sizeRef} className="size" style={{ display: 'none' }} />
      <div ref={toolbarRef} className="toolbar" style={{ display: 'none' }} onPointerDown={(e) => e.stopPropagation()}>
        <button className="tool" title="保存为 PNG" onClick={() => runAction('save')}><Icon name="save" /></button>
        <button className="tool" title="提取文字" onClick={() => runAction('ocr')}><Icon name="ocr" /></button>
        <button className="tool" title="翻译" onClick={() => runAction('translate')}><Icon name="translate" /></button>
        <span className="tool-sep" />
        <button className="tool" title="重新选区 (Delete)" onClick={clearSelection}><Icon name="redo" /></button>
        <button className="tool" title="取消 (Esc)" onClick={() => void call('cancel_capture', { reason: 'close-button' }).catch(() => {})}><Icon name="close" /></button>
        {/* 最常用，放在最右：拖拽结束时光标就在这里 */}
        <button className="tool" title="复制到剪贴板 (Enter)" onClick={() => runAction('copy')}><Icon name="copy" /></button>
      </div>
      {busy && (
        <div className="notice">
          <span className="spinner" />
          正在识别…
        </div>
      )}
      {error && <div className="notice notice-error">{error}</div>}
      {payload && payload.mode !== 'shot' && !busy && !error && !interacting && (
        <div className="hint">{payload.mode === 'ocr' ? '框选要提取文字的区域，Esc 取消' : '框选要翻译的区域，Esc 取消'}</div>
      )}
    </div>
  );
}
