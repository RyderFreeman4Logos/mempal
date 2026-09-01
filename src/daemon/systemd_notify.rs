#[cfg(unix)]
pub(crate) fn validate_api_enabled(api_enabled: bool) -> anyhow::Result<()> {
    if !api_enabled && std::env::var_os("NOTIFY_SOCKET").is_some() {
        anyhow::bail!("systemd readiness requires an enabled API");
    }
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn validate_api_enabled(_api_enabled: bool) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn notify_systemd_ready() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::{ffi::OsStrExt, net::UnixDatagram};
    use std::path::Path;

    let Some(socket_path) = std::env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    if !cfg!(feature = "rest") {
        anyhow::bail!("systemd readiness requires a compiled REST transport");
    }
    let socket = UnixDatagram::unbound().context("failed to create systemd readiness notifier")?;
    socket
        .set_nonblocking(true)
        .context("failed to configure systemd readiness notifier")?;
    let socket_bytes = socket_path.as_os_str().as_bytes();
    if socket_bytes.is_empty() {
        anyhow::bail!("systemd readiness notification address is invalid");
    }

    #[cfg(target_os = "linux")]
    if let Some(name) = socket_bytes.strip_prefix(b"@") {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;
        if name.is_empty() {
            anyhow::bail!("systemd readiness notification address is invalid");
        }
        let address = SocketAddr::from_abstract_name(name)
            .map_err(|_| anyhow::anyhow!("systemd readiness notification address is invalid"))?;
        socket
            .send_to_addr(b"READY=1", &address)
            .context("failed to send systemd readiness notification")?;
        return Ok(());
    }

    socket
        .send_to(b"READY=1", Path::new(&socket_path))
        .context("failed to send systemd readiness notification")?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn notify_systemd_ready() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(all(test, unix, not(feature = "rest")))]
mod tests {
    use std::os::unix::net::UnixDatagram;

    use super::{notify_systemd_ready, validate_api_enabled};

    #[tokio::test]
    async fn no_rest_build_refuses_systemd_readiness() {
        let _shutdown_lock = crate::daemon::global_shutdown_test_lock()
            .lock_owned()
            .await;
        let tempdir = tempfile::tempdir().expect("create no-REST notification fixture");
        let notify_path = tempdir.path().join("notify.sock");
        let receiver =
            UnixDatagram::bind(&notify_path).expect("bind no-REST notification receiver");
        receiver
            .set_nonblocking(true)
            .expect("set no-REST notification receiver nonblocking");
        let previous = std::env::var_os("NOTIFY_SOCKET");
        // SAFETY: the daemon test lock serializes production-path tests that read this variable.
        unsafe { std::env::set_var("NOTIFY_SOCKET", &notify_path) };

        let result = validate_api_enabled(true).and_then(|()| notify_systemd_ready());
        let mut packet = [0_u8; 128];
        let received = receiver.recv(&mut packet);

        // SAFETY: restore the process-global variable before releasing the daemon test lock.
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("NOTIFY_SOCKET", previous);
            } else {
                std::env::remove_var("NOTIFY_SOCKET");
            }
        }

        let rejected = result
            .as_ref()
            .is_err_and(|error| error.to_string().contains("REST transport"));
        let no_packet = matches!(
            received,
            Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock
        );
        assert!(
            rejected && no_packet,
            "no-REST startup must reject systemd readiness without sending READY=1: result={result:?}, received={received:?}"
        );
    }
}
