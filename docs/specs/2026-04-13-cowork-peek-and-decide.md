# mempal cowork: peek-and-decide — 设计文档

**日期**：2026-04-13
**状态**：Draft — awaiting review
**前置**：`docs/specs/2026-04-08-mempal-design.md`（mempal v1 总体设计）
**关联 spec**（待创建）：`specs/p6-cowork-peek-and-decide.spec.md`

## 一句话定位

> 让 Claude Code 和 Codex 两个 coding agent 在 mempal 上协作：**live 上下文通过读对方的 session 文件获取（不进 mempal），只有决策定论才写入 mempal drawer**。

## 动机

当前跨 agent 协作有两条既有路径，各自都有根本性问题：

| 方案 | 问题 |
|------|------|
| `~/.cowork/bridge/{claude,codex}-latest.md` + `cowork-sync/handoff` skill | 纯文本不可检索；完全手动触发；两边状态容易不同步 |
| 让 agent 把对话往 mempal 里直接 `mempal_ingest` | mempal 变成"会话副本"，低信号密度污染长期记忆，违反现有 Rule 4 的"只存决策"精神 |

真正需要的是三件事同时成立：
- **A. 知道 partner 此刻在干啥**（live 工作状态）
- **B. 决策能跨 agent 接续**（crystallized 长期记忆）
- **C. 多项目并行不互相污染**（隔离）

## 核心洞察：CoALA 认知分层

这版设计的关键在于区分两类记忆：

| 层 | 内容 | 持久性 | 来源 | 价值密度 |
|----|------|--------|------|----------|
| **L0 Working memory** | 此刻在对话里讨论的那串上下文 | 瞬时 | session 文件已经存在 | 低 |
| **L1 Semantic memory** | "我们决定用 X" 类的结论 | 持久 | mempal drawer | 高 |

**错误的前一版思路**：把 L0 往 L1 里塞（每次 ingest 都是未沉淀的对话状态 → mempal 变成对话副本）。
**这一版的修正**：L0 永远只留在 session 文件里，通过 "peek" 按需读一眼；L1 只在决策定论时才结晶一条。

这条洞察和 mempal 现有的 Rule 4（"每次 commit 后调 `mempal_ingest` 存**决策**记忆"）方向完全一致，只是把它从单 agent 推广到 cowork 场景。

## 架构决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| live 上下文获取方式 | 直接读 session .jsonl 文件 | 两边都已经在写这些文件，零新存储，永远最新 |
| 存入 mempal 的内容 | 只存"决策定论" | 保持 mempal 的高信号密度，避免低价值对话污染 |
| 是否引入 task slug 系统 | **不引入** | Session peek + decision-only ingest 不需要任务标识 |
| 是否引入 inbox/claim/dispatch | **不引入** | peek + 决策捕获已覆盖协作所需，YAGNI |
| 多项目隔离机制 | 沿用 `wing = basename(cwd)` | 与现有 drawer 分布一致，零新约定 |
| 实现形式 | mempal 新增一个 MCP 工具 `mempal_peek_partner` + 两条协议规则更新 | 自描述协议 → 任何 MCP 客户端零配置接入 |
| 默认 peek 窗口 | 最近 30 条 user+assistant 消息 | 固定条数比固定时间可预测，比 token 计数实现简单 |
| "决策定论"触发 | commit 触发（默认）+ agent 主动探询（补充） | 沿用 Rule 4 的 commit 语义 + 对无 commit 的讨论留兜底 |
| 调用方识别（`auto` 模式） | MCP `ClientInfo.name`（缺失时要求显式 `tool` 参数） | 使用 MCP 协议标准字段，fallback 不崩 |

## 新增 MCP 工具：`mempal_peek_partner`

### 参数

```
tool      : "claude" | "codex" | "auto"     必填，"auto" 需 ClientInfo 可用
project   : string                           可选，默认 basename(cwd)
limit     : int                              可选，默认 30（条消息）
since     : RFC3339 timestamp                可选
```

### 返回值

```json
{
  "partner_tool": "codex",
  "session_path": "/Users/.../codex/sessions/2026/04/13/rollout-...-<uuid>.jsonl",
  "session_mtime": "2026-04-13T14:22:08Z",
  "partner_active": true,
  "messages": [
    {"role": "user",      "at": "2026-04-13T14:20:11Z", "text": "..."},
    {"role": "assistant", "at": "2026-04-13T14:20:45Z", "text": "..."}
  ],
  "truncated": false
}
```

- `messages` 按时间升序
- 只保留 user+assistant 文本内容，过滤掉 tool-use 内部结构
- `partner_active = true` 表示 `session_mtime` 在最近 30 分钟内
- `truncated = true` 表示有消息被 `limit` 截断

### 行为

