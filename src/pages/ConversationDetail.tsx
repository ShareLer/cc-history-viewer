import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import {
  ArrowLeft,
  Check,
  Download,
  Folder,
  FolderOpen,
  GitBranch,
  MessageSquare,
  Terminal,
} from "lucide-react";
import { useConversation } from "@/hooks/queries";
import { useCopy } from "@/hooks/useCopy";
import { getCurrentLang, useT } from "@/i18n";
import {
  Badge,
  Button,
  CenterMessage,
  Skeleton,
  Spinner,
} from "@/components/ui";
import { MarkdownMessage } from "@/components/MarkdownMessage";
import type {
  ChatMessage,
  ContentBlock,
  ConversationExportResult,
} from "@/lib/types";
import {
  absoluteTime,
  cn,
  encodePath,
  formatNumber,
  prettyPath,
} from "@/lib/utils";
import { api, errMessage } from "@/lib/api";

type ViewOptions = {
  showThinking: boolean;
  showTools: boolean;
  showSkills: boolean;
  showMeta: boolean;
};

type ExportOptions = {
  includeThinking: boolean;
  includeTools: boolean;
  includeSkills: boolean;
  includeMeta: boolean;
  includeTime: boolean;
};

function shouldShowBlock(block: ContentBlock, options: ViewOptions): boolean {
  switch (block.kind) {
    case "text":
    case "image":
      return true;
    case "thinking":
      return options.showThinking;
    case "tool_use":
    case "tool_result":
      return options.showTools;
    case "skill":
      return options.showSkills;
    default:
      return false;
  }
}

function shouldExportBlock(block: ContentBlock, options: ExportOptions): boolean {
  switch (block.kind) {
    case "text":
    case "image":
      return true;
    case "thinking":
      return options.includeThinking;
    case "tool_use":
    case "tool_result":
      return options.includeTools;
    case "skill":
      return options.includeSkills;
    default:
      return false;
  }
}

function isMessageVisible(msg: ChatMessage, options: ViewOptions): boolean {
  if (msg.isMeta && !options.showMeta) return false;
  if (msg.metaKind === "command" && !options.showTools) return false;
  if (msg.metaKind === "skill" && !options.showSkills) return false;
  return msg.blocks.some((block) => shouldShowBlock(block, options));
}

function isMessageExportable(msg: ChatMessage, options: ExportOptions): boolean {
  if (msg.isMeta && !options.includeMeta) return false;
  if (msg.metaKind === "command" && !options.includeTools) return false;
  if (msg.metaKind === "skill" && !options.includeSkills) return false;
  return msg.blocks.some((block) => shouldExportBlock(block, options));
}

function BlockView({
  block,
  renderMarkdown = false,
}: {
  block: ContentBlock;
  renderMarkdown?: boolean;
}) {
  const t = useT();
  if (block.kind === "text") {
    if (renderMarkdown) {
      return <MarkdownMessage text={block.text ?? ""} />;
    }
    return (
      <div className="whitespace-pre-wrap break-words text-sm leading-relaxed text-foreground">
        {block.text}
      </div>
    );
  }
  if (block.kind === "image") {
    return (
      <div className="text-xs text-muted">
        🖼 {block.text ?? t("imageFallback")}
      </div>
    );
  }

  const summary =
    block.kind === "tool_use"
      ? t("toolUseLabel", { name: block.toolName ?? "tool" })
      : block.kind === "thinking"
        ? t("thinkingLabel")
        : block.kind === "tool_result"
          ? t("toolResultLabel")
          : block.kind === "skill"
            ? t("skillDetailLabel")
            : block.kind;
  const body =
    block.kind === "tool_use"
      ? JSON.stringify(block.toolInput ?? {}, null, 2)
      : block.text ?? "";

  return (
    <details className="rounded-lg border border-border bg-background/70">
      <summary className="cursor-pointer select-none px-3 py-1.5 text-xs font-medium text-muted">
        {summary}
      </summary>
      <pre className="overflow-x-auto whitespace-pre-wrap break-words px-3 pb-2.5 text-[11px] leading-relaxed text-muted">
        {body}
      </pre>
    </details>
  );
}

