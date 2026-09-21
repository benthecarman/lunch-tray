//! Small systemd integration: readiness and status over `NOTIFY_SOCKET`,
//! and detection of the journal so logs are not double-stamped.

use std::os::unix::net::UnixDatagram;

/// Send one `sd_notify` message. Does nothing when not started by systemd.
pub fn notify(state: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    let path = path.to_string_lossy();
    let result = if let Some(name) = path.strip_prefix('@') {
        use std::os::linux::net::SocketAddrExt;
        match std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()) {
            Ok(addr) => sock.send_to_addr(state.as_bytes(), &addr).map(|_| ()),
            Err(e) => Err(e),
        }
    } else {
        sock.send_to(state.as_bytes(), path.as_ref()).map(|_| ())
    };
    if let Err(e) = result {
        log::debug!("sd_notify {state:?}: {e}");
    }
}

/// True when stderr goes to the journal, which adds its own timestamps.
pub fn under_journal() -> bool {
    std::env::var_os("JOURNAL_STREAM").is_some()
}
