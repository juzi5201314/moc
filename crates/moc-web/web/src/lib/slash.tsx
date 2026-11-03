// Composer 斜杠命令：内置命令元数据来自后端注册表（/commands.json，
// 存 useConnection.commands），插件命令经 plugin/list 并入（同名时内置优先）。
// 命令只在整条输入以 `/名字` 开头时生效；不认识的 `/xxx` 按普通提示发送。

import { toast } from "sonner";
import { AUTONOMY } from "@/lib/autonomy";
import { rpc } from "@/protocol/client";
import type {
  CommandMeta,
  GoalAction,
  PluginRunCommandResult,
  SessionGoalResult,
} from "@/protocol/types";
import { retrySession, sendPrompt } from "@/store/actions";
import { useBtw } from "@/store/btw";
import { toInput, useComposer } from "@/store/composer";
import { useLayout } from "@/store/layout";
import {
  compactSession,
  createQuickSession,
  setSessionAutonomy,
  togglePlanMode,
} from "@/store/lifecycle";
import { useTranscript } from "@/store/transcript";
import { openSettings, useUi } from "@/store/ui";
import { useWorkflow, workflowActivate, workflowRuns } from "@/store/workflow";

export type SlashCommand = {
  name: string;
  description: string;
  /** 需要参数时的用法提示；有值时选中只补全命令，不立即执行。 */
  usage?: string;
  /** 主名之外的触发名；命中别名时仍按主名分派。 */
  aliases?: string[];
  /** 分组的机器键（session/mode/context/agents/plugins/ui）。 */
  group?: string;
  /** 依赖的特性键；`features[flag] === false` 时不出补全、执行被拒。 */
  flag?: string;
  /** 草稿会话（core 里还不存在）不可用。 */
  needsSession?: boolean;
  /** 生成中可执行；false 时运行中给出提示。 */
  availableDuringTask?: boolean;
  plugin?: string;
};

/** 后端 `CommandMeta` → 前端命令项。 */
export function fromMeta(meta: CommandMeta): SlashCommand {
  return {
    name: meta.name,
    description: meta.description,
    usage: meta.usage || undefined,
    aliases: meta.aliases,
    group: meta.group,
    flag: meta.flag,
    needsSession: meta.needsSession,
    availableDuringTask: meta.availableDuringTask,
  };
}

/** 拆出 `/name args`；不是斜杠开头返回 null。 */
export function parseSlash(text: string): { name: string; args: string } | null {
  const match = /^\/([\w-]+)(?:\s+([\s\S]*))?$/.exec(text.trim());
  return match ? { name: match[1], args: (match[2] ?? "").trim() } : null;
}

/** flag 关闭的命令（`features` 未加载时视为开启）。 */
function flagDisabled(command: SlashCommand, features?: Record<string, boolean>): boolean {
  return command.flag !== undefined && features?.[command.flag] === false;
}

