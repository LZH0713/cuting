import { useEffect, useRef, useState } from 'react';
import { call, type AppInfo, type ProxyConfig, type Settings, type TestReport, type TranslatorConfig, type UpdateInfo } from '../lib/ipc';

/** Keys that only modify the next one: wait for it instead of recording them. */
const MODIFIER_KEYS = new Set(['Shift', 'Control', 'Alt', 'Meta', 'CapsLock', 'Fn', 'FnLock', 'OS', 'AltGraph']);

/**
 * A click-to-record hotkey field.
 *
 * Key presses are listened for on the window, not on the button: WebKit on
 * macOS (like Safari) does not give a button focus when it is clicked, so a
 * listener on the button never hears the keys it is waiting for.
 */
function HotkeyRecorder({ label, value, onChange }: { label: string; value: string; onChange: (combo: string) => void }) {
  const [recording, setRecording] = useState(false);
  // The latest `onChange`, for an effect that outlives the render that created it.
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;

  useEffect(() => {
    if (!recording) return;
    const onKeyDown = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === 'Escape') {
        setRecording(false);
        return;
      }
      if (e.key === 'Backspace' || e.key === 'Delete') {
        onChangeRef.current('');
        setRecording(false);
        return;
      }
      if (MODIFIER_KEYS.has(e.key) || !e.code) return;
      const mods: string[] = [];
      if (e.ctrlKey) mods.push('Ctrl');
      if (e.altKey) mods.push('Alt');
      if (e.shiftKey) mods.push('Shift');
      if (e.metaKey) mods.push('Cmd');
      onChangeRef.current([...mods, e.code].join('+'));
      setRecording(false);
    };
    // Clicking anywhere else gives up on recording.
    const onPointerDown = (e: PointerEvent) => {
      if (!(e.target as HTMLElement | null)?.closest('.hotkey')) setRecording(false);
    };
    window.addEventListener('keydown', onKeyDown, true);
    window.addEventListener('pointerdown', onPointerDown, true);
    return () => {
      window.removeEventListener('keydown', onKeyDown, true);
      window.removeEventListener('pointerdown', onPointerDown, true);
    };
  }, [recording]);

  return (
    <label className="field">
      <span>{label}</span>
      <button type="button" className={`hotkey ${recording ? 'recording' : ''}`} onClick={() => setRecording((current) => !current)}>
        {recording ? '按下新的快捷键…（Esc 取消，退格清除）' : value || '未设置'}
      </button>
    </label>
  );
}

/** A fresh, empty configuration for a provider. */
const EMPTY_TRANSLATORS: Record<TranslatorConfig['provider'], TranslatorConfig> = {
  google: { provider: 'google' },
  youdao: { provider: 'youdao', appKey: '', appSecret: '' },
  deepL: { provider: 'deepL', authKey: '' },
  baidu: { provider: 'baidu', appId: '', secret: '' },
  openAi: { provider: 'openAi', baseUrl: 'https://open.bigmodel.cn/api/paas/v4', apiKey: '', model: '', prompt: '' },
};

interface ConnectionTest {
  /** The configuration this result belongs to, so stale results are ignored. */
  config: string;
  running: boolean;
  report: TestReport | null;
}

const openLink = (url: string) => (e: React.MouseEvent) => {
  e.preventDefault();
  void call('open_url', { url }).catch(() => {});
};

