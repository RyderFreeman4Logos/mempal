spec: task
name: "P10: Folder-level CLAUDE.md hotpatch suggestions (opt-in, conservative, user-approved only)"
tags: [feature, context-awareness, optional, conservative]
estimate: 1d
status: optional
---

## Intent

当 P8 hook 捕获到 agent 修改或读取某文件夹下文件、且对应 drawer 具有高价值信号（`importance_stars >= 4` 或 `flags` 含 `DECISION`/`PIVOT`）时，生成一条 **hotpatch suggestion**——候选要追加到该文件夹最近 CLAUDE.md 的简短摘要，**写入 mempal 独占的建议池** (`~/.mempal/hotpatch/CLAUDE-<dir-hash>.md`)，**绝不**自动修改用户的实际 CLAUDE.md 文件。

用户通过 `mempal hotpatch review` 审查建议，`mempal hotpatch apply --dir <path>` 显式批准后，才把 suggestion 合并到真实 CLAUDE.md。

**动机**：claude-mem `src/services/worker/agents/ResponseProcessor.ts:241-260` (`updateFolderClaudeMdFiles`) 展示的思路——让高价值记忆自然"冒泡"到 agent 每次会话都会读的 CLAUDE.md，减少 MCP 查询次数。但 claude-mem 直接写用户文件的风险太大；mempal 的保守化改造是"建议池 + 人工 apply 门控"。

**v3 判决依据**：v2 分析 "claude-mem 值得吸收但 7 个 issue 没覆盖" 第 3 项。保守改造后仍有价值。**标记为 optional**——可作为 P10 里程碑的最后一项，也可推迟到 P11；不阻塞 P10 核心验收。

**Default off**：`[hotpatch] enabled = false`，显式 opt-in。

## Decisions

- 新建 `crates/mempal-cli/src/hotpatch/{mod,generator,manager}.rs`
- Daemon 处理 `hook_post_tool` 时，对满足 gate 的 drawer（`importance_stars >= 4` 或 `flags` 含 `DECISION`/`PIVOT`）额外触发 `hotpatch::generator::suggest_for_drawer`
- Suggestion 生成逻辑：
  1. 从 drawer payload 提取涉及的文件路径（`tool_input.file_path`、`tool_input.files[]` 等常见字段）
  2. 对每个路径，沿目录向上找第一个含 CLAUDE.md（或 AGENTS.md / GEMINI.md，按配置）的目录 `<D>`
  3. 先把 `<D>` 标准化为**绝对路径**（`std::fs::canonicalize(<D>)?`；解析符号链接、消除 `..` / `.` 段），再对标准化结果计算稳定 hash（SHA-256 截前 12 字符）作为 suggestion 文件名：`~/.mempal/hotpatch/CLAUDE-<hash>.md`。**未规范化直接 hash 相对路径是禁止的**——同一物理目录被不同 cwd 访问会产生不同 hash，污染建议池并让 `mempal hotpatch apply --dir <D>` 找不到对应 suggestion。若 `canonicalize` 失败（路径不存在 / 权限不足）→ fail-fast 并在 daemon log warn，**不**回退为相对路径 hash
  4. 生成 suggestion 行（可选 AAAK signal 辅助）：`- <importance_stars 星> <topic>: <一句话摘要 (≤ 80 字符)> [drawer:<id[:8]>]`
  5. append 到 suggestion 文件（去重：若同 drawer_id 已存在，skip）。**必须 `flock`** 该 suggestion 文件（排他锁）再读、再 append、再 unlock——CSA design review 2026-04-20 识别的并发写问题：daemon 可能并行跑多个 worker（每个处理队列里一条消息），若同一目录下连续 hit 多个 drawer、触发多个 worker 同时 append 同一 `CLAUDE-<hash>.md` 文件，POSIX 文件系统下无锁 append 在 Linux 上未必原子（依赖 page cache 行为），更不用说 dedup 需要先 read 再 skip——read-then-append 的组合必须在锁内完成才能保证"同 drawer_id 不重复"的不变量
