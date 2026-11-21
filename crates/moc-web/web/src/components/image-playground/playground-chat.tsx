// Image Playground 会话对话视图：每一次消息是一次独立的 OpenAI Images API 调用。

import {
  AlertCircleIcon,
  BookmarkPlusIcon,
  CopyIcon,
  PanelLeftOpenIcon,
  RotateCcwIcon,
  SquarePenIcon,
} from "lucide-react";
import { useMemo } from "react";
import { toast } from "sonner";
import {
  Conversation,
  ConversationContent,
  ConversationScrollButton,
} from "@/components/ai-elements/conversation";
import { Shimmer } from "@/components/ai-elements/shimmer";
import { IconButton } from "@/components/common/icon-button";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { useT } from "@/lib/i18n";
import {
  type ImageGenParams,
  type PlaygroundTurn,
  pgImageUrl,
  pgUploadUrl,
} from "@/lib/image-playground-api";
import { useNarrow } from "@/lib/use-narrow";
import { cn } from "@/lib/utils";
import { useImagePlayground } from "@/store/image-playground";
import { useLayout } from "@/store/layout";
import { formatFileSize, ImageCard } from "./image-card";
import { PlaygroundComposer } from "./playground-composer";

function SidebarToggleIfHidden() {
  const t = useT();
  const collapsed = useLayout((s) => s.sidebarCollapsed);
  const overlay = useLayout((s) => s.sidebarOverlay);
  const narrow = useNarrow();
  if (!(narrow ? !overlay : collapsed)) return null;
  return (
    <IconButton label={t("hd.sidebar")} onClick={() => useLayout.getState().toggleSidebar()}>
      <PanelLeftOpenIcon />
    </IconButton>
  );
}

function paramChips(params: ImageGenParams): string[] {
  const chips: string[] = [];
  if (params.size) chips.push(params.size);
  if (params.aspect_ratio) chips.push(params.aspect_ratio);
  if (params.image_size) chips.push(params.image_size);
  if (params.quality) chips.push(params.quality);
  if (params.output_format) chips.push(params.output_format);
  if (params.n && params.n > 1) chips.push(`n=${params.n}`);
  return chips;
}

