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
import { type MsgKey, useT } from "@/lib/i18n";
import { type FeatureRow, fetchFeatures } from "@/lib/web-api";
import { useUi } from "@/store/ui";

const STAGE_LABEL: Record<string, MsgKey> = {
  dev: "feat.dev",
  experimental: "feat.experimental",
  beta: "feat.beta",
  stable: "feat.stable",
  deprecated: "feat.deprecated",
};

export function FeaturesDialog() {
  const t = useT();
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
          <DialogTitle>{t("feat.title")}</DialogTitle>
          <DialogDescription>{t("feat.desc")}</DialogDescription>
        </DialogHeader>
        <div className="min-h-0 overflow-y-auto">
          {error ? (
            <p role="alert" className="text-sm text-destructive">
              {t("dr.fetch_failed", { error })}
            </p>
          ) : features === null ? (
            <p role="status" className="text-sm text-muted-foreground">
              {t("feat.reading")}
            </p>
          ) : (
            <table className="w-full text-sm">
              <thead>
                <tr className="text-left text-2xs text-text-tertiary">
                  <th className="pb-2 font-medium">{t("feat.name")}</th>
                  <th className="pb-2 font-medium">{t("feat.stage")}</th>
                  <th className="pb-2 font-medium">{t("feat.status")}</th>
                  <th className="pb-2 font-medium">{t("feat.cost")}</th>
                  <th className="pb-2 font-medium">{t("feat.note")}</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-border/60">
                {features.map((row) => (
                  <tr key={row.key}>
                    <td className="py-2 pr-3 font-mono text-xs">{row.key}</td>
                    <td className="py-2 pr-3">
                      <Badge variant="secondary" className="font-normal">
                        {STAGE_LABEL[row.stage] ? t(STAGE_LABEL[row.stage]) : row.stage}
                      </Badge>
                    </td>
                    <td className="py-2 pr-3">
                      <Badge variant={row.enabled ? "brand" : "outline"} className="font-normal">
                        {row.enabled ? t("feat.on") : t("feat.off")}
                      </Badge>
                    </td>
                    <td className="py-2 pr-3 whitespace-nowrap text-xs text-muted-foreground">
                      {row.contextTokens !== null ? `~${row.contextTokens} tok` : t("feat.env")}
                    </td>
                    <td className="py-2 text-xs text-muted-foreground">
                      {row.summary}
                      <span className="ml-1.5 text-text-tertiary">
                        {t("feat.toggle_wrap", { toggle: row.toggle })}
                      </span>
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
