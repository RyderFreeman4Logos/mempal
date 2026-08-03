use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use super::config::{CompiledPrivacyConfig, Config, ConfigError, ConfigSnapshotMeta};

const MAX_EVENT_LOG: usize = 64;
const MAX_PERSISTED_EVENT_LOG_BYTES: u64 = 64 * 1024;
const HOT_RELOAD_EVENT_LOG_FILE: &str = "hot-reload-events.log";
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(25);
const HOT_RELOAD_EVENT_KIND_RESTART_REQUIRED: &str = "restart_required";

#[derive(Debug, Deserialize, Serialize)]
struct PersistedHotReloadEvent {
    mempal_hot_reload_event: String,
    schema_version: u8,
    pid: u32,
    message: String,
}

#[derive(Debug)]
struct ConfigSnapshot {
    config: Arc<Config>,
    compiled_privacy: Arc<CompiledPrivacyConfig>,
    version: String,
    loaded_at_unix_ms: u64,
}

impl ConfigSnapshot {
    fn from_config(config: Config) -> Result<Self, ConfigError> {
        let compiled_privacy = Arc::new(config.compile_privacy()?);
        Ok(Self {
            version: config.effective_hash()?,
            loaded_at_unix_ms: now_unix_ms(),
            config: Arc::new(config),
            compiled_privacy,
        })
    }
}

enum WatchMessage {
    FileChanged,
    NotifyFailed,
    Stop,
}

#[cfg(unix)]
static SIGHUP_PENDING: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
static SIGHUP_HANDLER_INSTALLED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn hot_reload_sighup_handler(_signal: i32) {
    SIGHUP_PENDING.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_sighup_handler_once() {
    if SIGHUP_HANDLER_INSTALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    // SAFETY: the handler only flips an AtomicBool, which is signal-safe.
    unsafe {
        let handler = hot_reload_sighup_handler as *const () as usize;
        let _ = libc::signal(libc::SIGHUP, handler);
    }
}

#[cfg(not(unix))]
fn install_sighup_handler_once() {}

struct RuntimeControl {
    stop: Arc<AtomicBool>,
    control_tx: mpsc::Sender<WatchMessage>,
    coordinator: thread::JoinHandle<()>,
    poller: thread::JoinHandle<()>,
    poller_thread: thread::Thread,
}

impl RuntimeControl {
    fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.control_tx.send(WatchMessage::Stop);
        self.poller_thread.unpark();
        let _ = self.coordinator.join();
        let _ = self.poller.join();
    }
}

pub struct HotReloadState {
    snapshot: ArcSwap<ConfigSnapshot>,
    runtime: Mutex<Option<RuntimeControl>>,
    event_log: Mutex<VecDeque<String>>,
    event_log_path: Mutex<Option<PathBuf>>,
    parse_attempts: AtomicUsize,
    // harness-point: PR0 — counts successful reload applications (version changes)
    reload_count: Arc<AtomicUsize>,
    runtime_prototypes: ArcSwap<Vec<String>>,
    /// Incremented whenever a hot-reloadable LLM field changes (endpoint,
    /// credentials, model, extra body, timeout, retry_interval_secs,
    /// enabled_for, max_concurrent). LLM workers subscribe to this to cancel
    /// in-flight requests and restart with the new config.
    llm_gen_tx: tokio::sync::watch::Sender<u64>,
    /// Incremented when hot-reload applies embedding endpoint runtime changes
    /// that preserve vector identity. Long-lived embedding clients rebuild on
    /// the next request and keep sharing the configured endpoint pool.
    embed_gen_tx: tokio::sync::watch::Sender<u64>,
}

impl HotReloadState {
    fn new() -> Self {
        let initial = Config::default();
        let snapshot =
            ConfigSnapshot::from_config(initial.clone()).expect("default config is valid");
        let (llm_gen_tx, _) = tokio::sync::watch::channel(0u64);
        let (embed_gen_tx, _) = tokio::sync::watch::channel(0u64);
        Self {
            snapshot: ArcSwap::from_pointee(snapshot),
            runtime: Mutex::new(None),
            event_log: Mutex::new(VecDeque::new()),
            event_log_path: Mutex::new(None),
            parse_attempts: AtomicUsize::new(0),
            reload_count: Arc::new(AtomicUsize::new(0)),
            runtime_prototypes: ArcSwap::from_pointee(
                initial.ingest_gating.embedding_classifier.prototypes,
            ),
            llm_gen_tx,
            embed_gen_tx,
        }
    }

