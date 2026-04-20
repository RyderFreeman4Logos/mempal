spec: task
name: "P8: Crash-safe SQLite pending-message queue with Claim-Confirm + exponential backoff"
tags: [feature, queue, sqlite, concurrency, infrastructure]
estimate: 2d
---

## Intent

为 hook-based passive capture（P8 `p8-hook-passive-capture.spec.md`）提供底层的 **crash-safe 持久化队列**：hook 侧只负责把事件快速 `enqueue` 到 `pending_messages` SQLite 表并立即返回；后台 worker 以 **Claim-Confirm 模式**取消息、处理完 `confirm` 删除，处理失败走**指数退避**重试，进程崩溃时未 `confirm` 的消息在超时后自动回滚到 `pending` 状态不丢失。

**动机**：claude-mem 源码 `src/services/sqlite/PendingMessageStore.ts:98-251` 实际依赖这个队列作为 auto-capture 的心脏——没有它，高频 hook 写入直接打到主存储会触发 SQLite 锁竞争导致 `database is locked` 错误，且进程崩溃会丢消息。Rust 版完全可以复现，并且 `rusqlite` + WAL 模式下并发优于 Bun 默认 better-sqlite3。

**v3 判决依据**：这是 v2 分析中"claude-mem 值得吸收但 7 个 issue 没覆盖"的特性第 1 项（v2 报告 line 231-235），`PendingMessageStore.ts:175-251` 的 Claim-Confirm + retry_count 指数退避机制。

## Decisions

- 新建 `crates/mempal-core/src/queue.rs` 模块，暴露 `PendingMessageStore` struct
- 新建 schema migration，引入 `pending_messages` 表（字段见下），bump `CURRENT_SCHEMA_VERSION` v4 → v5
- 表结构：
  ```sql
  CREATE TABLE pending_messages (
      id TEXT PRIMARY KEY,              -- ULID
      kind TEXT NOT NULL,               -- 'hook_event' | 'bulk_import' | etc.
      payload TEXT NOT NULL,            -- JSON 原文
      status TEXT NOT NULL,             -- 'pending' | 'claimed' | 'failed'
      claimed_at INTEGER,               -- unix seconds (NULL if pending)
      claimed_by TEXT,                  -- worker id (NULL if pending)
      retry_count INTEGER NOT NULL DEFAULT 0,
      next_attempt_at INTEGER NOT NULL, -- unix seconds; indexed
      last_error TEXT,                  -- latest error message
      created_at INTEGER NOT NULL,
      CHECK (status IN ('pending','claimed','failed'))
  );
  CREATE INDEX idx_pending_next_attempt ON pending_messages(status, next_attempt_at);
  ```
- 公共 API：
  - `enqueue(kind: &str, payload: &str) -> Result<String>` 返回新消息 ULID，写入时 `status='pending'`, `next_attempt_at=now`
  - `claim_next(worker_id: &str, claim_ttl_secs: i64) -> Result<Option<ClaimedMessage>>` 原子更新（SELECT + UPDATE in txn）一条 `status='pending' AND next_attempt_at <= now` 的消息，标为 `claimed`
  - `confirm(id: &str) -> Result<()>` 处理成功，DELETE 该行
  - `mark_failed(id: &str, error: &str) -> Result<()>` 处理失败，`retry_count += 1`，`next_attempt_at = now + backoff(retry_count)`，`status='pending'`（重新可领取）；超过 `max_retries` 时 `status='failed'` 永久保留供审计
  - `reclaim_stale(older_than_secs: i64) -> Result<u64>` 把 `claimed` 但 `claimed_at < now - older_than_secs` 的消息回滚到 `pending`（进程崩溃恢复）
