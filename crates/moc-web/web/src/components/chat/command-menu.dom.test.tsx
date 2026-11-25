import { cleanup, fireEvent, render } from "@testing-library/react";
import { useRef } from "react";
import { afterEach, expect, it, vi } from "vitest";
import { RefChip } from "@/components/common/ref-chip";
import { Popover, PopoverAnchor } from "@/components/ui/popover";
import { TooltipProvider } from "@/components/ui/tooltip";
import {
  buildCommandEntries,
  type CommandContext,
  parseCommandChip,
  resolveStage2,
  type SubmenuAction,
} from "@/lib/command-params";
import type { SlashCommand } from "@/lib/slash";
import { CommandChipEditor } from "./command-chip-editor";
import { CommandArgMenu, CommandMenu, CommandParamHint } from "./command-menu";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const SAMPLE_COMMANDS: SlashCommand[] = [
  {
    name: "export",
    description: "将当前 session 历史脱敏导出为 Markdown",
    group: "session",
    needsSession: true,
    availableDuringTask: true,
    usage: "[path]",
    params: [
      {
        name: "path",
        kind: "path",
        required: false,
        description: "自定义保存路径，不覆盖已有文件",
        placeholder: "例如 ./notes/session.md",
        defaultBehavior: "保存到默认路径并自动下载 Markdown",
      },
    ],
  },
  {
    name: "goal",
    description: "管理 session 目标",
    group: "session",
    needsSession: false,
    availableDuringTask: true,
    usage: "[<objective>|edit <objective>|pause|resume|clear]",
    params: [
      {
        name: "objective",
        kind: "choice",
        required: false,
        acceptsText: true,
        description: "输入新目标或选择子命令",
        defaultBehavior: "查看当前目标",
        choices: [
          {
            value: "edit",
            description: "修改当前目标原文",
            params: [
              {
                name: "objective",
                kind: "text",
                required: true,
                description: "替换后的目标原文",
              },
            ],
          },
          { value: "pause", description: "暂停自动续跑" },
        ],
      },
    ],
  },
  {
    name: "ask",
    description: "只读问答",
    group: "mode",
    needsSession: false,
    availableDuringTask: true,
    usage: "<question>",
    params: [
      {
        name: "question",
        kind: "text",
        required: true,
        description: "只读提问内容",
        placeholder: "输入只读问题…",
      },
    ],
  },
];

const CTX: CommandContext = {
  isDraft: true,
  promptRunning: false,
  skillsWorking: false,
};

function stubBrowserEnv(width: number) {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  HTMLElement.prototype.scrollIntoView = vi.fn();
  Object.defineProperty(window, "innerWidth", { configurable: true, value: width });
}

function CommandMenuHarness({
  active = 1,
  focusPane = "list",
  subActive = -1,
  onPickCommand = vi.fn(),
  onPickSubAction = vi.fn(),
}: {
  active?: number;
  focusPane?: "list" | "sub";
  subActive?: number;
  onPickCommand?: (index: number) => void;
  onPickSubAction?: (action: SubmenuAction) => void;
}) {
  const inputRef = useRef<HTMLDivElement>(null);
  const entries = buildCommandEntries("/", SAMPLE_COMMANDS, undefined, CTX);
  return (
    <Popover open>
      <PopoverAnchor asChild>
        <div ref={inputRef}>input</div>
      </PopoverAnchor>
      <CommandMenu
        id="cmd-menu-test"
        side="top"
        entries={entries}
        active={active}
        focusPane={focusPane}
        subActive={subActive}
        ctx={CTX}
        onPickCommand={onPickCommand}
        onHoverCommand={vi.fn()}
        onPickSubAction={onPickSubAction}
        onHoverSubAction={vi.fn()}
        inputRef={inputRef}
      />
    </Popover>
  );
}

it("一级列表分组且不显示原始 BNF 语法，二级菜单支持点击填入并展示不可用原因", async () => {
  stubBrowserEnv(1280);
  const onPickSubAction = vi.fn();
  const view = render(<CommandMenuHarness active={1} onPickSubAction={onPickSubAction} />);

  const listbox = view.getByRole("listbox", { name: "Commands" });
  expect(listbox.textContent).toContain("Session");
  expect(listbox.textContent).toContain("Modes & Engine");
  // 一级菜单彻底移除原始 BNF 串。
  expect(listbox.textContent).not.toContain("pause|resume|clear");
  expect(listbox.textContent).not.toContain("[path]");

  // 高亮 /goal 时右侧二级面板渲染可点击的子命令项，点击只回调填词载荷。
  const detail = await view.findByRole("region", { name: "/goal" });
  expect(detail.dataset.placement).toBe("side");
  fireEvent.click(view.getByRole("menuitem", { name: /pause/ }));
  expect(onPickSubAction).toHaveBeenCalledWith(
    expect.objectContaining({ fillText: "/goal pause", closeMenu: true }),
  );

  // 切换到草稿态不可用的 /export，二级卡展示结构化参数与不可用原因。
  view.rerender(<CommandMenuHarness active={0} onPickSubAction={onPickSubAction} />);
  const exportDetail = await view.findByRole("region", { name: "/export" });
  expect(exportDetail.textContent).toContain("Optional");
  expect(exportDetail.textContent).toContain("保存到默认路径并自动下载 Markdown");
  expect(exportDetail.textContent).toContain("需要先打开一个 session");
});

