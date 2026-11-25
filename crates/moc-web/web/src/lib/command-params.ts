// 斜杠命令结构化参数引擎：分组排序、上下文门控、动态候选供给、二级菜单条目构建与阶段二参数解析。

import { type MsgKey, t } from "@/lib/i18n";
import type { SlashCommand } from "@/lib/slash";
import type {
  CommandParamKind,
  CommandParamMeta,
  McpServerInfo,
  McpServerPhase,
  SessionAutonomy,
  SessionPermission,
} from "@/protocol/types";

/** flag 关闭的命令（`features` 未加载时视为开启）。 */
export function isCommandFlagDisabled(
  command: SlashCommand,
  features?: Record<string, boolean>,
): boolean {
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
        !isCommandFlagDisabled(command, features) &&
        (command.name.toLowerCase().startsWith(query) ||
          command.aliases?.some((alias) => alias.toLowerCase().startsWith(query))),
    )
    .sort(
      (a, b) => Number(b.name.toLowerCase() === query) - Number(a.name.toLowerCase() === query),
    );
}

/** 规范命令分组顺序（与后端 `CommandGroup` 顺序一致）。 */
const GROUP_ORDER = ["session", "mode", "context", "agents", "plugins", "ui"] as const;

const GROUP_LABEL: Record<string, MsgKey> = {
  session: "cmd.group.session",
  mode: "cmd.group.mode",
  context: "cmd.group.context",
  agents: "cmd.group.agents",
  plugins: "cmd.group.plugins",
  ui: "cmd.group.ui",
};

const SANDBOX_CHOICES: { id: SessionPermission; desc: MsgKey }[] = [
  { id: "full", desc: "perm.full_desc" },
  { id: "workspace", desc: "perm.workspace_desc" },
  { id: "workspace_only", desc: "perm.workspace_only_desc" },
];

export type ChoiceStatusTone = "success" | "warn" | "destructive" | "muted";

export type CommandContext = {
  isDraft: boolean;
  promptRunning: boolean;
  skillsWorking: boolean;
  autonomy?: SessionAutonomy;
  permission?: SessionPermission;
  mcpServers?: McpServerInfo[];
};

export type CommandEntry = {
  command: SlashCommand;
  groupKey: string;
  groupLabel: string;
  /** 仅当本项是所属分组首项且处于未搜索态时为 true。 */
  showGroupHeading: boolean;
  disabledReason: string | null;
  /** `/name` 中命中前缀查询的字符下标（含前导 `/` 偏移）。 */
  titleMatches?: number[];
};

export type CommandTrailing =
  | { kind: "submenu"; currentValue?: string }
  | { kind: "param"; required: boolean };

export type ResolvedChoice = {
  value: string;
  label: string;
  description: string;
  current?: boolean;
  statusTone?: ChoiceStatusTone;
  params?: CommandParamMeta[];
};

export type SubmenuAction = {
  key: string;
  label: string;
  /** 子命令 / 字面值用等宽字，动作文案用正文 sans。 */
  mono?: boolean;
  description: string;
  /** 选中后写入输入框的文本（需继续填文本时含尾随空格，叶子动作不带尾随空格）。 */
  fillText: string;
  /** 选中填充后是否关闭补全菜单（叶子动作 true，继续填后续参数 false）。 */
  closeMenu: boolean;
  /** 为混合命令选择直接输入自由文本时，跳过子命令下拉直接进文本引导。 */
  preferHint?: boolean;
  current?: boolean;
  statusTone?: ChoiceStatusTone;
  nextParam?: { name: string; required: boolean };
  nested?: boolean;
};

export type SubmenuSection = {
  key: string;
  heading?: string;
  items: SubmenuAction[];
};

export type Stage2ChoiceItem = {
  key: string;
  value: string;
  label: string;
  description: string;
  matches: number[];
  fillText: string;
  closeMenu: boolean;
  current?: boolean;
  statusTone?: ChoiceStatusTone;
  nextParam?: { name: string; required: boolean };
};

export type Stage2State =
  | {
      kind: "choices";
      command: SlashCommand;
      param: CommandParamMeta;
      commandPath: string;
      items: Stage2ChoiceItem[];
      acceptsText: boolean;
      ghostPlaceholder?: string;
    }
  | {
      kind: "hint";
      command: SlashCommand;
      commandPath: string;
      param?: CommandParamMeta;
      legacyUsage?: string;
      required: boolean;
      empty: boolean;
      missingRequired: boolean;
      ghostPlaceholder?: string;
      emptyDynamicNotice?: string;
    };

