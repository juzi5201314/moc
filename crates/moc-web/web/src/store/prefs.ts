// webui 前端状态：界面偏好与用户手动添加的工作区目录。
// 持久化在 core 侧 `~/.moc/web.toml`，经 web/state、web/state_save RPC 读写；
// localStorage 只剩主题的首帧镜像和待迁移的旧版 key。

import { toast } from "sonner";
import { create } from "zustand";
import { clientListeners, rpc } from "@/protocol/client";
import type { WebState, WebStateDocument } from "@/protocol/types";

export type Theme = "system" | "light" | "dark";
/** 运行中按 Enter：插队补充到当前提示（steer），或中断后作为下一条提示（interrupt）。 */
export type RunningEnter = "steer" | "interrupt";
export type DiffMode = "unified" | "split";

export type Prefs = {
  theme: Theme;
  runningEnter: RunningEnter;
  notify: boolean;
  diffMode: DiffMode;
  /** 思考过程默认展开。 */
  expandThinking: boolean;
};

type PrefsStore = Prefs & {
  /** 用户手动添加的工作区目录。 */
  workspaces: string[];
  set: (patch: Partial<Prefs>) => void;
  addWorkspace: (cwd: string) => void;
  removeWorkspace: (cwd: string) => void;
};

/** 首帧主题镜像：index.html 在 JS 起来前读它防闪屏，权威来源是 web.toml。 */
const THEME_MIRROR_KEY = "moc-web-theme";
/** 旧版 localStorage key；web.toml 尚不存在时迁上去，之后作废。 */
const LEGACY_PREFS_KEY = "moc-web-prefs";
const LEGACY_WORKSPACES_KEY = "dsh-workspaces-paths";

/** 最近一次 web/state 返回的版本戳；null 表示还没读到，不能写。 */
let revision: string | null = null;
let queue: Promise<unknown> = Promise.resolve();

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function cleanPath(cwd: string): string {
  return cwd.trim().replace(/(.)[\\/]+$/, "$1");
}

function mirrorTheme(theme: Theme): void {
  try {
    localStorage.setItem(THEME_MIRROR_KEY, theme);
  } catch {
    // 写不进去时只影响下次打开的首帧
  }
}

function toWire(prefs: Prefs, workspaces: string[]): WebState {
  return {
    theme: prefs.theme,
    active_submit: prefs.runningEnter === "steer" ? "steering" : "interrupt",
    notify: prefs.notify,
    diff_mode: prefs.diffMode,
    expand_thinking: prefs.expandThinking,
    workspaces,
  };
}

function fromWire(state: WebState): Prefs {
  return {
    theme: state.theme,
    runningEnter: state.active_submit === "steering" ? "steer" : "interrupt",
    notify: state.notify,
    diffMode: state.diff_mode,
    expandThinking: state.expand_thinking,
  };
}

function apply(doc: WebStateDocument): void {
  revision = doc.revision;
  const prefs = fromWire(doc.state);
  usePrefs.setState({ ...prefs, workspaces: doc.state.workspaces });
  mirrorTheme(prefs.theme);
}

async function sync(): Promise<void> {
  if (revision === null) await read();
  if (revision === null) return;
  const { workspaces, theme, runningEnter, notify, diffMode, expandThinking } = usePrefs.getState();
  try {
    const doc = await rpc.call<WebStateDocument>("web/state_save", {
      revision,
      state: toWire({ theme, runningEnter, notify, diffMode, expandThinking }, workspaces),
    });
    // 服务端原样回显，只收版本戳；乐观更新保持领先。
    revision = doc.revision;
  } catch (error) {
    // 冲突或掉线：以服务端为准重读，乐观改动随之作废。
    await read();
    toast.error(`偏好保存失败：${errorText(error)}`);
  }
}

function readLegacy(): { prefs: Partial<Prefs>; workspaces: string[] } {
  const prefs: Partial<Prefs> = {};
  try {
    const raw = JSON.parse(localStorage.getItem(LEGACY_PREFS_KEY) ?? "null");
    // zustand persist 包成 { state, version }；兜底兼容裸对象。
    const state = (raw?.state ?? raw ?? {}) as Record<string, unknown>;
    if (state.theme === "system" || state.theme === "light" || state.theme === "dark") {
      prefs.theme = state.theme;
    }
    if (state.runningEnter === "steer" || state.runningEnter === "interrupt") {
      prefs.runningEnter = state.runningEnter;
    }
    if (typeof state.notify === "boolean") prefs.notify = state.notify;
    if (state.diffMode === "unified" || state.diffMode === "split") {
      prefs.diffMode = state.diffMode;
    }
    if (typeof state.expandThinking === "boolean") prefs.expandThinking = state.expandThinking;
  } catch {
    // 解析失败按没有旧数据处理
  }
  let workspaces: string[] = [];
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(LEGACY_WORKSPACES_KEY) ?? "[]");
    if (Array.isArray(parsed)) {
      workspaces = parsed.filter((item): item is string => typeof item === "string");
    }
  } catch {
    // 解析失败按没有旧数据处理
  }
  return { prefs, workspaces };
}

function clearLegacy(): void {
  try {
    localStorage.removeItem(LEGACY_PREFS_KEY);
    localStorage.removeItem(LEGACY_WORKSPACES_KEY);
  } catch {
    // 清不掉时旧 key 留在原地，下次仍会尝试迁移
  }
}

/** web.toml 还不存在时把旧 localStorage 数据迁上去；写失败保留旧 key 下次再试。 */
async function migrate(doc: WebStateDocument): Promise<WebStateDocument> {
  const { prefs, workspaces } = readLegacy();
  if (Object.keys(prefs).length === 0 && workspaces.length === 0) return doc;
  const state = toWire({ ...fromWire(doc.state), ...prefs }, [
    ...new Set([...workspaces, ...doc.state.workspaces]),
  ]);
  try {
    const saved = await rpc.call<WebStateDocument>("web/state_save", {
      revision: doc.revision,
      state,
    });
    clearLegacy();
    return saved;
  } catch {
    return doc;
  }
}

async function read(): Promise<void> {
  try {
    let doc = await rpc.call<WebStateDocument>("web/state", {});
    if (doc.exists) clearLegacy();
    else doc = await migrate(doc);
    apply(doc);
  } catch (error) {
    toast.error(`加载 web 偏好失败：${errorText(error)}`);
  }
}

export const usePrefs = create<PrefsStore>()((set, get) => ({
  theme: "system",
  runningEnter: "steer",
  notify: false,
  diffMode: "unified",
  expandThinking: false,
  workspaces: [],

  set: (patch) => {
    set(patch);
    if (patch.theme) mirrorTheme(patch.theme);
    queue = queue.then(sync);
  },

  addWorkspace: (cwd) => {
    const clean = cleanPath(cwd);
    if (!clean || get().workspaces.includes(clean)) return;
    set({ workspaces: [clean, ...get().workspaces] });
    queue = queue.then(sync);
  },

  removeWorkspace: (cwd) => {
    const clean = cleanPath(cwd);
    set({ workspaces: get().workspaces.filter((item) => item !== clean) });
    queue = queue.then(sync);
  },
}));

// 每次握手（含重连）后重读：另一浏览器可能动过同一份 web.toml。
clientListeners.ready.add(() => {
  queue = queue.then(read);
});
