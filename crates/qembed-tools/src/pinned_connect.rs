//! TLS connection over an already validated, immutable DNS address set.
use std::{error::Error, fmt, net::SocketAddr, sync::Arc, time::Duration};

use tokio::{net::TcpStream, time::timeout};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
    rustls::{ClientConfig, RootCertStore, crypto::ring::default_provider, pki_types::ServerName},
};

use crate::dns_resolution::ResolvedAddressSet;

/// Connects directly to validated IP addresses while retaining the original TLS identity.
pub struct PinnedConnector {
    tls_connector: TlsConnector,
    connect_timeout: Duration,
}

impl PinnedConnector {
    /// Creates a connector using the WebPKI public root set and the ring crypto provider.
    pub fn new() -> Result<Self, PinnedConnectError> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let provider = Arc::new(default_provider());
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| PinnedConnectError::TlsConfiguration(error.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();

        Ok(Self::with_tls_config(Arc::new(config)))
    }

    fn with_tls_config(config: Arc<ClientConfig>) -> Self {
        Self {
            tls_connector: TlsConnector::from(config),
            connect_timeout: Duration::from_secs(5),
        }
    }

    /// Opens TCP sockets only to the checked IPs, then performs TLS using the hostname
    /// bound to the address set for SNI and certificate hostname verification.
    pub async fn connect_tls(
        &self,
        addresses: &ResolvedAddressSet,
        port: u16,
    ) -> Result<PinnedTlsStream, PinnedConnectError> {
        if port == 0 {
            return Err(PinnedConnectError::InvalidPort);
        }

        if addresses.addresses().is_empty() {
            return Err(PinnedConnectError::EmptyAddressSet);
        }

        let server_name = addresses
            .hostname()
            .as_fqdn()
            .trim_end_matches('.')
            .to_owned();
        let tls_server_name = ServerName::try_from(server_name.clone())
            .map_err(|_| PinnedConnectError::InvalidServerName(server_name.clone()))?;

        let mut failures = Vec::new();
        for ip in addresses.addresses() {
            let address = SocketAddr::new(*ip, port);
            let stream = match timeout(self.connect_timeout, TcpStream::connect(address)).await {
                Ok(Ok(stream)) => stream,
                Ok(Err(error)) => {
                    failures.push(format!("{address}: {error}"));
                    continue;
                }
                Err(_) => {
                    failures.push(format!("{address}: connection timed out"));
                    continue;
                }
            };

            // A TLS identity failure is terminal. Do not hide it by trying another IP.
            let stream = self
                .tls_connector
                .connect(tls_server_name.clone(), stream)
                .await
                .map_err(|error| PinnedConnectError::TlsHandshakeFailed {
                    address,
                    message: error.to_string(),
                })?;
            let peer_addr =
                stream.get_ref().0.peer_addr().map_err(|error| {
                    PinnedConnectError::PeerAddressUnavailable(error.to_string())
                })?;

            return Ok(PinnedTlsStream {
                stream,
                peer_addr,
                server_name,
            });
        }

        Err(PinnedConnectError::AllAddressesFailed(failures.join("; ")))
    }
}

/// A completed TLS connection with its actual peer and authenticated server name.
pub struct PinnedTlsStream {
    stream: TlsStream<TcpStream>,
    peer_addr: SocketAddr,
    server_name: String,
}

impl PinnedTlsStream {
    /// Returns the socket peer selected from the validated address set.
    #[must_use]
    pub const fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Returns the DNS name used for SNI and certificate verification.
    #[must_use]
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Consumes the wrapper and returns the TLS stream for the HTTP protocol layer.
    #[must_use]
    pub fn into_inner(self) -> TlsStream<TcpStream> {
        self.stream
    }
}

/// Errors raised while connecting to a validated DNS address set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PinnedConnectError {
    InvalidPort,
    EmptyAddressSet,
    InvalidServerName(String),
    TlsConfiguration(String),
    AllAddressesFailed(String),
    TlsHandshakeFailed {
        address: SocketAddr,
        message: String,
    },
    PeerAddressUnavailable(String),
}

impl fmt::Display for PinnedConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPort => formatter.write_str("destination port must be non-zero"),
            Self::EmptyAddressSet => formatter.write_str("validated DNS address set is empty"),
            Self::InvalidServerName(name) => {
                write!(formatter, "invalid TLS server name: {name}")
            }
            Self::TlsConfiguration(message) => {
                write!(formatter, "TLS client configuration failed: {message}")
            }
            Self::AllAddressesFailed(message) => {
                write!(
                    formatter,
                    "all pinned TCP connection attempts failed: {message}"
                )
            }
            Self::TlsHandshakeFailed { address, message } => {
                write!(
                    formatter,
                    "TLS handshake with pinned peer {address} failed: {message}"
                )
            }
            Self::PeerAddressUnavailable(message) => {
                write!(
                    formatter,
                    "connected peer address is unavailable: {message}"
                )
            }
        }
    }
}

