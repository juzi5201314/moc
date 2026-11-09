// 终端分页的状态：每个工作区（会话 cwd）一份终端列表与激活标签。
// xterm 实例不进 store，组件按 terminal id 附着；这里只记元信息与激活项。

import { toast } from "sonner";
import { create } from "zustand";
import type { PtyInfo } from "@/lib/web-api";
import { createPty, deletePty, listPty } from "@/lib/web-api";
import { useLayout } from "./layout";

export type TerminalsState = {
  /** workspace cwd → 该工作区的终端列表与激活 id。 */
  byCwd: Record<string, { items: PtyInfo[]; active?: number }>;
};

export const useTerminals = create<TerminalsState>()(() => ({ byCwd: {} }));

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** dev StrictMode 双跑 effect 会让 autoCreate 建两个终端：按 cwd 记一次就够。 */
const autoCreated = new Set<string>();

/** 拉取某工作区的终端列表；`autoCreate` 时一个都没有就顺手建一个。 */
export async function refreshTerminals(cwd: string, autoCreate = false): Promise<PtyInfo[]> {
  try {
    let items = await listPty(cwd);
    if (items.length === 0 && autoCreate && !autoCreated.has(cwd)) {
      autoCreated.add(cwd);
      items = [await createPty(cwd)];
    }
    useTerminals.setState((state) => {
      const current = state.byCwd[cwd];
      const active = current?.active;
      const stillThere = items.some((item) => item.id === active);
      return {
        byCwd: {
          ...state.byCwd,
          [cwd]: { items, active: stillThere ? active : items.at(-1)?.id },
        },
      };
    });
    return items;
  } catch (error) {
    toast.error(`获取终端失败：${errorText(error)}`);
    return [];
  }
}

/** 新建终端并激活；切到终端分页由调用方做。 */
export async function spawnTerminal(cwd: string): Promise<PtyInfo | null> {
  try {
    const item = await createPty(cwd);
    useTerminals.setState((state) => {
      const current = state.byCwd[cwd] ?? { items: [] };
      return {
        byCwd: {
          ...state.byCwd,
          [cwd]: { items: [...current.items, item], active: item.id },
        },
      };
    });
    return item;
  } catch (error) {
    toast.error(`创建终端失败：${errorText(error)}`);
    return null;
  }
}

/** 关闭终端标签：杀进程并移除条目。 */
export async function closeTerminal(cwd: string, id: number): Promise<void> {
  try {
    await deletePty(id);
  } catch (error) {
    toast.error(`关闭终端失败：${errorText(error)}`);
  }
  useTerminals.setState((state) => {
    const current = state.byCwd[cwd];
    if (!current) return state;
    const items = current.items.filter((item) => item.id !== id);
    return {
      byCwd: {
        ...state.byCwd,
        [cwd]: { items, active: current.active === id ? items.at(-1)?.id : current.active },
      },
    };
  });
}

export function activateTerminal(cwd: string, id: number): void {
  useTerminals.setState((state) => {
    const current = state.byCwd[cwd];
    if (!current) return state;
    return { byCwd: { ...state.byCwd, [cwd]: { ...current, active: id } } };
  });
}

/** 文件树「在终端中打开」：建终端、切到终端分页并激活。 */
export async function openTerminal(cwd: string): Promise<void> {
  const item = await spawnTerminal(cwd);
  if (!item) return;
  useLayout.getState().openInspector("terminal");
}
