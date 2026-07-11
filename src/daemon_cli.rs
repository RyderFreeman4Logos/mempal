//! Command-line shape for daemon lifecycle operations.

use clap::Subcommand;

#[derive(Subcommand, Clone, Debug)]
pub(crate) enum DaemonSubcommand {
    /// Start the daemon. Fails if already running.
    Start {
        /// Run in foreground without daemonizing.
        #[arg(long, default_value_t = false)]
        foreground: bool,
    },
    /// Gracefully stop the running daemon (waits up to 30s).
    Stop,
    /// Stop and restart the daemon.
    Restart,
    /// Wait until the Linux singleton daemon and its write transport are ready.
    Wait {
        /// Maximum bounded wait in seconds.
        #[arg(long = "timeout-secs", default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=300))]
        timeout_secs: u64,
    },
    /// Reap duplicate daemon processes while keeping one singleton alive.
    Reap,
    /// Show daemon status, PID, and queue stats.
    Status,
}
