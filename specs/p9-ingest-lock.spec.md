spec: task
name: "P9: per-source ingest lock — eliminate duplicate-drawer races for concurrent Claude↔Codex cowork"
tags: [bugfix, concurrency, ingest, cowork, atomicity]
estimate: 0.5d
---

## Intent

借鉴 mempalace 30a4319 (`mine_lock()` in `palace.py`) 的思路，解决 **mempal 多 agent 并发 ingest 同一 source 时的 TOCTOU race**。

Race 场景：Claude Code 和 Codex 同时看到同一个文件需要 ingest（比如 user 同时问两边"把这个 README 存进记忆"），两边都：
1. `check_dedup(source_hash)` — 都返回 `not exists`
2. `delete_existing_by_source(path)` — 竞态；一边可能删掉另一边刚写的 vectors
3. `insert_drawers + vectors` — 两份重复 drawer 或部分丢失

SQLite WAL 给行级原子，但 ingest pipeline 是**多语句的临界区**：dedup check / delete / insert 之间 SQLite 原子性无效。Claude+Codex 作为 mempal 的首要 cowork pair（P6/P8），这个 race 在真实多 agent 场景里**每天都可能发生一次**，后果是 drawer 重复或数据丢失（参考 `drawer_mempal_mempal_mcp_4b55f386 permanently lost` 事件教训）。

核心用户价值：**让 "concurrent ingest same source" 成为安全操作**，两边的 `mempal_ingest` 序列化但不 deadlock。

## Decisions

- **新模块 `src/ingest/lock.rs`**：
  - `pub struct IngestLock { _file: std::fs::File, ... }` — RAII 句柄，drop 时自动释放
  - `pub fn acquire_source_lock(mempal_home: &Path, source_key: &str, timeout: Duration) -> Result<IngestLock, LockError>`
  - `pub enum LockError { Timeout, Io(io::Error), InvalidSourceKey }`
- **Source key 归一化**：`source_key = blake3_hex(normalized_source_file)[..16]` — 短 hex 避免文件名长度限制；blake3 已在 dep 中（如没有则用 std `DefaultHasher` 的 u64 hex，**不引新 crate**）
- **锁文件路径**：`<mempal_home>/locks/<source_key>.lock`（目录在 acquire 时 lazy create，**不**在 ingest pipeline 外自动清理 — 文件长期存在但大小 0，OS level 可接受）
- **实现**：**不引 `fs2` crate**，当前用 std + inline `extern "C"` `flock` (Unix) 的 thin wrapper。macOS/Linux 用 `flock(LOCK_EX | LOCK_NB)` + 重试循环做 timeout；Windows 暂为 no-op fallback，并在代码注释中显式标记 follow-up。**优先**复用项目内已有 dep（查 Cargo.toml：如果 `fs2` / `fd-lock` 已依赖则用之；否则走最薄平台 wrapper）
- **阻塞策略**：`timeout=5s` 默认；超时返回 `LockError::Timeout`；重试间隔 50ms（带 jitter 避免 thundering herd）
- **Ingest pipeline 改造**：`src/ingest/mod.rs` 的 `ingest_file_with_options` 在进入 dedup/insert 临界区**前**拿锁，临界区结束后（`insert` commit 完成后）drop 锁：
  ```rust
  let _guard = acquire_source_lock(&home, &source_key, Duration::from_secs(5))?;
  let exists = db.drawer_exists(&drawer_id)?;
  if exists { return Ok(IngestStats { skipped: 1, .. }); }
  db.insert_drawer(&drawer)?;
  db.insert_vector(&drawer_id, &vector)?;
  // _guard dropped here, releases lock
  ```
- **Re-check after lock**：拿到锁**后**必须重新做一次 `drawer_exists` / dedup 检查，因为锁等待期间另一个 agent 可能已 commit 了同 source 的 drawer。这是 "double-checked locking" 的正确姿势
- **锁粒度 = 归一化 source_file**：同一逻辑 source（比如 `README.md` 从 project root 和 sub-dir 都可能 ingest）归一到相同 key，保证真实世界 race 被覆盖
- **可观测性**：`IngestStats` / `IngestResponse` 追加 `lock_wait_ms: Option<u64>` 字段，帮 agent debug 并发等待情况
- **Ingest 其他路径不加锁**：`dry_run` 不加锁（不写临界区）；`mempal reindex` 走独立全局锁（由另一 spec 决定，本 spec 不涉及）

## Boundaries

### Allowed
- `src/ingest/lock.rs`（新增）
- `src/ingest/mod.rs`（在 `ingest_file_with_options` / `ingest_dir_with_options` 加锁）
- `src/ingest/mod.rs` 的 `IngestStats` 追加 `lock_wait_ms` 字段
- `src/mcp/tools.rs`（`IngestResponse` 追加 `lock_wait_ms: Option<u64>`）
- `tests/ingest_lock.rs`（新增集成测试）
- `Cargo.toml`（如必须则 dev-dep 加 `tempfile`，已有则免）

