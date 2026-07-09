import { memo, useEffect, useMemo, useRef, useState } from "react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router-dom";
import {
  ArrowLeft,
  Check,
  ChevronDown,
  ChevronUp,
  Copy,
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
  pathBaseName,
  prettyPath,
} from "@/lib/utils";
import { api, errMessage } from "@/lib/api";

type ViewOptions = {
  showThinking: boolean;
  showTools: boolean;
  showSkills: boolean;
  showMeta: boolean;
  showCodexCommentary: boolean;
  showSubagent: boolean;
};

type ExportOptions = {
  includeThinking: boolean;
  includeTools: boolean;
  includeSkills: boolean;
  includeMeta: boolean;
  includeCodexCommentary: boolean;
  includeSubagent: boolean;
  includeTime: boolean;
};

type BlockToggles = {
  thinking: boolean;
  tools: boolean;
  skills: boolean;
};

type TFunction = ReturnType<typeof useT>;

type UserPromptAnchor = {
  index: number;
  preview: string;
};

function blockEnabled(block: ContentBlock, toggles: BlockToggles): boolean {
  switch (block.kind) {
    case "text":
    case "image":
      return true;
    case "thinking":
      return toggles.thinking;
    case "tool_use":
    case "tool_result":
      return toggles.tools;
    case "skill":
      return toggles.skills;
    default:
      return false;
  }
}

function shouldShowBlock(
  block: ContentBlock,
  options: ViewOptions,
  msg: ChatMessage
): boolean {
  // 子代理消息整体由 showSubagent 控制（见 isMessageVisible），
  // 展开时内部块全部显示，不再受工具/思考/Skill 开关影响。
  if (msg.isSubagent) return true;
  return blockEnabled(block, {
    thinking: options.showThinking,
    tools: options.showTools,
    skills: options.showSkills,
  });
}

function shouldExportBlock(
  block: ContentBlock,
  options: ExportOptions,
  msg: ChatMessage
): boolean {
  if (msg.isSubagent) return true;
  return blockEnabled(block, {
    thinking: options.includeThinking,
    tools: options.includeTools,
    skills: options.includeSkills,
  });
}

function isMessageVisible(
  msg: ChatMessage,
  options: ViewOptions,
  agent: string
): boolean {
  // 子代理消息（sidechain / Agent / spawn_agent 等）独立开关，与工具解耦。
  if (msg.isSubagent) return options.showSubagent && msg.blocks.length > 0;
  if (
    agent === "codex" &&
    msg.role === "assistant" &&
    msg.phase === "commentary" &&
    !options.showCodexCommentary
  ) {
    return false;
  }
  if (msg.isMeta && !options.showMeta) return false;
  if (msg.metaKind === "command" && !options.showTools) return false;
  if (msg.metaKind === "skill" && !options.showSkills) return false;
  return msg.blocks.some((block) => shouldShowBlock(block, options, msg));
}

function isMessageExportable(
  msg: ChatMessage,
  options: ExportOptions,
  agent: string
): boolean {
  if (msg.isSubagent) return options.includeSubagent && msg.blocks.length > 0;
  if (
    agent === "codex" &&
    msg.role === "assistant" &&
    msg.phase === "commentary" &&
    !options.includeCodexCommentary
  ) {
    return false;
  }
  if (msg.isMeta && !options.includeMeta) return false;
  if (msg.metaKind === "command" && !options.includeTools) return false;
  if (msg.metaKind === "skill" && !options.includeSkills) return false;
  return msg.blocks.some((block) => shouldExportBlock(block, options, msg));
}

function revealOptionsForMessage(
  msg: ChatMessage,
  current: ViewOptions,
  agent: string
): ViewOptions {
  // 子代理消息只需打开 showSubagent；其内部块不受其它开关约束。
  if (msg.isSubagent) {
    return { ...current, showSubagent: true };
  }
  return {
    ...current,
    showThinking:
      current.showThinking || msg.blocks.some((block) => block.kind === "thinking"),
    showTools:
      current.showTools ||
      msg.metaKind === "command" ||
      msg.blocks.some(
        (block) => block.kind === "tool_use" || block.kind === "tool_result"
      ),
    showSkills:
      current.showSkills ||
      msg.metaKind === "skill" ||
      msg.blocks.some((block) => block.kind === "skill"),
    showMeta: current.showMeta || msg.isMeta,
    showCodexCommentary:
      current.showCodexCommentary ||
      (agent === "codex" &&
        msg.role === "assistant" &&
        msg.phase === "commentary"),
  };
}

