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

// ---------- /fs/raw 与 /pty：令牌拼 query，与 /ws 同一套校验 ----------

/** 拼上令牌的 HTTP 地址；path 之外的参数调用方给。 */
export function authedUrl(path: string, params: Record<string, string>): string {
  const query = new URLSearchParams({ token: accessToken ?? "", ...params });
  return `${path}?${query.toString()}`;
}

/** `/fs/raw` 下载/预览地址。 */
export function rawFileUrl(path: string, download = false): string {
  return authedUrl("/fs/raw", download ? { path, download: "1" } : { path });
}

export type PtyInfo = {
  id: number;
  cwd: string;
  shell: string;
  created_at_ms: number;
  exit_code: number | null;
  exited: boolean;
};

/** `GET /pty/list`：cwd 子树下的终端（服务端按创建时间排序）。 */
export async function listPty(cwd: string): Promise<PtyInfo[]> {
  const response = await fetch(authedUrl("/pty/list", { cwd }));
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return (await response.json()) as PtyInfo[];
}

/** `POST /pty`：新建终端。 */
export async function createPty(cwd: string, cols = 120, rows = 32): Promise<PtyInfo> {
  const response = await fetch(authedUrl("/pty", {}), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ cwd, cols, rows }),
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return (await response.json()) as PtyInfo;
}

/** `DELETE /pty/{id}`。 */
export async function deletePty(id: number): Promise<void> {
  const response = await fetch(authedUrl(`/pty/${id}`, {}), { method: "DELETE" });
  if (!response.ok && response.status !== 404) throw new Error(`HTTP ${response.status}`);
}

/** `/pty/{id}/ws` 的 ws(s) 地址。 */
export function ptyWsUrl(id: number): string {
  const protocol = window.location.protocol === "https:" ? "wss" : "ws";
  return `${protocol}://${window.location.host}/pty/${id}/ws?token=${encodeURIComponent(accessToken ?? "")}`;
}

/** `PUT /fs/raw` 流式上传；409 返回 false，其余错误抛出。 */
export async function uploadFile(path: string, file: Blob, overwrite = false): Promise<boolean> {
  const response = await fetch(
    authedUrl("/fs/raw", overwrite ? { path, overwrite: "1" } : { path }),
    { method: "PUT", body: file },
  );
  if (response.status === 409) return false;
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return true;
}
