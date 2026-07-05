import { useMemo, useState, type ReactNode } from "react";
import { Link, useParams } from "react-router-dom";
import { Folder, GitBranch, ListTree, MessagesSquare } from "lucide-react";
import { useStore } from "@/store";
import {
  useProjectPrompts,
  useProjectSessions,
  useProjects,
} from "@/hooks/queries";
import { PromptList } from "@/components/PromptList";
import { PromptVisibilityFilters } from "@/components/PromptVisibilityFilters";
import { Badge, CenterMessage, Skeleton } from "@/components/ui";
import { useT, type DictKey } from "@/i18n";
import type { AgentKind, SessionSummary, SortMode } from "@/lib/types";
import { absoluteTime, cn, formatNumber, pathBaseName } from "@/lib/utils";
import { errMessage } from "@/lib/api";

const sortOptions: { value: SortMode; labelKey: DictKey }[] = [
  { value: "newest", labelKey: "sortNewest" },
  { value: "oldest", labelKey: "sortOldest" },
  { value: "longest", labelKey: "sortLongest" },
];

type SessionAgentFilter = "all" | AgentKind;
type PromptAgentFilter = "all" | AgentKind;

const sessionAgentFilters: {
  value: SessionAgentFilter;
  labelKey: DictKey;
}[] = [
  { value: "all", labelKey: "agentFilterAll" },
  { value: "claudeCode", labelKey: "agentClaudeCode" },
  { value: "codex", labelKey: "agentCodex" },
];
const promptAgentFilters: {
  value: PromptAgentFilter;
  labelKey: DictKey;
}[] = sessionAgentFilters;

function ListSkeleton() {
  return (
    <div className="space-y-2.5">
      {Array.from({ length: 6 }).map((_, i) => (
        <Skeleton key={i} className="h-20 w-full" />
      ))}
    </div>
  );
}

function TabButton({
  active,
  onClick,
  icon,
  children,
}: {
  active: boolean;
  onClick: () => void;
  icon: ReactNode;
  children: ReactNode;
}) {
  return (
    <button
      role="radio"
      aria-checked={active}
      onClick={onClick}
      className={cn(
        "flex items-center gap-1.5 rounded-md px-3 py-1.5 text-xs font-medium transition-colors",
        active
          ? "bg-accent text-accent-fg"
          : "text-muted hover:text-foreground"
      )}
    >
      {icon}
      {children}
    </button>
  );
}

function SessionRow({ session }: { session: SessionSummary }) {
  const t = useT();
  const agentLabel =
    session.agent === "codex" ? t("agentCodex") : t("agentClaudeCode");
  return (
    <Link
      to={`/conversation/${session.sessionId}`}
      className="block rounded-xl border border-border bg-surface p-3.5 transition-colors hover:border-accent/40"
    >
      <div className="line-clamp-2 text-sm font-medium text-foreground">
        {session.title || t("untitledSession")}
      </div>
      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted">
        <Badge tone={session.agent === "codex" ? "accent" : "muted"}>
          {agentLabel}
        </Badge>
        <span>{absoluteTime(session.startedAt)}</span>
        <span>
          {t("messagesCount", { count: formatNumber(session.messageCount) })}
        </span>
        {session.gitBranch && (
          <span className="flex items-center gap-1">
            <GitBranch size={11} />
            {session.gitBranch}
          </span>
        )}
      </div>
    </Link>
  );
}

