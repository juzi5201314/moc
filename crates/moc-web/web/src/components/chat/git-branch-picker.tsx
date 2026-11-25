// Git 分支选择器：欢迎页与会话顶栏共用。
// 支持搜索本地/远程分支、输入新名一键创建并切换、worktree 占用与进行中操作阻断提示，
// 以及脏工作区 / 覆盖冲突 / detached HEAD / 未保存编辑器草稿的统一确认弹窗。

import {
  AlertTriangleIcon,
  CheckIcon,
  ChevronDownIcon,
  GitBranchIcon,
  Loader2Icon,
  PlusIcon,
} from "lucide-react";
import { useCallback, useMemo, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Command,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
  CommandSeparator,
} from "@/components/ui/command";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { type t as translateFn, useT } from "@/lib/i18n";
import { applySwitchedBranch, formatGitBranchLabel, useGitBranch } from "@/lib/use-git-branch";
import { cn } from "@/lib/utils";
import { rpc } from "@/protocol/client";
import type {
  GitOperation,
  GitSwitchDirtyPolicy,
  GitWorktreeStatus,
  WorkspaceGitBranchesResult,
  WorkspaceGitSwitchResult,
} from "@/protocol/types";
import { dirtyEditorFilesForRepo, refreshRepoFiles } from "@/store/files";
import { workspaceName } from "@/store/sessions";

type PendingConfirm = {
  target: { branch: string; remote: boolean };
  status: GitWorktreeStatus;
  dirtyFiles: string[];
  detached: boolean;
  overwriteConflict: boolean;
  conflictFiles: string[];
  unsavedEditorFiles: string[];
};

const EMPTY_STATUS: GitWorktreeStatus = {
  staged: 0,
  unstaged: 0,
  untracked: 0,
  conflicted: 0,
};

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function hasDirtyChanges(status: GitWorktreeStatus): boolean {
  return status.staged > 0 || status.unstaged > 0 || status.untracked > 0;
}

function formatDirtySummary(t: typeof translateFn, status: GitWorktreeStatus): string {
  const parts: string[] = [];
  if (status.staged > 0) parts.push(t("git.stat_staged", { n: status.staged }));
  if (status.unstaged > 0) parts.push(t("git.stat_unstaged", { n: status.unstaged }));
  if (status.untracked > 0) parts.push(t("git.stat_untracked", { n: status.untracked }));
  return parts.join(" · ");
}

function operationName(op: GitOperation): string {
  return op === "cherry_pick" ? "cherry-pick" : op;
}

