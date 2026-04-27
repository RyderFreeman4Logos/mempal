spec: task
name: "P10: explicit cross-wing tunnels — agent-created semantic links alongside passive room-name discovery"
tags: [feature, mcp, tunnels, kg, cross-project]
estimate: 1.0d
---

## Intent

借鉴 mempalace 1b4ce0b (`create_tunnel` / `list_tunnels` / `follow_tunnels` in `palace_graph.py`)，给 mempal 已有的**被动** tunnel 发现机制（SQL `GROUP BY room HAVING COUNT(DISTINCT wing) > 1`）补充**主动**显式建链能力。

当前痛点：mempal 的 tunnel 只识别"两个 wing 有同名 room"这一种耦合。真实多项目场景里，agent 知道"mempal 的 auth 决策 ↔ robrix2 的 matrix 认证"这种**语义**链接，但 room 名字不同，被动发现永远看不到。

核心用户价值：**让 agent 把多项目知识显式连成网**——跨 wing 的 "这个和那个相关" 以机器可查询形式存下来，下次 search 可以沿 tunnel 跳到另一个 wing 的相关 room。

和现有 tunnels 共存：`mempal_tunnels` MCP 工具同时返回被动发现 + 显式 tunnel，两种 tunnel 类型在 response 中打 `kind: "passive" | "explicit"` 标签。

## Decisions

- **Schema v6**：新增 `tunnels` 表（**不**删被动发现路径）。P12 已占用 schema v5（typed drawers），因此本 spec 使用 `CURRENT_SCHEMA_VERSION: 5 → 6`：
  ```sql
  CREATE TABLE IF NOT EXISTS tunnels (
    id TEXT PRIMARY KEY,              -- sha256_hex(sorted endpoints)[..16]，复用现有 sha2 依赖
    left_wing TEXT NOT NULL,
    left_room TEXT,                   -- NULL = whole wing
    right_wing TEXT NOT NULL,
    right_room TEXT,
    label TEXT NOT NULL,              -- agent-provided description
    created_at TEXT NOT NULL,         -- RFC3339 UTC
    created_by TEXT,                  -- Optional: "claude-code" / "codex" / MCP client name
    deleted_at TEXT                   -- soft delete, mirror drawer pattern
  );
  CREATE INDEX idx_tunnels_left ON tunnels(left_wing, left_room) WHERE deleted_at IS NULL;
  CREATE INDEX idx_tunnels_right ON tunnels(right_wing, right_room) WHERE deleted_at IS NULL;
  ```
- **去重**：同一对 endpoint（无序）只能存一条。`id` 用排序后的 4-tuple SHA-256 hash 保证 `(A, B)` 和 `(B, A)` 映射到同一 id，INSERT OR IGNORE 去重
- **`mempal_tunnels` MCP 工具扩展**（不新增工具，扩展现有）：
  - 当前 action: `"discover"`（已存在的被动发现）
  - 新增 action: `"add"` / `"list"` / `"delete"` / `"follow"`
  - Request schema: `TunnelsRequest { action: String, ..fields depending on action }`
- **新增 action: `add`** — payload: `{ left: {wing, room?}, right: {wing, room?}, label }` → response: `{ tunnel_id, created_at }`；self-link（left == right）拒绝
- **新增 action: `list`** — 可选 filter: `{ wing?: String, kind?: "passive"|"explicit"|"all" }` → response: `Vec<TunnelEntry>`，`TunnelEntry.kind` 区分来源
- **新增 action: `delete`** — payload: `{ tunnel_id }` → soft-delete（`deleted_at = now`），被动发现 tunnel 删不掉（返回错误）
- **新增 action: `follow`** — payload: `{ from: {wing, room?}, max_hops: 1|2 }` → 返回从该 room 出发可达的 `(wing, room, via_tunnel_id, hop)` 列表，hop=1 是直接相邻，hop=2 是穿一个中间 room。不做全图遍历（工具级 safety）
- **`mempal_search` 联动**：search 结果的 `tunnel_hints` 字段同时汇入 passive + explicit tunnels，调用方无感（透明融合）
- **CLI 子命令**：
  - `mempal tunnels add --left w1:r1 --right w2:r2 --label "..."`
  - `mempal tunnels list [--wing w] [--kind passive|explicit|all]`
  - `mempal tunnels delete <id>`
  - `mempal tunnels follow --from w:r [--hops 1|2]`
- **CLI graceful**：delete 不存在的 id 报 `TunnelError::NotFound` + exit 1
- **Protocol Rule 更新**：Rule 3 "QUERY WHEN UNCERTAIN" 尾部补一句"也可以用 `mempal_tunnels` with `action:list` 发现跨项目相关 room"
- **Migration**：schema v5 → v6 在 db open 时自动执行 `CREATE TABLE IF NOT EXISTS tunnels ...`；旧 palace 打开后显式 tunnels 表空，行为完全向后兼容
- **不破坏被动发现**：`db.find_tunnels()`（现有）保留，被动发现逻辑不动

## Boundaries

### Allowed
- `src/core/db.rs`（新增 `create_tunnel` / `list_tunnels` / `delete_tunnel` / `follow_tunnels`；schema v5 migration）
- `src/core/schema.sql`（CREATE TABLE tunnels）
- `src/mcp/tools.rs`（`TunnelsRequest` action enum 扩展）
- `src/mcp/server.rs`（`mempal_tunnels` handler 增加 add/list/delete/follow 分支）
- `src/search/mod.rs`（`tunnel_hints` 融合显式 tunnel）
- `src/cli.rs`（tunnels add/list/delete/follow 子命令）
- `src/core/protocol.rs`（Rule 3 补充）
- `tests/tunnels_explicit.rs`（新增集成测试）

