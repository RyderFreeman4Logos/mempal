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
    let socket = UnixDatagram::unbound().context("failed to create systemd readiness notifier")?;
    let socket_bytes = socket_path.as_os_str().as_bytes();

    #[cfg(target_os = "linux")]
    if let Some(name) = socket_bytes.strip_prefix(b"@") {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;
        let name = std::str::from_utf8(name)
            .map_err(|_| anyhow::anyhow!("systemd readiness notification address is invalid"))?;
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