1. **定位 session 文件**（见下"实现细节"）
2. **解析 JSONL**，按 `tool` 类型调对应 adapter
3. **过滤 + 截断**：去掉 tool-use 噪音，按 `since` 过滤，取最后 `limit` 条
4. **返回**

工具**不会**把读到的内容写进 mempal。peek 是纯读操作。

## 协议规则更新（MEMORY_PROTOCOL）

### Rule 8（新增）— PARTNER AWARENESS

> When the user references the partner agent ("Codex 那边...", "ask Claude about...", "partner is working on..."), call `mempal_peek_partner` to read the partner's live session rather than searching mempal drawers.
>
> **Live conversation is transient and stays in session logs, not mempal.** Use peek for *current state*; use `mempal_search` for *crystallized past decisions*. Don't conflate the two.

### Rule 9（替换现有 Rule 4 的 cowork 部分）— DECISION CAPTURE

> `mempal_ingest` is for decisions, not chat logs. A drawer-worthy entry is one where the user (and you, optionally with partner agent input via peek) have reached a firm conclusion:
> - ✓ architectural choice ("we're going with Arc<Mutex<_>>")
> - ✓ naming / API contract
> - ✓ bug root cause + patch
> - ✓ spec acceptance / change
> - ✗ brainstorming scratchpad
> - ✗ intermediate exploration
> - ✗ raw conversation log
>
> When the decision was shaped by partner involvement (you called `mempal_peek_partner` this turn), include partner's key points in the drawer `body` so the drawer is self-contained without re-peeking. Cite the partner session file path in `source_file` alongside your own citation.

### Rule 4（原规则保留，语义细化）

> After a successful `git commit`, call `mempal_ingest` with the commit message and the key decision that the commit embodies. This is the **primary automated trigger** for decision capture. If the commit was informed by partner peek this turn, follow Rule 9's dual-perspective format.

## 实现细节

### `ClaudeSessionReader`

```rust
// 定位
let encoded = cwd.to_string_lossy().replace('/', "-");
let project_dir = home_dir.join(".claude/projects").join(&encoded);
let latest = glob(&format!("{project_dir}/*.jsonl"))
    .max_by_key(|p| mtime(p))?;

// 解析：JSONL 每行是一个 entry
// 关键 entry 形态：
//   {"type": "permission-mode", ...}              → skip
//   {"parentUuid": ..., "attachment": {...}, ...} → message link
// message 内容在 attachment.content 或 embedded stringified JSON
// 过滤只保留 user/assistant text，丢弃 tool-use 内部结构
```

**已通过目测验证**：
`~/.claude/projects/-Users-zhangalex-Work-Projects-AI-mempal/6e0ce37c-ae2e-42b8-b36b-0f4b85225e3d.jsonl`
存在且格式正确。

### `CodexSessionReader`

```rust
// 定位
let base = home_dir.join(".codex/sessions");
// 扫最近 N 天（例如 7 天）的子目录
let files = walkdir(&base, depth=4)
    .filter(|f| f.ends_with(".jsonl"))
    .filter(|f| session_cwd_matches(f, &cwd))   // 从 jsonl header 读 cwd metadata
    .max_by_key(|f| mtime(f))?;
```

**已通过目测验证**：
`~/.codex/sessions/2026/03/03/rollout-2026-03-03T05-57-57-<uuid>.jsonl` 等文件存在，格式为 JSONL。

如果 Codex jsonl header 里没有 cwd 字段，fallback 策略：读最新的那一个，由调用方 agent 自己判断是否相关（由协议规则约束 agent 行为）。

### Project 匹配

- **Claude**：encoded path 直接反解 = cwd 字符串匹配
- **Codex**：jsonl header 的 cwd/working_dir 字段匹配 basename
- **两者 fallback**：如果带 `project` 参数，仅按 basename 比对；否则用调用方 cwd basename

### `ClientInfo.name` 识别

```rust
// MCP initialize 握手时拿到
impl McpServer {
    fn on_initialize(&mut self, params: InitializeParams) {
        self.client_name = params.client_info.name;  // "claude-code" | "codex" | ...
    }
}

// peek 时使用
fn peek_partner(&self, req: PeekRequest) -> Result<PeekResponse> {
    let target = match req.tool {
        Tool::Auto => self.infer_partner(),  // 根据 client_name 推 partner
        Tool::Claude | Tool::Codex => req.tool,
    };
    // ...
}

fn infer_partner(&self) -> Result<Tool> {
    match self.client_name.as_deref() {
        Some(s) if s.contains("claude") => Ok(Tool::Codex),
        Some(s) if s.contains("codex")  => Ok(Tool::Claude),
        _ => Err("cannot infer partner; pass `tool` explicitly"),
    }
}
```

## 使用场景走查