function MessageBubble({
  msg,
  blocks,
  highlighted = false,
}: {
  msg: ChatMessage;
  blocks: ContentBlock[];
  highlighted?: boolean;
}) {
  const t = useT();
  const isUser = msg.role === "user";
  const isSystem = msg.role === "system";
  const isMeta = msg.isMeta;
  const isSkillMeta = msg.metaKind === "skill";
  const shouldRenderMarkdown = !isUser && !isSystem && !isMeta;
  const roleLabel = isMeta
    ? t("metaBadge")
    : isUser
    ? t("roleUser")
    : isSkillMeta
      ? t("skillBadge")
      : isSystem
        ? t("commandBadge")
        : "Claude";

  return (
    <div
      className={cn(
        "rounded-xl border p-4 transition-shadow duration-500",
        isUser && !isMeta && "border-accent/30 bg-accent/[0.08]",
        !isUser && !isSystem && "border-border bg-surface",
        isSystem &&
          !isSkillMeta &&
          "border-dashed border-warning/30 bg-warning/[0.08]",
        isSkillMeta && "border-border bg-surface-2/80",
        isMeta && "border-dashed border-muted/40 bg-surface-2/70",
        highlighted && "ring-2 ring-accent shadow-lg"
      )}
    >
      <div className="mb-2.5 flex items-center gap-2">
        <Badge
          tone={
            isMeta
              ? "outline"
              : isUser
                ? "accent"
                : isSkillMeta
                  ? "muted"
                  : isSystem
                    ? "warning"
                    : "default"
          }
        >
          {roleLabel}
        </Badge>
        {msg.isSidechain && <Badge tone="muted">{t("sidechainBadge")}</Badge>}
        <span className="text-[11px] text-muted">{absoluteTime(msg.timestamp)}</span>
      </div>
      <div className="space-y-2">
        {blocks.map((b, i) => (
          <BlockView
            key={i}
            block={b}
            renderMarkdown={shouldRenderMarkdown && b.kind === "text"}
          />
        ))}
      </div>
      {msg.metaKind === "command" && (
        <p className="mt-2 text-[11px] text-muted">{t("commandReplyNote")}</p>
      )}
    </div>
  );
}

