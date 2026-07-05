//! 索引构建：扫描数据源、合并去重 prompt、聚合项目、计算统计、磁盘缓存。
//! 缓存按「文件粒度」存解析结果（CacheV2）：单个 jsonl 变化只重解析该文件。

use crate::models::*;
use crate::parser::{self, ConvFileResult, RawPrompt};
use crate::pricing;
use crate::state::DataPaths;
use crate::util::project_name;
use chrono::{Datelike, Local, TimeZone, Timelike};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 同一文本在此时间窗内（毫秒）视为同一条 prompt，用于跨数据源去重
const DEDUP_WINDOW_MS: i64 = 5 * 60 * 1000;

/// 缓存格式版本；结构或解析规则变化时递增，旧缓存自动失效
/// （v12：Codex 子代理会话不再作为独立会话索引；ConvFileResult 增加 is_subagent 字段）
/// （v13：Codex skill 注入不再作为 prompt；compaction 摘要不再计入消息数）
/// （v14：Claude Code 会话标题跳过控制命令，优先使用命令参数里的真实请求）
/// （v15：Claude Code 去除技能/上下文预派发造成的 replay 重复用户 prompt）
/// （v16：嵌入在普通文本/代码示例中的 command wrapper 不再被误改写为 /model）
/// （v17：Claude Code compact summary 不再作为真实用户 prompt / 标题 / 统计）
const CACHE_VERSION: u32 = 17;

/// 构建好的全量索引（仅驻留内存；磁盘缓存见 CacheV2）
pub struct AppIndex {
    pub prompts: Vec<PromptEntry>,
    pub projects: Vec<ProjectInfo>,
    pub sessions: Vec<SessionSummary>,
    pub stats: AppStats,
    /// sessionId -> 对话文件绝对路径
    pub session_files: HashMap<String, String>,
    /// 数据源文件数（history + 对话文件）
    pub source_files: usize,
    pub built_at: i64,
    /// 本次构建是否完全复用缓存（history 与全部对话文件均未重解析）
    pub from_cache: bool,
    /// 本次重解析的对话文件数（全缓存命中 = 0）
    pub reparsed_files: usize,
}

// ----------------------------- 磁盘缓存（v2） -----------------------------

/// 文件级磁盘缓存：history 解析结果 + 每个对话文件的解析结果
#[derive(Serialize, Deserialize)]
struct CacheV2 {
    version: u32,
    /// history.jsonl 的绝对路径；切换数据源时即使 mtime 巧合相同也必须重解析
    history_path: String,
    /// history.jsonl 的 freshness；切换数据源或文件变化时必须重解析 history
    history_stamp: FileStamp,
    history_prompts: Vec<RawPrompt>,
    /// 对话文件绝对路径 -> 文件级缓存
    files: HashMap<String, FileCache>,
}

#[derive(Serialize, Deserialize)]
struct FileCache {
    /// 文件 freshness；不一致则重解析该文件
    stamp: FileStamp,
    /// 原始解析结果（未经 resolve_missing_projects 回填，保证缓存与全量重建一致）
    conv: ConvFileResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FileStamp {
    /// 文件 mtime(ms)。0 表示 metadata 不可用，不能复用缓存。
    mtime: i64,
    len: u64,
}

impl CacheV2 {
    /// 空缓存：history_stamp 取无效值，保证首次构建必然重解析
    fn empty() -> Self {
        Self {
            version: CACHE_VERSION,
            history_path: String::new(),
            history_stamp: FileStamp { mtime: 0, len: 0 },
            history_prompts: Vec::new(),
            files: HashMap::new(),
        }
    }
}

/// 读缓存：文件缺失 / 解析失败 / 版本不符 都视为无缓存
fn read_cache_v2(path: &Path) -> Option<CacheV2> {
    let text = fs::read_to_string(path).ok()?;
    let version = cache_version_from_json(&text)?;
    if version != u64::from(CACHE_VERSION) {
        return None;
    }
    let c: CacheV2 = serde_json::from_str(&text).ok()?;
    Some(c)
}

fn cache_version_from_json(text: &str) -> Option<u64> {
    #[derive(Deserialize)]
    struct CacheVersion {
        version: u64,
    }

    serde_json::from_str::<CacheVersion>(text)
        .ok()
        .map(|v| v.version)
}

fn write_cache_v2(cache_path: &Path, cache: &CacheV2) {
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(cache) {
        let tmp = cache_path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, cache_path);
        }
    }
}

fn history_cache_changed(cache: &CacheV2, history_path: &str, history_stamp: FileStamp) -> bool {
    cache.history_path != history_path || !stamp_matches(cache.history_stamp, history_stamp)
}

fn stamp_matches(cached: FileStamp, current: FileStamp) -> bool {
    current.mtime > 0 && cached == current
}

// ----------------------------- 公共入口 -----------------------------

/// 增量构建：逐文件对比 mtime，仅重解析新增 / 变化的文件；其余复用缓存。
pub fn load_or_build(paths: &DataPaths, cache_path: Option<&Path>) -> AppIndex {
    let conv_files = collect_all_conversation_files(paths);
    let mut old = cache_path
        .and_then(read_cache_v2)
        .unwrap_or_else(CacheV2::empty);

    // history.jsonl：mtime 一致则复用缓存的解析结果
    let history_path = paths.history.to_string_lossy().to_string();
    let history_stamp = file_stamp(&paths.history);
    let history_changed = history_cache_changed(&old, &history_path, history_stamp);
    let history_prompts = if history_changed {
        parser::parse_history(&paths.history)
    } else {
        std::mem::take(&mut old.history_prompts)
    };

    // 对话文件：mtime 一致 → 复用；新增 / 变化 → 待重解析
    let mut files: HashMap<String, FileCache> = HashMap::with_capacity(conv_files.len());
    let mut to_parse: Vec<(String, FileStamp, PathBuf, AgentKind)> = Vec::new();
    for (f, agent) in conv_files {
        let key = f.to_string_lossy().to_string();
        let stamp = file_stamp(&f);
        match old.files.remove(&key) {
            Some(fc) if stamp_matches(fc.stamp, stamp) => {
                files.insert(key, fc);
            }
            _ => to_parse.push((key, stamp, f, agent)),
        }
    }
    // old.files 中剩余条目对应磁盘已删除的文件，直接丢弃
    let removed_any = !old.files.is_empty();

    let reparsed_files = to_parse.len();
    for (key, stamp, conv) in parse_files_par(to_parse) {
        files.insert(key, FileCache { stamp, conv });
    }

    // 完全无重解析（history 也没变）才算「来自缓存」
    let from_cache = !history_changed && reparsed_files == 0;

    let cache = CacheV2 {
        version: CACHE_VERSION,
        history_path,
        history_stamp,
        history_prompts,
        files,
    };
    // 内容有变化才写回，避免每次启动都重写大文件
    if history_changed || reparsed_files > 0 || removed_any {
        if let Some(cp) = cache_path {
            write_cache_v2(cp, &cache);
        }
    }

    assemble_index(paths, cache, from_cache, reparsed_files)
}