### Forbidden
- 不加 runtime dependency（无论 `fs2` / `fd-lock` / `file-lock`）— 若仓库现无则走薄 libc/windows-sys wrapper
- 不改 `Db` 的 schema / 表 / 列
- 不改 `db.check_dedup` / `delete_by_source` / `insert_drawers` 的签名
- 不动 P6 `peek_partner` / P8 `cowork-inbox` 相关任何代码
- 不引入 async mutex（纯 std 同步锁足矣，ingest 是 CPU-bound + IO-bound 但临界区短）
- 不破坏现有 `tests/ingest_test.rs` 任何 pass 状态
- 不 bump schema version

## Out of Scope

- 跨机器分布式锁（单机 filesystem 足够）
- 锁文件自动清理（孤儿 `.lock` 文件不影响正确性）
- 细粒度锁（per-drawer 锁太复杂，per-source 足够）
- 死锁检测（单把锁 + 短临界区，不会死锁）
- `mempal reindex` / `mempal purge` 的锁行为（独立 spec）
- 把 SQLite 升级到 BEGIN IMMEDIATE 事务（正交方案，更重）
- MCP 的 `mempal_ingest` 排队 / 限流

## Completion Criteria

Scenario: 两个并发 ingest 同一 source 最终只产生一份 drawer
  Test:
    Filter: test_concurrent_ingest_same_source_single_drawer
    Level: integration
  Given tempdir 作为 mempal_home，空 palace.db
  And 文件 `/tmp/fake-doc.md` 内容 "hello P9"
  When spawn 两个 tokio task 同时调 `ingest_file_with_options(&path, wing="test", ...)`
  Then 两个 task 都返回 Ok（不是错误）
  And `db.drawer_count()` == "1"（不是 2）
  And 恰好一个 task 的 `IngestOutcome.lock_wait_ms` > 0（winner 0ms，loser 等过锁）

Scenario: 不同 source 并发 ingest 不互相阻塞
  Test:
    Filter: test_concurrent_ingest_different_source_no_blocking
    Level: integration
  Given 两个不同文件 `/tmp/a.md` 和 `/tmp/b.md`
  When spawn 两个 task 同时 ingest 各自
  Then 两者 `lock_wait_ms` 都 < 100 ms（不等彼此）
  And drawer_count == "2"

Scenario: 锁超时返回 LockError::Timeout
  Test:
    Filter: test_lock_timeout_returns_error
    Level: integration
  Given 一个 task 拿到锁后持续持有，直到测试显式释放
  When 第二个 task 调 `acquire_source_lock` with timeout=300ms
  Then 第二个 task 返回 `Err(LockError::Timeout)`

Scenario: 锁 release 后下一个 ingest 立即能拿到
  Test:
    Filter: test_lock_released_on_guard_drop
    Level: unit
  Given task A 拿锁后 guard 立即 drop
  When task B 调 `acquire_source_lock` with timeout=100ms
  Then task B 在 100ms 内拿到锁（guard 非空）

Scenario: double-checked locking 防止锁等待期间重复 ingest
  Test:
    Filter: test_double_check_after_lock_skips_duplicate
    Level: integration
  Given task A 完成 ingest `/tmp/doc.md` 并写入 1 个 drawer
  And task B 在 task A 释放锁前启动并等待
  When task A 释放锁，task B 拿到锁
  Then task B 的 `IngestOutcome` == `Skipped(reason=AlreadyIngested)`
  And drawer_count == "1"（没有 duplicate insert）

Scenario: panic 不泄漏锁（RAII guard）
  Test:
    Filter: test_panic_in_critical_section_releases_lock
    Level: integration
  Given task A 拿锁后 panic
  When task B 调 `acquire_source_lock` with timeout=500ms
  Then task B 在 500ms 内拿到锁
  And `/tmp/.mempal/locks/<key>.lock` 文件仍存在（OS 已 release flock）

Scenario: dry_run 不占锁（不阻塞其他 ingest）
  Test:
    Filter: test_dry_run_does_not_acquire_lock
    Level: integration
  Given 调用 `ingest_file_with_options(..., opts.dry_run=true)`
  When dry_run 路径完成
  Then `lock_wait_ms` 为 `None`
  And drawer_count == "0"（dry_run 不写库，也不进入加锁临界区）

Scenario: MCP 工具 response 含 lock_wait_ms 字段
  Test:
    Filter: test_mcp_ingest_response_exposes_lock_wait
    Level: integration
  Given 并发两个 `mempal_ingest` 同 source
  Then 两次 response 中恰好一个 `lock_wait_ms > 0`
  And 字段在 JSON 中正常序列化
