# Fix Report - Conversation Parser Review Fixes

## Bug 描述

一次整体 review 发现 Claude Code / Codex 会话解析、统计和展示存在多处边界缺陷：token 用量项目归属可能不稳定，Codex fallback 模式消息计数偏小，Claude Code 会把工具调用/结果载体行计入会话消息数，缺失 `last_token_usage` 时可能把累计 token 当作单轮增量，Codex commentary 无法独立导出，对话跳转到隐藏消息时静默失败，以及首启索引构建持锁阻塞读命令。

## 根因

- 索引阶段从 `HashMap::into_values()` 直接构造会话列表，后续 usage 去重采用“第一条出现者”归属项目，导致跨文件重复 usage 的项目归属依赖非稳定迭代顺序。
- Codex `event_msg.user_message` 和 `response_item.message(role=user)` 两种模式只在 prompt 提取上做了 fallback，`message_count` 没有同样 fallback。
- Claude Code 原先按 JSONL `user`/`assistant` 行数统计 `message_count`，而工具结果也以 `user` 行保存、工具调用也以 `assistant` 行保存，导致与 Codex 的“用户轮次 + 助手回答”口径不可比。
- Codex `token_count` 中 `last_token_usage` 和 `total_token_usage` 语义不同，缺失单轮增量时回退累计值会膨胀统计。
- 对话页导出选择依赖当前可见消息，Codex commentary 默认隐藏时没有独立导出开关。
- 对话跳转使用全量消息索引，但 DOM 只渲染过滤后的消息。
- 跳转高亮淡出定时器和跳转/放宽过滤逻辑放在同一个 effect 中，过滤开关变化会清理旧定时器，但已跳转守卫又会阻止新定时器注册。
- `ensure_index` 在持有全局索引锁时执行全量构建，使所有读命令排队等待。

## 尝试记录

- 验证真实 Codex 数据：扫描 `/Users/didi/.codex/sessions`，确认存在缺失 `last_token_usage` 的 `token_count`，且 `web_search_end` 当前主要来自 `event_msg`。
- 运行现有测试和类型检查，确认旧测试未覆盖这些边界。
- 增加针对 fallback 计数、Claude 工具载体行不计数、usage 缺失、compaction 摘要、web search 去重、Codex commentary 导出和 usage 稳定归属的单元测试。

## 最终方案

- 索引组装时按路径和 session id 稳定排序；usage/project/top 列表排序增加 tie-breaker。
- Codex fallback 模式补 user message count；缺失 `last_token_usage` 的 token_count 直接跳过。
- Claude Code `message_count` 改为只统计真实 user prompt 和含可见回答内容的 assistant message，不再统计纯 tool_use/tool_result 载体行；缓存版本升到 11，避免复用旧口径缓存。
- Codex compaction 删除逻辑只删除看起来像 handoff summary 且被 compacted payload 覆盖的候选消息，避免误删普通 final answer。
- event/response 双路径 tool result 增加 `call_id + tool + text` 去重。
- 对话导出增加 `includeCodexCommentary`，导出标签区分 Codex commentary/final；消息选择从 uuid 改为完整消息数组下标，避免 Codex commentary/final 共用同一 payload id 时误导出。
- 对话跳转目标被过滤时自动放宽相关显示选项；切换页面时清理旧高亮；高亮淡出拆成独立 effect，避免过滤开关变化导致高亮不消失。
- `ensure_index` 改为锁外构建、锁内替换；导出写文件改为 `create_new` 原子创建。
- 缓存版本预检改为反序列化只含 `version` 的小结构，不再依赖 serde 字段输出顺序。

## 验证

- `cargo test --manifest-path src-tauri/Cargo.toml`
- `pnpm -s exec tsc --noEmit`
- `pnpm -s build`
- `git diff --check`

## 经验教训

多来源历史解析里，“去重”和“归属”必须同时定义稳定语义；只验证总量正确不足以保证分组统计稳定。前端过滤和导出也应分离视图开关与导出意图，否则默认隐藏内容会变成不可导出内容。

# Fix Report - Claude Code Replay Prompt Dedup

## Bug 描述

Claude Code 在加载 skill / hooks / context 时，可能先写入一条用户命令 prompt，随后插入 skill 内容、attachment、`last-prompt` 等上下文，再写入同父节点、同文本但不同 `promptId` 的正式用户 prompt。详情页会把两条相同 prompt 连续展示，会话摘要和消息数也会把预派发记录算进去。

## 根因

原解析逻辑把每条 `type=user` 且可提取文本的 JSONL 行都当作独立用户输入。Claude Code 本地 transcript 不是纯聊天流，其中包含 preflight/replay 记录；是否为真实模型请求入口需要结合 `parentUuid`、`promptId`、清洗后的文本以及后续是否已经出现 assistant 输出判断。

## 最终方案

- `ConvLine` 读取 `parentUuid` / `promptId`。
- 文件级摘要解析中，如果前一条普通用户 prompt 尚未触发 assistant 输出，且后一条与其同 `parentUuid`、同清洗文本、不同 `promptId`，则移除前一条 replay prompt 并回退消息数。
- 详情解析中使用同样判定删除前一条普通用户消息，但保留中间的 meta / skill / attachment 消息，继续受现有元信息开关控制。
- 缓存版本升到 15，避免复用旧解析结果。

## 验证

- 新增 replay/preflight 夹具测试，覆盖摘要与详情去重。
- 新增真实重复用户输入夹具测试，确认 assistant 输出后再次发送相同文本不会被误删。

# Fix Report - Embedded Command Wrapper Misparse

## Bug 描述

