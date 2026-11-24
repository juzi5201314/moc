// 上下文正文里的工具调用：core 把三种协议统一成 ```tool-call 代码块（{name, id, arguments}），
// 这里切出来渲染成卡片，其余正文照常走 Markdown。
import { WrenchIcon } from "lucide-react";
import { Card } from "@/components/ui/card";
import { useT } from "@/lib/i18n";

export type ToolCall = { name: string; id: string | null; arguments: unknown };
export type BodySegment = { kind: "markdown"; text: string } | { kind: "tool"; call: ToolCall };

// pretty JSON 的行首只会是空白、引号或括号，"\n```" 一定是围栏结束，懒惰匹配不会提前截断。
const FENCE = /^```tool-call\n([\s\S]*?)\n```$/gm;

function parseCall(json: string): ToolCall | null {
  try {
    const value = JSON.parse(json) as Partial<ToolCall>;
    return typeof value.name === "string"
      ? {
          name: value.name,
          id: typeof value.id === "string" ? value.id : null,
          arguments: value.arguments,
        }
      : null;
  } catch {
    return null;
  }
}

export function splitToolCalls(text: string): BodySegment[] {
  const segments: BodySegment[] = [];
  let last = 0;
  for (const match of text.matchAll(FENCE)) {
    const call = parseCall(match[1]);
    if (!call) continue;
    const before = text.slice(last, match.index).trim();
    if (before) segments.push({ kind: "markdown", text: before });
    segments.push({ kind: "tool", call });
    last = match.index + match[0].length;
  }
  const rest = text.slice(last).trim();
  if (rest) segments.push({ kind: "markdown", text: rest });
  return segments;
}

function formatValue(value: unknown): string {
  return typeof value === "string" ? value : JSON.stringify(value, null, 2);
}

export function ToolCallCard({ call }: { call: ToolCall }) {
  const t = useT();
  const args = call.arguments;
  const entries =
    args && typeof args === "object" && !Array.isArray(args) ? Object.entries(args) : null;
  return (
    <Card className="my-3 gap-0 overflow-hidden rounded-lg py-0 shadow-none">
      <div className="flex items-center gap-2 border-b border-border-subtle px-3 py-2">
        <WrenchIcon className="size-3.5 shrink-0 text-text-tertiary" />
        <span className="shrink-0 font-mono text-xs font-medium">{call.name}</span>
        {call.id && (
          <span
            className="ml-auto min-w-0 truncate font-mono text-2xs text-text-tertiary"
            title={call.id}
          >
            {call.id}
          </span>
        )}
      </div>
      {entries ? (
        entries.length > 0 ? (
          <dl className="grid grid-cols-[max-content_minmax(0,1fr)] gap-x-4 gap-y-1.5 px-3 py-2.5 text-xs leading-5">
            {entries.map(([key, value]) => (
              <div key={key} className="contents">
                <dt className="font-mono text-muted-foreground">{key}</dt>
                <dd className="whitespace-pre-wrap break-words font-mono">{formatValue(value)}</dd>
              </div>
            ))}
          </dl>
        ) : (
          <p className="px-3 py-2.5 text-xs text-muted-foreground">{t("cv.no_args")}</p>
        )
      ) : (
        <pre className="whitespace-pre-wrap break-words px-3 py-2.5 font-mono text-xs leading-5">
          {formatValue(args)}
        </pre>
      )}
    </Card>
  );
}
