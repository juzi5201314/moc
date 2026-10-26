// 顶部标签页：打开的会话与当前会话，持久化到 localStorage，重连后按顺序重开。

import { create } from "zustand";

const TABS_KEY = "moc-web-tabs";

type Persisted = { open: string[]; active: string | null };

function loadTabs(): Persisted {
  try {
    const raw = localStorage.getItem(TABS_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : null;
    if (parsed && typeof parsed === "object" && "open" in parsed && Array.isArray(parsed.open)) {
      const open = parsed.open.filter((item): item is string => typeof item === "string");
      const active =
        "active" in parsed && typeof parsed.active === "string" && open.includes(parsed.active)
          ? parsed.active
          : (open[0] ?? null);
      return { open, active };
    }
  } catch {
    // 损坏的持久化数据按空处理
  }
  return { open: [], active: null };
}

export type TabsState = Persisted & {
  add: (id: string, activate?: boolean) => void;
  close: (id: string) => void;
  activate: (id: string) => void;
  replace: (oldId: string, newId: string) => void;
  move: (from: number, to: number) => void;
};

function save(state: Persisted) {
  localStorage.setItem(TABS_KEY, JSON.stringify({ open: state.open, active: state.active }));
}

export const useTabs = create<TabsState>()((set, get) => {
  const update = (next: Persisted) => {
    save(next);
    set(next);
  };
  return {
    ...loadTabs(),
    add: (id, activate = true) => {
      const { open, active } = get();
      update({
        open: open.includes(id) ? open : [...open, id],
        active: activate ? id : (active ?? id),
      });
    },
    close: (id) => {
      const { open, active } = get();
      const index = open.indexOf(id);
      if (index < 0) return;
      const next = open.filter((item) => item !== id);
      const nextActive = active === id ? (next[Math.min(index, next.length - 1)] ?? null) : active;
      update({ open: next, active: nextActive });
    },
    activate: (id) => {
      if (get().open.includes(id)) update({ open: get().open, active: id });
    },
    replace: (oldId, newId) => {
      const { open, active } = get();
      const replaced = open.map((item) => (item === oldId ? newId : item));
      const deduped = replaced.filter((item, index) => replaced.indexOf(item) === index);
      update({ open: deduped, active: active === oldId ? newId : active });
    },
    move: (from, to) => {
      const open = [...get().open];
      const [item] = open.splice(from, 1);
      if (item === undefined) return;
      open.splice(to, 0, item);
      update({ open, active: get().active });
    },
  };
});
