//! JSONL 数据解析：Claude history/projects 与 Codex sessions JSONL。

use crate::models::{AgentKind, ChatMessage, ContentBlock, ConversationDetail, PromptKind};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// 超过此长度的行不参与 prompt 提取（多为 base64 图片 / 工具结果）
const MAX_LINE_FOR_PROMPT: usize = 2_000_000;
/// 对话详情中单个内容块的最大字符数，超出则截断
const MAX_BLOCK_CHARS: usize = 24_000;

/// 解析过程中的中间 prompt 表示（参与文件级缓存序列化）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawPrompt {
    pub text: String,
    pub agent: AgentKind,
    pub project: String,
    pub timestamp: i64,
    pub kind: PromptKind,
    /// 原始记录的稳定来源键，用于区分同文本、同时间附近的真实重复 prompt。
    pub origin_key: String,
    /// 对话文件中的 user 消息 uuid；history.jsonl 没有该字段。
    pub message_uuid: Option<String>,
    pub session_id: Option<String>,
    pub git_branch: Option<String>,
    pub pasted_count: usize,
    pub from_history: bool,
}

/// 单条 assistant 消息的 token 用量（参与文件级缓存序列化）。
/// resume / fork 会把旧消息行复制进新文件，跨文件聚合时必须按 dedup_key 全局去重。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageEntry {
    /// 去重键：优先 message.id，其次行 uuid，其次 requestId，最后 "{session_id}:{行号}"
    pub dedup_key: String,
    pub model: String,
    pub timestamp: i64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

/// 单个对话文件的解析结果（参与文件级缓存序列化）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvFileResult {
    pub path: PathBuf,
    pub session_id: String,
    pub agent: AgentKind,
    pub project: Option<String>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    pub started_at: i64,
    pub ended_at: i64,
    pub message_count: usize,
    pub first_prompt: String,
    pub user_prompts: Vec<RawPrompt>,
    /// assistant 行的 token 用量（文件内已按 dedup_key 去重；跨文件去重在 indexer 完成）
    pub usage_entries: Vec<UsageEntry>,
    /// 是否为子代理会话文件。Codex 子代理是独立 rollout 文件
    /// （session_meta.source.subagent），索引装配阶段整体排除，不作为独立会话。
    /// Claude Code 子代理文件按 subagents/ 目录在文件收集阶段已过滤，此处恒为 false。
    pub is_subagent: bool,
}

// ----------------------------- history.jsonl -----------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryLine {
    display: Option<String>,
    pasted_contents: Option<serde_json::Value>,
    timestamp: Option<i64>,
    project: Option<String>,
    session_id: Option<String>,
}

/// 解析 ~/.claude/history.jsonl
pub fn parse_history(path: &Path) -> Vec<RawPrompt> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: HistoryLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let display = match parsed.display {
            Some(d) => d,
            None => continue,
        };
        let timestamp = match parsed.timestamp {
            Some(t) => t,
            None => continue,
        };
        let project = match parsed.project {
            Some(p) => p,
            None => continue,
        };
        let text = normalize_image_placeholders(display.trim());
        if text.is_empty() {
            continue;
        }
        let pasted_count = match parsed.pasted_contents {
            Some(serde_json::Value::Object(m)) => m.len(),
            _ => 0,
        };
        out.push(RawPrompt {
            text,
            agent: AgentKind::ClaudeCode,
            project,
            timestamp,
            kind: classify_history_prompt_kind(&display),
            origin_key: format!("history:{}", line_no + 1),
            message_uuid: None,
            session_id: parsed.session_id,
            git_branch: None,
            pasted_count,
            from_history: true,
        });
    }
    out
}

// --------------------------- 对话 JSONL ---------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConvLine {
    #[serde(rename = "type")]
    line_type: Option<String>,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    prompt_id: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    git_branch: Option<String>,
    version: Option<String>,
    is_sidechain: Option<bool>,
    is_meta: Option<bool>,
    origin: Option<ConvOrigin>,
    #[serde(rename = "promptSource")]
    prompt_source: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    /// system 行的子类型（如 "local_command"：斜杠命令的调用标记）
    subtype: Option<String>,
    /// system 行的顶层内容（local_command 时为命令文本）
    content: Option<serde_json::Value>,
    message: Option<ConvMessage>,
    attachment: Option<ConvAttachment>,
    attribution_skill: Option<String>,
}

struct PendingClaudePrompt {
    parent_uuid: Option<String>,
    prompt_id: Option<String>,
    text: String,
    origin_key: String,
    counted: bool,
    assistant_seen: bool,
}

struct PendingClaudeDetailUser {
    parent_uuid: Option<String>,
    prompt_id: Option<String>,
    text: String,
    start: usize,
    len: usize,
    assistant_seen: bool,
}

#[derive(Deserialize)]
struct ConvOrigin {
    kind: Option<String>,
}

#[derive(Deserialize)]
struct ConvMessage {
    role: Option<String>,
    content: Option<serde_json::Value>,
    /// assistant 行才有：API 消息 id（msg_xxx），用作用量去重键
    id: Option<String>,
    /// assistant 行才有：模型 id
    model: Option<String>,
    /// assistant 行才有：token 用量
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConvAttachment {
    #[serde(rename = "type")]
    attachment_type: Option<String>,
    content: Option<serde_json::Value>,
    skills: Option<Vec<InvokedSkill>>,
}

#[derive(Deserialize)]
struct InvokedSkill {
    name: Option<String>,
    content: Option<String>,
}

/// assistant 行 message.usage 的原始形状（JSON 字段即 snake_case，无需 rename）
#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

/// ISO8601 字符串转毫秒时间戳
fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// 解析单个对话文件，提取 user prompt 与会话摘要信息。
pub fn parse_conversation_file(path: &Path) -> Option<ConvFileResult> {
    let content = fs::read_to_string(path).ok()?;
    let session_id = path.file_stem()?.to_string_lossy().to_string();

    let mut project: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut version: Option<String> = None;
    let mut started_at = i64::MAX;
    let mut ended_at = i64::MIN;
    let mut message_count = 0usize;
    let mut first_human_prompt = String::new();
    let mut first_any_prompt = String::new();
    let mut user_prompts: Vec<RawPrompt> = Vec::new();
    let mut usage_entries: Vec<UsageEntry> = Vec::new();
    let mut pending_prompt: Option<PendingClaudePrompt> = None;
    // 文件内去重：同一 API 响应会按内容块拆成多行（message.id 相同、usage 相同），只记一次
    let mut seen_usage_keys: HashSet<String> = HashSet::new();

    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let too_big = line.len() > MAX_LINE_FOR_PROMPT;
        let parsed: ConvLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ltype = parsed.line_type.as_deref().unwrap_or("");
        let ts = parsed.timestamp.as_deref().and_then(iso_to_ms);
        if let Some(t) = ts {
            if t < started_at {
                started_at = t;
            }
            if t > ended_at {
                ended_at = t;
            }
        }
        if project.is_none() {
            if let Some(c) = parsed.cwd.clone() {
                if !c.is_empty() {
                    project = Some(c);
                }
            }
        }
        if git_branch.is_none() {
            if let Some(b) = parsed.git_branch.clone() {
                if !b.is_empty() {
                    git_branch = Some(b);
                }
            }
        }
        if version.is_none() {
            version = parsed.version.clone();
        }

        // assistant 行（含 sidechain，同样计入用量）提取 token 用量
        if ltype == "assistant" {
            if let Some(e) =
                extract_usage_entry(&parsed, &session_id, line_no, ts, &mut seen_usage_keys)
            {
                usage_entries.push(e);
            }
            let has_visible_answer = parsed
                .message
                .as_ref()
                .and_then(|m| m.content.as_ref())
                .is_some_and(assistant_content_has_visible_answer);
            if has_visible_answer {
                message_count += 1;
            }
            if has_visible_answer
                || parsed
                    .message
                    .as_ref()
                    .and_then(|m| m.content.as_ref())
                    .is_some_and(assistant_content_has_model_output)
            {
                if let Some(pending) = pending_prompt.as_mut() {
                    pending.assistant_seen = true;
                }
            }
        }
        if ltype != "user" || too_big {
            continue;
        }
        let msg = match &parsed.message {
            Some(m) => m,
            None => continue,
        };
        // 仅保留真正的 user 角色
        if let Some(role) = &msg.role {
            if role != "user" {
                continue;
            }
        }
        let content_val = match &msg.content {
            Some(c) => c,
            None => continue,
        };
        let prompt_text = match extract_prompt_text(content_val) {
            Some(t) => t,
            None => continue,
        };
        let kind = classify_conversation_prompt_kind(&parsed, content_val);
        if !matches!(kind, PromptKind::Meta | PromptKind::System) {
            message_count += 1;
        }
        let timestamp = match ts {
            Some(t) => t,
            None => continue,
        };
        let stable_uuid = parsed
            .uuid
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{session_id}:{}", line_no + 1));
        let origin_key = format!("conversation:{session_id}:{stable_uuid}");
        let counted = !matches!(kind, PromptKind::Meta | PromptKind::System);
        if counted
            && pending_prompt.as_ref().is_some_and(|pending| {
                is_claude_replay_duplicate(
                    !pending.assistant_seen,
                    pending.parent_uuid.as_deref(),
                    pending.prompt_id.as_deref(),
                    &pending.text,
                    parsed.parent_uuid.as_deref(),
                    parsed.prompt_id.as_deref(),
                    &prompt_text,
                )
            })
        {
            if let Some(pending) = pending_prompt.take() {
                user_prompts.retain(|p| p.origin_key != pending.origin_key);
                if pending.counted {
                    message_count = message_count.saturating_sub(1);
                }
            }
        }
        if first_human_prompt.is_empty() {
            if let Some(title) = conversation_title_candidate(content_val, kind, &prompt_text) {
                first_human_prompt = title;
            }
        }
        if first_any_prompt.is_empty() {
            first_any_prompt = prompt_text.clone();
        }
        let line_project = parsed
            .cwd
            .clone()
            .filter(|c| !c.is_empty())
            .or_else(|| project.clone())
            .unwrap_or_default();
        user_prompts.push(RawPrompt {
            text: prompt_text,
            agent: AgentKind::ClaudeCode,
            project: line_project,
            timestamp,
            kind,
            origin_key: origin_key.clone(),
            message_uuid: Some(stable_uuid),
            session_id: Some(session_id.clone()),
            git_branch: parsed
                .git_branch
                .clone()
                .filter(|b| !b.is_empty())
                .or_else(|| git_branch.clone()),
            pasted_count: 0,
            from_history: false,
        });
        if counted {
            pending_prompt = Some(PendingClaudePrompt {
                parent_uuid: parsed.parent_uuid.clone(),
                prompt_id: parsed.prompt_id.clone(),
                text: user_prompts
                    .last()
                    .map(|p| p.text.clone())
                    .unwrap_or_default(),
                origin_key,
                counted,
                assistant_seen: false,
            });
        }
    }

    if started_at == i64::MAX {
        started_at = 0;
    }
    if ended_at == i64::MIN {
        ended_at = 0;
    }

    // 回填：早于首个 cwd 出现的 prompt 行没有 project
    if let Some(proj) = &project {
        for p in user_prompts.iter_mut() {
            if p.project.is_empty() {
                p.project = proj.clone();
            }
        }
    }

    let first_prompt = if !first_human_prompt.is_empty() {
        first_human_prompt
    } else {
        first_any_prompt
    };

    Some(ConvFileResult {
        path: path.to_path_buf(),
        session_id,
        agent: AgentKind::ClaudeCode,
        project,
        git_branch,
        version,
        started_at,
        ended_at,
        message_count,
        first_prompt,
        user_prompts,
        usage_entries,
        // Claude 子代理文件按路径（subagents/）在索引阶段整体排除，走不到这里
        is_subagent: false,
    })
}