/// 强制全量重建：忽略缓存重解析全部文件，并写回最新 CacheV2。
pub fn build_and_cache(paths: &DataPaths, cache_path: Option<&Path>) -> AppIndex {
    let conv_files = collect_all_conversation_files(paths);
    let history_path = paths.history.to_string_lossy().to_string();
    let history_stamp = file_stamp(&paths.history);
    let history_prompts = parser::parse_history(&paths.history);

    let to_parse: Vec<(String, FileStamp, PathBuf, AgentKind)> = conv_files
        .into_iter()
        .map(|(f, agent)| (f.to_string_lossy().to_string(), file_stamp(&f), f, agent))
        .collect();
    let reparsed_files = to_parse.len();

    let mut files: HashMap<String, FileCache> = HashMap::with_capacity(reparsed_files);
    for (key, stamp, conv) in parse_files_par(to_parse) {
        files.insert(key, FileCache { stamp, conv });
    }

    let cache = CacheV2 {
        version: CACHE_VERSION,
        history_path,
        history_stamp,
        history_prompts,
        files,
    };
    if let Some(cp) = cache_path {
        write_cache_v2(cp, &cache);
    }

    assemble_index(paths, cache, false, reparsed_files)
}

/// 并行解析一批对话文件；解析失败（不可读等）的文件直接丢弃，下次仍会重试。
fn parse_files_par(
    items: Vec<(String, FileStamp, PathBuf, AgentKind)>,
) -> Vec<(String, FileStamp, ConvFileResult)> {
    items
        .into_par_iter()
        .filter_map(|(key, stamp, path, agent)| {
            let parsed = match agent {
                AgentKind::ClaudeCode => parser::parse_conversation_file(&path),
                AgentKind::Codex => parser::parse_codex_session_file(&path),
            };
            match parsed {
                Some(conv) => Some((key, stamp, conv)),
                None => {
                    eprintln!("failed to parse conversation file: {}", path.display());
                    None
                }
            }
        })
        .collect()
}

// ----------------------------- 构建主流程 -----------------------------

/// 用「复用 + 新解析」的全量解析结果跑后续构建步骤。
/// 注意：缓存写回发生在本函数之前，缓存中保存的是未回填 project 的原始解析结果。
fn assemble_index(
    paths: &DataPaths,
    cache: CacheV2,
    from_cache: bool,
    reparsed_files: usize,
) -> AppIndex {
    let source_files = cache.files.len() + usize::from(paths.history.exists());
    let history_prompts = cache.history_prompts;
    let mut conv_results: Vec<ConvFileResult> =
        cache.files.into_values().map(|fc| fc.conv).collect();
    conv_results.sort_by(|a, b| {
        a.path
            .to_string_lossy()
            .cmp(&b.path.to_string_lossy())
            .then_with(|| a.session_id.cmp(&b.session_id))
    });

    // Codex 子代理文件（session_meta.source.subagent）不作为独立会话：整体排除，
    // 使其不进入会话列表 / prompt / 项目聚合 / 用量统计，与 Claude 的 subagents/ 路径过滤对齐。
    // 其在父会话里的表现（spawn_agent / wait_agent / close_agent 工具调用）不受影响。
    conv_results.retain(|cr| !cr.is_subagent);

    // 1. cwd 缺失的会话用「真实路径字典」兜底解码目录名
    resolve_missing_projects(&history_prompts, &mut conv_results);

    // 2. 合并 + 去重 prompt
    let prompts = merge_prompts(history_prompts, &conv_results);

    // 3. sessionId -> 文件路径
    let mut session_files = HashMap::new();
    for cr in &conv_results {
        session_files.insert(cr.session_id.clone(), cr.path.to_string_lossy().to_string());
    }

    // 4. 项目聚合
    let projects = aggregate_projects(&prompts, &conv_results);

    // 5. 会话摘要
    let sessions = build_sessions(&conv_results);

    // 6. 统计
    let stats = compute_stats(&prompts, &conv_results, &paths.sessions, projects.len());

    AppIndex {
        prompts,
        projects,
        sessions,
        stats,
        session_files,
        source_files,
        built_at: now_ms(),
        from_cache,
        reparsed_files,
    }
}

