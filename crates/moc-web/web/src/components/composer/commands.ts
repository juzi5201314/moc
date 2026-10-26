// 斜杠命令：内置命令 + `plugin/list` 返回的插件命令（重名时内置优先）。
// 执行结果统一以状态块或 toast 呈现；需要浮层的命令只改 ui store。

import { toast } from "sonner";
import { rpc } from "@/protocol/client";
import { RpcError } from "@/protocol/errors";
import type {
  CompactResult,
  PluginInfo,
  PluginListResult,
  SessionInfo,
  SessionMode,
} from "@/protocol/types";
import { sendPrompt } from "@/store/actions";
import { useBtw } from "@/store/btw";
import { closeSession, renameSession } from "@/store/lifecycle";
import { usePrefs } from "@/store/prefs";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { useUi } from "@/store/ui";

export type CommandSpec = { name: string; usage: string; description: string; plugin?: string };

export const BUILTIN_COMMANDS: CommandSpec[] = [
  { name: "new", usage: "/new", description: "新建会话" },
  { name: "model", usage: "/model", description: "切换模型与思考等级" },
  { name: "mode", usage: "/mode <normal|ask|plan|ptc>", description: "切换会话模式" },
  { name: "ask", usage: "/ask <文本>", description: "以问答模式发送一次" },
  { name: "compact", usage: "/compact [说明]", description: "压缩上下文" },
  { name: "usage", usage: "/usage", description: "查看用量" },
  { name: "context", usage: "/context", description: "查看上下文占用" },
  { name: "plugins", usage: "/plugins [reload]", description: "查看或重载插件" },
  { name: "config", usage: "/config", description: "打开设置" },
  { name: "btw", usage: "/btw <问题>", description: "旁路提问，不影响主任务" },
  { name: "rename", usage: "/rename <标题>", description: "重命名会话" },
  { name: "close", usage: "/close", description: "关闭当前标签" },
  { name: "confirm-plan", usage: "/confirm-plan", description: "审阅待确认的计划" },
  {
    name: "clm-experimental",
    usage: "/clm-experimental",
    description: "启用 CLM 模式（不可撤销）",
  },
  { name: "agents", usage: "/agents", description: "查看子代理" },
];

const BUILTIN_NAMES: Record<string, true> = Object.fromEntries(
  BUILTIN_COMMANDS.map((command) => [command.name, true]),
);

const SWITCHABLE_MODES: Record<string, SessionMode> = {
  normal: "normal",
  ask: "ask",
  plan: "plan",
  ptc: "ptc",
};

/** 插件命令（去掉与内置重名的）。 */
export function pluginCommands(plugins: PluginInfo[]): CommandSpec[] {
  return plugins.flatMap((plugin) =>
    plugin.commands
      .filter((command) => !Object.hasOwn(BUILTIN_NAMES, command.name))
      .map((command) => ({
        name: command.name,
        usage: command.usage || `/${command.name}`,
        description: command.description,
        plugin: plugin.name,
      })),
  );
}

export async function listPlugins(sessionId: string): Promise<PluginInfo[]> {
  const result = await rpc.call<PluginListResult>("plugin/list", { session_id: sessionId });
  return result.plugins;
}

function errorText(error: unknown): string {
  return error instanceof RpcError ? error.message : String(error);
}

export async function setMode(sessionId: string, mode: SessionMode): Promise<void> {
  const info = await rpc.call<SessionInfo>("session/set_mode", { session_id: sessionId, mode });
  useTranscript.getState().mutate(sessionId, (view) => {
    view.info = info;
  });
  useSessions.getState().upsert(info);
}

export async function compact(sessionId: string, instructions?: string): Promise<void> {
  const result = await rpc.call<CompactResult>("session/compact", {
    session_id: sessionId,
    ...(instructions ? { instructions } : {}),
  });
  toast.success(`已压缩：${result.tokens_before} → ${result.tokens_after} tokens`);
}

export async function reloadPlugins(sessionId: string): Promise<void> {
  await rpc.call("plugin/reload", { session_id: sessionId });
  useTranscript.getState().mutate(sessionId, (view) => {
    view.pluginNotice = null;
  });
  toast.success("插件已重载");
}

/**
 * 执行斜杠命令。返回 false 表示命令未知（调用方保留输入并提示）。
 * `confirmClm` 由 composer 注入（需要确认对话框）。
 */
export async function runSlash(
  sessionId: string,
  name: string,
  args: string,
  hooks: { confirmClm: () => void; plugins: CommandSpec[] },
): Promise<boolean> {
  const ui = useUi.getState();
  const local = useTranscript.getState().localStatus;
  try {
    switch (name) {
      case "new": {
        const cwd = useTranscript.getState().bySession[sessionId]?.info.cwd;
        ui.set({ newSession: { open: true, cwd } });
        return true;
      }
      case "model":
        ui.set({ modelPicker: sessionId });
        return true;
      case "mode": {
        const mode = SWITCHABLE_MODES[args];
        if (!mode) {
          toast.error("用法：/mode <normal|ask|plan|ptc>");
          return true;
        }
        await setMode(sessionId, mode);
        return true;
      }
      case "ask":
        if (!args) {
          toast.error("用法：/ask <文本>");
          return true;
        }
        await sendPrompt(sessionId, [{ type: "text", text: args }], { mode: "ask" });
        return true;
      case "compact":
        await compact(sessionId, args || undefined);
        return true;
      case "usage":
        ui.set({ usage: sessionId });
        return true;
      case "context":
        usePrefs.getState().setRightPanel({ open: true, tab: "context" });
        return true;
      case "plugins":
        if (args === "reload") await reloadPlugins(sessionId);
        else ui.set({ plugins: sessionId });
        return true;
      case "config":
        ui.set({ settings: { open: true } });
        return true;
      case "btw":
        usePrefs.getState().setRightPanel({ open: true, tab: "btw" });
        if (args) {
          if (!useBtw.getState().ask(sessionId, args)) toast.error("已有一条旁路请求在进行");
        }
        return true;
      case "rename":
        if (!args) {
          const info = useTranscript.getState().bySession[sessionId]?.info;
          if (info) ui.set({ rename: info });
          return true;
        }
        await renameSession(sessionId, args);
        return true;
      case "close":
        await closeSession(sessionId);
        return true;
      case "confirm-plan": {
        const plan = useTranscript
          .getState()
          .bySession[sessionId]?.plans.findLast((item) => item.status === "submitted");
        if (plan) ui.set({ planReview: { sessionId, planId: plan.plan_id } });
        else toast("没有待确认的计划");
        return true;
      }
      case "clm-experimental":
        hooks.confirmClm();
        return true;
      case "agents":
        usePrefs.getState().setRightPanel({ open: true, tab: "agents" });
        return true;
      default: {
        const plugin = hooks.plugins.find((command) => command.name === name);
        if (!plugin) return false;
        const result = await rpc.call<{ text: string }>("plugin/run_command", {
          session_id: sessionId,
          name,
          args,
        });
        local(sessionId, "info", result.text || `/${name} 已执行`);
        return true;
      }
    }
  } catch (error) {
    local(sessionId, "error", `/${name} 失败：${errorText(error)}`);
    return true;
  }
}