export function ConversationDetail() {
  const { sessionId } = useParams();
  const navigate = useNavigate();
  const t = useT();
  const { data, isLoading, isError, error } = useConversation(
    sessionId ?? null
  );
  const { copied, copy } = useCopy();

  const [searchParams] = useSearchParams();
  const targetTs = Number(searchParams.get("t")) || null;
  const [highlightIdx, setHighlightIdx] = useState<number | null>(null);

  const [viewOptions, setViewOptions] = useState<ViewOptions>({
    showThinking: false,
    showTools: false,
    showSkills: false,
    showMeta: false,
  });

  const [exportOpen, setExportOpen] = useState(false);
  const [exportOptions, setExportOptions] = useState<ExportOptions>({
    includeThinking: false,
    includeTools: false,
    includeSkills: false,
    includeMeta: false,
    includeTime: false,
  });
  const [selectedMessageUuids, setSelectedMessageUuids] = useState<string[]>([]);
  const selectAllRef = useRef<HTMLInputElement>(null);
  const previousExportableVisibleIdsRef = useRef<string[]>([]);
  const [exporting, setExporting] = useState(false);
  const [exportResult, setExportResult] =
    useState<ConversationExportResult | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);

  useEffect(() => {
    if (!data || !targetTs || data.messages.length === 0) return;
    let best = 0;
    let bestDiff = Number.POSITIVE_INFINITY;
    data.messages.forEach((m, i) => {
      const diff = Math.abs(m.timestamp - targetTs);
      if (diff < bestDiff) {
        bestDiff = diff;
        best = i;
      }
    });
    setHighlightIdx(best);
    requestAnimationFrame(() => {
      document
        .getElementById(`msg-${best}`)
        ?.scrollIntoView({ block: "center" });
    });
    const timer = setTimeout(() => setHighlightIdx(null), 2500);
    return () => clearTimeout(timer);
  }, [data, targetTs]);

  const effectiveViewOptions = useMemo<ViewOptions>(() => {
    if (!exportOpen) return viewOptions;
    return {
      showThinking: viewOptions.showThinking || exportOptions.includeThinking,
      showTools: viewOptions.showTools || exportOptions.includeTools,
      showSkills: viewOptions.showSkills || exportOptions.includeSkills,
      showMeta: viewOptions.showMeta || exportOptions.includeMeta,
    };
  }, [exportOpen, exportOptions, viewOptions]);

  const visibleMessages = useMemo(() => {
    if (!data) return [];
    return data.messages
      .map((msg, index) => ({
        msg,
        index,
        blocks: msg.blocks.filter((block) =>
          shouldShowBlock(block, effectiveViewOptions)
        ),
      }))
      .filter(
        ({ msg, blocks }) =>
          isMessageVisible(msg, effectiveViewOptions) && blocks.length > 0
      );
  }, [data, effectiveViewOptions]);

  const exportableVisibleIds = useMemo(() => {
    return visibleMessages
      .filter(({ msg }) => isMessageExportable(msg, exportOptions))
      .map(({ msg }) => msg.uuid);
  }, [visibleMessages, exportOptions]);
  const selectedExportableCount = useMemo(() => {
    const selected = new Set(selectedMessageUuids);
    return exportableVisibleIds.filter((uuid) => selected.has(uuid)).length;
  }, [exportableVisibleIds, selectedMessageUuids]);
  const allExportableSelected =
    exportableVisibleIds.length > 0 &&
    selectedExportableCount === exportableVisibleIds.length;
  const someExportableSelected =
    selectedExportableCount > 0 && !allExportableSelected;

  useEffect(() => {
    if (!exportOpen) {
      previousExportableVisibleIdsRef.current = exportableVisibleIds;
      return;
    }
    const previousIds = previousExportableVisibleIdsRef.current;
    const allowed = new Set(exportableVisibleIds);
    setSelectedMessageUuids((prev) => {
      const selected = new Set(prev);
      const hadAllSelected =
        previousIds.length > 0 && previousIds.every((uuid) => selected.has(uuid));
      if (hadAllSelected) return exportableVisibleIds;
      return prev.filter((uuid) => allowed.has(uuid));
    });
    previousExportableVisibleIdsRef.current = exportableVisibleIds;
  }, [exportOpen, exportableVisibleIds]);

  useEffect(() => {
    if (selectAllRef.current) {
      selectAllRef.current.indeterminate = someExportableSelected;
    }
  }, [someExportableSelected]);

  const handleToggleExport = () => {
    const next = !exportOpen;
    setExportOpen(next);
    setExportError(null);
    setExportResult(null);
    if (next) {
      setSelectedMessageUuids(exportableVisibleIds);
    }
  };

  const handleToggleSelected = (uuid: string, checked: boolean) => {
    setSelectedMessageUuids((prev) =>
      checked ? [...new Set([...prev, uuid])] : prev.filter((id) => id !== uuid)
    );
  };

  const handleToggleSelectAll = (checked: boolean) => {
    setSelectedMessageUuids(checked ? exportableVisibleIds : []);
  };

  const handleExport = async () => {
    if (!data || exporting || selectedMessageUuids.length === 0) return;
    setExporting(true);
    setExportError(null);
    setExportResult(null);
    try {
      const res = await api.exportConversation({
        sessionId: data.sessionId,
        write: true,
        lang: getCurrentLang(),
        includeThinking: exportOptions.includeThinking,
        includeTools: exportOptions.includeTools,
        includeSkills: exportOptions.includeSkills,
        includeMeta: exportOptions.includeMeta,
        includeTime: exportOptions.includeTime,
        messageUuids: selectedMessageUuids,
      });
      setExportResult(res);
    } catch (e) {
      setExportError(errMessage(e));
    } finally {
      setExporting(false);
    }
  };

  const revealExported = async () => {
    if (exportResult?.path) {
      try {
        await api.revealPath(exportResult.path);
      } catch {
        /* 文件可能被移动，忽略 */
      }
    }
  };

  const resumeCommand = data
    ? data.project
      ? `cd "${data.project}" && claude --resume ${data.sessionId}`
      : `claude --resume ${data.sessionId}`
    : "";

  return (
    <div className="mx-auto max-w-4xl px-6 py-6">
      <Button
        variant="ghost"
        size="sm"
        onClick={() => navigate(-1)}
        className="mb-4 -ml-2"
      >
        <ArrowLeft size={14} />
        {t("back")}
      </Button>

      {isLoading ? (
        <div className="space-y-3">
          {Array.from({ length: 4 }).map((_, i) => (
            <Skeleton key={i} className="h-28 w-full" />
          ))}
        </div>
      ) : isError ? (
        <CenterMessage
          icon={<MessageSquare size={28} />}
          title={t("cannotLoadConversation")}
          hint={errMessage(error)}
        />
      ) : data ? (
        <>
          <div className="mb-5">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h1 className="text-lg font-semibold text-foreground">
                {t("conversationDetailTitle")}
              </h1>
              <div className="flex flex-wrap items-center gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => copy(resumeCommand)}
                  title={resumeCommand}
                >
                  {copied ? (
                    <Check size={13} className="text-success" />
                  ) : (
                    <Terminal size={13} />
                  )}
                  {copied ? t("copied") : t("copyResumeCommand")}
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  onClick={handleToggleExport}
                  title={t("exportMarkdownTitle")}
                >
                  <Download size={13} />
                  {t("exportMarkdown")}
                </Button>
              </div>
            </div>

            <div className="mt-3 flex flex-wrap items-center gap-3 rounded-lg border border-border bg-surface px-3 py-2.5">
              <span className="text-xs text-muted">{t("showOptionsLabel")}</span>
              <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                <input
                  type="checkbox"
                  checked={viewOptions.showThinking}
                  onChange={(e) =>
                    setViewOptions((prev) => ({
                      ...prev,
                      showThinking: e.target.checked,
                    }))
                  }
                  className="accent-[var(--accent)]"
                />
                {t("showThinkingToggle")}
              </label>
              <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                <input
                  type="checkbox"
                  checked={viewOptions.showTools}
                  onChange={(e) =>
                    setViewOptions((prev) => ({
                      ...prev,
                      showTools: e.target.checked,
                    }))
                  }
                  className="accent-[var(--accent)]"
                />
                {t("showToolsToggle")}
              </label>
              <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                <input
                  type="checkbox"
                  checked={viewOptions.showSkills}
                  onChange={(e) =>
                    setViewOptions((prev) => ({
                      ...prev,
                      showSkills: e.target.checked,
                    }))
                  }
                  className="accent-[var(--accent)]"
                />
                {t("showSkillsToggle")}
              </label>
              <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                <input
                  type="checkbox"
                  checked={viewOptions.showMeta}
                  onChange={(e) =>
                    setViewOptions((prev) => ({
                      ...prev,
                      showMeta: e.target.checked,
                    }))
                  }
                  className="accent-[var(--accent)]"
                />
                {t("showMetaToggle")}
              </label>
            </div>

            {exportOpen && (
              <div className="mt-3 rounded-lg border border-border bg-surface">
                <div className="flex flex-wrap items-center gap-3 px-3 py-2.5">
                  <span className="text-xs text-muted">{t("exportSelectMessages")}</span>
                  <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs font-medium text-foreground">
                    <input
                      ref={selectAllRef}
                      type="checkbox"
                      checked={allExportableSelected}
                      disabled={exportableVisibleIds.length === 0}
                      onChange={(e) => handleToggleSelectAll(e.target.checked)}
                      className="accent-[var(--accent)] disabled:opacity-40"
                    />
                    {t("selectAllMessagesLabel")}
                  </label>
                  <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                    <input
                      type="checkbox"
                      checked={exportOptions.includeThinking}
                      onChange={(e) =>
                        setExportOptions((prev) => ({
                          ...prev,
                          includeThinking: e.target.checked,
                        }))
                      }
                      className="accent-[var(--accent)]"
                    />
                    {t("includeThinkingInExportLabel")}
                  </label>
                  <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                    <input
                      type="checkbox"
                      checked={exportOptions.includeTools}
                      onChange={(e) =>
                        setExportOptions((prev) => ({
                          ...prev,
                          includeTools: e.target.checked,
                        }))
                      }
                      className="accent-[var(--accent)]"
                    />
                    {t("includeToolsInExportLabel")}
                  </label>
                  <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                    <input
                      type="checkbox"
                      checked={exportOptions.includeSkills}
                      onChange={(e) =>
                        setExportOptions((prev) => ({
                          ...prev,
                          includeSkills: e.target.checked,
                        }))
                      }
                      className="accent-[var(--accent)]"
                    />
                    {t("includeSkillsInExportLabel")}
                  </label>
                  <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                    <input
                      type="checkbox"
                      checked={exportOptions.includeMeta}
                      onChange={(e) =>
                        setExportOptions((prev) => ({
                          ...prev,
                          includeMeta: e.target.checked,
                        }))
                      }
                      className="accent-[var(--accent)]"
                    />
                    {t("includeMetaInExportLabel")}
                  </label>
                  <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                    <input
                      type="checkbox"
                      checked={exportOptions.includeTime}
                      onChange={(e) =>
                        setExportOptions((prev) => ({
                          ...prev,
                          includeTime: e.target.checked,
                        }))
                      }
                      className="accent-[var(--accent)]"
                    />
                    {t("includeTimeInExportLabel")}
                  </label>
                  <span className="text-xs text-muted">
                    {t("selectedMessagesCount", {
                      count: formatNumber(selectedMessageUuids.length),
                    })}
                  </span>
                  <Button
                    size="sm"
                    onClick={handleExport}
                    disabled={exporting || selectedMessageUuids.length === 0}
                  >
                    {exporting ? (
                      <Spinner className="border-accent-fg/40 border-t-accent-fg" />
                    ) : (
                      <Download size={13} />
                    )}
                    {exporting ? t("exporting") : t("confirmExport")}
                  </Button>
                </div>
                <div className="border-t border-border px-3 py-2 text-xs text-muted">
                  {t("exportSelectionHint")}
                </div>
              </div>
            )}

            {exportError && (
              <p className="mt-2 text-xs text-danger">
                {t("exportFailed", { error: exportError })}
              </p>
            )}
            {exportResult && (
              <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs">
                <Check size={13} className="shrink-0 text-success" />
                <span className="text-foreground">
                  {t("exportedMessages", {
                    count: formatNumber(exportResult.messageCount),
                  })}{" "}
                  <span
                    className="font-medium"
                    title={exportResult.path ?? undefined}
                  >
                    {exportResult.path
                      ? exportResult.path.split("/").pop()
                      : t("notWrittenToFile")}
                  </span>
                </span>
                {exportResult.path && (
                  <button
                    onClick={revealExported}
                    className="flex items-center gap-1 text-accent transition-colors hover:underline"
                  >
                    <FolderOpen size={12} />
                    {t("revealInFinder")}
                  </button>
                )}
              </div>
            )}
            <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1.5 text-[11px] text-muted">
              {data.project && (
                <Link
                  to={`/project/${encodePath(data.project)}`}
                  className="flex items-center gap-1 transition-colors hover:text-accent"
                  title={data.project}
                >
                  <Folder size={11} />
                  {prettyPath(data.project)}
                </Link>
              )}
              {data.gitBranch && (
                <span className="flex items-center gap-1">
                  <GitBranch size={11} />
                  {data.gitBranch}
                </span>
              )}
              {data.version && <Badge tone="muted">CC {data.version}</Badge>}
              <span>
                {absoluteTime(data.startedAt)} ~ {absoluteTime(data.endedAt)}
              </span>
              <span>
                · {t("messagesCount", { count: formatNumber(data.messages.length) })}
              </span>
            </div>
          </div>

          {data.messages.length === 0 ? (
            <CenterMessage
              icon={<MessageSquare size={28} />}
              title={t("noMessagesInSession")}
            />
          ) : visibleMessages.length === 0 ? (
            <CenterMessage
              icon={<MessageSquare size={28} />}
              title={t("noMessagesWithCurrentFilters")}
            />
          ) : (
            <div className="space-y-3">
              {visibleMessages.map(({ msg, index, blocks }) => {
                const exportable = isMessageExportable(msg, exportOptions);
                const checked = selectedMessageUuids.includes(msg.uuid);
                return (
                  <div
                    key={msg.uuid || index}
                    id={`msg-${index}`}
                    className={cn("gap-3", exportOpen && "flex items-start")}
                  >
                    {exportOpen && (
                      <input
                        type="checkbox"
                        checked={checked}
                        disabled={!exportable}
                        onChange={(e) =>
                          handleToggleSelected(msg.uuid, e.target.checked)
                        }
                        className="mt-3 accent-[var(--accent)] disabled:opacity-40"
                      />
                    )}
                    <div className="min-w-0 flex-1">
                      <MessageBubble
                        msg={msg}
                        blocks={blocks}
                        highlighted={highlightIdx === index}
                      />
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </>
      ) : null}
    </div>
  );
}