### Forbidden
- 不删除或改写 `db.find_tunnels()`（被动发现）
- 不改 `drawers` / `drawer_vectors` / `triples` 表 schema
- 不改 `mempal_search` 的 request/response 字段名（只扩 `tunnel_hints` 内部）
- 不新增 MCP 工具（扩展现有 `mempal_tunnels`，工具总数保持 10 — 若 P9 fact_check 已加则是 10）
- 不让 `add` 路径写 `drawers` / `triples`（tunnels 是独立 graph，不混）
- 不允许 >2 跳 `follow`（safety upper bound）
- 不引新 runtime dep

## Out of Scope

- 带权 tunnel（score / strength）
- Tunnel 自动发现（基于 embedding 相似度）— 留给未来 spec
- Tunnel 可视化（graph 导出）
- 跨用户 tunnel 分享
- Tunnel 的 valid_from/valid_to 时态（可后续补，本 spec 走 soft-delete 足够）
- 给 `mempal_ingest` 加自动创建 tunnel 的能力（agent 主动决定，不自动推断）

## Completion Criteria

Scenario: 显式 add tunnel 后 list 能看到
  Test:
    Filter: test_add_and_list_explicit_tunnel
    Level: integration
  Given tempfile palace.db（schema v5）
  When 调用 `tunnels(action="add", left={wing:"mempal", room:"auth"}, right={wing:"robrix2", room:"matrix-routing"}, label="both handle user auth")`
  And 调用 `tunnels(action="list", wing="mempal", kind="explicit")`
  Then list response 包含 1 条 entry
  And entry.kind == "explicit"
  And entry.label == "both handle user auth"

Scenario: 同一对 endpoint add 两次去重（有序无关）
  Test:
    Filter: test_add_tunnel_dedup_unordered
    Level: unit
  Given add (A=mempal:auth, B=robrix2:matrix)
  When 再次 add (A=robrix2:matrix, B=mempal:auth)
  Then 第二次返回同一个 tunnel_id
  And `SELECT COUNT(*) FROM tunnels` == 1

Scenario: follow 返回一跳邻居
  Test:
    Filter: test_follow_one_hop
    Level: integration
  Given add 两条 tunnel：(mempal:auth ↔ robrix2:matrix) 和 (mempal:auth ↔ octos:login)
  When 调用 `tunnels(action="follow", from={wing:"mempal", room:"auth"}, max_hops=1)`
  Then response.len() == 2
  And response 中有 entry {wing:"robrix2", room:"matrix", hop:1}
  And response 中有 entry {wing:"octos", room:"login", hop:1}

Scenario: follow 两跳穿透中间节点
  Test:
    Filter: test_follow_two_hops
    Level: integration
  Given add (mempal:auth ↔ robrix2:matrix) 和 (robrix2:matrix ↔ hermes-agent:sso)
  When `tunnels(action="follow", from={wing:"mempal", room:"auth"}, max_hops=2)`
  Then response 包含 entry {wing:"hermes-agent", room:"sso", hop:2}

Scenario: delete 显式 tunnel 后 list 不再返回
  Test:
    Filter: test_delete_explicit_tunnel_soft_delete
    Level: integration
  Given add 一条 tunnel 拿到 id
  When 调用 `tunnels(action="delete", tunnel_id=id)`
  And 调用 `tunnels(action="list", kind="explicit")`
  Then list 不包含该 id
  And `SELECT deleted_at FROM tunnels WHERE id = ?` IS NOT NULL

Scenario: delete 被动 tunnel 返回错误
  Test:
    Filter: test_delete_passive_tunnel_rejected
    Level: integration
  Given 两个 wing 都有 room "matrix-routing" — 被动发现返回一条 tunnel
  When `tunnels(action="delete", tunnel_id="<passive-id>")`
  Then response 是 `TunnelError::CannotDeletePassive`

Scenario: search tunnel_hints 融合被动 + 显式
  Test:
    Filter: test_search_tunnel_hints_merges_passive_and_explicit
    Level: integration
  Given 被动发现 1 条 + 显式 add 1 条 tunnel 都涉及 wing=mempal
  When 调用 `mempal_search(query="...", wing="mempal")`
  Then search result 的 `tunnel_hints` 字段长度 == 2
  And 两条都出现（顺序不约束，内容字段检查）

Scenario: schema v5 palace 自动迁移到 v6 且现有数据不丢
  Test:
    Filter: test_schema_v5_to_v6_migration_preserves_data
    Level: integration
  Given 预置 schema v5 的 palace.db with 10 drawers + 5 triples
  When 重新打开 db（触发 migration）
  Then schema_version == 6
  And drawer_count == 10
  And triple_count == 5
  And tunnels 表存在且空

Scenario: self-link (left == right) 拒绝
  Test:
    Filter: test_add_self_tunnel_rejected
    Level: unit
  Given A = {wing:"mempal", room:"auth"}
  When `tunnels(action="add", left=A, right=A, label="x")`
  Then 返回 `Err(TunnelError::SelfLink)`
  And 表中无新行
