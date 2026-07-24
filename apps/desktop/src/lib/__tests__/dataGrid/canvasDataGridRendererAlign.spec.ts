// @vitest-environment happy-dom

import { beforeEach, describe, expect, it, vi } from "vitest";
import { drawCanvasDataGrid, type DrawCanvasDataGridOptions } from "@/lib/dataGrid/canvasDataGridRenderer";

interface FillTextCall {
  text: string;
  x: number;
  align: string;
}

/** Minimal fake 2D context that records fillText calls and no-ops the rest. */
function createFakeCtx(): { ctx: Partial<CanvasRenderingContext2D>; calls: FillTextCall[] } {
  const calls: FillTextCall[] = [];
  const state: { textAlign: string } = { textAlign: "left" };
  const ctx: Partial<CanvasRenderingContext2D> = {
    set textAlign(v: string) {
      state.textAlign = v;
    },
    get textAlign(): string {
      return state.textAlign;
    },
    font: "",
    textBaseline: "middle",
    fillStyle: "",
    strokeStyle: "",
    globalAlpha: 1,
    lineWidth: 1,
    imageSmoothingEnabled: false,
    setTransform: () => {},
    clearRect: () => {},
    fillRect: () => {},
    strokeRect: () => {},
    beginPath: () => {},
    moveTo: () => {},
    lineTo: () => {},
    stroke: () => {},
    fill: () => {},
    rect: () => {},
    clip: () => {},
    save: () => {},
    restore: () => {},
    fillText: (text: string, x: number) => {
      calls.push({ text: String(text), x, align: state.textAlign });
    },
    measureText: (text: string) => ({ width: String(text).length * 7 }) as TextMetrics,
    // fontVariantNumeric is set via a cast in the renderer; keep it writable.
  } as Partial<CanvasRenderingContext2D>;
  return { ctx, calls };
}

function createMockCanvas(fakeCtx: Partial<CanvasRenderingContext2D>) {
  const canvas = document.createElement("canvas");
  canvas.width = 800;
  canvas.height = 400;
  canvas.style.width = "800px";
  canvas.style.height = "400px";
  canvas.style.fontFamily = "sans-serif";
  canvas.style.fontSize = "13px";
  canvas.style.fontWeight = "400";
  canvas.style.lineHeight = "normal";
  vi.spyOn(canvas, "getContext").mockReturnValue(fakeCtx as CanvasRenderingContext2D);
  return canvas;
}

function createMockScroller(scrollLeft = 0, scrollTop = 0, clientWidth = 800, clientHeight = 400) {
  const scroller = document.createElement("div");
  Object.defineProperty(scroller, "scrollLeft", { value: scrollLeft, writable: true, configurable: true });
  Object.defineProperty(scroller, "scrollTop", { value: scrollTop, writable: true, configurable: true });
  Object.defineProperty(scroller, "clientWidth", { value: clientWidth, writable: true, configurable: true });
  Object.defineProperty(scroller, "clientHeight", { value: clientHeight, writable: true, configurable: true });
  return scroller;
}

function createMockRow(id: number, displayIndex: number, data: (string | null)[]) {
  return {
    id,
    displayIndex,
    data,
    isNew: false,
    isDraft: false,
    isDeleted: false,
    isDirtyCol: data.map(() => false),
    status: "clean" as const,
  };
}

function createBaseOptions(canvas: HTMLCanvasElement, overrides: Partial<DrawCanvasDataGridOptions> = {}): DrawCanvasDataGridOptions {
  return {
    canvas,
    scroller: createMockScroller(),
    width: 800,
    height: 400,
    pixelRatio: 1,
    isDark: false,
    rowCount: 1,
    rowAt: (index: number) => (index < 1 ? createMockRow(index, index, ["left", "right"]) : undefined),
    renderedColumnWidths: [200, 200],
    visibleColumnIndexes: [0, 1],
    rowNumberWidth: 40,
    hoverCell: null,
    isScrolling: false,
    editingCell: null,
    searchMatchKeys: new Set(),
    currentSearchMatch: null,
    formatCell: (value) => String(value ?? ""),
    isRowActive: () => false,
    rowCellsUseSelectionVisual: () => false,
    cellIsSelected: () => false,
    cellCanHover: () => true,
    infiniteScrollEnabled: false,
    pageSize: 100,
    currentPage: 1,
    ...overrides,
  };
}

describe("drawCanvasDataGrid columnAligns", () => {
  beforeEach(() => {
    vi.spyOn(window, "getComputedStyle").mockReturnValue({
      fontFamily: "sans-serif",
      fontSize: "13px",
      fontWeight: "400",
      lineHeight: "normal",
      getPropertyValue: () => "",
    } as CSSStyleDeclaration);
  });

  it("uses left align for all data cells when columnAligns is omitted", () => {
    const { ctx, calls } = createFakeCtx();
    drawCanvasDataGrid(createBaseOptions(createMockCanvas(ctx)));

    const dataCalls = calls.filter((c) => c.text === "left" || c.text === "right");
    expect(dataCalls.length).toBe(2);
    expect(dataCalls.every((c) => c.align === "left")).toBe(true);
  });

  it("right-aligns cells in columns marked right", () => {
    const { ctx, calls } = createFakeCtx();
    drawCanvasDataGrid(createBaseOptions(createMockCanvas(ctx), { columnAligns: ["left", "right"] }));

    const rightCall = calls.find((c) => c.text === "right");
    expect(rightCall).toBeDefined();
    expect(rightCall!.align).toBe("right");
    // Column 1 base x = rowNumberWidth(40) + col0Width(200) = 240; right anchor = 240 + 200 - 12 = 428.
    expect(rightCall!.x).toBe(428);
  });

  it("left-aligns cells in columns marked left", () => {
    const { ctx, calls } = createFakeCtx();
    drawCanvasDataGrid(createBaseOptions(createMockCanvas(ctx), { columnAligns: ["left", "right"] }));

    const leftCall = calls.find((c) => c.text === "left");
    expect(leftCall).toBeDefined();
    expect(leftCall!.align).toBe("left");
    // Column 0 base x = rowNumberWidth(40); left anchor = 40 + 12 = 52.
    expect(leftCall!.x).toBe(52);
  });

  it("draws without errors when columnAligns is empty array", () => {
    const { ctx } = createFakeCtx();
    expect(() => drawCanvasDataGrid(createBaseOptions(createMockCanvas(ctx), { columnAligns: [] }))).not.toThrow();
  });
});
