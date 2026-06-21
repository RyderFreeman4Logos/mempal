#![warn(clippy::all)]

//! Stateless memory operators.
//!
//! This module is the home for deterministic algorithms that operate on plain
//! memory-shaped inputs. Operators here must not open databases, call embedders,
//! read global config, spawn tasks, or depend on CLI/MCP/runtime handles. Store
//! and runtime layers adapt their data into these APIs and then persist or
//! render the outputs.

pub mod ranking;