| 剧本 | 之前 slug 方案下的坑 | 这版怎么化解 |
|------|---------------------|--------------|
| 1. "开始做 p5 wake up importance" | slug 派生 OK | peek 不需要 slug；decision-only ingest 直接按 `wing=mempal` 写 |
| 2. "wake up importance 那个 task 继续" | 要 fuzzy match spec 文件 → slug | 无 slug 系统。peek 直接拉 partner 最新 session，看对方在做什么 |
| 3. "修 parser 那个诡异 panic"（无 spec） | 无稳定 slug 来源 | 不需要 slug。debug 过程留在 session 文件里；root cause + fix 定论后走 Rule 9 ingest |
| 4. 任务中途切出去问别的 | 污染 task room | 无 task room。peek 和 ingest 是正交的，切话题不影响任何存储 |
| 5. 任务内问历史决策 | "task scope 活跃时要不要用全局 search" | `mempal_search` 语义不变，永远全 wing；peek 只给 live 视图。职责分明 |
| 6. 任务结束归档 | 没有关单动作 | 无 task 概念。session 文件归档由 Claude Code / Codex 各自管；mempal 只看 drawer 时间分布 |

## 风险和限制

| 风险 | 缓解 |
|------|------|
| Claude Code / Codex jsonl 格式升级 | adapter 解耦 + 版本检测 + 明确报错；两边同等风险 |
| 单机多用户场景下 peek 越权读对方 session | v1 限定单用户同进程用户 UID。v2 若多租户需补访问控制 |
| 大 jsonl 全读成本 | 从尾部反向读取，达到 `limit` 或 `since` 截止 |
| partner 没在跑 | `partner_active = false`，仍返回最近一次完成 session 的内容 |
| ClientInfo 缺失 | `auto` 模式报错要求显式 `tool`，不崩 |
| Codex jsonl 没 cwd 字段 | 目测后再决定是按"最近即可"还是要求显式 `project` 参数 |

## 不做（YAGNI）

明确排除的复杂性，避免 scope creep：

- **任务 slug 系统**：没有 wing/room 约定、没有 spec 文件 fuzzy match、没有任务 lifecycle
- **Async dispatch / inbox / claim**：peek 已覆盖 "知道 partner 在干啥" 的需求，投递任务靠用户在两个 session 间切换即可
- **实时推送/事件总线**：agent 只在自己的 turn 有心跳，实时推送没意义
- **session 内容写入 mempal**：明令禁止。违反 L0/L1 分层
- **多用户/多租户访问控制**：v1 单用户本地使用
- **跨语言 session adapter**（如 Cursor、Continue 等第三方 client）：等有人实际用时再加

## 成本估算

| 工作项 | 估时 |
|--------|------|
| `mempal_peek_partner` MCP 工具 schema + 入口 | 0.2 day |
| `ClaudeSessionReader` adapter | 0.2 day |
| `CodexSessionReader` adapter | 0.2 day |
| ClientInfo 握手支持 + `auto` 推断 | 0.1 day |
| 协议 Rule 8 / Rule 9 字符串更新 + 测试 | 0.1 day |
| 集成测试（两边真实 jsonl fixtures） | 0.2 day |
| 文档更新（README、usage、CLAUDE.md） | 0.1 day |
| **总计** | **约 1.1 day** |

代码量估算：约 400-600 行 Rust（tool schema 50 + 两个 adapter 各 150 + ClientInfo hook 50 + tests 150）。

**无 schema 迁移**，`palace.db` 继续 v4。

## 后续工作

- [ ] 本 design 被用户 approved
- [ ] 写 agent-spec task contract：`specs/p6-cowork-peek-and-decide.spec.md`
- [ ] 写实现计划：`docs/plans/2026-04-13-p6-implementation.md`
- [ ] 验证 Codex jsonl header 里的 cwd 字段（决定 project 匹配策略）
- [ ] 验证两边 assistant 消息在 jsonl 里的具体 schema（决定 adapter 过滤规则）
- [ ] （实现后）更新 `src/mcp/server.rs` 里的 `MEMORY_PROTOCOL` 常量
- [ ] （实现后）更新 README 的 cowork 章节

## 开放问题（非阻塞）

1. `mempal_peek_partner` 要不要带 `search` 参数（在读 session 的同时按关键词过滤）？
   **初步答案**：v1 不加，让 agent 自己在返回的 messages 上做语义过滤。避免 adapter 层塞太多语义。

2. Peek 结果要不要缓存？同一 turn 内两次 peek 是否复用？
   **初步答案**：不缓存。session 文件在写入，缓存引入一致性复杂度。

3. Codex 的 jsonl 里如果有 assistant streaming 的部分帧，要合并成一条还是保留每帧？
   **初步答案**：合并成完整 assistant turn。streaming 细节是 tool 内部实现，agent 只关心 "partner 说了什么"。
