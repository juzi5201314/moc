// Composer 斜杠命令：内置命令元数据来自后端注册表（/commands.json，
// 存 useConnection.commands），插件命令经 plugin/list 并入（同名时内置优先）。
// 命令只在整条输入以 `/名字` 开头时生效；不认识的 `/xxx` 按普通提示发送。

import { toast } from "sonner";
import { AUTONOMY } from "@/lib/autonomy";
import { type MsgKey, t } from "@/lib/i18n";
import { applySessionEngine, cycleSessionEngine } from "@/lib/shortcuts";
import { rpc } from "@/protocol/client";
import type {
  CommandMeta,
  GoalAction,
  PluginRunCommandResult,
  SessionGoalResult,
  SessionPermission,
  SkillsCompactAction,
} from "@/protocol/types";
import { retrySession, sendPrompt } from "@/store/actions";
import { useBtw } from "@/store/btw";
import { toInput, useComposer } from "@/store/composer";
import { openFile } from "@/store/files";
import { useLayout } from "@/store/layout";
import {
  closeSession,
  compactSession,
  compactSkills,
  createQuickSession,
  refreshSideData,
  setSessionAutonomy,
  setSessionPermission,
  togglePlanMode,
} from "@/store/lifecycle";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";
import { openSettings, useUi } from "@/store/ui";
import { useWorkflow, workflowActivate, workflowRuns } from "@/store/workflow";

const PERMISSION_LABEL: Record<SessionPermission, MsgKey> = {
  full: "perm.full",
  workspace: "perm.workspace",
  workspace_only: "perm.workspace_only",
};
const PERMISSION_MODES: SessionPermission[] = ["full", "workspace", "workspace_only"];

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