/** 当前上下文下该命令不可用的原因；返回 null 表示可用。 */
export function commandDisabledReason(command: SlashCommand, ctx: CommandContext): string | null {
  if (ctx.skillsWorking) return t("cmd.disabled_skills");
  if (command.needsSession && ctx.isDraft) return t("cmd.disabled_session");
  if (command.availableDuringTask === false && ctx.promptRunning) return t("cmd.disabled_running");
  return null;
}

/** 命令的根参数是否具备可交互的子命令 / 枚举二级菜单。 */
export function hasInteractiveSubmenu(command: SlashCommand): boolean {
  const root = command.params?.[0];
  if (!root) return false;
  return root.kind === "choice" || root.kind === "dynamic";
}

/** 一级菜单行尾极简指示：无参留空、子命令给 `›`（状态类附当前值）、自由文本给 Required/Optional。 */
export function commandTrailing(
  command: SlashCommand,
  ctx: CommandContext,
): CommandTrailing | null {
  const root = command.params?.[0];
  if (root) {
    if (root.kind === "choice" || root.kind === "dynamic") {
      const currentValue =
        !command.plugin && command.name === "autonomy"
          ? (ctx.autonomy ?? "default")
          : !command.plugin && command.name === "permission"
            ? (ctx.permission ?? "full")
            : undefined;
      return { kind: "submenu", currentValue };
    }
    return { kind: "param", required: root.required };
  }
  if (command.usage) {
    return { kind: "param", required: command.usage.startsWith("<") };
  }
  return null;
}

/** 为一级命令列表生成展示条目：空查询 `/` 时按组聚类，输入过滤词后拍平排序并高亮命中字符。 */
export function buildCommandEntries(
  text: string,
  commands: SlashCommand[],
  features: Record<string, boolean> | undefined,
  ctx: CommandContext,
): CommandEntry[] {
  const matched = slashMatches(text, commands, features);
  if (matched.length === 0) return [];
  const query = text.slice(1).toLowerCase();

  if (query.length === 0) {
    const byGroup = new Map<string, SlashCommand[]>();
    for (const command of matched) {
      const key = command.group ?? (command.plugin ? "plugins" : "ui");
      const bucket = byGroup.get(key);
      if (bucket) bucket.push(command);
      else byGroup.set(key, [command]);
    }
    const orderedKeys = [
      ...GROUP_ORDER.filter((key) => byGroup.has(key)),
      ...[...byGroup.keys()].filter((key) => !(GROUP_ORDER as readonly string[]).includes(key)),
    ];
    const entries: CommandEntry[] = [];
    for (const groupKey of orderedKeys) {
      const labelKey = GROUP_LABEL[groupKey];
      const groupLabel = labelKey ? t(labelKey) : groupKey;
      const list = byGroup.get(groupKey) ?? [];
      list.forEach((command, idx) => {
        entries.push({
          command,
          groupKey,
          groupLabel,
          showGroupHeading: idx === 0,
          disabledReason: commandDisabledReason(command, ctx),
        });
      });
    }
    return entries;
  }

  return matched.map((command) => {
    const lower = command.name.toLowerCase();
    const titleMatches = lower.startsWith(query)
      ? Array.from({ length: query.length }, (_, index) => index + 1)
      : undefined;
    const groupKey = command.group ?? (command.plugin ? "plugins" : "ui");
    const labelKey = GROUP_LABEL[groupKey];
    return {
      command,
      groupKey,
      groupLabel: labelKey ? t(labelKey) : groupKey,
      showGroupHeading: false,
      disabledReason: commandDisabledReason(command, ctx),
      titleMatches,
    };
  });
}

/** 列表初始高亮或跳过不可用项：若存在可用项则返回首个可用项下标，否则回退 0 便于查看不可用原因。 */
export function firstEnabledIndex(entries: CommandEntry[]): number {
  const idx = entries.findIndex((entry) => entry.disabledReason === null);
  return Math.max(idx, 0);
}

/** 按方向步进下一个可用命令下标；若全表均不可用则逐项步进。 */
export function stepEnabledIndex(entries: CommandEntry[], current: number, step: 1 | -1): number {
  const total = entries.length;
  if (total === 0) return 0;
  const hasEnabled = entries.some((entry) => entry.disabledReason === null);
  if (!hasEnabled) return (current + step + total) % total;
  let next = current;
  for (let i = 0; i < total; i += 1) {
    next = (next + step + total) % total;
    if (entries[next].disabledReason === null) return next;
  }
  return current;
}

