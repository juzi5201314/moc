import { fireEvent, render } from "@testing-library/react";
import { afterEach, expect, it } from "vitest";
import { useGlobalShortcuts } from "@/lib/shortcuts";
import type { ToolBlock } from "@/lib/transcript-model";
import { usePrefs } from "@/store/prefs";
import { ActivityGroup } from "./activity";

const originalDetail = usePrefs.getState().toolDetail;
afterEach(() => usePrefs.setState({ toolDetail: originalDetail }));

function Shortcuts() {
  useGlobalShortcuts();
  return null;
}

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

it("各档直接显示读取和搜索警告，不暴露原始正文或标题警告标记", () => {
  const notice = "[… 2048 bytes omitted; full output: artifact://large]";
  for (const level of ["hidden", "brief", "full"] as const) {
    usePrefs.setState({ toolDetail: level });
    const blocks = ["read", "grep", "find", "skill", "clm_context", "stop_agent"].map((name) =>
      tool(name, { result: `secret-body\n${notice}`, arguments: '{"path":"a.rs"}' }),
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
    expect(view.queryByText(/有提示|有警告/)).toBeNull();
    for (const button of view.getAllByRole("button")) {
      expect(button.textContent).not.toContain(notice);
    }
    expect(view.queryByText(/secret-body/)).toBeNull();
    view.unmount();
  }
});

it("网页摘要逐页保留标题、估算 token、截断和部分失败，错误调用仍可查看", () => {
  usePrefs.setState({ toolDetail: "brief" });
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

it("子代理简略档显示任务与报告而不是回执，批量任务分别排版", () => {
  usePrefs.setState({ toolDetail: "brief" });
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

it("可折叠行的标题钉在滚动容器顶部，嵌套子工具与状态行不钉", () => {
  // 隐藏档正文完全收起，点击标题展开才有正文——顺带验证钉住后点击仍走原折叠路径。
  usePrefs.setState({ toolDetail: "hidden" });
  const view = render(
    <ActivityGroup
      blocks={[
        { kind: "reasoning", id: "r1", text: "长思考\n".repeat(50) },
        tool("bash", {
          arguments: '{"command":"ls"}',
          result: "secret-out",
          children: [
            tool("read", {
              id: "c1",
              toolCallId: "c1",
              arguments: '{"path":"a.rs"}',
              result: "x",
            }),
          ],
        }),
        { kind: "status", id: "s1", tone: "info", text: "就绪" },
      ]}
      live={false}
      sessionId="s"
    />,
  );
  // 顶层可折叠行钉住；点击仍走原有折叠路径。
  const thinking = view.getByRole("button", { name: /思考/ });
  expect(thinking.className).toContain("sticky");
  const bash = view.getByRole("button", { name: /ls/ });
  expect(bash.className).toContain("sticky");
  const nested = view.getByRole("button", { name: /a\.rs/ });
  expect(nested.className).not.toContain("sticky");
  const status = view.getByRole("button", { name: /就绪/ });
  expect(status.className).not.toContain("sticky");
  expect((status as HTMLButtonElement).disabled).toBe(true);
  fireEvent.click(bash);
  expect(view.getByText(/secret-out/)).toBeTruthy();
  view.unmount();
});

it("Ctrl+O 只切换隐藏和简略，点击展开只影响当前工具且会被全局切换收回", () => {
  usePrefs.setState({ toolDetail: "hidden" });
  const output = Array.from({ length: 12 }, (_, index) => `日志-${index}`).join("\n");
  const view = render(
    <>
      <Shortcuts />
      <ActivityGroup
        blocks={[
          tool("bash", {
            arguments: '{"description":"检查构建","command":"build"}',
            result: output,
          }),
          tool("read", { arguments: '{"path":"src/a.rs"}', result: "源码不可见" }),
          tool("web_fetch", {
            arguments: '{"urls":["https://example.com"]}',
            result:
              'Web fetch summary: [{"url":"https://example.com","title":"网页详情","tokens":12}]',
          }),
          tool("spawn_agent", {
            arguments: '{"title":"检查任务","task":"任务正文"}',
            result: "回执",
          }),
        ]}
        live={false}
        sessionId="two-presets"
      />
    </>,
  );
  expect(view.queryByText(/日志-0|网页详情|任务正文|源码不可见/)).toBeNull();
  const toggle = () => fireEvent.keyDown(window, { code: "KeyO", ctrlKey: true });
  toggle();
  expect(usePrefs.getState().toolDetail).toBe("brief");
  expect(view.getByText("日志-11")).toBeTruthy();
  expect(view.queryByText("日志-0")).toBeNull();
  expect(view.getByText("网页详情")).toBeTruthy();
  expect(view.queryByText("源码不可见")).toBeNull();
  fireEvent.click(view.getByRole("button", { name: /检查构建/ }));
  expect(view.getByText(/日志-0/)).toBeTruthy();
  toggle();
  expect(usePrefs.getState().toolDetail).toBe("hidden");
  expect(view.queryByText(/日志-0|网页详情|任务正文/)).toBeNull();
  toggle();
  expect(usePrefs.getState().toolDetail).toBe("brief");
  expect(view.queryByText("日志-0")).toBeNull();
  view.unmount();
});

it("todo 简略档保留已完成项，跨阶段超过五项时只显示前三项和后两项", () => {
  for (const count of [4, 5, 6, 10]) {
    usePrefs.setState({ toolDetail: "brief" });
    const result = Array.from(
      { length: count },
      (_, index) => `阶段${index}\n  [x] 任务${index}\n`,
    ).join("");
    const view = render(
      <ActivityGroup
        blocks={[tool("todo", { arguments: '{"op":"done"}', result })]}
        live={false}
        sessionId="todo-preview"
      />,
    );
    for (let index = 0; index < count; index++) {
      expect(Boolean(view.queryByText(`任务${index}`))).toBe(
        count <= 5 || index < 3 || index >= count - 2,
      );
    }
    expect(Boolean(view.queryByText(`⋯ 已省略 ${count - 5} 项`))).toBe(count > 5);
    fireEvent.click(view.getByRole("button"));
    for (let index = 0; index < count; index++) {
      expect(view.getByText(`任务${index}`)).toBeTruthy();
    }
    expect(view.queryByText(/已省略/)).toBeNull();
    view.unmount();
  }
});

it("bash 中段归档提示在隐藏和简略档都可见，且不会被重复展示", () => {
  const notice = "[… 4096 bytes omitted; full output: artifact://build]";
  for (const level of ["hidden", "brief"] as const) {
    usePrefs.setState({ toolDetail: level });
    const result = `开始\n${notice}\n${Array.from({ length: 10 }, (_, index) => `日志${index}`).join("\n")}`;
    const view = render(
      <ActivityGroup blocks={[tool("bash", { result })]} live={false} sessionId="warnings" />,
    );
    expect(view.getAllByText(notice)).toHaveLength(1);
    expect(view.queryByText(/有提示|有警告/)).toBeNull();
    view.unmount();
  }
});