    pub fn bootstrap(&self, path: &Path) -> Result<(), ConfigError> {
        self.bootstrap_with_bootstrap_stderr(path, true)
    }

    pub fn bootstrap_quiet(&self, path: &Path) -> Result<(), ConfigError> {
        self.bootstrap_with_bootstrap_stderr(path, false)
    }

    fn bootstrap_with_bootstrap_stderr(
        &self,
        path: &Path,
        emit_bootstrap_event_to_stderr: bool,
    ) -> Result<(), ConfigError> {
        let config = Config::load_from(path)?;
        let snapshot = ConfigSnapshot::from_config(config.clone())?;
        self.snapshot.store(Arc::new(snapshot));
        *self
            .event_log_path
            .lock()
            .expect("event log path mutex poisoned") = Some(hot_reload_event_log_path(path));
        clear_persisted_restart_events_for_pid(
            &hot_reload_event_log_path(path),
            std::process::id(),
        );
        self.event_log
            .lock()
            .expect("event log mutex poisoned")
            .clear();
        self.runtime_prototypes.store(Arc::new(
            config.ingest_gating.embedding_classifier.prototypes.clone(),
        ));
        self.record_event(
            format!(
                "config hot-reload: bootstrapped version {}",
                self.snapshot_meta().version
            ),
            emit_bootstrap_event_to_stderr,
        );

        let mut runtime = self.runtime.lock().expect("runtime mutex poisoned");
        if let Some(existing) = runtime.take() {
            existing.stop();
        }
        if config.config_hot_reload.enabled {
            install_sighup_handler_once();
            #[cfg(unix)]
            SIGHUP_PENDING.store(false, Ordering::SeqCst);
            *runtime = Some(self.start_runtime(
                path.to_path_buf(),
                config.config_hot_reload.debounce_ms,
                config.config_hot_reload.poll_fallback_secs,
            ));
        }

        Ok(())
    }

    pub fn current(&self) -> Arc<Config> {
        Arc::clone(&self.snapshot.load_full().config)
    }

    pub fn current_compiled_privacy(&self) -> Arc<CompiledPrivacyConfig> {
        Arc::clone(&self.snapshot.load_full().compiled_privacy)
    }

    pub fn current_privacy_snapshot(&self) -> (Arc<Config>, Arc<CompiledPrivacyConfig>) {
        let snapshot = self.snapshot.load_full();
        (
            Arc::clone(&snapshot.config),
            Arc::clone(&snapshot.compiled_privacy),
        )
    }

    pub fn snapshot_meta(&self) -> ConfigSnapshotMeta {
        let snapshot = self.snapshot.load_full();
        ConfigSnapshotMeta {
            version: snapshot.version.clone(),
            loaded_at_unix_ms: snapshot.loaded_at_unix_ms,
        }
    }

    pub fn parse_attempts(&self) -> usize {
        self.parse_attempts.load(Ordering::SeqCst)
    }

    /// Number of successful reloads (version actually changed).
    // harness-point: PR0
    pub fn reload_count(&self) -> usize {
        self.reload_count.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub fn reload_counter_arc(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.reload_count)
    }

