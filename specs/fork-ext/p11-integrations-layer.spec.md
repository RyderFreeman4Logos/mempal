spec: task
name: "P11: Tool-neutral integrations layer (~/.mempal/integrations/ as shared hook/skill asset root)"
tags: [feature, integration, hooks, cli, distribution]
estimate: 1.5d
---

## Intent

为 mempal 建立一个**工具中立**的 integration 资产目录 `~/.mempal/integrations/{claude-code,codex,csa}/`，把 hook scripts + skill manifests 放在 agent 无关的 POSIX 路径上；agent 配置（`~/.claude/settings.json` 等**全局** config）只写 reference，不内嵌逻辑。

**动机**：
- 用户不想跟踪 repo 内 `./.claude`（现有 `mempal cowork-install-hooks` 往 `./.claude/settings.json` 写 hook，违反此约束）
- 后续 Codex / CSA 集成应共享同一 source-of-truth，避免在每个工具的 config 内复制 hook 脚本
- Mempal 二进制升级时，只更新 `~/.mempal/integrations/` 即可，agent configs 指向的 path 不变

**v11 判决依据**：
- claude-mem 的 skill/hook 资产散落在 `~/.claude/plugins/claude-mem-*` 下，Claude Code 专属；mempal 定位是多 agent backend，资产与 agent 解耦是必然
- 现有 `p8-hook-passive-capture` spec 里的 `~/.claude/settings.json` 直接 merge 策略已能 work，但不符合"整合点分离"原则；此 spec **不 cutover** 现有 hook，只给新 hook/skill 铺路，后续 spec 再做迁移

## Decisions

- 新建目录树 `~/.mempal/integrations/`：
  ```
  ~/.mempal/integrations/
  ├── claude-code/
  │   ├── hooks/         # shell scripts that call `mempal prime`, `mempal session-end`, etc.
  │   ├── skills/        # SKILL.md files (Claude Code format)
  │   └── settings-snippet.json  # JSON fragment to merge into ~/.claude/settings.json
  ├── codex/
  │   ├── hooks/
  │   ├── skills/
  │   └── config-snippet.toml
  ├── csa/
  │   └── (reserved; trait-only in this spec)
  └── manifest.toml      # asset inventory: path -> expected_hash, for bootstrap verification
  ```
- 新 CLI 子命令组 `mempal integrations`：
  - `mempal integrations bootstrap` —— 把 mempal 二进制 `include_str!` 打包的资产解压到 `~/.mempal/integrations/`，校验 hash；已存在且 hash 一致则 no-op，不一致则先备份再覆写
  - `mempal integrations install --tool <tool> [--profile user|global]` —— 把 `<tool>/settings-snippet.*` 合并进 tool 的 **global** 配置（Claude Code 默认 `~/.claude/settings.json`），`hooks` / `mcpServers` 数组**追加**不覆盖
  - `mempal integrations uninstall --tool <tool>` —— 从 tool 全局 config 移除由 mempal 写入的 entries（用 `"mempal_source": true` marker 识别），其他 entries 原样保留
  - `mempal integrations status` —— 列每个 tool 的 install 状态 + 资产 hash 是否 drift
- 资产 vendoring：mempal 二进制编译时 `include_str!` 把 `assets/integrations/**/*` 嵌入，`bootstrap` 时解压
- Tool-specific installer 走 trait：
  ```rust
  trait ToolIntegration {
      fn name(&self) -> &'static str;
      fn config_paths(&self) -> Vec<PathBuf>;
      fn merge_snippet(&self, existing: &str, snippet: &str) -> Result<String>;
      fn detect_our_entries<'a>(&self, existing: &'a str) -> Vec<MempalMarker<'a>>;
  }
  ```
  Claude Code 实现落地；Codex / CSA 给占位 impl（返回 `not_yet_implemented` error），为后续 spec 预留
- Installer 写前**备份**：`settings.json.bak.<unix_ts>`
- 所有整合写操作通过 `flock(~/.mempal/integrations/.lock)` 串行化（防并发 install）
- 默认 install target 是**全局**（`~/.claude/settings.json`），**禁止** `--profile project` 落到 `./.claude/`（spec 级 forbidden，见 Boundaries）

## Boundaries

### Allowed
- `integrations/mod.rs`（新 module）
- `integrations/{claude_code,codex,csa}.rs`（per-tool impl）
- `src/main.rs`（注册 `integrations` 子命令组）
- `assets/integrations/**/*`（资产模板；`cargo build` 时通过 `include_dir!` 或 `include_str!` 打包）
- `Cargo.toml`（引入 `include_dir` / `serde_json` 已有 / `fs2` for flock）
- `tests/integrations_layer.rs`（新建集成测试）
- `docs/integrations.md`（使用说明）

### Forbidden
- 不写入 repo 内 `./.claude/`、`./.codex/`、`./.csa/`、`./CLAUDE.md`（此 spec 覆盖面）
- 不写 `~/.claude/settings.local.json`（per-project override，与全局约束冲突）
- 不改 schema / 不 bump `fork_ext_version`
- 不 cutover 现有 `p8-hook-passive-capture` 的 hook 写入（本 spec 只给新渠道铺路，迁移是后续 spec）
- 不做 Windows 路径支持（POSIX only；明确 error on Windows detect）
- 不实现 Codex / CSA 的 actual merge 逻辑（仅 trait + `not_yet_implemented` stub）
- 不做 asset 自动升级 / 热更新（只 bootstrap 时校验 hash，drift 提示用户手动 rerun）
- 不探测 tool 是否已有其他 memory backend（不与 claude-mem 竞争同一 hook slot，完全独立 append）

