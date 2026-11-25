import { useEffect, useRef, useState } from 'react';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { call, type AppInfo, type LangInfo, type ResultPayload, type Translation } from '../lib/ipc';

/**
 * OCR / translation result window. Shown by Rust after a recognition; the
 * payload is pulled once on mount (events can't reach a window being created).
 */
export function ResultWindow() {
  const [langs, setLangs] = useState<LangInfo[]>([]);
  const [target, setTarget] = useState('');
  const [item, setItem] = useState<ResultPayload | null>(null);
  const [text, setText] = useState('');
  const [translation, setTranslation] = useState<Translation | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const seq = useRef(0);

  const translateNow = async (source: string, id: number) => {
    setLoading(true);
    setError('');
    try {
      const result = await call<Translation>('translate_text', { text: source, target: target || null });
      if (id !== seq.current) return;
      setTranslation(result);
    } catch (e) {
      if (id !== seq.current) return;
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (id === seq.current) setLoading(false);
    }
  };

  const take = async () => {
    const payload = await call<ResultPayload | null>('take_result').catch(() => null);
    if (!payload) return;
    const id = ++seq.current;
    setItem(payload);
    setText(payload.text);
    setTranslation(null);
    setError('');
    setLoading(false);
    if (payload.mode === 'translate') void translateNow(payload.text, id);
  };

  useEffect(() => {
    let un: UnlistenFn | undefined;
    call<AppInfo>('get_app_info')
      .then((info) => {
        setLangs(info.langs);
        setTarget(info.settings.targetLang);
      })
      .catch(() => {});
    void take();
    listen('show-result', () => void take()).then((u) => {
      un = u;
    });
    return () => un?.();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const langName = (code: string | null) => {
    if (!code) return '';
    const hit = langs.find((l) => l.code.toLowerCase() === code.toLowerCase());
    return hit ? hit.name : code.toUpperCase();
  };

  return (
    <div className="page">
      <div className="result">
        <section className="preview">
          {item ? <img src={item.image} alt="截图区域" /> : <div className="empty">截图后选择「提取文字」或「翻译」，结果会显示在这里。</div>}
        </section>
        <section className="texts">
          <div className="row">
            <span className="muted">{item ? `${item.lines.length} 行 · ${item.elapsedMs} ms` : ''}</span>
            <span className="grow" />
            <select value={target} onChange={(e) => setTarget(e.target.value)} disabled={!item} title="目标语言">
              {langs.map((l) => (
                <option key={l.code} value={l.code}>{l.name}</option>
              ))}
            </select>
            <button className="btn btn-primary" disabled={!item || loading} onClick={() => item && void translateNow(text, ++seq.current)}>
              {loading ? '翻译中…' : '翻译'}
            </button>
            <button className="btn" disabled={!item || !text} onClick={() => item && void call('copy_text', { text }).catch(() => {})}>
              复制原文
            </button>
          </div>
          <textarea
            className="text"
            value={text}
            onChange={(e) => setText(e.target.value)}
            placeholder="识别到的文字"
            spellCheck={false}
          />
          <div className="row">
            <span className="muted">
              {translation
                ? `译文 · ${translation.provider}${translation.source ? ` · 识别为 ${langName(translation.source)}` : ''} · ${translation.elapsedMs} ms`
                : ''}
            </span>
            <span className="grow" />
            <button className="btn" disabled={!translation} onClick={() => translation && void call('copy_text', { text: translation.text }).catch(() => {})}>
              复制译文
            </button>
          </div>
          {error ? (
            <div className="error">
              <span className="error-text">{error}</span>
              <button className="btn btn-small" onClick={() => void call('open_settings_window').catch(() => {})}>
                打开设置换个翻译服务
              </button>
            </div>
          ) : null}
          <textarea className="text" readOnly value={translation?.text ?? ''} placeholder={loading ? '翻译中…' : '译文'} />
        </section>
      </div>
    </div>
  );
}
