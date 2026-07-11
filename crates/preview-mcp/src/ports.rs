//! Localhost dev-server discovery for the preview chrome's port quick-picks.
//!
//! We probe a fixed list of common dev ports by attempting a short loopback TCP
//! connect (no extra deps, no `lsof`), plus pure helpers to turn a port into a
//! URL and to parse a port out of user input.

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

/// Parse a port from free-form input: a bare number, `host:port`, or a full
/// URL. Returns `None` when there's no plausible port. Used to normalize what
/// the user types into the address bar into a quick-pick candidate.
pub fn parse_port_input(input: &str) -> Option<u16> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Bare port.
    if let Ok(port) = trimmed.parse::<u16>() {
        return Some(port);
    }
    // Strip a scheme, then a path/query, then take the ":port" of the authority.
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let (_host, port) = authority.rsplit_once(':')?;
    port.parse::<u16>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_port() {
        assert_eq!(parse_port_input("3000"), Some(3000));
        assert_eq!(parse_port_input(" 5173 "), Some(5173));
    }

    #[test]
    fn parse_host_port() {
        assert_eq!(parse_port_input("localhost:8080"), Some(8080));
        assert_eq!(parse_port_input("127.0.0.1:4321"), Some(4321));
    }

    #[test]
    fn parse_full_url() {
        assert_eq!(parse_port_input("http://localhost:5173/app?x=1"), Some(5173));
        assert_eq!(parse_port_input("https://127.0.0.1:8443/"), Some(8443));
    }

    #[test]
    fn parse_no_port() {
        assert_eq!(parse_port_input("example.com"), None);
        assert_eq!(parse_port_input(""), None);
        assert_eq!(parse_port_input("http://example.com/path"), None);
    }

    #[test]
    fn parse_out_of_range() {
        assert_eq!(parse_port_input("70000"), None);
    }

    #[test]
    fn url_for_port_formats_loopback() {
        assert_eq!(url_for_port(3000), "http://localhost:3000/");
    }
}
