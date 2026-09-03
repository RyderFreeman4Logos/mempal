#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::linux::net::SocketAddrExt;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(target_os = "linux")]
use std::os::unix::net::{SocketAddr as UnixSocketAddr, UnixDatagram as StdUnixDatagram};
use std::{
    ffi::OsString,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration,
};

use crate::{
    bootstrap_events::BootstrapEvent,
    core::db::{Database, DbError},
    daemon_bootstrap::DaemonContext,
};

use super::{
    global_shutdown_test_lock, notify_systemd_ready, request_shutdown, reset_shutdown_request,
    run_loop,
};

static NOTIFY_ENV_LOCK: Mutex<()> = Mutex::new(());
struct NotifySocketEnv {
    previous: Option<OsString>,
    previous_home: Option<OsString>,
    restore_home: bool,
    _lock: MutexGuard<'static, ()>,
}
impl NotifySocketEnv {
    fn set(path: &Path) -> Self {
        Self::set_value(path.as_os_str().to_owned())
    }
    fn set_value(value: OsString) -> Self {
        Self::set_value_and_home(value, None)
    }
    fn set_with_home(path: &Path, home: &Path) -> Self {
        Self::set_value_and_home(path.as_os_str().to_owned(), Some(home))
    }
    fn set_value_and_home(value: OsString, home: Option<&Path>) -> Self {
        let lock = NOTIFY_ENV_LOCK.lock().expect("NOTIFY_SOCKET env lock");
        let previous = std::env::var_os("NOTIFY_SOCKET");
        let previous_home = std::env::var_os("HOME");
        // SAFETY: the test lock serializes this process-global environment mutation.
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", value);
            if let Some(home) = home {
                std::env::set_var("HOME", home);
            }
        }
        Self {
            previous,
            previous_home,
            restore_home: home.is_some(),
            _lock: lock,
        }
    }
}
impl Drop for NotifySocketEnv {
    fn drop(&mut self) {
        // SAFETY: the test lock serializes this process-global environment restore.
        unsafe {
            if let Some(previous) = &self.previous {
                std::env::set_var("NOTIFY_SOCKET", previous);
            } else {
                std::env::remove_var("NOTIFY_SOCKET");
            }
            if self.restore_home {
                if let Some(previous_home) = &self.previous_home {
                    std::env::set_var("HOME", previous_home);
                } else {
                    std::env::remove_var("HOME");
                }
            }
        }
    }
}
async fn receive_notify_packet(
    socket: &tokio::net::UnixDatagram,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let mut packet = [0_u8; 128];
    match tokio::time::timeout(timeout, socket.recv(&mut packet)).await {
        Ok(Ok(length)) => Some(packet[..length].to_vec()),
        Ok(Err(error)) => panic!("receive systemd notification: {error}"),
        Err(_) => None,
    }
}
fn rest_readiness_test_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
}
async fn acquire_shutdown_test_lock() -> (
    tokio::sync::OwnedMutexGuard<()>,
    tokio::sync::OwnedMutexGuard<()>,
) {
    let rest = rest_readiness_test_lock().lock_owned().await;
    let lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();
    (rest, lock)
}
async fn assert_no_ready_packet(socket: &tokio::net::UnixDatagram, message: &str) {
    assert!(
        receive_notify_packet(socket, Duration::from_millis(100))
            .await
            .is_none(),
        "{message}"
    );
}
async fn assert_resources_released(api_addr: SocketAddr, db_path: &Path, label: &str) {
    tokio::net::TcpListener::bind(api_addr)
        .await
        .unwrap_or_else(|error| panic!("{label} must release the REST listener: {error}"));
    assert_eq!(
        crate::core::queue::queue_stats_readonly(db_path)
            .unwrap_or_else(|error| panic!("read queue after {label}: {error}"))
            .claimed,
        0,
        "{label} must leave no claimed queue work"
    );
    assert_eq!(
        rusqlite::Connection::open(db_path)
            .unwrap_or_else(|error| panic!("open {label} database: {error}"))
            .query_row("SELECT COUNT(*) FROM runtime_writer_leases", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap_or_else(|error| panic!("count runtime writer leases after {label}: {error}")),
        0,
        "{label} must release the daemon writer lease"
    );
}
async fn reserve_rest_address() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve REST address");
    let address = listener.local_addr().expect("read REST address");
    drop(listener);
    address
}
fn write_fixture(
    tempdir: &tempfile::TempDir,
    api_addr: SocketAddr,
) -> (std::path::PathBuf, std::path::PathBuf) {
    write_fixture_with_options(tempdir, api_addr, true, false)
}
fn write_fixture_with_options(
    tempdir: &tempfile::TempDir,
    api_addr: SocketAddr,
    api_enabled: bool,
    hooks_enabled: bool,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let db_path = tempdir.path().join("palace.db");
    let config_path = tempdir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "db_path = \"{}\"\n\n[api]\nenabled = {api_enabled}\naddr = \"{api_addr}\"\n\n[embed]\nbackend = \"stub\"\n\n[hooks]\nenabled = {hooks_enabled}\n\n[ingest_gating]\nenabled = false\n",
            db_path.display()
        ),
    )
    .expect("write REST readiness config");
    (db_path, config_path)
}
async fn bootstrap_fixture(
    config_path: std::path::PathBuf,
    tempdir: &tempfile::TempDir,
    events: tokio::sync::mpsc::Sender<BootstrapEvent>,
) -> DaemonContext {
    let mut context = bootstrap_fixture_with_runtime(config_path, tempdir, events).await;
    context
        .runtime
        .take()
        .expect("daemon bootstrap runtime")
        .shutdown_background();
    context
}
async fn bootstrap_fixture_with_runtime(
    config_path: std::path::PathBuf,
    tempdir: &tempfile::TempDir,
    events: tokio::sync::mpsc::Sender<BootstrapEvent>,
) -> DaemonContext {
    let runtime_root = tempdir.path().join("runtime");
    tokio::task::spawn_blocking(move || {
        DaemonContext::bootstrap_with_events_for_test(
            config_path,
            true,
            Some(events),
            &runtime_root,
        )
        .expect("bootstrap REST readiness fixture")
    })
    .await
    .expect("REST readiness bootstrap task panicked")
}
async fn run_loop_with_runtime_shutdown(mut context: DaemonContext) -> anyhow::Result<()> {
    let runtime = context
        .runtime
        .take()
        .expect("daemon bootstrap runtime for isolated run-loop");
    tokio::task::spawn_blocking(move || {
        let result = runtime.block_on(run_loop(&context));
        runtime.shutdown_timeout(Duration::from_secs(1));
        result
    })
    .await
    .expect("isolated daemon run-loop task panicked")
}
async fn run_loop_with_timeout(context: DaemonContext) -> anyhow::Result<()> {
    let mut run_task = tokio::spawn(async move { run_loop(&context).await });
    match tokio::time::timeout(Duration::from_secs(7), &mut run_task).await {
        Ok(result) => result.expect("daemon task panicked"),
        Err(_) => {
            request_shutdown();
            match tokio::time::timeout(Duration::from_secs(5), &mut run_task).await {
                Ok(result) => {
                    let _ = result.expect("daemon task panicked");
                }
                Err(_) => {
                    run_task.abort();
                    let _ = run_task.await;
                }
            }
            Err(anyhow::anyhow!(
                "daemon startup did not finish within the test deadline"
            ))
        }
    }
}
fn has_ready_event(receiver: &mut tokio::sync::mpsc::Receiver<BootstrapEvent>) -> bool {
    loop {
        match receiver.try_recv() {
            Ok(BootstrapEvent::Ready) => return true,
            Ok(_) => {}
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return false,
        }
    }
}
#[tokio::test]
async fn daemon_ready_requires_a_serving_rest_listener_and_rejects_bind_failure() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let occupied_tempdir = tempfile::tempdir().expect("create occupied REST fixture");
    let occupied_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve occupied REST address");
    let occupied_addr = occupied_listener
        .local_addr()
        .expect("read occupied REST address");
    let (occupied_db_path, occupied_config_path) = write_fixture(&occupied_tempdir, occupied_addr);
    let occupied_notify_path = occupied_tempdir.path().join("notify.sock");
    let occupied_notify = tokio::net::UnixDatagram::bind(&occupied_notify_path)
        .expect("bind occupied startup notification receiver");
    let _occupied_notify_env = NotifySocketEnv::set(&occupied_notify_path);
    let (occupied_events, mut occupied_receiver) = tokio::sync::mpsc::channel(16);
    let occupied_context =
        bootstrap_fixture(occupied_config_path, &occupied_tempdir, occupied_events).await;

    assert!(
        !has_ready_event(&mut occupied_receiver),
        "daemon-ready was advertised before REST bind"
    );
    let run_task = tokio::spawn(async move { run_loop(&occupied_context).await });
    let result = tokio::time::timeout(Duration::from_secs(5), run_task)
        .await
        .expect("REST bind failure must be reported promptly")
        .expect("daemon task panicked");
    assert!(
        result.is_err(),
        "daemon must not continue after REST bind failure"
    );
    assert!(
        !has_ready_event(&mut occupied_receiver),
        "daemon-ready was advertised after REST bind failure"
    );
    assert_no_ready_packet(&occupied_notify, "REST bind failure must not send READY=1").await;
    drop(_occupied_notify_env);
    drop(occupied_listener);
    assert!(
        Database::open(&occupied_db_path).is_ok(),
        "REST readiness fixture database should remain readable"
    );
    let serving_tempdir = tempfile::tempdir().expect("create serving REST fixture");
    let serving_addr = reserve_rest_address().await;
    let (serving_db_path, serving_config_path) = write_fixture(&serving_tempdir, serving_addr);
    let serving_notify_path = serving_tempdir.path().join("notify.sock");
    let serving_notify = tokio::net::UnixDatagram::bind(&serving_notify_path)
        .expect("bind serving startup notification receiver");
    let _serving_notify_env = NotifySocketEnv::set(&serving_notify_path);
    let (serving_events, mut serving_receiver) = tokio::sync::mpsc::channel(16);
    let serving_context =
        bootstrap_fixture(serving_config_path, &serving_tempdir, serving_events).await;

    assert!(
        !has_ready_event(&mut serving_receiver),
        "daemon-ready was advertised before REST serving"
    );
    let serving_task = tokio::spawn(async move { run_loop(&serving_context).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match serving_receiver.recv().await {
                Some(BootstrapEvent::Ready) => break,
                Some(_) => {}
                None => panic!("REST readiness event channel closed"),
            }
        }
    })
    .await
    .expect("REST readiness event did not arrive");
    assert_eq!(
        receive_notify_packet(&serving_notify, Duration::from_secs(5)).await,
        Some(b"READY=1".to_vec()),
        "successful startup must send exactly READY=1 after final readiness"
    );
    assert_no_ready_packet(
        &serving_notify,
        "successful startup must send only one READY=1 packet",
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://{serving_addr}/api/ingest/durable"))
        .json(&serde_json::json!({
            "idempotency_key": "rest-readiness-test",
            "request": {"content": "serving REST", "wing": "smoke"}
        }))
        .send()
        .await
        .expect("send durable ingest through serving REST listener");
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    request_shutdown();
    tokio::time::timeout(Duration::from_secs(5), serving_task)
        .await
        .expect("serving REST fixture did not stop")
        .expect("serving REST fixture task panicked")
        .expect("serving REST fixture failed");
    assert!(Database::open(&serving_db_path).is_ok());
}
#[tokio::test]
async fn systemd_ready_rejects_a_writer_lease_lost_after_admission() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let tempdir = tempfile::tempdir().expect("create stale writer lease fixture");
    let notify_path = tempdir.path().join("notify.sock");
    let notify_socket = tokio::net::UnixDatagram::bind(&notify_path)
        .expect("bind stale writer lease notification receiver");
    let (_db_path, config_path) = write_fixture(
        &tempdir,
        "127.0.0.1:0".parse().expect("parse ephemeral REST address"),
    );
    let _process_env = NotifySocketEnv::set_with_home(&notify_path, tempdir.path());
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture(config_path, &tempdir, events).await;
    context
        .db
        .lock()
        .await
        .conn()
        .execute_batch(
            "CREATE TRIGGER invalidate_daemon_writer_lease_after_admission
             AFTER INSERT ON runtime_writer_leases
             WHEN NEW.name = 'sqlite-writer' AND NEW.mode = 'daemon'
             BEGIN
               DELETE FROM runtime_writer_leases
               WHERE name = NEW.name
                 AND owner = NEW.owner
                 AND session_id = NEW.session_id
                 AND generation = NEW.generation;
             END;",
        )
        .expect("install stale writer lease fixture trigger");
    let result = run_loop_with_timeout(context).await;
    assert_no_ready_packet(&notify_socket, "stale writer lease must not send READY=1").await;
    let error = result.expect_err("stale writer lease must fail final readiness");
    assert!(
        error.chain().any(|cause| matches!(
            cause.downcast_ref::<DbError>(),
            Some(DbError::RuntimeWriterLeaseLost { .. })
        )),
        "stale writer lease must return the typed lease-loss error: {error:#}"
    );
}
#[cfg(target_os = "linux")]
fn fill_notify_receiver_queue(path: &Path) -> Vec<StdUnixDatagram> {
    let filler = StdUnixDatagram::unbound().expect("create notification queue filler");
    filler
        .set_nonblocking(true)
        .expect("set notification queue filler nonblocking");
    let send_buffer_size: libc::c_int = 1 << 20;
    // SAFETY: filler owns a valid datagram fd and the option value is a live c_int.
    let result = unsafe {
        libc::setsockopt(
            filler.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_SNDBUF,
            (&send_buffer_size as *const libc::c_int).cast(),
            std::mem::size_of_val(&send_buffer_size) as libc::socklen_t,
        )
    };
    assert_eq!(
        result,
        0,
        "set notification queue filler send buffer: {}",
        std::io::Error::last_os_error()
    );
    let payload = b"READY=1";
    for _ in 0..4_096 {
        match filler.send_to(payload, path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return vec![filler],
            Err(error) => panic!("fill notification receiver queue: {error}"),
        }
    }
    panic!("notification receiver queue did not become full");
}
#[cfg(target_os = "linux")]
#[test]
fn systemd_notify_returns_bounded_error_when_receiver_queue_is_full() {
    let _rest_lock = rest_readiness_test_lock().blocking_lock_owned();
    let tempdir = tempfile::tempdir().expect("create full notification queue fixture");
    let notify_path = tempdir.path().join("notify.sock");
    let receiver = StdUnixDatagram::bind(&notify_path).expect("bind full notification receiver");
    let fillers = fill_notify_receiver_queue(&notify_path);
    assert_eq!(
        fillers.len(),
        1,
        "notification queue saturation must retain one sender FD"
    );
    let _notify_env = NotifySocketEnv::set(&notify_path);
    let (result_sender, result_receiver) = std::sync::mpsc::channel();
    let notifier = std::thread::spawn(move || {
        result_sender
            .send(notify_systemd_ready())
            .expect("send notification test result");
    });
    let (result, timed_out) = match result_receiver.recv_timeout(Duration::from_secs(2)) {
        Ok(result) => (result, false),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let mut packet = [0_u8; 4096];
            receiver
                .recv(&mut packet)
                .expect("drain notification receiver queue");
            let result = result_receiver
                .recv_timeout(Duration::from_secs(1))
                .expect("blocked notification did not finish after queue cleanup");
            (result, true)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("notification test thread exited without a result")
        }
    };
    notifier.join().expect("notification test thread panicked");
    assert!(
        !timed_out,
        "systemd readiness notification blocked on a full receiver queue"
    );
    let error = result.expect_err("a full notification receiver queue must return an error");
    assert_eq!(
        error
            .root_cause()
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind),
        Some(std::io::ErrorKind::WouldBlock),
        "full-queue errors must preserve WouldBlock"
    );
    assert!(
        error.to_string().contains("systemd readiness notification"),
        "full-queue errors must retain notification context: {error:#}"
    );
    assert!(
        !error.to_string().contains(notify_path.to_str().unwrap()),
        "full-queue errors must not expose the socket path: {error:#}"
    );
}
#[tokio::test]
async fn systemd_notify_empty_socket_values_fail_bounded_without_leaking_value() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    for value in [OsString::new(), OsString::from("@")] {
        let tempdir = tempfile::tempdir().expect("create empty notification fixture");
        let (_db_path, config_path) = write_fixture(&tempdir, "127.0.0.1:0".parse().unwrap());
        let (events, _receiver) = tokio::sync::mpsc::channel(16);
        let context = bootstrap_fixture(config_path, &tempdir, events).await;
        let result = {
            let _notify_env = NotifySocketEnv::set_value(value.clone());
            run_loop_with_timeout(context).await
        };
        let error = result.expect_err("an empty systemd notification value must fail startup");
        assert!(
            error.to_string().contains("systemd readiness notification"),
            "empty notification values must fail with notification context: {error:#}"
        );
        if !value.is_empty() {
            assert!(
                !error.to_string().contains(value.to_str().unwrap()),
                "empty notification errors must not expose the raw value: {error:#}"
            );
        }
    }
}
#[cfg(target_os = "linux")]
#[test]
fn systemd_notify_rejects_bound_empty_abstract_address_without_sending() {
    use std::io::ErrorKind;

    let _rest_lock = rest_readiness_test_lock().blocking_lock_owned();
    let address = UnixSocketAddr::from_abstract_name("")
        .expect("empty abstract address should be constructible");
    let receiver = StdUnixDatagram::bind_addr(&address).expect("bind empty abstract receiver");
    receiver
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("bound empty abstract receiver read timeout");
    let result = {
        let _notify_env = NotifySocketEnv::set_value(OsString::from("@"));
        notify_systemd_ready()
    };
    let mut packet = [0_u8; 128];
    let receive_result = receiver.recv(&mut packet);
    assert_eq!(
        result.as_ref().err().map(ToString::to_string).as_deref(),
        Some("systemd readiness notification address is invalid"),
        "bound empty abstract receiver observed {receive_result:?}"
    );
    assert!(
        matches!(
            receive_result,
            Err(ref error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock)
        ),
        "invalid empty abstract address must not send READY=1: {receive_result:?}"
    );
}
#[cfg(target_os = "linux")]
#[test]
fn systemd_notify_delivers_to_non_unicode_abstract_address() {
    let _rest_lock = rest_readiness_test_lock().blocking_lock_owned();
    let abstract_name = b"mempal-\xff".to_vec();
    let address = UnixSocketAddr::from_abstract_name(&abstract_name)
        .expect("non-Unicode abstract address should be constructible");
    let receiver = StdUnixDatagram::bind_addr(&address)
        .expect("bind non-Unicode abstract notification receiver");
    receiver
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("non-Unicode abstract receiver read timeout");
    let mut notify_value = vec![b'@'];
    notify_value.extend_from_slice(&abstract_name);
    let result = {
        let _notify_env = NotifySocketEnv::set_value(OsString::from_vec(notify_value));
        notify_systemd_ready()
    };
    let mut packet = [0_u8; 128];
    let first_packet = receiver
        .recv(&mut packet)
        .ok()
        .map(|length| packet[..length].to_vec());
    receiver
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("non-Unicode abstract second-packet read timeout");
    let second_packet = receiver.recv(&mut packet);
    assert!(
        result.is_ok(),
        "non-Unicode abstract notification should send successfully: {result:?}"
    );
    assert_eq!(
        first_packet,
        Some(b"READY=1".to_vec()),
        "non-Unicode abstract notification must send exactly READY=1"
    );
    assert!(
        matches!(second_packet, Err(ref error) if error.kind() == std::io::ErrorKind::TimedOut || error.kind() == std::io::ErrorKind::WouldBlock),
        "non-Unicode abstract notification must send only one packet: {second_packet:?}"
    );
}
#[tokio::test]
async fn systemd_notify_send_failure_is_propagated_without_leaking_socket_path() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let tempdir = tempfile::tempdir().expect("create notification failure fixture");
    let notify_path = tempdir.path().join("notify-secret-path.sock");
    let api_addr = reserve_rest_address().await;
    let (db_path, config_path) = write_fixture(&tempdir, api_addr);
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture(config_path, &tempdir, events).await;

    let result = {
        let _notify_env = NotifySocketEnv::set(&notify_path);
        tokio::time::timeout(Duration::from_secs(5), run_loop(&context))
            .await
            .expect("missing notification socket must not stall startup")
    };
    let error = result.expect_err("missing systemd notification socket must fail startup");
    assert!(
        error.to_string().contains("systemd readiness notification"),
        "startup should return the notification failure, not a test timeout: {error:#}"
    );
    assert!(
        !error.to_string().contains(notify_path.to_str().unwrap()),
        "systemd notification errors must not expose the socket path: {error:#}"
    );
    assert_resources_released(api_addr, &db_path, "notification failure").await;
}
#[tokio::test]
async fn run_loop_timeout_drains_children_before_restoring_notify_environment() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let tempdir = tempfile::tempdir().expect("create timeout cleanup fixture");
    let api_addr = reserve_rest_address().await;
    let (_db_path, config_path) = write_fixture(&tempdir, api_addr);
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture(config_path, &tempdir, events).await;

    let result = {
        let _notify_env = NotifySocketEnv::set(&tempdir.path().join("notify.sock"));
        run_loop_with_timeout(context).await
    };
    assert!(
        result.is_err(),
        "the timeout helper must report its expired deadline"
    );
    tokio::net::TcpListener::bind(api_addr)
        .await
        .expect("timeout cleanup must release the REST listener");
}
#[tokio::test]
async fn systemd_notify_non_unicode_socket_value_is_an_error() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let tempdir = tempfile::tempdir().expect("create non-Unicode notification fixture");
    let (_db_path, config_path) = write_fixture(&tempdir, "127.0.0.1:0".parse().unwrap());
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture(config_path, &tempdir, events).await;

    let result = {
        let _notify_env = NotifySocketEnv::set_value(OsString::from_vec(vec![
            b'/', b'n', b'o', b't', b'i', b'f', b'y', b'-', 0xff,
        ]));
        run_loop_with_timeout(context).await
    };
    let error =
        result.expect_err("non-Unicode systemd notification socket values must fail startup");
    assert!(
        error.to_string().contains("systemd readiness notification"),
        "startup should return the notification failure, not a test timeout: {error:#}"
    );
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn systemd_notify_abstract_address_errors_are_propagated() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let tempdir = tempfile::tempdir().expect("create abstract notification fixture");
    let abstract_name = "x".repeat(256);
    let (_db_path, config_path) = write_fixture(&tempdir, "127.0.0.1:0".parse().unwrap());
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture(config_path, &tempdir, events).await;

    let result = {
        let _notify_env = NotifySocketEnv::set_value(OsString::from(format!("@{abstract_name}")));
        run_loop_with_timeout(context).await
    };
    let error = result.expect_err("an invalid Linux abstract address must fail startup");
    assert!(
        error.to_string().contains("systemd readiness notification"),
        "startup should return the notification failure, not a test timeout: {error:#}"
    );
    assert!(
        !error.to_string().contains(&abstract_name),
        "systemd notification errors must not expose the abstract socket name: {error:#}"
    );
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn systemd_notify_supports_linux_abstract_namespace() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let tempdir = tempfile::tempdir().expect("create abstract notification fixture");
    let abstract_name = format!("mempal-notify-{}", std::process::id());
    let address = UnixSocketAddr::from_abstract_name(&abstract_name).expect("abstract address");
    let socket = StdUnixDatagram::bind_addr(&address).expect("bind abstract receiver");
    socket
        .set_nonblocking(true)
        .expect("set abstract receiver nonblocking");
    let socket = tokio::net::UnixDatagram::from_std(socket).expect("adopt abstract receiver");
    let (_db_path, config_path) =
        write_fixture_with_options(&tempdir, "127.0.0.1:0".parse().unwrap(), true, true);
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture(config_path, &tempdir, events).await;

    let (packet, second_packet, run_result) = {
        let _notify_env = NotifySocketEnv::set_value(OsString::from(format!("@{abstract_name}")));
        let run_task = tokio::spawn(async move { run_loop(&context).await });
        let packet = receive_notify_packet(&socket, Duration::from_secs(2)).await;
        let second_packet = receive_notify_packet(&socket, Duration::from_millis(100)).await;
        request_shutdown();
        let run_result = tokio::time::timeout(Duration::from_secs(5), run_task)
            .await
            .expect("abstract notification fixture did not stop")
            .expect("abstract notification fixture task panicked");
        (packet, second_packet, run_result)
    };
    assert_eq!(packet, Some(b"READY=1".to_vec()));
    assert!(
        second_packet.is_none(),
        "successful startup must send only one READY=1 packet"
    );
    run_result.expect("abstract notification fixture failed");
}
#[tokio::test]
async fn systemd_notify_fails_when_api_is_disabled() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let tempdir = tempfile::tempdir().expect("create API-disabled notification fixture");
    let notify_path = tempdir.path().join("notify.sock");
    let notify_socket = tokio::net::UnixDatagram::bind(&notify_path)
        .expect("bind API-disabled notification receiver");
    let (_db_path, config_path) =
        write_fixture_with_options(&tempdir, "127.0.0.1:0".parse().unwrap(), false, false);
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture(config_path, &tempdir, events).await;

    let result = {
        let _notify_env = NotifySocketEnv::set(&notify_path);
        run_loop_with_timeout(context).await
    };
    let error = result.expect_err("configured systemd readiness requires an enabled API");
    assert!(
        error.to_string().contains("API"),
        "API-disabled systemd readiness failure should be explicit: {error:#}"
    );
    assert_no_ready_packet(
        &notify_socket,
        "API-disabled startup must not send false READY=1",
    )
    .await;
}
#[tokio::test]
async fn recovery_publication_failure_returns_error_without_ready_or_child_leaks() {
    let _shutdown_lock = acquire_shutdown_test_lock().await;

    let tempdir = tempfile::tempdir().expect("create recovery publication fixture");
    let api_addr = reserve_rest_address().await;
    let (db_path, config_path) = write_fixture(&tempdir, api_addr);
    let notify_path = tempdir.path().join("notify.sock");
    let notify_socket = tokio::net::UnixDatagram::bind(&notify_path)
        .expect("bind recovery publication notification receiver");
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture_with_runtime(config_path, &tempdir, events).await;
    let temporary = context
        .mempal_home
        .join(format!("daemon-recovery.json.tmp.{}", std::process::id()));
    std::fs::create_dir(&temporary).expect("make only recovery publication temporary path fail");
    let result = {
        let _notify_env = NotifySocketEnv::set(&notify_path);
        run_loop_with_runtime_shutdown(context).await
    };
    std::fs::remove_dir(&temporary).expect("restore recovery publication path");
    let error = result.expect_err("recovery publication failure must fail startup");
    assert!(
        error.chain().any(|cause| cause
            .to_string()
            .contains("failed to mark daemon recovery complete")),
        "startup should return the recovery publication failure: {error:#}"
    );
    assert_no_ready_packet(
        &notify_socket,
        "recovery publication failure must not send READY=1",
    )
    .await;
    assert_resources_released(api_addr, &db_path, "recovery publication failure").await;
}