    pub fn recent_events(&self) -> Vec<String> {
        self.event_log
            .lock()
            .expect("event log mutex poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub fn restart_required_pending_events(&self) -> Vec<String> {
        let mut events = Vec::new();
        events.extend(self.recent_events());
        if let Some(path) = self
            .event_log_path
            .lock()
            .expect("event log path mutex poisoned")
            .clone()
        {
            events.extend(read_restart_required_events_from_path(&path));
        }
        dedupe_restart_required_events(events)
    }

    pub fn runtime_prototypes(&self) -> Vec<String> {
        (*self.runtime_prototypes.load_full()).clone()
    }

    pub fn simulate_notify_failure(&self) {
        if let Some(runtime) = self
            .runtime
            .lock()
            .expect("runtime mutex poisoned")
            .as_ref()
        {
            let _ = runtime.control_tx.send(WatchMessage::NotifyFailed);
        }
    }

    /// Subscribe to LLM generation changes.
    ///
    /// The channel value is a monotonically-increasing counter that is bumped
    /// whenever a hot-reloadable LLM field changes. LLM workers use this to
    /// detect config changes and cancel in-flight requests.
    pub fn subscribe_llm_gen(&self) -> tokio::sync::watch::Receiver<u64> {
        self.llm_gen_tx.subscribe()
    }

    pub fn current_llm_generation(&self) -> u64 {
        *self.llm_gen_tx.borrow()
    }

    /// Subscribe to embedding endpoint generation changes.
    ///
    /// The channel value is a monotonically-increasing counter that is bumped
    /// when hot-reload applies endpoint pool runtime fields without changing
    /// the embedding vector identity.
    pub fn subscribe_embed_gen(&self) -> tokio::sync::watch::Receiver<u64> {
        self.embed_gen_tx.subscribe()
    }

    pub fn current_embed_generation(&self) -> u64 {
        *self.embed_gen_tx.borrow()
    }

    /// Trigger a reload from the given path without going through the watcher.
    /// Only for use in tests via `ConfigHandle::harness_reload_from_path`.
    #[doc(hidden)]
    pub fn reload_from_disk_for_test(&self, path: &Path) {
        self.reload_from_disk(path);
    }

    /// Test-only: resets the snapshot, watcher runtime, event path/log, parse
    /// attempts, and runtime prototypes without runtime-allowed merges.
    /// Successful-reload and generation counters intentionally remain monotonic so
    /// existing subscribers never move backward.
    #[doc(hidden)]
    pub fn harness_reset(&self) {
        if let Some(existing) = self.runtime.lock().expect("runtime mutex poisoned").take() {
            existing.stop();
        }

        let defaults = Config::default();
        let snapshot =
            ConfigSnapshot::from_config(defaults.clone()).expect("default config is valid");
        self.snapshot.store(Arc::new(snapshot));
        *self
            .event_log_path
            .lock()
            .expect("event log path mutex poisoned") = None;
        self.event_log
            .lock()
            .expect("event log mutex poisoned")
            .clear();
        self.parse_attempts.store(0, Ordering::SeqCst);
        self.runtime_prototypes.store(Arc::new(
            defaults.ingest_gating.embedding_classifier.prototypes,
        ));
    }

    #[doc(hidden)]
    pub fn harness_runtime_active(&self) -> bool {
        self.runtime
            .lock()
            .expect("runtime mutex poisoned")
            .is_some()
    }

    #[doc(hidden)]
    pub fn harness_event_log_path(&self) -> Option<PathBuf> {
        self.event_log_path
            .lock()
            .expect("event log path mutex poisoned")
            .clone()
    }

    fn start_runtime(
        &self,
        path: PathBuf,
        debounce_ms: u64,
        poll_fallback_secs: u64,
    ) -> RuntimeControl {
        let stop = Arc::new(AtomicBool::new(false));
        let fallback_poll_enabled = Arc::new(AtomicBool::new(false));
        let (control_tx, control_rx) = mpsc::channel::<WatchMessage>();
        let notify_tx = control_tx.clone();
        let poll_tx = control_tx.clone();
        let start_failure_tx = control_tx.clone();
        let stop_for_coordinator = Arc::clone(&stop);
        let stop_for_poller = Arc::clone(&stop);
        let poll_toggle = Arc::clone(&fallback_poll_enabled);
        let state = global_hot_reload_state_arc();
        let watch_path = path.clone();
        let poll_path = path;
        let debounce = Duration::from_millis(debounce_ms.max(1));
        let poll_interval = Duration::from_secs(poll_fallback_secs.max(1));
        let (ready_tx, ready_rx) = mpsc::channel::<()>();

        let coordinator = thread::spawn(move || {
            let file_name = watch_path.file_name().map(OsStr::to_os_string);
            let watch_dir = watch_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            let mut watcher = create_watcher(notify_tx, file_name.clone());

            if let Some(active_watcher) = watcher.as_mut() {
                if let Err(error) = active_watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
                    state.push_event(format!(
                        "config hot-reload: notify watch failed for {}: {error}",
                        watch_dir.display()
                    ));
                    fallback_poll_enabled.store(true, Ordering::SeqCst);
                    drop(watcher.take());
                }
            } else {
                fallback_poll_enabled.store(true, Ordering::SeqCst);
            }

            let _ = ready_tx.send(());

            if std::env::var_os("MEMPAL_TEST_NOTIFY_FAIL_AFTER_START").is_some() {
                let _ = start_failure_tx.send(WatchMessage::NotifyFailed);
            }

            loop {
                let message = match control_rx.recv_timeout(SIGNAL_POLL_INTERVAL) {
                    Ok(message) => message,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        #[cfg(unix)]
                        if SIGHUP_PENDING.swap(false, Ordering::SeqCst) {
                            WatchMessage::FileChanged
                        } else {
                            continue;
                        }
                        #[cfg(not(unix))]
                        {
                            continue;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                match message {
                    WatchMessage::Stop => break,
                    WatchMessage::NotifyFailed => {
                        drop(watcher.take());
                        if !fallback_poll_enabled.swap(true, Ordering::SeqCst) {
                            state.push_event(
                                "config hot-reload: notify watcher crashed, falling back to poll"
                                    .to_string(),
                            );
                        }
                    }
                    WatchMessage::FileChanged => {
                        while let Ok(next) = control_rx.recv_timeout(debounce) {
                            match next {
                                WatchMessage::FileChanged => {}
                                WatchMessage::NotifyFailed => {
                                    drop(watcher.take());
                                    if !fallback_poll_enabled.swap(true, Ordering::SeqCst) {
                                        state.push_event("config hot-reload: notify watcher crashed, falling back to poll".to_string());
                                    }
                                }
                                WatchMessage::Stop => return,
                            }
                        }
                        if stop_for_coordinator.load(Ordering::SeqCst) {
                            break;
                        }
                        state.reload_from_disk(&watch_path);
                    }
                }
            }
        });

        let poller = thread::spawn(move || {
            let mut previous = file_signature(&poll_path);
            while !stop_for_poller.load(Ordering::SeqCst) {
                thread::park_timeout(poll_interval);
                if stop_for_poller.load(Ordering::SeqCst) {
                    break;
                }
                if !poll_toggle.load(Ordering::SeqCst) {
                    previous = file_signature(&poll_path);
                    continue;
                }

                let current = file_signature(&poll_path);
                if current != previous {
                    previous = current;
                    let _ = poll_tx.send(WatchMessage::FileChanged);
                }
            }
        });
        let poller_thread = poller.thread().clone();

        let _ = ready_rx.recv_timeout(Duration::from_secs(1));

        RuntimeControl {
            stop,
            control_tx,
            coordinator,
            poller,
            poller_thread,
        }
    }

    fn reload_from_disk(&self, path: &Path) {
        self.parse_attempts.fetch_add(1, Ordering::SeqCst);
        let previous = self.snapshot.load_full();
        let candidate = match Config::load_from(path) {
            Ok(config) => config,
            Err(error) => {
                self.push_event(format!(
                    "config hot-reload: parse failed, keeping previous version: {error}"
                ));
                return;
            }
        };

        for field in previous.config.restart_required_fields_changed(&candidate) {
            self.push_event(format!(
                "config hot-reload: {field} requires restart, change ignored"
            ));
        }

        if previous
            .config
            .ingest_gating
            .embedding_classifier
            .prototypes
            != candidate.ingest_gating.embedding_classifier.prototypes
        {
            self.push_event(
                "config hot-reload: prototype change detected, effective after daemon restart"
                    .to_string(),
            );
        }

        if previous.config.llm.max_concurrent != candidate.llm.max_concurrent {
            self.push_event(format!(
                "config hot-reload: llm.max_concurrent changed from {} to {}",
                previous.config.llm.max_concurrent, candidate.llm.max_concurrent
            ));
        }
        if previous.config.llm.enabled_for != candidate.llm.enabled_for {
            self.push_event("config hot-reload: llm.enabled_for changed".to_string());
        }

        let effective = previous.config.merge_runtime_allowed(&candidate);
        let next_version = match effective.effective_hash() {
            Ok(version) => version,
            Err(error) => {
                self.push_event(format!(
                    "config hot-reload: parse failed, keeping previous version: {error}"
                ));
                return;
            }
        };
        if next_version == previous.version {
            return;
        }

        // Compute LLM gen change before `effective` is consumed by from_config.
        let llm_hot_changed = previous.config.llm.base_url != effective.llm.base_url
            || previous.config.llm.model != effective.llm.model
            || previous.config.llm.api_key != effective.llm.api_key
            || previous.config.llm.api_key_env != effective.llm.api_key_env
            || previous.config.llm.extra_body != effective.llm.extra_body
            || previous.config.llm.endpoints != effective.llm.endpoints
            || previous.config.llm.request_timeout_secs != effective.llm.request_timeout_secs
            || previous.config.llm.max_concurrent != effective.llm.max_concurrent
            || previous.config.llm.retry_interval_secs != effective.llm.retry_interval_secs
            || previous.config.llm.enabled_for != effective.llm.enabled_for;
        let embed_hot_changed = previous.config.embed.endpoints != effective.embed.endpoints
            || previous.config.embed.max_concurrent != effective.embed.max_concurrent
            || previous.config.embed.retry != effective.embed.retry;

        let next_snapshot = match ConfigSnapshot::from_config(effective) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.push_event(format!(
                    "config hot-reload: parse failed, keeping previous version: {error}"
                ));
                return;
            }
        };
        self.snapshot.store(Arc::new(next_snapshot));
        // harness-point: PR0 — increment reload counter on successful version change
        self.reload_count.fetch_add(1, Ordering::SeqCst);

        // Notify LLM workers when any hot-reloadable LLM field changes so they
        // can cancel in-flight requests and restart with the new configuration.
        if llm_hot_changed {
            let prev_gen = *self.llm_gen_tx.borrow();
            let _ = self.llm_gen_tx.send(prev_gen.wrapping_add(1));
            self.push_event(format!(
                "config hot-reload: LLM config changed, generation bumped to {}",
                prev_gen.wrapping_add(1)
            ));
        }
        if embed_hot_changed {
            let prev_gen = *self.embed_gen_tx.borrow();
            let _ = self.embed_gen_tx.send(prev_gen.wrapping_add(1));
            self.push_event(format!(
                "config hot-reload: embedding endpoint pool changed, generation bumped to {}",
                prev_gen.wrapping_add(1)
            ));
        }

        self.push_event(format!(
            "config hot-reload: version changed from {} to {}",
            previous.version, next_version
        ));
    }