export function SettingsWindow() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [settings, setSettings] = useState<Settings | null>(null);
  const [status, setStatus] = useState<{ kind: 'ok' | 'err'; text: string } | null>(null);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [permission, setPermission] = useState(false);
  const [ports, setPorts] = useState<number[]>([]);
  const [test, setTest] = useState<ConnectionTest | null>(null);
  const [update, setUpdate] = useState<{ state: 'idle' | 'checking' | 'available' | 'none' | 'installing'; info: UpdateInfo | null }>({ state: 'idle', info: null });

  useEffect(() => {
    call<AppInfo>('get_app_info')
      .then((data) => {
        setInfo(data);
        setSettings(data.settings);
        setPermission(data.screenCaptureAllowed);
      })
      .catch((e) => setStatus({ kind: 'err', text: e instanceof Error ? e.message : String(e) }));
    call<number[]>('detect_local_proxies').then(setPorts).catch(() => {});
    // One quiet check when the window opens; the button re-checks on demand.
    setUpdate({ state: 'checking', info: null });
    call<UpdateInfo | null>('check_update')
      .then((info) => setUpdate({ state: info ? 'available' : 'none', info: info ?? null }))
      .catch(() => setUpdate({ state: 'idle', info: null }));
  }, []);

  if (!info || !settings) {
    return (
      <div className="page">
        <div className="loading">{status ? status.text : '加载中…'}</div>
      </div>
    );
  }

  /** Every edit marks the form as unsaved and drops the last status message. */
  const patch = (part: Partial<Settings>) => {
    setSettings({ ...settings, ...part });
    setDirty(true);
    setStatus(null);
  };

  const save = async () => {
    setSaving(true);
    setStatus(null);
    try {
      await call('save_settings', { settings });
      setDirty(false);
      setStatus({ kind: 'ok', text: '设置已保存，快捷键已生效' });
    } catch (e) {
      setStatus({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    } finally {
      setSaving(false);
    }
  };

  /** Try the proxy as it is in the form right now, saved or not. */
  const runTest = async () => {
    const config = JSON.stringify(settings.proxy);
    setTest({ config, running: true, report: null });
    const empty: TestReport = { networkOk: false, networkMs: null, googleOk: false, googleMs: null, summary: '' };
    try {
      const report = await call<TestReport>('test_network', { proxy: settings.proxy });
      setTest((current) => (current?.config === config ? { config, running: false, report } : current));
    } catch (e) {
      const summary = e instanceof Error ? e.message : String(e);
      setTest((current) => (current?.config === config ? { config, running: false, report: { ...empty, summary } } : current));
    }
  };

  const checkForUpdates = async () => {
    setUpdate({ state: 'checking', info: null });
    try {
      const info = await call<UpdateInfo | null>('check_update');
      setUpdate({ state: info ? 'available' : 'none', info: info ?? null });
    } catch (e) {
      setUpdate({ state: 'idle', info: null });
      setStatus({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    }
  };

  /** Downloads and installs, then the app restarts itself. */
  const installUpdate = async () => {
    if (!update.info) return;
    const info = update.info;
    setUpdate({ state: 'installing', info });
    try {
      await call('install_update');
      // Unreachable in practice: install_update restarts the app.
    } catch (e) {
      setUpdate({ state: 'available', info });
      setStatus({ kind: 'err', text: e instanceof Error ? e.message : String(e) });
    }
  };

  const setProxy = (proxy: ProxyConfig) => {
    patch({ proxy });
    setTest(null);
  };

  const translator = settings.translator;
  const provider = translator.provider;
  const installing = update.state === 'installing';
  const updateVersion = update.info?.version;

  return (
    <div className="page">
      <div className="settings">
        <h2>截图</h2>
        <section className="card">
          <HotkeyRecorder label="截图" value={settings.hotkeys.shot} onChange={(v) => patch({ hotkeys: { ...settings.hotkeys, shot: v } })} />
          <HotkeyRecorder label="提取文字" value={settings.hotkeys.ocr} onChange={(v) => patch({ hotkeys: { ...settings.hotkeys, ocr: v } })} />
          <HotkeyRecorder label="截图翻译" value={settings.hotkeys.translate} onChange={(v) => patch({ hotkeys: { ...settings.hotkeys, translate: v } })} />
          <span className="note">点击快捷键后直接按下新的组合键，至少带一个修饰键（⌘ ⌥ ⌃ ⇧）。退格清除，Esc 取消。改完在底部保存。</span>
          <label className="field">
            <span>保存位置</span>
            <span className="dir">
              <input
                type="text"
                value={settings.saveDir ?? info.defaultSaveDir}
                onChange={(e) => patch({ saveDir: e.target.value })}
                placeholder={info.defaultSaveDir}
              />
              <button
                className="btn"
                onClick={() =>
                  void call<string | null>('pick_save_dir', { current: settings.saveDir ?? info.defaultSaveDir })
                    .then((dir) => dir && patch({ saveDir: dir }))
                    .catch(() => {})
                }
              >
                选择…
              </button>
            </span>
          </label>
        </section>

        <h2>文字识别</h2>
        <section className="card">
          <div className="field">
            <span>识别语言（按优先级）</span>
            <div className="checks">
              {info.ocrLanguages.map((l) => (
                <label key={l.code} className="check">
                  <input
                    type="checkbox"
                    checked={settings.ocrLanguages.includes(l.code)}
                    onChange={(e) =>
                      patch({
                        ocrLanguages: e.target.checked
                          ? [...settings.ocrLanguages, l.code]
                          : settings.ocrLanguages.filter((c) => c !== l.code),
                      })
                    }
                  />
                  {l.name}
                </label>
              ))}
            </div>
            <span className="note">不选任何语言时自动检测。识别中文 + 英文混排建议保持「简体中文 + 英语」。</span>
          </div>
        </section>

        <h2>翻译</h2>
        <section className="card">
          <label className="field">
            <span>目标语言</span>
            <select value={settings.targetLang} onChange={(e) => patch({ targetLang: e.target.value })}>
              {info.langs.map((l) => (
                <option key={l.code} value={l.code}>{l.name}</option>
              ))}
            </select>
          </label>
          <label className="field">
            <span>翻译服务</span>
            <select
              id="translator-provider"
              value={provider}
              onChange={(e) => patch({ translator: EMPTY_TRANSLATORS[e.target.value as TranslatorConfig['provider']] })}
            >
              <option value="google">Google 翻译（免配置，国内常被限流）</option>
              <option value="youdao">有道翻译（官方接口，国内直连）</option>
              <option value="deepL">DeepL（需要 API Key）</option>
              <option value="baidu">百度翻译（需要 APPID）</option>
              <option value="openAi">大模型（OpenAI 兼容接口）</option>
            </select>
          </label>
          {provider === 'youdao' && 'appKey' in translator && (
            <>
              <label className="field">
                <span>应用 ID</span>
                <input
                  type="text"
                  value={translator.appKey}
                  onChange={(e) => patch({ translator: { ...translator, appKey: e.target.value } })}
                  spellCheck={false}
                />
              </label>
              <label className="field">
                <span>应用密钥</span>
                <input type="password" value={translator.appSecret} onChange={(e) => patch({ translator: { ...translator, appSecret: e.target.value } })} />
                <span className="note">
                  在<a href="https://ai.youdao.com" onClick={openLink('https://ai.youdao.com')}>有道智云</a>
                  创建应用并开通「文本翻译」服务后获得，和其它工具里用的是同一组。
                </span>
              </label>
            </>
          )}
          {provider === 'deepL' && 'authKey' in translator && (
            <label className="field">
              <span>Auth Key</span>
              <input
                type="password"
                value={translator.authKey}
                onChange={(e) => patch({ translator: { ...translator, authKey: e.target.value } })}
                placeholder="免费版以 :fx 结尾"
              />
              <span className="note">
                在 <a href="https://www.deepl.com/pro-api" onClick={openLink('https://www.deepl.com/pro-api')}>DeepL API</a> 申请，密钥只保存在本机。
              </span>
            </label>
          )}
          {provider === 'baidu' && 'appId' in translator && (
            <>
              <label className="field">
                <span>APPID</span>
                <input type="text" value={translator.appId} onChange={(e) => patch({ translator: { ...translator, appId: e.target.value } })} />
              </label>
              <label className="field">
                <span>密钥</span>
                <input type="password" value={translator.secret} onChange={(e) => patch({ translator: { ...translator, secret: e.target.value } })} />
                <span className="note">
                  在<a href="https://fanyi-api.baidu.com" onClick={openLink('https://fanyi-api.baidu.com')}>百度翻译开放平台</a>
                  注册开发者后获取，需实名认证。
                </span>
              </label>
            </>
          )}
          {provider === 'openAi' && 'apiKey' in translator && (
            <>
              <label className="field">
                <span>接口地址</span>
                <input
                  type="text"
                  value={translator.baseUrl}
                  onChange={(e) => patch({ translator: { ...translator, baseUrl: e.target.value } })}
                  placeholder="https://open.bigmodel.cn/api/paas/v4"
                />
                <span className="note">任何 OpenAI 兼容的 /chat/completions 接口都可以，例如智谱、DeepSeek、Ollama。</span>
              </label>
              <label className="field">
                <span>API Key</span>
                <input type="password" value={translator.apiKey} onChange={(e) => patch({ translator: { ...translator, apiKey: e.target.value } })} />
              </label>
              <label className="field">
                <span>模型</span>
                <input
                  type="text"
                  value={translator.model}
                  onChange={(e) => patch({ translator: { ...translator, model: e.target.value } })}
                  placeholder="由服务商提供"
                />
              </label>
              <label className="field">
                <span>提示词（可选）</span>
                <textarea
                  className="small"
                  value={translator.prompt}
                  onChange={(e) => patch({ translator: { ...translator, prompt: e.target.value } })}
                  placeholder="留空使用内置翻译提示词。{lang} 会替换为目标语言。"
                />
              </label>
            </>
          )}
        </section>

        <h2>网络代理</h2>
        <section className="card">
          <label className="field">
            <span>代理方式</span>
            <select
              id="proxy-mode"
              value={settings.proxy.mode}
              onChange={(e) => {
                const mode = e.target.value as ProxyConfig['mode'];
                // Keep what was typed when the user only switches away and back.
                const url = settings.proxy.mode === 'manual' ? settings.proxy.url : '';
                setProxy(mode === 'manual' ? { mode, url } : { mode });
              }}
            >
              <option value="system">跟随系统设置</option>
              <option value="direct">不使用代理（直连）</option>
              <option value="manual">自定义代理地址</option>
            </select>
          </label>
          {settings.proxy.mode === 'manual' && (
            <label className="field">
              <span>代理地址</span>
              <span className="dir">
                <input
                  id="proxy-url"
                  type="text"
                  value={settings.proxy.url}
                  onChange={(e) => setProxy({ mode: 'manual', url: e.target.value })}
                  placeholder="http://127.0.0.1:7897"
                  spellCheck={false}
                />
                <button className="btn" onClick={() => void runTest()} disabled={test?.running}>
                  {test?.running ? '测试中…' : '测试连接'}
                </button>
              </span>
            </label>
          )}
          {test && !test.running && test.report && (
            <div className={`test-report ${test.report.networkOk ? (test.report.googleOk ? 'ok' : 'warn') : 'error'}`}>
              {test.report.summary}
            </div>
          )}
          <span className="note">
            只影响翻译服务的网络请求。
            {settings.proxy.mode === 'system'
              ? '开着 TUN 模式的代理工具（如 Clash）会接管所有流量，保持「跟随系统」即可，不用再填它的端口。'
              : ''}
          </span>
          {settings.proxy.mode === 'manual' && ports.length > 0 && (
            <div className="note proxy-ports">
              本机这些端口上有服务在监听，可能是代理，点击填入：
              {ports.map((port) => (
                <button
                  key={port}
                  className="btn btn-small proxy-port"
                  data-port={port}
                  onClick={() => setProxy({ mode: 'manual', url: `http://127.0.0.1:${port}` })}
                >
                  {port}
                </button>
              ))}
            </div>
          )}
        </section>

        <h2>更新</h2>
        <section className="card">
          <div className="field">
            <span>当前版本</span>
            <span>v{info.version}</span>
            {update.state === 'available' && <span className="ok">新版本 v{update.info?.version} 可用</span>}
            {update.state === 'none' && <span className="ok">已是最新版本</span>}
            <span className="grow" />
            <button className="btn" onClick={() => void checkForUpdates()} disabled={update.state === 'checking'}>
              {update.state === 'checking' ? '检查中…' : '检查更新'}
            </button>
            {update.state === 'available' && (
              <button className="btn btn-primary" onClick={() => void installUpdate()} disabled={installing}>
                {installing ? '下载安装中…' : `更新到 v${updateVersion} 并重启`}
              </button>
            )}
          </div>
          {update.state === 'available' && update.info?.notes ? <div className="note pre-line">{update.info.notes}</div> : null}
          <div className="note">
            更新包带签名校验，只接受项目发布的内容；安装后自动重启，屏幕录制等授权不受影响。
          </div>
        </section>

        <h2>权限</h2>
        <section className="card">
          <div className="field">
            <span>屏幕录制</span>
            {permission ? (
              <span className="ok">已授权</span>
            ) : (
              <>
                <span className="warn">未授权，截图无法使用</span>
                <button className="btn" onClick={() => void call('open_screen_settings').catch(() => {})}>打开系统设置</button>
              </>
            )}
            {/* The status is a snapshot from when the window opened. */}
            <button className="btn" onClick={() => void call<boolean>('screen_capture_allowed').then(setPermission).catch(() => {})}>
              重新检查
            </button>
          </div>
          <div className="note">
            日志文件：{info.logFile} · 版本 v{info.version}
          </div>
        </section>

        {/* Stays in view however far down the page has scrolled. */}
        <div className="actions">
          {dirty && <span className="dirty">有未保存的修改</span>}
          {status && <span className={`status ${status.kind === 'ok' ? 'ok' : 'error'}`}>{status.text}</span>}
          <span className="grow" />
          <button className="btn btn-primary" onClick={() => void save()} disabled={saving || !dirty}>
            {saving ? '保存中…' : '保存设置'}
          </button>
        </div>
      </div>
    </div>
  );
}