- 指数退避公式：`backoff(n) = min(base_delay_secs * 2^n, max_delay_secs)`，默认 `base=5`, `max=3600`, `max_retries=10`
- Claim atomicity：用单一 `UPDATE ... WHERE id = (SELECT id FROM ... ORDER BY next_attempt_at LIMIT 1) RETURNING *` 或等价 txn 保证无两 worker 抢同一消息
- SQLite 必须在 WAL 模式（`PRAGMA journal_mode=WAL`）。**事实核查**：截至 P7，`crates/mempal-core/src/db.rs` 的 `Database::open` 仅设 `PRAGMA foreign_keys=ON`，**未**启用 WAL。本 spec 同时在 `Database::open` 初始化路径追加 `PRAGMA journal_mode=WAL` 和 `PRAGMA synchronous=NORMAL`（后者是 WAL 下的推荐性能档：崩溃保 WAL 不保最后一次 commit 到磁盘，写入并发显著提升）；不追加 WAL 就谈不上队列无锁竞争
- `rusqlite` 作为同步接口，但队列操作包在 `tokio::task::spawn_blocking` 内给异步调用方；不引入 `sqlx`
- 启动时调一次 `reclaim_stale(claim_ttl_secs)` 自动恢复上次崩溃的 claimed 消息
- 队列 stats 通过 `stats() -> QueueStats { pending, claimed, failed, oldest_pending_age_secs }` 暴露给 `mempal status`
- 错误类型：`QueueError`（`thiserror::Error`），含 `DatabaseError`、`SerializationError`、`MaxRetriesExceeded`、`MessageNotFound`
- **不**引入 inter-process message bus（NATS/Redis/ZeroMQ）——全走 SQLite
- **不**用 rusqlite 的 `busy_timeout` 逃避锁竞争——用 WAL + 精细事务隔离
- 所有公共方法是 `&self`（内部用连接池或连接 handle），方便 `Arc<PendingMessageStore>` 共享

## Boundaries

### Allowed
- `crates/mempal-core/src/queue.rs`（新建）
- `crates/mempal-core/src/lib.rs`（`pub mod queue` + re-export）
- `crates/mempal-core/src/db/schema.rs` 或等价（migration v4 → v5 + `pending_messages` DDL）
- `crates/mempal-core/src/db.rs` 或 `db/mod.rs`（`Database::open` 追加 `PRAGMA journal_mode=WAL` + `PRAGMA synchronous=NORMAL`；启动时调 `reclaim_stale`）
- `crates/mempal-core/Cargo.toml`（添加 `thiserror`、`ulid` workspace dep，若尚未有）
- `crates/mempal-cli/src/main.rs`（`mempal status` 加队列 stats 行）
- `tests/queue_crash_safety.rs`（新建集成测试文件）
- `tests/queue_concurrency.rs`（新建）

### Forbidden
- 不要把 `pending_messages` 挂到 `drawers` / `drawer_vectors` / `triples` 以外的既有表上
- 不要用 `sqlx`、`diesel`、`sea-orm` 替代 `rusqlite`
- 不要引入 Redis / NATS / 外部 broker
- 不要在 `mempal-search` / `mempal-aaak` / `mempal-api` / `mempal-mcp` 里直接读写 `pending_messages`——只通过 `PendingMessageStore` API
- 不要在 `claim_next` 实现里用 `SELECT` 然后 Rust 侧循环逐条 update——必须原子 SQL
- 不要给 `claim_next` 的返回值 `ClaimedMessage` 加 `Drop` 自动 confirm/fail——明确 API 语义
- 不要实现"消息优先级"字段（YAGNI；`next_attempt_at` 已经够用）
- 不要暴露"暂停队列"开关——队列一直可用

## Out of Scope

- 分布式多 worker 跨进程协作（claim_by 用于审计但不做 worker registry）
- 消息大 payload 分片（payload > 10MB 视为调用方 bug，store 不做 chunking）
- 消息 TTL 自动过期（`status='failed'` 永久保留，清理是独立的 `mempal queue purge --older-than` 工具，不在此 spec）
- 队列 metrics 上报到 Prometheus / OTel
- 多队列（per-wing queue）——全局单队列
- GUI 监控（见 P10 `mempal timeline` / `mempal audit` 子命令）

## Completion Criteria

Scenario: enqueue 后 claim_next 能取到相同消息
  Test:
    Filter: test_enqueue_then_claim_returns_same_payload
    Level: unit
    Targets: crates/mempal-core/src/queue.rs
  Given 一个空队列的 `PendingMessageStore`
  When 调 `store.enqueue("hook_event", "{\"tool\":\"Bash\"}")`
  And 然后调 `store.claim_next("worker-1", 60)`
  Then 返回 `Some(ClaimedMessage)`
  And `msg.kind == "hook_event"`
  And `msg.payload == "{\"tool\":\"Bash\"}"`
  And `msg.retry_count == 0`

Scenario: claim 后直接再 claim 不会重复领同一条
  Test:
    Filter: test_claim_is_exclusive
    Level: unit
    Targets: crates/mempal-core/src/queue.rs
  Given 队列只有 1 条 pending 消息
  When worker-A 调 `claim_next("worker-A", 60)`
  And worker-B 调 `claim_next("worker-B", 60)`
  Then worker-A 得到 `Some(msg)`
  And worker-B 得到 `None`

