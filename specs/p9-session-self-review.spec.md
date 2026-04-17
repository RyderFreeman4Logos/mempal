spec: task
name: "P9: Session self-review capture — passive session-end digest as primary memory"
tags: [feature, capture, session, hooks]
estimate: 1d
---

## Intent

在 P8 hook 捕获到 `hook_session_end` 事件时，从 payload 中**提取 agent 最后生成的 assistant 消息**，作为**高信号密度**的主记忆条目存入 `wing="session-reviews"` / `room=<agent-name>`；同时把这条 review drawer 的 `metadata.linked_drawer_ids` 填为同 session 的其他 tool-call drawer ID 列表，便于后续通过 `mempal_read_drawer` 按需 drill-down。

**为什么这是 mempal 的高价值增强**：
- Agent 在 session 末尾生成的 "I accomplished X, decided Y, discovered Z" 是**最高信号密度**的内容（人类已经"为了让用户读懂"写过一遍）
- 这些文字**已经存在于 agent stdout**——零额外 LLM 调用，零 token 成本
- 当前 mempal + claude-mem 都把它们当 ephemeral 消息丢弃，然后反倒去靠 hook 捕获 / LLM 压缩重建——duplicative 且 lossy
- 把 self-review 作为**主记忆**，把工具调用作为**linked_evidence**，retrieval 模式变成"先读 summary、按需 drill-down"，节省 10× token

**v3 判决依据**：Issue #6 原文 Innovation 3。v2 REJECT 理由是"claude-mem 没做"——不是判决依据（`feedback_feature_value_not_origin.md`）。独立价值极高且零成本。

**Default off**：`[hooks.session_end] extract_self_review = false`，opt-in 激活。

**与 P5 agent-diary 的区别**：agent-diary 要求 agent 主动调 `mempal_ingest(wing="agent-diary")`——voluntary compliance。self-review 是 passive capture，agent 忘记写 diary 也照样存到 `session-reviews` wing。二者共存：diary 用于显式反思，self-review 用于 session-end 自动归档。

## Decisions

- 依赖 P8 `p8-hook-passive-capture.spec.md`——daemon 已能收到 `kind=hook_session_end` 消息
- 在 daemon handler 新增 `handle_session_end(payload: &SessionEndPayload)`：
  1. parse payload（预期 schema：`{ session_id, messages: [{role, content}], tool_calls: [{drawer_id}] }`）
  2. 找 `messages` 中最后一条 `role == "assistant"` 的 content；若启用 `trailing_messages > 1`，取倒数 N 条拼接
  3. 若长度 < `min_length`（默认 100），log debug 并 skip
  4. 写 drawer：
     - `wing = "session-reviews"`（新 wing，无需显式声明）
     - `room = <agent-name>`（从 payload `agent` 字段推断，fallback "unknown-agent"）
     - `content = <提取的 assistant message 原文>`（raw verbatim，**不做** LLM 改写）
     - `source_file = <session_id>`（用作引用锚点）
     - `metadata.linked_drawer_ids = <tool_calls 中的所有 drawer_id>`（从 payload 获取）
     - `metadata.session_id = <session_id>`
     - `importance_stars = 3`（因为是 session 级主记忆，默认偏高；实际值由 AAAK signal 提取覆盖）
  5. 经 privacy scrub（P8）+ gating（P9，可选）+ novelty filter（P9，可选）→ 写入
  6. daemon confirm
- 配置：
  ```toml
  [hooks.session_end]
  extract_self_review = false  # default: off
  trailing_messages = 1        # 取倒数 N 条 assistant 消息拼接
  min_length = 100             # 字符数（不是字节）
  wing = "session-reviews"     # 存储 wing（可覆盖）
  ```
- payload schema 是**宽松的**：tool_calls 字段可缺失，messages 必须有；解析失败 → `mark_failed` 走 retry
- `metadata` 字段的**最终决策**：**不新增侧表、不 bump schema**。用 drawer content 末尾的 `\n\n--- session_metadata ---\nsession_id: X\nlinked_drawer_ids: Y, Z` 段做标记，字符串解析；后续需要 structured metadata 时再独立起 spec 升级
  - 这个段使用 sentinel `--- session_metadata ---` 作为 AAAK signal 提取器的可识别边界
  - 本 spec **零 schema 迁移**；v8 被 `p10-project-vector-isolation.spec.md` 独占使用，本 spec 与其无 schema 冲突
