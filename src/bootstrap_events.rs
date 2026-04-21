#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapEvent {
    Daemonize,
    RuntimeInit,
    ConfigHandleBootstrap,
    DbOpen,
    TracingInit,
    Ready,
}
