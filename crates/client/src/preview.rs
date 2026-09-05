//! Map host-loopback preview URLs to the address used for the remote connection.
/// Preserve scheme, port, path, query and fragment; never replace text in a path.
/// Dev servers must listen on all interfaces to accept this connection.
pub fn rewrite_preview_url(url: &str, host: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme @ ("http" | "https"), rest)) => (scheme, rest),
        Some(_) => return url.to_string(),
        None => ("http", url),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let (name, port) = authority
        .split_once(':')
        .map_or((authority, ""), |(name, port)| (name, port));
    if !["localhost", "127.0.0.1", "0.0.0.0"]
        .iter()
        .any(|loopback| name.eq_ignore_ascii_case(loopback))
    {
        return url.to_string();
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let port = if port.is_empty() {
        String::new()
    } else {
        format!(":{port}")
    };
    format!("{scheme}://{host}{port}{}", &rest[end..])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rewrites_only_loopback_authorities() {
        for local in ["localhost", "127.0.0.1", "0.0.0.0"] {
            assert_eq!(
                rewrite_preview_url(
                    &format!("http://{local}:5173/a?next=localhost#here"),
                    "192.168.1.8"
                ),
                "http://192.168.1.8:5173/a?next=localhost#here"
            );
        }
        assert_eq!(
            rewrite_preview_url("localhost:5173", "fd00::1"),
            "http://[fd00::1]:5173"
        );
        assert_eq!(
            rewrite_preview_url("https://localhost/x", "host.lan"),
            "https://host.lan/x"
        );
        for url in [
            "https://example.com/localhost",
            "http://localhost.evil:80",
            "file:///localhost",
            "http://user@localhost:80",
        ] {
            assert_eq!(rewrite_preview_url(url, "host.lan"), url);
        }
    }
}