fn assistant_content_has_visible_answer(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::String(s) => !prettify_display_text(s.trim()).is_empty(),
        serde_json::Value::Array(arr) => arr.iter().any(|block| {
            let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match bt {
                "text" => block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| !prettify_display_text(t).is_empty()),
                "image" => true,
                _ => false,
            }
        }),
        _ => false,
    }
}

fn assistant_content_has_model_output(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::String(s) => !s.trim().is_empty(),
        serde_json::Value::Array(arr) => arr.iter().any(|block| {
            let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
            matches!(bt, "text" | "thinking" | "tool_use")
                || block
                    .get("text")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.trim().is_empty())
                || block.get("input").is_some()
        }),
        _ => false,
    }
}

fn is_claude_replay_duplicate(
    previous_is_pending: bool,
    previous_parent_uuid: Option<&str>,
    previous_prompt_id: Option<&str>,
    previous_text: &str,
    current_parent_uuid: Option<&str>,
    current_prompt_id: Option<&str>,
    current_text: &str,
) -> bool {
    previous_is_pending
        && previous_parent_uuid.is_some()
        && previous_parent_uuid == current_parent_uuid
        && previous_text.trim() == current_text.trim()
        && match (previous_prompt_id, current_prompt_id) {
            (Some(a), Some(b)) => a != b,
            _ => true,
        }
}

/// 从 assistant 行提取一条用量记录。
/// 跳过：model 为空或 `<synthetic>` 的行、usage 缺失或四项全为 0 的行、文件内重复 dedup_key 的行。
fn extract_usage_entry(
    line: &ConvLine,
    session_id: &str,
    line_no: usize,
    ts: Option<i64>,
    seen: &mut HashSet<String>,
) -> Option<UsageEntry> {
    let msg = line.message.as_ref()?;
    let model = msg.model.clone().unwrap_or_default();
    if model.is_empty() || model == "<synthetic>" {
        return None;
    }
    let u = msg.usage.as_ref()?;
    let input = u.input_tokens.unwrap_or(0);
    let output = u.output_tokens.unwrap_or(0);
    let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);
    let cache_read = u.cache_read_input_tokens.unwrap_or(0);
    if input == 0 && output == 0 && cache_creation == 0 && cache_read == 0 {
        return None;
    }
    // 去重键优先级：message.id > 行 uuid > requestId > "{session_id}:{行号(1 基)}"
    let dedup_key = msg
        .id
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| line.uuid.clone().filter(|s| !s.is_empty()))
        .or_else(|| line.request_id.clone().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("{session_id}:{}", line_no + 1));
    if !seen.insert(dedup_key.clone()) {
        return None;
    }
    Some(UsageEntry {
        dedup_key,
        model,
        timestamp: ts.unwrap_or(0),
        input,
        output,
        cache_read,
        cache_creation,
    })
}

/// 解析对话文件的完整内容（用于「对话详情」页面）。
pub fn parse_conversation_detail(path: &Path) -> Option<ConversationDetail> {
    let content = fs::read_to_string(path).ok()?;
    let session_id = path.file_stem()?.to_string_lossy().to_string();

    let mut project: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut version: Option<String> = None;
    let mut started_at = i64::MAX;
    let mut ended_at = i64::MIN;
    let mut messages: Vec<ChatMessage> = Vec::new();
    let mut pending_user: Option<PendingClaudeDetailUser> = None;

    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: ConvLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ltype = parsed.line_type.as_deref().unwrap_or("");
        let ts = parsed.timestamp.as_deref().and_then(iso_to_ms);
        if let Some(t) = ts {
            if t < started_at {
                started_at = t;
            }
            if t > ended_at {
                ended_at = t;
            }
        }
        if project.is_none() {
            if let Some(c) = &parsed.cwd {
                if !c.is_empty() {
                    project = Some(c.clone());
                }
            }
        }
        if git_branch.is_none() {
            if let Some(b) = &parsed.git_branch {
                if !b.is_empty() {
                    git_branch = Some(b.clone());
                }
            }
        }
        if version.is_none() {
            version = parsed.version.clone();
        }
        // 斜杠命令的调用标记（system/local_command）也呈现在对话流里：
        // /btw 这类侧问命令的回答 CC 不持久化，但至少能看到「这里执行过命令」。
        let stable_uuid = parsed
            .uuid
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{session_id}:{}", line_no + 1));
        if ltype == "system" {
            if parsed.subtype.as_deref() == Some("local_command") {
                if let Some(serde_json::Value::String(c)) = &parsed.content {
                    let text = prettify_display_text(c);
                    if !text.is_empty() {
                        messages.push(ChatMessage {
                            uuid: stable_uuid,
                            role: "system".to_string(),
                            phase: None,
                            call_id: None,
                            timestamp: ts.unwrap_or(0),
                            is_sidechain: false,
                            is_subagent: false,
                            is_meta: parsed.is_meta.unwrap_or(false),
                            meta_kind: Some("command".to_string()),
                            attribution_skill: None,
                            blocks: vec![ContentBlock {
                                kind: "text".to_string(),
                                text: Some(text),
                                tool_name: None,
                                tool_input: None,
                            }],
                        });
                    }
                }
            }
            continue;
        }
        if ltype == "attachment" {
            if let Some(blocks) = skill_attachment_blocks(parsed.attachment.as_ref()) {
                if !blocks.is_empty() {
                    messages.push(ChatMessage {
                        uuid: stable_uuid,
                        role: "system".to_string(),
                        phase: None,
                        call_id: None,
                        timestamp: ts.unwrap_or(0),
                        is_sidechain: parsed.is_sidechain.unwrap_or(false),
                        is_subagent: false,
                        is_meta: parsed.is_meta.unwrap_or(false),
                        meta_kind: Some("skill".to_string()),
                        attribution_skill: None,
                        blocks,
                    });
                }
            }
            continue;
        }
        if ltype != "user" && ltype != "assistant" {
            continue;
        }
        let msg = match &parsed.message {
            Some(m) => m,
            None => continue,
        };
        let role = msg.role.clone().unwrap_or_else(|| ltype.to_string());
        let blocks = content_to_parsed_blocks(msg.content.as_ref());
        if blocks.is_empty() {
            continue;
        }
        if ltype == "assistant" {
            if let Some(pending) = pending_user.as_mut() {
                pending.assistant_seen = true;
            }
        }
        let replay_user_text = if ltype == "user"
            && role == "user"
            && !parsed.is_meta.unwrap_or(false)
            && !parsed.is_sidechain.unwrap_or(false)
        {
            parsed
                .message
                .as_ref()
                .and_then(|m| m.content.as_ref())
                .and_then(extract_prompt_text)
        } else {
            None
        };
        if let Some(text) = replay_user_text.as_deref() {
            if pending_user.as_ref().is_some_and(|pending| {
                is_claude_replay_duplicate(
                    !pending.assistant_seen,
                    pending.parent_uuid.as_deref(),
                    pending.prompt_id.as_deref(),
                    &pending.text,
                    parsed.parent_uuid.as_deref(),
                    parsed.prompt_id.as_deref(),
                    text,
                )
            }) {
                if let Some(pending) = pending_user.take() {
                    let end = pending
                        .start
                        .saturating_add(pending.len)
                        .min(messages.len());
                    if pending.start < end {
                        messages.drain(pending.start..end);
                    }
                }
            }
        }
        let start_len = messages.len();
        append_claude_messages(
            &mut messages,
            &stable_uuid,
            &role,
            ts.unwrap_or(0),
            parsed.is_sidechain.unwrap_or(false),
            parsed.is_meta.unwrap_or(false),
            parsed.attribution_skill.clone(),
            blocks,
        );
        if let Some(text) = replay_user_text {
            let added = messages.len().saturating_sub(start_len);
            if added > 0 {
                pending_user = Some(PendingClaudeDetailUser {
                    parent_uuid: parsed.parent_uuid.clone(),
                    prompt_id: parsed.prompt_id.clone(),
                    text,
                    start: start_len,
                    len: added,
                    assistant_seen: false,
                });
            }
        }
    }

    if started_at == i64::MAX {
        started_at = 0;
    }
    if ended_at == i64::MIN {
        ended_at = 0;
    }

    Some(ConversationDetail {
        session_id,
        project: project.unwrap_or_default(),
        agent: AgentKind::ClaudeCode,
        git_branch,
        started_at,
        ended_at,
        version,
        messages: finalize_conversation_messages(messages),
    })
}

// --------------------------- Codex sessions JSONL ---------------------------

#[derive(Deserialize)]
struct CodexLine {
    #[serde(rename = "type")]
    line_type: Option<String>,
    timestamp: Option<String>,
    payload: Option<serde_json::Value>,
}

#[derive(Default)]
struct CodexMeta {
    session_id: Option<String>,
    project: Option<String>,
    git_branch: Option<String>,
    version: Option<String>,
    model: Option<String>,
    /// session_meta.source.subagent 存在 → 这是被父会话 spawn 的子代理文件
    is_subagent: bool,
}

#[derive(Clone)]
struct CodexUserEvent {
    timestamp: i64,
    text: String,
}

#[derive(Clone)]
struct ParsedContentBlock {
    block: ContentBlock,
    call_id: Option<String>,
}

