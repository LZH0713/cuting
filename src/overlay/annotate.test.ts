import { beforeEach, describe, expect, it } from 'vitest';

import { Annotator, type AnnoTool } from './annotate';

/**
 * Annotator 只依赖 canvas 的 2D 上下文，用记录调用的 Proxy 桩代替真
 * canvas，即可在 node 环境下测试操作序列与导出坐标变换。
 */

type Recorder = {
  canvas: HTMLCanvasElement;
  calls: Map<string, unknown[][]>;
  last(prop: string): unknown[] | undefined;
};

function makeCanvas(): Recorder {
  const calls = new Map<string, unknown[][]>();
  const record = (prop: string, args: unknown[]) => {
    const list = calls.get(prop) ?? [];
    list.push(args);
    calls.set(prop, list);
  };
  const ctx = new Proxy(
    {},
    {
      get: (_t, prop: string) => (...args: unknown[]) => record(prop, args),
      set: (_t, _prop) => true,
    },
  ) as unknown as CanvasRenderingContext2D;
  const canvas = {
    width: 0,
    height: 0,
    style: {} as CSSStyleDeclaration,
    getContext: () => ctx,
    toDataURL: () => 'data:image/png;base64,QUJD',
  } as unknown as HTMLCanvasElement;
  return {
    canvas,
    calls,
    last: (prop: string) => {
      const list = calls.get(prop);
      return list ? list[list.length - 1] : undefined;
    },
  };
}

function setup() {
  const committed = makeCanvas();
  const live = makeCanvas();
  const anno = new Annotator(committed.canvas, live.canvas);
  anno.setScale(2); // 2x Retina 是主场景
  return { committed, live, anno };
}

/** 让 exportBase64 的 document.createElement 落到桩上。 */
function stubDocument() {
  const created: Recorder[] = [];
  (globalThis as Record<string, unknown>).document = {
    createElement: () => {
      const r = makeCanvas();
      created.push(r);
      return r.canvas;
    },
  };
  return created;
}

describe('Annotator — 操作序列', () => {
  const draw = (anno: Annotator, tool: AnnoTool = 'pen') => {
    anno.begin(tool, '#ef4444', 2, 10, 10);
    anno.extend(30, 10);
    anno.extend(30, 40);
    anno.commit();
  };

  it('一笔完成后计入 opCount', () => {
    const { anno } = setup();
    draw(anno);
    expect(anno.opCount).toBe(1);
    expect(anno.isEmpty).toBe(false);
  });

  it('单击形状（位移 < 2px）不记录，画笔单击留下一个点', () => {
    const { anno } = setup();
    anno.begin('rect', '#ef4444', 2, 10, 10);
    anno.extend(11, 10.5);
    anno.commit();
    expect(anno.opCount).toBe(0);

    anno.begin('pen', '#ef4444', 2, 10, 10);
    anno.commit();
    expect(anno.opCount).toBe(1);
  });

  it('undo / redo 维护两个栈', () => {
    const { anno } = setup();
    draw(anno);
    draw(anno);
    expect(anno.opCount).toBe(2);
    anno.undo();
    expect(anno.opCount).toBe(1);
    expect(anno.redoCount).toBe(1);
    anno.redo();
    expect(anno.opCount).toBe(2);
    expect(anno.redoCount).toBe(0);
  });

  it('clear 清空所有状态', () => {
    const { anno } = setup();
    draw(anno);
    anno.undo();
    anno.clear();
    expect(anno.isEmpty).toBe(true);
    expect(anno.redoCount).toBe(0);
    expect(anno.hasBase).toBe(false);
  });

  it('resize 把画布后备存储设为 CSS 尺寸 × scale', () => {
    const { committed, anno } = setup();
    anno.resize(300, 200);
    expect(committed.canvas.width).toBe(600);
    expect(committed.canvas.height).toBe(400);
    expect(committed.canvas.style.width).toBe('300px');
  });
});

describe('Annotator — 导出坐标', () => {
  beforeEach(() => {
    delete (globalThis as Record<string, unknown>).document;
  });

  it('没有操作时不导出', () => {
    const created = stubDocument();
    const { anno } = setup();
    expect(anno.exportBase64(100, 100)).toBeNull();
    expect(created).toHaveLength(0);
  });

  it('导出的变换是纯缩放——这是导出错位 bug 的回归测试', () => {
    // 2026-11-26 的 bug：setTransform 带了 -cropX0/-cropY0 平移，把笔画
    // 整体平移了选区起点×scale。选区局部坐标只允许纯缩放。
    const created = stubDocument();
    const { anno } = setup();
    anno.begin('pen', '#ef4444', 2, 10, 10);
    anno.extend(40, 50);
    anno.commit();

    const data = anno.exportBase64(600, 400);
    expect(data).toBe('QUJD'); // 桩返回的固定 base64
    const exportCanvas = created[0];
    expect(exportCanvas.canvas.width).toBe(600);
    expect(exportCanvas.canvas.height).toBe(400);
    expect(exportCanvas.last('setTransform')).toEqual([2, 0, 0, 2, 0, 0]);
  });

  it('有操作但画布上下文不可用时返回 null 而不是抛错', () => {
    (globalThis as Record<string, unknown>).document = {
      createElement: () => ({
        width: 0,
        height: 0,
        style: {},
        getContext: () => null,
        toDataURL: () => '',
      }),
    };
    const { anno } = setup();
    anno.begin('pen', '#ef4444', 2, 1, 1);
    anno.commit();
    expect(anno.exportBase64(100, 100)).toBeNull();
  });
});
