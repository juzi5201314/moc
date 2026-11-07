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
        .then((result) => alive && setBranch(result.branch ?? null))
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
      label: "连接",
      ok: online,
      text: handshakeError
        ? `握手失败：${handshakeError.message}`
        : online
          ? `WebSocket 已连接${rttMs !== null ? `（ping ${rttMs}ms）` : ""}`
          : "未连接，等待重连…",
    },
    {
      label: "协议",
      ok: online ? true : null,
      text: server
        ? `服务端 ${server.server.name} ${server.server.version} · 协议 ${server.protocol_version}`
        : "尚未完成握手",
    },
    info && {
      label: "运行环境",
      ok: true,
      text: `${info.os}/${info.arch} · moc web ${info.version}`,
    },
    info && {
      label: "配置目录",
      ok: true,
      text: info.configHome,
    },
    info && {
      label: "Git",
      ok: info.gitVersion !== null,
      text: info.gitVersion
        ? `${info.gitVersion}${branch ? ` · ${config?.cwd ?? ""} 在 ${branch} 分支` : ""}`
        : "未找到 git，工作区变更检测与 diff 将不可用",
    },
    info && {
      label: "沙盒",
      ok: info.sandbox.runner !== null ? true : null,
      text: info.sandbox.runner
        ? `${info.sandbox.runner} · 可用模式 ${info.sandbox.modes.join(", ")}${info.sandbox.homeOverlay ? " · home overlay" : ""}`
        : `未探测到沙盒 runner，仅完全访问（${info.sandbox.modes.join(", ")}）`,
    },
    {
      label: "浏览器",
      ok: navigator.onLine ? true : null,
      text: `${navigator.language} · ${navigator.onLine ? "在线" : "离线"}`,
    },
    config && {
      label: "特性",
      ok: true,
      text: `${features}/${Object.keys(featureStates).length} 项开启 · 可用模型 ${config.models.length} 个`,
    },
  ];

  return (
    <Dialog open onOpenChange={(open) => !open && useUi.getState().set({ doctor: false })}>
      <DialogContent className="flex max-h-[85vh] flex-col sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>环境诊断</DialogTitle>
          <DialogDescription>连接、服务端与平台能力逐项检查。</DialogDescription>
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
              服务端环境读取失败：{error}
            </p>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
