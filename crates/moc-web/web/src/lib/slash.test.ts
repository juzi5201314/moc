import { toast } from "sonner";
import { afterEach, describe, expect, it, vi } from "vitest";
import { rpc } from "@/protocol/client";
import type { CommandMeta } from "@/protocol/types";
import { refreshSideData } from "@/store/lifecycle";
import { fromMeta, parseSlash, runSlash, slashMatches } from "./slash";

vi.mock("@/store/lifecycle", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/store/lifecycle")>()),
  refreshSideData: vi.fn().mockResolvedValue(undefined),
}));

afterEach(() => vi.restoreAllMocks());

function meta(partial: Partial<CommandMeta> & { name: string }): CommandMeta {
  return {
    aliases: [],
    usage: "",
    description: "",
    group: "ui",
    needsSession: false,
    availableDuringTask: true,
    availableInSide: true,
    ...partial,
  };
}

describe("slash 注册表语义", () => {
  it("插件重载成功刷新会话命令缓存，草稿或失败不刷新", async () => {
    const refresh = vi.mocked(refreshSideData);
    refresh.mockClear();
    const call = vi.spyOn(rpc, "call").mockResolvedValue({});
    vi.spyOn(toast, "success").mockImplementation(() => "toast");
    const error = vi.spyOn(toast, "error").mockImplementation(() => "toast");
    const commands = [fromMeta(meta({ name: "plugins", usage: "[reload]" }))];
    expect(runSlash("session", "/plugins reload", commands)).toBe(true);
    await vi.waitFor(() => expect(refresh).toHaveBeenCalledWith("session"));
    expect(call).toHaveBeenLastCalledWith("plugin/reload", { session_id: "session" });

    refresh.mockClear();
    runSlash("draft-session", "/plugins reload", commands);
    await vi.waitFor(() => expect(toast.success).toHaveBeenCalledTimes(2));
    expect(call).toHaveBeenLastCalledWith("plugin/reload", { session_id: "" });
    expect(refresh).not.toHaveBeenCalled();

    call.mockRejectedValueOnce(new Error("reload failed"));
    runSlash("session", "/plugins reload", commands);
    await vi.waitFor(() => expect(error).toHaveBeenCalled());
    expect(refresh).not.toHaveBeenCalled();
  });

  it("精确命令名优先，其他候选保持原顺序", () => {
    const commands = ["compact-skills", "compact", "compact-other"].map((name) =>
      fromMeta(meta({ name })),
    );
    expect(slashMatches("/compact", commands).map((c) => c.name)).toEqual([
      "compact",
      "compact-skills",
      "compact-other",
    ]);
    expect(slashMatches("/co", commands)).toEqual(commands);
    expect(slashMatches("/", commands)).toEqual(commands);
    expect(slashMatches("/missing", commands)).toEqual([]);
  });

  it("补全命中主名与别名", () => {
    const commands = [
      fromMeta(meta({ name: "quit", aliases: ["exit"] })),
      fromMeta(meta({ name: "ask" })),
    ];
    expect(slashMatches("/ex", commands).map((c) => c.name)).toEqual(["quit"]);
    expect(slashMatches("/a", commands).map((c) => c.name)).toEqual(["ask"]);
  });

  it("flag 关闭的命令不进补全", () => {
    const commands = [
      fromMeta(meta({ name: "workflow", flag: "workflow", group: "agents" })),
      fromMeta(meta({ name: "new", group: "session" })),
    ];
    const features = { workflow: false };
    expect(slashMatches("/", commands, features).map((c) => c.name)).toEqual(["new"]);
    // 快照未加载（undefined）或特性开启时照常提示。
    expect(slashMatches("/", commands).map((c) => c.name)).toEqual(["workflow", "new"]);
  });

  it("usage 空串映射为 undefined", () => {
    expect(fromMeta(meta({ name: "x", usage: "<问题>" })).usage).toBe("<问题>");
    expect(fromMeta(meta({ name: "x" })).usage).toBeUndefined();
  });

  it("绝对路径和未注册命令作为普通消息发送", () => {
    const commands = [fromMeta(meta({ name: "goal" }))];
    for (const text of [
      "/tmp/a.txt",
      "/tmp",
      "/goal/file.txt",
      "/tmp/my file.txt 请检查",
      "/unknown-command",
    ]) {
      expect(slashMatches(text, commands)).toEqual([]);
      expect(runSlash("test", text, commands)).toBe(false);
    }
  });

  it("parseSlash 拆分名字与参数", () => {
    expect(parseSlash("/ask 你好")).toEqual({ name: "ask", args: "你好" });
    expect(parseSlash("/retry")).toEqual({ name: "retry", args: "" });
    expect(parseSlash("普通文本")).toBeNull();
  });
});
