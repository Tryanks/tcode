//! Native connection establishment shared by HTTP, WebSocket and CONNECT.
use futures_lite::io::{AsyncRead, AsyncWrite};
use futures_util::StreamExt as _;
use std::{io, net::SocketAddr, sync::Arc, time::Duration};

pub(crate) trait Socket: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Socket for T {}
pub(crate) type Stream = Box<dyn Socket>;

pub(crate) struct Endpoint {
    url: url::Url,
    #[cfg(test)]
    roots: Option<rustls::RootCertStore>,
}

impl Endpoint {
    pub(crate) fn new(origin: &str) -> Result<Self, String> {
        let origin = tcode_client::pairing::parse_origin(origin)?;
        Ok(Self {
            url: url::Url::parse(&origin).map_err(|e| e.to_string())?,
            #[cfg(test)]
            roots: None,
        })
    }

    pub(crate) fn authority(&self) -> String {
        format!(
            "{}:{}",
            self.url.host_str().unwrap(),
            self.url.port_or_known_default().unwrap()
        )
    }

    pub(crate) async fn connect(&self) -> io::Result<Stream> {
        self.establish(|stream| async { Ok(stream) })
            .await
            .map_err(|error| io::Error::other(format!("{error:?}")))
    }

    /// Race complete protocol handshakes, not just TCP connects: a reachable
    /// address that stalls during TLS or hello must not hide a healthy address.
    pub(crate) async fn establish<T, F, Fut>(
        &self,
        handshake: F,
    ) -> Result<T, tcode_client::ConnectionFailure>
    where
        F: Fn(Stream) -> Fut,
        Fut: std::future::Future<Output = Result<T, tcode_client::ConnectionFailure>>,
    {
        use tcode_client::ConnectionFailure;
        let addresses = futures_lite::future::race(
            async {
                smol::net::resolve((
                    self.url.host_str().unwrap().trim_matches(['[', ']']),
                    self.url.port_or_known_default().unwrap(),
                ))
                .await
                .map_err(|_| ConnectionFailure::Unreachable)
            },
            async {
                smol::Timer::after(Duration::from_secs(5)).await;
                Err(ConnectionFailure::Timeout)
            },
        )
        .await?;
        let mut attempts = futures_util::stream::FuturesUnordered::new();
        for (index, address) in addresses.into_iter().enumerate() {
            let handshake = &handshake;
            attempts.push(async move {
                smol::Timer::after(Duration::from_millis(250 * index as u64)).await;
                futures_lite::future::race(
                    async {
                        let stream = self.connect_address(address).await.map_err(|error| {
                            log::debug!("remote connection failed: {error}");
                            ConnectionFailure::Unreachable
                        })?;
                        handshake(stream).await
                    },
                    async {
                        smol::Timer::after(Duration::from_secs(15)).await;
                        Err(ConnectionFailure::Timeout)
                    },
                )
                .await
            });
        }
        let mut failure = ConnectionFailure::Unreachable;
        while let Some(result) = attempts.next().await {
            match result {
                Ok(value) => return Ok(value),
                Err(error) if error.is_terminal() => return Err(error),
                Err(error) => failure = error,
            }
        }
        Err(failure)
    }

