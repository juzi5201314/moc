// Markdown 渲染：react-markdown + GFM；不渲染原始 HTML。代码块懒加载 shiki 高亮，带复制按钮。

import { Check, Copy } from "lucide-react";
import { memo, useEffect, useState } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { highlight } from "@/lib/highlight";
import { useIsDark } from "@/lib/use-dark";
import { cn } from "@/lib/utils";

export function CopyButton({ text, className }: { text: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      aria-label={copied ? "已复制" : "复制"}
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => {
          setCopied(true);
          window.setTimeout(() => setCopied(false), 1500);
        });
      }}
      className={cn(
        "rounded p-1 text-muted-foreground transition-colors hover:bg-accent hover:text-foreground",
        className,
      )}
    >
      {copied ? (
        <Check className="size-3.5" aria-hidden />
      ) : (
        <Copy className="size-3.5" aria-hidden />
      )}
    </button>
  );
}

function CodeBlock({ code, lang }: { code: string; lang?: string }) {
  const dark = useIsDark();
  const [html, setHtml] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    if (!lang) return;
    void highlight(code, lang, dark).then((result) => {
      if (!cancelled) setHtml(result);
    });
    return () => {
      cancelled = true;
    };
  }, [code, lang, dark]);

  return (
    <div className="group/code relative my-3 overflow-hidden rounded-lg border bg-panel">
      <div className="flex items-center justify-between border-b px-3 py-1 text-muted-foreground text-xs">
        <span className="font-mono">{lang ?? "text"}</span>
        <CopyButton text={code} />
      </div>
      {html ? (
        <div
          className="overflow-x-auto p-3 font-mono text-[13px] leading-relaxed [&_pre]:bg-transparent!"
          // biome-ignore lint/security/noDangerouslySetInnerHtml: shiki 输出已对代码内容转义
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        <pre className="overflow-x-auto p-3 font-mono text-[13px] leading-relaxed">
          <code>{code}</code>
        </pre>
      )}
    </div>
  );
}

const COMPONENTS: Components = {
  pre: ({ children }) => <>{children}</>,
  code: ({ className, children }) => {
    const text = String(children ?? "");
    const match = /language-([\w+-]+)/.exec(className ?? "");
    // 有语言标记或跨多行的视为代码块，其余是行内代码。
    if (match || text.includes("\n")) {
      return <CodeBlock code={text.replace(/\n$/, "")} lang={match?.[1]} />;
    }
    return <code className="rounded bg-muted px-1 py-0.5 font-mono text-[0.9em]">{children}</code>;
  },
  a: ({ href, children }) => (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className="text-brand underline-offset-2 hover:underline"
    >
      {children}
    </a>
  ),
  table: ({ children }) => (
    <div className="my-3 overflow-x-auto">
      <table className="w-full border-collapse text-sm">{children}</table>
    </div>
  ),
  th: ({ children }) => <th className="border px-2 py-1 text-left font-medium">{children}</th>,
  td: ({ children }) => <td className="border px-2 py-1 align-top">{children}</td>,
  ul: ({ children }) => <ul className="my-2 list-disc pl-6">{children}</ul>,
  ol: ({ children }) => <ol className="my-2 list-decimal pl-6">{children}</ol>,
  li: ({ children }) => <li className="my-0.5">{children}</li>,
  p: ({ children }) => <p className="my-2 leading-relaxed">{children}</p>,
  h1: ({ children }) => <h1 className="mt-4 mb-2 font-semibold text-xl">{children}</h1>,
  h2: ({ children }) => <h2 className="mt-4 mb-2 font-semibold text-lg">{children}</h2>,
  h3: ({ children }) => <h3 className="mt-3 mb-1.5 font-semibold text-base">{children}</h3>,
  blockquote: ({ children }) => (
    <blockquote className="my-2 border-l-2 pl-3 text-muted-foreground">{children}</blockquote>
  ),
  hr: () => <hr className="my-4" />,
};

export const Markdown = memo(function Markdown({
  text,
  className,
}: {
  text: string;
  className?: string;
}) {
  return (
    <div className={cn("break-words", className)}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={COMPONENTS}>
        {text}
      </ReactMarkdown>
    </div>
  );
});
