import { describe, expect, it } from "vitest";
import {
  formatLineBadge,
  formatTargetDetail,
  isResourceUri,
  labelMentionsLine,
  matchSingleInternalUri,
  parseLinkTarget,
  resolvePath,
  scanInternalUris,
  splitLineRef,
} from "./file-target";

describe("splitLineRef", () => {
  it("拆 :N / #LN / #N 与行范围后缀", () => {
    expect(splitLineRef("src/main.rs:42")).toEqual({ path: "src/main.rs", line: 42 });
    expect(splitLineRef("src/main.rs:10-25")).toEqual({
      path: "src/main.rs",
      line: 10,
      endLine: 25,
    });
    expect(splitLineRef("src/main.rs#L7")).toEqual({ path: "src/main.rs", line: 7 });
    expect(splitLineRef("src/main.rs#L532-L725")).toEqual({
      path: "src/main.rs",
      line: 532,
      endLine: 725,
    });
    expect(splitLineRef("src/main.rs#L10-25")).toEqual({
      path: "src/main.rs",
      line: 10,
      endLine: 25,
    });
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
  it("相对路径与目录路径解析到工作区", () => {
    expect(parseLinkTarget("./src/a.ts:10", "/repo")).toEqual({
      target: { kind: "fs", path: "/repo/src/a.ts" },
      line: 10,
    });
    expect(parseLinkTarget("/abs/a.ts", "/repo")?.target).toEqual({
      kind: "fs",
      path: "/abs/a.ts",
    });
    expect(parseLinkTarget("src/chat/", "/repo")).toEqual({
      target: { kind: "fs", path: "/repo/src/chat" },
      isDir: true,
    });
  });
});

describe("scanInternalUris / matchSingleInternalUri / 展示辅助", () => {
  it("提取裸正文内部 URI 并正确切分中文邻接与句末标点，忽略占位符与命名空间根", () => {
    expect(
      scanInternalUris("该moc工具使用内部使用如skill://skill-a来表示skill a").map((s) => s.uri),
    ).toEqual(["skill://skill-a"]);
    expect(scanInternalUris("见 local://plan_1.md. 与 artifact://jobs/job_1.log:12-20。")).toEqual([
      { from: 2, to: 19, raw: "local://plan_1.md", uri: "local://plan_1.md" },
      {
        from: 23,
        to: 54,
        raw: "artifact://jobs/job_1.log:12-20",
        uri: "artifact://jobs/job_1.log",
        line: 12,
        endLine: 20,
      },
    ]);
    expect(scanInternalUris("local://<id>.md skill:// myskill://demo https://a.b")).toEqual([]);
  });

  it("行内代码只命中单个具体内部 URI，拒绝自然语言整句", () => {
    expect(
      matchSingleInternalUri("该moc工具使用内部使用如skill://skill-a来表示skill a"),
    ).toBeNull();
    expect(matchSingleInternalUri("local://<id>.md")).toBeNull();
    expect(matchSingleInternalUri("skill://")).toBeNull();
    expect(matchSingleInternalUri("local://计划.md:3")).toEqual({
      raw: "local://计划.md:3",
      uri: "local://计划.md",
      line: 3,
    });
  });

  it("行号徽标去重与干净路径展示", () => {
    expect(formatLineBadge(532, 725)).toBe("L532-725");
    expect(labelMentionsLine("L532-L725", 532, 725)).toBe(true);
    expect(labelMentionsLine("descriptor.rs:532", 532)).toBe(true);
    expect(labelMentionsLine("descriptor.rs", 532, 725)).toBe(false);
    expect(
      formatTargetDetail({ target: { kind: "fs", path: "/repo/src/a.ts" }, line: 9 }, "/repo"),
    ).toBe("src/a.ts:9");
  });
});
