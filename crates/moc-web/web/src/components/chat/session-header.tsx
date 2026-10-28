import {
  ChevronDownIcon,
  CopyIcon,
  ExternalLinkIcon,
  GitForkIcon,
  MoreHorizontalIcon,
  PanelRightCloseIcon,
  PanelRightOpenIcon,
  PencilIcon,
  ShieldCheckIcon,
  Trash2Icon,
  UsersIcon,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Input } from "@/components/ui/input";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { rpc } from "@/protocol/client";
import type { SessionInfo, SessionMode } from "@/protocol/types";
import { useLayout } from "@/store/layout";
import { deleteSession, forkSession, renameSession } from "@/store/lifecycle";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";

interface SessionHeaderProps {
  sessionId: string;
  activeTab?: "chat" | "trajectory";
  onTabChange?: (tab: "chat" | "trajectory") => void;
}

const MODES: { id: SessionMode; label: string; desc: string }[] = [
  { id: "normal", label: "标准模式", desc: "通用交互与自动工具调用" },
  { id: "plan", label: "计划模式", desc: "分步规划，并在执行前提交审核" },
  { id: "ask", label: "问答模式", desc: "只解答疑问，不直接执行写入操作" },
  { id: "ptc", label: "PTC 模式", desc: "程序化思考与代码执行模式" },
];