/// 用 history 与对话文件里的真实路径，反查 cwd 缺失会话的项目路径。
fn resolve_missing_projects(history: &[RawPrompt], conv: &mut [ConvFileResult]) {
    let mut real_paths: HashSet<String> = HashSet::new();
    for rp in history {
        real_paths.insert(rp.project.clone());
    }
    for cr in conv.iter() {
        if let Some(p) = &cr.project {
            real_paths.insert(p.clone());
        }
    }
    // 编码目录名 -> 真实路径
    let decode_dict: HashMap<String, String> = real_paths
        .iter()
        .map(|p| (p.replace('/', "-"), p.clone()))
        .collect();

    for cr in conv.iter_mut() {
        if cr.project.is_some() {
            continue;
        }
        let dir_name = cr
            .path
            .parent()
            .and_then(|d| d.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let resolved = decode_dict
            .get(&dir_name)
            .cloned()
            .unwrap_or_else(|| dir_name.clone());
        for up in cr.user_prompts.iter_mut() {
            if up.project.is_empty() {
                up.project = resolved.clone();
            }
        }
        cr.project = Some(resolved);
    }
}

// ----------------------------- prompt 合并去重 -----------------------------

fn merge_prompts(history: Vec<RawPrompt>, conv: &[ConvFileResult]) -> Vec<PromptEntry> {
    let mut all: Vec<RawPrompt> = history;
    let mut seen_conversation_uuids: HashSet<(AgentKind, String)> = HashSet::new();
    for cr in conv {
        for prompt in &cr.user_prompts {
            if let Some(uuid) = prompt.message_uuid.as_deref().filter(|s| !s.is_empty()) {
                if !seen_conversation_uuids.insert((prompt.agent, uuid.to_string())) {
                    continue;
                }
            }
            all.push(prompt.clone());
        }
    }

    // 按 (工具, 项目, 文本, kind) 分组，避免跨工具或不同类型记录互相吞并
    let mut groups: HashMap<(AgentKind, String, String, PromptKind), Vec<RawPrompt>> =
        HashMap::new();
    for rp in all {
        if rp.text.is_empty() || rp.project.is_empty() {
            continue;
        }
        groups
            .entry((rp.agent, rp.project.clone(), rp.text.clone(), rp.kind))
            .or_default()
            .push(rp);
    }

    let mut entries: Vec<PromptEntry> = Vec::new();
    for ((agent, project, text, kind), mut items) in groups {
        items.sort_by_key(|r| r.timestamp);
        let mut used = vec![false; items.len()];
        let pairs = nearest_cross_source_pairs(&items);
        for i in 0..items.len() {
            if used[i] {
                continue;
            }
            used[i] = true;
            let mut members = vec![items[i].clone()];

            // 只把 history 与 conversation 的同一条输入配成 source=both。
            // 同来源的短时间重复输入是用户真实行为，必须保留为多条。
            if let Some(j) = pairs[i] {
                used[j] = true;
                members.push(items[j].clone());
            }

            entries.push(make_prompt_entry(agent, &project, &text, kind, &members));
        }
    }

    entries.sort_by(|a, b| {
        b.timestamp
            .cmp(&a.timestamp)
            .then_with(|| prompt_agent_rank(a.agent).cmp(&prompt_agent_rank(b.agent)))
            .then_with(|| a.project.cmp(&b.project))
            .then_with(|| prompt_kind_rank(a.kind).cmp(&prompt_kind_rank(b.kind)))
            .then_with(|| a.id.cmp(&b.id))
    });
    entries
}

fn prompt_agent_rank(agent: AgentKind) -> u8 {
    match agent {
        AgentKind::ClaudeCode => 0,
        AgentKind::Codex => 1,
    }
}

fn prompt_kind_rank(kind: PromptKind) -> u8 {
    match kind {
        PromptKind::Human => 0,
        PromptKind::Command => 1,
        PromptKind::Meta => 2,
        PromptKind::Sidechain => 3,
        PromptKind::System => 4,
        PromptKind::Queued => 5,
        PromptKind::Sdk => 6,
        PromptKind::Other => 7,
    }
}

fn nearest_cross_source_pairs(items: &[RawPrompt]) -> Vec<Option<usize>> {
    let mut candidates: Vec<(u64, i64, usize, usize)> = Vec::new();
    for i in 0..items.len() {
        for j in (i + 1)..items.len() {
            if items[i].from_history == items[j].from_history {
                continue;
            }
            let diff = items[i].timestamp.abs_diff(items[j].timestamp);
            if diff <= DEDUP_WINDOW_MS as u64 {
                candidates.push((diff, items[i].timestamp.min(items[j].timestamp), i, j));
            }
        }
    }
    candidates.sort_unstable();

    let mut pairs = vec![None; items.len()];
    for (_, _, i, j) in candidates {
        if pairs[i].is_none() && pairs[j].is_none() {
            pairs[i] = Some(j);
            pairs[j] = Some(i);
        }
    }
    pairs
}

fn make_prompt_entry(
    agent: AgentKind,
    project: &str,
    text: &str,
    kind: PromptKind,
    members: &[RawPrompt],
) -> PromptEntry {
    let timestamp = members.iter().map(|m| m.timestamp).min().unwrap_or(0);
    let has_history = members.iter().any(|m| m.from_history);
    let has_conv = members.iter().any(|m| !m.from_history);
    let source = match (has_history, has_conv) {
        (true, true) => "both",
        (true, false) => "history",
        _ => "conversation",
    };
    let session_id = members.iter().find_map(|m| m.session_id.clone());
    let message_uuid = members.iter().find_map(|m| m.message_uuid.clone());
    let git_branch = members.iter().find_map(|m| m.git_branch.clone());
    let pasted_count = members.iter().map(|m| m.pasted_count).max().unwrap_or(0);
    let origin_key = members
        .iter()
        .map(|m| m.origin_key.as_str())
        .collect::<Vec<_>>()
        .join("|");

    PromptEntry {
        id: make_id(agent, project, timestamp, text, kind, &origin_key),
        text: text.to_string(),
        project: project.to_string(),
        timestamp,
        source: source.to_string(),
        agent,
        kind,
        message_uuid,
        session_id,
        git_branch,
        is_command: kind == PromptKind::Command,
        pasted_count,
        char_count: text.chars().count(),
    }
}

fn make_id(
    agent: AgentKind,
    project: &str,
    ts: i64,
    text: &str,
    kind: PromptKind,
    origin_key: &str,
) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    agent.hash(&mut h);
    project.hash(&mut h);
    ts.hash(&mut h);
    text.hash(&mut h);
    kind.hash(&mut h);
    origin_key.hash(&mut h);
    format!("{:016x}", h.finish())
}

// ----------------------------- 项目 / 会话聚合 -----------------------------

