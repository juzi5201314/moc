// `moc web` 进程内 JSON 端点的类型化读取：这些路由不进 JSON-RPC，
// 令牌经 ?token= 传递（与 /ws 握手同一套校验）。

import { accessToken } from "@/protocol/client";

async function apiFetch<T>(path: string): Promise<T> {
  const response = await fetch(`${path}?token=${encodeURIComponent(accessToken ?? "")}`);
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return (await response.json()) as T;
}

export type FeatureRow = {
  key: string;
  stage: string;
  enabled: boolean;
  contextTokens: number | null;
  summary: string;
  toggle: string;
};

export const fetchFeatures = () => apiFetch<{ features: FeatureRow[] }>("/features.json");

export type DoctorInfo = {
  version: string;
  os: string;
  arch: string;
  configHome: string;
  sandbox: { runner: string | null; modes: string[]; homeOverlay: boolean };
  gitVersion: string | null;
};

export const fetchDoctor = () => apiFetch<DoctorInfo>("/doctor.json");

export type ProcSample = {
  ageMs: number;
  rssBytes: number;
  cpuPct: number;
  diskReadBytes: number;
  diskWriteBytes: number;
  netRxBytes: number;
  netTxBytes: number;
};

export type ProcStats = {
  intervalSecs: number;
  windowSecs: number;
  samples: ProcSample[];
};

export const fetchProcStats = () => apiFetch<ProcStats>("/procstats.json");
