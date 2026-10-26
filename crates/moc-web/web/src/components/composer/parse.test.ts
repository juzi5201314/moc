import { describe, expect, it } from "vitest";
import {
  applyCompletion,
  buildSelector,
  currentTokenRange,
  parseBash,
  parseSelector,
  parseSlash,
  resourcePrefix,
} from "./parse";

describe("parseBash", () => {
  it("parses ! as in-context bash", () => {
    expect(parseBash("!ls -la")).toEqual({ command: "ls -la", exclude_from_context: false });
  });

  it("parses !! as excluded from context", () => {
    expect(parseBash("!!git status")).toEqual({
      command: "git status",
      exclude_from_context: true,
    });
  });

  it("only consumes two leading bangs", () => {
    expect(parseBash("!!!x")).toEqual({ command: "!x", exclude_from_context: true });
  });

  it("ignores text without a leading bang", () => {
    expect(parseBash("echo !hi")).toBeNull();
  });
});

describe("parseSlash", () => {
  it("splits name and trimmed args", () => {
    expect(parseSlash("/mode  plan ")).toEqual({ name: "mode", args: "plan" });
  });

  it("keeps multi-line args", () => {
    expect(parseSlash("/compact keep\nthe tests")).toEqual({
      name: "compact",
      args: "keep\nthe tests",
    });
  });

  it("accepts commands without args", () => {
    expect(parseSlash("/usage")).toEqual({ name: "usage", args: "" });
  });

  it("treats // as a resource, not a command", () => {
    expect(parseSlash("//skill")).toBeNull();
  });

  it("rejects plain text and a bare slash", () => {
    expect(parseSlash("hello")).toBeNull();
    expect(parseSlash("/")).toBeNull();
  });
});

describe("resource prefix", () => {
  it("finds the token around the cursor on the current line", () => {
    const text = "first line\nsee //sk here";
    const cursor = text.indexOf("//sk") + 4;
    expect(currentTokenRange(text, cursor)).toEqual({ start: 15, end: 19 });
  });

  it("detects //, :/ and :// prefixes", () => {
    expect(resourcePrefix("open //", 7)?.prefix).toBe("//");
    expect(resourcePrefix("read file:/", 11)?.prefix).toBe("file:/");
    expect(resourcePrefix("mcp://serv", 10)?.prefix).toBe("mcp://serv");
  });

  it("ignores ordinary tokens", () => {
    expect(resourcePrefix("path/to/file", 12)).toBeNull();
    expect(resourcePrefix("a / b", 3)).toBeNull();
  });

  it("replaces the token and optionally appends a space", () => {
    const text = "use skill://re now";
    const hit = resourcePrefix(text, 14);
    expect(hit).not.toBeNull();
    if (!hit) return;
    expect(applyCompletion(text, hit.range, "skill://review", true)).toEqual({
      text: "use skill://review  now",
      cursor: 19,
    });
    expect(applyCompletion(text, hit.range, "skill://review", false).text).toBe(
      "use skill://review now",
    );
  });
});

describe("model selector", () => {
  const levels: Record<string, string[]> = { "openai/gpt": ["off", "low", "high"] };
  const levelsOf = (id: string) => levels[id];

  it("splits a known level off the last colon", () => {
    expect(parseSelector("openai/gpt:high", levelsOf)).toEqual({ id: "openai/gpt", level: "high" });
  });

  it("keeps the whole string when the suffix is not a level", () => {
    expect(parseSelector("ollama/qwen:7b", levelsOf)).toEqual({
      id: "ollama/qwen:7b",
      level: null,
    });
  });

  it("builds selectors with and without a level", () => {
    expect(buildSelector("openai/gpt", "low")).toBe("openai/gpt:low");
    expect(buildSelector("openai/gpt", null)).toBe("openai/gpt");
  });
});