fn aggregate_projects(prompts: &[PromptEntry], conv: &[ConvFileResult]) -> Vec<ProjectInfo> {
    let mut map: HashMap<String, ProjectInfo> = HashMap::new();
    for p in prompts {
        if p.project.is_empty() {
            continue;
        }
        let info = map.entry(p.project.clone()).or_insert_with(|| ProjectInfo {
            path: p.project.clone(),
            name: project_name(&p.project),
            prompt_count: 0,
            command_count: 0,
            session_count: 0,
            first_active: p.timestamp,
            last_active: p.timestamp,
            has_conversations: false,
        });
        info.prompt_count += 1;
        if p.kind == PromptKind::Command {
            info.command_count += 1;
        }
        if p.timestamp < info.first_active {
            info.first_active = p.timestamp;
        }
        if p.timestamp > info.last_active {
            info.last_active = p.timestamp;
        }
    }

    // 会话数量与「有对话」标记
    let mut sess_count: HashMap<String, usize> = HashMap::new();
    for cr in conv {
        if let Some(proj) = &cr.project {
            if !proj.is_empty() {
                *sess_count.entry(proj.clone()).or_insert(0) += 1;
            }
        }
    }
    for (proj, cnt) in sess_count {
        match map.get_mut(&proj) {
            Some(info) => {
                info.session_count = cnt;
                info.has_conversations = true;
            }
            None => {
                map.insert(
                    proj.clone(),
                    ProjectInfo {
                        path: proj.clone(),
                        name: project_name(&proj),
                        prompt_count: 0,
                        command_count: 0,
                        session_count: cnt,
                        first_active: 0,
                        last_active: 0,
                        has_conversations: true,
                    },
                );
            }
        }
    }

    let mut list: Vec<ProjectInfo> = map.into_values().collect();
    list.sort_by(|a, b| {
        b.last_active
            .cmp(&a.last_active)
            .then_with(|| a.path.cmp(&b.path))
    });
    list
}

fn build_sessions(conv: &[ConvFileResult]) -> Vec<SessionSummary> {
    let mut out: Vec<SessionSummary> = conv
        .iter()
        .map(|cr| SessionSummary {
            session_id: cr.session_id.clone(),
            project: cr.project.clone().unwrap_or_default(),
            agent: cr.agent,
            // 直接用首条 user prompt（可为空串），空标题的兜底展示由前端负责
            title: cr.first_prompt.clone(),
            started_at: cr.started_at,
            ended_at: cr.ended_at,
            message_count: cr.message_count,
            git_branch: cr.git_branch.clone(),
        })
        .collect();
    out.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    out
}

// ----------------------------- 统计 -----------------------------

fn compute_stats(
    prompts: &[PromptEntry],
    conv: &[ConvFileResult],
    sessions_dir: &Path,
    total_projects: usize,
) -> AppStats {
    let mut history_prompts = 0;
    let mut conversation_prompts = 0;
    let mut command_count = 0;
    let mut first_use = i64::MAX;
    let mut last_use = i64::MIN;
    let mut by_day: HashMap<String, usize> = HashMap::new();
    let mut by_hour = [0usize; 24];
    let mut by_weekday = [0usize; 7];
    let mut by_project: HashMap<String, usize> = HashMap::new();

    for p in prompts {
        match p.source.as_str() {
            "history" => history_prompts += 1,
            "conversation" => conversation_prompts += 1,
            "both" => {
                history_prompts += 1;
                conversation_prompts += 1;
            }
            _ => {}
        }
        if p.kind == PromptKind::Command {
            command_count += 1;
        }
        if p.timestamp < first_use {
            first_use = p.timestamp;
        }
        if p.timestamp > last_use {
            last_use = p.timestamp;
        }
        if let Some(dt) = Local.timestamp_millis_opt(p.timestamp).single() {
            *by_day.entry(dt.format("%Y-%m-%d").to_string()).or_insert(0) += 1;
            by_hour[dt.hour() as usize] += 1;
            by_weekday[dt.weekday().num_days_from_monday() as usize] += 1;
        }
        if !p.project.is_empty() {
            *by_project.entry(p.project.clone()).or_insert(0) += 1;
        }
    }
    if first_use == i64::MAX {
        first_use = 0;
    }
    if last_use == i64::MIN {
        last_use = 0;
    }

    let mut by_day_vec: Vec<DayCount> = by_day
        .into_iter()
        .map(|(day, count)| DayCount { day, count })
        .collect();
    by_day_vec.sort_by(|a, b| a.day.cmp(&b.day));

    let by_hour_vec: Vec<HourCount> = (0..24)
        .map(|h| HourCount {
            hour: h as u8,
            count: by_hour[h],
        })
        .collect();
    let by_weekday_vec: Vec<WeekdayCount> = (0..7)
        .map(|w| WeekdayCount {
            weekday: w as u8,
            count: by_weekday[w],
        })
        .collect();

    let mut top_projects: Vec<ProjectCount> = by_project
        .into_iter()
        .map(|(path, count)| ProjectCount {
            name: project_name(&path),
            path,
            count,
        })
        .collect();
    top_projects.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.path.cmp(&b.path)));
    top_projects.truncate(8);

    // CC 版本：对话文件 + sessions 元数据
    let mut versions: HashSet<String> = HashSet::new();
    for cr in conv {
        if let Some(v) = &cr.version {
            if !v.is_empty() {
                versions.insert(v.clone());
            }
        }
    }
    collect_session_versions(sessions_dir, &mut versions);
    let mut cc_versions: Vec<String> = versions.into_iter().collect();
    cc_versions.sort_by(|a, b| version_cmp(b, a)); // 新 -> 旧

    let total_messages: usize = conv.iter().map(|c| c.message_count).sum();

    AppStats {
        total_prompts: prompts.len(),
        total_projects,
        total_sessions: conv.len(),
        total_messages,
        history_prompts,
        conversation_prompts,
        command_count,
        first_use,
        last_use,
        by_day: by_day_vec,
        by_hour: by_hour_vec,
        by_weekday: by_weekday_vec,
        top_projects,
        cc_versions,
        usage: compute_usage(conv),
    }
}

