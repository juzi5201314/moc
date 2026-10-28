import {
  ArrowLeftIcon,
  CheckCircle2Icon,
  ChevronRightIcon,
  CompassIcon,
  DatabaseIcon,
  FileDiffIcon,
  FolderIcon,
  Loader2Icon,
  Maximize2Icon,
  Minimize2Icon,
  RefreshCwIcon,
  SearchIcon,
  SendIcon,
  SquareIcon,
  TerminalIcon,
  XIcon,
} from "lucide-react";
import {
  type FormEvent,
  type ReactNode,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { DiffFile } from "@/components/panels/diff-file";
import { PlanReview } from "@/components/plan/plan-review";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { rpc } from "@/protocol/client";
import type {
  AgentInfo,
  AgentListResult,
  BashResult,
  ContextContentResult,
  FsListDirResult,
  PlanAction,
  PlanInfo,
  SessionPlanApproveResult,
  SessionUsageResult,
  ToolFileDiff,
} from "@/protocol/types";
import { type RightTab, useLayout } from "@/store/layout";
import { useTranscript } from "@/store/transcript";

interface InspectorPanelProps {
  sessionId: string;
}

type BashRun = { id: number; command: string; result: BashResult };
type AgentSpawnResult = { agent: AgentInfo };
type AgentStopResult = { stopped: boolean; status: AgentInfo["status"] };

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isToolFileDiff(value: unknown): value is ToolFileDiff {
  if (typeof value !== "object" || value === null) return false;
  const candidate = value as Partial<ToolFileDiff>;
  return typeof candidate.path === "string" && Array.isArray(candidate.hunks);
}

function diffFilesFrom(value: unknown): ToolFileDiff[] {
  if (isToolFileDiff(value)) return [value];
  if (typeof value !== "object" || value === null) return [];
  const record = value as Record<string, unknown>;
  if (record.diff !== undefined) return diffFilesFrom(record.diff);
  if (!Array.isArray(record.files)) return [];
  return record.files.filter(isToolFileDiff);
}

function formatDuration(milliseconds: number): string {
  if (milliseconds < 1000) return `${milliseconds} 毫秒`;
  if (milliseconds < 60_000) return `${(milliseconds / 1000).toFixed(1)} 秒`;
  return `${(milliseconds / 60_000).toFixed(1)} 分钟`;
}

function formatTokens(value: number | undefined): string {
  return (value ?? 0).toLocaleString();
}

function IconAction({
  label,
  onClick,
  children,
  disabled = false,
}: {
  label: string;
  onClick: () => void;
  children: ReactNode;
  disabled?: boolean;
}) {
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          onClick={onClick}
          disabled={disabled}
          aria-label={label}
          className="flex size-7 shrink-0 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
        >
          {children}
        </button>
      </TooltipTrigger>
      <TooltipContent side="bottom">{label}</TooltipContent>
    </Tooltip>
  );
}