    fn push_event(&self, event: String) {
        self.record_event(event, true);
    }

    fn record_event(&self, event: String, emit_stderr: bool) {
        if emit_stderr {
            eprintln!("{event}");
        }
        self.persist_event(&event);
        let mut events = self.event_log.lock().expect("event log mutex poisoned");
        if events.len() == MAX_EVENT_LOG {
            let _ = events.pop_front();
        }
        events.push_back(event);
    }

    fn persist_event(&self, event: &str) {
        let Some(path) = self
            .event_log_path
            .lock()
            .expect("event log path mutex poisoned")
            .clone()
        else {
            return;
        };
        if std::fs::metadata(&path)
            .map(|metadata| metadata.len() > MAX_PERSISTED_EVENT_LOG_BYTES)
            .unwrap_or(false)
        {
            let _ = std::fs::write(&path, "");
        }
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", persisted_event_line(event));
            }
            Err(error) => {
                eprintln!(
                    "config hot-reload: failed to persist event to {}: {error}",
                    path.display()
                );
            }
        }
    }
}

pub fn hot_reload_event_log_path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(HOT_RELOAD_EVENT_LOG_FILE)
}

pub fn restart_required_pending_events_from_config_path(config_path: &Path) -> Vec<String> {
    read_restart_required_events_from_path(&hot_reload_event_log_path(config_path))
}

