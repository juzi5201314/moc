import { describe, expect, it } from "vitest";
import { isResourceUri, parseLinkTarget, resolvePath, splitLineRef } from "./file-target";

describe("splitLineRef", () => {
  it("拆 :N 与 #LN / #N 行号后缀", () => {
    expect(splitLineRef("src/main.rs:42")).toEqual({ path: "src/main.rs", line: 42 });
    expect(splitLineRef("src/main.rs#L7")).toEqual({ path: "src/main.rs", line: 7 });
    expect(splitLineRef("src/main.rs#7")).toEqual({ path: "src/main.rs", line: 7 });
    expect(splitLineRef("src/main.rs")).toEqual({ path: "src/main.rs", line: undefined });
  });
  it("Windows 盘符的冒号不拆", () => {
    expect(splitLineRef("C:\\work\\a.ts")).toEqual({ path: "C:\\work\\a.ts", line: undefined });
  });
  it("URI 里的端口样冒号不动", () => {
    expect(splitLineRef("artifact://jobs/1.log")).toEqual({
      path: "artifact://jobs/1.log",
      line: undefined,
    });
  });
});

describe("resolvePath", () => {
  it("绝对路径与 ~ 原样；相对路径接 cwd", () => {
    expect(resolvePath("/a/b", "/w")).toBe("/a/b");
    expect(resolvePath("~/a", "/w")).toBe("~/a");
    expect(resolvePath("src/x.ts", "/work/dir")).toBe("/work/dir/src/x.ts");
    expect(resolvePath("src/x.ts", "/work/dir/")).toBe("/work/dir/src/x.ts");
  });

  it("折叠 ./ 与 ../ 段", () => {
    expect(resolvePath("./src/a.ts", "/repo")).toBe("/repo/src/a.ts");
    expect(resolvePath("../shared/b.ts", "/repo/pkg")).toBe("/repo/shared/b.ts");
    expect(resolvePath("./x/../y.ts", "/repo")).toBe("/repo/y.ts");
    expect(resolvePath("/abs/./p/../q", "/repo")).toBe("/abs/q");
  });
});

describe("isResourceUri / parseLinkTarget", () => {
  it("识别会话空间 URI", () => {
    for (const uri of ["local://p.md", "artifact://jobs/1.log", "history://", "skill://x/y"]) {
      expect(isResourceUri(uri)).toBe(true);
      expect(parseLinkTarget(uri, "/w")?.target).toEqual({ kind: "resource", uri });
    }
    expect(isResourceUri("file:///a")).toBe(false);
    expect(isResourceUri("ftp://x")).toBe(false);
  });
  it("URI 也能带行号", () => {
    expect(parseLinkTarget("local://p.md:3", "/w")).toEqual({
      target: { kind: "resource", uri: "local://p.md" },
      line: 3,
    });
  });
  it("外部链接与锚点返回 null", () => {
    for (const href of ["https://a.b", "mailto:x@y", "#section", "tel:1", ""]) {
      expect(parseLinkTarget(href, "/w")).toBeNull();
    }
  });
  it("相对路径解析到工作区", () => {
    expect(parseLinkTarget("./src/a.ts:10", "/repo")).toEqual({
      target: { kind: "fs", path: "/repo/src/a.ts" },
      line: 10,
    });
    expect(parseLinkTarget("/abs/a.ts", "/repo")?.target).toEqual({
      kind: "fs",
      path: "/abs/a.ts",
    });
  });
});