- MCP `mempal_search` 天然能召回 session-reviews wing 的 drawer（向量+FTS+RRF 不区分 wing）
- 引入便利过滤器：未来 `mempal_search(wing_filter="session-reviews")` 可一键查 session reviews；本 spec 不实现 wing_filter，留给 P10 `p10-cli-dashboard.spec.md`
- 生成 self-review 的失败（payload 格式错 / 无 assistant message）不阻塞其他 hook 处理——handler 返回 error，daemon `mark_failed` retry
- 不破坏既有 `hook_session_end` 的其他副作用（P8 已规定写 `wing=hooks-raw, room=session-lifecycle` 的审计 drawer；self-review handler **额外**写一条到 `session-reviews`，二者并存）

## Boundaries

### Allowed
- `crates/mempal-cli/src/daemon.rs`（扩展 `hook_session_end` handler）
- `crates/mempal-cli/src/session_review.rs`（新建：extraction 逻辑 + payload parser）
- `crates/mempal-core/src/config.rs`（`SessionEndConfig` struct + `[hooks.session_end]` parsing）
- `tests/session_self_review.rs`（新建）

### Forbidden
- 不要用 LLM 总结 messages（零成本原则：直接用原文）
- 不要丢弃 P8 已有的 `hooks-raw` 审计 drawer——self-review 是额外产物
- 不要给 self-review drawer 应用 novelty drop（session review 可能每次内容不同但结构相似，drop 会损失重要信号）；**仅允许** novelty merge，或直接 bypass novelty（由 `wing=session-reviews` 触发 bypass 列表）
- 不要在 MCP ServerInfo.instructions 里暴露 self-review 的存在——agent 从检索侧自然发现
- 不要引入新 schema migration（复用现有 drawer 表 + 末尾 sentinel 段策略）
- 不要在 self-review drawer 的 content 前加装饰性前缀（如 "## Session Review\n"）——content 是 raw assistant message
- 不要自动触发 KG triple 抽取——单独的 KG 生成是 agent 职责

## Out of Scope

- 跨 session 的 self-review 聚合（"本周总结"类衍生）
- Multi-turn trailing_messages > 1 时的智能分段（本版直接拼接）
- `linked_drawer_ids` 的 KG 建边（future work）
- Non-assistant role 过滤（只保留 "assistant"；"user"/"system"/"tool" 始终忽略）
- 在 search 结果中优先置顶 session-reviews（排序由现有 RRF 决定）
- Web UI / CLI 专属 "session reviews" 视图（P10 `mempal timeline` 自然能查到，不单独工具）
- 对 payload 里 `tool_calls` 中未找到的 drawer_id 做 best-effort 清理

## Completion Criteria

Scenario: enabled=false 时不生成 session-review drawer
  Test:
    Filter: test_disabled_no_session_review
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs, crates/mempal-cli/src/session_review.rs
  Given `[hooks.session_end] extract_self_review = false`
  And 队列中一条 hook_session_end payload 含 assistant message 200 字符
  When daemon 处理完该消息
  Then `drawers` 表中 `wing = "session-reviews"` 的 drawer 计数为 "0"
  And `wing = "hooks-raw"` 的 drawer 计数为 "1"（P8 默认行为）

Scenario: enabled=true 且 message 足够长时生成 session-review drawer
  Test:
    Filter: test_enabled_creates_session_review_drawer
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs, crates/mempal-cli/src/session_review.rs
  Given `extract_self_review = true, min_length = 100`
  And 队列中 hook_session_end payload：
    `{session_id: "S1", agent: "claude", messages: [{role:"user",content:"go"},{role:"assistant",content:"I finished refactor X and decided to use approach Y because Z..." (200 字)}]}`
  When daemon 处理完该消息
  Then `drawers` 表中 `wing = "session-reviews", room = "claude"` 新增 1 行
  And 该 drawer `content` 开头是 assistant message 原文
  And `source_file == "S1"`

