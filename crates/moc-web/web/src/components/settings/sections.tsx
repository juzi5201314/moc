// 设置里需要定制界面的分区：通用（webui 前端偏好）、快捷键、模型角色、插件、会话存储维护。

import { Loader2Icon, MonitorIcon, MoonIcon, RefreshCwIcon, SunIcon } from "lucide-react";
import { type ReactNode, useEffect, useState } from "react";
import { toast } from "sonner";
import { Kbd } from "@/components/common/icon-button";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import {
  FUSION_ALIAS,
  groupByProvider,
  levelLabel,
  modelName,
  parseFusion,
  parseSelector,
} from "@/lib/models";
import { SHORTCUTS } from "@/lib/shortcuts";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  ConfigAction,
  ConfigField,
  ConfigReply,
  MaintenanceReport,
  PluginInfo,
  PluginListResult,
  SettingsDocument,
} from "@/protocol/types";
import { selectModels, useConnection } from "@/store/connection";
import { type Theme, usePrefs } from "@/store/prefs";
import { useTabs } from "@/store/tabs";
import { fieldValues, SettingRow } from "./field";
import { writeField } from "./settings-store";

const INHERIT = "__inherit__";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`;
}

export function Card({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div
      className={cn(
        "divide-y divide-border-subtle rounded-xl border border-border bg-card px-4",
        className,
      )}
    >
      {children}
    </div>
  );
}

function Segmented<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T;
  options: { value: T; label: string; icon?: ReactNode }[];
  onChange: (value: T) => void;
}) {
  return (
    <div className="ml-auto flex w-fit rounded-lg bg-muted p-0.5">
      {options.map((option) => (
        <button
          key={option.value}
          type="button"
          onClick={() => onChange(option.value)}
          className={cn(
            "inline-flex h-7 items-center gap-1.5 rounded-md px-2.5 text-[12.5px] transition-colors",
            value === option.value
              ? "bg-background text-foreground shadow-xs"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {option.icon}
          {option.label}
        </button>
      ))}
    </div>
  );
}

// ---------- 通用 ----------

export function GeneralSection() {
  const prefs = usePrefs();
  const server = useConnection((state) => state.server);
  const cwd = useConnection((state) => state.config?.cwd);

  const toggleNotify = async (on: boolean) => {
    if (!on) {
      prefs.set({ notify: false });
      return;
    }
    if (typeof Notification === "undefined") {
      toast.error("当前浏览器不支持桌面通知");
      return;
    }
    const permission =
      Notification.permission === "default"
        ? await Notification.requestPermission()
        : Notification.permission;
    if (permission !== "granted") {
      toast.error("浏览器拒绝了通知权限，请在站点设置里允许");
      return;
    }
    prefs.set({ notify: true });
  };

  return (
    <div className="space-y-6">
      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">外观</h3>
        <Card>
          <SettingRow label="主题" description="跟随系统，或固定浅色 / 深色。">
            <Segmented<Theme>
              value={prefs.theme}
              onChange={(theme) => prefs.set({ theme })}
              options={[
                { value: "system", label: "系统", icon: <MonitorIcon className="size-3.5" /> },
                { value: "light", label: "浅色", icon: <SunIcon className="size-3.5" /> },
                { value: "dark", label: "深色", icon: <MoonIcon className="size-3.5" /> },
              ]}
            />
          </SettingRow>
          <SettingRow
            label="展开思考过程"
            description="默认展开模型的思考内容；也可以随时按 Alt+T 切换。"
          >
            <div className="flex justify-end">
              <Switch
                checked={prefs.expandThinking}
                onCheckedChange={(value) => prefs.set({ expandThinking: value })}
              />
            </div>
          </SettingRow>
          <SettingRow label="差异视图" description="文件变更的展示方式。">
            <Segmented
              value={prefs.diffMode}
              onChange={(diffMode) => prefs.set({ diffMode })}
              options={[
                { value: "unified", label: "统一" },
                { value: "split", label: "并排" },
              ]}
            />
          </SettingRow>
        </Card>
      </section>

      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">交互</h3>
        <Card>
          <SettingRow
            label="生成中按回车"
            description="补充：把消息并入当前这一轮；中断：停下当前生成，再把消息作为下一轮发送。"
          >
            <Segmented
              value={prefs.runningEnter}
              onChange={(runningEnter) => prefs.set({ runningEnter })}
              options={[
                { value: "steer", label: "补充" },
                { value: "interrupt", label: "中断" },
              ]}
            />
          </SettingRow>
          <SettingRow
            label="桌面通知"
            description="页面在后台时，有授权请求或提问需要处理会弹出系统通知。"
          >
            <div className="flex justify-end">
              <Switch
                checked={prefs.notify}
                onCheckedChange={(value) => void toggleNotify(value)}
              />
            </div>
          </SettingRow>
        </Card>
        <p className="px-1 text-[11.5px] text-text-tertiary">
          以上偏好保存在 web.toml，对所有连接到此 moc 的浏览器生效。
        </p>
      </section>

      <section className="space-y-2">
        <h3 className="text-xs font-medium text-text-tertiary">关于</h3>
        <Card className="py-1">
          <InfoRow
            label="服务"
            value={server ? `${server.server.name} ${server.server.version}` : "未连接"}
          />
          <InfoRow label="协议版本" value={server?.protocol_version ?? "—"} />
          <InfoRow label="Core 工作目录" value={cwd ?? "—"} mono />
        </Card>
      </section>
    </div>
  );
}

function InfoRow({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-center justify-between gap-6 py-2.5 text-[13px]">
      <span className="shrink-0 text-muted-foreground">{label}</span>
      <span
        className={cn("min-w-0 truncate text-foreground", mono && "font-mono text-[12px]")}
        title={value}
      >
        {value}
      </span>
    </div>
  );
}

// ---------- 快捷键 ----------

export function ShortcutsSection() {
  return (
    <Card className="py-1">
      {SHORTCUTS.map((item) => (
        <div key={item.keys} className="flex items-center justify-between py-2.5 text-[13px]">
          <span className="text-foreground">{item.label}</span>
          <span className="flex gap-1">
            {item.keys.split(" ").map((key) => (
              <Kbd key={key} className="h-5 min-w-5 px-1.5 text-[11px]">
                {key}
              </Kbd>
            ))}
          </span>
        </div>
      ))}
    </Card>
  );
}

// ---------- 模型角色 ----------

/** 模型 selector 编辑器：模型下拉 + 思考强度下拉，写成 `provider/model[:level]`。 */
function ModelSelectorRow({ field, doc }: { field: ConfigField; doc: SettingsDocument }) {
  const models = useConnection(selectModels);
  const { explicit, fallback } = fieldValues(doc, field.path);
  const selector = typeof explicit === "string" ? explicit : "";
  const parsed = parseSelector(selector, models);
  const fallbackText = typeof fallback === "string" && fallback ? fallback : "";
  const levels = parsed.model?.thinking_levels ?? [];
  // roles.default 与 roles.plan 可以选 `models.fusion` 组合；
  // 值是 fusion 别名或规范 fusion: 组合时都显示为 Fusion 而不是「未找到」。
  const fusionSelectable =
    field.path[1] === "roles" && (field.path[2] === "default" || field.path[2] === "plan");
  const fusionCurrent =
    fusionSelectable && (selector === FUSION_ALIAS || parseFusion(selector) !== null);
  const unknown = selector && !parsed.model && !fusionCurrent;

  const write = (id: string, level?: string) =>
    void writeField(field.path, id ? (level ? `${id}:${level}` : id) : undefined);

  return (
    <SettingRow
      label={field.label}
      description={field.description}
      title={field.path.join(".")}
      overridden={explicit !== undefined}
      onReset={() => write("")}
    >
      <div className="flex gap-1.5">
        <Select
          value={selector ? (fusionCurrent ? FUSION_ALIAS : parsed.id) : INHERIT}
          onValueChange={(value) => write(value === INHERIT ? "" : value)}
        >
          <SelectTrigger
            size="sm"
            className="h-8 min-w-0 flex-1 rounded-lg text-[13px] shadow-none"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value={INHERIT}>
              {fallbackText ? `默认（${modelName(fallbackText)}）` : "默认"}
            </SelectItem>
            {fusionSelectable && (
              <SelectItem value={FUSION_ALIAS}>Fusion（models.fusion 组合）</SelectItem>
            )}
            {unknown && <SelectItem value={parsed.id}>{parsed.id}（未找到）</SelectItem>}
            {groupByProvider(models).map(([provider, list]) => (
              <SelectGroup key={provider}>
                <SelectLabel className="text-[11px]">{provider}</SelectLabel>
                {list.map((model) => (
                  <SelectItem key={model.id} value={model.id}>
                    {modelName(model.id)}
                  </SelectItem>
                ))}
              </SelectGroup>
            ))}
          </SelectContent>
        </Select>
        {levels.length > 0 && (
          <Select
            value={parsed.level ?? INHERIT}
            onValueChange={(value) => write(parsed.id, value === INHERIT ? undefined : value)}
          >
            <SelectTrigger
              size="sm"
              className="h-8 w-[84px] shrink-0 rounded-lg text-[13px] shadow-none"
              aria-label="思考强度"
            >
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value={INHERIT}>默认</SelectItem>
              {levels.map((level) => (
                <SelectItem key={level} value={level}>
                  {levelLabel(level)}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
      </div>
    </SettingRow>
  );
}

export function RolesSection({ doc }: { doc: SettingsDocument }) {
  const roles = doc.fields.filter(
    (field) =>
      field.path[0] === "models" && (field.path[1] === "roles" || field.path[1] === "fusion"),
  );
  return (
    <Card>
      {roles.map((field) => (
        <ModelSelectorRow key={field.path.join(".")} field={field} doc={doc} />
      ))}
    </Card>
  );
}

// ---------- 插件 ----------

export function PluginsOverview() {
  const active = useTabs((state) => state.active);
  // 草稿会话在 core 里不存在：传空 id，由 core 按自身工作目录发现插件。
  const sessionId = active && active !== "draft-session" ? active : "";
  const [plugins, setPlugins] = useState<PluginInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async (reload: boolean) => {
    setBusy(true);
    setError(null);
    try {
      if (reload) await rpc.call("plugin/reload", { session_id: sessionId });
      const result = await rpc.call<PluginListResult>("plugin/list", { session_id: sessionId });
      setPlugins(result.plugins);
      if (reload) toast.success("插件已重新加载");
    } catch (loadError) {
      setError(errorText(loadError));
    } finally {
      setBusy(false);
    }
  };

  // biome-ignore lint/correctness/useExhaustiveDependencies: 进入分区或切换会话时读取一次
  useEffect(() => {
    void load(false);
  }, [sessionId]);

  return (
    <section className="space-y-2">
      <div className="flex items-center justify-between">
        <h3 className="text-xs font-medium text-text-tertiary">已加载 · {plugins?.length ?? 0}</h3>
        <Button size="xs" variant="outline" disabled={busy} onClick={() => void load(true)}>
          {busy ? <Loader2Icon className="animate-spin" /> : <RefreshCwIcon />}
          重新加载
        </Button>
      </div>
      {error && <p className="text-xs text-destructive">{error}</p>}
      <Card>
        {plugins?.length === 0 && (
          <p className="py-6 text-center text-[13px] text-text-tertiary">
            当前工作区没有发现插件。
          </p>
        )}
        {plugins?.map((plugin) => (
          <div key={`${plugin.source}:${plugin.name}`} className="space-y-1.5 py-3">
            <div className="flex items-center gap-2">
              <span
                className={cn(
                  "size-1.5 shrink-0 rounded-full",
                  plugin.error ? "bg-destructive" : "bg-success",
                )}
              />
              <span className="text-[13.5px] font-medium">{plugin.name}</span>
              {plugin.version && (
                <span className="font-mono text-[11px] text-text-tertiary">{plugin.version}</span>
              )}
              <span className="ml-auto shrink-0 text-[11px] text-text-tertiary">
                {plugin.source}
              </span>
            </div>
            {plugin.description && (
              <p className="pl-3.5 text-[12.5px] text-muted-foreground">{plugin.description}</p>
            )}
            {plugin.error && (
              <p className="pl-3.5 text-[12.5px] break-words text-destructive">{plugin.error}</p>
            )}
            {(plugin.tools.length > 0 || plugin.commands.length > 0) && (
              <div className="flex flex-wrap gap-1 pl-3.5">
                {plugin.tools.map((tool) => (
                  <span
                    key={tool.name}
                    title={tool.description}
                    className="rounded bg-muted px-1.5 py-px font-mono text-[11px] text-muted-foreground"
                  >
                    {tool.name}
                  </span>
                ))}
                {plugin.commands.map((command) => (
                  <span
                    key={command.name}
                    title={command.description}
                    className="rounded bg-brand/10 px-1.5 py-px font-mono text-[11px] text-brand"
                  >
                    /{command.name}
                  </span>
                ))}
              </div>
            )}
          </div>
        ))}
      </Card>
    </section>
  );
}

// ---------- 会话存储维护 ----------

export function MaintenanceCard() {
  const [report, setReport] = useState<MaintenanceReport | null>(null);
  const [busy, setBusy] = useState(false);

  const run = async (apply: boolean) => {
    setBusy(true);
    try {
      const action: ConfigAction = { action: "maintenance", apply };
      const reply = await rpc.call<ConfigReply>("config/manage", action);
      if (reply.kind !== "maintenance") throw new Error("config/manage 返回了非 maintenance 响应");
      setReport(reply.data);
      if (apply)
        toast.success(
          `已清理 ${reply.data.removed.length} 项，释放 ${formatBytes(reply.data.bytes)}`,
        );
    } catch (error) {
      toast.error(`清理失败：${errorText(error)}`);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card>
      <SettingRow
        label="清理过期数据"
        description={
          report && !report.applied
            ? `可清理 ${report.removed.length} 项（${formatBytes(report.bytes)}），${report.protected} 项受保护不会删除。`
            : "按上面的保留天数删除过期的临时产物与任务日志；先检查再清理。"
        }
      >
        <div className="flex justify-end gap-1.5">
          <Button size="xs" variant="outline" disabled={busy} onClick={() => void run(false)}>
            {busy && !report && <Loader2Icon className="animate-spin" />}
            检查
          </Button>
          <Button
            size="xs"
            variant="destructive"
            disabled={busy || !report || report.applied || report.removed.length === 0}
            onClick={() => void run(true)}
          >
            清理
          </Button>
        </div>
      </SettingRow>
    </Card>
  );
}
