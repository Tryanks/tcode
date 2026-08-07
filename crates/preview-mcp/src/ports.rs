//! Localhost dev-server discovery for the preview chrome's port quick-picks.
//!
//! We probe a fixed list of common dev ports by attempting a short loopback TCP
//! connect (no extra deps, no `lsof`), plus a pure helper to turn a port into a
//! URL.

use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

/// Common local dev-server ports (Vite, CRA, Next, Rails, Django, http.server…).
pub const COMMON_DEV_PORTS: &[u16] = &[
    3000, 3001, 4200, 4321, 5000, 5173, 5174, 8000, 8080, 8081, 8888, 9000,
];

/// Whether something is accepting connections on `127.0.0.1:port` right now.
pub fn is_listening(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(60)).is_ok()
}

/// Probe [`COMMON_DEV_PORTS`] and return those with a listener, in order.
pub fn scan_listening() -> Vec<u16> {
    COMMON_DEV_PORTS
        .iter()
        .copied()
        .filter(|&port| is_listening(port))
        .collect()
}

/// The loopback URL for a dev-server port.
pub fn url_for_port(port: u16) -> String {
    format!("http://localhost:{port}/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_for_port_formats_loopback() {
        assert_eq!(url_for_port(3000), "http://localhost:3000/");
    }
}
