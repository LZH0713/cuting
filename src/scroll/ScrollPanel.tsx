import { useEffect, useRef, useState } from 'react';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { call } from '../lib/ipc';

/**
 * Live state of the scroll capture, pushed by Rust on every stitch and pulled
 * once on mount (a session may have started before this page loaded).
 */
interface Progress {
  widthPx: number;
  heightPx: number;
  frames: number;
  preview: string;
}

/** Sessions end with a null progress event; anything running is then moot. */
function applyProgress(
  payload: Progress | null,
  setProgress: (p: Progress | null) => void,
  setBusyBoth: (b: boolean) => void,
  setError: (e: string | null) => void,
) {
  setProgress(payload);
  // A new session (or the previous one ending) always lifts a stuck busy
  // flag: the buttons must never stay dead across sessions.
  setBusyBoth(false);
  if (payload === null) setError(null);
}

/**
 * The floating control panel of a long screenshot. Scrolling stays with the
 * app under the cursor (the outline window is mouse-transparent and this
 * panel sits beside the region); this page only reports progress and takes
 * the finishing actions: Enter saves, Esc cancels.
 */
export function ScrollPanel() {
  const [progress, setProgress] = useState<Progress | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const busyRef = useRef(false);

  const setBusyBoth = (next: boolean) => {
    busyRef.current = next;
    setBusy(next);
  };

  useEffect(() => {
    let disposed = false;
    const unlistens: UnlistenFn[] = [];
    call<Progress | null>('scroll_state')
      .then((p) => {
        if (!disposed) applyProgress(p, setProgress, setBusyBoth, setError);
      })
      .catch(() => {});
    listen<Progress | null>('scroll-progress', (e) => {
      if (!disposed) applyProgress(e.payload, setProgress, setBusyBoth, setError);
    }).then((un) => unlistens.push(un));
    return () => {
      disposed = true;
      unlistens.forEach((un) => un());
    };
  }, []);

  const save = () => {
    if (busyRef.current || !progress) return;
    setBusyBoth(true);
    setError(null);
    call('scroll_capture_save').catch((e) => {
      setError(e instanceof Error ? e.message : String(e));
      setBusyBoth(false);
    });
    // 成功时 Rust 在编码完成后才收起面板（编码大图要几秒），busy 由下一个
    // 会话的首个事件或本会话的收尾事件复位。
  };

  const copy = () => {
    if (busyRef.current || !progress) return;
    setBusyBoth(true);
    setError(null);
    call('scroll_capture_copy')
      .catch((e) => setError(e instanceof Error ? e.message : String(e)))
      .finally(() => setBusyBoth(false));
  };

  const cancel = () => {
    if (busyRef.current) return;
    setBusyBoth(true);
    call('scroll_capture_cancel', { reason: 'panel' }).catch(() => setBusyBoth(false));
  };

  const keyRef = useRef<(e: KeyboardEvent) => void>(() => {});
  keyRef.current = (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      copy();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      cancel();
    } else if ((e.metaKey || e.ctrlKey) && (e.key === 's' || e.key === 'S')) {
      e.preventDefault();
      save();
    }
  };

  useEffect(() => {
    const handler = (e: KeyboardEvent) => keyRef.current(e);
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, []);

  return (
    <div className="scroll-panel">
      <div className="thumb">{progress?.preview && <img src={progress.preview} alt="长截图预览" />}</div>
      <div className="info">
        <div className="title">长截图</div>
        {error ? (
          <div className="status scroll-error">{error}</div>
        ) : progress ? (
          <div className="status">
            {busy ? '正在保存…' : (
              <>
                滚动页面内容，完成后点保存
                <br />
                <b>{Math.round(progress.widthPx)} × {Math.round(progress.heightPx)}</b> px · {progress.frames} 帧
              </>
            )}
          </div>
        ) : (
          <div className="status">正在准备…</div>
        )}
      </div>
      <div className="actions">
        {/* 复制是最高频动作：放最前、主色、Enter 直达 */}
        <button className="primary" onClick={copy} disabled={!progress || busy}>
          复制 (Enter)
        </button>
        <button onClick={cancel} disabled={busy}>
          取消 (Esc)
        </button>
        <button onClick={save} disabled={!progress || busy}>
          保存 PNG (⌘S)
        </button>
      </div>
    </div>
  );
}
