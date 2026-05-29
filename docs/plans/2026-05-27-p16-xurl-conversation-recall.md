# P16 Implementation Plan — xurl Conversation Recall

**Spec:** `specs/fork-ext/p16-xurl-conversation-recall.spec.md`
**Branch:** `feat/xurl-conversation-recall`
**Baseline commit:** `3f0d493` (sync upstream 2026-05-27)
**Estimate:** 3d

---

## Goal

Replace the full-JSONL conversation ingestion approach from PR #236 / issue #235 with a
targeted system that:

1. Indexes **screen-visible turns only** (user text + assistant text) from CC, Codex, Hermes
2. Stores them in a dedicated `conversation_turns` table (not `drawers`) at `fork_ext_v16`
3. Provides `mempal xurl ingest / search / timeline / stats` CLI subcommands
4. Removes or redirects the old `mempal ingest-conversation` command and SessionEnd auto-ingest

**Hard constraints:**
- Zero external LLM API calls (embedding only)
- Hermes DB opened read-only; never creates WAL/SHM files
- No MCP tool in this PR (CLI-only)
- Reuse existing `OpenAiCompatibleEmbedder` / model2vec infrastructure

---

## Architecture

### New module: `src/xurl/`

```
src/xurl/mod.rs          — pub re-exports; XurlError type
src/xurl/model.rs        — RawTurn, Tool, Role structs
src/xurl/parser/mod.rs   — parser trait + dispatch
src/xurl/parser/cc.rs    — CC JSONL parser
src/xurl/parser/codex.rs — Codex rollout JSONL parser
src/xurl/parser/hermes.rs— Hermes state.db SQLite parser
src/xurl/store.rs        — DB read/write helpers (conversation_turns table)
src/xurl/embed.rs        — long-turn chunking + embedding pipeline
src/xurl/search.rs       — semantic search over conversation_turn_vectors
src/xurl/ingest.rs       — orchestration: parse → store → embed
src/xurl/cli.rs          — CLI output formatting (markdown + JSON)
```

### Schema: `fork_ext_v16`

```sql
CREATE TABLE conversation_turns (
    id              TEXT PRIMARY KEY,   -- ulid
    session_id      TEXT NOT NULL,
    tool            TEXT NOT NULL,      -- 'cc' | 'codex' | 'hermes'
    turn_index      INTEGER NOT NULL,   -- 0-based within session
    role            TEXT NOT NULL,      -- 'user' | 'assistant'
    content         TEXT NOT NULL,
    timestamp_epoch REAL NOT NULL,
    token_count     INTEGER,
    project_path    TEXT,
    git_branch      TEXT,
    is_csa_delegated BOOLEAN NOT NULL DEFAULT FALSE,
    provenance       TEXT NOT NULL DEFAULT 'human',
    UNIQUE(session_id, tool, turn_index)
);
CREATE TABLE conversation_turn_vectors (
    turn_id     TEXT NOT NULL REFERENCES conversation_turns(id),
    chunk_index INTEGER NOT NULL DEFAULT 0,
    vector      BLOB NOT NULL,
    PRIMARY KEY (turn_id, chunk_index)
);
CREATE INDEX idx_ct_session    ON conversation_turns(session_id, tool);
CREATE INDEX idx_ct_timestamp  ON conversation_turns(timestamp_epoch DESC);
CREATE INDEX idx_ct_project    ON conversation_turns(project_path);
```

### Files modified

| File | Change |
|------|--------|
| `src/core/db_fork_ext.rs` | Add `FORK_EXT_V16_SCHEMA_SQL` + `apply_v16` + register in `fork_ext_migrations()` + bump `CURRENT_FORK_EXT_VERSION` to 16 |
| `src/main.rs` | Add `Commands::Xurl { ... }` + wire dispatch; remove / redirect `Commands::IngestConversation` |
| `src/daemon.rs` | Remove `try_ingest_session_conversation`, the dead helpers `session_already_ingested` / `find_session_jsonl_path` / `hook_payload_session_id` |
| `src/core/config.rs` | Deprecate `HooksSessionEndConfig::auto_ingest_conversation` (keep field for compat, log warning if true) |
| `src/lib.rs` | Add `pub mod xurl;` |