export function GitBranchPicker({
  cwd,
  variant,
  disabled,
  disabledReason,
}: {
  cwd: string | undefined;
  variant: "welcome" | "header";
  disabled?: boolean;
  disabledReason?: string;
}) {
  const t = useT();
  const branchInfo = useGitBranch(cwd);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [data, setData] = useState<WorkspaceGitBranchesResult | null>(null);
  const [switching, setSwitching] = useState(false);
  const [confirm, setConfirm] = useState<PendingConfirm | null>(null);

  const loadBranches = useCallback(async () => {
    if (!cwd) return;
    setLoading(true);
    setLoadError(null);
    try {
      const result = await rpc.call<WorkspaceGitBranchesResult>("workspace/git_branches", { cwd });
      setData(result);
    } catch (error) {
      setLoadError(errorText(error));
    } finally {
      setLoading(false);
    }
  }, [cwd]);

  const handleOpenChange = (next: boolean) => {
    if (next && disabled) return;
    setOpen(next);
    if (next) {
      setQuery("");
      void loadBranches();
    }
  };

  const localBranches = data?.local ?? [];
  const remoteBranches = data?.remote ?? [];
  const trimmedQuery = query.trim();
  const lowerQuery = trimmedQuery.toLowerCase();

  const filteredLocal = useMemo(
    () =>
      lowerQuery
        ? localBranches.filter((item) => item.name.toLowerCase().includes(lowerQuery))
        : localBranches,
    [localBranches, lowerQuery],
  );
  const filteredRemote = useMemo(
    () =>
      lowerQuery
        ? remoteBranches.filter((item) => item.name.toLowerCase().includes(lowerQuery))
        : remoteBranches,
    [remoteBranches, lowerQuery],
  );

  const exactLocalExists = useMemo(
    () => localBranches.some((item) => item.name === trimmedQuery),
    [localBranches, trimmedQuery],
  );

  if (!branchInfo || !cwd) return null;

  const label = formatGitBranchLabel(branchInfo);
  const repoRoot = data?.root ?? branchInfo.root;
  const busySessions = data?.busy_sessions ?? [];
  const hardBlocked =
    busySessions.length > 0 || Boolean(data?.in_progress) || (data?.status.conflicted ?? 0) > 0;
  const dirtyNow = data ? hasDirtyChanges(data.status) : false;
  const canCreate = Boolean(trimmedQuery) && !exactLocalExists && !hardBlocked;

  const executeSwitch = async (options: {
    branch: string;
    create: boolean;
    remote: boolean;
    dirty: GitSwitchDirtyPolicy;
  }) => {
    if (switching) return;
    const unsavedEditorFiles = options.create ? [] : dirtyEditorFilesForRepo(cwd, repoRoot);

    // 工作区干净且非 detached 时，若编辑器仍有未保存草稿，在首次触发时先行提醒。
    if (
      !options.create &&
      options.dirty === "check" &&
      unsavedEditorFiles.length > 0 &&
      !dirtyNow &&
      !data?.detached
    ) {
      setOpen(false);
      setConfirm({
        target: { branch: options.branch, remote: options.remote },
        status: data?.status ?? EMPTY_STATUS,
        dirtyFiles: [],
        detached: false,
        overwriteConflict: false,
        conflictFiles: [],
        unsavedEditorFiles,
      });
      return;
    }

    setSwitching(true);
    try {
      const result = await rpc.call<WorkspaceGitSwitchResult>("workspace/git_switch", {
        cwd,
        branch: options.branch,
        create: options.create,
        remote: options.remote,
        dirty: options.dirty,
      });

      if (result.outcome === "switched") {
        setOpen(false);
        setConfirm(null);
        applySwitchedBranch(cwd, repoRoot, result.branch);
        refreshRepoFiles(cwd, repoRoot);
        if (result.stash_message) {
          toast.success(t("git.switched_stashed", { branch: result.branch }), {
            description: t("git.stash_hint", { message: result.stash_message }),
          });
        } else if (result.created) {
          toast.success(t("git.created_switched", { branch: result.branch }));
        } else {
          toast.success(t("git.switched", { branch: result.branch }));
        }
        return;
      }

      if (result.outcome === "needs_confirmation") {
        setOpen(false);
        setConfirm({
          target: { branch: options.branch, remote: options.remote },
          status: result.status,
          dirtyFiles: result.dirty_files ?? [],
          detached: Boolean(result.detached),
          overwriteConflict: Boolean(result.overwrite_conflict),
          conflictFiles: result.conflict_files ?? [],
          unsavedEditorFiles,
        });
        return;
      }

      // Blocked
      setConfirm(null);
      toast.error(
        result.message,
        result.details?.length ? { description: result.details.join(" · ") } : undefined,
      );
      if (open) void loadBranches();
    } catch (error) {
      toast.error(t("git.switch_failed", { error: errorText(error) }));
    } finally {
      setSwitching(false);
    }
  };

  const triggerButton = (
    <button
      type="button"
      disabled={disabled || switching}
      aria-label={t("git.switch_branch", { branch: label })}
      title={
        disabled && disabledReason ? disabledReason : t("git.switch_branch", { branch: label })
      }
      className={cn(
        "inline-flex min-w-0 items-center transition-colors hover:bg-accent hover:text-foreground disabled:pointer-events-none data-[state=open]:bg-accent data-[state=open]:text-foreground",
        variant === "welcome"
          ? "h-8 gap-1.5 rounded-lg px-2 text-text-tertiary"
          : "h-7 gap-1 rounded-md px-1.5 text-muted-foreground",
      )}
    >
      {switching ? (
        <Loader2Icon className="size-3.5 shrink-0 animate-spin" />
      ) : (
        <GitBranchIcon className="size-3.5 shrink-0" />
      )}
      <span className="max-w-[160px] truncate font-mono text-xs">{label}</span>
      {!disabled && <ChevronDownIcon className="size-3 shrink-0 opacity-60" />}
    </button>
  );

  const confirmHasDirty = confirm ? hasDirtyChanges(confirm.status) : false;
  const confirmFiles = confirm
    ? confirm.overwriteConflict && confirm.conflictFiles.length > 0
      ? confirm.conflictFiles
      : confirm.dirtyFiles
    : [];

  return (
    <>
      <Popover open={open} onOpenChange={handleOpenChange}>
        {disabled && disabledReason ? (
          <Tooltip>
            <TooltipTrigger asChild>
              <span className="inline-flex min-w-0">{triggerButton}</span>
            </TooltipTrigger>
            <TooltipContent>{disabledReason}</TooltipContent>
          </Tooltip>
        ) : (
          <PopoverTrigger asChild>{triggerButton}</PopoverTrigger>
        )}

        <PopoverContent
          align="start"
          side="bottom"
          sideOffset={6}
          className="w-72 max-w-[calc(100vw-2rem)] overflow-hidden p-0"
        >
          <Command shouldFilter={false}>
            <CommandInput
              value={query}
              onValueChange={setQuery}
              placeholder={t("git.search_placeholder")}
            />

            {/* 硬阻断或游离头指针提示 */}
            {data && busySessions.length > 0 && (
              <div className="flex items-start gap-2 border-b border-border-subtle bg-warn/10 px-3 py-2 text-xs text-warn">
                <AlertTriangleIcon className="mt-0.5 size-3.5 shrink-0" />
                <span>{t("git.blocked_busy", { sessions: busySessions.join(", ") })}</span>
              </div>
            )}
            {data && busySessions.length === 0 && data.in_progress && (
              <div className="flex items-start gap-2 border-b border-border-subtle bg-warn/10 px-3 py-2 text-xs text-warn">
                <AlertTriangleIcon className="mt-0.5 size-3.5 shrink-0" />
                <span>{t("git.blocked_op", { op: operationName(data.in_progress) })}</span>
              </div>
            )}
            {data &&
              busySessions.length === 0 &&
              !data.in_progress &&
              data.status.conflicted > 0 && (
                <div className="flex items-start gap-2 border-b border-border-subtle bg-warn/10 px-3 py-2 text-xs text-warn">
                  <AlertTriangleIcon className="mt-0.5 size-3.5 shrink-0" />
                  <span>{t("git.blocked_conflicts", { n: data.status.conflicted })}</span>
                </div>
              )}
            {data && !hardBlocked && data.detached && (
              <div className="border-b border-border-subtle bg-warn/10 px-3 py-2 text-xs text-warn">
                {t("git.detached_hint")}
              </div>
            )}

            <CommandList className="max-h-72">
              {loading && !data ? (
                <div className="flex items-center justify-center gap-2 py-6 text-xs text-muted-foreground">
                  <Loader2Icon className="size-3.5 animate-spin" />
                  <span>{t("git.loading")}</span>
                </div>
              ) : loadError ? (
                <div className="flex flex-col items-center gap-2 px-3 py-5 text-center text-xs text-muted-foreground">
                  <span className="text-destructive">
                    {t("git.load_failed", { error: loadError })}
                  </span>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-7 text-xs shadow-none"
                    onClick={() => void loadBranches()}
                  >
                    {t("git.retry")}
                  </Button>
                </div>
              ) : (
                <>
                  {canCreate && (
                    <CommandGroup>
                      <CommandItem
                        value={`create:${trimmedQuery}`}
                        disabled={switching}
                        onSelect={() =>
                          void executeSwitch({
                            branch: trimmedQuery,
                            create: true,
                            remote: false,
                            dirty: "check",
                          })
                        }
                        className="items-start gap-2 py-1.5"
                      >
                        <PlusIcon className="mt-0.5 size-3.5 shrink-0" />
                        <span className="flex min-w-0 flex-1 flex-col">
                          <span className="truncate text-ui text-foreground">
                            {t("git.create_switch", { name: trimmedQuery })}
                          </span>
                          {dirtyNow && (
                            <span className="text-2xs text-text-tertiary">
                              {t("git.create_carries_changes")}
                            </span>
                          )}
                        </span>
                      </CommandItem>
                    </CommandGroup>
                  )}

                  {canCreate && (filteredLocal.length > 0 || filteredRemote.length > 0) && (
                    <CommandSeparator />
                  )}

                  {filteredLocal.length > 0 && (
                    <CommandGroup heading={t("git.local")}>
                      {filteredLocal.map((entry) => {
                        const occupied = Boolean(entry.worktree_path);
                        const itemDisabled =
                          switching || occupied || (hardBlocked && !entry.current);
                        return (
                          <CommandItem
                            key={`local:${entry.name}`}
                            value={`local:${entry.name}`}
                            disabled={itemDisabled}
                            title={
                              entry.worktree_path
                                ? t("git.in_worktree", { path: entry.worktree_path })
                                : entry.name
                            }
                            onSelect={() => {
                              if (entry.current) {
                                setOpen(false);
                                return;
                              }
                              void executeSwitch({
                                branch: entry.name,
                                create: false,
                                remote: false,
                                dirty: "check",
                              });
                            }}
                            className="gap-2 py-1.5"
                          >
                            <span className="min-w-0 flex-1 truncate font-mono text-xs text-foreground">
                              {entry.name}
                            </span>
                            {entry.worktree_path && (
                              <span className="max-w-[96px] shrink-0 truncate text-2xs text-text-tertiary">
                                {workspaceName(entry.worktree_path)}
                              </span>
                            )}
                            {entry.current && (
                              <CheckIcon className="size-3.5 shrink-0 text-muted-foreground" />
                            )}
                          </CommandItem>
                        );
                      })}
                    </CommandGroup>
                  )}

                  {filteredLocal.length > 0 && filteredRemote.length > 0 && <CommandSeparator />}

                  {filteredRemote.length > 0 && (
                    <CommandGroup heading={t("git.remote")}>
                      {filteredRemote.map((entry) => (
                        <CommandItem
                          key={`remote:${entry.name}`}
                          value={`remote:${entry.name}`}
                          disabled={switching || hardBlocked || Boolean(data?.unborn)}
                          title={entry.name}
                          onSelect={() =>
                            void executeSwitch({
                              branch: entry.name,
                              create: false,
                              remote: true,
                              dirty: "check",
                            })
                          }
                          className="gap-2 py-1.5"
                        >
                          <span className="min-w-0 flex-1 truncate font-mono text-xs text-foreground">
                            {entry.name}
                          </span>
                          {entry.has_local && (
                            <span className="shrink-0 text-2xs text-text-tertiary">
                              {t("git.local_exists")}
                            </span>
                          )}
                        </CommandItem>
                      ))}
                    </CommandGroup>
                  )}

                  {!canCreate && filteredLocal.length === 0 && filteredRemote.length === 0 && (
                    <div className="px-3 py-6 text-center text-xs text-text-tertiary">
                      {data?.unborn ? t("git.unborn_empty") : t("git.empty")}
                    </div>
                  )}
                </>
              )}
            </CommandList>

            {data && !hardBlocked && dirtyNow && (
              <div className="border-t border-border-subtle px-3 py-1.5 text-2xs text-text-tertiary">
                {t("git.dirty_summary", { summary: formatDirtySummary(t, data.status) })}
              </div>
            )}
          </Command>
        </PopoverContent>
      </Popover>

      <Dialog
        open={confirm !== null}
        onOpenChange={(next) => {
          if (!next && !switching) setConfirm(null);
        }}
      >
        <DialogContent showCloseButton={!switching}>
          <DialogHeader>
            <DialogTitle>
              {t("git.confirm_title", { branch: confirm?.target.branch ?? "" })}
            </DialogTitle>
            <DialogDescription>
              {confirm?.overwriteConflict
                ? t("git.confirm_conflict_desc", { branch: confirm.target.branch })
                : confirmHasDirty && confirm
                  ? t("git.confirm_dirty_desc", {
                      summary: formatDirtySummary(t, confirm.status),
                    })
                  : confirm?.detached
                    ? t("git.confirm_detached_desc")
                    : t("git.confirm_unsaved_desc")}
            </DialogDescription>
          </DialogHeader>

          {confirmFiles.length > 0 && (
            <ul className="max-h-40 space-y-1 overflow-y-auto rounded-xl bg-code px-3 py-2.5 font-mono text-xs text-foreground">
              {confirmFiles.map((file) => (
                <li key={file} className="truncate">
                  {file}
                </li>
              ))}
            </ul>
          )}

          {confirm?.detached && confirmHasDirty && (
            <div className="rounded-lg bg-warn/10 px-3 py-2 text-xs text-warn">
              {t("git.detached_hint")}
            </div>
          )}

          {confirm && confirm.unsavedEditorFiles.length > 0 && (
            <div className="rounded-lg bg-warn/10 px-3 py-2 text-xs text-warn">
              {t("git.unsaved_warning", { files: confirm.unsavedEditorFiles.join(", ") })}
            </div>
          )}

          <DialogFooter>
            <Button variant="ghost" disabled={switching} onClick={() => setConfirm(null)}>
              {t("common.cancel")}
            </Button>
            {confirm && confirmHasDirty && !confirm.overwriteConflict && (
              <Button
                variant="outline"
                disabled={switching}
                className="shadow-none"
                onClick={() =>
                  void executeSwitch({
                    branch: confirm.target.branch,
                    create: false,
                    remote: confirm.target.remote,
                    dirty: "carry",
                  })
                }
              >
                {t("git.carry_changes")}
              </Button>
            )}
            {confirm && (confirmHasDirty || confirm.overwriteConflict) ? (
              <Button
                disabled={switching}
                onClick={() =>
                  void executeSwitch({
                    branch: confirm.target.branch,
                    create: false,
                    remote: confirm.target.remote,
                    dirty: "stash",
                  })
                }
              >
                {switching && <Loader2Icon className="size-3.5 animate-spin" />}
                {t("git.stash_and_switch")}
              </Button>
            ) : (
              confirm && (
                <Button
                  disabled={switching}
                  onClick={() =>
                    void executeSwitch({
                      branch: confirm.target.branch,
                      create: false,
                      remote: confirm.target.remote,
                      dirty: "carry",
                    })
                  }
                >
                  {switching && <Loader2Icon className="size-3.5 animate-spin" />}
                  {t("git.switch_anyway")}
                </Button>
              )
            )}
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