function mcpPhaseTone(phase: McpServerPhase): ChoiceStatusTone {
  if (phase === "connected") return "success";
  if (phase === "connecting" || phase === "reconnecting") return "warn";
  if (phase === "failed") return "destructive";
  return "muted";
}

function mcpServerDescription(server: McpServerInfo): string {
  const base =
    server.phase === "connected"
      ? t("cmd.mcp_connected", { n: server.tool_count ?? 0 })
      : server.phase === "connecting"
        ? t("cmd.mcp_connecting")
        : server.phase === "reconnecting"
          ? t("cmd.mcp_reconnecting")
          : server.phase === "failed"
            ? t("cmd.mcp_failed")
            : t("cmd.mcp_disabled");
  if (server.phase === "failed" && server.error) return `${base} · ${server.error}`;
  return server.target ? `${base} · ${server.target}` : base;
}

/** 展开参数的所有静态 + 动态候选，并附加当前生效状态与运行态色阶。 */
export function resolveParamChoices(
  command: SlashCommand,
  param: CommandParamMeta,
  ctx: CommandContext,
): ResolvedChoice[] {
  const result: ResolvedChoice[] = [];
  if (param.dynamic === "sandbox_mode") {
    const activePerm = ctx.permission ?? "full";
    for (const item of SANDBOX_CHOICES) {
      result.push({
        value: item.id,
        label: item.id,
        description: t(item.desc),
        current: item.id === activePerm,
      });
    }
  } else if (param.dynamic === "mcp_server") {
    for (const server of ctx.mcpServers ?? []) {
      result.push({
        value: server.name,
        label: server.name,
        description: mcpServerDescription(server),
        statusTone: mcpPhaseTone(server.phase),
      });
    }
  }

  for (const choice of param.choices ?? []) {
    result.push({
      value: choice.value,
      label: choice.label ?? choice.value,
      description: choice.description,
      current:
        !command.plugin && command.name === "autonomy"
          ? choice.value === (ctx.autonomy ?? "default")
          : undefined,
      params: choice.params,
    });
  }
  return result;
}

/** 构建二级交互菜单分组与扁平项列表；所有条目均仅填入输入框，由用户回车确认执行。 */
export function buildSubmenuItems(
  command: SlashCommand,
  ctx: CommandContext,
): { sections: SubmenuSection[]; flat: SubmenuAction[] } {
  const root = command.params?.[0];
  if (!root || (root.kind !== "choice" && root.kind !== "dynamic")) {
    return { sections: [], flat: [] };
  }
  const sections: SubmenuSection[] = [];
  const primary: SubmenuAction[] = [];

  if (root.defaultBehavior) {
    primary.push({
      key: "__default__",
      label: t("cmd.default_action"),
      mono: false,
      description: root.defaultBehavior,
      fillText: `/${command.name}`,
      closeMenu: true,
    });
  }
  if (root.acceptsText) {
    primary.push({
      key: "__text__",
      label: `<${root.name}>`,
      mono: true,
      description: root.description,
      fillText: `/${command.name} `,
      closeMenu: false,
      preferHint: true,
      nextParam: { name: root.name, required: true },
    });
  }
  if (primary.length > 0) {
    sections.push({ key: "primary", items: primary });
  }

  const choiceItems: SubmenuAction[] = [];
  const resolved = resolveParamChoices(command, root, ctx);
  for (const choice of resolved) {
    const subParam = choice.params?.[0];
    const expectsInputNext = Boolean(
      subParam && subParam.kind !== "choice" && subParam.kind !== "dynamic",
    );
    choiceItems.push({
      key: `choice:${choice.value}`,
      label: choice.label,
      mono: true,
      description: choice.description,
      fillText: expectsInputNext
        ? `/${command.name} ${choice.value} `
        : `/${command.name} ${choice.value}`,
      closeMenu: !expectsInputNext,
      current: choice.current,
      statusTone: choice.statusTone,
      nextParam: subParam ? { name: subParam.name, required: subParam.required } : undefined,
    });

    // 展开 1 级固定/动态子选项（如 `cleanup all`、`reconnect <server>`），在二级菜单里一步填入。
    if (subParam && (subParam.kind === "choice" || subParam.kind === "dynamic")) {
      const nestedChoices = resolveParamChoices(command, subParam, ctx);
      for (const nested of nestedChoices) {
        choiceItems.push({
          key: `choice:${choice.value}:${nested.value}`,
          label: `${choice.value} ${nested.label}`,
          mono: true,
          description: nested.description,
          fillText: `/${command.name} ${choice.value} ${nested.value}`,
          closeMenu: true,
          current: nested.current,
          statusTone: nested.statusTone,
          nested: true,
        });
      }
    }
  }
  if (choiceItems.length > 0) {
    sections.push({
      key: "choices",
      heading: primary.length > 0 ? t("cmd.subcommands") : undefined,
      items: choiceItems,
    });
  }

  return { sections, flat: sections.flatMap((section) => section.items) };
}

