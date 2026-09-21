//! What crosses a QUIC connection before and around the NDJSON protocol.
//!
//! Two ALPNs. `tcode/pair/1` carries one bi stream with a `pair` line and its
//! `paired`/`pair_rejected` answer. `tcode/1` is accepted only from a paired
//! `EndpointId`; its first bi stream opens with `hello` and, after `hello_ok`,
//! carries `ClientMessage`/`HostMessage` lines. Every control line is JSON
//! terminated by `\n`, at most [`MAX_CONTROL_LINE`] bytes, and must arrive
//! within [`CONTROL_TIMEOUT`].
//!
//! After hello, a further bi stream on `tcode/1` opening with
//! `{"type":"connect","host":..,"port":..}` carries one Preview TCP tunnel:
//! the machine dials `host:port` the way any local program would (loopback
//! included), answers `connected` or `connect_failed` before any tunnel
//! byte, and from then on both directions are opaque bytes. Finishing the
//! device's send side shuts down the machine's write half to the service;
//! the service closing is a finished stream back; a reset closes both. A
//! `connect` before hello is refused, and one connection holds at most
//! [`MAX_TUNNELS`] tunnels at once.
use std::{io, time::Duration};

use iroh::endpoint::{QuicTransportConfig, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, BufReader};

pub const ALPN_PAIR: &[u8] = b"tcode/pair/1";
pub const ALPN_MAIN: &[u8] = b"tcode/1";

/// Bound on any control line and on the first line of any stream.
pub const MAX_CONTROL_LINE: usize = 4096;
/// How long a peer has to send a stream's first line.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on one NDJSON line after hello, in either direction.
pub const MAX_LINE: usize = 16 * 1024 * 1024;
/// Bi streams one connection may hold open at once.
pub const MAX_STREAMS: u32 = 64;
/// Preview tunnels one connection may hold open at once, leaving streams for
/// the main stream and for answering refusals.
pub const MAX_TUNNELS: usize = MAX_STREAMS as usize - 8;
/// How long the machine dials a tunnel target before answering
/// `connect_failed`.
pub const TUNNEL_CONNECT: Duration = Duration::from_secs(10);
/// A tunnel with no bytes in either direction for this long is closed.
pub const TUNNEL_IDLE: Duration = Duration::from_secs(60);

pub const KEEP_ALIVE: Duration = Duration::from_secs(10);
pub const MAX_IDLE: Duration = Duration::from_secs(30);

/// Both endpoints share one QUIC profile: keep-alives every 10 s, a 30 s idle
/// limit, and no more streams than the host is willing to serve.
pub fn transport_config() -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .keep_alive_interval(KEEP_ALIVE)
        .max_idle_timeout(Some(
            MAX_IDLE
                .try_into()
                .expect("30 s fits the QUIC idle timeout"),
        ))
        .max_concurrent_bidi_streams(MAX_STREAMS.into())
        .max_concurrent_uni_streams(0_u32.into())
        .build()
}

/// How a device introduces itself when pairing and connecting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceClaim {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

impl DeviceClaim {
    pub fn is_valid(&self) -> bool {
        let ok = |value: &str| value.len() <= 256 && !value.chars().any(char::is_control);
        !self.name.trim().is_empty() && ok(&self.name) && self.platform.as_deref().is_none_or(ok)
    }

    /// The claim as the host records it: trimmed, empty platform dropped.
    pub fn normalized(&self) -> (String, Option<String>) {
        (
            self.name.trim().to_owned(),
            self.platform
                .as_deref()
                .map(str::trim)
                .filter(|platform| !platform.is_empty())
                .map(str::to_owned),
        )
    }
}

/// First line a device sends on a stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientLine {
    /// Present an invitation's secret to join the allow list.
    Pair {
        secret: String,
        device: DeviceClaim,
    },
    Hello {
        protocol_version: u32,
        device: DeviceClaim,
    },
    /// Open a Preview tunnel; see the module documentation.
    Connect {
        host: String,
        port: u16,
    },
}