function blockSummary(block: ContentBlock, t: TFunction): string {
  return block.kind === "tool_use"
    ? t("toolUseLabel", { name: block.toolName ?? "tool" })
    : block.kind === "thinking"
      ? t("thinkingLabel")
      : block.kind === "tool_result"
        ? t("toolResultLabel")
        : block.kind === "skill"
          ? t("skillDetailLabel")
          : block.kind;
}

function blockBody(block: ContentBlock): string {
  return block.kind === "tool_use"
    ? JSON.stringify(block.toolInput ?? {}, null, 2)
    : block.text ?? "";
}

function blockCopyText(block: ContentBlock, t: TFunction): string {
  if (block.kind === "text") return block.text ?? "";
  if (block.kind === "image") return block.text ?? t("imageFallback");
  const summary = blockSummary(block, t);
  const body = blockBody(block);
  return body ? `${summary}\n${body}` : summary;
}

function messageCopyText(blocks: ContentBlock[], t: TFunction): string {
  return blocks
    .map((block) => blockCopyText(block, t).trim())
    .filter(Boolean)
    .join("\n\n");
}

function truncatePromptPreview(text: string, maxLength = 120): string {
  const normalized = text.replace(/\s+/g, " ").trim();
  if (normalized.length <= maxLength) return normalized;
  return `${normalized.slice(0, maxLength - 1)}…`;
}

function scrollToElement(
  id: string,
  block: ScrollLogicalPosition = "center",
  behavior: ScrollBehavior = "smooth"
) {
  document.getElementById(id)?.scrollIntoView({ block, behavior });
}

function flashMessageElement(id: string) {
  const bubble = document
    .getElementById(id)
    ?.querySelector<HTMLElement>("[data-message-bubble]");
  if (!bubble) return;

  const previousTimer = bubble.dataset.jumpHighlightTimer;
  if (previousTimer) window.clearTimeout(Number(previousTimer));

  bubble.classList.add("message-jump-highlight");
  const timer = window.setTimeout(() => {
    bubble.classList.remove("message-jump-highlight");
    delete bubble.dataset.jumpHighlightTimer;
  }, 2500);
  bubble.dataset.jumpHighlightTimer = String(timer);
}

function getConversationScrollContainer(): HTMLElement | Window {
  const main = document.getElementById("conversation-top")?.closest("main");
  return main instanceof HTMLElement ? main : window;
}

function scrollContainerCenter(container: HTMLElement | Window): number {
  if (container instanceof Window) return window.innerHeight / 2;
  const rect = container.getBoundingClientRect();
  return rect.top + rect.height / 2;
}

