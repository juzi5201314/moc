// `/doctor`：环境诊断——连接、协议、服务端事实（/doctor.json）与浏览器侧检查，
// 每行一项，异常行给出修复建议。对应 TUI 的终端能力诊断。

import { CheckCircle2Icon, CircleAlertIcon, Loader2Icon } from "lucide-react";
import { useEffect, useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useT } from "@/lib/i18n";
import { type DoctorInfo, fetchDoctor } from "@/lib/web-api";
import { rpc } from "@/protocol/client";
import type { PingResult, WorkspaceGitBranchResult } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { useUi } from "@/store/ui";

type Row = { label: string; ok: boolean | null; text: string };

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function DoctorDialog() {
  const t = useT();
  const server = useConnection((state) => state.server);
  const status = useConnection((state) => state.status);
  const handshakeError = useConnection((state) => state.handshakeError);
  const config = useConnection((state) => state.config);
  const [info, setInfo] = useState<DoctorInfo | null>(null);
  const [rttMs, setRttMs] = useState<number | null>(null);
  const [branch, setBranch] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    void fetchDoctor()
      .then((result) => alive && setInfo(result))
      .catch((failure: unknown) => alive && setError(errorText(failure)));
    const at = Date.now();
    void rpc
      .call<PingResult>("ping", {})
      .then(() => alive && setRttMs(Date.now() - at))
      .catch(() => undefined);
    const cwd = useConnection.getState().config?.cwd;
    if (cwd) {
      void rpc
        .call<WorkspaceGitBranchResult>("workspace/git_branch", { cwd })
        .then(
          (result) =>
            alive &&
            setBranch(
              result.branch ??
                (result.detached
                  ? result.head_oid
                    ? `detached · ${result.head_oid}`
                    : "HEAD"
                  : null),
            ),
        )
        .catch(() => undefined);
    }
    return () => {
      alive = false;
    };
  }, []);

  const online = status === "online";
  const featureStates = config?.features ?? {};
  const features = Object.values(featureStates).filter(Boolean).length;
  const rows: (Row | null)[] = [
    {
      label: t("dr.conn"),
      ok: online,
      text: handshakeError
        ? t("sb.handshake_failed", { error: handshakeError.message })
        : online
          ? t("dr.ws_ok", { rtt: rttMs !== null ? t("dr.ping", { n: rttMs }) : "" })
          : t("dr.waiting"),
    },
    {
      label: t("dr.proto"),
      ok: online ? true : null,
      text: server
        ? t("dr.server_info", {
            name: server.server.name,
            version: server.server.version,
            proto: server.protocol_version,
          })
        : t("dr.no_handshake"),
    },
    info && {
      label: t("dr.runtime"),
      ok: true,
      text: `${info.os}/${info.arch} · moc web ${info.version}`,
    },
    info && {
      label: t("dr.config_dir"),
      ok: true,
      text: info.configHome,
    },
    info && {
      label: "Git",
      ok: info.gitVersion !== null,
      text: info.gitVersion
        ? `${info.gitVersion}${branch ? t("dr.git_branch", { cwd: config?.cwd ?? "", branch }) : ""}`
        : t("dr.no_git"),
    },
    info && {
      label: t("dr.sandbox"),
      ok: info.sandbox.runner !== null ? true : null,
      text: info.sandbox.runner
        ? `${info.sandbox.runner} · ${t("dr.sandbox_modes", { modes: info.sandbox.modes.join(", ") })}${info.sandbox.homeOverlay ? t("dr.home_overlay") : ""}`
        : t("dr.no_sandbox", { modes: info.sandbox.modes.join(", ") }),
    },
    {
      label: t("dr.browser"),
      ok: navigator.onLine ? true : null,
      text: `${navigator.language} · ${navigator.onLine ? t("dr.online") : t("dr.offline")}`,
    },
    config && {
      label: t("dr.features"),
      ok: true,
      text: t("dr.features_text", {
        on: features,
        total: Object.keys(featureStates).length,
        models: config.models.length,
      }),
    },
  ];

  return (
    <Dialog open onOpenChange={(open) => !open && useUi.getState().set({ doctor: false })}>
      <DialogContent className="flex max-h-[85vh] flex-col sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{t("dr.title")}</DialogTitle>
          <DialogDescription>{t("dr.desc")}</DialogDescription>
        </DialogHeader>
        <div className="min-h-0 overflow-y-auto">
          <ul className="flex flex-col gap-2.5">
            {rows
              .filter((row): row is Row => row !== null)
              .map((row) => (
                <li key={row.label} className="flex items-start gap-2.5 text-sm">
                  {row.ok === null ? (
                    <Loader2Icon className="mt-0.5 size-4 shrink-0 animate-spin text-text-tertiary" />
                  ) : row.ok ? (
                    <CheckCircle2Icon className="mt-0.5 size-4 shrink-0 text-success" />
                  ) : (
                    <CircleAlertIcon className="mt-0.5 size-4 shrink-0 text-warn" />
                  )}
                  <span className="w-16 shrink-0 text-muted-foreground">{row.label}</span>
                  <span className="min-w-0 break-words">{row.text}</span>
                </li>
              ))}
          </ul>
          {error && (
            <p role="alert" className="mt-3 text-sm text-destructive">
              {t("dr.fetch_failed", { error })}
            </p>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