function Stage2Harness({ onPick }: { onPick: (index: number) => void }) {
  const inputRef = useRef<HTMLDivElement>(null);
  const choicesState = resolveStage2("/goal ", 6, SAMPLE_COMMANDS, undefined, CTX);
  const hintState = resolveStage2("/ask ", 5, SAMPLE_COMMANDS, undefined, CTX);
  return (
    <>
      <Popover open>
        <PopoverAnchor asChild>
          <div ref={inputRef}>input</div>
        </PopoverAnchor>
        {choicesState?.kind === "choices" && (
          <CommandArgMenu
            id="arg-test"
            side="top"
            state={choicesState}
            active={0}
            onPick={onPick}
            onHover={vi.fn()}
            inputRef={inputRef}
          />
        )}
      </Popover>
      <Popover open>
        <PopoverAnchor asChild>
          <div>input2</div>
        </PopoverAnchor>
        {hintState?.kind === "hint" && (
          <CommandParamHint side="top" state={hintState} missingError inputRef={inputRef} />
        )}
      </Popover>
    </>
  );
}

it("阶段二参数浮层与自由文本必填报错提示渲染清晰", () => {
  stubBrowserEnv(1280);
  const onPick = vi.fn();

  const view = render(<Stage2Harness onPick={onPick} />);
  fireEvent.click(view.getByRole("option", { name: /edit/ }));
  expect(onPick).toHaveBeenCalledWith(0);

  const hint = view.getByRole("status");
  expect(hint.textContent).toContain("Required");
  expect(hint.textContent).toContain("请先填写必填参数 question");
});

it("命令胶囊同块渲染非自然语言参数，点开浮层可切换子命令并编辑数值参数", () => {
  stubBrowserEnv(1280);
  const loopCmd: SlashCommand = {
    name: "loop",
    description: "重复执行命令",
    usage: "<count> <command>",
    plugin: "demo",
  };
  const allCommands = [...SAMPLE_COMMANDS, loopCmd];
  const onChipClick = vi.fn();
  const anchor = document.createElement("div");

  // 胶囊内分层展示命令名与非自然语言参数，且支持点击。
  const chipView = render(
    <TooltipProvider>
      <RefChip
        chip={{ kind: "command", label: "loop", args: ["3"], interactive: true }}
        onClick={onChipClick}
      />
    </TooltipProvider>,
  );
  const chipButton = chipView.getByRole("button");
  expect(chipButton.textContent).toBe("loop3");
  expect(chipButton.querySelector('[data-slot="ref-chip-args"]')?.textContent).toBe("3");
  fireEvent.click(chipButton);
  expect(onChipClick).toHaveBeenCalledTimes(1);
  chipView.unmount();

  // 子命令编辑：`/goal pause` 切换为 `edit` 自动保留尾随空格待填目标。
  const goalParsed = parseCommandChip("/goal pause", allCommands, CTX);
  if (!goalParsed) throw new Error("expected /goal pause to parse");
  const onGoalApply = vi.fn();
  const goalView = render(
    <CommandChipEditor
      open
      side="top"
      parsed={goalParsed}
      text="/goal pause"
      ctx={CTX}
      getAnchorElement={() => anchor}
      onApply={onGoalApply}
      onClose={vi.fn()}
    />,
  );
  fireEvent.click(goalView.getByRole("menuitemradio", { name: /edit/ }));
  expect(onGoalApply).toHaveBeenCalledWith("/goal edit ", 11, true, true);
  goalView.unmount();

  // 数值参数编辑：`/loop 3 echo pong` 改 `3` 为 `5` 并校验非法数字。
  const loopParsed = parseCommandChip("/loop 3 echo pong", allCommands, CTX);
  if (!loopParsed) throw new Error("expected /loop 3 echo pong to parse");
  const onLoopApply = vi.fn();
  const loopView = render(
    <CommandChipEditor
      open
      side="top"
      parsed={loopParsed}
      text="/loop 3 echo pong"
      ctx={CTX}
      getAnchorElement={() => anchor}
      onApply={onLoopApply}
      onClose={vi.fn()}
    />,
  );
  const countInput = loopView.getByLabelText("count") as HTMLInputElement;
  expect(countInput.value).toBe("3");
  fireEvent.change(countInput, { target: { value: "abc" } });
  fireEvent.keyDown(countInput, { key: "Enter" });
  expect(onLoopApply).not.toHaveBeenCalled();
  expect(loopView.getByRole("dialog").textContent).toContain("请输入有效数字");

  fireEvent.change(countInput, { target: { value: "5" } });
  fireEvent.keyDown(countInput, { key: "Enter" });
  expect(onLoopApply).toHaveBeenCalledWith("/loop 5 echo pong", 17, true, true);
});