function matchChoiceIndices(value: string, query: string): number[] | null {
  if (!query) return [];
  const lower = value.toLowerCase();
  const idx = lower.indexOf(query);
  if (idx < 0) return null;
  return Array.from({ length: query.length }, (_, offset) => idx + offset);
}

/**
 * 阶段二参数解析：当用户输入 `/命令 <args>` 且光标位于参数段时，沿结构化参数树递归或顺序钻取，
 * 产出可选参数列表（`choices`）或轻量自由文本参数引导（`hint`）。
 */
export function resolveStage2(
  text: string,
  cursor: number,
  commands: SlashCommand[],
  features: Record<string, boolean> | undefined,
  ctx: CommandContext,
  preferHint = false,
): Stage2State | null {
  const head = /^\/([\w-]+)([ \t]+)([\s\S]*)$/.exec(text);
  if (!head) return null;
  const typedName = head[1];
  const sep = head[2];
  const prefixLen = typedName.length + 1 + sep.length;
  if (cursor < prefixLen) return null;

  const command = commands.find(
    (item) => item.name === typedName || item.aliases?.includes(typedName),
  );
  if (!command) return null;
  if (command.flag !== undefined && features?.[command.flag] === false) return null;
  if (commandDisabledReason(command, ctx) !== null) return null;

  let commandPath = `/${typedName}`;
  let argText = head[3];
  const caretAtEnd = cursor === text.length;

  // 插件命令只有旧 `usage` 串时退回轻量参数提示。
  if (!command.params?.length) {
    if (!command.usage) return null;
    const required = command.usage.startsWith("<");
    const empty = argText.trim() === "";
    return {
      kind: "hint",
      command,
      commandPath,
      legacyUsage: command.usage,
      required,
      empty,
      missingRequired: required && empty,
    };
  }

  const queue: CommandParamMeta[] = [...command.params];
  let depth = 0;

  while (queue.length > 0) {
    const currentParam = queue.shift();
    if (!currentParam) break;

    if (currentParam.kind === "text") {
      const empty = argText.trim() === "";
      return {
        kind: "hint",
        command,
        commandPath,
        param: currentParam,
        required: currentParam.required,
        empty,
        missingRequired: currentParam.required && empty,
        ghostPlaceholder:
          argText === "" && caretAtEnd ? (currentParam.placeholder ?? undefined) : undefined,
      };
    }

    if (
      currentParam.kind === "path" ||
      currentParam.kind === "number" ||
      currentParam.kind === "token"
    ) {
      if (queue.length > 0) {
        const split = /^(\S+)([ \t]+)([\s\S]*)$/.exec(argText);
        if (split) {
          const token = split[1];
          const matchesScalar = currentParam.kind !== "number" || isValidNumberToken(token);
          if (matchesScalar) {
            commandPath = `${commandPath} ${token}`;
            argText = split[3];
            depth += 1;
            continue;
          }
          if (!currentParam.required) {
            depth += 1;
            continue;
          }
        }
        const trimmed = argText.trim();
        if (trimmed !== "" && (currentParam.kind !== "number" || isValidNumberToken(trimmed))) {
          const nextRequired = queue.find((param) => param.required);
          if (nextRequired) {
            return {
              kind: "hint",
              command,
              commandPath: `${commandPath} ${trimmed}`,
              param: nextRequired,
              required: true,
              empty: true,
              missingRequired: true,
            };
          }
        }
      }
      const empty = argText.trim() === "";
      return {
        kind: "hint",
        command,
        commandPath,
        param: currentParam,
        required: currentParam.required,
        empty,
        missingRequired: currentParam.required && empty,
        ghostPlaceholder:
          argText === "" && caretAtEnd ? (currentParam.placeholder ?? undefined) : undefined,
      };
    }

    const choices = resolveParamChoices(command, currentParam, ctx);
    const split = /^(\S+)([ \t]+)([\s\S]*)$/.exec(argText);
    if (split) {
      const token = split[1];
      const matched = matchResolvedChoice(currentParam, choices, token);
      if (matched) {
        commandPath = `${commandPath} ${matched.value}`;
        argText = split[3];
        if (matched.params?.length) {
          queue.unshift(...matched.params);
        }
        depth += 1;
        continue;
      }
      if (currentParam.acceptsText) {
        return {
          kind: "hint",
          command,
          commandPath,
          param: currentParam,
          required: false,
          empty: false,
          missingRequired: false,
        };
      }
      if (!currentParam.required && queue.length > 0) {
        depth += 1;
        continue;
      }
      return null;
    }

    // 混合命令由二级菜单点选 `<objective>` 进来时，首层直接进入文本提示，避免子命令列表挡路。
    if (preferHint && depth === 0 && currentParam.acceptsText) {
      const empty = argText.trim() === "";
      return {
        kind: "hint",
        command,
        commandPath,
        param: currentParam,
        required: false,
        empty,
        missingRequired: false,
        ghostPlaceholder:
          argText === "" && caretAtEnd ? (currentParam.placeholder ?? undefined) : undefined,
      };
    }

    const query = argText.toLowerCase();
    if (currentParam.dynamic === "mcp_server" && choices.length === 0) {
      return {
        kind: "hint",
        command,
        commandPath,
        param: currentParam,
        required: false,
        empty: query === "",
        missingRequired: false,
        ghostPlaceholder:
          argText === "" && caretAtEnd ? (currentParam.placeholder ?? undefined) : undefined,
        emptyDynamicNotice: t("cmd.mcp_no_servers"),
      };
    }

    const scored: { choice: ResolvedChoice; matches: number[]; prefix: boolean }[] = [];
    for (const choice of choices) {
      const matches = matchChoiceIndices(choice.value, query);
      if (matches !== null) {
        scored.push({
          choice,
          matches,
          prefix: query === "" || choice.value.toLowerCase().startsWith(query),
        });
      }
    }
    scored.sort((a, b) => Number(b.prefix) - Number(a.prefix));

    // 用户已键入唯一完整子命令时不再弹出阻塞选择菜单，让 Enter 可直接执行；
    // 若该子命令带有必填后续参数（如 `/goal edit`），则给出非阻塞提示提醒补充。
    if (query !== "" && scored.length === 1 && scored[0].choice.value.toLowerCase() === query) {
      const nextRequired =
        scored[0].choice.params?.find((param) => param.required) ??
        queue.find((param) => param.required);
      if (nextRequired) {
        return {
          kind: "hint",
          command,
          commandPath: `${commandPath} ${scored[0].choice.value}`,
          param: nextRequired,
          required: true,
          empty: true,
          missingRequired: true,
        };
      }
      return null;
    }

    if (scored.length === 0) {
      if (currentParam.acceptsText) {
        return {
          kind: "hint",
          command,
          commandPath,
          param: currentParam,
          required: false,
          empty: false,
          missingRequired: false,
        };
      }
      return null;
    }

    const items: Stage2ChoiceItem[] = scored.map(({ choice, matches }) => {
      const subParam = choice.params?.[0] ?? queue[0];
      const expectsInputNext = Boolean(
        subParam && subParam.kind !== "choice" && subParam.kind !== "dynamic",
      );
      return {
        key: `${commandPath}:${choice.value}`,
        value: choice.value,
        label: choice.label,
        description: choice.description,
        matches,
        fillText: expectsInputNext
          ? `${commandPath} ${choice.value} `
          : `${commandPath} ${choice.value}`,
        closeMenu: !expectsInputNext,
        current: choice.current,
        statusTone: choice.statusTone,
        nextParam: subParam ? { name: subParam.name, required: subParam.required } : undefined,
      };
    });

    return {
      kind: "choices",
      command,
      param: currentParam,
      commandPath,
      items,
      acceptsText: Boolean(currentParam.acceptsText),
      ghostPlaceholder:
        argText === "" && caretAtEnd ? (currentParam.placeholder ?? undefined) : undefined,
    };
  }

  return null;
}

