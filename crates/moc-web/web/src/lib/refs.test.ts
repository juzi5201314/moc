import { describe, expect, it } from "vitest";
import { absorbRefs } from "@/store/composer";
import { remapImageRefs, scanRefs, uniqueRefName } from "./refs";

const image = (data: string) => ({
  id: data,
  name: data,
  source: { Base64: { media_type: "image/png", data } },
});

describe("引用扫描", () => {
  it("#N 只认已有图片且可紧贴中文，#name 取最长的已知文件名，资源 token 按空白切分", () => {
    const text =
      "/goal 看看#2是羊还是#1是羊，#3 不存在，C#1 不算，#notes.md 与 #notes.md.bak @src/a.rs skill://";
    const spans = scanRefs(text, {
      command: { start: 0, end: 5 },
      imageCount: 2,
      fileNames: ["notes.md", "notes.md.bak"],
    });
    expect(spans.map((span) => [span.kind, span.token])).toEqual([
      ["command", "/goal"],
      ["image", "#2"],
      ["image", "#1"],
      ["file-ref", "#notes.md"],
      ["file-ref", "#notes.md.bak"],
      ["resource", "@src/a.rs"],
    ]);
  });

  it("删除图片时删掉它的引用、后面的序号前移", () => {
    const text = "#1 和 #2 还有 #3";
    expect(remapImageRefs(text, 3, (n) => (n === 2 ? null : n > 2 ? n - 1 : n))).toBe(
      "#1 和 还有 #2",
    );
  });

  it("文件引用名去空白、避开纯数字与重名", () => {
    expect(uniqueRefName("my report.pdf", [])).toBe("my_report.pdf");
    expect(uniqueRefName("12", [])).toBe("file-12");
    expect(uniqueRefName("a.txt", ["a.txt", "a-2.txt"])).toBe("a-3.txt");
  });

  it("并入另一份内容时图片同内容复用、文件同路径复用，文本引用随之改写", () => {
    const base = {
      text: "",
      images: [image("x")],
      files: [{ id: "f", name: "a.txt", size: 1, path: "/p/a.txt" }],
    };
    const merged = absorbRefs(base, {
      text: "#1 #2 #a.txt #b.txt",
      images: [image("y"), image("x")],
      files: [
        { id: "g", name: "a.txt", size: 1, path: "/p/a.txt" },
        { id: "h", name: "b.txt", size: 2, path: "/q/b.txt" },
      ],
    });
    expect(merged.text).toBe("#2 #1 #a.txt #b.txt");
    expect(merged.images.map((entry) => entry.name)).toEqual(["x", "y"]);
    expect(merged.files.map((entry) => entry.path)).toEqual(["/p/a.txt", "/q/b.txt"]);
  });
});
