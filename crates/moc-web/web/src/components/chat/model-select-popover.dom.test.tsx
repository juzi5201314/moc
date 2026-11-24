import { cleanup, fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { useI18n } from "@/lib/i18n";
import type { ModelInfo } from "@/protocol/types";
import { useUi } from "@/store/ui";

const state = vi.hoisted(() => ({
  models: [] as ModelInfo[],
  recent: [] as string[],
  selector: "provider/first",
}));

vi.mock("@/store/connection", () => ({
  useConnection: (select: (value: unknown) => unknown) =>
    select({
      config: { models: state.models, recent_models: state.recent, default_model: state.selector },
    }),
  selectModels: (value: { config: { models: ModelInfo[] } }) => value.config.models,
  selectRecentModels: (value: { config: { recent_models: string[] } }) =>
    value.config.recent_models,
}));

vi.mock("@/store/lifecycle", () => ({
  setSessionModel: vi.fn(() => Promise.resolve()),
  setSessionFastMode: vi.fn(() => Promise.resolve()),
}));

import { ModelPicker } from "./model-select-popover";

const initialUi = useUi.getState();
const initialLocale = useI18n.getState().locale;
const scrollIntoView = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollIntoView");

beforeEach(() => {
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  Object.defineProperty(HTMLElement.prototype, "scrollIntoView", {
    configurable: true,
    value: vi.fn(),
  });
  useI18n.setState({ locale: "zh" });
  useUi.setState({ modelPicker: "mobile-picker" });
  state.models = [{ id: "provider/first", display_name: "First", thinking_levels: [] }];
  state.recent = [];
  state.selector = "provider/first";
});

afterEach(() => {
  cleanup();
  useUi.setState(initialUi);
  useI18n.setState({ locale: initialLocale });
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  if (scrollIntoView)
    Object.defineProperty(HTMLElement.prototype, "scrollIntoView", scrollIntoView);
  else Reflect.deleteProperty(HTMLElement.prototype, "scrollIntoView");
});

it("最近使用置顶；详情卡是独立浮层，不在列表弹层里留下遮挡区", () => {
  state.recent = ["provider/first:high"];
  const view = render(<ModelPicker sessionId="mobile-picker" />);
  expect(view.getAllByRole("option")[0].textContent).toContain("high");
  const [list, detail] = view.getAllByRole("dialog");
  expect(list.className).toContain("max-w-[calc(100vw-2rem)]");
  expect(list.contains(detail)).toBe(false);
  expect(detail.textContent).toContain("first");
});

it("空模型列表仍显示搜索和模型管理入口", () => {
  state.models = [];
  const view = render(<ModelPicker sessionId="mobile-picker" />);
  expect(view.getByRole("combobox")).toBeTruthy();
  expect(view.getByRole("button", { name: /Manage providers/ })).toBeTruthy();
  expect(view.queryByRole("option", { name: "first" })).toBeNull();
});

it("搜索无结果后可恢复模型列表", async () => {
  const view = render(<ModelPicker sessionId="mobile-picker" />);
  const search = view.getByRole("combobox");
  fireEvent.change(search, { target: { value: "not-a-model" } });
  await waitFor(() => expect(view.queryByRole("option", { name: "first" })).toBeNull());
  fireEvent.change(search, { target: { value: "" } });
  await waitFor(() => expect(view.getByRole("option", { name: "first" })).toBeTruthy());
});
