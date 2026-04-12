//! Cross-agent cowork: live session peek (no storage) + decision-only ingest.
//!
//! See `docs/specs/2026-04-13-cowork-peek-and-decide.md`.

pub mod claude;
pub mod codex;
pub mod peek;

pub use peek::{PeekError, PeekMessage, PeekRequest, PeekResponse, Tool, peek_partner};
