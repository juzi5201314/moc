import { toast } from "sonner";
import { afterEach, expect, it, vi } from "vitest";
import { useI18n } from "@/lib/i18n";
import { rpc } from "@/protocol/client";
import { fromMeta, runSlash } from "./slash";

vi.mock("sonner", () => ({ toast: Object.assign(vi.fn(), { success: vi.fn(), error: vi.fn() }) }));

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("/export 共用 RPC，下载返回正文并显示路径，失败时不下载", async () => {
  const originalLocale = useI18n.getState().locale;
  const create = vi.fn().mockReturnValue("blob:moc-export");
  const revoke = vi.fn();
  vi.stubGlobal("URL", { createObjectURL: create, revokeObjectURL: revoke });
  const downloads: { name: string; connected: boolean }[] = [];
  vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (
    this: HTMLAnchorElement,
  ) {
    downloads.push({ name: this.download, connected: this.isConnected });
  });
  const call = vi.spyOn(rpc, "call");
  const commands = [
    fromMeta({
      name: "export",
      aliases: [],
      usage: "[path]",
      description: "export",
      group: "session",
      needsSession: true,
      availableDuringTask: true,
      availableInSide: true,
    }),
  ];
  try {
    for (const [locale, input, content] of [
      ["zh", "/export reports/会话.md", "# 会话\n\n正文"],
      ["en", "/export", ""],
    ] as const) {
      useI18n.setState({ locale });
      call.mockResolvedValueOnce({ path: "/workspace/会话.md", content });
      expect(runSlash("session", input, commands)).toBe(true);
      await vi.waitFor(() =>
        expect(toast.success).toHaveBeenCalledWith(expect.stringContaining("/workspace/会话.md")),
      );
      expect(call).toHaveBeenLastCalledWith("session/export", {
        session_id: "session",
        ...(locale === "zh" ? { path: "reports/会话.md" } : {}),
      });
      expect(create).toHaveBeenLastCalledWith(expect.any(Blob));
      expect(downloads.at(-1)).toEqual({ name: "会话.md", connected: true });
      expect(document.querySelector("a[download]")).toBeNull();
      vi.mocked(toast.success).mockClear();
    }
    for (const locale of ["zh", "en"] as const) {
      useI18n.setState({ locale });
      call.mockResolvedValueOnce({
        path: "/workspace/large.md",
        ...(locale === "en" ? { content: null } : {}),
      });
      runSlash("session", "/export", commands);
      await vi.waitFor(() =>
        expect(toast.success).toHaveBeenCalledWith(
          locale === "zh"
            ? "会话已导出至 /workspace/large.md；内容较大，请从该路径获取文件"
            : "Session exported to /workspace/large.md; too large to download here. Retrieve the file from that path.",
        ),
      );
      expect(create).toHaveBeenCalledTimes(2);
      expect(downloads).toHaveLength(2);
      vi.mocked(toast.success).mockClear();
    }
    call.mockRejectedValueOnce(new Error("File exists"));
    runSlash("session", "/export reports/会话.md", commands);
    await vi.waitFor(() =>
      expect(toast.error).toHaveBeenCalledWith(expect.stringContaining("File exists")),
    );
    expect(downloads).toHaveLength(2);
    call.mockClear();
    runSlash("draft-session", "/export", commands);
    expect(call).not.toHaveBeenCalled();
    await vi.waitFor(() => expect(revoke).toHaveBeenCalledTimes(2), { timeout: 2000 });
  } finally {
    useI18n.setState({ locale: originalLocale });
  }
});