fn read_restart_required_events_from_path(path: &Path) -> Vec<String> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    dedupe_restart_required_events(
        contents
            .lines()
            .filter_map(restart_required_message_from_persisted_line),
    )
}

fn dedupe_restart_required_events(events: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    events
        .into_iter()
        .filter(|event| is_restart_required_event(event))
        .filter(|event| seen.insert(event.clone()))
        .collect()
}

fn is_restart_required_event(event: &str) -> bool {
    event.contains("requires restart, change ignored")
        || event.contains("effective after daemon restart")
}

fn persisted_event_line(event: &str) -> String {
    if !is_restart_required_event(event) {
        return event.to_string();
    }

    let record = PersistedHotReloadEvent {
        mempal_hot_reload_event: HOT_RELOAD_EVENT_KIND_RESTART_REQUIRED.to_string(),
        schema_version: 1,
        pid: std::process::id(),
        message: event.to_string(),
    };
    serde_json::to_string(&record).unwrap_or_else(|_| event.to_string())
}

fn restart_required_message_from_persisted_line(line: &str) -> Option<String> {
    let record = serde_json::from_str::<PersistedHotReloadEvent>(line).ok()?;
    if record.mempal_hot_reload_event != HOT_RELOAD_EVENT_KIND_RESTART_REQUIRED {
        return None;
    }
    if !is_restart_required_event(&record.message) || !process_is_running(record.pid) {
        return None;
    }
    Some(record.message)
}

