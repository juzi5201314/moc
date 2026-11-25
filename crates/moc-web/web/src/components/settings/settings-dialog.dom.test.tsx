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

it("新建供应商支持预设自动填充、URL 协议校验与 API Key 显隐/env 徽标", () => {
  useSettings.setState({ docs: { global: doc } });
  const view = openDialog();
  const nav = within(view.getByRole("navigation"));
  fireEvent.click(nav.getByRole("button", { name: "Providers" }));

  // 点击 OpenAI 预设自动填入 ID / URL / env:OPENAI_API_KEY，且 env: 引用默认明文并带 ENV 徽标
  fireEvent.click(view.getByRole("button", { name: "OpenAI" }));
  const urlInput = view.getByPlaceholderText("https://api.example.com/v1") as HTMLInputElement;
  expect(urlInput.value).toBe("https://api.openai.com/v1");
  const keyInput = view.getByDisplayValue("env:OPENAI_API_KEY") as HTMLInputElement;
  expect(keyInput.type).toBe("text");
  expect(view.getByText("env")).toBeTruthy();

  // 改为字面密钥后默认隐藏，点击显隐按钮切换为 text
  fireEvent.change(keyInput, { target: { value: "sk-live-secret" } });
  expect(keyInput.type).toBe("password");
  fireEvent.click(view.getByRole("button", { name: "Show API Key" }));
  expect(keyInput.type).toBe("text");

  // 输入缺少协议头的 URL 时前端立即给出错误提示
  fireEvent.change(urlInput, { target: { value: "api.openai.com/v1" } });
  expect(view.getByText("Base URL 必须以 http:// 或 https:// 开头。")).toBeTruthy();
});

it("搜索可直达供应商与模型，详情支持模型钻取、通配默认规则与按需添加参数", () => {
  const providerDoc: SettingsDocument = {
    ...doc,
    document: {
      providers: {
        acme: {
          api: "openai_responses",
          base_url: "https://acme.example/v1",
          models: {
            "gpt-5": {
              context_window: 400000,
              reasoning: true,
              thinking_levels: ["low", "high"],
            },
          },
        },
      },
      defaults: {
        models: {
          "gpt-*": { context_window: 200000 },
        },
      },
    },
    effective: {
      providers: {
        acme: {
          api: "openai_responses",
          base_url: "https://acme.example/v1",
          models: {
            "gpt-5": {
              context_window: 400000,
              reasoning: true,
              thinking_levels: ["low", "high"],
            },
          },
        },
      },
      defaults: {
        models: {
          "gpt-*": { context_window: 200000 },
        },
      },
    },
    fields: [
      {
        tab: "providers",
        path: ["providers", "acme", "api"],
        label: "API",
        description: "Protocol",
        kind: {
          type: "choice",
          options: [
            { value: "openai_chat", label: "OpenAI Chat" },
            { value: "openai_responses", label: "OpenAI Responses" },
          ],
        },
        advanced: false,
      },
      {
        tab: "providers",
        path: ["providers", "acme", "models", "gpt-5", "context_window"],
        label: "context_window",
        description: "Context window",
        kind: { type: "integer" },
        advanced: false,
      },
      {
        tab: "providers",
        path: ["providers", "acme", "models", "gpt-5", "thinking_levels"],
        label: "thinking_levels",
        description: "Thinking levels",
        kind: { type: "strings" },
        advanced: false,
      },
      {
        tab: "providers",
        path: ["providers", "acme", "models", "gpt-5", "pricing"],
        label: "pricing",
        description: "Pricing",
        kind: { type: "json" },
        advanced: false,
      },
      {
        tab: "providers",
        path: [
          "providers",
          "acme",
          "models",
          "gpt-5",
          "compat",
          "extra_body",
          "reasoning",
          "summary",
        ],
        label: "summary",
        description: "Reasoning summary",
        kind: { type: "text" },
        advanced: true,
      },
    ],
  };
  useSettings.setState({ docs: { global: providerDoc } });
  const view = openDialog();

  // 全局搜索命中供应商与模型并直接跳转至模型编辑器
  fireEvent.change(view.getByPlaceholderText("搜索设置"), { target: { value: "gpt-5" } });
  fireEvent.click(view.getByRole("button", { name: /acme \/ gpt-5/ }));
  expect(view.getByRole("heading", { name: "gpt-5" })).toBeTruthy();
  // 定价结构化 4 格输入存在，不再是 JSON 文本框
  expect(view.getByLabelText("pricing input")).toBeTruthy();
  // 思考等级芯片可见（已选展示为芯片，未选内置等级可点击添加）
  expect(view.getByText("high")).toBeTruthy();
  expect(view.getByRole("button", { name: /medium/ })).toBeTruthy();

  // 切到 Params 子页：未配置参数默认折叠，点击 Add parameter 后可在列表中找到
  fireEvent.click(view.getByRole("button", { name: "Params & Headers" }));
  expect(view.queryByText("Reasoning summary")).toBeNull();
  expect(view.getByRole("button", { name: /Add parameter/ })).toBeTruthy();

  // 切到通配默认规则视图可见已有规则
  fireEvent.click(view.getByRole("button", { name: /Defaults/ }));
  expect(view.getByText("gpt-*")).toBeTruthy();
});
