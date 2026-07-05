import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { Outlet, useLocation, useNavigate } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Languages, Layers, RefreshCw, Settings, Terminal } from "lucide-react";
import { useStore } from "@/store";
import { useLang, useT } from "@/i18n";
import { api } from "@/lib/api";
import { cn, decodePath, pathBaseName } from "@/lib/utils";
import { SearchBar } from "./SearchBar";
import { SettingsDialog } from "./SettingsDialog";
import { Sidebar } from "./Sidebar";
import { ThemeToggle } from "./ThemeToggle";
import { Button } from "./ui";
import { SearchResults } from "@/pages/SearchResults";

const MIN_REFRESH_FEEDBACK_MS = 500;

function delay(ms: number) {
  return new Promise<void>((resolve) => window.setTimeout(resolve, ms));
}

export function Layout() {
  const {
    query,
    includeCommands,
    setIncludeCommands,
    setQuery,
    setCurrentProject,
    setScope,
  } = useStore();
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const location = useLocation();
  const t = useT();
  const { lang, setLang } = useLang();
  const mainRef = useRef<HTMLElement>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const lastProjectPathRef = useRef<string | null>(null);

  // 根据路由派生「当前文件夹」，使其不受搜索时页面卸载的影响
  useEffect(() => {
    const m = location.pathname.match(/^\/project\/(.+)$/);
    if (m) {
      const path = decodePath(m[1]);
      const name = pathBaseName(path);
      setCurrentProject(path, name);
      if (lastProjectPathRef.current !== path) {
        setScope("folder");
        lastProjectPathRef.current = path;
      }
    } else {
      setCurrentProject(null);
      lastProjectPathRef.current = null;
    }
  }, [location.pathname, setCurrentProject, setScope]);

  const searching = query.trim().length > 0;

  useLayoutEffect(() => {
    mainRef.current?.scrollTo({ top: 0, left: 0 });
  }, [location.pathname]);

  const handleRefresh = async () => {
    if (refreshing) return;
    const startedAt = performance.now();
    setRefreshing(true);
    try {
      await api.refreshIndex();
      await queryClient.invalidateQueries({
        predicate: (query) => query.queryKey[0] !== "settings",
        refetchType: "active",
      });
    } catch {
      // 刷新失败时保留当前内容；再次点击可重试
    } finally {
      const remaining = MIN_REFRESH_FEEDBACK_MS - (performance.now() - startedAt);
      if (remaining > 0) {
        await delay(remaining);
      }
      setRefreshing(false);
    }
  };

  return (
    <div className="flex h-screen flex-col">
      <header className="flex h-14 shrink-0 items-center gap-3 border-b border-border bg-surface px-4">
        <button
          onClick={() => {
            setQuery("");
            navigate("/");
          }}
          className="flex shrink-0 items-center gap-2 rounded-lg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
          title={t("backHome")}
        >
          <span
            className="flex h-7 w-7 items-center justify-center rounded-lg text-white"
            style={{
              background: "linear-gradient(135deg, #7c6cff, #a855f7)",
            }}
          >
            <Layers size={16} />
          </span>
          <span className="hidden text-sm font-semibold text-foreground sm:inline">
            CC History Viewer
          </span>
        </button>

        <SearchBar />

        <button
          onClick={() => setIncludeCommands(!includeCommands)}
          title={includeCommands ? t("commandsShownTitle") : t("commandsHiddenTitle")}
          className={cn(
            "flex h-9 shrink-0 items-center gap-1.5 rounded-lg border px-2.5 text-xs font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60",
            includeCommands
              ? "border-accent/40 bg-accent/15 text-accent"
              : "border-border text-muted hover:text-foreground"
          )}
        >
          <Terminal size={14} />
          {t("commandsToggle")}
        </button>

        <Button
          variant="ghost"
          size="icon"
          onClick={handleRefresh}
          disabled={refreshing}
          title={t("refreshTitle")}
        >
          <RefreshCw
            size={16}
            className={cn(refreshing && "animate-spin")}
          />
        </Button>

        <Button
          variant="ghost"
          size="icon"
          onClick={() => setSettingsOpen(true)}
          title={t("settingsButtonTitle")}
        >
          <Settings size={16} />
        </Button>

        <button
          onClick={() => setLang(lang === "zh" ? "en" : "zh")}
          title={t("switchLanguage")}
          className="flex h-9 shrink-0 items-center gap-1 rounded-lg border border-border px-2 text-xs font-medium text-muted transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
        >
          <Languages size={14} />
          {t("langBadge")}
        </button>

        <ThemeToggle />
      </header>

      <SettingsDialog
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
      />

      <div className="flex flex-1 overflow-hidden">
        <Sidebar />
        <main ref={mainRef} className="flex-1 overflow-y-auto">
          {searching ? <SearchResults /> : <Outlet />}
        </main>
      </div>
    </div>
  );
}