- Suggestion 文件结构：
  ```md
  # mempal hotpatch suggestions for <dir-path>
  
  <!-- managed by mempal, safe to edit — apply via `mempal hotpatch apply --dir <dir-path>` -->
  
  - ★★★★ decision: use Arc<Mutex<>> over RwLock for low-write path [drawer:01KAB34C]
  - ★★★ bug-fix: trailing newline corrupts FTS5 tokenization [drawer:01KAB56D]
  ```
- 追加是 **append-only**——从不自动 modify 或删除既有 suggestion
- `mempal hotpatch review [--dir <path>]` 子命令：
  - 默认列所有 suggestion 文件 + 对应目录
  - `--dir <path>` 显示该目录的 pending suggestion
  - 输出含原 CLAUDE.md 路径 + suggestion 文件路径 + 条数
- `mempal hotpatch apply --dir <path> [--confirm]` 子命令：
  - 无 `--confirm` 时 dry-run，打印 diff
  - 有 `--confirm` 时按以下规则合并到该目录 CLAUDE.md：
    - 在 CLAUDE.md 末尾（或 `## Hotpatch` section 内）append suggestion
    - 去重：若 suggestion 行已在 CLAUDE.md 中，skip
    - 保留原 CLAUDE.md 其他内容 byte-level 不变
    - apply 成功后 suggestion 文件不删除（审计保留），但加 marker `<!-- applied <timestamp> -->` 到该行末
- 配置：
  ```toml
  [hotpatch]
  enabled = false
  min_importance_stars = 4
  watch_files = ["CLAUDE.md", "AGENTS.md", "GEMINI.md"]  # 向上找哪些文件
  max_suggestion_length = 80  # 一句摘要字符上限
  ```
- 一句摘要生成：
  - 取 drawer content 首行（剥离 markdown heading 前缀）
  - 若超 `max_suggestion_length`，走 `preview::truncate`（P9 新增）截断
  - **不**调 LLM 生成 smart summary（零成本原则 + feedback `no_llm_api_dependency`）
- suggestion 文件 gc：`mempal hotpatch clean --older-than 30d` 手动清理过老的 applied suggestion（本 spec 不含 auto-gc）
- Hotpatch 不触及 `drawers` 表 / 不加 schema 变更
- 所有写操作仅触及 `~/.mempal/hotpatch/` 目录 + 用户 apply 时指定的 CLAUDE.md

## Boundaries

### Allowed
- `crates/mempal-cli/src/hotpatch/` 子目录（新建）
- `crates/mempal-cli/src/main.rs`（注册 `mempal hotpatch review/apply/clean`）
- `crates/mempal-cli/src/daemon.rs`（post-ingest 扩展钩子触发 hotpatch generator）
- `crates/mempal-core/src/config.rs`（`HotpatchConfig`）
- `~/.mempal/hotpatch/` 目录（新建，运行时管理）
- `tests/hotpatch_generator.rs`、`tests/hotpatch_apply.rs`（新建）

