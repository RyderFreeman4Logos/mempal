#[cfg(unix)]
pub(crate) fn notify_systemd_ready() {
    use std::os::unix::net::{SocketAddr, UnixDatagram};
    use std::path::Path;

    let Ok(socket_path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let _ = UnixDatagram::unbound().and_then(|socket| {
        #[cfg(target_os = "linux")]
        if let Some(name) = socket_path.strip_prefix('@') {
            use std::os::linux::net::SocketAddrExt;
            let address = SocketAddr::from_abstract_name(name)?;
            return socket.send_to_addr(b"READY=1", &address).map(drop);
        }
        socket
            .send_to(b"READY=1", Path::new(&socket_path))
            .map(drop)
    });
}

#[cfg(not(unix))]
pub(crate) fn notify_systemd_ready() {}
