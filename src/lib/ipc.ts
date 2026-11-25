import { invoke } from '@tauri-apps/api/core';
import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface WinRect {
  id: number;
  layer: number;
  pid: number;
  rect: Rect;
}

export type Mode = 'shot' | 'ocr' | 'translate';

/** Sent to the overlay when a capture starts. Geometry is desktop-local CSS. */
export interface StartPayload {
  sessionId: number;
  mode: Mode;
  /** Physical pixels per CSS pixel. */
  scale: number;
  width: number;
  height: number;
  /** Windows in z-order, front first. */
  windows: WinRect[];
}

export interface LangInfo {
  code: string;
  name: string;
}

export type TranslatorConfig =
  | { provider: 'google' }
  | { provider: 'youdao'; appKey: string; appSecret: string }
  | { provider: 'deepL'; authKey: string }
  | { provider: 'baidu'; appId: string; secret: string }
  | { provider: 'openAi'; baseUrl: string; apiKey: string; model: string; prompt: string };

/** How translation requests reach the network. */
export type ProxyConfig = { mode: 'system' } | { mode: 'direct' } | { mode: 'manual'; url: string };

/** What `test_network` found for a proxy configuration. */
export interface TestReport {
  networkOk: boolean;
  networkMs: number | null;
  googleOk: boolean;
  googleMs: number | null;
  /** A line or two, written for the settings window. */
  summary: string;
}

export interface Settings {
  hotkeys: { shot: string; ocr: string; translate: string };
  ocrLanguages: string[];
  targetLang: string;
  translator: TranslatorConfig;
  saveDir: string | null;
  proxy: ProxyConfig;
}

export interface AppInfo {
  settings: Settings;
  langs: LangInfo[];
  ocrLanguages: LangInfo[];
  screenCaptureAllowed: boolean;
  defaultSaveDir: string;
  logFile: string;
  version: string;
}

export interface OcrLine {
  text: string;
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface ResultPayload {
  mode: 'ocr' | 'translate';
  image: string;
  text: string;
  lines: OcrLine[];
  elapsedMs: number;
}

/** What the release feed offers, if anything. */
export interface UpdateInfo {
  version: string;
  notes: string | null;
  currentVersion: string;
}

export interface Translation {
  text: string;
  source: string | null;
  provider: string;
  elapsedMs: number;
}

export type WindowKind = { kind: 'overlay' } | { kind: 'result' } | { kind: 'settings' };

/** The window label decides what a page renders: `overlay`, `result` or `settings`. */
export function detectWindow(): WindowKind {
  const label = getCurrentWebviewWindow().label;
  if (label === 'overlay') {
    return { kind: 'overlay' };
  }
  return label === 'result' ? { kind: 'result' } : { kind: 'settings' };
}

/** Rust commands reject with a plain string; normalize everything to Error. */
export function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(cmd, args).catch((e: unknown) => {
    throw e instanceof Error ? e : new Error(typeof e === 'string' ? e : JSON.stringify(e));
  });
}