Codex 会话中，如果用户 prompt 或 assistant 回答里只是“引用/粘贴”了 Claude Code JSON/XML 示例，且示例内容包含 `<command-name>/model</command-name>`，详情页会把整条消息误展示成 `/model`。典型表现是用户粘贴三条 Claude JSONL 记录进行分析时，Codex 详情中出现一条用户 `/model` 和一条 assistant 最终回答 `/model`。

## 根因

`clean_prompt_text` 和 `prettify_display_text` 原先只要在文本任意位置发现 `<command-name>`，就抽取第一段 command wrapper 并返回 `/cmd args`。这适合处理“整条消息就是 slash command 包装”的真实控制命令，但会误伤普通文本、JSON 示例和 Markdown 代码块中的 command wrapper。

## 最终方案

- 新增 `command_wrapper_parts`，只有当整条文本去掉 `command-name` / `command-message` / `command-args` 后只剩空白时，才认为它是真实命令包装。
- `clean_prompt_text`、`prettify_display_text`、`is_command_wrapper_text` 都改用该窄条件。
- 非整条 command wrapper 的普通文本不再删除 command 标签内容，避免破坏用户粘贴的 JSON/XML 示例。
- 缓存版本升到 16，避免旧缓存继续保留错误摘要。

## 验证

- 新增文本级测试：嵌入 JSON 示例的 `<command-name>/model</command-name>` 不会被改写为 `/model`。
- 新增 Codex 集成测试：`parse_codex_session_file` 和 `parse_codex_conversation_detail` 都保留用户粘贴的 JSON 示例和 assistant 回答中的 XML 代码块。
- `cargo test --manifest-path src-tauri/Cargo.toml`

# Fix Report - Claude Code Compact Summary Misclassified As Prompt

## Bug 描述

Claude Code compact 后写入的上下文恢复摘要会以 `type=user` / `message.role=user` 形式出现在 project JSONL 中，导致会话列表标题、prompt 索引和统计把它当成真实用户输入。详情页也会把它显示为普通用户气泡。

## 根因

解析结构体没有读取 Claude Code 的 `isCompactSummary` 和 `isVisibleInTranscriptOnly` 字段，只按 `type` 与 `message.role` 判断用户消息。真实数据扫描显示 compact summary 均带 `isCompactSummary: true`，且当前样本中同时带 `isVisibleInTranscriptOnly: true`。

## 最终方案

- `ConvLine` 读取 `isCompactSummary` / `isVisibleInTranscriptOnly`。
- 文件级解析中跳过 `isCompactSummary` 行，使其不进入真实 prompt、标题候选和消息数。
- 详情解析中保留 compact summary，但标记为 `is_meta=true`，默认受“元信息”开关隐藏，避免展示为普通用户输入。
- 缓存版本升到 17，避免复用旧索引结果。

## 验证

- 扫描本地 Claude projects：11 条 compact summary 全部带 `isCompactSummary=true` 与 `isVisibleInTranscriptOnly=true`，未发现旧格式漏标样本。
- 新增回归测试 `claude_compact_summary_is_metadata_not_prompt`，覆盖标题、prompt 数、消息数和详情 meta 标记。
- `cargo test --manifest-path src-tauri/Cargo.toml`

# Fix Report - Codex Compacted Handoff Summary Shown As Final Answer

## Bug 描述

Codex rollout 中由上下文压缩生成的 handoff summary 可能以 `response_item` / `payload.type=message` / `role=assistant` / `phase=final_answer` 写入，随后再出现顶层 `type=compacted` 和 `event_msg.context_compacted`。详情页会把这类压缩摘要当成普通最终回答展示。

## 根因

解析器已有 Codex compaction 处理，但此前把“是否像压缩摘要”建立在标题/前缀白名单上。这类规则覆盖面不稳定，真实数据中的压缩摘要标题和语言形态会变化，继续维护前缀列表会反复漏判。更可靠的结构信号是：Codex 会先写入一个 `response_item` assistant `final_answer` 形式的摘要候选，随后写入顶层 `type=compacted` 控制事件，且 compacted payload 会覆盖这段摘要文本。

## 尝试记录

- 扫描含 `context_compacted` 的 rollout，确认该事件本身会在普通 commentary 附近反复出现，不能单独作为“前一条是压缩摘要”的判据。
- 扫描含顶层 `type=compacted` 的 rollout，确认目标问题中的 `assistant/final_answer` 摘要会被后续 compacted payload 完整覆盖。
- 测试中验证了不能只按 `compacted.payload` 包含候选文本无条件处理，否则普通短回答文本被 compacted payload 引用时会被误判。

## 最终方案

- 继续以顶层 `type=compacted` 为主判据，并只检查 compacted 前最近一条可展示消息是否为 assistant `final_answer` 候选；若中间出现真实用户消息、工具结果或其它可展示 response item，则候选失效。
- 不再依赖 handoff/current-progress 等摘要前缀白名单；改为要求 compacted payload 覆盖候选全文或足够长的候选前缀。
- 对候选文本设置最小实质长度，避免普通短回答被 compacted payload 引用时误判为压缩摘要。
- 详情解析中不再删除这类消息，而是标记为 `is_meta=true`，默认受“元信息”开关隐藏。
- 会话统计中这类压缩摘要不计入普通 assistant final answer。
- 缓存版本升到 18，避免复用旧解析结果。

## 验证

- 新增/调整 Codex compaction 回归测试，覆盖 `**Current Progress**` + `compacted` + `context_compacted` 形态、无固定标题前缀的长摘要、以及普通短回答防误判。
- `cargo test --manifest-path src-tauri/Cargo.toml`