export function ProjectPrompts() {
  const params = useParams();
  const projectPath = params.encoded ?? "";
  const name = pathBaseName(projectPath);

  // 「当前文件夹」由 Layout 根据路由统一登记，这里只读 promptVisibility
  const { promptVisibility } = useStore();
  const t = useT();
  const [sort, setSort] = useState<SortMode>("newest");
  const [tab, setTab] = useState<"prompts" | "sessions">("sessions");
  const [promptAgentFilter, setPromptAgentFilter] =
    useState<PromptAgentFilter>("all");
  const [sessionAgentFilter, setSessionAgentFilter] =
    useState<SessionAgentFilter>("all");

  const projectsQ = useProjects();
  const info = projectsQ.data?.find((p) => p.path === projectPath);
  const promptsQ = useProjectPrompts(projectPath, sort, promptVisibility);
  const sessionsQ = useProjectSessions(
    tab === "sessions" ? projectPath : null
  );

  // memo 保持引用稳定：PromptList 以 items 引用变化作为重置分批的信号
  const promptItems = useMemo(
    () =>
      (promptsQ.data ?? [])
        .filter((entry) =>
          promptAgentFilter === "all" ? true : entry.agent === promptAgentFilter
        )
        .map((entry) => ({ entry })),
    [promptAgentFilter, promptsQ.data]
  );
  const filteredSessions = useMemo(() => {
    const sessions = sessionsQ.data ?? [];
    if (sessionAgentFilter === "all") return sessions;
    return sessions.filter((session) => session.agent === sessionAgentFilter);
  }, [sessionAgentFilter, sessionsQ.data]);

  return (
    <div className="mx-auto max-w-4xl px-6 py-6">
      <div className="mb-5">
        <div className="flex items-center gap-2">
          <Folder size={18} className="text-accent" />
          <h1 className="text-lg font-semibold text-foreground">{name}</h1>
        </div>
        <p className="mt-1 break-all text-xs text-muted">{projectPath}</p>
        {info && (
          <div className="mt-2.5 flex flex-wrap items-center gap-2">
            <Badge tone="accent">
              {t("promptCountLabel", {
                count: formatNumber(info.promptCount),
              })}
            </Badge>
            {info.commandCount > 0 && (
              <Badge tone="muted">
                {t("commandCountLabel", {
                  count: formatNumber(info.commandCount),
                })}
              </Badge>
            )}
            {info.hasConversations && (
              <Badge tone="muted">
                {t("sessionCountLabel", {
                  count: formatNumber(info.sessionCount),
                })}
              </Badge>
            )}
          </div>
        )}
      </div>

      <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
        <div
          role="radiogroup"
          aria-label={t("projectViewModeLabel")}
          className="flex items-center rounded-lg border border-border bg-surface p-0.5"
        >
          <TabButton
            active={tab === "sessions"}
            onClick={() => setTab("sessions")}
            icon={<MessagesSquare size={13} />}
          >
            {t("sessionsTab")}
          </TabButton>
          <TabButton
            active={tab === "prompts"}
            onClick={() => setTab("prompts")}
            icon={<ListTree size={13} />}
          >
            {t("promptsTab")}
          </TabButton>
        </div>

        {tab === "prompts" && (
          <div className="flex flex-wrap items-center gap-2">
            <div
              role="radiogroup"
              aria-label={t("promptAgentFilterLabel")}
              className="flex items-center gap-1 rounded-lg border border-border bg-surface p-0.5"
            >
              {promptAgentFilters.map((o) => (
                <button
                  key={o.value}
                  role="radio"
                  aria-checked={promptAgentFilter === o.value}
                  onClick={() => setPromptAgentFilter(o.value)}
                  className={cn(
                    "rounded-md px-2.5 py-1 text-xs font-medium transition-colors",
                    promptAgentFilter === o.value
                      ? "bg-accent text-accent-fg"
                      : "text-muted hover:text-foreground"
                  )}
                >
                  {t(o.labelKey)}
                </button>
              ))}
            </div>
            <div
              role="radiogroup"
              aria-label={t("sortModeLabel")}
              className="flex items-center gap-1 rounded-lg border border-border bg-surface p-0.5"
            >
              {sortOptions.map((o) => (
                <button
                  key={o.value}
                  role="radio"
                  aria-checked={sort === o.value}
                  onClick={() => setSort(o.value)}
                  className={cn(
                    "rounded-md px-2.5 py-1 text-xs font-medium transition-colors",
                    sort === o.value
                      ? "bg-accent text-accent-fg"
                      : "text-muted hover:text-foreground"
                  )}
                >
                  {t(o.labelKey)}
                </button>
              ))}
            </div>
          </div>
        )}
        {tab === "sessions" && (
          <div
            role="radiogroup"
            aria-label={t("sessionAgentFilterLabel")}
            className="flex items-center gap-1 rounded-lg border border-border bg-surface p-0.5"
          >
            {sessionAgentFilters.map((o) => (
              <button
                key={o.value}
                role="radio"
                aria-checked={sessionAgentFilter === o.value}
                onClick={() => setSessionAgentFilter(o.value)}
                className={cn(
                  "rounded-md px-2.5 py-1 text-xs font-medium transition-colors",
                  sessionAgentFilter === o.value
                    ? "bg-accent text-accent-fg"
                    : "text-muted hover:text-foreground"
                )}
              >
                {t(o.labelKey)}
              </button>
            ))}
          </div>
        )}
      </div>

      {tab === "prompts" ? (
        <>
          <PromptVisibilityFilters className="mb-3" />
          {promptsQ.isLoading ? (
            <ListSkeleton />
          ) : promptsQ.isError ? (
            <CenterMessage
              icon={<Folder size={28} />}
              title={t("loadFailed")}
              hint={errMessage(promptsQ.error)}
            />
          ) : promptItems.length > 0 ? (
            <PromptList items={promptItems} />
          ) : (
            <CenterMessage
              icon={<Folder size={28} />}
              title={t("noPromptsInFolder")}
              hint={t("noPromptsInFolderHint")}
            />
          )}
        </>
      ) : sessionsQ.isLoading ? (
        <ListSkeleton />
      ) : sessionsQ.isError ? (
        <CenterMessage
          icon={<MessagesSquare size={28} />}
          title={t("loadFailed")}
          hint={errMessage(sessionsQ.error)}
        />
      ) : sessionsQ.data && sessionsQ.data.length > 0 ? (
        filteredSessions.length > 0 ? (
          <div className="space-y-2.5">
            {filteredSessions.map((s) => (
              <SessionRow key={s.sessionId} session={s} />
            ))}
          </div>
        ) : (
          <CenterMessage
            icon={<MessagesSquare size={28} />}
            title={t("noSessionsForAgentFilter")}
            hint={t("noSessionsForAgentFilterHint")}
          />
        )
      ) : (
        <CenterMessage
          icon={<MessagesSquare size={28} />}
          title={t("noConversationsInFolder")}
          hint={t("noConversationsInFolderHint")}
        />
      )}
    </div>
  );
}
