import { cleanup, fireEvent, render, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it } from "vitest";
import { TooltipProvider } from "@/components/ui/tooltip";
import { useI18n } from "@/lib/i18n";
import type { SettingsDocument } from "@/protocol/types";
import { usePrefs } from "@/store/prefs";
import { useUi } from "@/store/ui";
import { SettingsDialog } from "./settings-dialog";
import { useSettings } from "./settings-store";

const initialSettings = useSettings.getState();
const initialUi = useUi.getState();
const initialPrefs = usePrefs.getState();
const initialLocale = useI18n.getState().locale;
const doc: SettingsDocument = {
  defaults: {},
  document: {},
  effective: {},
  fields: [],
  inherited: {},
  model_template: {},
  provider_template: {},
  path: `/long/${"workspace/".repeat(30)}web.toml`,
  revision: "1",
};

beforeEach(() => {
  useI18n.setState({ locale: "zh" });
  useUi.setState({ settings: { open: true, section: "general" } });
  useSettings.setState({ docs: {}, errors: {}, loading: false });
});

afterEach(() => {
  cleanup();
  useSettings.setState(initialSettings);
  useUi.setState(initialUi);
  usePrefs.setState(initialPrefs);
  useI18n.setState({ locale: initialLocale });
});

function openDialog() {
  const result = render(
    <TooltipProvider>
      <SettingsDialog />
    </TooltipProvider>,
  );
  return within(result.getByRole("dialog"));
}

it("分类在窄屏横向滚动，内容与控件占满一行，桌面保留侧栏布局", () => {
  const view = openDialog();
  const nav = view.getByRole("navigation");
  expect(nav.className).toContain("w-full");
  expect(nav.className).toContain("md:w-[220px]");
  expect(nav.parentElement?.className).toContain("flex-col");
  expect(nav.parentElement?.className).toContain("md:flex-row");
  expect(nav.querySelector(".overflow-x-auto")).not.toBeNull();
  const theme = view.getByRole("button", { name: "Dark" });
  expect(theme.parentElement?.className).toContain("max-w-full");
  expect(theme.parentElement?.className).toContain("flex-wrap");
  expect(theme.parentElement?.parentElement?.className).toContain("w-full");
  fireEvent.click(theme);
  expect(usePrefs.getState().theme).toBe("dark");
  fireEvent.click(within(nav).getByRole("button", { name: "Keybindings" }));
  expect(view.getByRole("heading", { name: "Keybindings" })).toBeTruthy();
  expect(
    within(nav).getByRole("button", { name: "Keybindings" }).getAttribute("aria-current"),
  ).toBe("page");
});

it("空分区和读取错误仍可导航，供应商表单使用可收缩的单列布局", () => {
  useSettings.setState({ docs: { global: doc } });
  const view = openDialog();
  const nav = within(view.getByRole("navigation"));
  fireEvent.click(nav.getByRole("button", { name: "Tools" }));
  expect(view.getByText("当前分区暂无可编辑的配置项。")).toBeTruthy();
  fireEvent.click(nav.getByRole("button", { name: "Providers" }));
  const input = view.getByPlaceholderText("https://api.example.com/v1");
  expect(input.parentElement?.className).toContain("min-w-0");
  expect(input.parentElement?.parentElement?.className).toContain("grid-cols-1");
  expect(view.getByTitle(doc.path).className).toContain("min-w-0");
  fireEvent.change(view.getByPlaceholderText("搜索设置"), { target: { value: "no-match" } });
  expect(view.queryByPlaceholderText("https://api.example.com/v1")).toBeNull();
});

it("加载失败或未连接时保留完整分类导航", () => {
  useSettings.setState({ errors: { global: "offline" } });
  const view = openDialog();
  const nav = within(view.getByRole("navigation"));
  fireEvent.click(nav.getByRole("button", { name: "Providers" }));
  expect(view.getByText(/offline/)).toBeTruthy();
  fireEvent.click(nav.getByRole("button", { name: "General" }));
  expect(view.getByRole("button", { name: "Dark" })).toBeTruthy();
});