/// Token 用量统计：所有文件的 UsageEntry 按 dedup_key 全局去重后聚合。
/// resume / fork 会把旧 assistant 行复制进新文件，不去重会大幅重复计数。
fn compute_usage(conv: &[ConvFileResult]) -> UsageStats {
    #[derive(Default)]
    struct Agg {
        input: u64,
        output: u64,
        cache_read: u64,
        cache_creation: u64,
        messages: usize,
        cost: f64,
    }

    let mut seen: HashSet<&str> = HashSet::new();
    let mut total_input = 0u64;
    let mut total_output = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_cache_creation = 0u64;
    let mut est_cost_usd = 0f64;
    let mut unknown_model_tokens = 0u64;
    let mut assistant_messages = 0usize;
    let mut by_model: HashMap<String, Agg> = HashMap::new();
    let mut by_day: HashMap<String, Agg> = HashMap::new();
    let mut by_project: HashMap<String, Agg> = HashMap::new();

    for cr in conv {
        let project = cr.project.as_deref().unwrap_or("");
        for e in &cr.usage_entries {
            if !seen.insert(e.dedup_key.as_str()) {
                continue;
            }
            let cost =
                pricing::estimate_cost(&e.model, e.input, e.output, e.cache_read, e.cache_creation);
            assistant_messages += 1;
            total_input += e.input;
            total_output += e.output;
            total_cache_read += e.cache_read;
            total_cache_creation += e.cache_creation;
            match cost {
                Some(c) => est_cost_usd += c,
                None => {
                    unknown_model_tokens += e.input + e.output + e.cache_read + e.cache_creation
                }
            }
            let add = |agg: &mut Agg| {
                agg.input += e.input;
                agg.output += e.output;
                agg.cache_read += e.cache_read;
                agg.cache_creation += e.cache_creation;
                agg.messages += 1;
                agg.cost += cost.unwrap_or(0.0);
            };
            add(by_model.entry(e.model.clone()).or_default());
            if let Some(dt) = Local.timestamp_millis_opt(e.timestamp).single() {
                add(by_day.entry(dt.format("%Y-%m-%d").to_string()).or_default());
            }
            if !project.is_empty() {
                add(by_project.entry(project.to_string()).or_default());
            }
        }
    }

    let mut by_model_vec: Vec<ModelUsage> = by_model
        .into_iter()
        .map(|(model, a)| {
            // 成本按聚合量重新估算（与逐条累加等价：公式是线性的）；未知模型为 None
            let est =
                pricing::estimate_cost(&model, a.input, a.output, a.cache_read, a.cache_creation);
            ModelUsage {
                model,
                input: a.input,
                output: a.output,
                cache_read: a.cache_read,
                cache_creation: a.cache_creation,
                messages: a.messages,
                est_cost_usd: est,
            }
        })
        .collect();
    // 已知成本按成本降序；未知成本(None)排最后，按 token 总量降序
    by_model_vec.sort_by(|a, b| match (a.est_cost_usd, b.est_cost_usd) {
        (Some(x), Some(y)) => y
            .partial_cmp(&x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.model.cmp(&b.model)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => {
            let ta = a.input + a.output + a.cache_read + a.cache_creation;
            let tb = b.input + b.output + b.cache_read + b.cache_creation;
            tb.cmp(&ta).then_with(|| a.model.cmp(&b.model))
        }
    });

    let mut by_day_vec: Vec<DayUsage> = by_day
        .into_iter()
        .map(|(day, a)| DayUsage {
            day,
            input: a.input,
            output: a.output,
            cache_read: a.cache_read,
            cache_creation: a.cache_creation,
            est_cost_usd: a.cost,
        })
        .collect();
    by_day_vec.sort_by(|a, b| a.day.cmp(&b.day));

    let mut by_project_vec: Vec<ProjectUsage> = by_project
        .into_iter()
        .map(|(path, a)| ProjectUsage {
            name: project_name(&path),
            path,
            input: a.input,
            output: a.output,
            cache_read: a.cache_read,
            cache_creation: a.cache_creation,
            est_cost_usd: a.cost,
        })
        .collect();
    by_project_vec.sort_by(|a, b| {
        b.est_cost_usd
            .partial_cmp(&a.est_cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let ta = a.input + a.output + a.cache_read + a.cache_creation;
                let tb = b.input + b.output + b.cache_read + b.cache_creation;
                tb.cmp(&ta)
            })
            .then_with(|| a.path.cmp(&b.path))
    });
    by_project_vec.truncate(8);

    UsageStats {
        total_input,
        total_output,
        total_cache_read,
        total_cache_creation,
        est_cost_usd,
        unknown_model_tokens,
        assistant_messages,
        by_model: by_model_vec,
        by_day: by_day_vec,
        by_project: by_project_vec,
    }
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    fn parts(v: &str) -> Vec<u32> {
        v.split('.')
            .map(|s| {
                let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse().unwrap_or(0)
            })
            .collect()
    }
    let pa = parts(a);
    let pb = parts(b);
    pa.cmp(&pb)
}

