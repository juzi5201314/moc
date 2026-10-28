import { CheckIcon, ChevronDownIcon, SearchIcon } from "lucide-react";
import { useMemo, useState } from "react";
import { toast } from "sonner";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { rpc } from "@/protocol/client";
import type { SessionSetModelResult } from "@/protocol/types";
import { useConnection } from "@/store/connection";
import { createQuickSession } from "@/store/lifecycle";
import { useSessions } from "@/store/sessions";
import { useTranscript } from "@/store/transcript";

interface ModelSelectPopoverProps {
  sessionId: string;
  currentModelId?: string;
}

export function ModelSelectPopover({ sessionId, currentModelId }: ModelSelectPopoverProps) {
  const [open, setOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");

  const config = useConnection((state) => state.config);
  const models = config?.models ?? [];

  const modelGroups = useMemo(() => {
    const q = searchQuery.trim().toLowerCase();
    const filtered = q
      ? models.filter((model) => `${model.id} ${model.display_name}`.toLowerCase().includes(q))
      : models;
    const groups: Record<string, typeof models> = {};
    for (const model of filtered) {
      const groupName = model.id.toLowerCase().includes("deepseek") ? "DeepSeek" : "其他模型";
      groups[groupName] ??= [];
      groups[groupName].push(model);
    }
    return Object.entries(groups);
  }, [models, searchQuery]);

  const activeModel = models.find((model) => model.id === currentModelId) ??
    models.find((model) => model.id === config?.default_model) ??
    models[0] ?? { id: currentModelId || "deepseek-v4.1-flash", display_name: "" };

  const handleSelect = async (modelId: string) => {
    setOpen(false);
    if (sessionId === "draft-session") {
      if (!useSessions.getState().draftSession) createQuickSession();
      const draft = useSessions.getState().draftSession;
      if (draft) useSessions.getState().setDraftSession({ ...draft, model: modelId });
      useTranscript.getState().mutate(sessionId, (view) => {
        view.info.model = modelId;
      });
      return;
    }

    try {
      const result = await rpc.call<SessionSetModelResult>("session/set_model", {
        session_id: sessionId,
        selector: modelId,
      });
      useSessions.getState().upsert(result.session);
      useTranscript.getState().mutate(sessionId, (view) => {
        view.info = result.session;
      });
      if (result.compaction_error)
        toast.error(`模型已切换，但上下文压缩失败：${result.compaction_error}`);
      else if (result.overflow)
        toast(
          `当前上下文超出目标模型容量：${result.overflow.tokens.toLocaleString()} / ${result.overflow.context_window.toLocaleString()} tokens`,
        );
      else toast.success(`已切换模型至: ${modelId}`);
    } catch (err) {
      toast.error(`切换模型失败: ${err instanceof Error ? err.message : String(err)}`);
    }
  };

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex h-7 items-center gap-1.5 rounded-md px-2 text-[13px] text-muted-foreground transition-colors hover:bg-[var(--accent)] hover:text-foreground"
          aria-label="选择模型"
        >
          <span className="max-w-[180px] truncate font-mono">{activeModel.id}</span>
          <ChevronDownIcon className="size-3 opacity-60" />
        </button>
      </PopoverTrigger>

      <PopoverContent align="end" side="top" sideOffset={8} className="w-64 p-0 text-xs shadow-lg">
        {/* 顶部搜索框 */}
        <div className="flex items-center gap-2 border-b px-2.5 py-2">
          <SearchIcon className="size-3.5 text-muted-foreground shrink-0" />
          <input
            type="text"
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            placeholder="搜索模型..."
            className="flex-1 bg-transparent text-xs outline-none placeholder:text-muted-foreground"
          />
        </div>

        {/* 分组模型列表 */}
        <div className="max-h-60 overflow-y-auto p-1 space-y-2">
          {modelGroups.map(([groupName, groupModels]) => (
            <div key={groupName} className="space-y-0.5">
              <div className="px-2 py-1 text-[10px] font-semibold text-muted-foreground uppercase tracking-wider">
                {groupName}
              </div>
              {groupModels.map((model) => {
                const isSelected = model.id === currentModelId;
                return (
                  <button
                    key={model.id}
                    type="button"
                    onClick={() => void handleSelect(model.id)}
                    className={`flex h-7 w-full items-center justify-between rounded-md px-2 text-xs transition-colors hover:bg-muted ${
                      isSelected ? "bg-muted font-medium text-foreground" : "text-muted-foreground"
                    }`}
                  >
                    <span className="truncate font-mono">{model.id}</span>
                    <div className="flex items-center gap-1.5 shrink-0">
                      {model.context_window ? (
                        <span className="rounded-full bg-muted-foreground/10 px-1.5 py-0.5 text-[9px] text-muted-foreground">
                          {Math.round(model.context_window / 1000)}k
                        </span>
                      ) : null}
                      {isSelected && <CheckIcon className="size-3.5 text-primary" />}
                    </div>
                  </button>
                );
              })}
            </div>
          ))}

          {modelGroups.length === 0 && (
            <div className="py-6 text-center text-xs text-muted-foreground">未找到匹配的模型</div>
          )}
        </div>
      </PopoverContent>
    </Popover>
  );
}
