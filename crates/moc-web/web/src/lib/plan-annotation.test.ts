import { beforeEach, describe, expect, it } from "vitest";
import {
  annotationAt,
  appendSuggestion,
  rangeFromSelection,
  sourceLines,
  splitSuggestion,
} from "@/lib/plan-annotation";

describe("sourceLines", () => {
  it("末尾换行不额外多出一行", () => {
    expect(sourceLines("# 计划\n\n1. 先看代码\n")).toEqual(["# 计划", "", "1. 先看代码"]);
    expect(sourceLines("")).toEqual([]);
  });
});

describe("suggestion 围栏", () => {
  it("没有围栏时返回 null", () => {
    expect(splitSuggestion("这一行太笼统")).toBeNull();
  });

  it("拆出意见与替换文本", () => {
    const body = "太笼统\n\n```suggestion\n1. 先看 InspectorPanel\n```";
    expect(splitSuggestion(body)).toEqual({
      note: "太笼统",
      suggestion: "1. 先看 InspectorPanel",
    });
  });

  it("追加围栏时保留已有意见", () => {
    const body = appendSuggestion("太笼统", "1. 先看 InspectorPanel");
    expect(body).toBe("太笼统\n\n```suggestion\n1. 先看 InspectorPanel\n```");
  });

  it("已有围栏时原地替换", () => {
    const body = appendSuggestion(
      "太笼统\n\n```suggestion\n旧内容\n```\n",
      "1. 先看 InspectorPanel",
    );
    expect(splitSuggestion(body)?.suggestion).toBe("1. 先看 InspectorPanel");
    expect(splitSuggestion(body)?.note).toBe("太笼统");
  });
});

describe("annotationAt", () => {
  it("按行范围命中批注", () => {
    const annotations = [
      { id: "a1", start_line: 2, end_line: 4, quote: "", body: "改这里" },
      { id: "a2", start_line: 7, end_line: 7, quote: "", body: "别动" },
    ];
    expect(annotationAt(annotations, 3)?.id).toBe("a1");
    expect(annotationAt(annotations, 5)).toBeUndefined();
    expect(annotationAt(annotations, 7)?.id).toBe("a2");
  });
});

describe("rangeFromSelection", () => {
  beforeEach(() => {
    document.body.innerHTML = `
      <div id="root">
        <p data-line-start="3" data-line-end="3">先看代码</p>
        <p data-line-start="5" data-line-end="6">再做修改</p>
      </div>`;
  });

  it("跨块的选区覆盖首尾块的整个行范围", () => {
    const [first, second] = Array.from(document.querySelectorAll("p"));
    const range = document.createRange();
    range.setStart(first.firstChild as Text, 0);
    range.setEnd(second.firstChild as Text, 2);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
    expect(rangeFromSelection(selection)).toEqual({ startLine: 3, endLine: 6 });
  });

  it("选区为空时返回 null", () => {
    const selection = window.getSelection();
    selection?.removeAllRanges();
    expect(rangeFromSelection(selection)).toBeNull();
  });
});