export function PlaygroundChatView() {
  const t = useT();
  const activeSessionId = useImagePlayground((s) => s.activeSessionId);
  const session = useImagePlayground((s) =>
    s.activeSessionId ? s.sessionDetails[s.activeSessionId] : undefined,
  );
  const running = useImagePlayground((s) =>
    Boolean(s.activeSessionId && s.runningBySession[s.activeSessionId]),
  );
  const newSession = useImagePlayground((s) => s.newSession);

  // 无会话时提供一键开始
  if (!activeSessionId || !session) {
    return (
      <div className="flex h-full flex-1 flex-col">
        <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border-subtle px-3">
          <SidebarToggleIfHidden />
          <span className="text-sm font-medium text-foreground">Image Playground</span>
        </header>
        <div className="flex flex-1 flex-col items-center justify-center gap-3 px-6 text-center">
          <h1 className="text-heading font-semibold text-foreground">Image Playground</h1>
          <p className="max-w-md text-ui text-muted-foreground">{t("ip.welcome_desc")}</p>
          <Button
            size="sm"
            onClick={() =>
              void newSession().catch((err) =>
                toast.error(t("ip.action_failed", { error: String(err) })),
              )
            }
          >
            <SquarePenIcon className="size-4" />
            {t("ip.new_session")}
          </Button>
        </div>
      </div>
    );
  }

  // 空会话：居中欢迎布局（复用 Agent 欢迎页风格）
  if (session.turns.length === 0 && !running) {
    return (
      <div className="flex h-full flex-1 flex-col">
        <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border-subtle px-3">
          <SidebarToggleIfHidden />
          <span className="truncate text-sm font-medium text-foreground">{session.title}</span>
        </header>
        <div className="flex flex-1 flex-col items-center justify-center px-6 pb-12">
          <div className="w-full max-w-[688px] space-y-5">
            <div className="space-y-1">
              <h1 className="text-heading font-semibold text-foreground">Image Playground</h1>
              <p className="text-ui text-muted-foreground">{t("ip.welcome_desc")}</p>
            </div>
            <PlaygroundComposer sessionId={activeSessionId} inline />
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-1 flex-col">
      <header className="flex h-11 shrink-0 items-center gap-2 border-b border-border-subtle px-3">
        <SidebarToggleIfHidden />
        <span className="truncate text-sm font-medium text-foreground">{session.title}</span>
        <span className="ml-auto text-xs text-text-tertiary tabular-nums">
          {t("ip.turns_count", { n: session.turns.length })}
        </span>
      </header>

      <div className="moc-chat-pane relative flex min-h-0 flex-1 overflow-hidden">
        <Conversation key={activeSessionId} className="flex-1">
          <ConversationContent className="moc-chat-column gap-8 py-8">
            {session.turns.map((turn) => (
              <TurnItem key={turn.id} sessionId={activeSessionId} turn={turn} />
            ))}
            {running && (
              <div className="flex items-center gap-2.5 text-sm text-muted-foreground">
                <Shimmer duration={1.8}>{t("ip.generating_detail")}</Shimmer>
              </div>
            )}
          </ConversationContent>
          <ConversationScrollButton className="bottom-4 size-8 border-0 bg-popover text-muted-foreground shadow-[var(--elevation-popover)] hover:bg-popover hover:text-foreground dark:bg-popover" />
        </Conversation>
      </div>

      <PlaygroundComposer sessionId={activeSessionId} />
    </div>
  );
}

function TurnItem({ sessionId, turn }: { sessionId: string; turn: PlaygroundTurn }) {
  const t = useT();
  const imagesById = useImagePlayground((s) => s.imagesById);
  const setLightbox = useImagePlayground((s) => s.setLightbox);
  const retryTurn = useImagePlayground((s) => s.retryTurn);
  const running = useImagePlayground((s) => Boolean(s.runningBySession[sessionId]));

  const outputs = useMemo(
    () =>
      turn.outputs
        .map((id) => imagesById[id])
        .filter((img): img is NonNullable<typeof img> => Boolean(img)),
    [turn.outputs, imagesById],
  );
  const outputIds = useMemo(() => outputs.map((i) => i.id), [outputs]);
  const chips = paramChips(turn.params);

  return (
    <section className="flex flex-col gap-3">
      {/* 用户请求气泡 */}
      <div className="ml-auto flex max-w-[85%] flex-col items-end gap-2">
        {turn.inputs.length > 0 && (
          <div className="flex flex-wrap justify-end gap-1.5">
            {turn.inputs.map((input, idx) => {
              const src = input.image_id
                ? pgImageUrl(input.image_id)
                : input.upload_id
                  ? pgUploadUrl(input.upload_id)
                  : "";
              if (!src) return null;
              return (
                <button
                  // biome-ignore lint/suspicious/noArrayIndexKey: 输入附件只读
                  key={idx}
                  type="button"
                  onClick={() => input.image_id && setLightbox(input.image_id, [input.image_id])}
                  className="size-16 overflow-hidden rounded-xl bg-card ring-1 ring-inset ring-border-subtle"
                  title={input.filename}
                >
                  <img src={src} alt={input.filename} className="size-full object-cover" />
                </button>
              );
            })}
          </div>
        )}
        <div className="rounded-[18px] bg-bubble px-4 py-2.5 text-chat text-foreground">
          <p className="whitespace-pre-wrap break-words">{turn.prompt}</p>
        </div>
        <div className="flex flex-wrap items-center justify-end gap-1.5 text-2xs text-text-tertiary">
          <button
            type="button"
            onClick={() => {
              const refIds = turn.inputs
                .map((i) => i.image_id)
                .filter((id): id is string => Boolean(id));
              const uploadRefs = turn.inputs
                .filter((i): i is typeof i & { upload_id: string } =>
                  Boolean(!i.image_id && i.upload_id),
                )
                .map((i) => ({ upload_id: i.upload_id, filename: i.filename }));
              useImagePlayground.getState().openPresetEditor({
                initialPrompt: turn.prompt,
                initialImageIds: refIds,
                initialUploads: uploadRefs,
              });
            }}
            title={t("ip.save_as_preset")}
            className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 text-text-tertiary transition-colors hover:bg-accent hover:text-foreground"
          >
            <BookmarkPlusIcon className="size-3" />
            <span>{t("ip.save_as_preset_short")}</span>
          </button>
          <span className="font-mono">{turn.model}</span>
          {turn.mode === "edit" && <Badge variant="brand">edit</Badge>}
          {chips.map((c) => (
            <span key={c} className="rounded bg-muted px-1.5 py-px font-mono">
              {c}
            </span>
          ))}
        </div>
      </div>

      {/* 输出结果或上游原样报错 */}
      {turn.error ? (
        <div className="rounded-xl bg-destructive/5 p-4 ring-1 ring-inset ring-destructive/20">
          <div className="flex items-start gap-2.5">
            <AlertCircleIcon className="mt-0.5 size-4 shrink-0 text-destructive" />
            <div className="min-w-0 flex-1 space-y-2">
              <div className="flex flex-wrap items-center gap-2">
                <span className="text-sm font-medium text-destructive">
                  {turn.error.status ? `HTTP ${turn.error.status}` : t("ip.request_failed")}
                </span>
                <span className="text-xs text-destructive/90">{turn.error.message}</span>
              </div>
              {turn.error.raw_body && (
                <pre className="max-h-60 overflow-auto rounded-lg bg-code p-3 font-mono text-xs leading-5 text-foreground/90 whitespace-pre-wrap break-all">
                  {turn.error.raw_body}
                </pre>
              )}
              <div className="flex items-center gap-2 pt-0.5">
                <Button
                  size="xs"
                  variant="outline"
                  disabled={running}
                  onClick={() => {
                    void retryTurn(
                      sessionId,
                      turn.prompt,
                      turn.params,
                      turn.inputs,
                      turn.model,
                    ).catch((err) => toast.error(String(err)));
                  }}
                >
                  <RotateCcwIcon className="size-3.5" />
                  {t("common.retry")}
                </Button>
                <Button
                  size="xs"
                  variant="ghost"
                  onClick={() => {
                    void navigator.clipboard.writeText(
                      turn.error?.raw_body || turn.error?.message || "",
                    );
                    toast.success(t("common.copied"));
                  }}
                >
                  <CopyIcon className="size-3.5" />
                  {t("common.copy")}
                </Button>
              </div>
            </div>
          </div>
        </div>
      ) : (
        <div className="space-y-2">
          <div
            className={cn(
              "grid gap-3",
              outputs.length === 1 ? "max-w-lg grid-cols-1" : "grid-cols-1 sm:grid-cols-2",
            )}
          >
            {outputs.map((img) => (
              <ImageCard key={img.id} image={img} listIds={outputIds} />
            ))}
          </div>
          {outputs.length === 0 && (
            <p className="text-xs text-text-tertiary">{t("ip.images_deleted")}</p>
          )}
          <div className="flex flex-wrap items-center gap-2 text-xs text-text-tertiary tabular-nums">
            <span>{(turn.duration_ms / 1000).toFixed(1)}s</span>
            {outputs[0] && (
              <>
                <span>·</span>
                <span className="font-mono">
                  {outputs[0].width}×{outputs[0].height}
                </span>
                <span>·</span>
                <span className="font-mono uppercase">{outputs[0].format}</span>
                <span>·</span>
                <span className="font-mono">{formatFileSize(outputs[0].size_bytes)}</span>
              </>
            )}
          </div>
          {turn.revised_prompt && turn.revised_prompt !== turn.prompt && (
            <details className="text-xs text-muted-foreground">
              <summary className="cursor-pointer text-text-tertiary hover:text-foreground">
                revised_prompt
              </summary>
              <p className="mt-1 rounded-lg bg-code p-2.5 leading-5">{turn.revised_prompt}</p>
            </details>
          )}
        </div>
      )}
    </section>
  );
}
