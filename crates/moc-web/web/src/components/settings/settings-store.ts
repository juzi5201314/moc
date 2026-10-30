// 设置文档状态：全局 / 项目两份 SettingsDocument，改动即保存（自动保存）。
// 保存按队列串行：每次基于最新 revision 计算文档，避免连续编辑互相覆盖。

import { toast } from "sonner";
import { create } from "zustand";
import { setPath } from "@/lib/settings-doc";
import { rpc } from "@/protocol/client";
import type { ConfigAction, ConfigReply, ConfigScope, SettingsDocument } from "@/protocol/types";
import { useConnection } from "@/store/connection";

type Doc = Record<string, unknown>;

type SettingsState = {
  cwd: string;
  scope: ConfigScope;
  docs: Partial<Record<ConfigScope, SettingsDocument>>;
  errors: Partial<Record<ConfigScope, string>>;
  loading: boolean;
  /** 在途保存数；>0 时顶栏显示「保存中」。 */
  saving: number;
  savedAt: number | null;
  setScope: (scope: ConfigScope) => void;
};

export const useSettings = create<SettingsState>()((set) => ({
  cwd: "",
  scope: "global",
  docs: {},
  errors: {},
  loading: false,
  saving: 0,
  savedAt: null,
  setScope: (scope) => set({ scope }),
}));

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function readScope(scope: ConfigScope, cwd: string): Promise<SettingsDocument> {
  const action: ConfigAction = { action: "read", scope, cwd };
  const reply = await rpc.call<ConfigReply>("config/manage", action);
  if (reply.kind !== "settings") throw new Error("config/manage 返回了非 settings 响应");
  return reply.data;
}

/** 打开设置或切换工作目录时重读两份文档。 */
export async function loadSettings(cwd: string): Promise<void> {
  useSettings.setState({ cwd, loading: true, errors: {} });
  const scopes: ConfigScope[] = ["global", "project"];
  const results = await Promise.allSettled(scopes.map((scope) => readScope(scope, cwd)));
  if (useSettings.getState().cwd !== cwd) return;
  const docs: SettingsState["docs"] = {};
  const errors: SettingsState["errors"] = {};
  results.forEach((result, index) => {
    const scope = scopes[index];
    if (result.status === "fulfilled") docs[scope] = result.value;
    else errors[scope] = errorText(result.reason);
  });
  useSettings.setState({ docs, errors, loading: false });
}

let queue: Promise<unknown> = Promise.resolve();

/**
 * 修改当前范围的文档并保存。`mutate` 拿到的是排队轮到时的最新文档。
 * 保存成功后刷新 `config/get`，让模型选择器等处立刻看到新配置。
 */
export function writeSettings(mutate: (doc: Doc) => Doc, label?: string): Promise<boolean> {
  const scope = useSettings.getState().scope;
  useSettings.setState((state) => ({ saving: state.saving + 1 }));
  const task = queue.then(async () => {
    const { docs, cwd } = useSettings.getState();
    const current = docs[scope];
    if (!current) return false;
    try {
      const action: ConfigAction = {
        action: "save",
        scope,
        cwd,
        document: mutate(current.document),
        revision: current.revision,
      };
      const reply = await rpc.call<ConfigReply>("config/manage", action);
      if (reply.kind !== "settings") throw new Error("config/manage 返回了非 settings 响应");
      useSettings.setState((state) => ({
        docs: { ...state.docs, [scope]: reply.data },
        savedAt: Date.now(),
      }));
      if (label) toast.success(label);
      void useConnection
        .getState()
        .refreshConfig()
        .catch(() => undefined);
      return true;
    } catch (error) {
      toast.error(`保存失败：${errorText(error)}`);
      // 本地文档没动；重读一次以防是 revision 冲突（文件被别处改过）。
      void readScope(scope, cwd)
        .then((data) =>
          useSettings.setState((state) => ({ docs: { ...state.docs, [scope]: data } })),
        )
        .catch(() => undefined);
      return false;
    } finally {
      useSettings.setState((state) => ({ saving: state.saving - 1 }));
    }
  });
  queue = task;
  return task;
}

/** 写一个字段；`undefined` 删除该键（回落到继承值 / 默认值）。 */
export function writeField(path: string[], value: unknown): Promise<boolean> {
  return writeSettings((doc) => setPath(doc, path, value));
}
