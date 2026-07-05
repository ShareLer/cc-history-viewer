//! 暴露给前端的 Tauri Commands。

use crate::export::{self, ExportParams, Lang};
use crate::indexer::{self, AppIndex};
use crate::models::*;
use crate::parser;
use crate::state::{self, load_settings, resolve_data_paths, resolve_from_settings, AppState};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};

/// 索引磁盘缓存文件路径（v2：文件级缓存）
fn cache_file(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_data_dir()
        .ok()
        .map(|d| d.join("index_cache_v2.json"))
}

/// 删除 v1 时代的旧缓存文件（结构已不兼容，留着只占磁盘）。
fn cleanup_legacy_cache(app: &AppHandle) {
    if let Ok(dir) = app.path().app_data_dir() {
        let legacy = dir.join("index_cache.json");
        if legacy.exists() {
            let _ = std::fs::remove_file(legacy);
        }
    }
}

/// 确保索引已构建（懒加载）
fn ensure_index(state: &AppState, app: &AppHandle) -> Result<(), String> {
    {
        let guard = state.index.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Ok(());
        }
    }
    let _build_guard = state.build_lock.lock().map_err(|e| e.to_string())?;
    {
        let guard = state.index.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Ok(());
        }
    }
    let paths = resolve_data_paths(app)?;
    if !paths.history.exists() && !paths.projects.exists() {
        if !paths.codex_sessions.exists() {
            return Err(format!(
                "未找到数据源：{}、{} 与 {} 均不存在。请在设置中检查数据目录配置。",
                paths.history.display(),
                paths.projects.display(),
                paths.codex_sessions.display()
            ));
        }
    }
    cleanup_legacy_cache(app);
    let cache = cache_file(app);
    let idx = indexer::load_or_build(&paths, cache.as_deref());
    let mut guard = state.index.lock().map_err(|e| e.to_string())?;
    if guard.is_none() {
        *guard = Some(idx);
    }
    Ok(())
}

/// 在索引上执行只读闭包
fn read_index<F, R>(state: &AppState, app: &AppHandle, f: F) -> Result<R, String>
where
    F: FnOnce(&AppIndex) -> R,
{
    ensure_index(state, app)?;
    let guard = state.index.lock().map_err(|e| e.to_string())?;
    let idx = guard.as_ref().ok_or("索引尚未就绪")?;
    Ok(f(idx))
}

fn sort_prompts(v: &mut [PromptEntry], sort: Option<&str>) {
    match sort {
        Some("oldest") => v.sort_by(|a, b| a.timestamp.cmp(&b.timestamp)),
        Some("longest") => v.sort_by(|a, b| b.char_count.cmp(&a.char_count)),
        _ => v.sort_by(|a, b| b.timestamp.cmp(&a.timestamp)),
    }
}

