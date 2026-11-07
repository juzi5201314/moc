// `/features`：特性注册表一览——阶段、有效状态、上下文成本与开关来源，
// 与 TUI `/features`（`format_features`）同一份注册表事实。

import { useEffect, useState } from "react";
import { Badge } from "@/components/ui/badge";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { type FeatureRow, fetchFeatures } from "@/lib/web-api";
import { useUi } from "@/store/ui";

const STAGE_LABEL: Record<string, string> = {
  dev: "开发中",
  experimental: "实验",
  beta: "公测",
  stable: "稳定",
  deprecated: "弃用",
};

export function FeaturesDialog() {
  const [features, setFeatures] = useState<FeatureRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    void fetchFeatures()
      .then((result) => alive && setFeatures(result.features))
      .catch(
        (failure: unknown) =>
          alive && setError(failure instanceof Error ? failure.message : String(failure)),
      );
    return () => {
      alive = false;
    };
  }, []);

  return (
    <Dialog open onOpenChange={(open) => !open && useUi.getState().set({ features: false })}>
      <DialogContent className="flex max-h-[85vh] flex-col sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>特性开关</DialogTitle>
          <DialogDescription>生效值来自 [features] 覆盖或对应配置段。</DialogDescription>
        </DialogHeader>
        <div className="min-h-0 overflow-y-auto">
          {error ? (
            <p role="alert" className="text-sm text-destructive">
              读取失败：{error}
            </p>
          ) : features === null ? (
            <p role="status" className="text-sm text-muted-foreground">
              正在读取特性表…
            </p>
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="text-left text-2xs text-text-tertiary">
                  <th className="pb-2 font-medium">特性</th>
                  <th className="pb-2 font-medium">阶段</th>
                  <th className="pb-2 font-medium">状态</th>
                  <th className="pb-2 font-medium">上下文成本</th>
                  <th className="pb-2 font-medium">说明</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-border/60">
                {features.map((row) => (
                  <tr key={row.key}>
                    <td className="py-2 pr-3 font-mono text-xs">{row.key}</td>
                    <td className="py-2 pr-3">
                      <Badge variant="secondary" className="font-normal">
                        {STAGE_LABEL[row.stage] ?? row.stage}
                      </Badge>
                    </td>
                    <td className="py-2 pr-3">
                      <Badge variant={row.enabled ? "brand" : "outline"} className="font-normal">
                        {row.enabled ? "开" : "关"}
                      </Badge>
                    </td>
                    <td className="py-2 pr-3 whitespace-nowrap text-xs text-muted-foreground">
                      {row.contextTokens !== null ? `~${row.contextTokens} tok` : "随环境"}
                    </td>
                    <td className="py-2 text-xs text-muted-foreground">
                      {row.summary}
                      <span className="ml-1.5 text-text-tertiary">（{row.toggle}）</span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