// ---------- 命令胶囊（Command Chip）：非自然语言参数收拢与编辑 ----------

const NUMERIC_PARAM_NAMES = new Set([
  "count",
  "n",
  "num",
  "number",
  "times",
  "limit",
  "max",
  "min",
  "repeat",
  "rounds",
  "steps",
  "retries",
  "depth",
  "port",
  "index",
  "seconds",
  "sec",
  "ms",
]);

const PATH_PARAM_NAMES = new Set(["path", "file", "filepath", "dir", "directory", "folder"]);

const TEXT_PARAM_NAMES = new Set([
  "text",
  "prompt",
  "question",
  "objective",
  "instruction",
  "message",
  "msg",
  "command",
  "cmd",
  "query",
  "task",
  "body",
  "content",
  "input",
  "args",
  "rest",
  "note",
  "reason",
]);

const NUMBER_TOKEN_RE = /^[+-]?\d+(?:\.\d+)?$/;

/** 参数是否属于非自然语言结构化参数（应纳入命令胶囊，而非留在外部自由正文）。 */
export function isChipParamKind(kind: CommandParamKind): boolean {
  return kind !== "text";
}

/** 校验 `number` 类参数的字面量（纯整数或小数，不接受 NaN/Infinity）。 */
export function isValidNumberToken(value: string): boolean {
  return NUMBER_TOKEN_RE.test(value.trim());
}

