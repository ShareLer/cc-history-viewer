// Tauri Commands 的前端封装。
// 注意：invoke 的参数键用 camelCase，Tauri 会自动映射到 Rust 的 snake_case 参数。

import { invoke } from "@tauri-apps/api/core";
import { translate } from "@/i18n";
import type {
  AppStats,
  ConversationDetail,
  ConversationExportParams,
  ConversationExportResult,
  ExportParams,
  ExportResult,
  IndexMeta,
  ProjectInfo,
  PromptEntry,
  PromptVisibility,
  SearchResult,
  SessionSummary,
  SettingsInput,
  SettingsView,
  SortMode,
} from "./types";

export const api = {
  getProjects: () => invoke<ProjectInfo[]>("get_projects"),

  getProjectPrompts: (
    project: string,
    sort: SortMode,
    visibility: PromptVisibility
  ) =>
    invoke<PromptEntry[]>("get_project_prompts", { project, sort, visibility }),

  getRecentPrompts: (limit: number, visibility: PromptVisibility) =>
    invoke<PromptEntry[]>("get_recent_prompts", { limit, visibility }),

  searchPrompts: (
    query: string,
    projectFilter: string | null,
    visibility: PromptVisibility
  ) =>
    invoke<SearchResult[]>("search_prompts", { query, projectFilter, visibility }),

  getStats: () => invoke<AppStats>("get_stats"),

  getProjectSessions: (project: string) =>
    invoke<SessionSummary[]>("get_project_sessions", { project }),

  getConversation: (sessionId: string) =>
    invoke<ConversationDetail>("get_conversation", { sessionId }),

  getIndexMeta: () => invoke<IndexMeta>("get_index_meta"),

  refreshIndex: () => invoke<IndexMeta>("refresh_index"),

  rebuildIndex: () => invoke<IndexMeta>("rebuild_index"),

  buildExport: (p: ExportParams) =>
    invoke<ExportResult>("build_prompt_export", {
      startDate: p.startDate,
      endDate: p.endDate,
      project: p.project,
      visibility: p.visibility,
      groupBy: p.groupBy,
      write: p.write,
      lang: p.lang,
    }),

  exportSearchResults: (p: {
    query: string;
    projectFilter: string | null;
    visibility: PromptVisibility;
    write: boolean;
    lang?: string;
  }) =>
    invoke<ExportResult>("export_search_results", {
      query: p.query,
      projectFilter: p.projectFilter,
      visibility: p.visibility,
      write: p.write,
      lang: p.lang,
    }),

  exportConversation: (p: ConversationExportParams) =>
    invoke<ConversationExportResult>("export_conversation", {
      sessionId: p.sessionId,
      includeThinking: p.includeThinking,
      includeTools: p.includeTools,
      includeSkills: p.includeSkills,
      includeMeta: p.includeMeta,
      includeCodexCommentary: p.includeCodexCommentary,
      includeSubagent: p.includeSubagent,
      includeTime: p.includeTime,
      messageIndexes: p.messageIndexes,
      write: p.write,
      lang: p.lang,
    }),

  getSettings: () => invoke<SettingsView>("get_settings"),

  setSettings: (settings: SettingsInput) =>
    invoke<SettingsView>("set_settings", { settings }),

  revealPath: (path: string) => invoke<void>("reveal_path", { path }),
};

/** 把后端返回的错误统一转成可读字符串 */
export function errMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return translate("unknownError");
}