/// Whether a `connect` line names something the machine will dial: a host
/// name or IP literal without brackets, and a real port.
pub fn valid_tunnel_target(host: &str, port: u16) -> bool {
    port != 0
        && !host.is_empty()
        && host.len() <= 253
        && !host
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '[' | ']' | '/'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairRejection {
    /// Wrong, expired or already used invitation.
    Invalid,
    /// The machine is not accepting new devices.
    Disabled,
    /// The machine could not record the pairing; try again.
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelloRejection {
    Protocol,
    Unpaired,
}

/// The machine's answer to a [`ClientLine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostLine {
    Paired {
        host_name: String,
    },
    PairRejected {
        reason: PairRejection,
    },
    HelloOk {
        host_name: String,
        protocol_version: u32,
    },
    HelloRejected {
        reason: HelloRejection,
    },
    /// The tunnel target accepted the connection; tunnel bytes follow.
    Connected,
    /// The tunnel target could not be reached; the stream ends here.
    ConnectFailed {
        reason: String,
    },
    /// A stream whose first line the machine does not serve.
    Refused {
        reason: String,
    },
}

pub type LineReader = BufReader<RecvStream>;

pub fn reader(recv: RecvStream) -> LineReader {
    BufReader::new(recv)
}

/// Read one `\n`-terminated line of at most `max` bytes. `None` at a clean
/// end of stream; a partial trailing line is an error.
pub async fn read_line(reader: &mut LineReader, max: usize) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    loop {
        let buffered = reader.fill_buf().await?;
        if buffered.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended inside a line",
                ))
            };
        }
        let (chunk, done) = match buffered.iter().position(|byte| *byte == b'\n') {
            Some(newline) => (newline + 1, true),
            None => (buffered.len(), false),
        };
        if line.len() + chunk > max + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("line exceeds {max} bytes"),
            ));
        }
        line.extend_from_slice(&buffered[..chunk]);
        reader.consume(chunk);
        if done {
            break;
        }
    }
    let mut line = String::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "line is not UTF-8"))?;
    line.truncate(line.trim_end_matches(['\n', '\r']).len());
    Ok(Some(line))
}

/// Read a control line within [`CONTROL_TIMEOUT`] and decode it.
pub async fn read_control<T: serde::de::DeserializeOwned>(
    reader: &mut LineReader,
) -> io::Result<T> {
    let line = tokio::time::timeout(CONTROL_TIMEOUT, read_line(reader, MAX_CONTROL_LINE))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "control line timed out"))??
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no control line"))?;
    serde_json::from_str(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub async fn write_line<T: Serialize>(send: &mut SendStream, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    send.write_all(&line).await?;
    Ok(())
}

pub async fn write_raw_line(send: &mut SendStream, line: &str) -> io::Result<()> {
    let line = line.trim_end_matches(['\n', '\r']);
    send.write_all(line.as_bytes()).await?;
    send.write_all(b"\n").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_lines_use_the_documented_shapes() {
        let pair = ClientLine::Pair {
            secret: "AAECAwQFBgcICQoLDA0ODw".into(),
            device: DeviceClaim {
                name: "Phone".into(),
                platform: Some("Android 15".into()),
            },
        };
        assert_eq!(
            serde_json::to_value(&pair).unwrap(),
            serde_json::json!({"type":"pair","secret":"AAECAwQFBgcICQoLDA0ODw","device":{"name":"Phone","platform":"Android 15"}})
        );
        assert_eq!(
            serde_json::to_value(HostLine::PairRejected {
                reason: PairRejection::Invalid
            })
            .unwrap(),
            serde_json::json!({"type":"pair_rejected","reason":"invalid"})
        );
        assert_eq!(
            serde_json::from_str::<ClientLine>(
                r#"{"type":"hello","protocol_version":5,"device":{"name":"Phone"}}"#
            )
            .unwrap(),
            ClientLine::Hello {
                protocol_version: 5,
                device: DeviceClaim {
                    name: "Phone".into(),
                    platform: None
                }
            }
        );
        assert_eq!(
            serde_json::to_value(HostLine::PairRejected {
                reason: PairRejection::Disabled
            })
            .unwrap(),
            serde_json::json!({"type":"pair_rejected","reason":"disabled"})
        );
        assert_eq!(
            serde_json::to_value(HostLine::HelloRejected {
                reason: HelloRejection::Unpaired
            })
            .unwrap(),
            serde_json::json!({"type":"hello_rejected","reason":"unpaired"})
        );
        assert_eq!(
            serde_json::to_value(HostLine::HelloOk {
                host_name: "Desk".into(),
                protocol_version: 5
            })
            .unwrap(),
            serde_json::json!({"type":"hello_ok","host_name":"Desk","protocol_version":5})
        );
        assert_eq!(
            serde_json::from_str::<ClientLine>(
                r#"{"type":"connect","host":"localhost","port":5173}"#
            )
            .unwrap(),
            ClientLine::Connect {
                host: "localhost".into(),
                port: 5173
            }
        );
        assert_eq!(
            serde_json::to_value(HostLine::Connected).unwrap(),
            serde_json::json!({"type":"connected"})
        );
        assert_eq!(
            serde_json::to_value(HostLine::ConnectFailed {
                reason: "refused".into()
            })
            .unwrap(),
            serde_json::json!({"type":"connect_failed","reason":"refused"})
        );
        assert!(valid_tunnel_target("::1", 80));
        assert!(!valid_tunnel_target("[::1]", 80));
        assert!(!valid_tunnel_target("localhost", 0));
        assert!(!valid_tunnel_target("", 80));
        assert!(
            !DeviceClaim {
                name: " ".into(),
                platform: None
            }
            .is_valid()
        );
        assert!(
            !DeviceClaim {
                name: "x".repeat(257),
                platform: None
            }
            .is_valid()
        );
    }
}
