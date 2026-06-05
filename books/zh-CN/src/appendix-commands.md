# 附录：常用命令速查

> **本附录定位**：汇总本书正文涉及的高频命令路径，方便对照查阅；它不替代 `mempal --help`，完整参数仍以 `--help` 和各 spec 为准。

## 安装与诊断

```bash
cargo install --path . --locked --force
which mempal
mempal doctor --format json
mempal release-readiness --format json
mempal status
```

`doctor` 和 `release-readiness` 都是只读检查。它们不发布、不联网、不修改数据库。

## MCP

```bash
mempal serve --mcp
```

升级 binary 后，重启 MCP 客户端。

常见 MCP tools（节选，当前共 23 个，完整列表见 `mempal_status` 或 `--help`）：

```text
mempal_status
mempal_search
mempal_context
mempal_brief
mempal_doctor
mempal_knowledge_cards
mempal_phase3
mempal_cowork_bus
```

## 记忆写入与检索

```bash
mempal ingest ./notes --wing mempal
mempal search "architecture decision" --wing mempal
mempal search "knowledge card" --wing mempal --with-neighbors
mempal wake-up
```

回答历史决策时优先使用 search，并保留 `drawer_id` 和 `source_file`。

## 知识图谱与隧道

```bash
mempal kg add "subject" "predicate" "object" --source-drawer drawer_...
mempal kg query --subject "subject" --all
mempal kg timeline "entity"
mempal kg list

mempal tunnels add --left drawer_... --right drawer_... --label "related-decision"
mempal tunnels list --wing mempal --kind all
mempal tunnels follow --from drawer_... --hops 1
```

`kg` 三元组是手动 CRUD（add/query/timeline/stats/list）；隧道用于显式声明跨 wing 的链接，search 时会把命中 drawer 的 tunnel hints 内联返回。

## 重建索引

```bash
mempal reindex --stale
mempal reindex --dry-run
```

`reindex --stale` 只重建 `normalize_version` 落后于当前版本的 drawer，让旧 memory 跟随新的归一化规则重新进入索引；`--dry-run` 只报告将要处理的 source，不写入。

## Context 与 Brief

```bash
mempal context "current task" --format plain
mempal context "current task" --include-cards --format json
mempal brief "current task" --format json
```

`--include-cards` 是 card-aware context。是否默认开启取决于 readiness/proposal/control，不应假设永远开启。

## Knowledge

```bash
mempal knowledge distill \
  --tier dao_ren \
  --statement "A stable domain rule..." \
  --content "Evidence-backed rationale, scope, and examples." \
  --supporting-ref drawer_...

mempal knowledge gate drawer_... --format json
mempal knowledge promote drawer_... \
  --status promoted \
  --verification-ref drawer_... \
  --reason "verified by linked evidence"

mempal knowledge demote drawer_... \
  --status demoted \
  --evidence-ref drawer_... \
  --reason "contradicted by newer evidence" \
  --reason-type contradicted
```

`distill` 产生 candidate。`gate` 是 read-only readiness check。`promote` 和 `demote` 是 lifecycle action，应带 evidence refs。

## Knowledge Card

```bash
mempal knowledge-card create \
  --statement "..." \
  --content "Evidence-backed rationale, scope, and examples." \
  --tier dao_ren \
  --status candidate \
  --anchor-id repo:/path/to/repo \
  --format json

mempal knowledge-card link card_... drawer_... --role supporting
mempal knowledge-card gate card_... --format json
mempal knowledge-card promote card_... \
  --status promoted \
  --verification-ref drawer_... \
  --reason "verified by linked evidence" \
  --enforce-gate \
  --format json

mempal knowledge-card demote card_... \
  --status demoted \
  --evidence-ref drawer_... \
  --reason "contradicted by newer evidence" \
  --reason-type contradicted \
  --format json

mempal knowledge-card retrieve "query" --format json
```

cards 是 governed beliefs。默认 search 仍是 drawer-based，card retrieval 和 card context 是独立 surface。

## Phase 3 Adoption

```bash
mempal phase3 adoption guidance --format plain
mempal phase3 adoption capture --surface card-context --outcome accepted --query "task" --execute
mempal phase3 adoption review --format json
mempal phase3 adoption analytics --format json
mempal phase3 readiness card-context-default --format json
mempal phase3 rollback-control card-context --format json
```

record/capture 类命令涉及写入 adoption evidence。带 `--execute` 才执行；不带时通常应视为 plan/dry-run 或指导路径。

Evaluator advisory：

```bash
mempal phase3 evaluator advise \
  --subject-kind card \
  --subject-id card_... \
  --proposed-action promote \
  --evidence-ref drawer_... \
  --format json
```

advisory 不能直接 promotion。

## Research

```bash
mempal phase3 research-validate-plan report.json --format json
mempal phase3 research-ingest-plan report.json --format json
mempal phase3 research-ingest-plan report.json --execute --format json
```

research output 只能进入 evidence 或 candidate insight，不能直接定义 `dao`。

## Cowork

```bash
mempal cowork-install-hooks --global-codex
mempal cowork-status --cwd "$PWD"

mempal cowork-register --cwd "$PWD" --agent-id claude-main --tool claude
mempal cowork-register --cwd "$PWD" --agent-id codex-a --tool codex
mempal cowork-send --cwd "$PWD" --from claude-main --to codex-a --message "Please review this."
mempal cowork-agent-drain --cwd "$PWD" --agent-id codex-a
mempal cowork-ack --cwd "$PWD" --agent-id codex-a --message-id evt-...

mempal cowork-session-create --cwd "$PWD" --session-id p105 --title "Book writing" --agent claude-main
mempal cowork-handoff --cwd "$PWD" --session-id p105
mempal cowork-session-close --cwd "$PWD" --session-id p105 --capture --execute --format json
```

tmux-backed agent：

```bash
mempal cowork-register \
  --cwd "$PWD" \
  --agent-id codex-a \
  --tool codex \
  --transport tmux \
  --tmux-target mempal:0.1

mempal cowork-tmux-peek --cwd "$PWD" --agent-id codex-a --lines 80
```

runtime cowork 默认不进入 durable memory。使用 `cowork-capture` 或 `cowork-session-close --capture --execute` 才写入 evidence。

## Maintenance

```bash
mempal maintenance-runbook --format plain
mempal maintenance guided-run --format json
```

维护命令输出推荐步骤，不是后台 daemon。

## 本附录来源

本附录依据 `mempal --help` 的 CLI 子命令定义、`AGENTS.md` 的 MCP 工具清单，以及正文第 4、6、7、8、9、10 章涉及的命令路径整理。
