// 斜杠命令结构化参数引擎：分组排序、上下文门控、动态候选供给、二级菜单条目构建与阶段二参数解析。

import { type MsgKey, t } from "@/lib/i18n";
import type { SlashCommand } from "@/lib/slash";
import type {
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
    const expectsTextNext = Boolean(
      subParam && (subParam.kind === "text" || subParam.kind === "path"),
    );
    choiceItems.push({
      key: `choice:${choice.value}`,
      label: choice.label,
      mono: true,
      description: choice.description,
      fillText: expectsTextNext
        ? `/${command.name} ${choice.value} `
        : `/${command.name} ${choice.value}`,
      closeMenu: !expectsTextNext,
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
 * 阶段二参数解析：当用户输入 `/命令 <args>` 且光标位于参数段时，沿结构化参数树递归钻取，
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

  let currentParam: CommandParamMeta | undefined = command.params[0];
  let depth = 0;

  while (currentParam) {
    if (currentParam.kind === "text" || currentParam.kind === "path") {
      const empty = argText.trim() === "";
      return {
        kind: "hint",
        command,
        commandPath,
        param: currentParam,
        required: currentParam.required,
        empty,
        missingRequired: currentParam.required && empty,
        ghostPlaceholder: argText === "" && caretAtEnd ? currentParam.placeholder : undefined,
      };
    }

    const choices = resolveParamChoices(command, currentParam, ctx);
    const split = /^(\S+)([ \t]+)([\s\S]*)$/.exec(argText);
    if (split) {
      const token = split[1];
      const matched = choices.find((item) => item.value.toLowerCase() === token.toLowerCase());
      if (matched?.params?.length) {
        commandPath = `${commandPath} ${matched.value}`;
        argText = split[3];
        currentParam = matched.params[0];
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
        ghostPlaceholder: argText === "" && caretAtEnd ? currentParam.placeholder : undefined,
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
        ghostPlaceholder: argText === "" && caretAtEnd ? currentParam.placeholder : undefined,
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
      const nextRequired = scored[0].choice.params?.find((param) => param.required);
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
      const subParam = choice.params?.[0];
      const expectsTextNext = Boolean(
        subParam && (subParam.kind === "text" || subParam.kind === "path"),
      );
      return {
        key: `${commandPath}:${choice.value}`,
        value: choice.value,
        label: choice.label,
        description: choice.description,
        matches,
        fillText: expectsTextNext
          ? `${commandPath} ${choice.value} `
          : `${commandPath} ${choice.value}`,
        closeMenu: !expectsTextNext,
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
      ghostPlaceholder: argText === "" && caretAtEnd ? currentParam.placeholder : undefined,
    };
  }

  return null;
}
