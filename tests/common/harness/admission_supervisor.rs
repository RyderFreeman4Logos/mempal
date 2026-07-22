// Regression modules below exercise the shared supervisor surface directly (Rust 011).
#[path = "../../../src/core/db_admission_test_process.rs"]
mod process;

const _: fn() = process::reference_shared_test_api;

#[path = "admission_supervisor/process_guard.rs"]
mod process_guard;
#[cfg(test)]
#[path = "admission_supervisor/stdio_regression_tests.rs"]
mod stdio_regression_tests;

pub use process::*;
pub(super) use process_guard::{ExactProcessGuard, process_identity};

#[cfg(test)]
#[path = "admission_supervisor/regression_tests.rs"]
mod regression_tests;