Scenario: message 短于 min_length 时跳过
  Test:
    Filter: test_short_message_skipped
    Level: unit
    Targets: crates/mempal-cli/src/session_review.rs
  Given `min_length = 100`
  And assistant message content 长度 50 字符
  When 调 `session_review::extract(payload, cfg)`
  Then 返回 `None` 或等价 skip
  And daemon 不写 session-reviews drawer

Scenario: trailing_messages=2 时拼接倒数 2 条 assistant 消息
  Test:
    Filter: test_trailing_messages_concatenation
    Level: unit
    Targets: crates/mempal-cli/src/session_review.rs
  Given `trailing_messages = 2`
  And messages 最后两条都是 assistant：`[..., {role:"assistant",content:"A"}, {role:"assistant",content:"B"}]`
  When 调 `session_review::extract`
  Then 返回的 content 含 "A" 和 "B" 之间有分隔符（如 `\n---\n`）

Scenario: linked_drawer_ids 写入 content 末尾 sentinel 段
  Test:
    Filter: test_linked_drawer_ids_in_sentinel_section
    Level: integration
    Targets: crates/mempal-cli/src/session_review.rs
  Given payload `tool_calls: [{drawer_id:"D1"}, {drawer_id:"D2"}]` 和 session_id "S1"
  When daemon 写 session-review drawer
  Then drawer.content 含 `--- session_metadata ---` sentinel 行
  And sentinel 段下方含 `session_id: S1`
  And sentinel 段下方含 `linked_drawer_ids: D1, D2`

Scenario: session-reviews wing 的 drawer bypass novelty filter 的 drop 决策
  Test:
    Filter: test_session_reviews_bypass_novelty_drop
    Level: integration
    Targets: crates/mempal-cli/src/session_review.rs, crates/mempal-ingest/src/novelty.rs
  Given novelty `enabled = true, duplicate_threshold = 0.95`
  And 已有 session-review drawer A 与 candidate cosine == 0.97
  When daemon 处理新 session-end 生成 candidate
  Then candidate 作为新 drawer 插入（未被 drop）
  And `novelty_audit` 无本次决策行

Scenario: payload 无 assistant message 时 handler 跳过但不 fail
  Test:
    Filter: test_no_assistant_message_handler_skips
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs, crates/mempal-cli/src/session_review.rs
  Given `extract_self_review = true`
  And payload messages 全为 `[{role:"user",...}, {role:"tool",...}]`
  When daemon 处理
  Then 不写 session-reviews drawer
  And daemon `confirm` 该消息（非 mark_failed）
  And log 含 info "no trailing assistant message"

Scenario: agent 字段缺失时 room fallback 为 unknown-agent
  Test:
    Filter: test_missing_agent_falls_back
    Level: unit
    Targets: crates/mempal-cli/src/session_review.rs
  Given payload 缺 `agent` 字段
  When 调 `session_review::extract(payload, cfg)`
  Then 返回 drawer 建议 `room == "unknown-agent"`

Scenario: self-review 经过 privacy scrub（不泄漏凭证）
  Test:
    Filter: test_self_review_subject_to_privacy_scrub
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs, crates/mempal-ingest/src/privacy.rs
  Given `privacy.enabled = true, openai_key` 默认 pattern 开启
  And assistant message 含 `"...I called the API with sk-abcdef1234567890abcdef1234567890abcd..."`
  When daemon 处理 session-end 后写 session-reviews drawer
  Then drawer.content 含 `[REDACTED:openai_key]` 且不含原 sk- 字串

Scenario: 正常 P8 hooks-raw 审计 drawer 不被替代
  Test:
    Filter: test_hooks_raw_audit_drawer_still_written
    Level: integration
    Targets: crates/mempal-cli/src/daemon.rs
  Given `extract_self_review = true` 且 payload 合法
  When daemon 处理 hook_session_end
  Then `wing = "hooks-raw", room = "session-lifecycle"` 新增 1 drawer（P8 行为）
  And `wing = "session-reviews", room = <agent>` 新增 1 drawer（本 spec 行为）
