//! Cross-agent cowork: live session peek (no storage) + decision-only ingest.
//!
//! See `docs/specs/2026-04-13-cowork-peek-and-decide.md`.

pub mod claude;
pub mod codex;
pub mod peek;

// Re-exports are added in Task 2 once the types exist.
