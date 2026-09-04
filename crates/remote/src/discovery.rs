#[cfg(feature = "client")]
use std::collections::HashMap;
#[cfg(feature = "client")]
use std::io;
#[cfg(feature = "client")]
use std::net::SocketAddr;
#[cfg(any(feature = "server", feature = "client"))]
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
#[cfg(feature = "server")]
use std::sync::Arc;
#[cfg(feature = "server")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "server")]
use std::thread::JoinHandle;
#[cfg(any(feature = "server", feature = "client"))]
use std::time::Duration;
#[cfg(feature = "client")]
use std::time::Instant;

#[cfg(any(feature = "server", feature = "client"))]
use serde::{Deserialize, Serialize};
#[cfg(feature = "client")]
use socket2::{Domain, Protocol, Socket, Type};

#[cfg(any(feature = "server", feature = "client"))]
const BEACON_PORT: u16 = 47_421;

#[cfg(feature = "client")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    pub host_id: String,
    pub name: String,
    pub port: u16,
    pub addr: String,
}

#[cfg(any(feature = "server", feature = "client"))]
#[derive(Serialize, Deserialize)]
struct BeaconWire {
    tcode: u8,
    host_id: String,
    name: String,
    port: u16,
}

#[cfg(feature = "server")]
pub struct BeaconHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(feature = "server")]
impl BeaconHandle {
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(feature = "server")]
impl Drop for BeaconHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(feature = "server")]
pub fn start_beacon(
    host_id: impl Into<String>,
    name: impl Into<String>,
    port: u16,
) -> BeaconHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let wire = BeaconWire {
        tcode: 1,
        host_id: host_id.into(),
        name: name.into(),
        port,
    };
    let thread = std::thread::Builder::new()
        .name("tcode-beacon".into())
        .spawn(move || {
            let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) else {
                log::warn!("unable to bind remote discovery beacon socket");
                return;
            };
            if let Err(error) = socket.set_broadcast(true) {
                log::warn!("unable to enable remote discovery broadcast: {error}");
                return;
            }
            let Ok(payload) = serde_json::to_vec(&wire) else {
                return;
            };
            let destination = SocketAddrV4::new(Ipv4Addr::BROADCAST, BEACON_PORT);
            while !thread_stop.load(Ordering::Relaxed) {
                if let Err(error) = socket.send_to(&payload, destination) {
                    log::warn!("remote discovery beacon send failed: {error}");
                }
                for _ in 0..20 {
                    if thread_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .ok();
    BeaconHandle { stop, thread }
}

#[cfg(feature = "client")]
pub fn browse(timeout: Duration) -> Vec<Beacon> {
    match browse_inner(timeout) {
        Ok(beacons) => beacons,
        Err(error) => {
            log::warn!("remote discovery browse unavailable: {error}");
            Vec::new()
        }
    }
}

#[cfg(feature = "client")]
fn browse_inner(timeout: Duration) -> io::Result<Vec<Beacon>> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, BEACON_PORT).into())?;
    let socket: UdpSocket = socket.into();
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    let deadline = Instant::now() + timeout;
    let mut found = HashMap::new();
    let mut buffer = [0_u8; 2048];
    while Instant::now() < deadline {
        match socket.recv_from(&mut buffer) {
            Ok((length, peer)) => {
                let Ok(wire) = serde_json::from_slice::<BeaconWire>(&buffer[..length]) else {
                    continue;
                };
                if wire.tcode != 1 {
                    continue;
                }
                let addr = match peer {
                    SocketAddr::V4(addr) => addr.ip().to_string(),
                    SocketAddr::V6(addr) => addr.ip().to_string(),
                };
                found.insert(
                    (wire.host_id.clone(), addr.clone()),
                    Beacon {
                        host_id: wire.host_id,
                        name: wire.name,
                        port: wire.port,
                        addr,
                    },
                );
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(found.into_values().collect())
}