/**
 * 为仅有旧 `usage` 串的插件命令推断非自然语言前导参数（如 `<count> <command>`、`[on|off]`、`[path]`）。
 * 含嵌套括号或无法稳定判定的单占位符保守返回空数组，避免误把自然语言参数收进胶囊。
 */
export function inferUsageParams(usage: string | undefined): CommandParamMeta[] {
  const trimmed = usage?.trim();
  if (!trimmed) return [];
  const rawTokens: { required: boolean; inner: string }[] = [];
  let cursor = 0;
  while (cursor < trimmed.length) {
    while (cursor < trimmed.length && /\s/.test(trimmed[cursor])) cursor += 1;
    if (cursor >= trimmed.length) break;
    const open = trimmed[cursor];
    const close = open === "<" ? ">" : open === "[" ? "]" : null;
    if (!close) return [];
    const end = trimmed.indexOf(close, cursor + 1);
    if (end < 0) return [];
    const inner = trimmed.slice(cursor + 1, end).trim();
    if (!inner || /[<>[\]]/.test(inner)) return [];
    rawTokens.push({ required: open === "<", inner });
    cursor = end + 1;
  }
  if (rawTokens.length === 0) return [];

  const params: CommandParamMeta[] = [];
  for (let i = 0; i < rawTokens.length; i += 1) {
    const { required, inner } = rawTokens[i];
    const isLast = i === rawTokens.length - 1;
    if (inner.includes("|")) {
      const choices = inner
        .split("|")
        .map((part) => part.trim())
        .filter(Boolean);
      if (choices.length < 2 || choices.some((choice) => !/^[\w.-]+$/.test(choice))) return [];
      params.push({
        name: "option",
        kind: "choice",
        required,
        description: "",
        choices: choices.map((value) => ({ value, description: "" })),
      });
      continue;
    }
    const normalized = inner.replace(/\.{3}$/, "").trim();
    const lower = normalized.toLowerCase();
    if (!normalized) return [];
    if (PATH_PARAM_NAMES.has(lower)) {
      params.push({ name: normalized, kind: "path", required, description: "" });
    } else if (NUMERIC_PARAM_NAMES.has(lower)) {
      params.push({ name: normalized, kind: "number", required, description: "" });
    } else if (!isLast) {
      params.push({ name: normalized, kind: "token", required, description: "" });
    } else if (TEXT_PARAM_NAMES.has(lower) || rawTokens.length > 1) {
      params.push({ name: normalized, kind: "text", required, description: "" });
    } else {
      return [];
    }
  }
  return params.some((param) => isChipParamKind(param.kind)) ? params : [];
}

/** 命令的有效参数定义：优先用显式结构化 `params`，插件仅有 `usage` 时轻量推断。 */
export function effectiveCommandParams(command: SlashCommand): CommandParamMeta[] {
  if (command.params?.length) return command.params;
  return inferUsageParams(command.usage);
}

function matchResolvedChoice(
  param: CommandParamMeta,
  choices: ResolvedChoice[],
  raw: string,
): ResolvedChoice | undefined {
  const normalized =
    param.dynamic === "sandbox_mode" && raw.toLowerCase() === "workspace-only"
      ? "workspace_only"
      : raw;
  return choices.find((item) => item.value.toLowerCase() === normalized.toLowerCase());
}

/** 扫描 `pos` 之后的水平空白 + 单个非空白 token（不跨换行）。 */
function scanNextToken(
  text: string,
  pos: number,
): { start: number; end: number; raw: string } | null {
  let cursor = pos;
  while (cursor < text.length && (text[cursor] === " " || text[cursor] === "\t")) cursor += 1;
  if (cursor === pos || cursor >= text.length || text[cursor] === "\n" || text[cursor] === "\r") {
    return null;
  }
  const start = cursor;
  while (cursor < text.length && !/[ \t\r\n]/.test(text[cursor])) cursor += 1;
  return { start, end: cursor, raw: text.slice(start, cursor) };
}

