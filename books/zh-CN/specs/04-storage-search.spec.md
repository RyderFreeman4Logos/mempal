spec: task
name: "mempal book zh-CN chapter 4 storage search"
inherits: project
tags: [book, writing, zh-cn, storage, search]
---

## Intent

Write chapter 4 to explain storage, hybrid retrieval, and citation discipline.
The chapter must help readers understand why mempal stores raw evidence and how
search differs from context assembly.

## Decisions

- The chapter must explain drawers as raw source-backed memory.
- The chapter must explain BM25/vector/RRF at a practical level.
- The chapter must mention chunk neighbors, tunnels, structured signals, and fact-checking.
- The chapter must include a clear search/context/brief comparison.

## Boundaries

### Allowed Changes
- books/zh-CN/src/ch04-storage-search.md

### Forbidden
- Do not dive into sqlite-vec internals.
- Do not claim AAAK is required for ingest.

## Acceptance Criteria

Scenario: Raw storage and citation are covered
  Test: rg "raw|drawer|source_file|drawer_id|引用|出处" books/zh-CN/src/ch04-storage-search.md
  When chapter 4 is read
  Then raw evidence storage is explained
  And citation fields are named

Scenario: Hybrid search is covered
  Test: rg "BM25|vector|RRF|neighbors|tunnels|signals" books/zh-CN/src/ch04-storage-search.md
  When chapter 4 is read
  Then the retrieval path is explained

Scenario: Search context distinction exists
  Test: rg "search|context|brief|区别|边界" books/zh-CN/src/ch04-storage-search.md
  When chapter 4 is read
  Then readers can distinguish the three runtime read paths

## Out of Scope

- Benchmark analysis.
- Full SQL schema dump.