    async fn connect_address(&self, address: SocketAddr) -> io::Result<Stream> {
        let tcp = smol::net::TcpStream::connect(address).await?;
        if self.url.scheme() == "http" {
            return Ok(Box::new(tcp));
        }
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        #[cfg(test)]
        let roots = self.roots.clone().unwrap_or(roots);
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .with_root_certificates(roots)
        .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(
            self.url
                .host_str()
                .unwrap()
                .trim_matches(['[', ']'])
                .to_owned(),
        )
        .map_err(io::Error::other)?;
        Ok(Box::new(
            futures_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await?,
        ))
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::*;
    use futures_lite::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn tls_entry_preserves_pair_ws_connect_and_denies_imported_admin() {
        smol::block_on(async {
            let root = std::env::temp_dir().join(format!("tcode-entry-{}", uuid::Uuid::new_v4()));
            let (to_host, _commands) = async_channel::unbounded();
            let (_events, from_host) = async_channel::unbounded();
            let server = Arc::new(
                crate::serve(
                    crate::HostMux::new(to_host, from_host),
                    crate::RemoteConfig {
                        listen: "127.0.0.1:0".parse().unwrap(),
                        host_name: "entry fixture".into(),
                        data_dir: root.clone(),
                        static_bundle: None,
                        browser_password: false,
                    },
                )
                .unwrap(),
            );
            let certificate = rustls::pki_types::CertificateDer::from(
                include_bytes!("../tests/fixtures/localhost.der").to_vec(),
            );
            let key = rustls::pki_types::PrivatePkcs8KeyDer::from(
                include_bytes!("../tests/fixtures/localhost-key.der").to_vec(),
            );
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], key.into())
            .unwrap();
            let acceptor = futures_rustls::TlsAcceptor::from(Arc::new(config));
            let listener = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!(
                "https://localhost:{}",
                listener.local_addr().unwrap().port()
            );
            let host = server.clone();
            let accepting = smol::spawn(async move {
                let mut tasks = Vec::new();
                while let Ok((tcp, _)) = listener.accept().await {
                    let acceptor = acceptor.clone();
                    let host = host.clone();
                    tasks.push(smol::spawn(async move {
                        if let Ok(tls) = acceptor.accept(tcp).await {
                            let _ = host.admit(tls).await;
                        }
                    }));
                }
            });
            // Public roots must reject the private fixture, never silently downgrade.
            assert!(Endpoint::new(&origin).unwrap().connect().await.is_err());
            let mut endpoint = Endpoint::new(&origin).unwrap();
            let mut roots = rustls::RootCertStore::empty();
            roots.add(certificate).unwrap();
            endpoint.roots = Some(roots);
            let direct = Endpoint::new(&format!("http://{}", server.local_addr())).unwrap();
            assert!(
                crate::client::http_async(&direct, "GET", "/admin/pair", "")
                    .await
                    .is_ok()
            );
            let mut imported = endpoint.connect().await.unwrap();
            imported.write_all(b"GET /admin/pair HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 127.0.0.1\r\n\r\n").await.unwrap();
            let mut reply = vec![];
            let _ = imported.read_to_end(&mut reply).await;
            assert!(reply.starts_with(b"HTTP/1.1 403 Forbidden\r\n"));
            let code = server.new_pairing_code().code;
            let body = serde_json::json!({"code":code,"device_name":"tls device"}).to_string();
            let response = crate::client::http_async(&endpoint, "POST", "/pair", &body)
                .await
                .unwrap();
            let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
            let paired = tcode_client::pairing::PairedHost {
                origin: origin.clone(),
                token: response["token"].as_str().unwrap().into(),
                host_id: response["host_id"].as_str().unwrap().into(),
                name: "entry fixture".into(),
                last_connected_unix: None,
            };
            let url = url::Url::parse(&origin).unwrap();
            let ws = endpoint
                .establish(|stream| {
                    crate::client::open_websocket(stream, &url, &paired, "tls device")
                })
                .await
                .unwrap();
            drop(ws);
            let destination = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = destination.local_addr().unwrap();
            let echo = smol::spawn(async move {
                let (mut socket, _) = destination.accept().await.unwrap();
                let mut bytes = vec![];
                socket.read_to_end(&mut bytes).await.unwrap();
                assert_eq!(bytes, b"request after CONNECT");
                socket.write_all(b"reply after EOF").await.unwrap();
                socket.close().await.unwrap();
            });
            let viewing = smol::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut tunnel = smol::net::TcpStream::connect(viewing.local_addr().unwrap())
                .await
                .unwrap();
            let (browser, _) = viewing.accept().await.unwrap();
            let token = paired.token.clone();
            let endpoint = Arc::new(endpoint);
            let forwarding_endpoint = endpoint.clone();
            let forwarding = smol::spawn(async move {
                crate::preview::forward(
                    browser,
                    &forwarding_endpoint,
                    &token,
                    &address.to_string(),
                )
                .await
                .unwrap();
            });
            tunnel.write_all(b"request after CONNECT").await.unwrap();
            tunnel.close().await.unwrap();
            let mut reply = vec![];
            tunnel.read_to_end(&mut reply).await.unwrap();
            assert_eq!(reply, b"reply after EOF");
            echo.await;
            forwarding.await;
            server.revoke_device(&server.devices()[0].id).unwrap();
            assert!(matches!(
                endpoint
                    .establish(|stream| crate::client::open_websocket(
                        stream,
                        &url,
                        &paired,
                        "tls device"
                    ))
                    .await,
                Err(tcode_client::ConnectionFailure::AuthenticationRejected)
            ));
            drop(accepting);
            drop(server);
            std::fs::remove_dir_all(root).unwrap();
        });
    }
}