/** 补全候选：只在输入还是单个 `/token`（未输入空格）时出现；flag 关闭的不提示。 */
export function slashMatches(
  text: string,
  commands: SlashCommand[],
  features?: Record<string, boolean>,
): SlashCommand[] {
  if (!/^\/[\w-]*$/.test(text)) return [];
  const query = text.slice(1).toLowerCase();
  return commands.filter(
    (command) =>
      !flagDisabled(command, features) &&
      (command.name.toLowerCase().startsWith(query) ||
        command.aliases?.some((alias) => alias.toLowerCase().startsWith(query))),
  );
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * 执行一条斜杠命令；返回 false 表示不是已知命令（调用方按普通提示发送）。
 * 草稿会话里需要真实会话的命令给出提示而不是静默失败。
 */
export function runSlash(
  sessionId: string,
  text: string,
  commands: SlashCommand[],
  features?: Record<string, boolean>,
): boolean {
  const parsed = parseSlash(text);
  if (!parsed) return false;
  const command = commands.find(
    (item) => item.name === parsed.name || item.aliases?.includes(parsed.name),
  );
  if (!command) return false;
  const { args } = parsed;
  const view = useTranscript.getState().bySession[sessionId];
  const composer = useComposer.getState();

  if (flagDisabled(command, features)) {
    toast(`/${command.name} 依赖的特性 ${command.flag} 已禁用`);
    return true;
  }
  if (command.needsSession && sessionId === "draft-session") {
    toast(`/${command.name} 需要先发送第一条消息`);
    return true;
  }
  if (command.availableDuringTask === false && view?.running) {
    toast(`正在运行中，暂时不能执行 /${command.name}`);
    return true;
  }
  if (!command.plugin && command.usage?.startsWith("<") && !args) {
    toast(`用法：/${command.name} ${command.usage}`);
    return true;
  }
  composer.clearDraft(sessionId);

  if (command.plugin) {
    void rpc
      .call<PluginRunCommandResult>("plugin/run_command", {
        session_id: sessionId,
        name: command.name,
        args,
      })
      .then((result) => {
        const output = result.text.trim();
        toast(`/${command.name}`, {
          description: output ? (
            <pre className="max-h-60 overflow-auto whitespace-pre-wrap font-sans">{output}</pre>
          ) : (
            "已执行"
          ),
          duration: output ? 12000 : 3000,
        });
      })
      .catch((error) => toast.error(`/${command.name} 执行失败：${errorText(error)}`));
    return true;
  }

  switch (command.name) {
    case "new":
      createQuickSession(view?.info.cwd);
      break;
    case "plan": {
      const active = args ? true : !view?.info.plan_active;
      void togglePlanMode(sessionId, active)
        .then(() => {
          if (args) void sendPrompt(sessionId, toInput(args, []));
          else toast(active ? "已进入计划状态" : "已退出计划状态");
        })
        .catch((error) => toast.error(`切换计划状态失败：${errorText(error)}`));
      break;
    }
    case "autonomy": {
      const index = AUTONOMY.findIndex((item) => item.id === view?.info.autonomy);
      const next = args
        ? AUTONOMY.find((item) => item.id === args)
        : AUTONOMY[(index + 1) % AUTONOMY.length];
      if (!next) {
        toast(`用法：/autonomy ${command.usage}`);
        break;
      }
      void setSessionAutonomy(sessionId, next.id)
        .then(() => toast(`自主程度：${next.label}`, { description: next.description }))
        .catch((error) => toast.error(`切换自主程度失败：${errorText(error)}`));
      break;
    }
    case "ask":
      if (view?.running) {
        composer.pushPending(sessionId, {
          key: crypto.randomUUID(),
          kind: "queue",
          text: args,
          images: [],
          mode: "ask",
        });
      } else {
        void sendPrompt(sessionId, toInput(args, []), { mode: "ask" });
      }
      break;
    case "retry":
      void retrySession(sessionId);
      break;
    case "compact":
      void compactSession(sessionId)
        .then((result) =>
          toast.success(
            `上下文已压缩：${result.tokens_before.toLocaleString()} → ${result.tokens_after.toLocaleString()} tokens`,
          ),
        )
        .catch((error) => toast.error(`压缩失败：${errorText(error)}`));
      break;
    case "model":
      useUi.getState().set({ modelPicker: sessionId });
      break;
    case "goal": {
      // 与 cli/tui 同一套解析：空=查看，edit <objective> 改原文，其余整段当新目标建。
      const rest = args.trim();
      const action = ((): GoalAction | null => {
        if (rest === "") return { action: "show" };
        if (rest === "clear") return { action: "clear" };
        if (rest === "pause") return { action: "pause" };
        if (rest === "resume") return { action: "resume" };
        if (rest === "edit") return null;
        const edit = rest.match(/^edit\s+(.+)$/s);
        if (edit) return { action: "edit", objective: edit[1].trim() };
        return { action: "create", objective: rest };
      })();
      if (!action) {
        toast("用法：/goal edit <objective>");
        break;
      }
      void rpc
        .call<SessionGoalResult>("session/goal", {
          session_id: sessionId,
          action,
        })
        .then((result) => {
          const goal = result.goal;
          if (!goal) {
            toast("当前没有目标；/goal <objective> 创建");
            return;
          }
          const phase =
            goal.phase === "active"
              ? "进行中"
              : goal.phase === "paused"
                ? "已暂停"
                : goal.phase === "blocked"
                  ? "已阻塞"
                  : "已完成";
          toast(`目标：${goal.objective}`, {
            description: `${phase} · 已跑 ${goal.rounds_started} 轮 · ${goal.armed ? "自动续跑" : "续跑已停"}`,
          });
          useTranscript.getState().mutate(sessionId, (view) => {
            view.goal = goal;
          });
        })
        .catch((error) => toast.error(`目标操作失败：${errorText(error)}`));
      break;
    }
    case "btw":
      useLayout.getState().openInspector("btw");
      if (!useBtw.getState().ask(sessionId, args)) toast("上一个旁路问题还在回答中");
      break;
    case "context":
      useLayout.getState().openInspector("context");
      break;
    case "usage":
      useLayout.getState().openInspector("usage");
      break;
    case "workflow": {
      // 激活后有指令就作为提示发送；失败（无审批能力/已禁用）显示 toast。
      void workflowActivate(sessionId)
        .then(() => {
          if (args) void sendPrompt(sessionId, toInput(args, []));
          else toast("workflow 已激活");
        })
        .catch((error) => toast.error(`workflow 激活失败：${errorText(error)}`));
      break;
    }
    case "workflows": {
      useWorkflow.setState({ runsOpen: true });
      void workflowRuns(sessionId)
        .then((runs) => useWorkflow.setState({ runList: runs }))
        .catch((error) => toast.error(`获取 run 列表失败：${errorText(error)}`));
      break;
    }
    case "jobs":
      useUi.getState().set({ jobsSession: sessionId });
      break;
    case "settings":
      openSettings();
      break;
  }
  return true;
}
