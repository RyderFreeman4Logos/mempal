#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapEvent {
    RecoveryAdmitted,
    Daemonize,
    RuntimeInit,
    ConfigHandleBootstrap,
    DbOpen,
    TracingInit,
    Ready,
}
