// Composer 斜杠命令：内置命令表 + 当前工作区插件注册的命令（同名时内置优先）。
// 命令只在整条输入以 `/名字` 开头时生效；不认识的 `/xxx` 按普通提示发送。

import { toast } from "sonner";
import { AUTONOMY } from "@/lib/autonomy";
import { rpc } from "@/protocol/client";
import type { PluginRunCommandResult } from "@/protocol/types";
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

export type SlashCommand = {
  name: string;
  description: string;
  /** 需要参数时的用法提示；有值时选中只补全命令，不立即执行。 */
  usage?: string;
  /** 草稿会话（core 里还不存在）不可用。 */
  needsSession?: boolean;
  plugin?: string;
};

export const BUILTIN_COMMANDS: SlashCommand[] = [
  { name: "new", description: "新建会话" },
  { name: "plan", description: "进入或退出计划状态，可附带提示", usage: "[提示]" },
  {
    name: "autonomy",
    description: "切换自主程度，无参数时循环",
    usage: "[default|dont_ask|unattended]",
  },
  { name: "ask", description: "只读问答：本条提示不改动文件", usage: "<问题>" },
  { name: "retry", description: "接着当前上下文重新生成", needsSession: true },
  { name: "compact", description: "压缩当前上下文", needsSession: true },
  { name: "model", description: "切换模型与思考强度" },
  { name: "btw", description: "旁路提问，不进入主对话上下文", usage: "<问题>", needsSession: true },
  { name: "context", description: "查看上下文占用", needsSession: true },
  { name: "usage", description: "查看本会话用量", needsSession: true },
  { name: "settings", description: "打开设置" },
];

/** 拆出 `/name args`；不是斜杠开头返回 null。 */
export function parseSlash(text: string): { name: string; args: string } | null {
  const match = /^\/([\w-]+)(?:\s+([\s\S]*))?$/.exec(text.trim());
  return match ? { name: match[1], args: (match[2] ?? "").trim() } : null;
}

/** 补全候选：只在输入还是单个 `/token`（未输入空格）时出现。 */
export function slashMatches(text: string, commands: SlashCommand[]): SlashCommand[] {
  if (!/^\/[\w-]*$/.test(text)) return [];
  const query = text.slice(1).toLowerCase();
  return commands.filter((command) => command.name.toLowerCase().startsWith(query));
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * 执行一条斜杠命令；返回 false 表示不是已知命令（调用方按普通提示发送）。
 * 草稿会话里需要真实会话的命令给出提示而不是静默失败。
 */
export function runSlash(sessionId: string, text: string, commands: SlashCommand[]): boolean {
  const parsed = parseSlash(text);
  if (!parsed) return false;
  const command = commands.find((item) => item.name === parsed.name);
  if (!command) return false;
  const { args } = parsed;
  const view = useTranscript.getState().bySession[sessionId];
  const composer = useComposer.getState();

  if (command.needsSession && sessionId === "draft-session") {
    toast(`/${command.name} 需要先发送第一条消息`);
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
      if (view?.running) toast("当前正在生成，结束后再重试");
      else void retrySession(sessionId);
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
    case "settings":
      openSettings();
      break;
  }
  return true;
}