### Forbidden
- 不要在没有 `--confirm` 的情况下修改 user 的 CLAUDE.md / AGENTS.md / GEMINI.md
- 不要删除 user 的 CLAUDE.md 既有内容（只 append）
- 不要自动移除 suggestion 文件中的条目（append-only + applied marker）
- 不要通过 MCP 工具触发 apply（只能 CLI——人类确认门控）
- 不要用 LLM 生成摘要
- 不要监听文件系统变化、主动扫描——只响应 daemon 的 hook ingest 事件
- 不要跨目录合并 suggestion（每个目标目录独立文件）
- 不要给 apply 后的 CLAUDE.md 加 mempal 专有 metadata 头——保持干净
- 不要把 suggestion 自动 propagate 到 parent 目录的 CLAUDE.md（只关注 nearest ancestor）
- **不要违反 rule 034（claude-md-compactness）**：apply 时若待追加条目会让 CLAUDE.md 单段超过合理篇幅（> ~10 行），**必须**拒绝合并并在 stderr 报错，提示用户走 rule 034 的 `.agents/project-rules-ref/` 分拆路径；hotpatch 不得把长文一次性塞进 CLAUDE.md
- **不要违反 rule 036（no-commit-ai-config）**：若目标 CLAUDE.md 路径命中 `~/.gitignore_noai`（通过读文件列表判断）或是 symlink 指向 `drafts/` / 其他受保护路径，apply 操作**必须**定位到 symlink target 的真实路径去写（不得把 symlink 替换为 regular file）；**安全要求**：写入 symlink 解析后的真实路径前，必须 `std::fs::canonicalize` 该路径并校验其落在 **白名单前缀**内（从 `[hotpatch] allowed_target_prefixes` 读取，默认含当前 workspace 根 + `$HOME/drafts/` + `$HOME/s/llm/` 等用户显式列出的 AI-config 目录）；若解析后路径逃出白名单 → fail-fast 拒绝写入并报错（防止恶意或误构造的 symlink 被利用成任意文件写入攻击面）；stderr 提示用户这是 symlink target 的真实路径写入

## Out of Scope

- Auto-apply without user confirmation（永久禁止）
- 基于 embedding 的相似 suggestion 合并
- Suggestion 投票 / 排序（用户审查时自行决定）
- 从 user 的 CLAUDE.md 反向 backport 到 mempal（`mempal hotpatch learn`——未来 spec）
- Web UI / TUI 审查（违反 CLI-first feedback）
- 多 user 协作的 suggestion 流程
- 对 `README.md`、`notes.md` 等非 agent 配置文件的 hotpatch
- Suggestion 自动过期

## Completion Criteria

Scenario: enabled=false 时不生成 suggestion
  Test:
    Filter: test_disabled_no_suggestion_generated
    Level: integration
    Targets: crates/mempal-cli/src/hotpatch/generator.rs
  Given `[hotpatch] enabled = false` 和一条 `importance_stars=5, flags=["DECISION"]` 的 drawer
  When daemon 走完 ingest
  Then `~/.mempal/hotpatch/` 目录不存在或为空

Scenario: 高重要度 drawer 生成 suggestion
  Test:
    Filter: test_high_importance_drawer_generates_suggestion
    Level: integration
    Test Double: tempfile_home
    Targets: crates/mempal-cli/src/hotpatch/generator.rs, crates/mempal-cli/src/daemon.rs
  Given `enabled = true, min_importance_stars = 4`
  And 被修改文件 `<tmp>/project/src/foo.rs`，`<tmp>/project/CLAUDE.md` 存在
  And drawer `importance_stars = 5, flags = ["DECISION"], content = "Decision: use Arc<Mutex<>>"`
  When daemon 处理完 ingest
  Then `~/.mempal/hotpatch/CLAUDE-<hash>.md` 存在
  And 文件内容含 `[drawer:...]` 标记
  And 文件首行是 header（含目标 dir 路径）

Scenario: importance 低于 threshold 不生成
  Test:
    Filter: test_low_importance_skipped
    Level: integration
    Targets: crates/mempal-cli/src/hotpatch/generator.rs
  Given `min_importance_stars = 4`
  And drawer `importance_stars = 2`
  When daemon 处理
  Then suggestion 文件不变化

Scenario: 同 drawer_id 不重复 append
  Test:
    Filter: test_duplicate_drawer_id_not_re_appended
    Level: integration
    Targets: crates/mempal-cli/src/hotpatch/generator.rs
  Given suggestion 文件已含 `[drawer:01KAB34C]` 行
  And daemon 再次触发同一 drawer_id 的 hotpatch
  Then suggestion 文件行数不变
  And 该 drawer 的 suggestion 仍只 1 行