**Not touched:** `src/ingest/conversation.rs`, `src/ingest/detect.rs`, `src/ingest/normalize.rs` (existing #235 parsers stay in place; the xurl path is parallel, not a rewrite of ingest pipeline), `drawers` / `drawer_vectors` tables, Cargo.toml dependency set.

---

## Pre-Flight Facts

Before starting, verify:

- `CURRENT_FORK_EXT_VERSION: u32 = 15` at `src/core/db_fork_ext.rs:17`
- `fork_ext_migrations()` has entries v1–v15; append v16 at the end
- `Commands::IngestConversation { path, session_id, project, dry_run, json, no_gate }` in `src/main.rs` at ~line 284
- `try_ingest_session_conversation` in `src/daemon.rs` at ~line 689
- `auto_ingest_conversation: bool` field in `HooksSessionEndConfig` at `src/core/config.rs:744`

---

## Tasks

### Task 0 — Fork-ext schema migration (ext_v16)

**What to test first (failing):**

```rust
// tests/xurl_schema.rs
#[test]
fn fork_ext_v16_creates_conversation_tables() {
    let db = open_temp_db_at_fork_ext(16);
    let tables: Vec<String> = db.conn().prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'conversation%'"
    ).unwrap()
    .query_map([], |r| r.get(0)).unwrap()
    .collect::<Result<_,_>>().unwrap();
    assert!(tables.contains(&"conversation_turns".to_string()));
    assert!(tables.contains(&"conversation_turn_vectors".to_string()));
}

#[test]
fn fork_ext_v16_unique_constraint() {
    let db = open_temp_db_at_fork_ext(16);
    db.conn().execute(
        "INSERT INTO conversation_turns VALUES ('id1','sess1','cc',0,'user','hello',1.0,5,NULL,NULL)",
        []
    ).unwrap();
    // Same (session_id, tool, turn_index) must fail
    let result = db.conn().execute(
        "INSERT INTO conversation_turns VALUES ('id2','sess1','cc',0,'user','world',1.0,5,NULL,NULL)",
        []
    );
    assert!(result.is_err());
}
```

**What to implement:**

1. In `src/core/db_fork_ext.rs`:
   - Add `pub const FORK_EXT_V16_SCHEMA_SQL: &str = r#"...#;` with `CREATE TABLE IF NOT EXISTS conversation_turns (...)`, `conversation_turn_vectors`, and all four indexes
   - Add `fn apply_v16(conn: &Connection) -> rusqlite::Result<()> { conn.execute_batch(FORK_EXT_V16_SCHEMA_SQL) }`
   - Add `Migration { version: 16, up: apply_v16 }` to `fork_ext_migrations()`
   - Bump `pub const CURRENT_FORK_EXT_VERSION: u32 = 16;`

**DONE WHEN:**
- `cargo test --test xurl_schema` passes
- `cargo check` has zero errors/warnings
- `git diff src/core/db_fork_ext.rs` shows version bump 15→16 and new schema SQL

---

### Task 1 — Data model + RawTurn struct

**What to test first (failing):**

```rust
// src/xurl/model.rs (unit tests)
#[test]
fn tool_display_round_trip() {
    assert_eq!(Tool::Cc.as_str(), "cc");
    assert_eq!(Tool::Codex.as_str(), "codex");
    assert_eq!(Tool::Hermes.as_str(), "hermes");
}

#[test]
fn role_display_round_trip() {
    assert_eq!(Role::User.as_str(), "user");
    assert_eq!(Role::Assistant.as_str(), "assistant");
}
```

**What to implement:**

- `src/xurl/mod.rs` — module declarations + `XurlError` (thiserror)
- `src/xurl/model.rs`:
  ```rust
  pub enum Tool { Cc, Codex, Hermes }
  pub enum Role { User, Assistant }
  pub struct RawTurn {
      pub session_id: String,
      pub tool: Tool,
      pub role: Role,
      pub content: String,
      pub timestamp_epoch: f64,
      pub project_path: Option<String>,
      pub git_branch: Option<String>,
      pub is_csa_delegated: bool,
      pub provenance: Provenance, // Human | Agent | System
  }
  ```
- Add `pub mod xurl;` to `src/lib.rs`

**DONE WHEN:**
- `cargo test -p mempal src::xurl::model` passes
- `cargo check` clean

---

### Task 2 — CC JSONL parser

**What to test first (failing):**

```rust
// src/xurl/parser/cc.rs (unit tests)
#[test]
fn cc_parser_extracts_35_turns_from_50_entry_file() {
    // 20 user text + 15 assistant text + 15 tool_use/tool_result
    let jsonl = build_cc_fixture(20, 15, 15);
    let turns = parse_cc_jsonl(&jsonl, "sess123").unwrap();
    assert_eq!(turns.len(), 35);
    let user_count = turns.iter().filter(|t| t.role == Role::User).count();
    let asst_count = turns.iter().filter(|t| t.role == Role::Assistant).count();
    assert_eq!(user_count, 20);
    assert_eq!(asst_count, 15);
}

#[test]
fn cc_parser_skips_non_text_content_blocks() {
    // assistant turn with [tool_use, text, tool_result] blocks
    // only the text block content should appear
    let line = r#"{"type":"assistant","timestamp":"2026-05-27T12:00:00Z","sessionId":"s1","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash"},{"type":"text","text":"Here is my answer."},{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
    let turns = parse_cc_jsonl(line, "s1").unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].content, "Here is my answer.");
}

#[test]
fn cc_parser_normalizes_timestamp_to_epoch() {
    let line = r#"{"type":"user","timestamp":"2026-05-27T14:30:00Z","sessionId":"s2","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
    let turns = parse_cc_jsonl(line, "s2").unwrap();
    // 2026-05-27T14:30:00Z = 1748356200.0
    assert!((turns[0].timestamp_epoch - 1748356200.0).abs() < 1.0);
}
```

**What to implement:**

- `src/xurl/parser/mod.rs` — mod declarations
- `src/xurl/parser/cc.rs`:
  - `pub fn parse_cc_jsonl(content: &str, fallback_session_id: &str) -> Result<Vec<RawTurn>>`
  - Parse line by line; keep only `type IN ["user","assistant"]`
  - Extract `sessionId` from first matching line (fallback to `fallback_session_id`)
  - Extract content by `message.content[].type == "text"` → join text blocks
  - Skip `tool_use`, `tool_result`, `progress`, `file-history-snapshot`, `queue-operation` blocks
  - Parse `timestamp` field (ISO 8601) via `chrono::DateTime::parse_from_rfc3339` → Unix epoch f64
  - Return turns with monotonically increasing `turn_index` (0-based, counting only kept turns)

**DONE WHEN:**
- Unit tests in cc.rs pass
- `cargo test` clean

---

### Task 3 — Codex JSONL parser

**What to test first (failing):**

```rust
// src/xurl/parser/codex.rs (unit tests)
#[test]
fn codex_parser_extracts_assistant_turns_from_response_items() {
    // response_item with payload.role == "assistant" + payload.type == "agent_message"
    let jsonl = concat!(
        r#"{"type":"session_meta","session_id":"sess42","ts":"2026-05-27T12:00:00Z"}"#, "\n",
        r#"{"type":"response_item","ts":"2026-05-27T12:00:01Z","payload":{"role":"assistant","type":"agent_message","text":"Hello world"}}"#, "\n",
        r#"{"type":"event_msg","ts":"2026-05-27T12:00:02Z","payload":{"type":"user_message","text":"How are you?"}}"#, "\n",
    );
    let turns = parse_codex_jsonl(jsonl, "sess42").unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].role, Role::Assistant);
    assert_eq!(turns[0].content, "Hello world");
    assert_eq!(turns[1].role, Role::User);
}

#[test]
fn codex_parser_skips_non_message_events() {
    // tool_call, tool_output events should not produce turns
    let jsonl = concat!(
        r#"{"type":"session_meta","session_id":"s1","ts":"2026-05-27T12:00:00Z"}"#, "\n",
        r#"{"type":"event_msg","ts":"2026-05-27T12:00:01Z","payload":{"type":"tool_call","tool_name":"bash","input":"ls"}}"#, "\n",
        r#"{"type":"response_item","ts":"2026-05-27T12:00:02Z","payload":{"role":"assistant","type":"agent_message","text":"Done"}}"#, "\n",
    );
    let turns = parse_codex_jsonl(jsonl, "s1").unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].content, "Done");
}
```

**What to implement:**

- `src/xurl/parser/codex.rs`:
  - `pub fn parse_codex_jsonl(content: &str, fallback_session_id: &str) -> Result<Vec<RawTurn>>`
  - Extract session_id from `session_meta` entry
  - From `response_item`: keep when `payload.role == "assistant"` AND `payload.type == "agent_message"`; extract `payload.text`
  - From `event_msg`: keep when `payload.type == "user_message"`; extract `payload.text`
  - Timestamp: `ts` field (RFC 3339) → epoch
  - Skip all other entry types

**DONE WHEN:**
- Unit tests in codex.rs pass
- `cargo test` clean

---

### Task 4 — Hermes SQLite parser

**What to test first (failing):**

```rust
// src/xurl/parser/hermes.rs (unit tests)
#[test]
fn hermes_parser_reads_user_and_assistant_turns() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    setup_hermes_fixture(&db_path, &[
        ("user", "Hello!", 1748356100.0),
        ("assistant", "Hi there!", 1748356200.0),
        ("tool", "bash output", 1748356250.0),  // should be skipped
        ("assistant", "Done.", 1748356300.0),
    ]);

    let turns = parse_hermes_db(&db_path, "sess-hermes").unwrap();
    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0].role, Role::User);
    assert_eq!(turns[1].role, Role::Assistant);
    assert_eq!(turns[2].role, Role::Assistant);
    assert_eq!(turns[2].content, "Done.");
}

#[test]
fn hermes_parser_opens_readonly_never_creates_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("state.db");
    setup_hermes_fixture(&db_path, &[("user", "hi", 1.0)]);

    parse_hermes_db(&db_path, "s1").unwrap();

    // No -wal or -shm files should exist
    assert!(!dir.path().join("state.db-wal").exists());
    assert!(!dir.path().join("state.db-shm").exists());
}
```

**What to implement:**

- `src/xurl/parser/hermes.rs`:
  - `pub fn parse_hermes_db(path: &Path, fallback_session_id: &str) -> Result<Vec<RawTurn>>`
  - Open with `Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)`
  - `SELECT id, role, content, timestamp FROM messages WHERE role IN ('user','assistant') ORDER BY timestamp, id`
  - Skip `role = 'tool'` (enforced in WHERE clause)
  - Apply `sanitize_context()`: strip `<context>…</context>` blocks and `<system-reminder>` tags (mirror Hermes' own scrubber)
  - Timestamp column is already Unix epoch float
  - Session_id from `PRAGMA database_list` or fallback to `fallback_session_id`

**DONE WHEN:**
- Unit tests pass including WAL absence check
- `cargo test` clean

---

### Task 5 — Turn storage + content-hash dedup

**What to test first (failing):**

```rust
// tests/xurl_store.rs
#[tokio::test]
async fn insert_turns_idempotent_same_content() {
    let db = open_temp_db_at_fork_ext(16);
    let turn = make_raw_turn("sess1", 0, Role::User, "Hello");
    store::insert_turns(&db.conn(), &[turn.clone()]).unwrap();
    store::insert_turns(&db.conn(), &[turn]).unwrap(); // second call
    let count: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM conversation_turns",
        [],
        |r| r.get(0)
    ).unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn insert_turns_updates_changed_content() {
    let db = open_temp_db_at_fork_ext(16);
    let turn = make_raw_turn("sess1", 0, Role::User, "Hello");
    store::insert_turns(&db.conn(), &[turn]).unwrap();
    let updated = make_raw_turn("sess1", 0, Role::User, "Hello v2");
    store::insert_turns(&db.conn(), &[updated]).unwrap();
    let content: String = db.conn().query_row(
        "SELECT content FROM conversation_turns WHERE session_id='sess1' AND turn_index=0",
        [],
        |r| r.get(0)
    ).unwrap();
    assert_eq!(content, "Hello v2");
}
```

**What to implement:**

- `src/xurl/store.rs`:
  - `pub struct StoredTurn { id: String, ... }` (DB row view)
  - `pub fn insert_turns(conn: &Connection, turns: &[RawTurn]) -> XurlResult<InsertStats>`
  - Dedup strategy: `INSERT INTO conversation_turns ... ON CONFLICT(session_id, tool, turn_index) DO UPDATE SET content=excluded.content, ... WHERE content != excluded.content`
  - Returns `InsertStats { inserted: usize, skipped: usize, updated: usize }`
  - Turn `id` = ULID generated at insert time
  - `pub fn get_turns(conn: &Connection, filter: TurnFilter) -> XurlResult<Vec<StoredTurn>>`
  - `TurnFilter { tool: Option<Tool>, session_id: Option<String>, since_epoch: Option<f64>, limit: usize, offset: usize }`

**DONE WHEN:**
- Tests in xurl_store.rs pass
- `cargo test` clean

---

### Task 6 — Embedding pipeline (long-turn chunking)

**What to test first (failing):**

```rust
// tests/xurl_embed.rs
#[tokio::test]
async fn long_assistant_turn_produces_multiple_vectors() {
    let db = open_temp_db_at_fork_ext(16);
    // Insert a turn whose content is ~2000 tokens
    let long_content = "word ".repeat(1500); // approximation
    let turn_id = "turn-001";
    db.conn().execute(
        "INSERT INTO conversation_turns VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![turn_id, "sess1", "cc", 0, "assistant", long_content, 1.0, 1500, None::<String>, None::<String>]
    ).unwrap();

    let embedder = MockEmbedder::new_fixed_dim(256);
    embed::embed_unindexed_turns(&db, &embedder).await.unwrap();

    let vector_count: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM conversation_turn_vectors WHERE turn_id=?",
        params![turn_id],
        |r| r.get(0)
    ).unwrap();
    assert!(vector_count > 1, "expected multiple chunks, got {vector_count}");
}

#[tokio::test]
async fn short_turn_produces_exactly_one_vector() {
    let db = open_temp_db_at_fork_ext(16);
    let turn_id = "turn-002";
    db.conn().execute(
        "INSERT INTO conversation_turns VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![turn_id, "sess1", "cc", 1, "user", "Hi there", 1.1, 3, None::<String>, None::<String>]
    ).unwrap();
    let embedder = MockEmbedder::new_fixed_dim(256);
    embed::embed_unindexed_turns(&db, &embedder).await.unwrap();
    let count: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM conversation_turn_vectors WHERE turn_id=?",
        params![turn_id],
        |r| r.get(0)
    ).unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn re_embed_already_indexed_turns_is_noop() {
    // turns with existing vectors are skipped
    let db = open_temp_db_at_fork_ext(16);
    // ... setup with pre-existing vector row
    let embedder = MockEmbedder::new_fixed_dim(256);
    let stats1 = embed::embed_unindexed_turns(&db, &embedder).await.unwrap();
    let stats2 = embed::embed_unindexed_turns(&db, &embedder).await.unwrap();
    assert_eq!(stats2.embedded, 0);
}
```

**What to implement:**

- `src/xurl/embed.rs`:
  - `pub async fn embed_unindexed_turns<E: Embedder + ?Sized>(db: &Database, embedder: &E) -> XurlResult<EmbedStats>`
  - Queries `conversation_turns` LEFT JOIN `conversation_turn_vectors` WHERE `turn_id IS NULL` (unindexed)
  - For each turn, applies token-aware sentence-boundary chunking (reuse `chunk_text_token_aware` from `src/ingest/chunk.rs`)
  - Embeds chunk batch via `embedder.embed(&chunks)`
  - Inserts `(turn_id, chunk_index, vector)` rows into `conversation_turn_vectors`
  - Uses `DEFAULT chunk_index=0`; multi-chunk turns get chunk_index 0,1,2,...
  - Returns `EmbedStats { turns_processed: usize, embedded: usize, chunks_total: usize }`

**DONE WHEN:**
- Tests in xurl_embed.rs pass
- `cargo test` clean

---

### Task 7 — `mempal xurl ingest` (single file + auto-scan)

**What to test first (failing):**

```rust
// tests/xurl_ingest.rs
#[tokio::test]
async fn ingest_cc_file_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db_at_fork_ext(16);
    let file = write_cc_fixture(&dir, 10, 8, 12); // 10 user, 8 asst, 12 tool
    let embedder = MockEmbedder::new_fixed_dim(256);

    let stats = ingest::ingest_file(&db, &embedder, &file, Tool::Cc, None).await.unwrap();
    assert_eq!(stats.turns_parsed, 18);
    assert_eq!(stats.turns_inserted, 18);
    assert_eq!(stats.turns_skipped, 0);

    // Re-ingest → no new turns
    let stats2 = ingest::ingest_file(&db, &embedder, &file, Tool::Cc, None).await.unwrap();
    assert_eq!(stats2.turns_inserted, 0);
    assert_eq!(stats2.turns_skipped, 18);
}
```

**What to implement:**

- `src/xurl/ingest.rs`:
  - `pub struct IngestStats { turns_parsed, turns_inserted, turns_skipped, turns_updated, vectors_created }`
  - `pub async fn ingest_file<E: Embedder + ?Sized>(db: &Database, embedder: &E, path: &Path, tool: Tool, session_id_override: Option<&str>) -> XurlResult<IngestStats>`
    1. Read file (or open SQLite for Hermes)
    2. Call appropriate parser → `Vec<RawTurn>`
    3. Call `store::insert_turns()` → InsertStats
    4. Call `embed::embed_unindexed_turns()` for newly inserted
    5. Return combined stats
  - `pub async fn ingest_all<E: Embedder + ?Sized>(db: &Database, embedder: &E, cfg: &AutoScanConfig) -> XurlResult<IngestStats>` — scans default dirs for each tool:
    - CC: `~/.claude/projects/**/*.jsonl`
    - Codex: `$CODEX_HOME/sessions/**/rollout-*.jsonl` (fallback: `~/.codex/sessions/`)
    - Hermes: `~/.hermes/state.db` (if exists)
  - `pub struct AutoScanConfig { cc_root: PathBuf, codex_root: PathBuf, hermes_db: Option<PathBuf> }`

- CLI wiring in `src/main.rs`:
  ```
  Commands::Xurl(XurlArgs) where XurlArgs::Ingest { tool, path, ... }
  ```

**DONE WHEN:**
- `tests/xurl_ingest.rs::ingest_cc_file_end_to_end` passes
- `cargo run -- xurl ingest --tool cc --path /dev/null` returns exit 0 (empty file)
- `cargo test` clean

---

### Task 8 — `mempal xurl search <query>`

**What to test first (failing):**

```rust
// tests/xurl_search.rs
#[tokio::test]
async fn search_returns_semantically_relevant_turn() {
    let db = open_temp_db_at_fork_ext(16);
    let embedder = MockEmbedder::semantic_fixture(); // returns distinct vectors per content

    // Index three turns
    seed_turn(&db, &embedder, "sess1", "cc", 0, "user", "database migration strategy").await;
    seed_turn(&db, &embedder, "sess1", "cc", 1, "assistant", "Use flyway for schema changes").await;
    seed_turn(&db, &embedder, "sess2", "codex", 0, "user", "how to bake bread").await;

    let results = search::search(&db, &embedder, "database migration", 5, None).await.unwrap();
    assert!(!results.is_empty());
    // Top result should be about database, not bread
    assert!(results[0].content.contains("database") || results[0].content.contains("flyway"));
}

#[tokio::test]
async fn search_with_tool_filter() {
    // Identical content in both cc and codex; --tool cc only returns cc results
    let db = open_temp_db_at_fork_ext(16);
    let embedder = MockEmbedder::semantic_fixture();
    seed_turn(&db, &embedder, "sess1", "cc", 0, "user", "rust ownership").await;
    seed_turn(&db, &embedder, "sess2", "codex", 0, "user", "rust ownership").await;

    let results = search::search(
        &db, &embedder, "rust ownership", 10,
        Some(TurnFilter { tool: Some(Tool::Cc), ..Default::default() })
    ).await.unwrap();
    assert!(results.iter().all(|r| r.tool == Tool::Cc));
}
```

**What to implement:**

- `src/xurl/search.rs`:
  - `pub async fn search<E: Embedder + ?Sized>(db: &Database, embedder: &E, query: &str, limit: usize, filter: Option<TurnFilter>) -> XurlResult<Vec<SearchHit>>`
  - Embed the query string via `embedder.embed(&[query])`
  - Run sqlite-vec KNN on `conversation_turn_vectors` → get top `limit * 10` candidates (for post-filter headroom)
  - JOIN back to `conversation_turns` and apply filter (tool, session_id, since_epoch)
  - Deduplicate by `turn_id` (a multi-chunk turn may match on multiple chunks; keep best score)
  - Return top `limit` results as `SearchHit { turn_id, session_id, tool, role, content, timestamp_epoch, score }`

- CLI wiring: `mempal xurl search <query> [--tool cc|codex|hermes] [--since 7d] [--limit N] [--format json]`
- Output format: markdown block per hit with `[tool] [session_abbrev] [timestamp] [role]` header

**DONE WHEN:**
- Tests in xurl_search.rs pass
- `cargo run -- xurl search "test query"` produces formatted output
- `cargo test` clean

---

### Task 9 — `mempal xurl timeline` + `mempal xurl stats`

**What to test first (failing):**

```rust
// tests/xurl_timeline.rs
#[test]
fn timeline_returns_newest_first() {
    let db = open_temp_db_at_fork_ext(16);
    insert_raw_turn(&db, "t1", "sess1", "cc", 0, "user", "first", 1000.0);
    insert_raw_turn(&db, "t2", "sess1", "cc", 1, "user", "second", 2000.0);
    insert_raw_turn(&db, "t3", "sess1", "cc", 2, "user", "third", 3000.0);

    let turns = store::get_turns(&db.conn(), TurnFilter { limit: 10, ..Default::default() }).unwrap();
    assert_eq!(turns[0].timestamp_epoch, 3000.0);
    assert_eq!(turns[2].timestamp_epoch, 1000.0);
}

#[test]
fn stats_shows_per_tool_counts() {
    let db = open_temp_db_at_fork_ext(16);
    insert_raw_turn(&db, "t1", "s1", "cc",     0, "user", "hi", 1.0);
    insert_raw_turn(&db, "t2", "s2", "codex",  0, "user", "hi", 2.0);
    insert_raw_turn(&db, "t3", "s2", "codex",  1, "assistant", "bye", 3.0);

    let stats = store::get_stats(&db.conn()).unwrap();
    assert_eq!(stats.iter().find(|s| s.tool == "cc").unwrap().count, 1);
    assert_eq!(stats.iter().find(|s| s.tool == "codex").unwrap().count, 2);
}
```

**What to implement:**

- In `src/xurl/store.rs`, add:
  - `pub fn get_stats(conn: &Connection) -> XurlResult<Vec<ToolStat>>`
    - `SELECT tool, COUNT(*) as count, MIN(timestamp_epoch), MAX(timestamp_epoch) FROM conversation_turns GROUP BY tool`
  - Extend `get_turns` with `ORDER BY timestamp_epoch DESC` and pagination (`LIMIT / OFFSET`)

- CLI wiring:
  - `mempal xurl timeline [--tool cc|codex|hermes] [--session <id>] [--since 7d] [--limit 20] [--page N]`
  - `mempal xurl stats`
  - Output: markdown table for stats, markdown blocks for timeline

**DONE WHEN:**
- Tests in xurl_timeline.rs pass
- `cargo run -- xurl stats` produces valid output when DB has data
- `cargo test` clean

---

### Task 10 — Remove old #235 implementation

**What to test first (failing):**

```rust
// Verify old CLI command either removed or redirected:
// cargo run -- ingest-conversation /tmp/fake.jsonl
// Should exit with error "use `mempal xurl ingest` instead" or be absent from --help
```

**What to implement:**

1. **`src/daemon.rs`** — Remove:
   - `try_ingest_session_conversation()` function (lines ~689–772)
   - `session_already_ingested()` helper
   - `find_session_jsonl_path()` helper
   - `hook_payload_session_id()` helper
   - The call site at line ~395–397:
     ```rust
     if envelope.event == "SessionEnd" {
         try_ingest_session_conversation(...).await;  // ← remove
     }
     ```

2. **`src/core/config.rs`** — In `HooksSessionEndConfig`:
   - Mark `auto_ingest_conversation` field deprecated: keep parsing for compat but add startup warning if `true`

3. **`src/main.rs`** — `Commands::IngestConversation`:
   - Either remove entirely and add a blank line in the subcommand list pointing to `xurl ingest`
   - Or alias: when called, print `"This command was removed in P16. Use `mempal xurl ingest --tool cc --path <path>` instead."` and exit 1

4. **`src/ingest/conversation.rs`** — Leave in place (used by existing ingest pipeline for drawer-based CC ingest); add doc comment: `// Legacy CC session ID helpers; xurl parsers use their own session discovery`

**DONE WHEN:**
- `cargo run -- ingest-conversation /tmp/x` either fails gracefully with redirect message, or `ingest-conversation` is absent from `cargo run -- --help`
- `cargo test` passes (no regressions)
- `auto_ingest_conversation = true` in config prints a warning but doesn't crash
- `cargo check` zero warnings

---

## Commit Strategy

One commit per task. Each commit must be compilable and pass `cargo test`:

| Commit | Scope | Message pattern |
|--------|-------|----------------|
| T0 | db_fork_ext.rs | `feat(schema): add fork_ext_v16 conversation_turns migration` |
| T1 | src/xurl/ | `feat(xurl): add RawTurn data model and module skeleton` |
| T2 | src/xurl/parser/cc.rs | `feat(xurl): add CC JSONL parser (screen-visible turns only)` |
| T3 | src/xurl/parser/codex.rs | `feat(xurl): add Codex rollout JSONL parser` |
| T4 | src/xurl/parser/hermes.rs | `feat(xurl): add Hermes SQLite parser (read-only)` |
| T5 | src/xurl/store.rs | `feat(xurl): add turn storage with content-hash dedup` |
| T6 | src/xurl/embed.rs | `feat(xurl): add embedding pipeline for conversation turns` |
| T7 | src/xurl/ingest.rs + main.rs | `feat(xurl): add ingest command (single file + auto-scan)` |
| T8 | src/xurl/search.rs + main.rs | `feat(xurl): add semantic search command` |
| T9 | src/xurl/store.rs + main.rs | `feat(xurl): add timeline and stats commands` |
| T10 | daemon.rs + config.rs + main.rs | `fix(xurl): remove old #235 ingest-conversation implementation` |

---

## Spec Scenario → Task Mapping

| Scenario | Task(s) |
|----------|---------|
| S1: Index CC session turns (35 of 50) | T2 (parser) + T7 (ingest) |
| S2: Semantic search across tools | T8 (search) |
| S3: Timeline with tool filter | T9 (timeline) |
| S4: Incremental re-ingest idempotent | T5 (store dedup) + T7 |
| S5: Long assistant output chunked | T6 (embed) |
| S6: Hermes SQLite read-only | T4 (hermes parser) |
| S7: Cross-tool chronological alignment | T9 (timeline ordering) |

---

## DONE WHEN (spec §DONE WHEN)

1. ✅ `conversation_turns` + `conversation_turn_vectors` at ext_v16 — Task 0
2. ✅ CC, Codex, Hermes parsers extract screen-visible content only — Tasks 2, 3, 4
3. ✅ `mempal xurl ingest` indexes turns from all three tools — Task 7
4. ✅ `mempal xurl search <query>` returns semantically relevant turns — Task 8
5. ✅ `mempal xurl timeline` shows paginated chronological view — Task 9
6. ✅ `mempal xurl stats` shows per-tool counts — Task 9
7. ✅ Incremental re-ingest is idempotent (S4) — Task 5
8. ✅ Old `ingest-conversation` command removed or redirected — Task 10
9. ✅ SessionEnd daemon hook removed or rewired — Task 10
10. ✅ All spec scenarios pass — verified across Tasks 0–10