## Out of Scope

- claude-mem 现有 hook / skill 的迁移（cutover 留给独立 spec，用户要求保留 claude-mem 作为兜底）
- 各 tool 的 skill 内容移植（`p11-session-priming-hook` 和 `p11-mcp-timeline-tool` 提供数据源；skill 文件模板由后续 spec 填充）
- 资产签名 / 供应链安全（mempal 自举资产信任 release 二进制，不引入 GPG 验证）
- Windows 支持
- 跨机器同步 `~/.mempal/integrations/`（用户本地事务，不做 cloud sync）

## Completion Criteria

Scenario: `mempal integrations bootstrap` 首次运行生成完整目录树
  Test:
    Filter: test_bootstrap_creates_integrations_tree
    Level: integration
    Targets: integrations/mod.rs
  Given `$HOME/.mempal/integrations/` 不存在
  When 运行 `mempal integrations bootstrap`
  Then `~/.mempal/integrations/claude-code/hooks/` 存在且非空
  And `~/.mempal/integrations/manifest.toml` 存在并列出所有资产 + blake3 hash
  And 退出码 0

Scenario: bootstrap 二次运行 hash 一致时 no-op
  Test:
    Filter: test_bootstrap_idempotent_when_hash_matches
    Level: integration
    Targets: integrations/mod.rs
  Given `bootstrap` 已运行一次
  And 所有资产文件 hash 未改
  When 再次运行 `mempal integrations bootstrap`
  Then 没有文件被重写（mtime 不变）
  And stdout 包含 "up-to-date"
  And 退出码 0

Scenario: `mempal integrations install --tool claude-code` append 而非覆盖
  Test:
    Filter: test_install_appends_to_existing_settings
    Level: integration
    Targets: integrations/claude_code.rs
  Given `~/.claude/settings.json` 已有 unrelated `hooks.SessionStart` entry
  When 运行 `mempal integrations install --tool claude-code`
  Then 原有 entry 保留，byte-for-byte
  And 新增 entry 带 `"mempal_source": true` marker
  And `settings.json.bak.<ts>` 备份文件存在

Scenario: 拒绝 repo 内 `./.claude/`
  Test:
    Filter: test_install_refuses_local_claude_dir
    Level: integration
    Targets: integrations/claude_code.rs
  Given CWD 含 `./.claude/` 目录
  When 运行 `mempal integrations install --tool claude-code --profile project`
  Then 退出码非零
  And stderr 包含 "project profile disabled by P11 spec; use --profile user"
  And `./.claude/settings.json` 未被修改

Scenario: `uninstall` 只清自己的 entries
  Test:
    Filter: test_uninstall_preserves_unrelated_entries
    Level: integration
    Targets: integrations/claude_code.rs
  Given `~/.claude/settings.json` 含 mempal + 非 mempal 的 hooks 各一条
  When 运行 `mempal integrations uninstall --tool claude-code`
  Then 带 `"mempal_source": true` 的 entry 被删
  And 不带 marker 的 entry 保留
  And JSON 仍然合法（无悬垂逗号）

Scenario: 并发 install 被 flock 串行
  Test:
    Filter: test_concurrent_install_flock_serialized
    Level: integration
    Targets: integrations/mod.rs
  Given 两个 `mempal integrations install --tool claude-code` 进程同时启动
  When 两者都 run to completion
  Then 只有一个 backup file 生成（第二个 install 看到已是最新，no-op）
  And `~/.claude/settings.json` 合法 JSON
  And 不存在"丢失的 entry"（至少一次 install 成功写入）

Scenario: Codex / CSA 未实现返回 stub error
  Test:
    Filter: test_codex_install_returns_not_yet_implemented
    Level: unit
    Targets: src/integrations/codex.rs
  When 运行 `mempal integrations install --tool codex`
  Then 退出码非零
  And stderr 含 "not_yet_implemented: codex integration is spec-reserved, see specs/fork-ext/p11-integrations-layer.spec.md"

Scenario: `status` 报告 asset drift
  Test:
    Filter: test_status_reports_drift_on_manual_edit
    Level: integration
    Targets: integrations/mod.rs
  Given `bootstrap` 已成功
  And 用户手动修改了 `~/.mempal/integrations/claude-code/hooks/session-start.sh`
  When 运行 `mempal integrations status`
  Then 该 hook 标为 `drifted`（current_hash != manifest_hash）
  And 退出码非零（drift 视作需要人工介入）

Scenario: Windows 平台 early-error
  Test:
    Filter: test_windows_detection_errors_early
    Level: unit
    Targets: integrations/mod.rs
  Given `cfg!(target_os = "windows")` 为 true
  When 运行 `mempal integrations bootstrap`
  Then 退出码非零
  And stderr 含 "integrations layer is POSIX-only; Windows not supported"