/// 文件夹（项目）列表
#[tauri::command]
pub fn get_projects(
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Vec<ProjectInfo>, String> {
    read_index(&state, &app, |idx| idx.projects.clone())
}

/// 指定文件夹下的 prompt 列表
#[tauri::command]
pub fn get_project_prompts(
    project: String,
    sort: Option<String>,
    visibility: Option<PromptVisibility>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Vec<PromptEntry>, String> {
    let vis = visibility.unwrap_or_default();
    read_index(&state, &app, |idx| {
        let mut v: Vec<PromptEntry> = idx
            .prompts
            .iter()
            .filter(|p| p.project == project)
            .filter(|p| vis.allows(p.kind))
            .cloned()
            .collect();
        sort_prompts(&mut v, sort.as_deref());
        v
    })
}

/// 全局最近的 prompt（已按时间倒序）
#[tauri::command]
pub fn get_recent_prompts(
    limit: Option<usize>,
    visibility: Option<PromptVisibility>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Vec<PromptEntry>, String> {
    let lim = limit.unwrap_or(30);
    let vis = visibility.unwrap_or_default();
    read_index(&state, &app, |idx| {
        idx.prompts
            .iter()
            .filter(|p| vis.allows(p.kind))
            .take(lim)
            .cloned()
            .collect()
    })
}

/// 模糊搜索（全局 / 文件夹内）
#[tauri::command]
pub fn search_prompts(
    query: String,
    project_filter: Option<String>,
    visibility: Option<PromptVisibility>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Vec<SearchResult>, String> {
    let vis = visibility.unwrap_or_default();
    read_index(&state, &app, |idx| {
        indexer::search(&idx.prompts, &query, project_filter.as_deref(), &vis)
    })
}

/// 统计信息
#[tauri::command]
pub fn get_stats(state: State<'_, AppState>, app: AppHandle) -> Result<AppStats, String> {
    read_index(&state, &app, |idx| idx.stats.clone())
}

/// 指定文件夹下的会话列表
#[tauri::command]
pub fn get_project_sessions(
    project: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<Vec<SessionSummary>, String> {
    read_index(&state, &app, |idx| {
        let mut v: Vec<SessionSummary> = idx
            .sessions
            .iter()
            .filter(|s| s.project == project)
            .cloned()
            .collect();
        v.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        v
    })
}

/// 按 sessionId 找到对话文件路径与索引阶段解析出的项目路径。
fn session_context(
    state: &AppState,
    app: &AppHandle,
    session_id: &str,
) -> Result<(String, AgentKind, Option<String>), String> {
    ensure_index(state, app)?;
    let guard = state.index.lock().map_err(|e| e.to_string())?;
    let idx = guard.as_ref().ok_or("索引尚未就绪")?;
    let file = idx
        .session_files
        .get(session_id)
        .cloned()
        .ok_or_else(|| format!("找不到会话文件：{session_id}"))?;
    let session = idx
        .sessions
        .iter()
        .find(|s| s.session_id == session_id)
        .ok_or_else(|| format!("找不到会话摘要：{session_id}"))?;
    let agent = session.agent;
    let project = Some(session.project.clone()).filter(|p| !p.is_empty());
    Ok((file, agent, project))
}

fn load_conversation_detail(
    state: &AppState,
    app: &AppHandle,
    session_id: &str,
) -> Result<ConversationDetail, String> {
    let (file, agent, project) = session_context(state, app, session_id)?;
    let mut detail = match agent {
        AgentKind::Codex => parser::parse_codex_conversation_detail(Path::new(&file)),
        AgentKind::ClaudeCode => parser::parse_conversation_detail(Path::new(&file)),
    }
    .ok_or_else(|| "对话文件解析失败".to_string())?;
    if detail.project.is_empty() {
        if let Some(project) = project {
            detail.project = project;
        }
    }
    Ok(detail)
}

/// 单个会话的完整对话详情
#[tauri::command]
pub fn get_conversation(
    session_id: String,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<ConversationDetail, String> {
    load_conversation_detail(&state, &app, &session_id)
}

/// 索引元信息
#[tauri::command]
pub fn get_index_meta(state: State<'_, AppState>, app: AppHandle) -> Result<IndexMeta, String> {
    read_index(&state, &app, |idx| IndexMeta {
        built_at: idx.built_at,
        from_cache: idx.from_cache,
        source_files: idx.source_files,
        reparsed_files: idx.reparsed_files,
    })
}

/// 增量刷新：仅重解析新增 / 变化的文件（其余复用缓存），拾取最新数据。
#[tauri::command]
pub fn refresh_index(state: State<'_, AppState>, app: AppHandle) -> Result<IndexMeta, String> {
    let paths = resolve_data_paths(&app)?;
    let cache = cache_file(&app);
    let _build_guard = state.build_lock.lock().map_err(|e| e.to_string())?;
    let idx = indexer::load_or_build(&paths, cache.as_deref());
    let meta = IndexMeta {
        built_at: idx.built_at,
        from_cache: idx.from_cache,
        source_files: idx.source_files,
        reparsed_files: idx.reparsed_files,
    };
    let mut guard = state.index.lock().map_err(|e| e.to_string())?;
    *guard = Some(idx);
    Ok(meta)
}

/// 强制全量重建：忽略缓存重解析全部文件。用于罕见的缓存失准（如保留 mtime 的文件恢复）。
#[tauri::command]
pub fn rebuild_index(state: State<'_, AppState>, app: AppHandle) -> Result<IndexMeta, String> {
    let paths = resolve_data_paths(&app)?;
    let cache = cache_file(&app);
    let _build_guard = state.build_lock.lock().map_err(|e| e.to_string())?;
    let idx = indexer::build_and_cache(&paths, cache.as_deref());
    let meta = IndexMeta {
        built_at: idx.built_at,
        from_cache: false,
        source_files: idx.source_files,
        reparsed_files: idx.reparsed_files,
    };
    let mut guard = state.index.lock().map_err(|e| e.to_string())?;
    *guard = Some(idx);
    Ok(meta)
}

// ----------------------------- 设置 -----------------------------

/// 由设置内容组装 SettingsView（含解析后的路径与存在性）。
fn settings_view(s: &SettingsInput, config_path: &Path) -> Result<SettingsView, String> {
    let paths = resolve_from_settings(s)?;
    Ok(SettingsView {
        claude_data_dir: s.claude_data_dir.clone(),
        history_file: s.history_file.clone(),
        projects_dir: s.projects_dir.clone(),
        sessions_dir: s.sessions_dir.clone(),
        codex_sessions_dir: s.codex_sessions_dir.clone(),
        config_path: config_path.to_string_lossy().to_string(),
        resolved: ResolvedPaths {
            history: paths.history.to_string_lossy().to_string(),
            projects: paths.projects.to_string_lossy().to_string(),
            sessions: paths.sessions.to_string_lossy().to_string(),
            codex_sessions: paths.codex_sessions.to_string_lossy().to_string(),
            history_exists: paths.history.is_file(),
            projects_exists: paths.projects.is_dir(),
            sessions_exists: paths.sessions.is_dir(),
            codex_sessions_exists: paths.codex_sessions.is_dir(),
        },
    })
}

/// 读取当前设置（含实际生效的配置文件路径与解析结果）
#[tauri::command]
pub fn get_settings(app: AppHandle) -> Result<SettingsView, String> {
    let (s, path) = load_settings(&app);
    settings_view(&s, &path)
}

/// 保存设置并使索引失效（下次查询时按新数据源懒重建）
#[tauri::command]
pub fn set_settings(
    settings: SettingsInput,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<SettingsView, String> {
    let path = state::save_settings(&app, &settings)?;
    {
        let mut guard = state.index.lock().map_err(|e| e.to_string())?;
        *guard = None;
    }
    settings_view(&settings, &path)
}

// ----------------------------- 导出 -----------------------------

/// 按日期范围导出 prompt。
/// write=false 仅生成预览与统计；write=true 额外把完整 Markdown 写入 ~/Downloads。
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn build_prompt_export(
    start_date: String,
    end_date: String,
    project: Option<String>,
    visibility: Option<PromptVisibility>,
    group_by: Option<String>,
    write: bool,
    lang: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<ExportResult, String> {
    let start_ms = export::day_start_ms(&start_date)
        .ok_or_else(|| format!("起始日期无法解析：{start_date}"))?;
    let end_ms =
        export::day_end_ms(&end_date).ok_or_else(|| format!("结束日期无法解析：{end_date}"))?;
    if start_ms > end_ms {
        return Err("起始日期不能晚于结束日期。".to_string());
    }
    let group = group_by.unwrap_or_else(|| "project".to_string());
    let lang = Lang::from_opt(lang.as_deref());
    let vis = visibility.unwrap_or_default();

    let data = read_index(&state, &app, |idx| {
        export::build(
            &idx.prompts,
            &ExportParams {
                start_ms,
                end_ms,
                project: project.as_deref(),
                visibility: vis,
                group_by: &group,
                start_date: &start_date,
                end_date: &end_date,
                lang,
            },
        )
    })?;

    let mut path: Option<String> = None;
    if write {
        if data.prompt_count == 0 {
            return Err("该范围内没有可导出的 prompt。".to_string());
        }
        let base = format!("CC-Prompts_{start_date}_{end_date}");
        let target = write_unique_export(&base, &data.markdown)?;
        path = Some(target.to_string_lossy().to_string());
    }

    Ok(ExportResult {
        preview: data.preview(),
        path,
        prompt_count: data.prompt_count,
        folder_count: data.folder_count,
        day_count: data.day_count,
    })
}

/// 把当前搜索命中的全部 prompt 导出为 Markdown（按文件夹分组）。
/// write=false 仅生成预览与统计；write=true 额外写入 ~/Downloads。
#[tauri::command]
pub fn export_search_results(
    query: String,
    project_filter: Option<String>,
    visibility: Option<PromptVisibility>,
    write: bool,
    lang: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<ExportResult, String> {
    let lang = Lang::from_opt(lang.as_deref());
    let vis = visibility.unwrap_or_default();
    let data = read_index(&state, &app, |idx| {
        let results = indexer::search(&idx.prompts, &query, project_filter.as_deref(), &vis);
        let items: Vec<&PromptEntry> = results.iter().map(|r| &r.entry).collect();
        export::build_search_export(&items, &query, project_filter.as_deref(), lang)
    })?;

    let mut path: Option<String> = None;
    if write {
        if data.prompt_count == 0 {
            return Err("没有可导出的搜索结果。".to_string());
        }
        let date = chrono::Local::now().format("%Y-%m-%d");
        let base = format!("CC-Search_{}_{date}", sanitize_for_filename(&query));
        let target = write_unique_export(&base, &data.markdown)?;
        path = Some(target.to_string_lossy().to_string());
    }

    Ok(ExportResult {
        preview: data.preview(),
        path,
        prompt_count: data.prompt_count,
        folder_count: data.folder_count,
        day_count: data.day_count,
    })
}

/// 把搜索词压成安全的文件名片段：保留字母数字与 CJK，其余替换为 '-'，最长 24 字符。
fn sanitize_for_filename(q: &str) -> String {
    let cleaned: String = q
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed: String = cleaned.trim_matches('-').chars().take(24).collect();
    if trimmed.is_empty() {
        "query".to_string()
    } else {
        trimmed
    }
}

/// 导出单个会话的完整对话为 Markdown。
/// write=false 仅生成预览；write=true 额外写入 ~/Downloads。
#[tauri::command]
pub fn export_conversation(
    session_id: String,
    include_thinking: bool,
    include_tools: bool,
    include_skills: bool,
    include_meta: bool,
    include_codex_commentary: bool,
    include_subagent: bool,
    include_time: bool,
    message_indexes: Vec<usize>,
    write: bool,
    lang: Option<String>,
    state: State<'_, AppState>,
    app: AppHandle,
) -> Result<ConversationExportResult, String> {
    let detail = load_conversation_detail(&state, &app, &session_id)?;
    let lang = Lang::from_opt(lang.as_deref());
    let selected = (!message_indexes.is_empty()).then(|| {
        message_indexes
            .into_iter()
            .collect::<std::collections::HashSet<_>>()
    });
    let options = export::ConversationExportOptions {
        include_thinking,
        include_tools,
        include_skills,
        include_meta,
        include_codex_commentary,
        include_subagent,
        include_time,
    };
    let (markdown, exported_count) =
        export::build_conversation_markdown(&detail, selected.as_ref(), &options, lang);

    let mut path: Option<String> = None;
    if write {
        let short_id: String = session_id.chars().take(8).collect();
        let date = chrono::Local::now().format("%Y-%m-%d");
        let base = format!("CC-Conversation_{short_id}_{date}");
        let target = write_unique_export(&base, &markdown)?;
        path = Some(target.to_string_lossy().to_string());
    }

    Ok(ConversationExportResult {
        preview: export::truncate_preview(&markdown, lang),
        path,
        message_count: exported_count,
    })
}

/// 在系统文件管理器中定位某个文件（macOS：Finder 选中）。
#[tauri::command]
pub fn reveal_path(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err("文件不存在或已被移动。".to_string());
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&p)
            .spawn()
            .map_err(|e| format!("无法打开 Finder：{e}"))?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(&p)
            .spawn()
            .map_err(|e| format!("无法打开资源管理器：{e}"))?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = p.parent().unwrap_or(&p);
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map_err(|e| format!("无法打开文件管理器：{e}"))?;
    }
    Ok(())
}

fn export_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn export_candidate(base: &str, n: usize) -> PathBuf {
    let dir = export_dir();
    if n == 1 {
        dir.join(format!("{base}.md"))
    } else {
        dir.join(format!("{base} ({n}).md"))
    }
}

/// 原子创建不冲突的导出文件：base.md → base (2).md → …
fn write_unique_export(base: &str, markdown: &str) -> Result<PathBuf, String> {
    let dir = export_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建导出目录失败：{e}"))?;
    let mut n = 1;
    loop {
        let candidate = export_candidate(base, n);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(markdown.as_bytes())
                    .map_err(|e| format!("写入文件失败：{e}"))?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                n += 1;
            }
            Err(e) => return Err(format!("写入文件失败：{e}")),
        }
    }
}
