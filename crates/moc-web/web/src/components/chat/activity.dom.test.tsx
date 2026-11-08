import { fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import type { ToolBlock } from "@/lib/transcript-model";
import { usePrefs } from "@/store/prefs";
import { ActivityGroup } from "./activity";

const originalDetail = usePrefs.getState().toolDetail;
afterEach(() => usePrefs.setState({ toolDetail: originalDetail }));

function tool(name: string, extra: Partial<ToolBlock> = {}): ToolBlock {
  return {
    kind: "tool",
    id: name,
    toolCallId: name,
    name,
    arguments: "{}",
    status: "completed",
    isError: false,
    images: [],
    children: [],
    awaitingPermission: false,
    ...extra,
  };
}

it("固定摘要在三档和点击后都不暴露正文，截断警告不会被合并组藏住", () => {
  const notice = "[… 2048 bytes omitted; full output: artifact://large]";
  for (const level of ["hidden", "brief", "full"] as const) {
    usePrefs.setState({ toolDetail: level });
    const blocks = ["read", "grep", "find", "web_fetch", "skill", "clm_context", "stop_agent"].map(
      (name) => tool(name, { result: `secret-body\n${notice}`, arguments: '{"path":"a.rs"}' }),
    );
    blocks.splice(
      1,
      0,
      tool("read", {
        id: "read-2",
        toolCallId: "read-2",
        batchId: "b",
        result: `secret-body\n${notice}`,
      }),
    );
    blocks[0].batchId = "b";
    const view = render(<ActivityGroup blocks={blocks} live={false} sessionId="summary" />);
    expect(view.queryByText(/secret-body/)).toBeNull();
    expect(view.getAllByText(notice)).toHaveLength(blocks.length);
    for (const button of view.getAllByRole("button")) {
      expect((button as HTMLButtonElement).disabled).toBe(true);
      fireEvent.click(button);
    }
    expect(view.queryByText(/secret-body/)).toBeNull();
    view.unmount();
  }
});

it("网页摘要逐页保留标题、估算 token、截断和部分失败，错误调用仍可查看", () => {
  usePrefs.setState({ toolDetail: "hidden" });
  const result = `Web fetch summary: ${JSON.stringify([
    { url: "https://a.test", title: "Rust 文档", tokens: 1234, truncated: true },
    { url: "https://b.test", error: "HTTP 403" },
  ])}\n\nsecret-page-body`;
  const view = render(
    <ActivityGroup
      blocks={[
        tool("web_fetch", { arguments: '{"urls":["https://a.test","https://b.test"]}', result }),
        tool("read", { result: "文件不存在\n错误详情", status: "error", isError: true }),
      ]}
      live={false}
      sessionId="summary"
    />,
  );
  expect(view.getByText("Rust 文档")).toBeTruthy();
  expect(view.getByText(/约 1,234 tokens/)).toBeTruthy();
  expect(view.getByText("警告：内容已截断")).toBeTruthy();
  expect(view.getByText("抓取失败：HTTP 403")).toBeTruthy();
  expect(view.queryByText(/secret-page-body/)).toBeNull();
  expect(view.getByText("文件不存在")).toBeTruthy();
  fireEvent.click(view.getByRole("button", { name: /读取/ }));
  expect(view.getByText(/错误详情/)).toBeTruthy();
  view.unmount();
});

it("子代理显示任务与报告而不是回执，批量任务分别排版且完整档不能展开", () => {
  usePrefs.setState({ toolDetail: "full" });
  const long =
    "**检查边界**\n\n- 核对错误路径\n- 核对取消路径\n- 核对缓存\n- 核对结果\nsecret-tail";
  const view = render(
    <ActivityGroup
      blocks={[
        tool("spawn_agent", {
          arguments: JSON.stringify({
            tasks: [
              { title: "探索解析器", task: long, role: "scout" },
              { title: "探索渲染器", task: "检查布局", role: "scout" },
            ],
          }),
          result: "secret-receipt",
        }),
        tool("yield", { arguments: JSON.stringify({ report: long }), result: "secret-receipt" }),
      ]}
      live={false}
      sessionId="summary"
    />,
  );
  expect(view.getByText("探索解析器")).toBeTruthy();
  expect(view.getByText("探索渲染器")).toBeTruthy();
  expect(view.getAllByText("检查边界")).toHaveLength(2);
  expect(view.queryByText(/secret-tail|secret-receipt/)).toBeNull();
  for (const button of view.getAllByRole("button"))
    expect((button as HTMLButtonElement).disabled).toBe(true);
  view.unmount();
});