Scenario: confirm 后消息从 DB 删除
  Test:
    Filter: test_confirm_deletes_row
    Level: unit
    Targets: crates/mempal-core/src/queue.rs
  Given 一条被 claim 的消息
  When 调 `store.confirm(&msg.id)`
  Then `pending_messages` 表中该 id 不存在
  And `store.stats().pending + claimed + failed == 0`

Scenario: mark_failed 指数退避
  Test:
    Filter: test_mark_failed_sets_backoff_next_attempt
    Level: unit
    Targets: crates/mempal-core/src/queue.rs
  Given 一条被 claim 的消息，`retry_count == 0`，`base_delay_secs == 5`
  When 调 `store.mark_failed(&msg.id, "timeout")`
  Then 该行 `retry_count == 1`
  And `next_attempt_at >= now + 5` 且 `< now + 15`（考虑测试延迟）
  And `status == 'pending'`（可重新被 claim）
  And `last_error == "timeout"`

Scenario: reclaim_stale 把崩溃 worker 的 claim 回滚
  Test:
    Filter: test_reclaim_stale_rolls_back_expired_claims
    Level: integration
    Targets: crates/mempal-core/src/queue.rs
  Given 一条被 claim 的消息，`claimed_at = now - 120`
  When 调 `store.reclaim_stale(60)`
  Then 返回值 == "1"
  And 该行 `status == 'pending'` 且 `claimed_at IS NULL`
  And 该行 `retry_count` 未变

Scenario: 超过 max_retries 后状态变为 failed 永久保留
  Test:
    Filter: test_max_retries_marks_failed_permanently
    Level: unit
    Targets: crates/mempal-core/src/queue.rs
  Given `max_retries == 3`，一条消息连续 claim + mark_failed 4 次
  When 第 4 次 mark_failed 执行完
  Then 该行 `status == 'failed'`
  And 调 `store.claim_next(...)` 返回 `None`（failed 不可再被 claim）
  And 该行仍存在于 `pending_messages` 表（审计保留）

Scenario: 启动时 reclaim_stale 被自动调用
  Test:
    Filter: test_store_startup_auto_reclaims_stale
    Level: integration
    Targets: crates/mempal-core/src/queue.rs, crates/mempal-core/src/db/mod.rs
  Given 一个已有 1 条 `claimed` 消息（`claimed_at = now - 3600`）的 palace.db
  When 新开一个 `PendingMessageStore::new(db)`
  Then 该消息 `status == 'pending'` 自动恢复

Scenario: 并发 enqueue 在 WAL 模式下不阻塞
  Test:
    Filter: test_concurrent_enqueue_does_not_block
    Level: integration
    Targets: crates/mempal-core/src/queue.rs
  Given 1 个 `PendingMessageStore`，8 个 tokio task 各 enqueue 100 条消息
  When 所有 task `join` 完
  Then 队列中恰好有 "800" 条 pending
  And 整体耗时 < 5 秒（naive upper bound，表明无严重锁竞争）

Scenario: schema 迁移 v4 → v5 创建 pending_messages 表
  Test:
    Filter: test_migration_v4_to_v5_creates_pending_messages_table
    Level: integration
    Targets: crates/mempal-core/src/db/schema.rs
  Given 一个 schema_version == "4" 的 palace.db
  When 启动 mempal 触发迁移
  Then `schema_version` == "5"
  And `sqlite_master` 查询 `name='pending_messages'` 返回 1 行
  And `idx_pending_next_attempt` 索引存在

Scenario: 队列 stats 通过 mempal status 展示
  Test:
    Filter: test_status_command_shows_queue_stats
    Level: integration
    Targets: crates/mempal-cli/src/main.rs
  Given 队列含 3 pending、1 claimed、0 failed
  When 运行 `mempal status`
  Then stdout 含 `queue:` 行，且 `pending=3 claimed=1 failed=0`

Scenario: 无 `unwrap` 调用
  Test:
    Filter: test_queue_module_no_unwrap
    Level: static
    Targets: crates/mempal-core/src/queue.rs
  Given `crates/mempal-core/src/queue.rs` 源文件
  When grep `\.unwrap\(\)` 除 test 模块之外
  Then 零匹配（所有错误用 `?` 或 `thiserror` 映射）