fn collect_session_versions(sessions_dir: &Path, versions: &mut HashSet<String>) {
    if !sessions_dir.is_dir() {
        return;
    }
    if let Ok(entries) = fs::read_dir(sessions_dir) {
        for e in entries.filter_map(|e| e.ok()) {
            let p = e.path();
            if p.extension().map(|x| x == "json").unwrap_or(false) {
                if let Ok(txt) = fs::read_to_string(&p) {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                        if let Some(ver) = v.get("version").and_then(|x| x.as_str()) {
                            if !ver.is_empty() {
                                versions.insert(ver.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
}

// ----------------------------- 搜索 -----------------------------

/// 大小写折叠：to_lowercase() 可能展开成多个码点（如 'İ' → "i̇"），
/// 这里只取第一个码点做 1:1 映射，保证折叠后的 char 序列长度与原文一致，
/// 命中区间（char 索引）才能直接套用到原始文本上做高亮。
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// 子串 + 大小写不敏感 + 空格分词（多关键词 AND）的模糊搜索。
pub fn search(
    prompts: &[PromptEntry],
    query: &str,
    project_filter: Option<&str>,
    visibility: &PromptVisibility,
) -> Vec<SearchResult> {
    let tokens: Vec<Vec<char>> = query
        .split_whitespace()
        .map(|t| t.chars().map(fold_char).collect::<Vec<char>>())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut results = Vec::new();
    for p in prompts {
        if let Some(pf) = project_filter {
            if p.project != pf {
                continue;
            }
        }
        if !visibility.allows(p.kind) {
            continue;
        }
        let lower: Vec<char> = p.text.chars().map(fold_char).collect();
        let mut ranges: Vec<[usize; 2]> = Vec::new();
        let mut matched_all = true;
        for tok in &tokens {
            let occ = find_all(&lower, tok);
            if occ.is_empty() {
                matched_all = false;
                break;
            }
            for s in occ {
                ranges.push([s, s + tok.len()]);
            }
        }
        if !matched_all {
            continue;
        }
        results.push(SearchResult {
            entry: p.clone(),
            match_ranges: merge_ranges(ranges),
        });
    }
    results
}

fn find_all(haystack: &[char], needle: &[char]) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || needle.len() > haystack.len() {
        return out;
    }
    let max = haystack.len() - needle.len();
    let mut i = 0;
    while i <= max {
        if haystack[i..].starts_with(needle) {
            out.push(i);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

fn merge_ranges(mut ranges: Vec<[usize; 2]>) -> Vec<[usize; 2]> {
    if ranges.is_empty() {
        return ranges;
    }
    ranges.sort();
    let mut merged: Vec<[usize; 2]> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut() {
            if r[0] <= last[1] {
                if r[1] > last[1] {
                    last[1] = r[1];
                }
                continue;
            }
        }
        merged.push(r);
    }
    merged
}

// ----------------------------- 文件 / 缓存工具 -----------------------------

fn collect_jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return files;
    }
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let p = entry.path();
        if p.is_file() && p.extension().map(|e| e == "jsonl").unwrap_or(false) {
            files.push(p.to_path_buf());
        }
    }
    files
}

fn is_claude_subagent_file(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("subagents"))
}

fn collect_all_conversation_files(paths: &DataPaths) -> Vec<(PathBuf, AgentKind)> {
    let mut files = Vec::new();
    files.extend(
        collect_jsonl_files(&paths.projects)
            .into_iter()
            .filter(|p| !is_claude_subagent_file(p))
            .map(|p| (p, AgentKind::ClaudeCode)),
    );
    files.extend(
        collect_jsonl_files(&paths.codex_sessions)
            .into_iter()
            .map(|p| (p, AgentKind::Codex)),
    );
    files
}

fn file_stamp(p: &Path) -> FileStamp {
    let Some(meta) = fs::metadata(p).ok() else {
        return FileStamp { mtime: 0, len: 0 };
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    FileStamp {
        mtime,
        len: meta.len(),
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::UsageEntry;

    fn rp(
        text: &str,
        project: &str,
        ts: i64,
        kind: PromptKind,
        from_history: bool,
        session: Option<&str>,
    ) -> RawPrompt {
        RawPrompt {
            text: text.to_string(),
            agent: AgentKind::ClaudeCode,
            project: project.to_string(),
            timestamp: ts,
            kind,
            origin_key: format!(
                "{}:{}:{}",
                if from_history {
                    "history"
                } else {
                    "conversation"
                },
                session.unwrap_or("none"),
                ts
            ),
            message_uuid: (!from_history).then(|| format!("msg-{ts}")),
            session_id: session.map(str::to_string),
            git_branch: None,
            pasted_count: 0,
            from_history,
        }
    }

    fn cf(
        session: &str,
        project: Option<&str>,
        prompts: Vec<RawPrompt>,
        usage: Vec<UsageEntry>,
    ) -> ConvFileResult {
        ConvFileResult {
            path: PathBuf::from(format!("/tmp/{session}.jsonl")),
            session_id: session.to_string(),
            agent: AgentKind::ClaudeCode,
            project: project.map(str::to_string),
            git_branch: None,
            version: None,
            started_at: 0,
            ended_at: 0,
            message_count: 0,
            first_prompt: String::new(),
            user_prompts: prompts,
            usage_entries: usage,
            is_subagent: false,
        }
    }

    fn ue(key: &str, model: &str, ts: i64, input: u64, output: u64) -> UsageEntry {
        UsageEntry {
            dedup_key: key.to_string(),
            model: model.to_string(),
            timestamp: ts,
            input,
            output,
            cache_read: 0,
            cache_creation: 0,
        }
    }

    fn pe(text: &str) -> PromptEntry {
        PromptEntry {
            id: text.to_string(),
            text: text.to_string(),
            project: "/p".to_string(),
            timestamp: 0,
            source: "history".to_string(),
            agent: AgentKind::ClaudeCode,
            kind: PromptKind::Human,
            message_uuid: None,
            session_id: None,
            git_branch: None,
            is_command: false,
            pasted_count: 0,
            char_count: text.chars().count(),
        }
    }

    // ---------- CacheV2 ----------

    #[test]
    fn history_cache_invalidates_when_path_changes_even_if_mtime_matches() {
        let cache = CacheV2 {
            version: CACHE_VERSION,
            history_path: "/old/.claude/history.jsonl".to_string(),
            history_stamp: FileStamp {
                mtime: 123,
                len: 10,
            },
            history_prompts: Vec::new(),
            files: HashMap::new(),
        };
        assert!(!history_cache_changed(
            &cache,
            "/old/.claude/history.jsonl",
            FileStamp {
                mtime: 123,
                len: 10,
            }
        ));
        assert!(history_cache_changed(
            &cache,
            "/new/.claude/history.jsonl",
            FileStamp {
                mtime: 123,
                len: 10,
            }
        ));
        assert!(history_cache_changed(
            &cache,
            "/old/.claude/history.jsonl",
            FileStamp {
                mtime: 123,
                len: 11,
            }
        ));
        assert!(history_cache_changed(
            &cache,
            "/old/.claude/history.jsonl",
            FileStamp { mtime: 0, len: 10 }
        ));
    }

    #[test]
    fn collect_all_conversation_files_skips_claude_subagents_only() {
        let root = std::env::temp_dir().join(format!(
            "cc_history_viewer_indexer_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let claude_project = root.join("claude").join("projects").join("project-a");
        let codex_sessions = root.join("codex").join("sessions");
        let subagents = claude_project.join("main-session").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::create_dir_all(&codex_sessions).unwrap();

        let main_file = claude_project.join("main-session.jsonl");
        let subagent_file = subagents.join("agent-1.jsonl");
        let codex_file = codex_sessions.join("rollout.jsonl");
        std::fs::write(&main_file, "").unwrap();
        std::fs::write(&subagent_file, "").unwrap();
        std::fs::write(&codex_file, "").unwrap();

        let paths = DataPaths {
            history: root.join("history.jsonl"),
            projects: root.join("claude").join("projects"),
            sessions: root.join("claude").join("sessions"),
            codex_sessions,
        };
        let files = collect_all_conversation_files(&paths);
        assert!(files
            .iter()
            .any(|(p, agent)| *agent == AgentKind::ClaudeCode && p == &main_file));
        assert!(files
            .iter()
            .any(|(p, agent)| *agent == AgentKind::Codex && p == &codex_file));
        assert!(!files.iter().any(|(p, _)| p == &subagent_file));

        let _ = std::fs::remove_dir_all(&root);
    }

    // ---------- merge_prompts ----------

    #[test]
    fn merge_same_text_within_window_becomes_both() {
        let t0 = 1_700_000_000_000i64;
        let history = vec![rp("跑测试", "/p/a", t0, PromptKind::Human, true, None)];
        let conv = vec![cf(
            "s1",
            Some("/p/a"),
            vec![rp(
                "跑测试",
                "/p/a",
                t0 + 60_000,
                PromptKind::Human,
                false,
                Some("s1"),
            )],
            vec![],
        )];
        let entries = merge_prompts(history, &conv);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].source, "both");
        assert_eq!(entries[0].session_id.as_deref(), Some("s1"));
        assert_eq!(
            entries[0].message_uuid.as_deref(),
            Some(format!("msg-{}", t0 + 60_000).as_str())
        );
        assert!(!entries[0].is_command);
    }

    #[test]
    fn merge_keeps_same_source_repeated_prompts_within_window() {
        let t0 = 1_700_000_000_000i64;
        let history = vec![
            rp("继续", "/p/a", t0, PromptKind::Human, true, None),
            rp("继续", "/p/a", t0 + 30_000, PromptKind::Human, true, None),
        ];
        let entries = merge_prompts(history, &[]);
        assert_eq!(entries.len(), 2, "同来源重复输入不能被去重吞掉");
        assert!(entries.iter().all(|e| e.source == "history"));
        assert_ne!(entries[0].id, entries[1].id);
    }

    #[test]
    fn merge_dedups_copied_conversation_prompts_by_message_uuid() {
        let t0 = 1_700_000_000_000i64;
        let mut original = rp("继续", "/p/a", t0, PromptKind::Human, false, Some("s1"));
        original.message_uuid = Some("shared-user-uuid".to_string());
        original.origin_key = "conversation:s1:shared-user-uuid".to_string();
        let mut copied = rp(
            "继续",
            "/p/a",
            t0 + 5_000,
            PromptKind::Human,
            false,
            Some("s2"),
        );
        copied.message_uuid = Some("shared-user-uuid".to_string());
        copied.origin_key = "conversation:s2:shared-user-uuid".to_string();

        let entries = merge_prompts(
            vec![],
            &[
                cf("s1", Some("/p/a"), vec![original], vec![]),
                cf("s2", Some("/p/a"), vec![copied], vec![]),
            ],
        );
        assert_eq!(
            entries.len(),
            1,
            "fork/resume 复制的同一 user uuid 不应重复计数"
        );
        assert_eq!(entries[0].message_uuid.as_deref(), Some("shared-user-uuid"));
    }

    #[test]
    fn merge_pairs_nearest_cross_source_prompt_globally() {
        let t0 = 1_700_000_000_000i64;
        let history = vec![
            rp("继续", "/p/a", t0, PromptKind::Human, true, None),
            rp("继续", "/p/a", t0 + 240_000, PromptKind::Human, true, None),
        ];
        let conv = vec![cf(
            "s1",
            Some("/p/a"),
            vec![rp(
                "继续",
                "/p/a",
                t0 + 241_000,
                PromptKind::Human,
                false,
                Some("s1"),
            )],
            vec![],
        )];
        let entries = merge_prompts(history, &conv);
        assert_eq!(entries.len(), 2);
        let both = entries.iter().find(|e| e.source == "both").unwrap();
        assert_eq!(both.timestamp, t0 + 240_000);
        assert_eq!(
            both.message_uuid.as_deref(),
            Some(format!("msg-{}", t0 + 241_000).as_str())
        );
    }

    #[test]
    fn merge_same_text_beyond_window_stays_separate() {
        let t0 = 1_700_000_000_000i64;
        let history = vec![rp("跑测试", "/p/a", t0, PromptKind::Human, true, None)];
        let conv = vec![cf(
            "s1",
            Some("/p/a"),
            vec![rp(
                "跑测试",
                "/p/a",
                t0 + DEDUP_WINDOW_MS + 1,
                PromptKind::Human,
                false,
                Some("s1"),
            )],
            vec![],
        )];
        let entries = merge_prompts(history, &conv);
        assert_eq!(entries.len(), 2);
        let sources: HashSet<&str> = entries.iter().map(|e| e.source.as_str()).collect();
        assert!(sources.contains("history"));
        assert!(sources.contains("conversation"));
    }

    #[test]
    fn merge_detects_command() {
        let entries = merge_prompts(
            vec![rp("/clear", "/p/a", 1_000, PromptKind::Command, true, None)],
            &[],
        );
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_command);
    }

    #[test]
    fn merge_keeps_human_and_command_same_text_separate() {
        let t0 = 1_700_000_000_000i64;
        let history = vec![rp("继续", "/p/a", t0, PromptKind::Human, true, None)];
        let conv = vec![cf(
            "s1",
            Some("/p/a"),
            vec![rp(
                "继续",
                "/p/a",
                t0 + 30_000,
                PromptKind::Command,
                false,
                Some("s1"),
            )],
            vec![],
        )];
        let entries = merge_prompts(history, &conv);
        assert_eq!(entries.len(), 2, "不同 kind 的同文本记录不应互相合并");
        let kinds: HashSet<PromptKind> = entries.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&PromptKind::Human));
        assert!(kinds.contains(&PromptKind::Command));
    }

    #[test]
    fn merge_orders_same_timestamp_prompts_deterministically() {
        let t0 = 1_700_000_000_000i64;
        let history = vec![
            rp("z project", "/p/z", t0, PromptKind::Human, true, None),
            rp("a project", "/p/a", t0, PromptKind::Human, true, None),
        ];
        let entries = merge_prompts(history, &[]);
        let projects: Vec<&str> = entries.iter().map(|e| e.project.as_str()).collect();
        assert_eq!(projects, vec!["/p/a", "/p/z"]);
    }

    // ---------- search ----------

    #[test]
    fn search_multi_keyword_and() {
        let prompts = vec![pe("foo something bar"), pe("foo only")];
        let r = search(&prompts, "foo bar", None, &PromptVisibility::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].entry.text, "foo something bar");
    }

    #[test]
    fn search_case_insensitive_non_ascii() {
        let prompts = vec![pe("Grab a café latte")];
        let r = search(&prompts, "CAFÉ", None, &PromptVisibility::default());
        assert_eq!(r.len(), 1, "大写 É 应命中小写 é");
        // "café" 起于 char 索引 7（高亮区间按 char 索引计）
        assert_eq!(r[0].match_ranges, vec![[7, 11]]);
    }

    #[test]
    fn search_merges_overlapping_ranges() {
        let prompts = vec![pe("abcdx")];
        let r = search(&prompts, "abc bcd", None, &PromptVisibility::default());
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].match_ranges, vec![[0, 4]], "重叠区间应合并");
    }

    // ---------- version_cmp / cache version ----------

    #[test]
    fn version_cmp_numeric_parts() {
        use std::cmp::Ordering;
        assert_eq!(version_cmp("2.1.10", "2.1.9"), Ordering::Greater);
        assert_eq!(version_cmp("2.0.0", "2.0.0"), Ordering::Equal);
        assert_eq!(version_cmp("1.9.9", "2.0.0"), Ordering::Less);
    }

    #[test]
    fn cache_version_precheck_tolerates_spaces() {
        assert_eq!(
            cache_version_from_json(r#"{"version":9,"files":{}}"#),
            Some(9)
        );
        assert_eq!(
            cache_version_from_json("{\n  \"version\": 9,\n  \"files\": {}\n}"),
            Some(9)
        );
        assert_eq!(cache_version_from_json(r#"{"files":{}}"#), None);
    }

    // ---------- compute_usage ----------

    #[test]
    fn usage_dedup_across_files_and_aggregation() {
        let ts = Local
            .with_ymd_and_hms(2026, 3, 1, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        // opus 4.1：15/75；1M input = 15 USD
        let e1 = ue("msg_1", "claude-opus-4-1-20250805", ts, 1_000_000, 0);
        let e1_copy = e1.clone(); // 模拟 resume 把旧行复制进另一个文件
        let e2 = ue("msg_2", "unknown-model-x", ts, 10, 20);
        let conv = vec![
            cf("s1", Some("/p/a"), vec![], vec![e1]),
            cf("s2", Some("/p/b"), vec![], vec![e1_copy, e2]),
        ];
        let u = compute_usage(&conv);
        assert_eq!(u.assistant_messages, 2, "重复 dedup_key 只记一次");
        assert_eq!(u.total_input, 1_000_010);
        assert_eq!(u.total_output, 20);
        assert_eq!(u.unknown_model_tokens, 30);
        assert!((u.est_cost_usd - 15.0).abs() < 1e-9);
        // by_model：已知成本在前，未知模型 est_cost_usd 为 None 且排最后
        assert_eq!(u.by_model.len(), 2);
        assert_eq!(u.by_model[0].model, "claude-opus-4-1-20250805");
        assert!(u.by_model[1].est_cost_usd.is_none());
        // by_day：同一天聚成一条
        assert_eq!(u.by_day.len(), 1);
        assert!((u.by_day[0].est_cost_usd - 15.0).abs() < 1e-9);
        // by_project：msg_1 只归属第一次出现的文件所在项目 /p/a
        let pa = u.by_project.iter().find(|p| p.path == "/p/a").unwrap();
        assert!((pa.est_cost_usd - 15.0).abs() < 1e-9);
        let pb = u.by_project.iter().find(|p| p.path == "/p/b").unwrap();
        assert_eq!(pb.input, 10);
    }

    #[test]
    fn usage_dedup_project_assignment_is_stable_after_sorting() {
        let ts = Local
            .with_ymd_and_hms(2026, 3, 1, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        let usage = ue("msg_shared", "claude-opus-4-1-20250805", ts, 1_000_000, 0);
        let conv = vec![
            cf("z-session", Some("/p/z"), vec![], vec![usage.clone()]),
            cf("a-session", Some("/p/a"), vec![], vec![usage]),
        ];
        let mut sorted = conv.clone();
        sorted.sort_by(|a, b| {
            a.path
                .to_string_lossy()
                .cmp(&b.path.to_string_lossy())
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        let u = compute_usage(&sorted);
        assert!(
            u.by_project.iter().any(|p| p.path == "/p/a"),
            "重复 usage 应稳定归属路径排序最先的会话"
        );
        assert!(!u.by_project.iter().any(|p| p.path == "/p/z"));
    }

    #[test]
    fn usage_by_model_ties_known_cost_by_model_name() {
        let ts = Local
            .with_ymd_and_hms(2026, 3, 1, 12, 0, 0)
            .unwrap()
            .timestamp_millis();
        let conv = vec![cf(
            "s1",
            Some("/p/a"),
            vec![],
            vec![
                ue("msg_1", "claude-sonnet-4-6-20260101", ts, 1_000_000, 0),
                ue("msg_2", "claude-sonnet-4-5-20260101", ts, 1_000_000, 0),
            ],
        )];
        let u = compute_usage(&conv);
        let models: Vec<&str> = u.by_model.iter().map(|m| m.model.as_str()).collect();
        assert_eq!(
            models,
            vec!["claude-sonnet-4-5-20260101", "claude-sonnet-4-6-20260101"]
        );
    }
}
