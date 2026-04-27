spec: task
name: "P11: verbatim-safe transcript noise stripping — scoped to Claude Code JSONL / Codex rollouts"
tags: [ingest, normalize, noise-strip, optional]
estimate: 0.5d
---

## Intent

借鉴 mempalace ca2598a (`make strip_noise verbatim-safe and scope it to Claude Code JSONL`) 的思路。

当前问题：mempal P5 格式支持识别 Slack DM 和 Codex CLI（`src/ingest/detect.rs` + `normalize.rs`），但对 **Claude Code JSONL transcript** 的 UI 噪声（system tag、hook output reminder、tool_use_id 裸露 token、reminder envelopes）处理不够彻底。ingest 后 drawer content 里夹杂大量 `<system-reminder>...</system-reminder>` 段，搜索时这些 token 污染 embedding 且占用 `content.len()`。

mempalace 的方案：严格的 **scoped** noise-strip（只对 Claude Code JSONL 启用，不碰人类撰写文本），且是 **verbatim-safe**（保留用户说的话、保留代码块、只去 UI chrome）。

核心用户价值：**从 Claude Code JSONL ingest 出来的 drawer 变干净**，search 命中时直接可读；不影响任何 human-authored ingest。

**Optional 原因**：P5 基础格式支持已 ok；本 spec 是**精炼**而非补缺。

## Decisions

- **新增模块 `src/ingest/noise.rs`**：
  - `pub fn strip_claude_jsonl_noise(content: &str) -> String`
  - `pub fn strip_codex_rollout_noise(content: &str) -> String`
  - 内部 **dependency-free 白名单扫描** 式剥离，逐行扫描（不引新 dep）：
    - 匹配 `<system-reminder>(.|\n)*?</system-reminder>` → 去
    - 匹配独立行 `<command-name>...</command-name>` → 去
    - 匹配 `[{"type":"tool_use_id","id":"..."}]` 裸块 → 去
    - 匹配行首 `=== DORA SKILLS LOADED ===`/`=== RUST SKILLS Loaded ===` 及其连续块 → 去（到下一个空行结束）
    - 匹配 Codex rollout 里的 `[session ... started]` / `[session ... ended]` → 去
    - 其他内容原样保留（包括 code block、user message quote、tool result 文本）
- **触发路径严格 scope**：
  - 只在 `detect.rs` 识别出 `Format::ClaudeJsonl` 或当前代码中的 `Format::CodexJsonl`（Codex rollout JSONL）时，`normalize.rs` 调对应 strip 函数
  - 其他 format（Markdown / Slack DM / Plain Text / YAML）**禁止**走这条路径
- **Verbatim 保留保证**（测试覆盖）：
  - 代码块 `\`\`\`...\`\`\`` 跨行保留
  - 用户消息中的 `<`/`>` 字符不被 escape 或删
  - 中文 / emoji / 引号字节级不变
- **不 bump schema**：这是 normalize 层改动；但**触发 normalize_version bump**（若 P10 normalize_version spec 已 merge），`CURRENT_NORMALIZE_VERSION: 1 → 2`。历史 drawer 通过 `mempal reindex --stale` 重处理
- **Normalize version 依赖**：**本 spec 依赖 P10 `p10-normalize-version` 先落地**；没有 normalize_version 机制时，历史 drawer 无法被选择性重刷
- **Metrics**：ingest 完成后如果触发 noise strip，`IngestStats` 追加 `noise_bytes_stripped: Option<u64>`（debug 可见）
- **CLI**：`mempal ingest` 自动走 strip（由 detect 决定），用户无感；如需强制禁用加 `--no-strip-noise` flag（默认 strip）
- **Fallback**：白名单扫描无匹配或 format detection 误判时走**整体保留**不 panic

## Boundaries

### Allowed
- `src/ingest/noise.rs`（新增）
- `src/ingest/normalize.rs`（调 noise strip；bump CURRENT_NORMALIZE_VERSION 到 2）
- `src/ingest/detect.rs`（如需识别更多格式变种）
- `src/ingest/mod.rs`（`IngestStats` 加 `noise_bytes_stripped` 字段）
- `src/main.rs`（`--no-strip-noise` flag）
- `tests/noise_strip.rs`（新增，含 verbatim 保全断言）

