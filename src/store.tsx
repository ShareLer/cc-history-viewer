// 全局轻量状态：主题、搜索词、搜索范围、命令过滤、当前文件夹。

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import type { PromptVisibility } from "@/lib/types";

type Theme = "dark" | "light";
export type SearchScope = "global" | "folder";

const DEFAULT_PROMPT_VISIBILITY: PromptVisibility = {
  includeCommands: true,
  includeMeta: false,
  includeSidechain: false,
  includeSystem: false,
  includeQueued: false,
  includeSdk: false,
  includeOther: false,
};

function loadPromptVisibility(): PromptVisibility {
  const raw = localStorage.getItem("cchv-prompt-visibility");
  if (raw) {
    try {
      const parsed = JSON.parse(raw) as Partial<PromptVisibility>;
      return { ...DEFAULT_PROMPT_VISIBILITY, ...parsed };
    } catch {
      // fall through
    }
  }
  const legacyIncludeCommands =
    localStorage.getItem("cchv-include-commands") !== "false";
  return {
    ...DEFAULT_PROMPT_VISIBILITY,
    includeCommands: legacyIncludeCommands,
  };
}

interface Store {
  theme: Theme;
  toggleTheme: () => void;

  /** 搜索框即时输入值 */
  query: string;
  setQuery: (q: string) => void;

  /** 搜索范围：全局 / 当前文件夹 */
  scope: SearchScope;
  setScope: (s: SearchScope) => void;

  /** 是否在结果中包含斜杠命令（/clear 等） */
  includeCommands: boolean;
  setIncludeCommands: (b: boolean) => void;

  /** 其它 prompt 类型的显示开关 */
  promptVisibility: PromptVisibility;
  setPromptVisibility: (key: keyof PromptVisibility, value: boolean) => void;

  /** 当前进入的文件夹（真实路径），用于「当前文件夹」搜索 */
  currentProject: string | null;
  currentProjectName: string | null;
  setCurrentProject: (path: string | null, name?: string | null) => void;
}

const StoreContext = createContext<Store | null>(null);

export function StoreProvider({ children }: { children: ReactNode }) {
  const [theme, setTheme] = useState<Theme>(() =>
    localStorage.getItem("cchv-theme") === "light" ? "light" : "dark"
  );
  const [query, setQuery] = useState("");
  const [scope, setScope] = useState<SearchScope>("global");
  const [promptVisibility, setPromptVisibilityState] = useState<PromptVisibility>(
    () => loadPromptVisibility()
  );
  const [currentProject, setCurrentProjectState] = useState<string | null>(
    null
  );
  const [currentProjectName, setCurrentProjectName] = useState<string | null>(
    null
  );

  useEffect(() => {
    document.documentElement.classList.toggle("dark", theme === "dark");
    localStorage.setItem("cchv-theme", theme);
  }, [theme]);

  useEffect(() => {
    localStorage.setItem(
      "cchv-prompt-visibility",
      JSON.stringify(promptVisibility)
    );
    localStorage.setItem(
      "cchv-include-commands",
      String(promptVisibility.includeCommands)
    );
  }, [promptVisibility]);

  const toggleTheme = useCallback(
    () => setTheme((t) => (t === "dark" ? "light" : "dark")),
    []
  );

  const setIncludeCommands = useCallback((b: boolean) => {
    setPromptVisibilityState((v) => ({ ...v, includeCommands: b }));
  }, []);

  const setPromptVisibility = useCallback(
    (key: keyof PromptVisibility, value: boolean) => {
      setPromptVisibilityState((v) => ({ ...v, [key]: value }));
    },
    []
  );

  const setCurrentProject = useCallback(
    (path: string | null, name: string | null = null) => {
      setCurrentProjectState(path);
      setCurrentProjectName(name);
      if (!path) setScope("global");
    },
    []
  );

  const value = useMemo<Store>(
    () => ({
      theme,
      toggleTheme,
      query,
      setQuery,
      scope,
      setScope,
      includeCommands: promptVisibility.includeCommands,
      setIncludeCommands,
      promptVisibility,
      setPromptVisibility,
      currentProject,
      currentProjectName,
      setCurrentProject,
    }),
    [
      theme,
      toggleTheme,
      query,
      scope,
      setIncludeCommands,
      promptVisibility,
      setPromptVisibility,
      currentProject,
      currentProjectName,
      setCurrentProject,
    ]
  );

  return (
    <StoreContext.Provider value={value}>{children}</StoreContext.Provider>
  );
}

export function useStore(): Store {
  const v = useContext(StoreContext);
  if (!v) throw new Error("useStore 必须在 StoreProvider 内使用");
  return v;
}
