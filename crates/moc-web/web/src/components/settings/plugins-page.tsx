// 插件页：标签栏里的独立页面，复用设置里的插件概览与 plugins.* 配置字段。

import { Loader2Icon } from "lucide-react";
import { useEffect } from "react";
import { useT } from "@/lib/i18n";
import { useConnection } from "@/store/connection";
import { useTabs } from "@/store/tabs";
import { useTranscript } from "@/store/transcript";
import { FieldList } from "./field";
import { PluginsOverview } from "./sections";
import { SaveStatus, ScopeSwitch } from "./settings-dialog";
import { loadSettings, useSettings } from "./settings-store";

export function PluginsPage() {
  const t = useT();
  const doc = useSettings((state) => state.docs[state.scope]);
  const error = useSettings((state) => state.errors[state.scope]);
  const loading = useSettings((state) => state.loading);
  const activeCwd = useTranscript(
    (state) => state.bySession[useTabs.getState().active ?? ""]?.info.cwd,
  );
  const coreCwd = useConnection((state) => state.config?.cwd);
  const cwd = activeCwd || coreCwd || "";

  useEffect(() => {
    if (cwd) void loadSettings(cwd);
  }, [cwd]);

  const fields = doc?.fields.filter((field) => field.path[0] === "plugins") ?? [];

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <header className="flex h-12 shrink-0 items-center gap-3 px-5">
        <h1 className="text-sm font-medium">{t("sd.plugins")}</h1>
        <div className="ml-auto flex min-w-0 items-center gap-3">
          <SaveStatus />
          <ScopeSwitch doc={doc} />
        </div>
      </header>
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[760px] space-y-8 px-6 pt-6 pb-16">
          <div>
            <h2 className="text-xl font-semibold tracking-tight">{t("sd.plugins")}</h2>
            <p className="mt-1.5 text-sm text-muted-foreground">{t("sd.plugins_desc")}</p>
          </div>
          <PluginsOverview />
          {error ? (
            <p className="rounded-lg bg-destructive/5 px-3 py-2 text-ui text-destructive">
              {t("sd.load_failed", { error })}
            </p>
          ) : !doc ? (
            <div className="flex justify-center py-10 text-text-tertiary">
              {loading ? <Loader2Icon className="size-5 animate-spin" /> : t("sd.not_connected")}
            </div>
          ) : (
            fields.length > 0 && <FieldList fields={fields} doc={doc} />
          )}
        </div>
      </div>
    </div>
  );
}
