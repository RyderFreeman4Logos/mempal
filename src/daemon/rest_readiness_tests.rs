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
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use crate::{
    bootstrap_events::BootstrapEvent, core::db::Database, daemon_bootstrap::DaemonContext,
};

use super::{
    global_shutdown_test_lock, notify_systemd_ready, request_shutdown, reset_shutdown_request,
    run_loop,
};

static NOTIFY_ENV_LOCK: Mutex<()> = Mutex::new(());

struct NotifySocketEnv {
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl NotifySocketEnv {
    fn set(path: &Path) -> Self {
        Self::set_value(path.as_os_str().to_owned())
    }

    fn set_value(value: OsString) -> Self {
        let lock = NOTIFY_ENV_LOCK.lock().expect("NOTIFY_SOCKET env lock");
        let previous = std::env::var_os("NOTIFY_SOCKET");
        // SAFETY: the test lock serializes this process-global environment mutation.
        unsafe {
            std::env::set_var("NOTIFY_SOCKET", value);
        }
        Self {
            previous,
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
    let runtime_root = tempdir.path().join("runtime");
    let context = tokio::task::spawn_blocking(move || {
        DaemonContext::bootstrap_with_events_for_test(
            config_path,
            true,
            Some(events),
            &runtime_root,
        )
        .expect("bootstrap REST readiness fixture")
    })
    .await
    .expect("REST readiness bootstrap task panicked");
    let mut context = context;
    context
        .runtime
        .take()
        .expect("daemon bootstrap runtime")
        .shutdown_background();
    context
}

async fn run_loop_with_timeout(context: DaemonContext) -> anyhow::Result<()> {
    let mut run_task = tokio::spawn(async move { run_loop(&context).await });
    match tokio::time::timeout(Duration::from_secs(2), &mut run_task).await {
        Ok(result) => result.expect("daemon task panicked"),
        Err(_) => {
            run_task.abort();
            let _ = run_task.await;
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
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();

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
    assert!(
        receive_notify_packet(&occupied_notify, Duration::from_millis(100))
            .await
            .is_none(),
        "REST bind failure must not send READY=1"
    );
    drop(_occupied_notify_env);
    drop(occupied_listener);
    assert!(
        Database::open(&occupied_db_path).is_ok(),
        "REST readiness fixture database should remain readable"
    );

    let serving_tempdir = tempfile::tempdir().expect("create serving REST fixture");
    let reserved_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve REST address");
    let serving_addr = reserved_listener.local_addr().expect("read REST address");
    drop(reserved_listener);
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
    assert!(
        receive_notify_packet(&serving_notify, Duration::from_millis(100))
            .await
            .is_none(),
        "successful startup must send only one READY=1 packet"
    );

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

#[cfg(target_os = "linux")]
fn fill_notify_receiver_queue(path: &Path) -> Vec<StdUnixDatagram> {
    let mut fillers = Vec::new();
    let payload = b"READY=1";
    for _ in 0..4_096 {
        let filler = StdUnixDatagram::unbound().expect("create notification queue filler");
        filler
            .set_nonblocking(true)
            .expect("set notification queue filler nonblocking");
        match filler.send_to(payload, path) {
            Ok(_) => fillers.push(filler),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return fillers,
            Err(error) => panic!("fill notification receiver queue: {error}"),
        }
    }
    panic!("notification receiver queue did not become full");
}

#[cfg(target_os = "linux")]
#[test]
fn systemd_notify_returns_bounded_error_when_receiver_queue_is_full() {
    let tempdir = tempfile::tempdir().expect("create full notification queue fixture");
    let notify_path = tempdir.path().join("notify.sock");
    let receiver = StdUnixDatagram::bind(&notify_path).expect("bind full notification receiver");
    let fillers = fill_notify_receiver_queue(&notify_path);
    assert!(
        !fillers.is_empty(),
        "notification queue filler sent no packets"
    );

    let _notify_env = NotifySocketEnv::set(&notify_path);
    let (result_sender, result_receiver) = std::sync::mpsc::channel();
    let notifier = std::thread::spawn(move || {
        result_sender
            .send(notify_systemd_ready())
            .expect("send notification test result");
    });

    let (result, timed_out) = match result_receiver.recv_timeout(Duration::from_millis(250)) {
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
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();

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

#[tokio::test]
async fn systemd_notify_send_failure_is_propagated_without_leaking_socket_path() {
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();

    let tempdir = tempfile::tempdir().expect("create notification failure fixture");
    let notify_path = tempdir.path().join("notify-secret-path.sock");
    let (_db_path, config_path) = write_fixture(&tempdir, "127.0.0.1:0".parse().unwrap());
    let (events, _receiver) = tokio::sync::mpsc::channel(16);
    let context = bootstrap_fixture(config_path, &tempdir, events).await;

    let result = {
        let _notify_env = NotifySocketEnv::set(&notify_path);
        run_loop_with_timeout(context).await
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
}

#[tokio::test]
async fn systemd_notify_non_unicode_socket_value_is_an_error() {
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();

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
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();

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
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();

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
    let _shutdown_lock = global_shutdown_test_lock().lock_owned().await;
    reset_shutdown_request();

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
    assert!(
        receive_notify_packet(&notify_socket, Duration::from_millis(100))
            .await
            .is_none(),
        "API-disabled startup must not send false READY=1"
    );
}

#[test]
fn systemd_unit_uses_notify_access_main() {
    let service = include_str!("../../contrib/systemd/mempal-daemon.service")
        .split_once("[Service]")
        .expect("systemd unit service section")
        .1;
    assert!(
        service.lines().any(|line| line.trim() == "Type=notify")
            && service
                .lines()
                .any(|line| line.trim() == "NotifyAccess=main"),
        "systemd unit must use main-process readiness notifications"
    );
}
