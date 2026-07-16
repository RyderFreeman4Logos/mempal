# Optional ADK-Rust evidence workflow

Mempal can project existing search results into a deterministic, quality-gated,
citation-preserving evidence pack. The workflow runs **after** Mempal's existing
BM25/vector hybrid retrieval and reranking; it does not create a second index or
retrieval pipeline.

This is an optional production slice. It performs deterministic relevance
selection and citation verification, not model-authored summarization. It makes
no model or remote-provider calls.

## Enable it

The workflow has two independent opt-ins:

1. Build with Rust 1.94 or newer and the `adk-rust` Cargo feature.
2. Enable the workflow in TOML and request evidence from `mempal_search`.

```bash
cargo build --features adk-rust
```

```toml
[evidence_workflow]
enabled = true
engine = "adk-rust"
mode = "quality-gated"
input_top_k = 30
output_top_k = 8
max_evidence_tokens = 6000
minimum_relevance = 0.01
```

The default relevance floor is calibrated to Mempal's reciprocal-rank-fusion
score domain (`k = 60`), where even a top result is approximately `0.016` from
one retrieval list or `0.033` when both lexical and vector retrieval rank it
first.

Both compile-time and runtime enablement are off by default. Normal
`mempal_search` requests omit the additive `evidence` field and preserve existing
response serialization.

Request an evidence pack from the production MCP tool:

```json
{
  "query": "why did we choose Clerk?",
  "top_k": 10,
  "evidence": true
}
```

## Result contract

The `evidence` response has one of two successful routes:

- `quality_gated_evidence`: candidates passed the configured relevance floor,
  output count and token budgets, and deterministic citation verification.
- `raw_bounded_hits`: transformation was disabled, unavailable, below threshold,
  or failed verification; the bounded cited retrieval results are returned
  without model-authored content.

Every evidence item carries:

- the stable `hit_id` (drawer ID);
- `source_uri`, source scope, and source kind;
- the verbatim `exact_quote` and byte span;
- the BLAKE3 content hash;
- the retrieval relevance score and its `score_type` provenance (`lexical`,
  `vector`, `fused`, or `rerank`).

Returned quotes are copied from retrieved content. The graph does not accept or
emit free-form selector prose. Retrieved text is data passed through deterministic
nodes, not workflow instructions.

## ADK-Rust dependency

The feature pins the audited released dependency `adk-graph = 1.0.0`. ADK-Rust
v1.0.0 is Apache-2.0 licensed and requires Rust 1.94; Mempal itself remains MIT
licensed. Builds without this feature retain Mempal's Rust 1.85 minimum. If a
caller explicitly requests evidence from a build without ADK-Rust, the response
uses the normal cited fallback with `feature_unavailable`.

The workflow uses ADK-Rust's typed `StateGraph`, explicit nodes, conditional
routing, and compiled execution. `adk-graph` v1.0.0 has no type or API for
Mempal drawer identity, authoritative content-hash/span validation, existing
hybrid-score admission, or the normal raw-hit fallback contract. Mempal-owned
code is therefore limited to that thin retrieval adapter and those deterministic
citation, token-budget, and quality invariants.

Before upgrading ADK-Rust, re-audit graph APIs and the dependency/license/MSRV
impact against the pinned v1.0.0 behavior.
