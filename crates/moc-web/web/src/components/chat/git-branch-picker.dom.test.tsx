import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useI18n } from "@/lib/i18n";
import { invalidateGitBranch, resetGitBranchCache } from "@/lib/use-git-branch";
import { rpc } from "@/protocol/client";
import type {
  WorkspaceGitBranchesResult,
  WorkspaceGitBranchResult,
  WorkspaceGitSwitchParams,
  WorkspaceGitSwitchResult,
} from "@/protocol/types";
import { GitBranchPicker } from "./git-branch-picker";

const initialLocale = useI18n.getState().locale;
const scrollIntoView = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollIntoView");
let callSpy: ReturnType<typeof vi.spyOn>;

beforeEach(() => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
    configurable: true,
    value: vi.fn(),
  });
  useI18n.setState({ locale: "zh" });
  resetGitBranchCache();
  callSpy = vi.spyOn(rpc, "call");
});

afterEach(() => {
  cleanup();
  resetGitBranchCache();
  useI18n.setState({ locale: initialLocale });
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  if (scrollIntoView)
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", scrollIntoView);
  else Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
});

it("支持输入新分支名直接创建并切换，且即时更新按钮标签", async () => {
  let currentBranch = "master";
  callSpy.mockImplementation(async (method: string, params?: unknown) => {
    if (method === "workspace/git_branch") {
      return {
        branch: currentBranch,
        root: "/repo",
        head_oid: "abc1234",
        detached: false,
        unborn: false,
      } satisfies WorkspaceGitBranchResult;
    }
    if (method === "workspace/git_branches") {
      return {
        in_repo: true,
        root: "/repo",
        current: currentBranch,
        head_oid: "abc1234",
        detached: false,
        unborn: false,
        status: { staged: 0, unstaged: 1, untracked: 0, conflicted: 0 },
        local: [{ name: "master", current: true }],
        remote: [{ name: "origin/main", remote: "origin", branch: "main", has_local: false }],
      } satisfies WorkspaceGitBranchesResult;
    }
    if (method === "workspace/git_switch") {
      const p = params as unknown as WorkspaceGitSwitchParams;
      expect(p.create).toBe(true);
      expect(p.branch).toBe("feat/new-branch");
      currentBranch = p.branch;
      return {
        outcome: "switched",
        branch: p.branch,
        previous_branch: "master",
        created: true,
      } satisfies WorkspaceGitSwitchResult;
    }
    throw new Error(`unexpected method: ${method}`);
  });

  const view = render(<GitBranchPicker cwd="/repo" variant="welcome" />);
  const trigger = await view.findByRole("button", { name: /master/ });
  fireEvent.click(trigger);

  const input = await view.findByRole("combobox");
  fireEvent.change(input, { target: { value: "feat/new-branch" } });

  const createOption = await view.findByRole("option", { name: /feat\/new-branch/ });
  fireEvent.click(createOption);

  await waitFor(() => {
    expect(view.getByRole("button", { name: /feat\/new-branch/ })).toBeTruthy();
  });
});

it("脏工作区切换弹确认框，Carry 冲突后收敛为 Stash & switch 并完成切换", async () => {
  let currentBranch = "master";
  const switchCalls: WorkspaceGitSwitchParams[] = [];

  callSpy.mockImplementation(async (method: string, params?: unknown) => {
    if (method === "workspace/git_branch") {
      return {
        branch: currentBranch,
        root: "/repo-dirty",
        head_oid: "1112223",
        detached: false,
        unborn: false,
      } satisfies WorkspaceGitBranchResult;
    }
    if (method === "workspace/git_branches") {
      return {
        in_repo: true,
        root: "/repo-dirty",
        current: currentBranch,
        head_oid: "1112223",
        detached: false,
        unborn: false,
        status: { staged: 0, unstaged: 1, untracked: 1, conflicted: 0 },
        local: [
          { name: "master", current: true },
          { name: "feat/target", current: false },
        ],
        remote: [],
      } satisfies WorkspaceGitBranchesResult;
    }
    if (method === "workspace/git_switch") {
      const p = params as unknown as WorkspaceGitSwitchParams;
      switchCalls.push(p);
      if (p.dirty === "check") {
        return {
          outcome: "needs_confirmation",
          status: { staged: 0, unstaged: 1, untracked: 1, conflicted: 0 },
          dirty_files: ["src/app.ts", "tmp.txt"],
          detached: false,
          overwrite_conflict: false,
        } satisfies WorkspaceGitSwitchResult;
      }
      if (p.dirty === "carry") {
        return {
          outcome: "needs_confirmation",
          status: { staged: 0, unstaged: 1, untracked: 1, conflicted: 0 },
          dirty_files: ["src/app.ts", "tmp.txt"],
          detached: false,
          overwrite_conflict: true,
          conflict_files: ["src/app.ts"],
        } satisfies WorkspaceGitSwitchResult;
      }
      currentBranch = p.branch;
      return {
        outcome: "switched",
        branch: p.branch,
        previous_branch: "master",
        stash_message: "moc: auto-stash from master before switching to feat/target",
        created: false,
      } satisfies WorkspaceGitSwitchResult;
    }
    throw new Error(`unexpected method: ${method}`);
  });

  const view = render(<GitBranchPicker cwd="/repo-dirty" variant="header" />);
  fireEvent.click(await view.findByRole("button", { name: /master/ }));
  fireEvent.click(await view.findByRole("option", { name: "feat/target" }));

  const carryBtn = await view.findByRole("button", { name: "Carry changes" });
  fireEvent.click(carryBtn);

  await waitFor(() => {
    expect(view.queryByRole("button", { name: "Carry changes" })).toBeNull();
    expect(view.getByText("src/app.ts")).toBeTruthy();
  });

  fireEvent.click(view.getByRole("button", { name: "Stash & switch" }));

  await waitFor(() => {
    expect(view.getByRole("button", { name: /feat\/target/ })).toBeTruthy();
  });
  expect(switchCalls.map((call) => call.dirty)).toEqual(["check", "carry", "stash"]);
});

it("外部或 Agent 切分支触发失效时，禁用态顶栏也能即时反映新分支", async () => {
  let currentBranch = "master";
  callSpy.mockImplementation(async (method: string) => {
    if (method === "workspace/git_branch") {
      return {
        branch: currentBranch,
        root: "/repo-live",
        head_oid: "999aaaa",
        detached: false,
        unborn: false,
      } satisfies WorkspaceGitBranchResult;
    }
    throw new Error(`unexpected method: ${method}`);
  });

  const view = render(
    <TooltipProvider>
      <GitBranchPicker cwd="/repo-live/sub" variant="header" disabled disabledReason="running" />
    </TooltipProvider>,
  );
  await view.findByRole("button", { name: /master/ });

  // 模拟 Agent 在同仓库根目录切到新分支并触发失效
  currentBranch = "agent/fix-1";
  invalidateGitBranch("/repo-live");

  await waitFor(() => {
    expect(view.getByRole("button", { name: /agent\/fix-1/ })).toBeTruthy();
  });
});