/** 扫描单独末尾 `path` 参数（允许路径内含空格，止于行尾并去掉尾随空白）。 */
function scanTrailingPath(
  text: string,
  pos: number,
): { start: number; end: number; raw: string } | null {
  let cursor = pos;
  while (cursor < text.length && (text[cursor] === " " || text[cursor] === "\t")) cursor += 1;
  if (cursor === pos || cursor >= text.length || text[cursor] === "\n" || text[cursor] === "\r") {
    return null;
  }
  const start = cursor;
  const lineBreak = text.indexOf("\n", start);
  let end = lineBreak >= 0 ? lineBreak : text.length;
  while (
    end > start &&
    (text[end - 1] === " " || text[end - 1] === "\t" || text[end - 1] === "\r")
  ) {
    end -= 1;
  }
  if (end <= start) return null;
  return { start, end, raw: text.slice(start, end) };
}

/** 单个非自然语言参数槽（已纳入胶囊或待编辑的空槽位）。 */
export type CommandChipArg = {
  /** 在原输入文本中的起止下标；未填写时为 null。 */
  from: number | null;
  to: number | null;
  /** 参数文本值；未填写时为空串。 */
  value: string;
  param: CommandParamMeta;
  matchedChoice?: ResolvedChoice;
};

/** 输入框开头斜杠命令的胶囊解析结果。 */
export type ParsedCommandChip = {
  command: SlashCommand;
  /** 用户输入的命令名（可能是别名，不含前导 `/`）。 */
  typedName: string;
  /** 胶囊覆盖的文本范围 `[start, end)`（含命令名与已落定的非自然语言参数，不含尾随空格与自然语言正文）。 */
  start: number;
  end: number;
  /** 仅命令名本身的结束下标（`start + 1 + typedName.length`）。 */
  nameEnd: number;
  /** 已纳入胶囊的非自然语言参数。 */
  chipArgs: CommandChipArg[];
  /** 点开胶囊编辑时呈现的全部非自然语言参数槽（含已填与当前分支下可继续填写的空槽）。 */
  editableSlots: CommandChipArg[];
  /** 胶囊之后的剩余文本（含前导空白或自然语言正文）。 */
  trailingText: string;
  /** 该命令是否存在任何非自然语言参数可供点开编辑。 */
  editable: boolean;
  /** 胶囊内按序展示的文本片段：命令名 + 已纳入的非自然语言参数值。 */
  tokens: string[];
};

/**
 * 解析输入开头的斜杠命令胶囊范围与结构化参数：
 * - 非自然语言参数（`choice` / `dynamic` / `path` / `number` / `token`）收拢进胶囊；
 * - 自然语言参数（`text` 或混合命令未命中子命令时的正文）留在胶囊外；
 * - `typingAt` 命中某个参数 token 时暂不吞入该 token，避免逐字键入或退格时跳变。
 */
