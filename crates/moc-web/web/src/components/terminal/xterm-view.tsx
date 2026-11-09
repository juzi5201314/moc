// xterm 附着视图：连接 /pty/{id}/ws，回放后实时输出；输入 binary 帧，
// resize 走 text 控制帧。意外断开按退避重连（重连前 term.reset 再收回放）。
// 切走时 dispose，重附着靠服务端滚动缓冲恢复画面。

import "@xterm/xterm/css/xterm.css";
import { useEffect, useRef, useState } from "react";
import type { PtyInfo } from "@/lib/web-api";
import { ptyWsUrl } from "@/lib/web-api";
import { useTerminals } from "@/store/terminal";

export default function XtermView({ terminal }: { terminal: PtyInfo }) {
  const hostRef = useRef<HTMLDivElement>(null);
  // 登录 shell（zshrc/p10k 等）要几秒才吐第一个字节；期间给个占位免得看起来像坏了。
  const [starting, setStarting] = useState(true);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    let disposed = false;
    let term: import("@xterm/xterm").Terminal | null = null;
    let fit: import("@xterm/addon-fit").FitAddon | null = null;
    let ws: WebSocket | null = null;
    let observer: ResizeObserver | null = null;
    let resizeTimer: ReturnType<typeof setTimeout> | undefined;
    let retryTimer: ReturnType<typeof setTimeout> | undefined;
    let retries = 0;
    // 进程退出后不再重连：服务端发完 exit 就关连接，重连只会拿到「回放+exit」循环刷屏。
    let exited = false;
    let themeObserver: MutationObserver | null = null;

    const theme = () => {
      const styles = getComputedStyle(document.documentElement);
      const read = (name: string, fallback: string) =>
        styles.getPropertyValue(name).trim() || fallback;
      const dark = document.documentElement.classList.contains("dark");
      return {
        background: read("--background", dark ? "#161618" : "#ffffff"),
        foreground: read("--foreground", dark ? "#ededef" : "#171717"),
        cursor: read("--brand", dark ? "#6d97ff" : "#3b6ff5"),
        cursorAccent: read("--background", dark ? "#161618" : "#ffffff"),
        selectionBackground: read("--selection", "rgba(59,111,245,.2)"),
        // ANSI 16 色：浅色偏暖灰，深色提亮一档，都保持低饱和。
        black: dark ? "#1b1b1e" : "#3f3f46",
        red: read("--destructive", dark ? "#f26161" : "#dc2626"),
        green: dark ? "#3fc56f" : "#16a34a",
        yellow: dark ? "#f0a33a" : "#d97706",
        blue: dark ? "#6d97ff" : "#3b6ff5",
        magenta: dark ? "#c084fc" : "#9333ea",
        cyan: dark ? "#34d5d5" : "#0e9f9f",
        white: dark ? "#a0a0a8" : "#71717a",
        brightBlack: dark ? "#6e6e76" : "#9a9aa0",
        brightRed: dark ? "#ff8a8a" : "#ef4444",
        brightGreen: dark ? "#6fe094" : "#22c55e",
        brightYellow: dark ? "#fbbf4f" : "#eab308",
        brightBlue: dark ? "#93b4ff" : "#5a86f7",
        brightMagenta: dark ? "#d4a8fd" : "#a855f7",
        brightCyan: dark ? "#5fe3e3" : "#14b8b8",
        brightWhite: dark ? "#ededef" : "#18181b",
      };
    };

    const connect = () => {
      if (disposed || !term) return;
      ws = new WebSocket(ptyWsUrl(terminal.id));
      ws.binaryType = "arraybuffer";
      ws.onopen = () => {
        retries = 0;
        sendResize();
      };
      ws.onmessage = (event) => {
        if (typeof event.data === "string") {
          try {
            const control = JSON.parse(event.data) as { type?: string; code?: number | null };
            if (control.type === "exit") {
              exited = true;
              // 同步进 store，标签立刻显示「已退出 (code)」。
              const code = control.code ?? null;
              useTerminals.setState((state) => {
                const current = state.byCwd[terminal.cwd];
                if (!current) return state;
                return {
                  byCwd: {
                    ...state.byCwd,
                    [terminal.cwd]: {
                      ...current,
                      items: current.items.map((item) =>
                        item.id === terminal.id ? { ...item, exited: true, exit_code: code } : item,
                      ),
                    },
                  },
                };
              });
              term?.writeln(`\r\n\x1b[2m进程已退出${code !== null ? ` (${code})` : ""}\x1b[0m`);
            }
          } catch {
            // 非 JSON text 帧按纯文本忽略
          }
          setStarting(false);
          return;
        }
        setStarting(false);
        term?.write(new Uint8Array(event.data as ArrayBuffer));
      };
      ws.onclose = () => {
        ws = null;
        if (disposed || exited) return;
        // 非主动断开的重连：先清屏再等服务端回放，避免重复输出。
        const delay = Math.min(4000, 300 * 2 ** retries);
        retries += 1;
        retryTimer = setTimeout(() => {
          term?.reset();
          connect();
        }, delay);
      };
    };

    const sendResize = () => {
      if (!fit || ws?.readyState !== WebSocket.OPEN) return;
      const dims = fit.proposeDimensions();
      if (dims) ws.send(JSON.stringify({ type: "resize", cols: dims.cols, rows: dims.rows }));
    };

    void (async () => {
      const [{ Terminal }, { FitAddon }, { WebLinksAddon }] = await Promise.all([
        import("@xterm/xterm"),
        import("@xterm/addon-fit"),
        import("@xterm/addon-web-links"),
      ]);
      if (disposed) return;
      term = new Terminal({
        fontFamily: '"Geist Mono Variable", ui-monospace, monospace',
        fontSize: 13,
        lineHeight: 1.35,
        cursorBlink: true,
        scrollback: 10000,
        theme: theme(),
      });
      fit = new FitAddon();
      term.loadAddon(fit);
      term.loadAddon(new WebLinksAddon());
      term.open(host);
      fit.fit();
      term.onData((data) => {
        if (ws?.readyState === WebSocket.OPEN) ws.send(new TextEncoder().encode(data));
      });
      term.onResize(() => sendResize());
      connect();

      observer = new ResizeObserver(() => {
        clearTimeout(resizeTimer);
        resizeTimer = setTimeout(() => {
          fit?.fit();
          sendResize();
        }, 80);
      });
      observer.observe(host);

      // 深浅色切换实时换主题。
      themeObserver = new MutationObserver(() => {
        if (term) term.options.theme = theme();
      });
      themeObserver.observe(document.documentElement, {
        attributes: true,
        attributeFilter: ["class"],
      });
    })();

    return () => {
      disposed = true;
      clearTimeout(resizeTimer);
      clearTimeout(retryTimer);
      observer?.disconnect();
      themeObserver?.disconnect();
      ws?.close();
      term?.dispose();
    };
  }, [terminal.id, terminal.cwd]);

  return (
    <div className="relative h-full min-h-0 px-3 py-2">
      <div ref={hostRef} className="h-full min-h-0 [&_.xterm]:h-full" />
      {starting && (
        <p className="absolute top-4 left-6 text-xs text-text-tertiary">正在连接终端…</p>
      )}
    </div>
  );
}