/** 已知命令首 token 的范围；未知命令保持普通文本。 */
export function commandToken(
  text: string,
  commands: SlashCommand[],
): { start: number; end: number } | null {
  const parsed = parseSlash(text);
  if (
    !parsed ||
    !commands.some((item) => item.name === parsed.name || item.aliases?.includes(parsed.name))
  )
    return null;
  const start = text.length - text.trimStart().length;
  return { start, end: start + parsed.name.length + 1 };
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
  return commands
    .filter(
      (command) =>
        !flagDisabled(command, features) &&
        (command.name.toLowerCase().startsWith(query) ||
          command.aliases?.some((alias) => alias.toLowerCase().startsWith(query))),
    )
    .sort(
      (a, b) => Number(b.name.toLowerCase() === query) - Number(a.name.toLowerCase() === query),
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

  if (useUi.getState().skillsWorking) {
    toast(t("slash.skills_busy"));
    return true;
  }
  if (flagDisabled(command, features)) {
    toast(t("slash.flag_disabled", { name: command.name, flag: command.flag ?? "" }));
    return true;
  }
  if (command.needsSession && sessionId === "draft-session") {
    toast(
      !command.plugin && command.name === "compact"
        ? t("slash.compact_empty")
        : t("slash.needs_session", { name: command.name }),
    );
    return true;
  }
  if (command.availableDuringTask === false && view?.running) {
    toast(t("slash.running", { name: command.name }));
    return true;
  }
  if (!command.plugin && command.usage?.startsWith("<") && !args) {
    toast(t("slash.usage", { cmd: `/${command.name} ${command.usage}` }));
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
            t("slash.done")
          ),
          duration: output ? 12000 : 3000,
        });
      })
      .catch((error) =>
        toast.error(t("slash.exec_failed", { name: command.name, error: errorText(error) })),
      );
    return true;
  }

  switch (command.name) {
    case "debuginfo":
    case "session": {
      if (args) {
        toast(t("slash.usage", { cmd: `/${command.name}` }));
        break;
      }
      const method = command.name === "debuginfo" ? "session/debug_info" : "session/inspect";
      const note = (tone: "info" | "error", text: string) => {
        useTranscript.getState().mutate(sessionId, (current) => {
          current.blocks.push({
            kind: "status",
            id: crypto.randomUUID(),
            tone,
            text,
            localCommand: true,
          });
        });
      };
      void rpc
        .call<string>(method, { session_id: sessionId })
        .then((text) => note("info", text))
        .catch((error) =>
          note("error", t("slash.exec_failed", { name: command.name, error: errorText(error) })),
        );
      break;
    }
    case "compact-skills": {
      const normalized = args.split(/\s+/).filter(Boolean).join(" ");
      const action: SkillsCompactAction | undefined = (
        {
          "": "missing",
          compact: "missing",
          force: "force",
          cleanup: "cleanup",
          "cleanup all": "clear",
        } as Record<string, SkillsCompactAction>
      )[normalized];
      if (!action) {
        toast.error(t("slash.usage", { cmd: "/compact-skills [compact | force | cleanup [all]]" }));
        break;
      }
      // 请求中的结果/错误由行内状态显示；这里只提示启动前被拒绝的情况。
      void compactSkills(sessionId, action).catch((error) => toast.error(errorText(error)));
      break;
    }
    case "new":
      createQuickSession(view?.info.cwd);
      break;
    case "resume":
      useUi.getState().set({ palette: true });
      break;
    case "close":
      // 与标签关闭同一套：运行中先 cancel 等收尾，再 unload 放租约、关标签。
      void closeSession(sessionId).catch((error) =>
        toast.error(t("slash.close_failed", { error: errorText(error) })),
      );
      break;
    case "plan": {
      const active = args ? true : !view?.info.plan_active;
      void togglePlanMode(sessionId, active)
        .then(() => {
          if (args) void sendPrompt(sessionId, toInput(args, []));
          else toast(active ? t("slash.plan_on") : t("slash.plan_off"));
        })
        .catch((error) => toast.error(t("slash.plan_failed", { error: errorText(error) })));
      break;
    }
    case "confirm-plan": {
      const plan = view?.plans.toSorted(
        (left, right) => (right.version ?? 0) - (left.version ?? 0),
      )[0];
      if (!plan) {
        toast(t("slash.no_plan"));
        break;
      }
      openFile(sessionId, { kind: "resource", uri: plan.path });
      break;
    }
    case "autonomy": {
      const index = AUTONOMY.findIndex((item) => item.id === view?.info.autonomy);
      const next = args
        ? AUTONOMY.find((item) => item.id === args)
        : AUTONOMY[(index + 1) % AUTONOMY.length];
      if (!next) {
        toast(t("slash.usage", { cmd: `/autonomy ${command.usage}` }));
        break;
      }
      void setSessionAutonomy(sessionId, next.id)
        .then(() =>
          toast(t("slash.autonomy_set", { label: t(next.label) }), {
            description: t(next.description),
          }),
        )
        .catch((error) => toast.error(t("slash.autonomy_failed", { error: errorText(error) })));
      break;
    }
    case "permission": {
      const rest = args.trim().toLowerCase();
      if (rest === "") {
        const current =
          sessionId === "draft-session"
            ? useSessions.getState().draftSession?.permission
            : view?.info.permission;
        toast(
          t("slash.perm_current", {
            label: current ? t(PERMISSION_LABEL[current]) : t("slash.perm_default"),
          }),
          {
            description: t("slash.usage", { cmd: `/permission ${command.usage}` }),
          },
        );
        break;
      }
      if (rest === "clear") {
        if (sessionId === "draft-session" || !view?.info.permission) {
          toast(t("slash.perm_none"));
          break;
        }
        void setSessionPermission(sessionId, view.info.permission, true)
          .then(() => toast(t("slash.perm_cleared")))
          .catch((error) => toast.error(t("slash.perm_clear_failed", { error: errorText(error) })));
        break;
      }
      const mode = rest === "workspace-only" ? "workspace_only" : rest;
      if (!PERMISSION_MODES.includes(mode as SessionPermission)) {
        toast(t("slash.usage", { cmd: `/permission ${command.usage}` }));
        break;
      }
      void setSessionPermission(sessionId, mode as SessionPermission)
        .then(() =>
          toast(t("slash.perm_set", { label: t(PERMISSION_LABEL[mode as SessionPermission]) })),
        )
        .catch((error) => toast.error(t("slash.perm_failed", { error: errorText(error) })));
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
        .then((result) => {
          if (result.method === "noop") {
            toast(t("slash.compact_empty"));
            return;
          }
          toast.success(
            t("slash.compacted", {
              before: result.tokens_before.toLocaleString(),
              after: result.tokens_after.toLocaleString(),
            }),
          );
        })
        .catch((error) => toast.error(t("slash.compact_failed", { error: errorText(error) })));
      break;
    case "model":
      useUi.getState().set({ modelPicker: sessionId });
      break;
    case "engine":
      cycleSessionEngine(sessionId);
      break;
    case "clm-experimental":
      applySessionEngine(sessionId, "clm_experimental");
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
        toast(t("slash.usage", { cmd: "/goal edit <objective>" }));
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
            toast(t("slash.goal_none"));
            return;
          }
          const phase =
            goal.phase === "active"
              ? "goal.active"
              : goal.phase === "paused"
                ? "goal.paused"
                : goal.phase === "blocked"
                  ? "goal.blocked"
                  : "goal.done";
          toast(t("slash.goal_title", { objective: goal.objective }), {
            description: t("slash.goal_desc", {
              phase: t(phase as MsgKey),
              rounds: goal.rounds_started,
              armed: t(goal.armed ? "goal.armed" : "goal.halted"),
            }),
          });
          useTranscript.getState().mutate(sessionId, (view) => {
            view.goal = goal;
          });
        })
        .catch((error) => toast.error(t("slash.goal_failed", { error: errorText(error) })));
      break;
    }
    case "btw":
      useLayout.getState().openInspector("btw");
      if (!useBtw.getState().ask(sessionId, args)) toast(t("slash.btw_busy"));
      break;
    case "tools":
      useUi.getState().set({ toolsSession: sessionId });
      break;
    case "context":
      useUi.getState().set({ contextDialog: sessionId });
      break;
    case "usage":
      useUi.getState().set({ statusPopover: { sessionId, kind: "usage" } });
      break;
    case "workflow": {
      // 激活后有指令就作为提示发送；失败（无审批能力/已禁用）显示 toast。
      void workflowActivate(sessionId)
        .then(() => {
          if (args) void sendPrompt(sessionId, toInput(args, []));
          else toast(t("slash.wf_active"));
        })
        .catch((error) => toast.error(t("slash.wf_failed", { error: errorText(error) })));
      break;
    }
    case "workflows": {
      useWorkflow.setState({ runsOpen: true });
      void workflowRuns(sessionId)
        .then((runs) => useWorkflow.setState({ runList: runs }))
        .catch((error) => toast.error(t("slash.runs_failed", { error: errorText(error) })));
      break;
    }
    case "agents":
      useUi.getState().set({ statusPopover: { sessionId, kind: "agents" } });
      break;
    case "jobs":
      useUi.getState().set({ jobsSession: sessionId });
      break;
    case "plugins": {
      const rest = args.trim();
      if (rest === "reload") {
        // 草稿会话在 core 里不存在：传空 id，core 按自身工作目录发现插件。
        void rpc
          .call("plugin/reload", {
            session_id: sessionId === "draft-session" ? "" : sessionId,
          })
          .then(() => {
            if (sessionId !== "draft-session") void refreshSideData(sessionId);
            toast.success(t("slash.plugins_reloaded"));
          })
          .catch((error) => toast.error(t("slash.plugins_failed", { error: errorText(error) })));
      } else if (rest === "") {
        openSettings("plugins");
      } else {
        toast(t("slash.usage", { cmd: `/plugins ${command.usage}` }));
      }
      break;
    }
    case "settings":
      openSettings();
      break;
    case "features":
      useUi.getState().set({ features: true });
      break;
    case "help":
      openSettings("shortcuts");
      break;
    case "doctor":
      useUi.getState().set({ doctor: true });
      break;
    case "monitor":
      useUi.getState().set({ monitor: true });
      break;
  }
  return true;
}