fn clear_persisted_restart_events_for_pid(path: &Path, pid: u32) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return;
    };

    let mut changed = false;
    let retained = contents
        .lines()
        .filter(
            |line| match serde_json::from_str::<PersistedHotReloadEvent>(line) {
                Ok(record)
                    if record.mempal_hot_reload_event == HOT_RELOAD_EVENT_KIND_RESTART_REQUIRED
                        && record.pid == pid =>
                {
                    changed = true;
                    false
                }
                _ => true,
            },
        )
        .collect::<Vec<_>>();

    if changed {
        let next = if retained.is_empty() {
            String::new()
        } else {
            format!("{}\n", retained.join("\n"))
        };
        let _ = std::fs::write(path, next);
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) does not send a signal; it only checks whether a
    // process exists and whether this process may signal it.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_running(pid: u32) -> bool {
    pid == std::process::id()
}

fn create_watcher(
    tx: mpsc::Sender<WatchMessage>,
    file_name: Option<OsString>,
) -> Option<RecommendedWatcher> {
    notify::recommended_watcher(move |result: notify::Result<Event>| match result {
        Ok(event) if should_reload_event(&event, file_name.as_deref()) => {
            let _ = tx.send(WatchMessage::FileChanged);
        }
        Ok(_) => {}
        Err(_) => {
            let _ = tx.send(WatchMessage::NotifyFailed);
        }
    })
    .ok()
}

fn should_reload_event(event: &Event, file_name: Option<&OsStr>) -> bool {
    if !matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    ) {
        return false;
    }

    event.paths.iter().any(|path| match file_name {
        Some(name) => path.file_name() == Some(name),
        None => true,
    })
}

fn file_signature(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let millis = modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
    Some((millis, metadata.len()))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after unix epoch")
        .as_millis() as u64
}

static HOT_RELOAD_STATE: OnceLock<Arc<HotReloadState>> = OnceLock::new();

pub fn global_hot_reload_state() -> &'static HotReloadState {
    HOT_RELOAD_STATE
        .get_or_init(|| Arc::new(HotReloadState::new()))
        .as_ref()
}

fn global_hot_reload_state_arc() -> Arc<HotReloadState> {
    Arc::clone(HOT_RELOAD_STATE.get_or_init(|| Arc::new(HotReloadState::new())))
}