Scenario: mempal hotpatch review 列出所有 pending
  Test:
    Filter: test_review_lists_pending_suggestions
    Level: integration
    Targets: crates/mempal-cli/src/hotpatch/manager.rs
  Given `~/.mempal/hotpatch/` 下 2 个 suggestion 文件共 5 条
  When 执行 `mempal hotpatch review`
  Then stdout 列 2 个目标目录 + 总 5 条
  And 每条含 importance / drawer_id

Scenario: mempal hotpatch apply --dir <D> 未 --confirm 只 dry-run
  Test:
    Filter: test_apply_without_confirm_is_dry_run
    Level: integration
    Test Double: tempfile_home
    Targets: crates/mempal-cli/src/hotpatch/manager.rs
  Given `<tmp>/project/CLAUDE.md` 原内容 "# Project\n"
  And `~/.mempal/hotpatch/CLAUDE-<hash>.md` 含 1 条 suggestion
  When 执行 `mempal hotpatch apply --dir <tmp>/project`
  Then stdout 含 diff 预览
  And `<tmp>/project/CLAUDE.md` byte-level 等于 "# Project\n"

Scenario: mempal hotpatch apply --confirm 合并到 CLAUDE.md
  Test:
    Filter: test_apply_confirm_merges_to_claudemd
    Level: integration
    Test Double: tempfile_home
    Targets: crates/mempal-cli/src/hotpatch/manager.rs
  Given `<tmp>/project/CLAUDE.md` 原内容 "# Project\n"
  And suggestion 文件含一条 "- ★★★★★ decision: use Arc<Mutex<>> [drawer:01KAB34C]"
  When 执行 `mempal hotpatch apply --dir <tmp>/project --confirm`
  Then `<tmp>/project/CLAUDE.md` 含原内容 "# Project"
  And 含 suggestion 那一行
  And suggestion 文件该条行末含 `<!-- applied ... -->` marker

Scenario: apply 保留 user CLAUDE.md 原有内容
  Test:
    Filter: test_apply_preserves_existing_content
    Level: integration
    Test Double: tempfile_home
    Targets: crates/mempal-cli/src/hotpatch/manager.rs
  Given CLAUDE.md 内容 "# Project\n## Rules\n- do X\n- do Y\n"
  When apply --confirm 追加 1 条 suggestion
  Then CLAUDE.md 含原 4 行 byte-level 不变
  And 新 suggestion 行追加在末尾

Scenario: suggestion 超长被截断
  Test:
    Filter: test_long_summary_truncated
    Level: unit
    Targets: crates/mempal-cli/src/hotpatch/generator.rs
  Given `max_suggestion_length = 50` 和 drawer content 首行 120 字符
  When 生成 suggestion
  Then suggestion 该行摘要部分 chars() <= 51（+ "…"）
  And 以 "…" 结尾

Scenario: 未调用 LLM API
  Test:
    Filter: test_hotpatch_no_llm_api_calls
    Level: integration
    Targets: crates/mempal-cli/src/hotpatch/*
  Given Daemon 处理一轮 hotpatch 生成 + manual apply
  When 审计进程的 outbound network connections
  Then 无任何对 `api.openai.com` / `api.anthropic.com` / `generativelanguage.googleapis.com` 等外部 LLM 端点的连接

Scenario: apply 对 `~/.gitignore_noai` 列表里的 CLAUDE.md 文件也生效
  Test:
    Filter: test_apply_works_on_gitignored_claudemd
    Level: integration
    Test Double: tempfile_home
    Targets: crates/mempal-cli/src/hotpatch/manager.rs
  Given 目标目录 CLAUDE.md 是 symlink to drafts（见 rule 036）
  When apply --confirm
  Then symlink target 文件被更新（而非替换 symlink）
  And symlink 本身未被破坏
