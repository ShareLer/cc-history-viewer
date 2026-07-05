import { useState } from "react";
import { Link } from "react-router-dom";
import {
  Check,
  ChevronDown,
  ChevronUp,
  Clipboard,
  Command,
  Copy,
  Folder,
  GitBranch,
  MessageSquare,
} from "lucide-react";
import type { PromptEntry } from "@/lib/types";
import { useCopy } from "@/hooks/useCopy";
import { useStore } from "@/store";
import { useT, type DictKey } from "@/i18n";
import {
  absoluteTime,
  cn,
  encodePath,
  formatNumber,
  prettyPath,
  relativeTime,
} from "@/lib/utils";
import { Highlight } from "./Highlight";
import { Badge } from "./ui";

const sourceLabelKey: Record<string, DictKey> = {
  history: "sourceHistory",
  conversation: "sourceConversation",
  both: "sourceBoth",
};

const kindLabelKey: Record<Exclude<PromptEntry["kind"], "human">, DictKey> = {
  command: "commandBadge",
  meta: "metaBadge",
  sidechain: "sidechainBadge",
  system: "systemBadge",
  queued: "queuedBadge",
  sdk: "sdkBadge",
  other: "otherBadge",
};

const kindTone: Record<Exclude<PromptEntry["kind"], "human">, "default" | "muted" | "outline" | "warning" | "success" | "accent"> = {
  command: "warning",
  meta: "outline",
  sidechain: "muted",
  system: "warning",
  queued: "default",
  sdk: "default",
  other: "outline",
};

export function PromptCard({
  entry,
  ranges,
  showProject = false,
}: {
  entry: PromptEntry;
  ranges?: [number, number][];
  showProject?: boolean;
}) {
  const t = useT();
  // 点击卡片内链接时清空搜索词：否则搜索结果层会一直盖住目标页面（路由其实已跳转）
  const { setQuery } = useStore();
  const [expanded, setExpanded] = useState(false);
  const { copied, copy } = useCopy();
  const collapsible = entry.charCount > 150 || entry.text.includes("\n");
  const sourceKey = sourceLabelKey[entry.source];
  const agentLabel =
    entry.agent === "codex" ? t("agentCodex") : t("agentClaudeCode");
  const conversationHref =
    entry.sessionId && entry.messageUuid
      ? `/conversation/${entry.sessionId}?m=${encodeURIComponent(entry.messageUuid)}`
      : entry.sessionId
        ? `/conversation/${entry.sessionId}?t=${entry.timestamp}`
        : "";

  return (
    <div className="rounded-xl border border-border bg-surface p-3.5 transition-colors hover:border-accent/40">
      <div
        onClick={() => collapsible && setExpanded((v) => !v)}
        onKeyDown={(e) => {
          if (!collapsible) return;
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setExpanded((v) => !v);
          }
        }}
        role={collapsible ? "button" : undefined}
        tabIndex={collapsible ? 0 : undefined}
        aria-expanded={collapsible ? expanded : undefined}
        className={cn(
          "whitespace-pre-wrap break-words text-sm leading-relaxed text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60",
          collapsible && "cursor-pointer",
          !expanded && "line-clamp-3"
        )}
      >
        <Highlight text={entry.text} ranges={ranges} />
      </div>

      {collapsible && (
        <button
          onClick={() => setExpanded((v) => !v)}
          className="mt-1 flex items-center gap-0.5 rounded text-[11px] font-medium text-accent hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
        >
          {expanded ? <ChevronUp size={12} /> : <ChevronDown size={12} />}
          {expanded ? t("collapse") : t("expandFull")}
        </button>
      )}

      <div className="mt-2.5 flex flex-wrap items-center gap-x-3 gap-y-1.5 text-[11px] text-muted">
        <span title={absoluteTime(entry.timestamp)}>
          {relativeTime(entry.timestamp)}
        </span>

        {showProject && entry.project && (
          <Link
            to={`/project/${encodePath(entry.project)}`}
            onClick={() => setQuery("")}
            className="flex items-center gap-1 transition-colors hover:text-accent"
            title={entry.project}
          >
            <Folder size={11} />
            <span className="max-w-[220px] truncate">
              {prettyPath(entry.project)}
            </span>
          </Link>
        )}

        {entry.kind !== "human" && (
          <Badge tone={kindTone[entry.kind as Exclude<PromptEntry["kind"], "human">]}>
            {entry.kind === "command" && <Command size={10} />}
            {t(kindLabelKey[entry.kind as Exclude<PromptEntry["kind"], "human">])}
          </Badge>
        )}

        <Badge tone="muted">{sourceKey ? t(sourceKey) : entry.source}</Badge>
        <Badge tone={entry.agent === "codex" ? "accent" : "muted"}>
          {agentLabel}
        </Badge>

        {entry.gitBranch && (
          <span className="flex items-center gap-1">
            <GitBranch size={11} />
            {entry.gitBranch}
          </span>
        )}

        {entry.pastedCount > 0 && (
          <span className="flex items-center gap-1">
            <Clipboard size={11} />
            {t("pastedCount", { count: entry.pastedCount })}
          </span>
        )}

        <span className="ml-auto flex items-center gap-3">
          <span>{t("charCount", { count: formatNumber(entry.charCount) })}</span>
          <button
            onClick={(e) => {
              e.stopPropagation();
              copy(entry.text);
            }}
            title={t("copyPrompt")}
            className={cn(
              "flex items-center rounded transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60",
              copied ? "text-success" : "text-muted hover:text-accent"
            )}
          >
            {copied ? <Check size={12} /> : <Copy size={12} />}
          </button>
          {entry.sessionId && (
            <Link
              to={conversationHref}
              onClick={() => setQuery("")}
              className="flex items-center gap-1 font-medium text-accent hover:underline"
            >
              <MessageSquare size={11} />
              {t("viewConversation")}
            </Link>
          )}
        </span>
      </div>
    </div>
  );
}