export function InspectorPanel({ sessionId }: InspectorPanelProps) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const {
    openRightTabs,
    activeRightTabId,
    openRightTab,
    toggleRightbar,
    rightbarFullscreen,
    toggleRightbarFullscreen,
  } = useLayout();
  const activeTab = openRightTabs.find((tab) => tab.id === activeRightTabId) ?? openRightTabs[0];
  const tabKind = activeTab?.kind;
  const activeTabKey = activeTab?.id;
  const cwd = view?.info.cwd ?? "";
  const tabTitle = activeTab?.title || "开始";
  const openInspectorTab = useCallback(
    (kind: RightTab["kind"], title: string) => {
      openRightTab({ kind, title });
    },
    [openRightTab],
  );

  const [directory, setDirectory] = useState({ root: cwd, path: cwd });
  const directoryPath = directory.root === cwd ? directory.path : cwd;
  const [directoryResult, setDirectoryResult] = useState<FsListDirResult | null>(null);
  const [directoryLoading, setDirectoryLoading] = useState(false);
  const [directoryError, setDirectoryError] = useState<string | null>(null);
  const [directorySearch, setDirectorySearch] = useState("");
  const [directoryRefresh, setDirectoryRefresh] = useState(0);

  const [command, setCommand] = useState("");
  const [bashRuns, setBashRuns] = useState<BashRun[]>([]);
  const [bashLoading, setBashLoading] = useState(false);
  const [bashError, setBashError] = useState<string | null>(null);
  const nextBashId = useRef(0);

  const [agents, setAgents] = useState<AgentInfo[]>([]);
  const [agentsLoading, setAgentsLoading] = useState(false);
  const [agentsError, setAgentsError] = useState<string | null>(null);
  const [agentsRefresh, setAgentsRefresh] = useState(0);
  const [agentTask, setAgentTask] = useState("");
  const [agentSpawnLoading, setAgentSpawnLoading] = useState(false);
  const [agentActionId, setAgentActionId] = useState<string | null>(null);

  const [context, setContext] = useState<ContextContentResult | null>(null);
  const [contextLoading, setContextLoading] = useState(false);
  const [contextError, setContextError] = useState<string | null>(null);
  const [contextRefresh, setContextRefresh] = useState(0);

  const [usage, setUsage] = useState<SessionUsageResult | null>(null);
  const [usageLoading, setUsageLoading] = useState(false);
  const [usageError, setUsageError] = useState<string | null>(null);
  const [usageRefresh, setUsageRefresh] = useState(0);

  const [planActionId, setPlanActionId] = useState<string | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);

  useEffect(() => {
    setDirectory({ root: cwd, path: cwd });
    setDirectorySearch("");
  }, [cwd]);

  useEffect(() => {
    void directoryRefresh;
    if (!directoryPath) {
      setDirectoryResult(null);
      setDirectoryError(null);
      setDirectoryLoading(false);
      return;
    }
    let current = true;
    setDirectoryLoading(true);
    setDirectoryError(null);
    setDirectoryResult(null);
    rpc
      .call<FsListDirResult>("fs/list_dir", { path: directoryPath, show_hidden: false })
      .then((result) => {
        if (current) setDirectoryResult(result);
      })
      .catch((error: unknown) => {
        if (current) setDirectoryError(errorMessage(error));
      })
      .finally(() => {
        if (current) setDirectoryLoading(false);
      });
    return () => {
      current = false;
    };
  }, [directoryPath, directoryRefresh]);

  const refreshDirectory = useCallback(() => setDirectoryRefresh((value) => value + 1), []);
  const visibleDirectories = useMemo(() => {
    const query = directorySearch.trim().toLocaleLowerCase();
    const entries = directoryResult?.entries ?? [];
    return query
      ? entries.filter((entry) => entry.name.toLocaleLowerCase().includes(query))
      : entries;
  }, [directoryResult, directorySearch]);

  const runCommand = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const submitted = command.trim();
    if (!submitted || bashLoading) return;
    setBashLoading(true);
    setBashError(null);
    try {
      const result = await rpc.call<BashResult>("session/bash", {
        session_id: sessionId,
        command: { command: submitted, exclude_from_context: false },
      });
      setBashRuns((runs) => [...runs, { id: nextBashId.current++, command: submitted, result }]);
      setCommand("");
    } catch (error) {
      setBashError(errorMessage(error));
    } finally {
      setBashLoading(false);
    }
  };

  useEffect(() => {
    void agentsRefresh;
    if (tabKind !== "subagents") return;
    let current = true;
    setAgentsLoading(true);
    setAgentsError(null);
    rpc
      .call<AgentListResult>("agent/list", { session_id: sessionId })
      .then((result) => {
        if (current) setAgents(result.agents);
      })
      .catch((error: unknown) => {
        if (current) setAgentsError(errorMessage(error));
      })
      .finally(() => {
        if (current) setAgentsLoading(false);
      });
    return () => {
      current = false;
    };
  }, [tabKind, sessionId, agentsRefresh]);

  const visibleAgents = useMemo(() => {
    const byId = new Map(agents.map((agent) => [agent.id, agent]));
    for (const agent of Object.values(view?.agents ?? {})) byId.set(agent.id, agent);
    return [...byId.values()].sort((left, right) => right.updated_at_ms - left.updated_at_ms);
  }, [agents, view?.agents]);

  const spawnAgent = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const task = agentTask.trim();
    if (!task || agentSpawnLoading) return;
    setAgentSpawnLoading(true);
    setAgentsError(null);
    try {
      const result = await rpc.call<AgentSpawnResult>("agent/spawn", {
        session_id: sessionId,
        task,
      });
      setAgents((current) => [
        result.agent,
        ...current.filter((agent) => agent.id !== result.agent.id),
      ]);
      setAgentTask("");
      setAgentsRefresh((value) => value + 1);
    } catch (error) {
      setAgentsError(errorMessage(error));
    } finally {
      setAgentSpawnLoading(false);
    }
  };

  const stopAgent = async (agentId: string) => {
    setAgentActionId(agentId);
    setAgentsError(null);
    try {
      const result = await rpc.call<AgentStopResult>("agent/stop", {
        session_id: sessionId,
        agent_id: agentId,
      });
      setAgents((current) =>
        current.map((agent) =>
          agent.id === agentId ? { ...agent, status: result.status } : agent,
        ),
      );
      setAgentsRefresh((value) => value + 1);
    } catch (error) {
      setAgentsError(errorMessage(error));
    } finally {
      setAgentActionId(null);
    }
  };

  useEffect(() => {
    void activeTabKey;
    void contextRefresh;
    if (tabKind !== "context") return;
    let current = true;
    setContextLoading(true);
    setContextError(null);
    rpc
      .call<ContextContentResult>("session/context_content", {
        session_id: sessionId,
        max_section_bytes: 64_000,
      })
      .then((result) => {
        if (current) setContext(result);
      })
      .catch((error: unknown) => {
        if (current) setContextError(errorMessage(error));
      })
      .finally(() => {
        if (current) setContextLoading(false);
      });
    return () => {
      current = false;
    };
  }, [tabKind, activeTabKey, sessionId, contextRefresh]);

  useEffect(() => {
    void activeTabKey;
    void usageRefresh;
    if (tabKind !== "usage") return;
    let current = true;
    setUsageLoading(true);
    setUsageError(null);
    rpc
      .call<SessionUsageResult>("session/usage", { session_id: sessionId })
      .then((result) => {
        if (current) setUsage(result);
      })
      .catch((error: unknown) => {
        if (current) setUsageError(errorMessage(error));
      })
      .finally(() => {
        if (current) setUsageLoading(false);
      });
    return () => {
      current = false;
    };
  }, [tabKind, activeTabKey, sessionId, usageRefresh]);

  const handlePlanAction = async (plan: PlanInfo, action: PlanAction) => {
    setPlanActionId(plan.plan_id);
    setPlanError(null);
    try {
      await rpc.call<SessionPlanApproveResult>("session/plan_approve", {
        session_id: sessionId,
        plan_id: plan.plan_id,
        action,
      });
      useTranscript.getState().mutate(sessionId, (sessionView) => {
        const current = sessionView.plans.find((item) => item.plan_id === plan.plan_id);
        if (current) current.status = action === "reject" ? "rejected" : "approved";
      });
    } catch (error) {
      setPlanError(errorMessage(error));
    } finally {
      setPlanActionId(null);
    }
  };

  const tabDataFiles = useMemo(() => diffFilesFrom(activeTab?.data), [activeTab?.data]);
  const transcriptFiles = useMemo(
    () => view?.diffs.flatMap((record) => record.diff.files) ?? [],
    [view?.diffs],
  );
  const changedFiles = useMemo(() => {
    const byPath = new Map<string, ToolFileDiff>();
    for (const file of transcriptFiles) byPath.set(file.path, file);
    for (const file of tabDataFiles) byPath.set(file.path, file);
    return [...byPath.values()];
  }, [tabDataFiles, transcriptFiles]);
  const requestedDocPath = activeTab?.path;
  const documentDiff = requestedDocPath
    ? [...tabDataFiles, ...transcriptFiles].find((file) => file.path === requestedDocPath)
    : tabDataFiles[0];
  const tabPlanId =
    typeof activeTab?.data === "string"
      ? activeTab.data
      : typeof activeTab?.data === "object" &&
          activeTab.data !== null &&
          "plan_id" in activeTab.data &&
          typeof activeTab.data.plan_id === "string"
        ? activeTab.data.plan_id
        : undefined;
  const visiblePlans = useMemo<PlanInfo[]>(() => {
    const plans = view?.plans ?? [];
    if (plans.length === 0) return [];
    // 指定了 plan_id 就沿 previous 链跟到最新版本：提交批注后新版本是一个新 plan_id。
    let current: PlanInfo | undefined = tabPlanId
      ? plans.find((plan) => plan.plan_id === tabPlanId)
      : undefined;
    if (!current) {
      const latest = [...plans].sort((left, right) => right.version - left.version)[0];
      return latest ? [latest] : [];
    }
    for (let guard = 0; guard < 16; guard += 1) {
      const parentId: string = current.plan_id;
      const next = plans
        .filter((plan) => plan.previous_plan_id === parentId)
        .sort((left, right) => right.version - left.version)[0];
      if (!next) break;
      current = next;
    }
    return [current];
  }, [view?.plans, tabPlanId]);

  return (
    <div className="flex h-full w-full flex-col bg-[var(--panel)] text-foreground">
      <div className="flex h-11 shrink-0 items-center justify-between px-3.5">
        <div className="flex min-w-0 items-center gap-2 text-[13px] text-[var(--text-tertiary)]">
          {tabKind === "guide" ? (
            <span className="flex items-center gap-1.5">
              <CompassIcon className="size-3.5" />
              <span>开始</span>
            </span>
          ) : (
            <div className="flex min-w-0 items-center gap-1.5">
              <button
                type="button"
                onClick={() => openInspectorTab("guide", "开始")}
                className="shrink-0 text-[13px] text-[var(--text-tertiary)] hover:text-foreground"
              >
                开始
              </button>
              <ChevronRightIcon className="size-3 shrink-0 opacity-60" />
              <span className="truncate text-[13px] font-medium text-foreground">{tabTitle}</span>
            </div>
          )}
        </div>
        <div className="flex items-center gap-1 text-[var(--text-tertiary)]">
          <IconAction
            label={rightbarFullscreen ? "退出全屏" : "全屏最大化"}
            onClick={toggleRightbarFullscreen}
          >
            {rightbarFullscreen ? (
              <Minimize2Icon className="size-3.5" />
            ) : (
              <Maximize2Icon className="size-3.5" />
            )}
          </IconAction>
          <IconAction label="关闭辅助面板" onClick={() => toggleRightbar()}>
            <XIcon className="size-3.5" />
          </IconAction>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto">
        {tabKind === "guide" ? (
          <div className="flex min-h-full flex-col items-center justify-center p-6 text-center">
            <div className="mb-8 text-muted-foreground/25">
              <CompassIcon className="size-14 stroke-[1.25]" />
            </div>
            <div className="flex w-full max-w-[380px] flex-col gap-3.5 text-left">
              <GuideEntry
                icon={<FolderIcon className="size-5 text-warn" />}
                title="工作区目录"
                description="浏览会话工作区中的目录"
                onClick={() => openInspectorTab("files", "工作区目录")}
              />
              <GuideEntry
                icon={<TerminalIcon className="size-5 text-primary" />}
                title="运行命令"
                description="在会话工作区执行 Bash 命令"
                onClick={() => openInspectorTab("terminal", "运行命令")}
              />
              <GuideEntry
                icon={<span className="text-base">◎</span>}
                title="子代理"
                description="查看、启动和停止子代理"
                onClick={() => openInspectorTab("subagents", "子代理")}
              />
              <GuideEntry
                icon={<DatabaseIcon className="size-5" />}
                title="上下文内容"
                description="查看实际发送给模型的上下文分块"
                onClick={() => openInspectorTab("context", "上下文内容")}
              />
              <GuideEntry
                icon={<CheckCircle2Icon className="size-5" />}
                title="模型用量"
                description="查看会话模型 token 与耗时统计"
                onClick={() => openInspectorTab("usage", "模型用量")}
              />
              {visiblePlans.some((plan) => plan.status === "submitted") && (
                <GuideEntry
                  icon={<FileDiffIcon className="size-5 text-primary" />}
                  title="待审计划"
                  description="审批或拒绝当前会话提交的计划"
                  onClick={() => openInspectorTab("plan", "计划审阅")}
                />
              )}
              {changedFiles.length > 0 && (
                <GuideEntry
                  icon={<FileDiffIcon className="size-5 text-primary" />}
                  title="代码变更"
                  description={`查看 ${changedFiles.length} 个变更文件的 diff`}
                  onClick={() => openInspectorTab("changes", "代码变更")}
                />
              )}
            </div>
          </div>
        ) : tabKind === "files" ? (
          <div className="flex h-full min-h-0 flex-col select-text">
            <div className="flex min-h-10 items-center gap-2 border-b border-border/60 px-2.5">
              <IconAction
                label="上级目录"
                disabled={!directoryResult?.parent || directoryLoading}
                onClick={() => {
                  if (directoryResult?.parent)
                    setDirectory({ root: cwd, path: directoryResult.parent });
                }}
              >
                <ArrowLeftIcon className="size-3.5" />
              </IconAction>
              <span
                className="min-w-0 flex-1 truncate font-mono text-[11px] text-muted-foreground"
                title={directoryPath}
              >
                {directoryPath || "没有工作目录"}
              </span>
              <IconAction
                label="刷新目录"
                disabled={!directoryPath || directoryLoading}
                onClick={refreshDirectory}
              >
                {directoryLoading ? (
                  <Loader2Icon className="size-3.5 animate-spin" />
                ) : (
                  <RefreshCwIcon className="size-3.5" />
                )}
              </IconAction>
            </div>
            <label className="flex h-9 shrink-0 items-center gap-2 border-b border-border/60 px-3">
              <SearchIcon className="size-3.5 text-muted-foreground" />
              <input
                type="search"
                value={directorySearch}
                onChange={(event) => setDirectorySearch(event.target.value)}
                placeholder="筛选当前目录..."
                className="min-w-0 flex-1 bg-transparent text-xs outline-none placeholder:text-muted-foreground"
              />
            </label>
            <div className="min-h-0 flex-1 overflow-y-auto p-2 font-mono text-xs">
              {!cwd ? (
                <PanelMessage>当前会话没有工作目录。</PanelMessage>
              ) : directoryLoading ? (
                <PanelMessage>
                  <Loader2Icon className="mr-2 inline size-3.5 animate-spin" />
                  正在读取目录...
                </PanelMessage>
              ) : directoryError ? (
                <PanelError message={directoryError} onRetry={refreshDirectory} />
              ) : visibleDirectories.length > 0 ? (
                <ul className="space-y-0.5">
                  {visibleDirectories.map((entry) => (
                    <li key={entry.path}>
                      <button
                        type="button"
                        onClick={() => setDirectory({ root: cwd, path: entry.path })}
                        className="flex h-8 w-full min-w-0 items-center gap-2 rounded px-2 text-left text-muted-foreground hover:bg-muted/50 hover:text-foreground"
                      >
                        <FolderIcon className="size-3.5 shrink-0 text-amber-500" />
                        <span className="truncate">{entry.name}</span>
                        {entry.git && (
                          <span className="ml-auto shrink-0 text-[10px] text-muted-foreground">
                            git
                          </span>
                        )}
                      </button>
                    </li>
                  ))}
                </ul>
              ) : directorySearch.trim() ? (
                <PanelMessage>当前目录中没有匹配的子目录。</PanelMessage>
              ) : (
                <PanelMessage>当前目录没有可显示的子目录。</PanelMessage>
              )}
              {directoryResult?.truncated && (
                <p className="px-2 pt-3 text-[10px] text-muted-foreground">
                  目录项过多，结果已截断。
                </p>
              )}
            </div>
          </div>
        ) : tabKind === "terminal" ? (
          <div className="flex h-full min-h-0 flex-col bg-zinc-950 p-3 font-mono text-xs text-zinc-200 select-text">
            <div className="mb-2 truncate text-[10px] text-zinc-500">
              工作目录：{cwd || "未设置"}
            </div>
            <div className="min-h-0 flex-1 space-y-3 overflow-y-auto py-2">
              {bashRuns.length === 0 && (
                <div className="text-zinc-500">输入命令后按 Enter 执行。</div>
              )}
              {bashRuns.map((run) => (
                <section key={run.id} className="space-y-1.5">
                  <div className="break-all text-emerald-400">$ {run.command}</div>
                  <pre className="whitespace-pre-wrap break-words text-zinc-200">
                    {run.result.result.content || "（无输出）"}
                  </pre>
                  <div
                    className={
                      run.result.status === "completed" ? "text-zinc-500" : "text-rose-400"
                    }
                  >
                    {run.result.status} · {formatDuration(run.result.duration_ms)}
                  </div>
                </section>
              ))}
              {bashError && (
                <p role="alert" className="whitespace-pre-wrap break-words text-rose-400">
                  {bashError}
                </p>
              )}
            </div>
            <form
              onSubmit={(event) => void runCommand(event)}
              className="mt-2 flex items-center gap-2 border-t border-zinc-800 pt-3"
            >
              <span className="text-emerald-500">$</span>
              <input
                autoComplete="off"
                value={command}
                onChange={(event) => setCommand(event.target.value)}
                placeholder="输入 Bash 命令..."
                disabled={bashLoading}
                className="min-w-0 flex-1 bg-transparent text-xs text-zinc-100 outline-none placeholder:text-zinc-600 disabled:opacity-60"
              />
              <button
                type="submit"
                disabled={!command.trim() || bashLoading}
                aria-label="执行命令"
                className="flex size-7 shrink-0 items-center justify-center rounded-md text-zinc-300 hover:bg-zinc-800 disabled:opacity-40"
              >
                {bashLoading ? (
                  <Loader2Icon className="size-3.5 animate-spin" />
                ) : (
                  <SendIcon className="size-3.5" />
                )}
              </button>
            </form>
          </div>
        ) : tabKind === "subagents" ? (
          <div className="flex min-h-full flex-col gap-3 p-3 text-xs select-text">
            <form
              onSubmit={(event) => void spawnAgent(event)}
              className="space-y-2 border-b border-border/60 pb-3"
            >
              <label
                className="block text-[11px] font-medium text-muted-foreground"
                htmlFor="subagent-task"
              >
                新建子代理
              </label>
              <textarea
                id="subagent-task"
                value={agentTask}
                onChange={(event) => setAgentTask(event.target.value)}
                placeholder="描述要交给子代理的任务..."
                rows={3}
                className="w-full resize-y rounded-md border border-input bg-background px-2.5 py-2 text-xs outline-none focus-visible:ring-1 focus-visible:ring-ring"
              />
              <div className="flex justify-end">
                <button
                  type="submit"
                  disabled={!agentTask.trim() || agentSpawnLoading}
                  className="inline-flex h-7 items-center gap-1.5 rounded-md bg-primary px-2.5 text-[11px] text-primary-foreground disabled:opacity-50"
                >
                  {agentSpawnLoading && <Loader2Icon className="size-3 animate-spin" />}
                  启动子代理
                </button>
              </div>
            </form>
            <div className="flex items-center justify-between">
              <span className="font-medium">子代理列表</span>
              <IconAction label="刷新子代理" onClick={() => setAgentsRefresh((value) => value + 1)}>
                {agentsLoading ? (
                  <Loader2Icon className="size-3.5 animate-spin" />
                ) : (
                  <RefreshCwIcon className="size-3.5" />
                )}
              </IconAction>
            </div>
            {agentsError && (
              <PanelError
                message={agentsError}
                onRetry={() => setAgentsRefresh((value) => value + 1)}
              />
            )}
            {agentsLoading && visibleAgents.length === 0 ? (
              <PanelMessage>
                <Loader2Icon className="mr-2 inline size-3.5 animate-spin" />
                正在读取子代理...
              </PanelMessage>
            ) : visibleAgents.length === 0 ? (
              <PanelMessage>当前会话没有子代理。</PanelMessage>
            ) : (
              <ul className="divide-y divide-border/60">
                {visibleAgents.map((agent) => (
                  <li key={agent.id} className="flex items-start gap-2 py-2.5">
                    <span
                      className={`mt-1 size-2 shrink-0 rounded-full ${
                        agent.status === "running" || agent.status === "queued"
                          ? "bg-amber-500"
                          : "bg-muted-foreground/40"
                      }`}
                    />
                    <div className="min-w-0 flex-1">
                      <div className="flex min-w-0 items-center gap-2">
                        <span className="truncate font-medium">{agent.title || agent.id}</span>
                        <span className="shrink-0 text-[10px] text-muted-foreground">
                          {agent.status}
                        </span>
                      </div>
                      <div className="mt-1 truncate font-mono text-[10px] text-muted-foreground">
                        {agent.model} · {agent.cwd}
                      </div>
                      {agent.last_message && (
                        <p className="mt-1 whitespace-pre-wrap break-words text-muted-foreground">
                          {agent.last_message}
                        </p>
                      )}
                    </div>
                    {(agent.status === "running" || agent.status === "queued") && (
                      <button
                        type="button"
                        onClick={() => void stopAgent(agent.id)}
                        disabled={agentActionId === agent.id}
                        aria-label={`停止 ${agent.title || agent.id}`}
                        className="flex size-7 shrink-0 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground disabled:opacity-50"
                      >
                        {agentActionId === agent.id ? (
                          <Loader2Icon className="size-3.5 animate-spin" />
                        ) : (
                          <SquareIcon className="size-3 fill-current" />
                        )}
                      </button>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </div>
        ) : tabKind === "context" ? (
          <div className="space-y-3 p-3 text-xs select-text">
            <div className="flex items-center justify-between">
              <div>
                <h3 className="font-medium">模型上下文</h3>
                {context && (
                  <p className="mt-1 text-[10px] text-muted-foreground">
                    {formatTokens(context.total_tokens)} / {formatTokens(context.context_window)}{" "}
                    tokens
                  </p>
                )}
              </div>
              <IconAction
                label="刷新上下文"
                disabled={contextLoading}
                onClick={() => setContextRefresh((value) => value + 1)}
              >
                {contextLoading ? (
                  <Loader2Icon className="size-3.5 animate-spin" />
                ) : (
                  <RefreshCwIcon className="size-3.5" />
                )}
              </IconAction>
            </div>
            {contextError ? (
              <PanelError
                message={contextError}
                onRetry={() => setContextRefresh((value) => value + 1)}
              />
            ) : contextLoading && !context ? (
              <PanelMessage>
                <Loader2Icon className="mr-2 inline size-3.5 animate-spin" />
                正在读取上下文...
              </PanelMessage>
            ) : context?.sections.length ? (
              <div className="space-y-2">
                {context.sections.map((section) => (
                  <details
                    key={`${section.name}:${section.tokens}:${section.text}`}
                    className="overflow-hidden rounded-md border border-border/70"
                  >
                    <summary className="flex cursor-pointer list-none items-center justify-between gap-3 bg-muted/30 px-2.5 py-2">
                      <span className="font-medium">{section.name}</span>
                      <span className="shrink-0 font-mono text-[10px] text-muted-foreground">
                        {formatTokens(section.tokens)} tokens
                        {section.truncated_bytes
                          ? ` · 截断 ${formatTokens(section.truncated_bytes)} bytes`
                          : ""}
                      </span>
                    </summary>
                    <pre className="max-h-96 overflow-auto whitespace-pre-wrap break-words p-2.5 font-mono text-[11px] leading-relaxed">
                      {section.text || "（空）"}
                    </pre>
                  </details>
                ))}
              </div>
            ) : context ? (
              <PanelMessage>上下文中没有可显示的分块。</PanelMessage>
            ) : null}
          </div>
        ) : tabKind === "usage" ? (
          <div className="space-y-3 p-3 text-xs select-text">
            <div className="flex items-center justify-between">
              <h3 className="font-medium">模型用量</h3>
              <IconAction
                label="刷新用量"
                disabled={usageLoading}
                onClick={() => setUsageRefresh((value) => value + 1)}
              >
                {usageLoading ? (
                  <Loader2Icon className="size-3.5 animate-spin" />
                ) : (
                  <RefreshCwIcon className="size-3.5" />
                )}
              </IconAction>
            </div>
            {usageError ? (
              <PanelError
                message={usageError}
                onRetry={() => setUsageRefresh((value) => value + 1)}
              />
            ) : usageLoading && !usage ? (
              <PanelMessage>
                <Loader2Icon className="mr-2 inline size-3.5 animate-spin" />
                正在读取用量...
              </PanelMessage>
            ) : usage ? (
              <>
                <UsageSummary summary={usage.total} />
                <div className="flex items-center justify-between border-b border-border/60 pb-2 text-[10px] text-muted-foreground">
                  <span>工具耗时</span>
                  <span className="font-mono">{formatDuration(usage.tool_ms)}</span>
                </div>
                {usage.models.length > 0 && (
                  <div className="space-y-2">
                    <h4 className="text-[11px] font-medium text-muted-foreground">按模型</h4>
                    {usage.models.map((summary) => (
                      <UsageSummary key={summary.model} summary={summary} />
                    ))}
                  </div>
                )}
              </>
            ) : null}
          </div>
        ) : tabKind === "plan" ? (
          <div className="min-h-full select-text">
            {visiblePlans.length === 0 ? (
              <PanelMessage>当前会话没有计划。</PanelMessage>
            ) : (
              visiblePlans.map((plan) => (
                <PlanReview
                  key={plan.plan_id}
                  sessionId={sessionId}
                  plan={plan}
                  busy={planActionId === plan.plan_id}
                  actionError={planError}
                  onAction={(target, action) => void handlePlanAction(target, action)}
                />
              ))
            )}
          </div>
        ) : tabKind === "doc" ? (
          <div className="min-h-full select-text">
            {documentDiff ? (
              <DiffFile file={documentDiff} />
            ) : (
              <PanelMessage>
                {requestedDocPath
                  ? `当前会话没有 ${requestedDocPath} 的变更快照。`
                  : "当前标签没有关联的文件差异。"}
              </PanelMessage>
            )}
          </div>
        ) : tabKind === "changes" ? (
          <div className="min-h-full select-text">
            {changedFiles.length > 0 ? (
              changedFiles.map((file) => <DiffFile key={file.path} file={file} />)
            ) : (
              <PanelMessage>当前会话没有文件变更。</PanelMessage>
            )}
          </div>
        ) : (
          <PanelMessage>此标签没有可显示的内容。</PanelMessage>
        )}
      </div>
    </div>
  );
}

function GuideEntry({
  icon,
  title,
  description,
  onClick,
}: {
  icon: ReactNode;
  title: string;
  description: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="group flex w-full items-center gap-3.5 rounded-[20px] border border-[rgba(0,0,0,0.12)] bg-card px-5 py-3.5 text-left transition-colors hover:bg-muted/50 dark:border-[rgba(255,255,255,0.12)]"
    >
      <span className="flex size-[26px] shrink-0 items-center justify-center text-[var(--text-tertiary)]">
        {icon}
      </span>
      <span className="min-w-0 flex-1">
        <span className="block truncate text-sm text-foreground">{title}</span>
        <span className="mt-0.5 block text-[11px] leading-4 text-[var(--text-tertiary)]">
          {description}
        </span>
      </span>
    </button>
  );
}

function PanelMessage({ children }: { children: ReactNode }) {
  return <p className="px-2 py-8 text-center text-xs text-muted-foreground">{children}</p>;
}

function PanelError({ message, onRetry }: { message: string; onRetry?: () => void }) {
  return (
    <div
      role="alert"
      className="space-y-2 rounded-md border border-destructive/30 bg-destructive/5 p-3 text-xs text-destructive"
    >
      <p className="whitespace-pre-wrap break-words">{message}</p>
      {onRetry && (
        <button
          type="button"
          onClick={onRetry}
          className="inline-flex h-7 items-center gap-1.5 rounded border border-destructive/30 px-2 hover:bg-destructive/10"
        >
          <RefreshCwIcon className="size-3" />
          重试
        </button>
      )}
    </div>
  );
}

function UsageSummary({ summary }: { summary: SessionUsageResult["total"] }) {
  return (
    <section className="rounded-md border border-border/70 p-2.5">
      <header className="mb-2 flex items-center justify-between gap-2">
        <span className="truncate font-medium">
          {summary.model === "total" ? "合计" : summary.model}
        </span>
        <span className="shrink-0 text-[10px] text-muted-foreground">{summary.calls} 次调用</span>
      </header>
      <dl className="grid grid-cols-2 gap-x-3 gap-y-1.5 text-[10px]">
        <UsageMetric label="输入" value={`${formatTokens(summary.input_tokens)} tok`} />
        <UsageMetric label="输出" value={`${formatTokens(summary.output_tokens)} tok`} />
        <UsageMetric label="缓存命中" value={`${formatTokens(summary.cached_input_tokens)} tok`} />
        <UsageMetric
          label="缓存写入"
          value={`${formatTokens(summary.cache_creation_input_tokens)} tok`}
        />
        <UsageMetric label="推理" value={`${formatTokens(summary.reasoning_tokens)} tok`} />
        <UsageMetric label="活跃耗时" value={formatDuration(summary.active_ms)} />
        <UsageMetric
          label="平均首 token"
          value={
            summary.ttft_calls ? formatDuration(summary.ttft_ms / summary.ttft_calls) : "无数据"
          }
        />
        <UsageMetric label="生成耗时" value={formatDuration(summary.gen_ms)} />
      </dl>
    </section>
  );
}

function UsageMetric({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex min-w-0 items-baseline justify-between gap-2">
      <dt className="truncate text-muted-foreground">{label}</dt>
      <dd className="shrink-0 font-mono">{value}</dd>
    </div>
  );
}
