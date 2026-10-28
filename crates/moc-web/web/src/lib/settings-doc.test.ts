import { describe, expect, it } from "vitest";
import type { DiffLine } from "@/protocol/types";
import { breadcrumbs, toSplitRows } from "./format";
import { choiceDisplay, formatField, getPath, parseField, setPath } from "./settings-doc";

describe("settings document paths", () => {
  it("reads nested values and returns undefined for missing ones", () => {
    const doc = { a: { b: { c: 1 } } };
    expect(getPath(doc, ["a", "b", "c"])).toBe(1);
    expect(getPath(doc, ["a", "x", "c"])).toBeUndefined();
  });

  it("sets values immutably and prunes emptied parents on delete", () => {
    const doc = { a: { b: 1 }, keep: true };
    const updated = setPath(doc, ["a", "c"], 2);
    expect(updated).toEqual({ a: { b: 1, c: 2 }, keep: true });
    expect(doc).toEqual({ a: { b: 1 }, keep: true });
    expect(setPath(doc, ["a", "b"], undefined)).toEqual({ keep: true });
  });
});

describe("field parsing", () => {
  it("accepts scaled integers and thousands separators", () => {
    expect(parseField("200k", { type: "integer" })).toEqual({ ok: true, value: 200_000 });
    expect(parseField("1m", { type: "integer" })).toEqual({ ok: true, value: 1_000_000 });
    expect(parseField("12,345", { type: "integer" })).toEqual({ ok: true, value: 12_345 });
    expect(parseField("1.5", { type: "integer" }).ok).toBe(false);
  });

  it("rejects negative numbers and maps empty text to undefined", () => {
    expect(parseField("0.5", { type: "number" })).toEqual({ ok: true, value: 0.5 });
    expect(parseField("-1", { type: "number" }).ok).toBe(false);
    expect(parseField("   ", { type: "number" })).toEqual({ ok: true, value: undefined });
  });

  it("splits string lists and round-trips through formatField", () => {
    const parsed = parseField(" a, b ,,c ", { type: "strings" });
    expect(parsed).toEqual({ ok: true, value: ["a", "b", "c"] });
    expect(formatField(["a", "b"], { type: "strings" })).toBe("a, b");
  });

  it("requires JSON objects for object fields", () => {
    expect(parseField('{"x":1}', { type: "object" })).toEqual({ ok: true, value: { x: 1 } });
    expect(parseField("[1]", { type: "object" }).ok).toBe(false);
    expect(parseField("{", { type: "object" }).ok).toBe(false);
  });

  it("shows choice labels once when equal to the value", () => {
    expect(choiceDisplay({ value: "on", label: "on" })).toBe("on");
    expect(choiceDisplay({ value: "on", label: "开启" })).toBe("开启（on）");
  });
});

describe("diff split rows", () => {
  it("pairs delete runs with following inserts", () => {
    const line = (kind: DiffLine["kind"], text: string): DiffLine => ({ kind, text });
    const rows = toSplitRows([
      line("equal", "a"),
      line("delete", "b"),
      line("delete", "c"),
      line("insert", "B"),
      line("equal", "d"),
    ]);
    expect(rows.map((row) => [row.left?.text, row.right?.text])).toEqual([
      ["a", "a"],
      ["b", "B"],
      ["c", undefined],
      ["d", "d"],
    ]);
  });
});

describe("folder breadcrumbs", () => {
  it("builds cumulative POSIX paths", () => {
    expect(breadcrumbs("/home/me")).toEqual([
      { name: "/", path: "/" },
      { name: "home", path: "/home" },
      { name: "me", path: "/home/me" },
    ]);
  });

  it("handles Windows drive roots", () => {
    expect(breadcrumbs("C:\\Users\\me")).toEqual([
      { name: "C:", path: "C:\\" },
      { name: "Users", path: "C:\\Users" },
      { name: "me", path: "C:\\Users\\me" },
    ]);
  });
});