function promptRailLineWidthClass(
  railIndex: number,
  hoveredRailIndex: number | null
): string {
  if (hoveredRailIndex == null) return "w-[11px]";

  const distance = Math.abs(railIndex - hoveredRailIndex);
  if (distance === 0) return "w-[27px]";
  if (distance === 1) return "w-[21px]";
  if (distance === 2) return "w-4";
  return "w-[11px]";
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

  const summary = blockSummary(block, t);
  const body = blockBody(block);

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

const MessageBubble = memo(function MessageBubble({
  msg,
  blocks,
  agentLabel,
  highlighted = false,
}: {
  msg: ChatMessage;
  blocks: ContentBlock[];
  agentLabel: string;
  highlighted?: boolean;
}) {
  const t = useT();
  const { copied: messageCopied, copy: copyMessage } = useCopy();
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
        : agentLabel;

  return (
    <div
      data-message-bubble
      className={cn(
        "rounded-xl border p-4 transition-shadow duration-150 motion-reduce:transition-none",
        isUser && !isMeta && "border-accent/30 bg-accent/[0.08]",
        !isUser && !isSystem && "border-border bg-surface",
        isSystem &&
          !isSkillMeta &&
          "border-dashed border-warning/30 bg-warning/[0.08]",
        isSkillMeta && "border-border bg-surface-2/80",
        isMeta && "border-dashed border-muted/40 bg-surface-2/70",
        highlighted && "message-jump-highlight"
      )}
    >
      <div className="mb-2.5 flex items-start justify-between gap-2">
        <div className="flex min-w-0 flex-wrap items-center gap-2">
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
          {msg.phase === "commentary" && (
            <Badge tone="outline">{t("codexCommentaryBadge")}</Badge>
          )}
          {msg.phase === "final_answer" && (
            <Badge tone="muted">{t("codexFinalAnswerBadge")}</Badge>
          )}
          <span className="text-[11px] text-muted">{absoluteTime(msg.timestamp)}</span>
        </div>
        <Button
          type="button"
          variant="ghost"
          size="icon-sm"
          onClick={() => copyMessage(messageCopyText(blocks, t))}
          title={t("copyMessage")}
          aria-label={t("copyMessage")}
          className={cn(
            "-mr-2 -mt-2 shrink-0 text-muted hover:text-accent",
            messageCopied && "text-success hover:text-success"
          )}
        >
          {messageCopied ? <Check size={13} /> : <Copy size={13} />}
        </Button>
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
});

function ConversationPromptRail({
  prompts,
  onJump,
  onTop,
  onBottom,
}: {
  prompts: UserPromptAnchor[];
  onJump: (index: number) => void;
  onTop: () => void;
  onBottom: () => void;
}) {
  const t = useT();
  const [activePromptIndex, setActivePromptIndex] = useState<number | null>(
    prompts[0]?.index ?? null
  );
  const [hoveredRailIndex, setHoveredRailIndex] = useState<number | null>(null);

  useEffect(() => {
    if (prompts.length === 0) return;

    const scrollContainer = getConversationScrollContainer();
    let frame = 0;

    const updateActivePrompt = () => {
      frame = 0;
      const center = scrollContainerCenter(scrollContainer);
      let nextIndex: number | null = null;
      let nearestDistance = Number.POSITIVE_INFINITY;

      for (const prompt of prompts) {
        const element = document.getElementById(`msg-${prompt.index}`);
        if (!element) continue;
        const distance = Math.abs(element.getBoundingClientRect().top - center);
        if (distance < nearestDistance) {
          nearestDistance = distance;
          nextIndex = prompt.index;
        }
      }

      setActivePromptIndex((prev) => (prev === nextIndex ? prev : nextIndex));
    };

    const scheduleUpdate = () => {
      if (frame) return;
      frame = window.requestAnimationFrame(updateActivePrompt);
    };

    scheduleUpdate();
    scrollContainer.addEventListener("scroll", scheduleUpdate, { passive: true });
    window.addEventListener("resize", scheduleUpdate);

    return () => {
      if (frame) window.cancelAnimationFrame(frame);
      scrollContainer.removeEventListener("scroll", scheduleUpdate);
      window.removeEventListener("resize", scheduleUpdate);
    };
  }, [prompts]);

  if (prompts.length === 0) return null;

  return (
    <nav
      aria-label={t("conversationPromptRail")}
      className="fixed right-5 top-1/2 z-20 hidden -translate-y-1/2 xl:block"
    >
      <div className="flex flex-col items-center px-1 py-1">
        <button
          type="button"
          onClick={onTop}
          title={t("jumpToTop")}
          aria-label={t("jumpToTop")}
          className="group relative mb-0.5 flex h-5 w-12 items-center justify-center rounded-full focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
        >
          <ChevronUp
            size={24}
            strokeWidth={2.25}
            className="text-muted/55 transition-[color,transform] duration-150 ease-out group-hover:scale-120 group-hover:text-foreground/75 group-focus-visible:scale-120 group-focus-visible:text-foreground/75"
          />
        </button>

        <div
          className="flex max-h-[min(34rem,calc(100vh-11rem))] flex-col items-center gap-0 overflow-visible py-0.5"
          onMouseLeave={() => setHoveredRailIndex(null)}
        >
          {prompts.map((prompt, railIndex) => (
            <button
              key={prompt.index}
              type="button"
              onMouseEnter={() => setHoveredRailIndex(railIndex)}
              onFocus={() => setHoveredRailIndex(railIndex)}
              onBlur={() => setHoveredRailIndex(null)}
              onClick={() => {
                setActivePromptIndex(prompt.index);
                onJump(prompt.index);
              }}
              aria-label={t("jumpToPrompt", { index: prompt.index + 1 })}
              className="group relative flex h-[9px] w-12 items-center justify-center rounded-full focus-visible:outline-none"
            >
              <span className="relative h-0.5 w-[11px]">
                <span
                  className={cn(
                    "prompt-rail-line absolute right-0 top-0 h-0.5 rounded-full transition-[width,background-color] duration-150 ease-out",
                    promptRailLineWidthClass(railIndex, hoveredRailIndex),
                    hoveredRailIndex === railIndex ||
                      activePromptIndex === prompt.index
                      ? "bg-foreground/70"
                      : "bg-muted/30"
                  )}
                />
              </span>
              <span className="pointer-events-none absolute right-full top-1/2 mr-3 hidden w-72 -translate-y-1/2 rounded-lg border border-border/80 bg-surface/95 px-3 py-2 text-left text-[11px] leading-relaxed text-foreground shadow-2xl shadow-black/10 backdrop-blur-md group-hover:block group-focus-visible:block dark:shadow-black/40">
                <span className="promptRailPreview line-clamp-4">
                  {prompt.preview || t("emptyPromptPreview")}
                </span>
              </span>
            </button>
          ))}
        </div>

        <button
          type="button"
          onClick={onBottom}
          title={t("jumpToBottom")}
          aria-label={t("jumpToBottom")}
          className="group relative mt-0.5 flex h-5 w-12 items-center justify-center rounded-full focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
        >
          <ChevronDown
            size={24}
            strokeWidth={2.25}
            className="text-muted/55 transition-[color,transform] duration-150 ease-out group-hover:scale-120 group-hover:text-foreground/75 group-focus-visible:scale-120 group-focus-visible:text-foreground/75"
          />
        </button>
      </div>
    </nav>
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
  const { copy: copyMarkdown } = useCopy();

  const [searchParams] = useSearchParams();
  const targetUuid = searchParams.get("m");
  const targetTs = Number(searchParams.get("t")) || null;
  const [highlightIdx, setHighlightIdx] = useState<number | null>(null);

  const [viewOptions, setViewOptions] = useState<ViewOptions>({
    showThinking: false,
    showTools: false,
    showSkills: false,
    showMeta: false,
    showCodexCommentary: false,
    showSubagent: false,
  });

  const [exportOpen, setExportOpen] = useState(false);
  const [exportOptions, setExportOptions] = useState<ExportOptions>({
    includeThinking: false,
    includeTools: false,
    includeSkills: false,
    includeMeta: false,
    includeCodexCommentary: false,
    includeSubagent: false,
    includeTime: false,
  });
  const [selectedMessageKeys, setSelectedMessageKeys] = useState<string[]>([]);
  const selectAllRef = useRef<HTMLInputElement>(null);
  const previousExportableVisibleKeysRef = useRef<string[]>([]);
  const jumpedTargetRef = useRef<string | null>(null);
  const [exportAction, setExportAction] = useState<"file" | "clipboard" | null>(
    null
  );
  const exportBusy = exportAction !== null;
  const [exportResult, setExportResult] =
    useState<ConversationExportResult | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);
  const [lastExportTarget, setLastExportTarget] = useState<
    "file" | "clipboard" | null
  >(null);

  const effectiveViewOptions = useMemo<ViewOptions>(() => {
    if (!exportOpen) return viewOptions;
    return {
      showThinking: viewOptions.showThinking || exportOptions.includeThinking,
      showTools: viewOptions.showTools || exportOptions.includeTools,
      showSkills: viewOptions.showSkills || exportOptions.includeSkills,
      showMeta: viewOptions.showMeta || exportOptions.includeMeta,
      showCodexCommentary:
        viewOptions.showCodexCommentary || exportOptions.includeCodexCommentary,
      showSubagent: viewOptions.showSubagent || exportOptions.includeSubagent,
    };
  }, [exportOpen, exportOptions, viewOptions]);

  const targetIndex = useMemo(() => {
    if (!data || data.messages.length === 0) return undefined;
    let best = targetUuid
      ? data.messages.findIndex((m) => m.uuid === targetUuid)
      : -1;
    if (best < 0) {
      if (!targetTs) return undefined;
      let bestDiff = Number.POSITIVE_INFINITY;
      data.messages.forEach((m, i) => {
        const diff = Math.abs(m.timestamp - targetTs);
        if (diff < bestDiff) {
          bestDiff = diff;
          best = i;
        }
      });
    }
    return best >= 0 ? best : undefined;
  }, [data, targetTs, targetUuid]);

  useEffect(() => {
    const jumpKey = sessionId ? `${sessionId}:${targetUuid ?? ""}:${targetTs ?? ""}` : null;
    if (!jumpKey || jumpedTargetRef.current !== jumpKey) {
      setHighlightIdx(null);
    }
    if (!data || targetIndex == null) return;
    const target = data.messages[targetIndex];
    if (!target) return;
    if (!isMessageVisible(target, effectiveViewOptions, data.agent)) {
      setViewOptions((prev) => {
        const next = revealOptionsForMessage(target, prev, data.agent);
        return Object.is(next, prev) ||
          (next.showThinking === prev.showThinking &&
            next.showTools === prev.showTools &&
            next.showSkills === prev.showSkills &&
            next.showMeta === prev.showMeta &&
            next.showCodexCommentary === prev.showCodexCommentary &&
            next.showSubagent === prev.showSubagent)
          ? prev
          : next;
      });
      return;
    }
    if (jumpedTargetRef.current === jumpKey) return;
    jumpedTargetRef.current = jumpKey;
    setHighlightIdx(targetIndex);
    requestAnimationFrame(() => {
      document
        .getElementById(`msg-${targetIndex}`)
        ?.scrollIntoView({ block: "center" });
    });
  }, [data, effectiveViewOptions, sessionId, targetIndex, targetTs, targetUuid]);

  useEffect(() => {
    if (highlightIdx == null) return;
    const timer = setTimeout(() => setHighlightIdx(null), 2500);
    return () => clearTimeout(timer);
  }, [highlightIdx]);

  const visibleMessages = useMemo(() => {
    if (!data) return [];
    return data.messages
      .map((msg, index) => ({
        msg,
        index,
        blocks: msg.blocks.filter((block) =>
          shouldShowBlock(block, effectiveViewOptions, msg)
        ),
      }))
      .filter(
        ({ msg, blocks }) =>
          isMessageVisible(msg, effectiveViewOptions, data.agent) &&
          blocks.length > 0
      );
  }, [data, effectiveViewOptions]);

  const userPromptAnchors = useMemo<UserPromptAnchor[]>(() => {
    return visibleMessages
      .filter(({ msg }) => msg.role === "user" && !msg.isMeta)
      .map(({ index, blocks }) => ({
        index,
        preview: truncatePromptPreview(messageCopyText(blocks, t)),
      }));
  }, [t, visibleMessages]);

  const exportableVisibleKeys = useMemo(() => {
    if (!data) return [];
    return visibleMessages
      .filter(({ msg }) => isMessageExportable(msg, exportOptions, data.agent))
      .map(({ index }) => String(index));
  }, [data, visibleMessages, exportOptions]);
  const selectedExportableCount = useMemo(() => {
    const selected = new Set(selectedMessageKeys);
    return exportableVisibleKeys.filter((key) => selected.has(key)).length;
  }, [exportableVisibleKeys, selectedMessageKeys]);
  const allExportableSelected =
    exportableVisibleKeys.length > 0 &&
    selectedExportableCount === exportableVisibleKeys.length;
  const someExportableSelected =
    selectedExportableCount > 0 && !allExportableSelected;

  useEffect(() => {
    if (!exportOpen) {
      previousExportableVisibleKeysRef.current = exportableVisibleKeys;
      return;
    }
    const previousKeys = previousExportableVisibleKeysRef.current;
    const allowed = new Set(exportableVisibleKeys);
    setSelectedMessageKeys((prev) => {
      const selected = new Set(prev);
      const hadAllSelected =
        previousKeys.length > 0 && previousKeys.every((key) => selected.has(key));
      if (hadAllSelected) return exportableVisibleKeys;
      return prev.filter((key) => allowed.has(key));
    });
    previousExportableVisibleKeysRef.current = exportableVisibleKeys;
  }, [exportOpen, exportableVisibleKeys]);

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
    setLastExportTarget(null);
    if (next) {
      setSelectedMessageKeys(exportableVisibleKeys);
    }
  };

  const handleToggleSelected = (key: string, checked: boolean) => {
    setSelectedMessageKeys((prev) =>
      checked ? [...new Set([...prev, key])] : prev.filter((id) => id !== key)
    );
  };

  const handleToggleSelectAll = (checked: boolean) => {
    setSelectedMessageKeys(checked ? exportableVisibleKeys : []);
  };

  const scrollToConversationTop = () => {
    scrollToElement("conversation-top", "start", "auto");
  };

  const scrollToConversationBottom = () => {
    scrollToElement("conversation-bottom", "end", "auto");
  };

  const scrollToPrompt = (index: number) => {
    const targetId = `msg-${index}`;
    flashMessageElement(targetId);
    scrollToElement(targetId, "center", "auto");
  };

  const selectedMessageIndexes = () => {
    if (!data) return [];
    return selectedMessageKeys
      .map((key) => Number(key))
      .filter(
        (index) =>
          Number.isInteger(index) && index >= 0 && index < data.messages.length
      );
  };

  const exportConversation = async (write: boolean) => {
    if (!data) return null;
    const messageIndexes = selectedMessageIndexes();
    if (messageIndexes.length === 0) return;
    return api.exportConversation({
      sessionId: data.sessionId,
      write,
      lang: getCurrentLang(),
      includeThinking: exportOptions.includeThinking,
      includeTools: exportOptions.includeTools,
      includeSkills: exportOptions.includeSkills,
      includeMeta: exportOptions.includeMeta,
      includeCodexCommentary: exportOptions.includeCodexCommentary,
      includeSubagent: exportOptions.includeSubagent,
      includeTime: exportOptions.includeTime,
      messageIndexes,
    });
  };

  const handleExportToFile = async () => {
    if (!data || exportBusy || selectedMessageKeys.length === 0) return;
    setExportAction("file");
    setExportError(null);
    try {
      const res = await exportConversation(true);
      if (!res) return;
      setExportResult(res);
      setLastExportTarget("file");
    } catch (e) {
      setExportError(errMessage(e));
    } finally {
      setExportAction(null);
    }
  };

  const handleCopyMarkdown = async () => {
    if (!data || exportBusy || selectedMessageKeys.length === 0) return;
    setExportAction("clipboard");
    setExportError(null);
    try {
      const res = await exportConversation(false);
      if (!res) return;
      const ok = await copyMarkdown(res.markdown);
      if (!ok) throw new Error(t("copyMarkdownFailed"));
      setExportResult(res);
      setLastExportTarget("clipboard");
    } catch (e) {
      setExportError(errMessage(e));
    } finally {
      setExportAction(null);
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

  const agentLabel = data?.agent === "codex" ? t("agentCodex") : t("agentClaudeCode");
  const hasSubagent = useMemo(
    () => data?.messages.some((m) => m.isSubagent) ?? false,
    [data]
  );
  const resumeCommand = data && data.agent === "claudeCode"
    ? data.project
      ? `cd "${data.project}" && claude --resume ${data.sessionId}`
      : `claude --resume ${data.sessionId}`
    : "";

  return (
    <div id="conversation-top" className="mx-auto max-w-4xl px-6 py-6">
      <Button
        variant="ghost"
        size="sm"
        onClick={() =>
          window.history.state?.idx > 0 ? navigate(-1) : navigate("/")
        }
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
          <ConversationPromptRail
            prompts={userPromptAnchors}
            onJump={scrollToPrompt}
            onTop={scrollToConversationTop}
            onBottom={scrollToConversationBottom}
          />

          <div className="mb-5">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h1 className="text-lg font-semibold text-foreground">
                {t("conversationDetailTitle")}
              </h1>
              <div className="flex flex-wrap items-center gap-2">
                {data.agent === "claudeCode" && (
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
                )}
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
              {data.agent === "codex" && (
                <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                  <input
                    type="checkbox"
                    checked={viewOptions.showCodexCommentary}
                    onChange={(e) =>
                      setViewOptions((prev) => ({
                        ...prev,
                        showCodexCommentary: e.target.checked,
                      }))
                    }
                    className="accent-[var(--accent)]"
                  />
                  {t("showCodexCommentaryToggle")}
                </label>
              )}
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
              {hasSubagent && (
                <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                  <input
                    type="checkbox"
                    checked={viewOptions.showSubagent}
                    onChange={(e) =>
                      setViewOptions((prev) => ({
                        ...prev,
                        showSubagent: e.target.checked,
                      }))
                    }
                    className="accent-[var(--accent)]"
                  />
                  {t("showSubagentToggle")}
                </label>
              )}
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
                      disabled={exportableVisibleKeys.length === 0}
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
                  {hasSubagent && (
                    <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                      <input
                        type="checkbox"
                        checked={exportOptions.includeSubagent}
                        onChange={(e) =>
                          setExportOptions((prev) => ({
                            ...prev,
                            includeSubagent: e.target.checked,
                          }))
                        }
                        className="accent-[var(--accent)]"
                      />
                      {t("includeSubagentInExportLabel")}
                    </label>
                  )}
                  {data.agent === "codex" && (
                    <label className="flex cursor-pointer select-none items-center gap-1.5 text-xs text-foreground">
                      <input
                        type="checkbox"
                        checked={exportOptions.includeCodexCommentary}
                        onChange={(e) =>
                          setExportOptions((prev) => ({
                            ...prev,
                            includeCodexCommentary: e.target.checked,
                          }))
                        }
                        className="accent-[var(--accent)]"
                      />
                      {t("includeCodexCommentaryInExportLabel")}
                    </label>
                  )}
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
                      count: formatNumber(selectedExportableCount),
                    })}
                  </span>
                  <Button
                    size="sm"
                    onClick={handleExportToFile}
                    disabled={selectedExportableCount === 0}
                    aria-disabled={exportBusy}
                    aria-busy={exportAction === "file"}
                  >
                    <Download size={13} />
                    {t("exportToFile")}
                  </Button>
                  <Button
                    size="sm"
                    variant="outline"
                    onClick={handleCopyMarkdown}
                    disabled={selectedExportableCount === 0}
                    aria-disabled={exportBusy}
                    aria-busy={exportAction === "clipboard"}
                  >
                    <Copy size={13} />
                    {t("copyMarkdownToClipboard")}
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
                {lastExportTarget === "clipboard" ? (
                  <span className="text-foreground">
                    {t("copiedMarkdownMessages", {
                      count: formatNumber(exportResult.messageCount),
                    })}
                  </span>
                ) : (
                  <span className="text-foreground">
                    {t("exportedMessages", {
                      count: formatNumber(exportResult.messageCount),
                    })}{" "}
                    <span
                      className="font-medium"
                      title={exportResult.path ?? undefined}
                    >
                      {exportResult.path
                        ? pathBaseName(exportResult.path)
                        : t("notWrittenToFile")}
                    </span>
                  </span>
                )}
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
              <Badge tone={data.agent === "codex" ? "accent" : "muted"}>
                {agentLabel}
              </Badge>
              {data.version && (
                <Badge tone="muted">
                  {agentLabel} {data.version}
                </Badge>
              )}
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
                const exportable = isMessageExportable(
                  msg,
                  exportOptions,
                  data.agent
                );
                const messageKey = String(index);
                const checked = selectedMessageKeys.includes(messageKey);
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
                          handleToggleSelected(messageKey, e.target.checked)
                        }
                        className="mt-3 accent-[var(--accent)] disabled:opacity-40"
                      />
                    )}
                    <div className="min-w-0 flex-1">
                      <MessageBubble
                        msg={msg}
                        blocks={blocks}
                        agentLabel={agentLabel}
                        highlighted={highlightIdx === index}
                      />
                    </div>
                  </div>
                );
              })}
            </div>
          )}
          <div id="conversation-bottom" className="h-px" />
        </>
      ) : null}
    </div>
  );
}