export function SessionHeader({ sessionId, activeTab = "chat", onTabChange }: SessionHeaderProps) {
  const view = useTranscript((state) => state.bySession[sessionId]);
  const { rightbarOpen, toggleRightbar, openRightTab } = useLayout();

  const [isEditingTitle, setIsEditingTitle] = useState(false);
  const [titleValue, setTitleValue] = useState("");

  if (!view) return null;

  const title = view.info.title || "未命名会话";
  const currentMode = view.info.mode || "normal";
  const currentModeItem = MODES.find((m) => m.id === currentMode) ?? MODES[0];

  const handleStartRename = () => {
    setTitleValue(title);
    setIsEditingTitle(true);
  };

  const handleCommitRename = async () => {
    setIsEditingTitle(false);
    const trimmed = titleValue.trim();
    if (trimmed && trimmed !== title && sessionId !== "draft-session") {
      await renameSession(sessionId, trimmed);
    }
  };

  const handleSelectMode = async (selectedMode: SessionMode) => {
    if (sessionId === "draft-session") {
      useTranscript.getState().mutate(sessionId, (v) => {
        v.info.mode = selectedMode;
      });
      return;
    }
    try {
      const info = await rpc.call<SessionInfo>("session/set_mode", {
        session_id: sessionId,
        mode: selectedMode,
      });
      useSessions.getState().upsert(info);
      useTranscript.getState().mutate(sessionId, (session) => {
        session.info = info;
      });
      toast.success(`已切换模式至: ${MODES.find((m) => m.id === selectedMode)?.label}`);
    } catch (err) {
      toast.error(`切换模式失败: ${err instanceof Error ? err.message : String(err)}`);
    }
  };

  const handleFork = () => {
    void forkSession(sessionId);
  };

  const handleCopyId = () => {
    void navigator.clipboard.writeText(sessionId);
    toast.success("已复制会话 ID");
  };

  const handleDelete = async () => {
    await deleteSession(sessionId);
    toast.success("会话已删除");
  };

  const handleOpenExternal = () => {
    window.open(window.location.href, "_blank");
  };

  const handleOpenGuide = () => {
    if (!rightbarOpen) {
      toggleRightbar();
    }
    openRightTab({ kind: "guide", title: "🧭 开始" });
  };

  return (
    <div className="flex flex-col border-b border-border bg-background select-none z-10 shrink-0">
      {/* 1. 顶部 Header 栏 (对齐 Image 1 / 2) */}
      <header className="flex h-11 items-center justify-between px-3.5">
        {/* 左侧：标题 + 智能体团队预设 + 模式选择 */}
        <div className="flex min-w-0 items-center gap-2">
          {/* 会话标题 */}
          {isEditingTitle ? (
            <Input
              autoFocus
              value={titleValue}
              onChange={(e) => setTitleValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void handleCommitRename();
                if (e.key === "Escape") setIsEditingTitle(false);
              }}
              onBlur={() => void handleCommitRename()}
              className="h-7 w-48 text-xs font-medium"
            />
          ) : (
            <button
              type="button"
              onClick={handleStartRename}
              className="group flex items-center gap-1.5 rounded-md px-1.5 py-1 text-xs font-semibold text-foreground hover:bg-muted/60 transition-colors"
              title="点击重命名会话"
            >
              <span className="truncate max-w-[240px] sm:max-w-[320px]">{title}</span>
              <PencilIcon className="size-3 text-muted-foreground opacity-0 group-hover:opacity-100 transition-opacity" />
            </button>
          )}

          {/* 👥 智能体团队 */}
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                className="flex items-center gap-1.5 rounded-md bg-muted/50 px-2 py-1 text-[11px] font-medium text-muted-foreground hover:bg-muted hover:text-foreground transition-colors"
              >
                <UsersIcon className="size-3 text-primary" />
                <span>智能体团队</span>
                <ChevronDownIcon className="size-2.5 opacity-60" />
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="w-48 text-xs">
              <DropdownMenuLabel className="text-[11px] font-medium text-muted-foreground">
                智能体预设配置
              </DropdownMenuLabel>
              <DropdownMenuItem className="gap-2 font-medium">
                <UsersIcon className="size-3.5 text-primary" />
                <span className="flex-1">智能体团队 (协同)</span>
                <span className="text-[10px] text-muted-foreground">默认</span>
              </DropdownMenuItem>
              <DropdownMenuItem className="gap-2">
                <span>独立智能体 (单代理)</span>
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>

          {/* 🛡 模式选择 (标准模式 ⌵) */}
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                className="flex items-center gap-1.5 rounded-md px-2 py-1 text-[11px] font-medium text-muted-foreground hover:bg-muted hover:text-foreground transition-colors"
              >
                <ShieldCheckIcon className="size-3 text-emerald-500" />
                <span>{currentModeItem.label}</span>
                <ChevronDownIcon className="size-2.5 opacity-60" />
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="w-52 text-xs">
              <DropdownMenuLabel className="text-[11px] font-medium text-muted-foreground">
                选择运行模式
              </DropdownMenuLabel>
              {MODES.map((item) => (
                <DropdownMenuItem
                  key={item.id}
                  onClick={() => void handleSelectMode(item.id)}
                  className="flex flex-col items-start py-1.5"
                >
                  <div className="flex items-center justify-between w-full">
                    <span className="font-medium">{item.label}</span>
                    {currentMode === item.id && (
                      <span className="size-1.5 rounded-full bg-primary" />
                    )}
                  </div>
                  <span className="text-[10px] text-muted-foreground">{item.desc}</span>
                </DropdownMenuItem>
              ))}
            </DropdownMenuContent>
          </DropdownMenu>
        </div>

        {/* 右侧：弹出在新窗口 ↗、更多操作 ···、右侧栏开关 */}
        <div className="flex items-center gap-1">
          {/* ↗ 在新窗口打开 */}
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                onClick={handleOpenExternal}
                className="flex size-7 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground transition-colors"
                aria-label="在新标签页或独立窗口打开"
              >
                <ExternalLinkIcon className="size-3.5" />
              </button>
            </TooltipTrigger>
            <TooltipContent side="bottom">在新窗口打开</TooltipContent>
          </Tooltip>

          {/* ··· 更多操作 */}
          <DropdownMenu>
            <Tooltip>
              <TooltipTrigger asChild>
                <DropdownMenuTrigger asChild>
                  <button
                    type="button"
                    className="flex size-7 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground transition-colors"
                    aria-label="更多会话操作"
                  >
                    <MoreHorizontalIcon className="size-3.5" />
                  </button>
                </DropdownMenuTrigger>
              </TooltipTrigger>
              <TooltipContent side="bottom">更多操作</TooltipContent>
            </Tooltip>
            <DropdownMenuContent align="end" className="w-40 text-xs">
              <DropdownMenuItem onClick={handleStartRename} className="gap-2">
                <PencilIcon className="size-3.5" />
                <span>重命名</span>
              </DropdownMenuItem>
              <DropdownMenuItem onClick={handleFork} className="gap-2">
                <GitForkIcon className="size-3.5" />
                <span>分叉会话</span>
              </DropdownMenuItem>
              <DropdownMenuItem onClick={handleCopyId} className="gap-2">
                <CopyIcon className="size-3.5" />
                <span>复制会话 ID</span>
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                onClick={() => void handleDelete()}
                className="gap-2 text-destructive focus:text-destructive"
              >
                <Trash2Icon className="size-3.5" />
                <span>删除此会话</span>
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>

          {/* 右侧面板开关 (指南/文件/终端) */}
          <Tooltip>
            <TooltipTrigger asChild>
              <button
                type="button"
                onClick={handleOpenGuide}
                className={`flex size-7 items-center justify-center rounded-md transition-colors ${
                  rightbarOpen
                    ? "bg-muted text-foreground"
                    : "text-muted-foreground hover:bg-muted hover:text-foreground"
                }`}
                aria-label="切换右侧扩展面板"
              >
                {rightbarOpen ? (
                  <PanelRightCloseIcon className="size-3.5" />
                ) : (
                  <PanelRightOpenIcon className="size-3.5" />
                )}
              </button>
            </TooltipTrigger>
            <TooltipContent side="bottom">
              {rightbarOpen ? "折叠辅助面板" : "打开辅助面板 (开始/文件/终端)"}
            </TooltipContent>
          </Tooltip>
        </div>
      </header>

      {/* 2. 二级视图切换栏：对话 | 轨迹 (对齐 Image 1 / 2) */}
      <div className="flex h-7.5 items-center px-3 gap-4 border-t border-border/40 text-xs">
        <button
          type="button"
          onClick={() => onTabChange?.("chat")}
          className={`relative h-full flex items-center font-medium transition-colors ${
            activeTab === "chat"
              ? "text-foreground after:absolute after:bottom-0 after:left-0 after:right-0 after:h-0.5 after:bg-primary"
              : "text-muted-foreground hover:text-foreground"
          }`}
        >
          对话
        </button>

        <button
          type="button"
          onClick={() => onTabChange?.("trajectory")}
          className={`relative h-full flex items-center font-medium transition-colors ${
            activeTab === "trajectory"
              ? "text-foreground after:absolute after:bottom-0 after:left-0 after:right-0 after:h-0.5 after:bg-primary"
              : "text-muted-foreground hover:text-foreground"
          }`}
        >
          轨迹
        </button>
      </div>
    </div>
  );
}
