use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use super::config::{CompiledPrivacyConfig, Config, ConfigError, ConfigSnapshotMeta};

const MAX_EVENT_LOG: usize = 64;
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(25);

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
}

impl RuntimeControl {
    fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.control_tx.send(WatchMessage::Stop);
        let _ = self.coordinator.join();
        let _ = self.poller.join();
    }
}

pub struct HotReloadState {
    snapshot: ArcSwap<ConfigSnapshot>,
    runtime: Mutex<Option<RuntimeControl>>,
    event_log: Mutex<VecDeque<String>>,
    parse_attempts: AtomicUsize,
    // harness-point: PR0 — counts successful reload applications (version changes)
    reload_count: Arc<AtomicUsize>,
    runtime_prototypes: ArcSwap<Vec<String>>,
}

impl HotReloadState {
    fn new() -> Self {
        let initial = Config::default();
        let snapshot =
            ConfigSnapshot::from_config(initial.clone()).expect("default config is valid");
        Self {
            snapshot: ArcSwap::from_pointee(snapshot),
            runtime: Mutex::new(None),
            event_log: Mutex::new(VecDeque::new()),
            parse_attempts: AtomicUsize::new(0),
            reload_count: Arc::new(AtomicUsize::new(0)),
            runtime_prototypes: ArcSwap::from_pointee(
                initial.ingest_gating.embedding_classifier.prototypes,
            ),
        }
    }

    pub fn bootstrap(&self, path: &Path) -> Result<(), ConfigError> {
        let config = Config::load_from(path)?;
        let snapshot = ConfigSnapshot::from_config(config.clone())?;
        self.snapshot.store(Arc::new(snapshot));
        self.runtime_prototypes.store(Arc::new(
            config.ingest_gating.embedding_classifier.prototypes.clone(),
        ));
        self.push_event(format!(
            "config hot-reload: bootstrapped version {}",
            self.snapshot_meta().version
        ));

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
                thread::sleep(poll_interval);
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

        let _ = ready_rx.recv_timeout(Duration::from_secs(1));

        RuntimeControl {
            stop,
            control_tx,
            coordinator,
            poller,
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
        self.push_event(format!(
            "config hot-reload: version changed from {} to {}",
            previous.version, next_version
        ));
    }

    fn push_event(&self, event: String) {
        eprintln!("{event}");
        let mut events = self.event_log.lock().expect("event log mutex poisoned");
        if events.len() == MAX_EVENT_LOG {
            let _ = events.pop_front();
        }
        events.push_back(event);
    }
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