/// 解析单个 Codex session 文件，提取真实 user prompt 与会话摘要信息。
///
/// Codex JSONL 中同一条用户输入通常会同时出现：
/// - `response_item` + `payload.type=message` + `role=user`：进入 Responses 上下文的 item；
/// - `event_msg` + `payload.type=user_message`：客户端事件流。
///
/// 这里优先采用 `event_msg.user_message` 作为真实用户输入，避免把上下文注入
/// （如 AGENTS.md、environment_context）误当作 prompt；若文件缺少事件流再退回 response item。
pub fn parse_codex_session_file(path: &Path) -> Option<ConvFileResult> {
    let content = fs::read_to_string(path).ok()?;
    let file_stem = path.file_stem()?.to_string_lossy().to_string();

    let mut meta = CodexMeta::default();
    let mut started_at = i64::MAX;
    let mut ended_at = i64::MIN;
    let mut assistant_message_count = 0usize;
    let mut first_prompt = String::new();
    let mut event_prompts: Vec<CodexUserEvent> = Vec::new();
    let mut fallback_prompts: Vec<CodexUserEvent> = Vec::new();
    let mut last_compaction_candidate: Option<String> = None;
    let mut usage_entries: Vec<UsageEntry> = Vec::new();
    let mut seen_usage_keys: HashSet<String> = HashSet::new();

    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let too_big = line.len() > MAX_LINE_FOR_PROMPT;
        let parsed: CodexLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts = parsed.timestamp.as_deref().and_then(iso_to_ms);
        if let Some(t) = ts {
            if t < started_at {
                started_at = t;
            }
            if t > ended_at {
                ended_at = t;
            }
        }
        let payload = match parsed.payload.as_ref() {
            Some(p) => p,
            None => continue,
        };
        match parsed.line_type.as_deref().unwrap_or("") {
            "session_meta" => apply_codex_session_meta(payload, &mut meta),
            "turn_context" => apply_codex_turn_context(payload, &mut meta),
            "event_msg" => {
                if payload.get("type").and_then(|v| v.as_str()) == Some("user_message") {
                    if !too_big {
                        if let Some(text) = payload
                            .get("message")
                            .and_then(|v| v.as_str())
                            .and_then(clean_prompt_text)
                        {
                            event_prompts.push(CodexUserEvent {
                                timestamp: ts.unwrap_or(0),
                                text,
                            });
                        }
                    }
                } else if payload.get("type").and_then(|v| v.as_str()) == Some("task_complete") {
                    // 仅表示一轮任务结束，不是独立对话消息。
                } else if payload.get("type").and_then(|v| v.as_str()) == Some("token_count") {
                    if let Some(e) = extract_codex_usage_entry(
                        payload,
                        &file_stem,
                        line_no,
                        ts,
                        &meta,
                        &mut seen_usage_keys,
                    ) {
                        usage_entries.push(e);
                    }
                }
            }
            "response_item" => {
                let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if ptype == "message" {
                    match payload.get("role").and_then(|v| v.as_str()).unwrap_or("") {
                        "assistant" => {
                            if let Some(text) = codex_countable_response_message_text(payload) {
                                assistant_message_count += 1;
                                if looks_like_codex_compaction_summary(&text) {
                                    last_compaction_candidate = Some(text);
                                }
                            }
                        }
                        "user" if !too_big => {
                            if let Some(text) = codex_response_message_text(payload)
                                .and_then(|s| clean_prompt_text(&s))
                            {
                                if !is_codex_context_injection(&text) {
                                    fallback_prompts.push(CodexUserEvent {
                                        timestamp: ts.unwrap_or(0),
                                        text,
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "compacted" => {
                if let Some(candidate) = last_compaction_candidate.take() {
                    if codex_compaction_payload_contains_response(payload, &candidate) {
                        assistant_message_count = assistant_message_count.saturating_sub(1);
                    }
                }
            }
            _ => {}
        }
    }

    let session_id = meta.session_id.clone().unwrap_or(file_stem);
    let project = meta.project.clone().unwrap_or_default();
    let (chosen, user_message_count) = if event_prompts.is_empty() {
        let count = fallback_prompts.len();
        (fallback_prompts, count)
    } else {
        let count = event_prompts.len();
        (event_prompts, count)
    };
    let mut user_prompts = Vec::new();
    for (idx, p) in chosen.into_iter().enumerate() {
        if first_prompt.is_empty() {
            first_prompt = p.text.clone();
        }
        let stable_uuid = format!("{session_id}:user:{}", idx + 1);
        user_prompts.push(RawPrompt {
            text: p.text,
            agent: AgentKind::Codex,
            project: project.clone(),
            timestamp: p.timestamp,
            kind: PromptKind::Human,
            origin_key: format!("codex:{session_id}:{stable_uuid}"),
            message_uuid: Some(stable_uuid),
            session_id: Some(provider_session_id(AgentKind::Codex, &session_id)),
            git_branch: meta.git_branch.clone(),
            pasted_count: 0,
            from_history: false,
        });
    }

    if started_at == i64::MAX {
        started_at = 0;
    }
    if ended_at == i64::MIN {
        ended_at = 0;
    }
    let message_count = user_message_count + assistant_message_count;

    Some(ConvFileResult {
        path: path.to_path_buf(),
        session_id: provider_session_id(AgentKind::Codex, &session_id),
        agent: AgentKind::Codex,
        project: meta.project,
        git_branch: meta.git_branch,
        version: meta.version,
        started_at,
        ended_at,
        message_count,
        first_prompt,
        user_prompts,
        usage_entries,
        // Codex 子代理是独立 rollout 文件（session_meta.source.subagent）；
        // 标记后在索引装配阶段整体排除，不作为独立会话展示
        is_subagent: meta.is_subagent,
    })
}

/// 解析 Codex 对话详情。详情页展示采用 response_item 作为主数据源：
/// 它包含 assistant 文本、reasoning、工具调用与工具结果；event_msg 只补充 user_message。
pub fn parse_codex_conversation_detail(path: &Path) -> Option<ConversationDetail> {
    let content = fs::read_to_string(path).ok()?;
    let file_stem = path.file_stem()?.to_string_lossy().to_string();

    let mut meta = CodexMeta::default();
    let mut started_at = i64::MAX;
    let mut ended_at = i64::MIN;
    let mut messages: Vec<ChatMessage> = Vec::new();
    let event_user_prompts = collect_codex_event_user_prompts(&content);
    let mut call_names: HashMap<String, String> = HashMap::new();
    let mut seen_tool_results: HashSet<(String, String, String)> = HashSet::new();
    let mut event_user_idx = 0usize;
    let mut fallback_user_idx = 0usize;
    let mut last_compaction_candidate: Option<usize> = None;

    for (line_no, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: CodexLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts = parsed.timestamp.as_deref().and_then(iso_to_ms);
        if let Some(t) = ts {
            if t < started_at {
                started_at = t;
            }
            if t > ended_at {
                ended_at = t;
            }
        }
        let payload = match parsed.payload.as_ref() {
            Some(p) => p,
            None => continue,
        };
        let stable_uuid =
            codex_payload_id(payload).unwrap_or_else(|| format!("{}:{}", file_stem, line_no + 1));

        match parsed.line_type.as_deref().unwrap_or("") {
            "session_meta" => apply_codex_session_meta(payload, &mut meta),
            "turn_context" => apply_codex_turn_context(payload, &mut meta),
            "event_msg" => {
                if payload.get("type").and_then(|v| v.as_str()) == Some("user_message") {
                    let raw = payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if let Some(text) = clean_prompt_text(raw) {
                        event_user_idx += 1;
                        let raw_session_id =
                            meta.session_id.clone().unwrap_or_else(|| file_stem.clone());
                        messages.push(ChatMessage {
                            uuid: format!("{raw_session_id}:user:{event_user_idx}"),
                            role: "user".to_string(),
                            phase: None,
                            call_id: None,
                            timestamp: ts.unwrap_or(0),
                            is_sidechain: false,
                            is_subagent: false,
                            is_meta: false,
                            meta_kind: None,
                            attribution_skill: None,
                            blocks: vec![ContentBlock {
                                kind: "text".to_string(),
                                text: Some(truncate(&text)),
                                tool_name: None,
                                tool_input: None,
                            }],
                        });
                    }
                } else if payload.get("type").and_then(|v| v.as_str()) == Some("web_search_end") {
                    if let Some(msg) = codex_event_tool_result_to_message(payload, &stable_uuid, ts)
                    {
                        push_unique_tool_result(&mut messages, msg, &mut seen_tool_results);
                    }
                }
            }
            "response_item" => {
                if let Some(mut msg) =
                    codex_response_item_to_message(payload, &stable_uuid, ts, &mut call_names)
                {
                    if msg.role == "user" {
                        let text = msg
                            .blocks
                            .iter()
                            .filter(|b| b.kind == "text")
                            .filter_map(|b| b.text.as_deref())
                            .collect::<Vec<_>>()
                            .join("\n");
                        if is_codex_skill_injection(&text) {
                            msg.role = "system".to_string();
                            msg.meta_kind = Some("skill".to_string());
                            msg.blocks = msg
                                .blocks
                                .into_iter()
                                .map(|mut block| {
                                    if block.kind == "text" {
                                        block.kind = "skill".to_string();
                                        block.tool_name = Some("codex_skill".to_string());
                                    }
                                    block
                                })
                                .collect();
                            messages.push(msg);
                            continue;
                        }
                        if has_near_codex_event_user(&event_user_prompts, ts, &text)
                            || is_codex_context_injection(&text)
                        {
                            continue;
                        }
                        if event_user_prompts.is_empty() {
                            fallback_user_idx += 1;
                            let raw_session_id =
                                meta.session_id.clone().unwrap_or_else(|| file_stem.clone());
                            msg.uuid = format!("{raw_session_id}:user:{fallback_user_idx}");
                        }
                    }
                    let is_compaction_candidate = msg.role == "assistant"
                        && msg.phase.as_deref() == Some("final_answer")
                        && msg.blocks.iter().any(|b| {
                            b.kind == "text"
                                && b.text
                                    .as_deref()
                                    .is_some_and(looks_like_codex_compaction_summary)
                        });
                    if is_tool_result_message(&msg) {
                        push_unique_tool_result(&mut messages, msg, &mut seen_tool_results);
                    } else {
                        messages.push(msg);
                    }
                    if is_compaction_candidate {
                        last_compaction_candidate = Some(messages.len().saturating_sub(1));
                    }
                }
            }
            "compacted" => {
                remove_codex_compaction_summary(
                    &mut messages,
                    payload,
                    &mut last_compaction_candidate,
                );
            }
            _ => {}
        }
    }

    if started_at == i64::MAX {
        started_at = 0;
    }
    if ended_at == i64::MIN {
        ended_at = 0;
    }
    let raw_session_id = meta.session_id.clone().unwrap_or(file_stem);
    Some(ConversationDetail {
        session_id: provider_session_id(AgentKind::Codex, &raw_session_id),
        project: meta.project.unwrap_or_default(),
        agent: AgentKind::Codex,
        git_branch: meta.git_branch,
        started_at,
        ended_at,
        version: meta.version,
        messages: finalize_conversation_messages(messages),
    })
}

fn remove_codex_compaction_summary(
    messages: &mut Vec<ChatMessage>,
    payload: &serde_json::Value,
    last_candidate: &mut Option<usize>,
) {
    let compacted_text = codex_compaction_payload_text(payload);
    if compacted_text.is_empty() {
        return;
    }
    let Some(idx) = last_candidate.take() else {
        return;
    };
    let Some(candidate) = messages.get(idx) else {
        return;
    };
    if candidate.role != "assistant" || candidate.phase.as_deref() != Some("final_answer") {
        return;
    }
    let candidate_text = message_text(candidate);
    if codex_compaction_payload_contains_response(payload, &candidate_text) {
        messages.remove(idx);
    }
}

fn compaction_payload_contains_message(compacted_text: &str, message: &str) -> bool {
    let message = message.trim();
    if message.is_empty() {
        return false;
    }
    if compacted_text.contains(message) {
        return true;
    }
    let prefix: String = message.chars().take(1_000).collect();
    prefix.len() >= 40 && compacted_text.contains(prefix.trim())
}

fn codex_compaction_payload_text(payload: &serde_json::Value) -> String {
    payload
        .get("message")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("summary").and_then(|v| v.as_str()))
        .or_else(|| payload.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn codex_compaction_payload_contains_response(
    payload: &serde_json::Value,
    candidate_text: &str,
) -> bool {
    let compacted_text = codex_compaction_payload_text(payload);
    !compacted_text.is_empty()
        && looks_like_codex_compaction_summary(candidate_text)
        && compaction_payload_contains_message(&compacted_text, candidate_text)
}

fn looks_like_codex_compaction_summary(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("**Task Context**")
        || trimmed.starts_with("**Task state**")
        || trimmed.starts_with("**Workspace / User Goal**")
        || trimmed.starts_with("**Handoff Summary**")
        || trimmed.starts_with("## Handoff Summary")
        || trimmed.starts_with("**Checkpoint Summary**")
        || trimmed.starts_with("**任务背景**")
        || trimmed.starts_with("- User request:")
        || trimmed.starts_with("- User asked:")
        || (trimmed.starts_with("We are in ") && trimmed.contains("User asked"))
}

fn codex_countable_response_message_text(payload: &serde_json::Value) -> Option<String> {
    if payload.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return None;
    }
    if payload.get("phase").and_then(|v| v.as_str()) == Some("commentary") {
        return None;
    }
    codex_response_message_text(payload).filter(|text| !text.trim().is_empty())
}

fn push_unique_tool_result(
    messages: &mut Vec<ChatMessage>,
    msg: ChatMessage,
    seen: &mut HashSet<(String, String, String)>,
) {
    let Some(call_id) = msg.call_id.clone() else {
        messages.push(msg);
        return;
    };
    if !is_tool_result_message(&msg) {
        messages.push(msg);
        return;
    }
    let tool_name = msg
        .blocks
        .iter()
        .find(|b| b.kind == "tool_result")
        .and_then(|b| b.tool_name.clone())
        .unwrap_or_default();
    let text = msg
        .blocks
        .iter()
        .filter(|b| b.kind == "tool_result")
        .filter_map(|b| b.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    if seen.insert((call_id, tool_name, text)) {
        messages.push(msg);
    }
}

fn group_tool_results(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let call_ids: HashSet<String> = messages
        .iter()
        .filter(|msg| is_tool_call_message(msg))
        .filter_map(|msg| msg.call_id.clone())
        .collect();
    let mut output_by_call: HashMap<String, Vec<ChatMessage>> = HashMap::new();
    for msg in messages.iter() {
        if is_tool_result_message(msg) {
            if let Some(call_id) = msg.call_id.clone() {
                if call_ids.contains(&call_id) {
                    output_by_call.entry(call_id).or_default().push(msg.clone());
                }
            }
        }
    }

    let mut ordered = Vec::with_capacity(messages.len());
    for msg in messages {
        if is_tool_result_message(&msg) {
            if msg
                .call_id
                .as_ref()
                .is_some_and(|call_id| call_ids.contains(call_id))
            {
                continue;
            }
        }
        let call_id = msg.call_id.clone();
        let is_call = is_tool_call_message(&msg);
        ordered.push(msg);
        if is_call {
            if let Some(call_id) = call_id {
                if let Some(mut outputs) = output_by_call.remove(&call_id) {
                    ordered.append(&mut outputs);
                }
            }
        }
    }
    for msg in output_by_call.into_values().flatten() {
        ordered.push(msg);
    }
    ordered
}

fn is_tool_call_message(msg: &ChatMessage) -> bool {
    msg.blocks.iter().any(|b| b.kind == "tool_use")
}

fn is_tool_result_message(msg: &ChatMessage) -> bool {
    msg.blocks.iter().any(|b| b.kind == "tool_result")
}

/// 子代理派发工具名。命中这些工具的调用消息（及其配对结果）标记为子代理。
/// - Claude Code：`Agent`（新版）/ `Task`（旧版）；
/// - Codex：`multi_agent_v1` 命名空间的 `spawn_agent` / `wait_agent` / `close_agent`。
fn is_subagent_tool_name(name: &str) -> bool {
    matches!(
        name,
        "Agent" | "Task" | "spawn_agent" | "wait_agent" | "close_agent"
    )
}

/// 对话详情的收尾处理：先按 call_id 把工具结果排到调用之后，再标记子代理消息。
/// 两个详情解析器（Claude / Codex）共用此出口，保证行为一致。
fn finalize_conversation_messages(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut ordered = group_tool_results(messages);
    mark_subagent_messages(&mut ordered);
    ordered
}

/// 标记子代理相关消息，供前端 / 导出用独立开关控制显隐：
/// - 任何 sidechain 消息（Claude 旧版把子代理消息混在同一文件里）；
/// - 子代理派发工具的调用消息；
/// - 上述调用按 call_id 配对的结果消息。
/// 不改变消息顺序，只翻转 `is_subagent` 标记。
fn mark_subagent_messages(messages: &mut [ChatMessage]) {
    let mut subagent_call_ids: HashSet<String> = HashSet::new();
    for msg in messages.iter() {
        let is_subagent_call = msg.blocks.iter().any(|b| {
            b.kind == "tool_use" && b.tool_name.as_deref().is_some_and(is_subagent_tool_name)
        });
        if is_subagent_call {
            if let Some(call_id) = msg.call_id.as_deref() {
                subagent_call_ids.insert(call_id.to_string());
            }
        }
    }
    for msg in messages.iter_mut() {
        let hit = msg.is_sidechain
            || msg.blocks.iter().any(|b| {
                b.kind == "tool_use" && b.tool_name.as_deref().is_some_and(is_subagent_tool_name)
            })
            || msg
                .call_id
                .as_deref()
                .is_some_and(|c| subagent_call_ids.contains(c));
        if hit {
            msg.is_subagent = true;
        }
    }
}

fn message_text(msg: &ChatMessage) -> String {
    msg.blocks
        .iter()
        .filter(|b| b.kind == "text")
        .filter_map(|b| b.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn provider_session_id(agent: AgentKind, raw: &str) -> String {
    match agent {
        AgentKind::ClaudeCode => raw.to_string(),
        AgentKind::Codex => format!("codex:{raw}"),
    }
}

fn apply_codex_session_meta(payload: &serde_json::Value, meta: &mut CodexMeta) {
    if meta.session_id.is_none() {
        meta.session_id = payload
            .get("session_id")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("id").and_then(|v| v.as_str()))
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    if meta.project.is_none() {
        meta.project = payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    if meta.git_branch.is_none() {
        meta.git_branch = payload
            .get("git")
            .and_then(|v| v.get("branch"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    if meta.version.is_none() {
        meta.version = payload
            .get("cli_version")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    // source.subagent 标识本文件是父会话 spawn 出来的子代理，不作为独立会话索引
    if payload
        .get("source")
        .and_then(|v| v.get("subagent"))
        .is_some()
    {
        meta.is_subagent = true;
    }
}

fn apply_codex_turn_context(payload: &serde_json::Value, meta: &mut CodexMeta) {
    if meta.project.is_none() {
        meta.project = payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    if meta.model.is_none() {
        meta.model = payload
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
}

fn extract_codex_usage_entry(
    payload: &serde_json::Value,
    file_stem: &str,
    line_no: usize,
    ts: Option<i64>,
    meta: &CodexMeta,
    seen: &mut HashSet<String>,
) -> Option<UsageEntry> {
    let info = payload.get("info")?;
    let last = info.get("last_token_usage").filter(|v| v.is_object())?;
    let input = last
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .saturating_sub(
            last.get("cached_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        );
    let cache_read = last
        .get("cached_input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output = last
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if input == 0 && output == 0 && cache_read == 0 {
        return None;
    }
    let key = format!("codex:{file_stem}:token_count:{}", line_no + 1);
    if !seen.insert(key.clone()) {
        return None;
    }
    Some(UsageEntry {
        dedup_key: key,
        model: meta.model.clone().unwrap_or_else(|| "codex".to_string()),
        timestamp: ts.unwrap_or(0),
        input,
        output,
        cache_read,
        cache_creation: 0,
    })
}

fn codex_event_tool_result_to_message(
    payload: &serde_json::Value,
    stable_uuid: &str,
    ts: Option<i64>,
) -> Option<ChatMessage> {
    let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let call_id = payload.get("call_id").and_then(|v| v.as_str())?;
    let tool_name = match ptype {
        "web_search_end" => "web_search",
        _ => return None,
    };
    let text = codex_tool_output_text(payload);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(ChatMessage {
        uuid: stable_uuid.to_string(),
        role: "user".to_string(),
        phase: None,
        call_id: Some(call_id.to_string()),
        timestamp: ts.unwrap_or(0),
        is_sidechain: false,
        is_subagent: false,
        is_meta: false,
        meta_kind: None,
        attribution_skill: None,
        blocks: vec![ContentBlock {
            kind: "tool_result".to_string(),
            text: Some(truncate(text)),
            tool_name: Some(tool_name.to_string()),
            tool_input: None,
        }],
    })
}

fn codex_response_item_to_message(
    payload: &serde_json::Value,
    stable_uuid: &str,
    ts: Option<i64>,
    call_names: &mut HashMap<String, String>,
) -> Option<ChatMessage> {
    let ptype = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
    match ptype {
        "message" => {
            let role = payload
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("assistant");
            if role != "user" && role != "assistant" {
                return None;
            }
            let blocks = codex_message_content_blocks(payload.get("content"), role)?;
            Some(ChatMessage {
                uuid: stable_uuid.to_string(),
                role: role.to_string(),
                phase: payload
                    .get("phase")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                call_id: None,
                timestamp: ts.unwrap_or(0),
                is_sidechain: false,
                is_subagent: false,
                is_meta: false,
                meta_kind: None,
                attribution_skill: None,
                blocks,
            })
        }
        "reasoning" => {
            let blocks = codex_reasoning_blocks(payload);
            if blocks.is_empty() {
                return None;
            }
            Some(ChatMessage {
                uuid: stable_uuid.to_string(),
                role: "assistant".to_string(),
                phase: None,
                call_id: None,
                timestamp: ts.unwrap_or(0),
                is_sidechain: false,
                is_subagent: false,
                is_meta: false,
                meta_kind: None,
                attribution_skill: None,
                blocks,
            })
        }
        "function_call" | "custom_tool_call" | "web_search_call" | "tool_search_call" => {
            let call_id = payload
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or(stable_uuid);
            let name = codex_tool_name(payload, ptype);
            call_names.insert(call_id.to_string(), name.clone());
            let input = payload
                .get("arguments")
                .cloned()
                .or_else(|| payload.get("input").cloned())
                .or_else(|| payload.get("action").cloned());
            Some(ChatMessage {
                uuid: stable_uuid.to_string(),
                role: "assistant".to_string(),
                phase: None,
                call_id: Some(call_id.to_string()),
                timestamp: ts.unwrap_or(0),
                is_sidechain: false,
                is_subagent: false,
                is_meta: false,
                meta_kind: None,
                attribution_skill: None,
                blocks: vec![ContentBlock {
                    kind: "tool_use".to_string(),
                    text: None,
                    tool_name: Some(name),
                    tool_input: input.map(normalize_tool_input),
                }],
            })
        }
        "function_call_output"
        | "custom_tool_call_output"
        | "web_search_end"
        | "tool_search_output" => {
            let call_id = payload
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or(stable_uuid);
            let name = call_names.get(call_id).cloned();
            let text = codex_tool_output_text(payload);
            if text.trim().is_empty() {
                return None;
            }
            Some(ChatMessage {
                uuid: stable_uuid.to_string(),
                role: "user".to_string(),
                phase: None,
                call_id: Some(call_id.to_string()),
                timestamp: ts.unwrap_or(0),
                is_sidechain: false,
                is_subagent: false,
                is_meta: false,
                meta_kind: None,
                attribution_skill: None,
                blocks: vec![ContentBlock {
                    kind: "tool_result".to_string(),
                    text: Some(truncate(text.trim())),
                    tool_name: name,
                    tool_input: None,
                }],
            })
        }
        _ => None,
    }
}

fn codex_message_content_blocks(
    content: Option<&serde_json::Value>,
    role: &str,
) -> Option<Vec<ContentBlock>> {
    let arr = content?.as_array()?;
    let mut blocks = Vec::new();
    for b in arr {
        let bt = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match bt {
            "input_text" | "output_text" => {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    let t = prettify_display_text(t).trim().to_string();
                    if !t.is_empty() {
                        blocks.push(ContentBlock {
                            kind: "text".to_string(),
                            text: Some(truncate(&t)),
                            tool_name: None,
                            tool_input: None,
                        });
                    }
                }
            }
            "input_image" | "image" => blocks.push(ContentBlock {
                kind: "image".to_string(),
                text: Some("[图片]".to_string()),
                tool_name: None,
                tool_input: None,
            }),
            "summary_text" if role == "assistant" => {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    blocks.push(ContentBlock {
                        kind: "thinking".to_string(),
                        text: Some(truncate(t.trim())),
                        tool_name: None,
                        tool_input: None,
                    });
                }
            }
            _ => {}
        }
    }
    (!blocks.is_empty()).then_some(blocks)
}

fn codex_response_message_text(payload: &serde_json::Value) -> Option<String> {
    let blocks = codex_message_content_blocks(payload.get("content"), "user")?;
    let text = blocks
        .into_iter()
        .filter(|b| b.kind == "text" || b.kind == "image")
        .filter_map(|b| b.text)
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn codex_reasoning_blocks(payload: &serde_json::Value) -> Vec<ContentBlock> {
    let mut out = Vec::new();
    if let Some(arr) = payload.get("summary").and_then(|v| v.as_array()) {
        for item in arr {
            let text = item
                .get("text")
                .and_then(|v| v.as_str())
                .or_else(|| item.get("summary_text").and_then(|v| v.as_str()));
            if let Some(t) = text.map(str::trim).filter(|t| !t.is_empty()) {
                out.push(ContentBlock {
                    kind: "thinking".to_string(),
                    text: Some(truncate(t)),
                    tool_name: None,
                    tool_input: None,
                });
            }
        }
    }
    out
}

fn codex_tool_name(payload: &serde_json::Value, ptype: &str) -> String {
    payload
        .get("name")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("tool").and_then(|v| v.as_str()))
        .unwrap_or(match ptype {
            "web_search_call" => "web_search",
            "tool_search_call" => "tool_search",
            "custom_tool_call" => "custom_tool",
            _ => "tool",
        })
        .to_string()
}

fn normalize_tool_input(v: serde_json::Value) -> serde_json::Value {
    if let serde_json::Value::String(s) = &v {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s) {
            return parsed;
        }
    }
    v
}

fn codex_tool_output_text(payload: &serde_json::Value) -> String {
    for key in ["output", "message", "stdout", "stderr"] {
        if let Some(v) = payload.get(key) {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
            if !v.is_null() {
                return serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
            }
        }
    }
    serde_json::to_string_pretty(payload).unwrap_or_else(|_| payload.to_string())
}

fn codex_payload_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| payload.get("call_id").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn collect_codex_event_user_prompts(content: &str) -> Vec<CodexUserEvent> {
    let mut out = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parsed: CodexLine = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if parsed.line_type.as_deref() != Some("event_msg") {
            continue;
        }
        let payload = match parsed.payload.as_ref() {
            Some(p) => p,
            None => continue,
        };
        if payload.get("type").and_then(|v| v.as_str()) != Some("user_message") {
            continue;
        }
        let Some(text) = payload
            .get("message")
            .and_then(|v| v.as_str())
            .and_then(clean_prompt_text)
        else {
            continue;
        };
        let timestamp = parsed.timestamp.as_deref().and_then(iso_to_ms).unwrap_or(0);
        out.push(CodexUserEvent { timestamp, text });
    }
    out
}

fn has_near_codex_event_user(events: &[CodexUserEvent], ts: Option<i64>, text: &str) -> bool {
    let text = text.trim();
    let ts = ts.unwrap_or(0);
    events.iter().any(|e| {
        e.text.trim() == text && (e.timestamp == 0 || ts == 0 || e.timestamp.abs_diff(ts) <= 5_000)
    })
}

fn is_codex_context_injection(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("# AGENTS.md instructions")
        || is_codex_skill_injection(trimmed)
        || trimmed.starts_with("<environment_context>")
        || trimmed.starts_with("<permissions instructions>")
        || trimmed.starts_with("<app-context>")
        || trimmed.starts_with("<collaboration_mode>")
        || trimmed.starts_with("<skills_instructions>")
        || trimmed.starts_with("<plugins_instructions>")
        || trimmed.starts_with("<turn_aborted>")
        || trimmed.starts_with("<subagent_notification>")
        || trimmed.starts_with("[system]")
}

fn is_codex_skill_injection(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<skill>") && trimmed.contains("</skill>") && trimmed.contains("<name>")
}

fn skill_attachment_blocks(attachment: Option<&ConvAttachment>) -> Option<Vec<ContentBlock>> {
    let attachment = attachment?;
    let kind = attachment.attachment_type.as_deref().unwrap_or("");
    match kind {
        "skill_listing" => {
            let text = attachment_value_text(attachment.content.as_ref());
            if text.trim().is_empty() {
                return None;
            }
            Some(vec![ContentBlock {
                kind: "skill".to_string(),
                text: Some(truncate(text.trim())),
                tool_name: Some("skill_listing".to_string()),
                tool_input: None,
            }])
        }
        "invoked_skills" => {
            let mut sections = Vec::new();
            if let Some(skills) = &attachment.skills {
                for skill in skills {
                    let name = skill.name.as_deref().unwrap_or("skill");
                    let body = skill.content.as_deref().unwrap_or("").trim();
                    sections.push(if body.is_empty() {
                        name.to_string()
                    } else {
                        format!("{name}\n\n{body}")
                    });
                }
            }
            let text = sections.join("\n\n---\n\n");
            if text.trim().is_empty() {
                return None;
            }
            Some(vec![ContentBlock {
                kind: "skill".to_string(),
                text: Some(truncate(text.trim())),
                tool_name: Some("invoked_skills".to_string()),
                tool_input: None,
            }])
        }
        _ => None,
    }
}

fn attachment_value_text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

// ----------------------------- 文本处理 -----------------------------

fn classify_history_prompt_kind(display: &str) -> PromptKind {
    if looks_like_command_text(display) {
        PromptKind::Command
    } else {
        PromptKind::Human
    }
}

fn classify_conversation_prompt_kind(line: &ConvLine, content: &serde_json::Value) -> PromptKind {
    if line.is_sidechain.unwrap_or(false) {
        return PromptKind::Sidechain;
    }
    let origin_kind = line.origin.as_ref().and_then(|o| o.kind.as_deref());
    let prompt_source = line.prompt_source.as_deref();
    if line.is_meta.unwrap_or(false) {
        return PromptKind::Meta;
    }
    if matches!(origin_kind, Some("task-notification")) || matches!(prompt_source, Some("system")) {
        return PromptKind::System;
    }
    if matches!(origin_kind, Some("human")) {
        return PromptKind::Human;
    }

    if let Some(raw) = prompt_classification_text(content) {
        if is_meta_like_text(&raw) {
            return PromptKind::Meta;
        }
        if is_command_wrapper_text(&raw) || looks_like_command_text(&raw) {
            return PromptKind::Command;
        }
    }

    match prompt_source {
        Some("typed") => PromptKind::Human,
        Some("queued") => PromptKind::Queued,
        Some("sdk") => PromptKind::Sdk,
        Some(_) => PromptKind::Other,
        None => PromptKind::Human,
    }
}

fn prompt_classification_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(arr) => {
            let mut parts = Vec::new();
            for block in arr {
                let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match bt {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    "image" => parts.push("[图片]".to_string()),
                    _ => {}
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n"))
            }
        }
        _ => None,
    }
}

fn is_meta_like_text(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<local-command-stderr>")
        || trimmed.starts_with("<local-command-caveat>")
        || trimmed.starts_with("<system-reminder>")
}

fn is_command_wrapper_text(raw: &str) -> bool {
    command_wrapper_parts(raw).is_some()
}

fn looks_like_command_text(raw: &str) -> bool {
    let trimmed = raw.trim();
    if !trimmed.starts_with('/') {
        return false;
    }
    if is_path_like_slash_text(trimmed) {
        return false;
    }
    let cmd = trimmed[1..].split_whitespace().next().unwrap_or("").trim();
    !cmd.is_empty()
        && cmd
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.' | '+'))
}

fn is_path_like_slash_text(raw: &str) -> bool {
    let first_line = raw.trim().lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        return false;
    }
    if first_line.starts_with("/Users/")
        || first_line.starts_with("/home/")
        || first_line.starts_with("/nfs/")
        || first_line.starts_with("/tmp/")
        || first_line.starts_with("/var/")
        || first_line.starts_with("/opt/")
        || first_line.starts_with("/private/")
        || first_line.starts_with("/Applications/")
    {
        return true;
    }
    let token = first_line.split_whitespace().next().unwrap_or("");
    token.len() > 1 && token[1..].contains('/')
}

fn conversation_title_candidate(
    content: &serde_json::Value,
    kind: PromptKind,
    prompt_text: &str,
) -> Option<String> {
    if matches!(
        kind,
        PromptKind::Meta | PromptKind::System | PromptKind::Sidechain
    ) {
        return None;
    }
    let raw = prompt_classification_text(content).unwrap_or_default();
    if let Some((_name, args)) = command_wrapper_parts(&raw) {
        return non_empty_title(&normalize_image_placeholders(args.trim()));
    }
    if kind == PromptKind::Command {
        return slash_command_args_title(prompt_text);
    }
    non_empty_title(prompt_text)
}

fn slash_command_args_title(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !looks_like_command_text(trimmed) {
        return non_empty_title(trimmed);
    }
    let rest = trimmed
        .split_once(char::is_whitespace)
        .map(|(_, rest)| rest.trim())
        .unwrap_or("");
    non_empty_title(rest)
}

fn non_empty_title(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// 从 user 消息 content 提取可作为 prompt 的纯文本
fn extract_prompt_text(content: &serde_json::Value) -> Option<String> {
    let raw = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            let mut parts: Vec<String> = Vec::new();
            let mut saw_text = false;
            let mut saw_tool_result = false;
            for block in arr {
                let bt = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match bt {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            parts.push(t.to_string());
                            saw_text = true;
                        }
                    }
                    "image" => {
                        parts.push("[图片]".to_string());
                        saw_text = true;
                    }
                    "tool_result" => saw_tool_result = true,
                    _ => {}
                }
            }
            // 纯 tool_result 的 user 消息不是真正的 prompt
            if !saw_text && saw_tool_result {
                return None;
            }
            parts.join("\n")
        }
        _ => return None,
    };
    clean_prompt_text(&raw)
}

/// 清洗 prompt 文本：剥离系统提示 / 命令包裹标签，识别斜杠命令。
fn clean_prompt_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 本地命令的标准输出/错误输出，不是 prompt
    if trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<local-command-stderr>")
        || trimmed.starts_with("<bash-stdout>")
    {
        return None;
    }
    // 斜杠命令：<command-name>/foo</command-name>...
    // 自定义命令的参数在 <command-args> 里，要一并保留——
    // 否则与 history.jsonl 中「/foo 参数」形式的同一条 prompt 无法去重合并。
    if let Some((name, args)) = command_wrapper_parts(trimmed) {
        let n = name.trim();
        if !n.is_empty() {
            let args = args.trim();
            return Some(if args.is_empty() {
                n.to_string()
            } else {
                format!("{n} {args}")
            });
        }
    }
    // 去掉系统提示与命令相关包裹标签
    let mut s = strip_tag_blocks(trimmed, "system-reminder");
    s = strip_tag_blocks(&s, "local-command-caveat");
    s = strip_tag_blocks(&s, "command-stdout");
    let s = normalize_image_placeholders(s.trim());
    let s = s.trim();
    if s.is_empty()
        || s == "[Request interrupted by user]"
        || s == "[Request interrupted by user for tool use]"
    {
        return None;
    }
    Some(s.to_string())
}

/// 把粘贴图片留下的 `[Image: source: /长/缓存/路径.png]` 占位符压缩为 `[Image]`，
/// 避免 prompt 列表被一长串本地缓存路径刷屏。语言中立，不参与 i18n。
fn normalize_image_placeholders(s: &str) -> String {
    const OPEN: &str = "[Image: source:";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        match rest.find(OPEN) {
            Some(start) => match rest[start..].find(']') {
                Some(close_rel) => {
                    out.push_str(&rest[..start]);
                    out.push_str("[Image]");
                    rest = &rest[start + close_rel + 1..];
                }
                None => {
                    out.push_str(rest);
                    break;
                }
            },
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// 删除所有 `<tag ...>...</tag>` 区块
fn strip_tag_blocks(s: &str, tag: &str) -> String {
    let open_prefix = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::new();
    let mut rest = s;
    loop {
        match rest.find(&open_prefix) {
            Some(start) => match rest[start..].find(&close) {
                Some(close_rel) => {
                    out.push_str(&rest[..start]);
                    let after = start + close_rel + close.len();
                    rest = &rest[after..];
                }
                None => {
                    out.push_str(rest);
                    break;
                }
            },
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

/// 取出 open 与 close 标记之间的内容
fn extract_between(s: &str, open: &str, close: &str) -> Option<String> {
    let start = s.find(open)? + open.len();
    let rel_end = s[start..].find(close)?;
    Some(s[start..start + rel_end].to_string())
}

fn command_wrapper_parts(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    if !trimmed.contains("<command-name>") {
        return None;
    }
    let name = extract_between(trimmed, "<command-name>", "</command-name>")?;
    let mut rest = strip_tag_blocks(trimmed, "command-name");
    rest = strip_tag_blocks(&rest, "command-message");
    rest = strip_tag_blocks(&rest, "command-args");
    if !rest.trim().is_empty() {
        return None;
    }
    let args = extract_between(trimmed, "<command-args>", "</command-args>").unwrap_or_default();
    Some((name, args))
}

/// 对话详情展示用的文本美化：
/// - `<command-name>/foo</command-name>...<command-args>bar</command-args>` → "/foo bar"
/// - 剥离 local-command-caveat 包裹块
/// 仅影响展示（parse_conversation_detail 不参与缓存），不改变索引/搜索的数据。
fn prettify_display_text(s: &str) -> String {
    if let Some((name, args)) = command_wrapper_parts(s) {
        let n = name.trim();
        if !n.is_empty() {
            let args = args.trim();
            return if args.is_empty() {
                n.to_string()
            } else {
                format!("{n} {args}")
            };
        }
    }
    strip_tag_blocks(s, "local-command-caveat")
        .trim()
        .to_string()
}

/// 字符级截断
fn truncate(s: &str) -> String {
    if s.chars().count() > MAX_BLOCK_CHARS {
        let t: String = s.chars().take(MAX_BLOCK_CHARS).collect();
        format!("{t}\n…（内容过长，已截断）")
    } else {
        s.to_string()
    }
}

fn append_claude_messages(
    messages: &mut Vec<ChatMessage>,
    stable_uuid: &str,
    role: &str,
    timestamp: i64,
    is_sidechain: bool,
    is_meta: bool,
    attribution_skill: Option<String>,
    parsed_blocks: Vec<ParsedContentBlock>,
) {
    let mut text_blocks = Vec::new();
    let mut tool_idx = 0usize;
    let mut text_idx = 0usize;

    let mut flush_text = |messages: &mut Vec<ChatMessage>, text_blocks: &mut Vec<ContentBlock>| {
        if text_blocks.is_empty() {
            return;
        }
        let uuid = if text_idx == 0 {
            stable_uuid.to_string()
        } else {
            format!("{stable_uuid}:text:{text_idx}")
        };
        text_idx += 1;
        messages.push(ChatMessage {
            uuid,
            role: role.to_string(),
            phase: None,
            call_id: None,
            timestamp,
            is_sidechain,
            is_subagent: false,
            is_meta,
            meta_kind: None,
            attribution_skill: attribution_skill.clone(),
            blocks: std::mem::take(text_blocks),
        });
    };

    for parsed in parsed_blocks {
        if parsed.block.kind == "tool_use" || parsed.block.kind == "tool_result" {
            flush_text(messages, &mut text_blocks);
            tool_idx += 1;
            messages.push(ChatMessage {
                uuid: format!("{stable_uuid}:tool:{tool_idx}"),
                role: role.to_string(),
                phase: None,
                call_id: parsed.call_id,
                timestamp,
                is_sidechain,
                is_subagent: false,
                is_meta,
                meta_kind: None,
                attribution_skill: attribution_skill.clone(),
                blocks: vec![parsed.block],
            });
        } else {
            text_blocks.push(parsed.block);
        }
    }
    flush_text(messages, &mut text_blocks);
}

fn content_to_parsed_blocks(content: Option<&serde_json::Value>) -> Vec<ParsedContentBlock> {
    let mut blocks = Vec::new();
    match content {
        Some(serde_json::Value::String(s)) => {
            let t = prettify_display_text(s.trim());
            if !t.is_empty() {
                blocks.push(ParsedContentBlock {
                    block: ContentBlock {
                        kind: "text".to_string(),
                        text: Some(truncate(&t)),
                        tool_name: None,
                        tool_input: None,
                    },
                    call_id: None,
                });
            }
        }
        Some(serde_json::Value::Array(arr)) => {
            for b in arr {
                let bt = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match bt {
                    "text" => {
                        if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                            let t = prettify_display_text(t);
                            if t.is_empty() {
                                continue;
                            }
                            blocks.push(ParsedContentBlock {
                                block: ContentBlock {
                                    kind: "text".to_string(),
                                    text: Some(truncate(&t)),
                                    tool_name: None,
                                    tool_input: None,
                                },
                                call_id: None,
                            });
                        }
                    }
                    "thinking" => {
                        if let Some(t) = b.get("thinking").and_then(|v| v.as_str()) {
                            blocks.push(ParsedContentBlock {
                                block: ContentBlock {
                                    kind: "thinking".to_string(),
                                    text: Some(truncate(t)),
                                    tool_name: None,
                                    tool_input: None,
                                },
                                call_id: None,
                            });
                        }
                    }
                    "tool_use" => {
                        let name = b
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tool")
                            .to_string();
                        blocks.push(ParsedContentBlock {
                            block: ContentBlock {
                                kind: "tool_use".to_string(),
                                text: None,
                                tool_name: Some(name),
                                tool_input: b.get("input").cloned(),
                            },
                            call_id: b
                                .get("id")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                                .map(str::to_string),
                        });
                    }
                    "tool_result" => {
                        let txt = tool_result_text(b.get("content"));
                        blocks.push(ParsedContentBlock {
                            block: ContentBlock {
                                kind: "tool_result".to_string(),
                                text: Some(truncate(&txt)),
                                tool_name: None,
                                tool_input: None,
                            },
                            call_id: b
                                .get("tool_use_id")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                                .map(str::to_string),
                        });
                    }
                    "image" => {
                        blocks.push(ParsedContentBlock {
                            block: ContentBlock {
                                kind: "image".to_string(),
                                text: Some("[图片]".to_string()),
                                tool_name: None,
                                tool_input: None,
                            },
                            call_id: None,
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    blocks
}

/// 提取 tool_result 的可读文本
fn tool_result_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(arr)) => {
            let mut parts = Vec::new();
            for b in arr {
                let bt = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if bt == "text" {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        parts.push(t.to_string());
                    }
                } else if bt == "image" {
                    parts.push("[图片]".to_string());
                }
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------- clean_prompt_text ----------

    #[test]
    fn clean_extracts_slash_command() {
        let raw = "<command-name>/clear</command-name>\n<command-message>clear</command-message>\n<command-args></command-args>";
        assert_eq!(clean_prompt_text(raw), Some("/clear".to_string()));
    }

    #[test]
    fn clean_keeps_custom_command_args() {
        // 自定义命令带参数：命令名 + 参数拼接，与 history.jsonl 的形式一致
        let raw = "<command-name>/btw</command-name>\n<command-message>btw</command-message>\n<command-args>给我一小段话，总结一下项目进度</command-args>";
        assert_eq!(
            clean_prompt_text(raw),
            Some("/btw 给我一小段话，总结一下项目进度".to_string())
        );
    }

    #[test]
    fn embedded_command_wrapper_example_is_not_rewritten_as_command() {
        let raw = r#"{
  "message": {
    "content": "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"
  }
}
这三条有什么区别？"#;
        assert_eq!(clean_prompt_text(raw), Some(raw.to_string()));
        assert_eq!(prettify_display_text(raw), raw.to_string());
        assert_eq!(classify_history_prompt_kind(raw), PromptKind::Human);
    }

    #[test]
    fn clean_strips_system_reminder_keeps_text() {
        let raw = "前文<system-reminder>注入的噪音</system-reminder>后文";
        assert_eq!(clean_prompt_text(raw), Some("前文后文".to_string()));
    }

    #[test]
    fn clean_strips_local_command_caveat() {
        // 纯 caveat 包裹 → 不是 prompt
        let only = "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands. DO NOT respond to these messages.</local-command-caveat>";
        assert_eq!(clean_prompt_text(only), None);
        // caveat + 真实内容 → 只留真实内容
        let mixed = "<local-command-caveat>Caveat: ...</local-command-caveat>真正的问题";
        assert_eq!(clean_prompt_text(mixed), Some("真正的问题".to_string()));
    }

    #[test]
    fn image_placeholders_are_normalized() {
        let raw = "[Image: source: /Users/x/.claude/image-cache/abc/12.png]\n[Image: source: /Users/x/.claude/image-cache/abc/13.png]看一下这两张图";
        assert_eq!(
            clean_prompt_text(raw),
            Some("[Image]\n[Image]看一下这两张图".to_string())
        );
        // 不闭合的占位符原样保留，不破坏文本
        assert_eq!(
            normalize_image_placeholders("[Image: source: /broken"),
            "[Image: source: /broken"
        );
        // 普通文本不受影响
        assert_eq!(
            normalize_image_placeholders("hello [Image] world"),
            "hello [Image] world"
        );
    }

    #[test]
    fn clean_local_command_stdout_is_none() {
        assert_eq!(
            clean_prompt_text("<local-command-stdout>output</local-command-stdout>"),
            None
        );
    }

    #[test]
    fn clean_interrupted_is_none() {
        assert_eq!(clean_prompt_text("[Request interrupted by user]"), None);
        assert_eq!(
            clean_prompt_text("[Request interrupted by user for tool use]"),
            None
        );
    }

    #[test]
    fn clean_plain_text_passthrough() {
        assert_eq!(
            clean_prompt_text("  帮我修复这个 bug  "),
            Some("帮我修复这个 bug".to_string())
        );
    }

    #[test]
    fn codex_session_uses_event_user_message_and_parses_detail() {
        let dir =
            std::env::temp_dir().join(format!("cc_history_viewer_codex_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rollout-2026-07-04T00-00-00-abc.jsonl");
        let jsonl = [
            json!({
                "timestamp": "2026-07-04T00:00:00.000Z",
                "type": "session_meta",
                "payload": {
                    "session_id": "abc",
                    "cwd": "/tmp/proj",
                    "cli_version": "0.142.5",
                    "git": {"branch": "main"}
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:00.100Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "<environment_context>noise</environment_context>"}]
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "真实问题"}]
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:01.000Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "真实问题", "images": []}
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": "msg1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "回答"}],
                    "phase": "final_answer"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.500Z",
                "type": "compacted",
                "payload": {
                    "message": "Another language model started to solve this problem. Here is the summary produced by the other language model:\n回答"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.600Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": "summary1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "**Handoff Summary**\n- User request: x"}],
                    "phase": "final_answer"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.700Z",
                "type": "compacted",
                "payload": {
                    "message": "**Handoff Summary**\n- User request: x"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:03.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "id": "fc1",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"pwd\"}",
                    "call_id": "call1"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:03.100Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "id": "fc2",
                    "name": "exec_command",
                    "arguments": "{\"cmd\":\"ls\"}",
                    "call_id": "call2"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:04.000Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call1",
                    "output": "ok"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:04.100Z",
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call2",
                    "output": "ls ok"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:04.200Z",
                "type": "response_item",
                "payload": {
                    "type": "web_search_call",
                    "id": "ws1",
                    "call_id": "web1",
                    "action": {"type": "search", "query": "Claude Code history"}
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:04.300Z",
                "type": "event_msg",
                "payload": {
                    "type": "web_search_end",
                    "call_id": "web1",
                    "query": "Claude Code history"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:04.350Z",
                "type": "response_item",
                "payload": {
                    "type": "web_search_end",
                    "call_id": "web1",
                    "query": "Claude Code history"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:04.500Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": "msg2",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "真实最终回答"}],
                    "phase": "final_answer"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:05.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 10,
                            "cached_input_tokens": 4,
                            "output_tokens": 2
                        },
                        "total_token_usage": {
                            "input_tokens": 10,
                            "cached_input_tokens": 4,
                            "output_tokens": 2
                        }
                    }
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_codex_session_file(&path).unwrap();
        assert_eq!(parsed.session_id, "codex:abc");
        assert_eq!(parsed.agent, AgentKind::Codex);
        assert_eq!(parsed.project.as_deref(), Some("/tmp/proj"));
        assert_eq!(parsed.git_branch.as_deref(), Some("main"));
        assert_eq!(parsed.first_prompt, "真实问题");
        assert_eq!(parsed.message_count, 3);
        assert_eq!(parsed.user_prompts.len(), 1);
        assert_eq!(parsed.user_prompts[0].text, "真实问题");
        assert_eq!(parsed.usage_entries.len(), 1);
        assert_eq!(parsed.usage_entries[0].input, 6);
        assert_eq!(parsed.usage_entries[0].cache_read, 4);
        assert_eq!(parsed.usage_entries[0].output, 2);

        let detail = parse_codex_conversation_detail(&path).unwrap();
        assert_eq!(detail.session_id, "codex:abc");
        assert_eq!(detail.agent, AgentKind::Codex);
        assert_eq!(
            detail
                .messages
                .iter()
                .filter(|m| m.role == "user" && m.blocks.iter().any(|b| b.kind == "text"))
                .count(),
            1
        );
        assert!(detail.messages.iter().any(|m| m.role == "assistant"
            && m.phase.as_deref() == Some("final_answer")
            && m.blocks.iter().any(|b| b.text.as_deref() == Some("回答"))));
        assert!(detail.messages.iter().any(|m| m.role == "assistant"
            && m.phase.as_deref() == Some("final_answer")
            && m.blocks
                .iter()
                .any(|b| b.text.as_deref() == Some("真实最终回答"))));
        assert!(
            !detail.messages.iter().any(|m| m.blocks.iter().any(|b| b
                .text
                .as_deref()
                .is_some_and(|t| t.starts_with("**Handoff Summary**")))),
            "compaction 生成的 handoff 摘要不应作为最终回答展示"
        );
        assert!(detail
            .messages
            .iter()
            .any(|m| m.blocks.iter().any(|b| b.kind == "tool_use")));
        assert!(detail
            .messages
            .iter()
            .any(|m| m.blocks.iter().any(|b| b.kind == "tool_result")));
        let tool_call_ids: Vec<Option<&str>> = detail
            .messages
            .iter()
            .filter(|m| is_tool_call_message(m) || is_tool_result_message(m))
            .map(|m| m.call_id.as_deref())
            .collect();
        assert_eq!(
            tool_call_ids,
            vec![
                Some("call1"),
                Some("call1"),
                Some("call2"),
                Some("call2"),
                Some("web1"),
                Some("web1")
            ]
        );
        assert_eq!(
            detail
                .messages
                .iter()
                .filter(|m| m.call_id.as_deref() == Some("web1")
                    && m.blocks.iter().any(|b| b.kind == "tool_result"))
                .count(),
            1,
            "event_msg 与 response_item 同时出现 web_search_end 时应去重"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn codex_fallback_user_counts_and_usage_requires_last_token_usage() {
        let dir = std::env::temp_dir().join(format!(
            "cc_history_viewer_codex_fallback_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("fallback.jsonl");
        let jsonl = [
            json!({
                "timestamp": "2026-07-04T00:00:00.000Z",
                "type": "session_meta",
                "payload": {"session_id": "fallback", "cwd": "/tmp/proj"}
            }),
            json!({
                "timestamp": "2026-07-04T00:00:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "fallback prompt"}]
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "answer"}],
                    "phase": "final_answer"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:03.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": 10,
                            "output_tokens": 20
                        }
                    }
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_codex_session_file(&path).unwrap();
        assert_eq!(parsed.user_prompts.len(), 1);
        assert_eq!(parsed.message_count, 2);
        assert!(parsed.usage_entries.is_empty());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn codex_timestampless_event_user_is_indexed_and_rendered() {
        let dir = std::env::temp_dir().join(format!(
            "cc_history_viewer_codex_no_ts_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("no-ts.jsonl");
        let jsonl = [
            json!({
                "timestamp": "2026-07-04T00:00:00.000Z",
                "type": "session_meta",
                "payload": {"session_id": "no-ts", "cwd": "/tmp/proj"}
            }),
            json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "无时间戳问题", "images": []}
            }),
            json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "无时间戳问题"}]
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_codex_session_file(&path).unwrap();
        assert_eq!(parsed.user_prompts.len(), 1);
        assert_eq!(parsed.user_prompts[0].timestamp, 0);
        assert_eq!(parsed.first_prompt, "无时间戳问题");
        assert_eq!(parsed.message_count, 1);

        let detail = parse_codex_conversation_detail(&path).unwrap();
        assert_eq!(
            detail
                .messages
                .iter()
                .filter(|m| m.role == "user" && m.blocks.iter().any(|b| b.kind == "text"))
                .count(),
            1
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn codex_fallback_preserves_user_angle_tag_prompts() {
        let dir = std::env::temp_dir().join(format!(
            "cc_history_viewer_codex_angle_tag_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("angle-tag.jsonl");
        let jsonl = [
            json!({
                "timestamp": "2026-07-04T00:00:00.000Z",
                "type": "session_meta",
                "payload": {"session_id": "angle-tag", "cwd": "/tmp/proj"}
            }),
            json!({
                "timestamp": "2026-07-04T00:00:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "<my_tag>\n请分析这个占位符"}]
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_codex_session_file(&path).unwrap();
        assert_eq!(parsed.user_prompts.len(), 1);
        assert_eq!(parsed.first_prompt, "<my_tag>\n请分析这个占位符");
        assert_eq!(parsed.message_count, 1);

        let detail = parse_codex_conversation_detail(&path).unwrap();
        assert!(detail.messages.iter().any(|m| {
            m.role == "user"
                && m.blocks
                    .iter()
                    .any(|b| b.text.as_deref() == Some("<my_tag>\n请分析这个占位符"))
        }));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn codex_preserves_embedded_command_wrapper_examples() {
        let dir = std::env::temp_dir().join(format!(
            "cc_history_viewer_codex_embedded_command_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("embedded-command.jsonl");
        let user_text = "{\n  \"message\": {\n    \"content\": \"<command-name>/model</command-name>\\n<command-message>model</command-message>\\n<command-args></command-args>\"\n  }\n}\n这三个有什么区别？";
        let answer_text = "这三条的本质区别不是 `type=user`。\n\n```xml\n<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>\n```";
        let jsonl = [
            json!({
                "timestamp": "2026-07-05T14:06:28.039Z",
                "type": "session_meta",
                "payload": {"session_id": "embedded-command", "cwd": "/tmp/proj"}
            }),
            json!({
                "timestamp": "2026-07-05T14:06:28.557Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": user_text}]
                }
            }),
            json!({
                "timestamp": "2026-07-05T14:06:28.558Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": user_text, "images": []}
            }),
            json!({
                "timestamp": "2026-07-05T14:07:43.435Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": "answer",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": answer_text}],
                    "phase": "final_answer"
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_codex_session_file(&path).unwrap();
        assert_eq!(parsed.user_prompts.len(), 1);
        assert_eq!(parsed.first_prompt, user_text);
        assert_ne!(parsed.first_prompt, "/model");

        let detail = parse_codex_conversation_detail(&path).unwrap();
        assert!(detail.messages.iter().any(|m| {
            m.role == "user"
                && m.blocks
                    .iter()
                    .any(|b| b.text.as_deref() == Some(user_text))
        }));
        assert!(detail.messages.iter().any(|m| {
            m.role == "assistant"
                && m.blocks
                    .iter()
                    .any(|b| b.text.as_deref() == Some(answer_text))
        }));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn codex_skill_injection_and_compaction_summary_are_metadata() {
        let dir = std::env::temp_dir().join(format!(
            "cc_history_viewer_codex_meta_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("meta.jsonl");
        let compacted_summary = "We are in `/tmp/proj`. User asked in Chinese: “再次进行深度review”. Need continue from current state.";
        let checkpoint_summary = "**Checkpoint Summary**\n\n**Progress / Decisions**\n- done";
        let jsonl = [
            json!({
                "timestamp": "2026-07-04T00:00:00.000Z",
                "type": "session_meta",
                "payload": {"session_id": "meta", "cwd": "/tmp/proj"}
            }),
            json!({
                "timestamp": "2026-07-04T00:00:00.100Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "<skill>\n<name>requesting-code-review</name>\n<path>/tmp/SKILL.md</path>\n---\n# Skill body\n</skill>"}]
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:01.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "真实问题"}]
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:01.000Z",
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "真实问题", "images": []}
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": "summary1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": compacted_summary}],
                    "phase": "final_answer"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.100Z",
                "type": "compacted",
                "payload": {
                    "message": format!("Another language model started to solve this problem.\n{compacted_summary}")
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.200Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": "summary2",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": checkpoint_summary}],
                    "phase": "final_answer"
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:02.300Z",
                "type": "compacted",
                "payload": {
                    "message": format!("Another language model started to solve this problem.\n{checkpoint_summary}")
                }
            }),
            json!({
                "timestamp": "2026-07-04T00:00:03.000Z",
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "id": "answer",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "真实回答"}],
                    "phase": "final_answer"
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_codex_session_file(&path).unwrap();
        assert_eq!(parsed.user_prompts.len(), 1);
        assert_eq!(parsed.first_prompt, "真实问题");
        assert_eq!(parsed.message_count, 2);

        let detail = parse_codex_conversation_detail(&path).unwrap();
        assert_eq!(
            detail
                .messages
                .iter()
                .filter(|m| m.role == "user" && m.blocks.iter().any(|b| b.kind == "text"))
                .count(),
            1
        );
        assert!(detail.messages.iter().any(|m| {
            m.role == "system"
                && m.meta_kind.as_deref() == Some("skill")
                && m.blocks.iter().any(|b| b.kind == "skill")
        }));
        assert!(
            !detail
                .messages
                .iter()
                .any(|m| message_text(m).contains("User asked in Chinese")),
            "compaction summary should not be shown as a final answer"
        );
        assert!(
            !detail
                .messages
                .iter()
                .any(|m| message_text(m).starts_with("**Checkpoint Summary**")),
            "checkpoint summary should not be shown as a final answer"
        );
        assert!(detail.messages.iter().any(|m| {
            m.role == "assistant"
                && m.phase.as_deref() == Some("final_answer")
                && m.blocks
                    .iter()
                    .any(|b| b.text.as_deref() == Some("真实回答"))
        }));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn classify_history_slash_path_as_human() {
        assert_eq!(
            classify_history_prompt_kind("/Users/didi/Documents/codes/project"),
            PromptKind::Human
        );
        assert_eq!(classify_history_prompt_kind("/model"), PromptKind::Command);
    }

    #[test]
    fn classify_origin_human_wins_over_command_wrapper() {
        let content = json!("<command-name>/model</command-name>");
        let line = ConvLine {
            line_type: Some("user".to_string()),
            uuid: None,
            parent_uuid: None,
            prompt_id: None,
            timestamp: None,
            cwd: None,
            git_branch: None,
            version: None,
            is_sidechain: None,
            is_meta: None,
            origin: Some(ConvOrigin {
                kind: Some("human".to_string()),
            }),
            prompt_source: None,
            request_id: None,
            subtype: None,
            content: Some(content.clone()),
            message: Some(ConvMessage {
                role: Some("user".to_string()),
                content: Some(content),
                id: None,
                model: None,
                usage: None,
            }),
            attachment: None,
            attribution_skill: None,
        };
        assert_eq!(
            classify_conversation_prompt_kind(&line, line.content.as_ref().unwrap()),
            PromptKind::Human
        );
    }

    #[test]
    fn classify_array_command_wrapper_as_command() {
        let content = json!([
            {
                "type": "text",
                "text": "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"
            }
        ]);
        let line = ConvLine {
            line_type: Some("user".to_string()),
            uuid: None,
            parent_uuid: None,
            prompt_id: None,
            timestamp: None,
            cwd: None,
            git_branch: None,
            version: None,
            is_sidechain: None,
            is_meta: None,
            origin: None,
            prompt_source: Some("typed".to_string()),
            request_id: None,
            subtype: None,
            content: Some(content.clone()),
            message: Some(ConvMessage {
                role: Some("user".to_string()),
                content: Some(content),
                id: None,
                model: None,
                usage: None,
            }),
            attachment: None,
            attribution_skill: None,
        };
        assert_eq!(
            classify_conversation_prompt_kind(&line, line.content.as_ref().unwrap()),
            PromptKind::Command
        );
    }

    #[test]
    fn claude_session_title_skips_control_command_and_uses_command_args() {
        let path = std::env::temp_dir().join(format!(
            "cc_history_viewer_claude_title_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let real_prompt = "对当前代码仓库进行一次深度的review\n主要是检查解析逻辑是否全面且准确";
        let jsonl = [
            json!({
                "type": "user",
                "uuid": "model-command",
                "timestamp": "2026-07-05T00:28:56.723Z",
                "cwd": "/tmp/project",
                "message": {
                    "role": "user",
                    "content": "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args></command-args>"
                }
            }),
            json!({
                "type": "user",
                "uuid": "model-output",
                "timestamp": "2026-07-05T00:28:56.723Z",
                "cwd": "/tmp/project",
                "message": {
                    "role": "user",
                    "content": "<local-command-stdout>Set model to dpsk-model and saved as your default for new sessions</local-command-stdout>"
                }
            }),
            json!({
                "type": "user",
                "uuid": "review-command",
                "timestamp": "2026-07-05T00:29:12.043Z",
                "cwd": "/tmp/project",
                "message": {
                    "role": "user",
                    "content": format!("<command-message>superpowers:requesting-code-review</command-message>\n<command-name>/superpowers:requesting-code-review</command-name>\n<command-args>{real_prompt}</command-args>")
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_conversation_file(&path).expect("fixture 应能解析");
        assert_eq!(parsed.first_prompt, real_prompt);
        assert_eq!(parsed.message_count, 2);
        assert_eq!(parsed.user_prompts.len(), 2);
        assert_eq!(parsed.user_prompts[0].text, "/model");
        assert_eq!(
            parsed.user_prompts[1].text,
            format!("/superpowers:requesting-code-review {real_prompt}")
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn claude_replay_user_prompt_is_removed_from_summary_and_detail() {
        let path = std::env::temp_dir().join(format!(
            "cc_history_viewer_claude_replay_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let real_prompt = "对当前代码仓库进行一次深度的review";
        let wrapped = format!(
            "<command-message>superpowers:requesting-code-review</command-message>\n<command-name>/superpowers:requesting-code-review</command-name>\n<command-args>{real_prompt}</command-args>"
        );
        let jsonl = [
            json!({
                "type": "user",
                "uuid": "first-replay",
                "parentUuid": "same-parent",
                "promptId": "prompt-preflight",
                "timestamp": "2026-07-05T01:30:48.409Z",
                "cwd": "/tmp/project",
                "message": {"role": "user", "content": wrapped}
            }),
            json!({
                "type": "user",
                "uuid": "skill-meta",
                "parentUuid": "first-replay",
                "promptId": "prompt-preflight",
                "timestamp": "2026-07-05T01:30:49.000Z",
                "isMeta": true,
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "Base directory for this skill: /tmp/skill\n\n# Skill body"}]
                }
            }),
            json!({
                "type": "attachment",
                "uuid": "skill-listing",
                "parentUuid": "skill-meta",
                "timestamp": "2026-07-05T01:30:50.000Z",
                "attachment": {"type": "skill_listing", "content": "available skills"}
            }),
            json!({
                "type": "last-prompt",
                "leafUuid": "first-replay",
                "sessionId": "session"
            }),
            json!({
                "type": "user",
                "uuid": "actual-prompt",
                "parentUuid": "same-parent",
                "promptId": "prompt-actual",
                "timestamp": "2026-07-05T01:31:00.000Z",
                "cwd": "/tmp/project",
                "message": {"role": "user", "content": wrapped}
            }),
            json!({
                "type": "assistant",
                "uuid": "answer",
                "parentUuid": "actual-prompt",
                "timestamp": "2026-07-05T01:31:01.000Z",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "真实回答"}]
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_conversation_file(&path).expect("fixture 应能解析");
        assert_eq!(parsed.first_prompt, real_prompt);
        assert_eq!(parsed.message_count, 2);
        assert_eq!(
            parsed
                .user_prompts
                .iter()
                .filter(|p| p.text == format!("/superpowers:requesting-code-review {real_prompt}"))
                .count(),
            1
        );

        let detail = parse_conversation_detail(&path).expect("fixture 应能解析");
        let expected_prompt = format!("/superpowers:requesting-code-review {real_prompt}");
        assert_eq!(
            detail
                .messages
                .iter()
                .filter(|m| {
                    m.role == "user"
                        && !m.is_meta
                        && m.blocks
                            .iter()
                            .any(|b| b.text.as_deref() == Some(expected_prompt.as_str()))
                })
                .count(),
            1
        );
        assert!(detail.messages.iter().any(|m| m.uuid == "skill-meta"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn claude_repeated_real_user_prompt_is_preserved_after_assistant_output() {
        let path = std::env::temp_dir().join(format!(
            "cc_history_viewer_claude_real_repeat_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let jsonl = [
            json!({
                "type": "user",
                "uuid": "u1",
                "parentUuid": "root",
                "promptId": "p1",
                "timestamp": "2026-07-05T01:00:00.000Z",
                "cwd": "/tmp/project",
                "message": {"role": "user", "content": "重复问题"}
            }),
            json!({
                "type": "assistant",
                "uuid": "a1",
                "parentUuid": "u1",
                "timestamp": "2026-07-05T01:00:01.000Z",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "第一次回答"}]
                }
            }),
            json!({
                "type": "user",
                "uuid": "u2",
                "parentUuid": "u1",
                "promptId": "p2",
                "timestamp": "2026-07-05T01:00:02.000Z",
                "cwd": "/tmp/project",
                "message": {"role": "user", "content": "重复问题"}
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_conversation_file(&path).expect("fixture 应能解析");
        assert_eq!(
            parsed
                .user_prompts
                .iter()
                .filter(|p| p.text == "重复问题")
                .count(),
            2
        );
        assert_eq!(parsed.message_count, 3);

        let detail = parse_conversation_detail(&path).expect("fixture 应能解析");
        assert_eq!(
            detail
                .messages
                .iter()
                .filter(|m| m.role == "user"
                    && m.blocks
                        .iter()
                        .any(|b| b.text.as_deref() == Some("重复问题")))
                .count(),
            2
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_conversation_detail_groups_tool_results_after_matching_calls() {
        let path = std::env::temp_dir().join(format!(
            "cc_history_viewer_tool_grouping_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let jsonl = [
            json!({
                "type": "assistant",
                "uuid": "assistant-tools",
                "timestamp": "2026-07-04T00:00:00.000Z",
                "cwd": "/tmp/project",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": "先查目录"},
                        {"type": "tool_use", "id": "tool-a", "name": "Bash", "input": {"command": "pwd"}},
                        {"type": "tool_use", "id": "tool-b", "name": "Bash", "input": {"command": "ls"}}
                    ]
                }
            }),
            json!({
                "type": "user",
                "uuid": "result-a",
                "timestamp": "2026-07-04T00:00:02.000Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tool-a", "content": "/tmp/project"}]
                }
            }),
            json!({
                "type": "user",
                "uuid": "result-b",
                "timestamp": "2026-07-04T00:00:03.000Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tool-b", "content": "src"}]
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let detail = parse_conversation_detail(&path).expect("fixture 应能解析");
        assert!(detail.messages.iter().any(|m| m.uuid == "assistant-tools"
            && m.call_id.is_none()
            && m.blocks
                .iter()
                .any(|b| b.text.as_deref() == Some("先查目录"))));
        let tool_call_ids: Vec<Option<&str>> = detail
            .messages
            .iter()
            .filter(|m| is_tool_call_message(m) || is_tool_result_message(m))
            .map(|m| m.call_id.as_deref())
            .collect();
        assert_eq!(
            tool_call_ids,
            vec![
                Some("tool-a"),
                Some("tool-a"),
                Some("tool-b"),
                Some("tool-b")
            ]
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn claude_message_count_excludes_tool_carrier_rows() {
        let path = std::env::temp_dir().join(format!(
            "cc_history_viewer_message_count_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let jsonl = [
            json!({
                "type": "user",
                "uuid": "u-1",
                "timestamp": "2026-07-04T00:00:00.000Z",
                "cwd": "/tmp/project",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "真实问题"}]
                }
            }),
            json!({
                "type": "assistant",
                "uuid": "a-tool",
                "timestamp": "2026-07-04T00:00:01.000Z",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "tool_use", "id": "tool-a", "name": "Bash", "input": {"command": "pwd"}}
                    ]
                }
            }),
            json!({
                "type": "user",
                "uuid": "result-a",
                "timestamp": "2026-07-04T00:00:02.000Z",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "tool-a", "content": "/tmp/project"}]
                }
            }),
            json!({
                "type": "assistant",
                "uuid": "a-answer",
                "timestamp": "2026-07-04T00:00:03.000Z",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "真实回答"}]
                }
            }),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&path, jsonl).unwrap();

        let parsed = parse_conversation_file(&path).expect("fixture 应能解析");
        assert_eq!(parsed.message_count, 2);
        assert_eq!(parsed.user_prompts.len(), 1);
        assert_eq!(parsed.first_prompt, "真实问题");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn parse_conversation_detail_keeps_attribution_skill_as_metadata_only() {
        let path = std::env::temp_dir().join(format!(
            "cc_history_viewer_skill_test_{}_{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let jsonl = r#"{"type":"assistant","uuid":"a-1","timestamp":"2026-05-16T02:00:00.000Z","message":{"role":"assistant","content":[{"type":"text","text":"处理完成"}]},"attributionSkill":"superpowers:requesting-code-review"}"#;
        std::fs::write(&path, jsonl).unwrap();

        let detail = parse_conversation_detail(&path).expect("fixture 应能解析");
        let msg = detail
            .messages
            .iter()
            .find(|m| m.uuid == "a-1")
            .expect("应保留原始 assistant 消息");
        assert_eq!(
            msg.attribution_skill.as_deref(),
            Some("superpowers:requesting-code-review")
        );
        assert_eq!(
            msg.blocks
                .iter()
                .map(|b| b.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["text"]
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn clean_empty_after_strip_is_none() {
        assert_eq!(
            clean_prompt_text("<system-reminder>只有噪音</system-reminder>"),
            None
        );
        assert_eq!(clean_prompt_text("   "), None);
    }

    // ---------- strip_tag_blocks ----------

    #[test]
    fn strip_unclosed_tag_kept_as_is() {
        let s = "a<system-reminder>没有闭合";
        assert_eq!(strip_tag_blocks(s, "system-reminder"), s);
    }

    #[test]
    fn strip_multiple_blocks() {
        assert_eq!(strip_tag_blocks("a<t>1</t>b<t>2</t>c", "t"), "abc");
    }

    #[test]
    fn strip_tag_with_attributes() {
        let s = r#"x<system-reminder foo="x">hidden</system-reminder>y"#;
        assert_eq!(strip_tag_blocks(s, "system-reminder"), "xy");
    }

    // ---------- extract_prompt_text ----------

    #[test]
    fn extract_from_plain_string() {
        assert_eq!(
            extract_prompt_text(&json!("你好")),
            Some("你好".to_string())
        );
    }

    #[test]
    fn extract_array_text_with_tool_result_keeps_text() {
        let v = json!([
            {"type": "text", "text": "保留这段"},
            {"type": "tool_result", "tool_use_id": "t1", "content": "工具输出"}
        ]);
        assert_eq!(extract_prompt_text(&v), Some("保留这段".to_string()));
    }

    #[test]
    fn extract_pure_tool_result_is_none() {
        let v = json!([
            {"type": "tool_result", "tool_use_id": "t1", "content": "工具输出"}
        ]);
        assert_eq!(extract_prompt_text(&v), None);
    }

    #[test]
    fn extract_image_becomes_placeholder() {
        let v = json!([{"type": "image", "source": {"type": "base64"}}]);
        assert_eq!(extract_prompt_text(&v), Some("[图片]".to_string()));
    }

    // ---------- iso_to_ms ----------

    #[test]
    fn iso_to_ms_basic() {
        assert_eq!(iso_to_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(iso_to_ms("1970-01-01T00:00:00.250Z"), Some(250));
        // 带时区偏移：当地 08:00 即 UTC 0 点
        assert_eq!(iso_to_ms("1970-01-01T08:00:00+08:00"), Some(0));
        assert_eq!(iso_to_ms("not-a-date"), None);
    }
}
