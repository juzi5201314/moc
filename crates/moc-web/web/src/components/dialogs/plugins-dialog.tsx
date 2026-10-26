// 插件对话框：`plugin/list` 列出名称、版本、来源、目录、工具、命令与错误；「重载」调 `plugin/reload`。

import { RefreshCw } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { listPlugins, reloadPlugins } from "@/components/composer/commands";
import { runAction } from "@/components/layout/session-actions";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { RpcError } from "@/protocol/errors";
import type { PluginInfo } from "@/protocol/types";
import { refreshSideData } from "@/store/lifecycle";
import { useUi } from "@/store/ui";

export function PluginsDialog() {
  const sessionId = useUi((state) => state.plugins);
  const set = useUi((state) => state.set);
  const [plugins, setPlugins] = useState<PluginInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async (id: string) => {
    setError(null);
    try {
      setPlugins(await listPlugins(id));
    } catch (err) {
      setError(err instanceof RpcError ? err.message : String(err));
    }
  }, []);

  useEffect(() => {
    setPlugins(null);
    if (sessionId) void load(sessionId);
  }, [sessionId, load]);

  const reload = () => {
    if (!sessionId) return;
    void runAction(async () => {
      await reloadPlugins(sessionId);
      await load(sessionId);
      await refreshSideData(sessionId);
    });
  };

  return (
    <Dialog open={sessionId !== null} onOpenChange={(open) => !open && set({ plugins: null })}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>插件</DialogTitle>
          <DialogDescription>当前会话工作目录下加载的插件。</DialogDescription>
        </DialogHeader>
        <div className="max-h-[60vh] space-y-3 overflow-y-auto">
          {error ? <p className="text-destructive text-sm">{error}</p> : null}
          {plugins === null && !error ? (
            <p className="text-muted-foreground text-sm">加载中…</p>
          ) : null}
          {plugins?.length === 0 ? <p className="text-muted-foreground text-sm">没有插件</p> : null}
          {plugins?.map((plugin) => (
            <section key={`${plugin.source}:${plugin.name}`} className="rounded-lg border p-3">
              <header className="flex flex-wrap items-center gap-2">
                <span className="font-medium">{plugin.name}</span>
                {plugin.version ? <Badge variant="secondary">{plugin.version}</Badge> : null}
                <Badge variant="outline">{plugin.source}</Badge>
                {plugin.error ? <Badge variant="destructive">已禁用</Badge> : null}
              </header>
              {plugin.description ? (
                <p className="mt-1 text-muted-foreground text-sm">{plugin.description}</p>
              ) : null}
              <p
                className="mt-1 truncate font-mono text-muted-foreground text-xs"
                title={plugin.dir}
              >
                {plugin.dir}
              </p>
              {plugin.tools.length > 0 ? (
                <div className="mt-2 text-sm">
                  <span className="text-muted-foreground">工具：</span>
                  {plugin.tools.map((tool) => tool.name).join("、")}
                </div>
              ) : null}
              {plugin.commands.length > 0 ? (
                <div className="mt-1 text-sm">
                  <span className="text-muted-foreground">命令：</span>
                  {plugin.commands.map((command) => `/${command.name}`).join("、")}
                </div>
              ) : null}
              {plugin.error ? (
                <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap rounded-md bg-muted p-2 font-mono text-destructive text-xs">
                  {plugin.error}
                </pre>
              ) : null}
            </section>
          ))}
        </div>
        <div className="flex justify-end">
          <Button variant="outline" onClick={reload} disabled={!sessionId}>
            <RefreshCw aria-hidden />
            重载
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}
