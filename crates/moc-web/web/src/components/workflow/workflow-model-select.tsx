// workflow role/ModelRef 的模型选择：不绑会话（选定值回传，由审批/run 面板消费）。
// 复用 lib/models 的模型列表、provider 分组与思考强度解析。

import { CheckIcon } from "lucide-react";
import { useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { groupByProvider, modelName, parseSelector, reasoningLevels } from "@/lib/models";
import { cn } from "@/lib/utils";
import { selectModels, useConnection } from "@/store/connection";

export type WorkflowModelSelectProps = {
  /** 当前 selector（`provider/model[:level]`；空串 = 继承默认）。 */
  value: string;
  onSelect: (selector: string) => void;
  disabled?: boolean;
};

export function WorkflowModelSelect({ value, onSelect, disabled }: WorkflowModelSelectProps) {
  const [open, setOpen] = useState(false);
  const models = useConnection(selectModels);
  const parsed = parseSelector(value, models);
  const levels = reasoningLevels(parsed.model);
  const label = value ? modelName(parsed.id) || value : "inherit";

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <Button variant="ghost" size="sm" disabled={disabled} className="h-7 px-2 text-ui">
          <span className="max-w-[160px] truncate">{label}</span>
          {parsed.level && parsed.level !== "off" && (
            <span className="text-text-tertiary">·{parsed.level}</span>
          )}
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-[300px] p-0" align="start">
        <Command>
          <CommandInput placeholder="搜索模型" />
          <CommandList>
            <CommandEmpty>没有匹配的模型</CommandEmpty>
            <CommandGroup heading="继承">
              <CommandItem
                value="inherit"
                onSelect={() => {
                  onSelect("");
                  setOpen(false);
                }}
              >
                <CheckIcon className={cn("size-4", !value ? "opacity-100" : "opacity-0")} />
                默认（继承）
              </CommandItem>
            </CommandGroup>
            {groupByProvider(models).map(([provider, list]) => (
              <CommandGroup key={provider} heading={provider}>
                {list.map((model) => {
                  const id = model.id ?? modelName(model.id ?? "") ?? "";
                  const current = parsed.id === id;
                  return (
                    <CommandItem
                      key={id}
                      value={id}
                      onSelect={() => {
                        onSelect(parsed.level ? `${id}:${parsed.level}` : id);
                        setOpen(false);
                      }}
                    >
                      <CheckIcon className={cn("size-4", current ? "opacity-100" : "opacity-0")} />
                      {modelName(id) || id}
                    </CommandItem>
                  );
                })}
              </CommandGroup>
            ))}
            {levels.length > 0 && (
              <CommandGroup heading="思考强度">
                {levels.map((level) => (
                  <CommandItem
                    key={level}
                    value={`level:${level}`}
                    onSelect={() => {
                      onSelect(`${parsed.id}:${level}`);
                      setOpen(false);
                    }}
                  >
                    <CheckIcon
                      className={cn("size-4", parsed.level === level ? "opacity-100" : "opacity-0")}
                    />
                    {level}
                  </CommandItem>
                ))}
              </CommandGroup>
            )}
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );
}