### Forbidden
- 不对非 ClaudeJsonl / CodexJsonl 格式运行 noise strip
- 不改变代码块内容
- 不改 user message 的字节（只去 system 生成的 UI chrome）
- 不引新 dep（现有 `regex` 足够）
- 不破坏 P5 已有格式测试
- 不在 `wing="agent-diary"` 的手动 ingest 走 strip（diary 是人/agent 写的观察）
- 不动 `mempal_peek_partner` 的 session reader 逻辑（peek 不走 ingest normalize）

## Out of Scope

- 对 Cursor / Aider / 其他 agent transcript 做 strip（单独 spec 按需加）
- LLM-based noise detection
- 可配置的自定义 strip 规则
- 把 tool_use block 结构化展开（保持 verbatim 的"看起来像什么就是什么"原则）
- 对 raw file 做 strip（strip 只针对 transcript）

## Completion Criteria

Scenario: Claude JSONL 的 system-reminder 被剥离
  Test:
    Filter: test_claude_jsonl_strips_system_reminder
    Level: unit
  Given content 含 `hello <system-reminder>mcp info</system-reminder> world`
  When `strip_claude_jsonl_noise(content)`
  Then 输出 == "hello  world"
  And 原始 "hello" 和 "world" 字节不变

Scenario: 代码块跨行保留
  Test:
    Filter: test_code_block_preserved_verbatim
    Level: unit
  Given content 含 `\`\`\`rust\nfn main() {}\n\`\`\`\n<system-reminder>x</system-reminder>`
  When strip
  Then 输出保留完整代码块
  And system-reminder 被去除

Scenario: 用户消息的 < / > 字符不被 escape
  Test:
    Filter: test_user_message_angle_brackets_preserved
    Level: unit
  Given content 含 `user: "I prefer Vec<T> over [T]"`
  When strip
  Then 字符串 "Vec<T>" 和 "[T]" 字节级保留

Scenario: 非 Claude JSONL 格式不走 strip
  Test:
    Filter: test_plain_markdown_not_stripped
    Level: integration
  Given content = "# Title\n<system-reminder>fake</system-reminder>\nbody"（被识别为 Markdown）
  When ingest
  Then drawer.content 保留 `<system-reminder>fake</system-reminder>` 字符串

Scenario: Codex rollout session 标记被剥离
  Test:
    Filter: test_codex_rollout_session_markers_stripped
    Level: unit
  Given content = "[session 12345 started]\nwork\n[session 12345 ended]"
  When `strip_codex_rollout_noise`
  Then 输出 == "work"

Scenario: regex 不匹配时整体保留不 panic
  Test:
    Filter: test_strip_no_match_returns_identity
    Level: unit
  Given content = "plain text no markers"
  When strip
  Then 输出 == "plain text no markers"（字节级相等）

Scenario: 中文 / emoji 字节级保留
  Test:
    Filter: test_strip_preserves_unicode_bytes
    Level: unit
  Given content = "决策 🎯 <system-reminder>x</system-reminder> 完成 ✅"
  When strip
  Then 输出含 "决策 🎯" 和 " 完成 ✅" 字节级不变

Scenario: ingest 返回 noise_bytes_stripped 字段
  Test:
    Filter: test_ingest_outcome_reports_stripped_bytes
    Level: integration
  Given Claude JSONL ingest 原文件 10KB，含 2KB system-reminder
  When ingest 完成
  Then outcome.noise_bytes_stripped ≈ 2048（容差 ±100B）
  And drawer.content.len() ≈ 8KB

Scenario: --no-strip-noise flag 禁用 strip
  Test:
    Filter: test_cli_no_strip_noise_flag
    Level: integration
  Given Claude JSONL 含 system-reminder
  When `mempal ingest --no-strip-noise file.jsonl`
  Then drawer.content 含 system-reminder 字符串（未剥离）

Scenario: CURRENT_NORMALIZE_VERSION bump 到 2 后 reindex --stale 触发重刷
  Test:
    Filter: test_normalize_version_bump_triggers_reindex_opportunity
    Level: integration
  Given 旧 drawer normalize_version=1, CURRENT_NORMALIZE_VERSION=2
  And 该 drawer 来自 Claude JSONL 含 system-reminder
  When `mempal reindex --stale`
  Then 重刷后 drawer.content 无 system-reminder
  And drawer.normalize_version == 2