impl Error for PinnedConnectError {}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::{net::TcpListener, time::timeout};
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{
            self, ClientConfig, RootCertStore, ServerConfig,
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
        },
    };

    use super::*;
    use crate::dns_resolution::{
        DnsAnswer, DnsName, DnsResolutionError, DnsResolver, ResolvedAddressSet,
    };

    const TEST_ROOT_CERTIFICATE: &[u8] =
        include_bytes!("../tests/fixtures/dns-test-root-cert.der");
    const TEST_CERTIFICATE: &[u8] = include_bytes!("../tests/fixtures/dns-test-cert.der");
    const TEST_PRIVATE_KEY: &[u8] = include_bytes!("../tests/fixtures/dns-test-key.der");

    struct RebindingResolver {
        calls: AtomicUsize,
    }

    impl DnsResolver for RebindingResolver {
        async fn lookup(&self, _name: &DnsName) -> Result<DnsAnswer, DnsResolutionError> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let address = if call == 0 { "127.0.0.1" } else { "127.0.0.2" };
            Ok(DnsAnswer::new(vec![address.to_owned()], Vec::new()))
        }
    }

    fn test_tls_configs() -> (Arc<ClientConfig>, Arc<ServerConfig>) {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let root_certificate = CertificateDer::from(TEST_ROOT_CERTIFICATE.to_vec());
        let certificate = CertificateDer::from(TEST_CERTIFICATE.to_vec());
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(TEST_PRIVATE_KEY.to_vec()));

        let server_config = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .expect("supported TLS protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![certificate.clone()], private_key)
            .expect("valid test certificate");

        let mut roots = RootCertStore::empty();
        roots
            .add(root_certificate)
            .expect("test CA certificate is trusted");

        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("supported TLS protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();

        (Arc::new(client_config), Arc::new(server_config))
    }

    fn spawn_tls_server(
        listener: TcpListener,
        config: Arc<ServerConfig>,
    ) -> tokio::task::JoinHandle<Option<String>> {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("TCP client accepted");
            match TlsAcceptor::from(config).accept(stream).await {
                Ok(stream) => stream.get_ref().1.server_name().map(str::to_owned),
                Err(_) => None,
            }
        })
    }

    fn local_addresses(hostname: DnsName, ip: Ipv4Addr) -> ResolvedAddressSet {
        ResolvedAddressSet::for_test(hostname, vec![IpAddr::V4(ip)])
    }

    #[tokio::test]
    async fn connects_to_the_pinned_ip_and_sends_original_hostname_as_sni() {
        let (client_config, server_config) = test_tls_configs();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("local TLS listener");
        let address = listener.local_addr().expect("listener address");
        let server = spawn_tls_server(listener, server_config);
        let connector = PinnedConnector::with_tls_config(client_config);
        let hostname = DnsName::parse("example.test").expect("valid DNS name");
        let addresses = local_addresses(hostname, Ipv4Addr::LOCALHOST);

        let connection = connector
            .connect_tls(&addresses, address.port())
            .await
            .expect("TLS connection to pinned IP");

        assert_eq!(connection.peer_addr(), address);
        assert_eq!(connection.server_name(), "example.test");
        drop(connection);
        assert_eq!(
            server.await.expect("TLS server task"),
            Some("example.test".to_owned())
        );
    }

    #[tokio::test]
    async fn rejects_a_certificate_for_a_different_hostname() {
        let (client_config, server_config) = test_tls_configs();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("local TLS listener");
        let address = listener.local_addr().expect("listener address");
        let server = spawn_tls_server(listener, server_config);
        let connector = PinnedConnector::with_tls_config(client_config);
        let hostname = DnsName::parse("wrong.example.test").expect("valid DNS name");
        let addresses = local_addresses(hostname, Ipv4Addr::LOCALHOST);

        let result = connector.connect_tls(&addresses, address.port()).await;

        assert!(matches!(
            result,
            Err(PinnedConnectError::TlsHandshakeFailed { .. })
        ));
        assert_eq!(server.await.expect("TLS server task"), None);
    }

    #[tokio::test]
    async fn keeps_the_original_socket_ip_after_a_later_dns_answer_changes() {
        let (client_config, server_config) = test_tls_configs();
        let listener = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 1), 0))
            .await
            .expect("original local TLS listener");
        let address = listener.local_addr().expect("listener address");
        let rebound_listener = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 2), address.port()))
            .await
            .expect("rebound local listener");
        let server = spawn_tls_server(listener, server_config);
        let connector = PinnedConnector::with_tls_config(client_config);
        let hostname = DnsName::parse("example.test").expect("valid DNS name");
        let resolver = RebindingResolver {
            calls: AtomicUsize::new(0),
        };

        let _initial_answer = resolver
            .lookup(&hostname)
            .await
            .expect("initial DNS answer");
        let pinned = local_addresses(hostname.clone(), Ipv4Addr::new(127, 0, 0, 1));
        let _rebound_answer = resolver
            .lookup(&hostname)
            .await
            .expect("rebound DNS answer");
        let later_dns_answer = local_addresses(hostname, Ipv4Addr::new(127, 0, 0, 2));
        assert_ne!(pinned.addresses(), later_dns_answer.addresses());
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);

        let connection = connector
            .connect_tls(&pinned, address.port())
            .await
            .expect("connection must use the original pinned IP");

        assert_eq!(
            connection.peer_addr().ip(),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            resolver.calls.load(Ordering::SeqCst),
            2,
            "pinned connect must not resolve the hostname again"
        );
        assert!(
            timeout(Duration::from_millis(100), rebound_listener.accept())
                .await
                .is_err(),
            "no connection may be made to the rebound DNS address"
        );

        drop(connection);
        assert_eq!(
            server.await.expect("TLS server task"),
            Some("example.test".to_owned())
        );
    }
}
