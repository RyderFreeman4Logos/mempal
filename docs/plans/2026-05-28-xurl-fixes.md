# xurl stall + search UX fixes

Branch: `fix/xurl-stall-and-search-ux`
Closes: #242, #243

## T1: Batch embed with progress and busy_timeout

**Files**: `src/xurl/embed.rs`, `src/xurl/ingest.rs`

**Changes**:
1. `embed.rs` — `embed_unindexed_turns`:
   - Process turns in batches of 50 (configurable `EMBED_BATCH_SIZE`)
   - Wrap each batch's INSERTs in a single transaction
   - Accept a progress callback `Fn(usize, usize)` (done, total) so callers can print progress
   - Set `busy_timeout(5000)` on the connection before starting embed work
2. `ingest.rs` — `ingest_all`:
   - Separate parse phase from embed phase: parse all files first (store turns), then embed all unindexed turns once at the end
   - Emit progress to stderr during both phases
   - Add `--json` progress mode: emit NDJSON lines `{"phase":"parse","file":"...","turns":N}` / `{"phase":"embed","done":N,"total":N}`
3. `ingest.rs` — `ingest_file`:
   - Keep single-file semantics unchanged (parse + embed) but use the batched embed

**Tests**: `tests/xurl_embed.rs` — add `test_embed_batch_progress` verifying callback fires with correct (done, total)

**DONE WHEN**: `cargo test --test xurl_embed` passes; `ingest_all` prints per-file progress to stderr; embed phase uses transactions with batch size 50

## T2: Search min-score floor

**Files**: `src/xurl/search.rs`, `src/main.rs`

**Changes**:
1. `search.rs` — `search()`: accept `min_score: Option<f32>`, filter hits below floor after scoring
2. `search.rs` — `print_hits_markdown()`: accept `min_score` param; when all hits filtered, print "No confident match (best score X.XXX < floor Y.YY)"
3. `main.rs` — `XurlCommands::Search`: add `--min-score <f32>` flag, default `0.70`; pass to `search()` and `print_hits_markdown()`

**Tests**: `tests/xurl_search.rs` — add `test_search_min_score_filters_low_hits`

**DONE WHEN**: `mempal xurl search "nonexistent" --min-score 0.9` prints "No confident match" message; `cargo test --test xurl_search` passes

## T3: Content dedup in search results

**Files**: `src/xurl/search.rs`

**Changes**:
1. After `best_by_turn` aggregation, add a second dedup pass: hash each hit's `content` (SHA-256 first 16 bytes), keep only the highest-scoring hit per content hash
2. This collapses identical messages that were ingested from overlapping sessions

**Tests**: `tests/xurl_search.rs` — add `test_search_dedup_identical_content`

**DONE WHEN**: Two turns with identical content but different turn_ids produce only one search hit (the higher-scored one); `cargo test --test xurl_search` passes

## T4: Full session_id and source path in markdown output

**Files**: `src/xurl/search.rs`, `src/xurl/store.rs`, `src/xurl/model.rs`, `src/main.rs`

**Changes**:
1. `search.rs` — `SearchHit`: add `source_path: Option<String>` field
2. `search.rs` — `search()`: join `ct.project_path` into SearchHit as `source_path`
3. `search.rs` — `print_hits_markdown()`: print **full** `session_id` (not truncated to 8 chars); if `source_path` is Some, print it on a sub-line
4. `main.rs` — timeline markdown: also print full `session_id` instead of 8-char truncation

**Tests**: `tests/xurl_search.rs` — existing tests verify full session_id in output

**DONE WHEN**: `print_hits_markdown` prints full session_id; timeline prints full session_id; source_path appears when available; `cargo test --test xurl_search` passes

## T5: Ingest progress output in CLI

**Files**: `src/main.rs`

**Changes**:
1. Wire progress callback from T1 into CLI: print `[parse] file: <name> (N turns)` to stderr per file
2. Print `[embed] N/M turns vectorized` every 50 turns to stderr
3. `--json` mode: emit NDJSON progress lines to stdout instead

**DONE WHEN**: `mempal xurl ingest` prints per-file parse progress and embed progress to stderr; `--json` emits structured progress; `cargo test` passes