export function parseCommandChip(
  text: string,
  commands: SlashCommand[],
  ctx: CommandContext,
  typingAt: number | null = null,
): ParsedCommandChip | null {
  const head = /^([ \t]*)\/([\w-]+)(?=$|\s)/.exec(text);
  if (!head) return null;
  const typedName = head[2];
  const command = commands.find(
    (item) => item.name === typedName || item.aliases?.includes(typedName),
  );
  if (!command) return null;

  const start = head[1].length;
  const nameEnd = start + 1 + typedName.length;
  const rootParams = effectiveCommandParams(command);

  const chipArgs: CommandChipArg[] = [];
  const editableSlots: CommandChipArg[] = [];
  const queue: CommandParamMeta[] = [...rootParams];
  let pos = nameEnd;
  let stopped = false;

  while (queue.length > 0) {
    const param = queue.shift();
    if (!param) break;
    if (param.kind === "text") break;

    if (stopped) {
      editableSlots.push({ from: null, to: null, value: "", param });
      continue;
    }

    const scanned =
      param.kind === "path" && queue.length === 0
        ? scanTrailingPath(text, pos)
        : scanNextToken(text, pos);

    if (!scanned || (typingAt !== null && typingAt >= scanned.start && typingAt <= scanned.end)) {
      editableSlots.push({ from: null, to: null, value: "", param });
      stopped = true;
      continue;
    }

    const { start: tokenStart, end: tokenEnd, raw } = scanned;

    if (param.kind === "choice" || param.kind === "dynamic") {
      const choices = resolveParamChoices(command, param, ctx);
      const matched = matchResolvedChoice(param, choices, raw);
      if (matched) {
        const arg: CommandChipArg = {
          from: tokenStart,
          to: tokenEnd,
          value: raw,
          param,
          matchedChoice: matched,
        };
        chipArgs.push(arg);
        editableSlots.push(arg);
        pos = tokenEnd;
        if (matched.params?.length) {
          queue.unshift(...matched.params);
        }
        continue;
      }
      // MCP server 列表尚未加载或手动输入自定义服务名时，单个非空白标识仍视作 server 参数。
      if (param.kind === "dynamic" && param.dynamic === "mcp_server" && !param.acceptsText) {
        const arg: CommandChipArg = { from: tokenStart, to: tokenEnd, value: raw, param };
        chipArgs.push(arg);
        editableSlots.push(arg);
        pos = tokenEnd;
        continue;
      }
      if (param.acceptsText) {
        editableSlots.push({ from: null, to: null, value: "", param });
        stopped = true;
        continue;
      }
      if (!param.required && queue.length > 0) {
        editableSlots.push({ from: null, to: null, value: "", param });
        continue;
      }
      editableSlots.push({ from: null, to: null, value: "", param });
      stopped = true;
      continue;
    }

    if (param.kind === "number") {
      if (isValidNumberToken(raw)) {
        const arg: CommandChipArg = { from: tokenStart, to: tokenEnd, value: raw, param };
        chipArgs.push(arg);
        editableSlots.push(arg);
        pos = tokenEnd;
        continue;
      }
      if (!param.required && queue.length > 0) {
        editableSlots.push({ from: null, to: null, value: "", param });
        continue;
      }
      editableSlots.push({ from: null, to: null, value: "", param });
      stopped = true;
      continue;
    }

    // `path` 或 `token`
    const arg: CommandChipArg = { from: tokenStart, to: tokenEnd, value: raw, param };
    chipArgs.push(arg);
    editableSlots.push(arg);
    pos = tokenEnd;
  }

  return {
    command,
    typedName,
    start,
    end: pos,
    nameEnd,
    chipArgs,
    editableSlots,
    trailingText: text.slice(pos),
    editable: editableSlots.length > 0,
    tokens: [typedName, ...chipArgs.map((arg) => arg.value)],
  };
}

/** 选中的子命令选项是否带有后续的非自然语言子参数（用于在编辑浮层中保持展开并继续填写）。 */
export function choiceHasChipSubparams(choice: ResolvedChoice | undefined): boolean {
  return Boolean(choice?.params?.some((param) => isChipParamKind(param.kind)));
}

/**
 * 将命令胶囊编辑器里的参数值写回整条输入文本：
 * - 若目标分支接受后续自然语言正文（存在后续 `text` 参数或 `acceptsText`），保留原有正文并在需要继续键入时补尾随空格；
 * - 若目标分支是不接受自由正文的叶子命令（如 `/goal pause`、`/autonomy dont_ask`），只保留命令与结构化参数。
 */
export function applyCommandChipEdit(
  parsed: ParsedCommandChip,
  text: string,
  values: readonly string[],
  ctx: CommandContext,
): { text: string; caret: number; expectsText: boolean } {
  const queue: CommandParamMeta[] = [...effectiveCommandParams(parsed.command)];
  const finalTokens: string[] = [];
  let slotIndex = 0;
  let acceptsTrailingText = false;
  let expectsText = false;

  while (queue.length > 0) {
    const param = queue.shift();
    if (!param) break;
    if (param.kind === "text") {
      acceptsTrailingText = true;
      expectsText = true;
      break;
    }
    const rawValue = (values[slotIndex] ?? "").trim();
    slotIndex += 1;

    if (rawValue === "") {
      if (param.acceptsText) acceptsTrailingText = true;
      if (!param.required && queue.length > 0) continue;
      break;
    }

    finalTokens.push(rawValue);
    if (param.kind === "choice" || param.kind === "dynamic") {
      const choices = resolveParamChoices(parsed.command, param, ctx);
      const matched = matchResolvedChoice(param, choices, rawValue);
      if (matched?.params?.length) {
        queue.unshift(...matched.params);
      }
    }
  }

  const leading = text.slice(0, parsed.start);
  const header = `${leading}/${parsed.typedName}${finalTokens.length > 0 ? ` ${finalTokens.join(" ")}` : ""}`;
  const trimmedTrailing = parsed.trailingText.replace(/^[ \t]+/, "");

  let nextText: string;
  if (acceptsTrailingText) {
    if (trimmedTrailing.length > 0) {
      nextText = parsed.trailingText.startsWith("\n")
        ? `${header}${parsed.trailingText}`
        : `${header} ${trimmedTrailing}`;
    } else if (expectsText) {
      nextText = `${header} `;
    } else {
      nextText = header;
    }
  } else {
    nextText = header;
  }

  return { text: nextText, caret: nextText.length, expectsText };
}
