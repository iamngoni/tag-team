//! A loopback-only CONNECT proxy for sandbox egress.
//!
//! This crate owns per-workspace destination policy and proxy lifecycle. DNS
//! resolution and connection creation are injected; the proxy validates every
//! answer and passes only those concrete IPs to the connector.

use std::{
    collections::BTreeSet,
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, timeout_at},
};

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use tokio::net::UnixListener;

/// Default package registry hosts supported by the proxy.
pub const DEFAULT_EGRESS: &[&str] = &[
    "crates.io",
    "static.crates.io",
    "index.crates.io",
    "registry.npmjs.org",
    "pypi.org",
    "files.pythonhosted.org",
    "proxy.golang.org",
];

const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const OUTCOME_CAPACITY: usize = 128;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// An owned asynchronous byte stream returned by a [`Connector`].
pub trait ProxyIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> ProxyIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// Resolves a hostname into IP candidates before any connection is opened.
pub trait Resolver: Send + Sync + 'static {
    /// Resolves `host` without changing its identity.
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>>;
}

/// Opens a connection to one already validated IP address.
pub trait Connector: Send + Sync + 'static {
    /// Connects to this concrete address; implementations must not resolve a hostname.
    fn connect<'a>(&'a self, address: SocketAddr) -> BoxFuture<'a, io::Result<Box<dyn ProxyIo>>>;
}

/// DNS resolver backed by Tokio's system hostname lookup.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioResolver;

impl Resolver for TokioResolver {
    fn resolve<'a>(&'a self, host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host, 0)).await?;
            Ok(addresses.map(|address| address.ip()).collect())
        })
    }
}

/// Direct TCP connector that receives only the concrete addresses validated by this proxy.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpConnector;

impl Connector for TcpConnector {
    fn connect<'a>(&'a self, address: SocketAddr) -> BoxFuture<'a, io::Result<Box<dyn ProxyIo>>> {
        Box::pin(async move {
            let stream = TcpStream::connect(address).await?;
            Ok(Box::new(stream) as Box<dyn ProxyIo>)
        })
    }
}

/// A canonical lowercase DNS hostname admitted by the policy.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AllowedHost(String);

impl AllowedHost {
    /// Creates an exact allow-list entry, lowercasing it and removing one final dot.
    pub fn new(host: impl AsRef<str>) -> Result<Self, ConfigError> {
        normalize_hostname(host.as_ref())
            .map(Self)
            .ok_or(ConfigError::InvalidAllowedHost)
    }

    /// Returns the canonical hostname.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AllowedHost {
    type Error = ConfigError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<AllowedHost> for String {
    fn from(value: AllowedHost) -> Self {
        value.0
    }
}

/// Validated runtime policy for one workspace proxy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Exact DNS hostnames that may be reached. An empty list denies all hosts.
    pub allowed_hosts: Vec<AllowedHost>,
    /// Destination ports allowed by this workspace.
    pub allowed_ports: BTreeSet<u16>,
    /// Maximum size of a complete inbound request header block (at most 8 KiB).
    pub max_header_bytes: usize,
}

impl ProxyConfig {
    /// Creates a config allowing the default registry hostnames and ports 80/443.
    pub fn defaults() -> Self {
        Self {
            allowed_hosts: DEFAULT_EGRESS
                .iter()
                .map(|host| AllowedHost((*host).to_owned()))
                .collect(),
            allowed_ports: [80, 443].into_iter().collect(),
            max_header_bytes: MAX_HEADER_BYTES,
        }
    }

    /// Creates a config with explicit hosts and default ports 80/443.
    pub fn new(allowed_hosts: Vec<AllowedHost>) -> Self {
        Self {
            allowed_hosts,
            ..Self::defaults()
        }
    }

    /// Creates a config with explicit hosts and ports.
    pub fn with_ports(allowed_hosts: Vec<AllowedHost>, allowed_ports: BTreeSet<u16>) -> Self {
        Self {
            allowed_hosts,
            allowed_ports,
            ..Self::defaults()
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.max_header_bytes == 0 || self.max_header_bytes > MAX_HEADER_BYTES {
            return Err(ConfigError::InvalidHeaderLimit);
        }
        if self.allowed_ports.contains(&0) {
            return Err(ConfigError::InvalidPort);
        }
        Ok(())
    }

    fn permits(&self, hostname: &str) -> bool {
        self.allowed_hosts.iter().any(|host| host.0 == hostname)
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Proxy configuration error.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    /// An allow-list entry was not a valid DNS hostname.
    #[error("allow-list entries must be valid DNS hostnames")]
    InvalidAllowedHost,
    /// Port zero cannot be a destination port.
    #[error("destination ports must be in 1..=65535")]
    InvalidPort,
    /// Header limits must be positive and no greater than 8 KiB.
    #[error("header limit must be between 1 byte and 8 KiB")]
    InvalidHeaderLimit,
}

/// Proxy listener startup error.
#[derive(Debug, Error)]
pub enum StartError {
    /// Configuration failed validation.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The loopback listener could not be bound.
    #[error("could not bind egress proxy listener: {0}")]
    Bind(#[source] io::Error),
    /// A Unix socket path was not absolute or traversed a symbolic link.
    #[cfg(unix)]
    #[error("Unix socket path must be absolute and contain no symbolic links")]
    InvalidUnixPath,
    /// A filesystem entry already exists at the requested Unix socket path.
    #[cfg(unix)]
    #[error("Unix socket path already exists: {0}")]
    UnixPathExists(PathBuf),
}

/// Policy category for a request rejected with HTTP 403.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenyReason {
    /// The hostname is not in this workspace's exact allow-list.
    HostNotAllowed,
    /// The target is an IP literal rather than an allowed hostname.
    IpLiteral,
    /// The destination port is not in the workspace's port set.
    PortNotAllowed,
    /// At least one resolved address is not globally routable.
    UnsafeResolvedAddress,
}

/// Category for a syntactically malformed request rejected with HTTP 400.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MalformedReason {
    InvalidRequest,
    InvalidAuthority,
    InvalidPort,
    InvalidHeader,
    HeaderTooLarge,
    HandshakeTimeout,
}

/// Structured result category for an accepted client connection.
///
/// Events intentionally contain no raw request data or destination strings.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "reason", rename_all = "snake_case")]
pub enum ConnectionOutcome {
    Allowed,
    Denied(DenyReason),
    Malformed(MalformedReason),
    MethodNotAllowed,
    UpstreamFailure,
    ClientDisconnected,
}

/// Running loopback proxy and its cancellation handle.
pub struct ProxyServer {
    addr: SocketAddr,
    runtime: ProxyRuntime,
}

struct ProxyRuntime {
    cancel: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Running proxy bound to one Unix-domain socket.
///
/// The supplied path must be absolute, unused, and free of symbolic-link components.
/// The socket is restricted to its owner and removed on shutdown or drop when it is
/// still the same socket created by this server.
#[cfg(unix)]
pub struct UnixProxyServer {
    path: PathBuf,
    identity: (u64, u64),
    runtime: ProxyRuntime,
}

impl ProxyServer {
    /// Starts on an available IPv4 loopback port.
    pub async fn start(
        config: ProxyConfig,
        resolver: Arc<dyn Resolver>,
        connector: Arc<dyn Connector>,
    ) -> Result<(Self, mpsc::Receiver<ConnectionOutcome>), StartError> {
        config.validate()?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(StartError::Bind)?;
        let addr = listener.local_addr().map_err(StartError::Bind)?;
        let (runtime, outcomes_rx) =
            start_runtime(Listener::Tcp(listener), config, resolver, connector);
        Ok((Self { addr, runtime }, outcomes_rx))
    }

    /// Starts on a fresh absolute Unix-domain socket path with owner-only permissions.
    ///
    /// Existing filesystem entries are never removed. Callers should provide a unique,
    /// fresh path and remove their own stale sockets only after verifying ownership.
    #[cfg(unix)]
    pub async fn start_unix(
        path: impl AsRef<Path>,
        config: ProxyConfig,
        resolver: Arc<dyn Resolver>,
        connector: Arc<dyn Connector>,
    ) -> Result<(UnixProxyServer, mpsc::Receiver<ConnectionOutcome>), StartError> {
        config.validate()?;
        let path = path.as_ref();
        validate_unix_path(path)?;
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Err(StartError::UnixPathExists(path.to_owned())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(StartError::Bind(error)),
        }
        let listener = UnixListener::bind(path).map_err(StartError::Bind)?;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            drop(listener);
            return Err(StartError::Bind(error));
        }
        let metadata = std::fs::symlink_metadata(path).map_err(StartError::Bind)?;
        if !metadata.file_type().is_socket() {
            drop(listener);
            return Err(StartError::InvalidUnixPath);
        }
        let identity = (metadata.dev(), metadata.ino());
        let (runtime, outcomes) =
            start_runtime(Listener::Unix(listener), config, resolver, connector);
        Ok((
            UnixProxyServer {
                path: path.to_owned(),
                identity,
                runtime,
            },
            outcomes,
        ))
    }

    /// Returns the loopback address including its selected port.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Signals shutdown and waits until the listener and its client tasks stop.
    pub async fn shutdown(mut self) {
        self.runtime.shutdown().await;
    }

    /// Signals shutdown without waiting.
    pub fn cancel(&self) {
        self.runtime.cancel();
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.runtime.cancel();
    }
}

impl ProxyRuntime {
    async fn shutdown(&mut self) {
        self.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }

    fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
}

impl Drop for ProxyRuntime {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(unix)]
impl UnixProxyServer {
    /// Returns the socket path supplied to [`ProxyServer::start_unix`].
    pub fn socket_path(&self) -> &Path {
        &self.path
    }

    /// Signals shutdown and waits until the listener and its client tasks stop.
    pub async fn shutdown(mut self) {
        self.runtime.shutdown().await;
        self.cleanup_socket();
    }

    /// Signals shutdown without waiting and removes the owned socket path best-effort.
    pub fn cancel(&self) {
        self.runtime.cancel();
        self.cleanup_socket();
    }

    fn cleanup_socket(&self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path)
            && metadata.file_type().is_socket()
            && (metadata.dev(), metadata.ino()) == self.identity
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
impl Drop for UnixProxyServer {
    fn drop(&mut self) {
        self.runtime.cancel();
        self.cleanup_socket();
    }
}

#[cfg(unix)]
fn validate_unix_path(path: &Path) -> Result<(), StartError> {
    if !path.is_absolute() {
        return Err(StartError::InvalidUnixPath);
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        ) {
            return Err(StartError::InvalidUnixPath);
        }
        current.push(component);
        if let Ok(metadata) = std::fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            return Err(StartError::InvalidUnixPath);
        }
    }
    Ok(())
}

fn start_runtime(
    listener: Listener,
    config: ProxyConfig,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
) -> (ProxyRuntime, mpsc::Receiver<ConnectionOutcome>) {
    let (cancel, cancel_rx) = watch::channel(false);
    let (outcomes_tx, outcomes_rx) = mpsc::channel(OUTCOME_CAPACITY);
    let task = tokio::spawn(serve(
        listener,
        config,
        resolver,
        connector,
        cancel_rx,
        outcomes_tx,
    ));
    (
        ProxyRuntime {
            cancel,
            task: Some(task),
        },
        outcomes_rx,
    )
}

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
}

impl Listener {
    async fn accept(&self) -> io::Result<Box<dyn ProxyIo>> {
        match self {
            Self::Tcp(listener) => listener
                .accept()
                .await
                .map(|(stream, _)| Box::new(stream) as Box<dyn ProxyIo>),
            #[cfg(unix)]
            Self::Unix(listener) => listener
                .accept()
                .await
                .map(|(stream, _)| Box::new(stream) as Box<dyn ProxyIo>),
        }
    }
}

async fn serve(
    listener: Listener,
    config: ProxyConfig,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
    mut cancel: watch::Receiver<bool>,
    outcomes: mpsc::Sender<ConnectionOutcome>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    connections.abort_all();
                    while connections.join_next().await.is_some() {}
                    return;
                }
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok(stream) => {
                        let config = config.clone();
                        let resolver = Arc::clone(&resolver);
                        let connector = Arc::clone(&connector);
                        let outcomes = outcomes.clone();
                        connections.spawn(async move {
                            let outcome = handle_client(stream, config, resolver, connector).await;
                            let _ = outcomes.send(outcome).await;
                        });
                    }
                    Err(_) => break,
                }
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

async fn handle_client(
    mut client: Box<dyn ProxyIo>,
    config: ProxyConfig,
    resolver: Arc<dyn Resolver>,
    connector: Arc<dyn Connector>,
) -> ConnectionOutcome {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let request =
        match timeout_at(deadline, read_request(&mut client, config.max_header_bytes)).await {
            Err(_) => {
                let _ = respond(&mut client, 400, "Bad Request").await;
                return ConnectionOutcome::Malformed(MalformedReason::HandshakeTimeout);
            }
            Ok(Ok(Some(request))) => request,
            Ok(Ok(None)) => return ConnectionOutcome::ClientDisconnected,
            Ok(Err(reason)) => {
                let _ = respond(&mut client, 400, "Bad Request").await;
                return ConnectionOutcome::Malformed(reason);
            }
        };

    if request.method != "CONNECT" {
        let _ = respond(&mut client, 405, "Method Not Allowed").await;
        return ConnectionOutcome::MethodNotAllowed;
    }

    let (hostname, port) = match parse_authority(&request.target) {
        Ok(Authority::Hostname(host, port)) => (host, port),
        Ok(Authority::IpLiteral) => {
            let _ = respond(&mut client, 403, "Forbidden").await;
            return ConnectionOutcome::Denied(DenyReason::IpLiteral);
        }
        Err(reason) => {
            let _ = respond(&mut client, 400, "Bad Request").await;
            return ConnectionOutcome::Malformed(reason);
        }
    };

    // Check exact hostname policy before any DNS lookup.
    if !config.permits(&hostname) {
        let _ = respond(&mut client, 403, "Forbidden").await;
        return ConnectionOutcome::Denied(DenyReason::HostNotAllowed);
    }
    if !config.allowed_ports.contains(&port) {
        let _ = respond(&mut client, 403, "Forbidden").await;
        return ConnectionOutcome::Denied(DenyReason::PortNotAllowed);
    }

    let addresses = match timeout_at(deadline, resolver.resolve(&hostname)).await {
        Ok(Ok(addresses)) if !addresses.is_empty() => addresses,
        _ => {
            let _ = respond(&mut client, 502, "Bad Gateway").await;
            return ConnectionOutcome::UpstreamFailure;
        }
    };
    // Validate the complete DNS answer before connecting to any member. This
    // prevents a public answer from masking a private/rebinding candidate.
    if addresses.iter().any(|address| !is_public_unicast(*address)) {
        let _ = respond(&mut client, 403, "Forbidden").await;
        return ConnectionOutcome::Denied(DenyReason::UnsafeResolvedAddress);
    }

    let mut upstream = None;
    for address in addresses {
        let socket = SocketAddr::new(address, port);
        match timeout_at(deadline, connector.connect(socket)).await {
            Ok(Ok(stream)) => {
                upstream = Some(stream);
                break;
            }
            _ => continue,
        }
    }
    let Some(mut upstream) = upstream else {
        let _ = respond(&mut client, 502, "Bad Gateway").await;
        return ConnectionOutcome::UpstreamFailure;
    };

    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
    {
        return ConnectionOutcome::ClientDisconnected;
    }
    if !request.rest.is_empty() && upstream.write_all(&request.rest).await.is_err() {
        return ConnectionOutcome::ClientDisconnected;
    }
    let _ = io::copy_bidirectional(&mut client, upstream.as_mut()).await;
    ConnectionOutcome::Allowed
}

struct Request {
    method: String,
    target: String,
    rest: Vec<u8>,
}

async fn read_request(
    client: &mut (impl AsyncRead + Unpin),
    limit: usize,
) -> Result<Option<Request>, MalformedReason> {
    let mut bytes = Vec::with_capacity(1024);
    let header_end = loop {
        if let Some(end) = find_header_end(&bytes) {
            break end;
        }
        if bytes.len() >= limit {
            return Err(MalformedReason::HeaderTooLarge);
        }
        let mut chunk = [0u8; 512];
        let read = client
            .read(&mut chunk)
            .await
            .map_err(|_| MalformedReason::InvalidRequest)?;
        if read == 0 {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(MalformedReason::InvalidRequest)
            };
        }
        bytes.extend_from_slice(&chunk[..read]);
        let checked_len = find_header_end(&bytes).unwrap_or(bytes.len());
        if bytes[..checked_len]
            .iter()
            .enumerate()
            .any(|(index, byte)| {
                (*byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r'))
                    || (*byte == b'\r' && index + 1 < checked_len && bytes[index + 1] != b'\n')
            })
        {
            return Err(MalformedReason::InvalidRequest);
        }
    };
    if header_end > limit {
        return Err(MalformedReason::HeaderTooLarge);
    }

    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut parsed = httparse::Request::new(&mut headers);
    match parsed
        .parse(&bytes[..header_end])
        .map_err(|_| MalformedReason::InvalidRequest)?
    {
        httparse::Status::Complete(_) => {}
        httparse::Status::Partial => return Err(MalformedReason::InvalidRequest),
    }
    let method = parsed
        .method
        .ok_or(MalformedReason::InvalidRequest)?
        .to_owned();
    let target = parsed
        .path
        .ok_or(MalformedReason::InvalidRequest)?
        .to_owned();
    if parsed.headers.iter().any(|header| {
        header
            .value
            .iter()
            .any(|byte| (*byte < 0x20 && *byte != b'\t') || *byte == 0x7f)
    }) {
        return Err(MalformedReason::InvalidHeader);
    }
    Ok(Some(Request {
        method,
        target,
        rest: bytes[header_end..].to_vec(),
    }))
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| at + 4)
}

enum Authority {
    Hostname(String, u16),
    IpLiteral,
}

fn parse_authority(authority: &str) -> Result<Authority, MalformedReason> {
    if authority.is_empty()
        || authority
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
        || authority.contains('@')
    {
        return Err(MalformedReason::InvalidAuthority);
    }
    let (raw_host, raw_port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or(MalformedReason::InvalidAuthority)?;
        let host = &bracketed[..close];
        if host.parse::<IpAddr>().is_err() {
            return Err(MalformedReason::InvalidAuthority);
        }
        let suffix = &bracketed[close + 1..];
        let port = suffix
            .strip_prefix(':')
            .ok_or(MalformedReason::InvalidAuthority)?;
        if port.is_empty() || port.contains(':') {
            return Err(MalformedReason::InvalidPort);
        }
        (host, port)
    } else {
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or(MalformedReason::InvalidAuthority)?;
        if host.is_empty() || host.contains(':') || port.is_empty() {
            return Err(MalformedReason::InvalidAuthority);
        }
        (host, port)
    };
    if !raw_port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(MalformedReason::InvalidPort);
    }
    let port = raw_port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(MalformedReason::InvalidPort)?;

    if raw_host.parse::<IpAddr>().is_ok() {
        return Ok(Authority::IpLiteral);
    }
    let hostname = normalize_hostname(raw_host).ok_or(MalformedReason::InvalidAuthority)?;
    Ok(Authority::Hostname(hostname, port))
}

fn normalize_hostname(hostname: &str) -> Option<String> {
    let hostname = hostname.strip_suffix('.').unwrap_or(hostname);
    if hostname.is_empty()
        || hostname.len() > 253
        || hostname.parse::<IpAddr>().is_ok()
        || !hostname.is_ascii()
    {
        return None;
    }
    if !hostname.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label.as_bytes()[0].is_ascii_alphanumeric()
            && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return None;
    }
    Some(hostname.to_ascii_lowercase())
}

fn is_public_unicast(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => is_public_ipv4(ip),
        IpAddr::V6(ip) => is_public_ipv6(ip),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || a == 0
        || a >= 224
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19 || b == 51))
        || (a == 203 && b == 0 && c == 113))
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped) = ip.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }
    let segments = ip.segments();
    let globally_routable_unicast = (segments[0] & 0xe000) == 0x2000;
    globally_routable_unicast
        && !ip.is_loopback()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && !ip.is_unicast_link_local()
        && (segments[0] & 0xfe00) != 0xfc00
        && !(segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8))
        && segments[0] != 0x2002
        && !(segments[0] == 0x3fff && segments[1] <= 0x0fff)
}

async fn respond(
    client: &mut (impl AsyncWrite + Unpin),
    status: u16,
    reason: &str,
) -> io::Result<()> {
    let body = format!("{reason}\n");
    client
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
}

#[cfg(test)]
mod tests {
    //! Exercises policy and transport with only fake DNS and local TCP streams.
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    #[cfg(unix)]
    use tokio::net::UnixStream;
    use tokio::time::timeout;

    struct FixedResolver {
        addresses: Vec<IpAddr>,
        calls: AtomicUsize,
    }

    impl FixedResolver {
        fn new(addresses: Vec<IpAddr>) -> Self {
            Self {
                addresses,
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl Resolver for FixedResolver {
        fn resolve<'a>(&'a self, _host: &'a str) -> BoxFuture<'a, io::Result<Vec<IpAddr>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(self.addresses.clone()) })
        }
    }

    struct LocalConnector {
        addr: SocketAddr,
        calls: AtomicUsize,
        connected_to: tokio::sync::Mutex<Vec<SocketAddr>>,
    }

    impl LocalConnector {
        fn new(addr: SocketAddr) -> Self {
            Self {
                addr,
                calls: AtomicUsize::new(0),
                connected_to: tokio::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl Connector for LocalConnector {
        fn connect<'a>(
            &'a self,
            address: SocketAddr,
        ) -> BoxFuture<'a, io::Result<Box<dyn ProxyIo>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                self.connected_to.lock().await.push(address);
                let stream = TcpStream::connect(self.addr).await?;
                let stream: Box<dyn ProxyIo> = Box::new(stream);
                Ok(stream)
            })
        }
    }

    async fn echo_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    loop {
                        let Ok(read) = stream.read(&mut buffer).await else {
                            break;
                        };
                        if read == 0 || stream.write_all(&buffer[..read]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (addr, task)
    }

    async fn start_with(
        addresses: Vec<IpAddr>,
        connector: Arc<dyn Connector>,
        hosts: Vec<AllowedHost>,
    ) -> (
        ProxyServer,
        mpsc::Receiver<ConnectionOutcome>,
        Arc<FixedResolver>,
    ) {
        let resolver = Arc::new(FixedResolver::new(addresses));
        let (server, events) =
            ProxyServer::start(ProxyConfig::new(hosts), resolver.clone(), connector)
                .await
                .unwrap();
        (server, events, resolver)
    }

    async fn proxy_request(server: &ProxyServer, request: &[u8]) -> (Vec<u8>, TcpStream) {
        let mut client = TcpStream::connect(server.addr()).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while response.len() < 512 && !response.ends_with(b"\r\n\r\n") {
            let count = timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap();
            if count == 0 {
                break;
            }
            response.push(byte[0]);
        }
        (response, client)
    }

    #[cfg(unix)]
    async fn unix_proxy_request(server: &UnixProxyServer, request: &[u8]) -> (Vec<u8>, UnixStream) {
        let mut client = UnixStream::connect(server.socket_path()).await.unwrap();
        client.write_all(request).await.unwrap();
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while response.len() < 512 && !response.ends_with(b"\r\n\r\n") {
            let count = timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap();
            if count == 0 {
                break;
            }
            response.push(byte[0]);
        }
        (response, client)
    }

    #[cfg(unix)]
    fn fresh_socket_path() -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "tagteam-proxy-{}-{}.sock",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ))
    }

    fn allowed_host() -> AllowedHost {
        AllowedHost::new("crates.io").unwrap()
    }

    fn public_ip() -> IpAddr {
        "8.8.8.8".parse().unwrap()
    }

    #[test]
    fn defaults_match_readme_and_hostname_normalization_removes_one_dot() {
        assert_eq!(DEFAULT_EGRESS.len(), 7);
        assert_eq!(ProxyConfig::default().allowed_ports, [80, 443].into());
        assert_eq!(
            AllowedHost::new("CrAtEs.io.").unwrap().as_str(),
            "crates.io"
        );
        assert!(AllowedHost::new("crates.io..").is_err());
        assert!(AllowedHost::new("*.crates.io").is_err());
        assert!(AllowedHost::new("127.0.0.1").is_err());
    }

    #[test]
    fn parses_bracketed_ipv6_and_rejects_bad_authorities() {
        assert!(matches!(
            parse_authority("[2001:4860:4860::8888]:443"),
            Ok(Authority::IpLiteral)
        ));
        for invalid in [
            "crates.io",
            "crates.io:",
            "crates.io:0",
            "user@crates.io:443",
            "crates.io:443 ",
            "[::1]443",
            "[::1]:99999",
            "[crates.io]:443",
        ] {
            assert!(parse_authority(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn rejects_all_non_public_address_classes_and_accepts_public() {
        for raw in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "169.254.10.1",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "ff02::1",
            "fe80::1",
            "fc00::1",
            "2001:db8::1",
            "2001::1",
            "2002::1",
            "3fff::1",
            "3fff:0fff::1",
            "::ffff:10.1.2.3",
            "::ffff:100.64.0.1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.1.1",
            "::ffff:192.168.1.1",
        ] {
            let ip = raw.parse().unwrap();
            assert!(!is_public_unicast(ip), "{raw}");
        }
        assert!(is_public_unicast(public_ip()));
        assert!(is_public_unicast("2001:4860:4860::8888".parse().unwrap()));
        assert!(is_public_unicast("3fff:1000::1".parse().unwrap()));
    }

    #[tokio::test]
    async fn allowed_public_resolution_tunnels_bytes_through_local_connector() {
        let (upstream, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(upstream));
        let (server, mut events, _) =
            start_with(vec![public_ip()], connector.clone(), vec![allowed_host()]).await;
        let (response, mut client) = proxy_request(
            &server,
            b"CONNECT crates.io:443 HTTP/1.1\r\nHost: crates.io:443\r\n\r\nthrough-tunnel",
        )
        .await;
        assert!(response.starts_with(b"HTTP/1.1 200 Connection Established"));
        let mut echoed = [0; 14];
        timeout(Duration::from_secs(1), client.read_exact(&mut echoed))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&echoed, b"through-tunnel");
        drop(client);
        assert_eq!(events.recv().await, Some(ConnectionOutcome::Allowed));
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            connector.connected_to.lock().await[0],
            SocketAddr::new(public_ip(), 443)
        );
        server.shutdown().await;
        echo_task.abort();
    }

    #[tokio::test]
    async fn denies_unlisted_hostname_before_dns_or_connect() {
        let (unused, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(unused));
        let (server, mut events, resolver) =
            start_with(vec![public_ip()], connector.clone(), vec![allowed_host()]).await;
        let (response, _) =
            proxy_request(&server, b"CONNECT example.com:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 403"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::Denied(DenyReason::HostNotAllowed))
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
        server.shutdown().await;
        echo_task.abort();
    }

    #[tokio::test]
    async fn tcp_listener_uses_exact_hostname_policy() {
        let (unused, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(unused));
        let (server, mut events, resolver) =
            start_with(vec![public_ip()], connector.clone(), vec![allowed_host()]).await;
        let (response, _) =
            proxy_request(&server, b"CONNECT sub.crates.io:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 403"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::Denied(DenyReason::HostNotAllowed))
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
        server.shutdown().await;
        echo_task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener_enforces_policy_permissions_and_owned_socket_cleanup() {
        let (upstream, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(upstream));
        let resolver = Arc::new(FixedResolver::new(vec![public_ip()]));
        let path = fresh_socket_path();
        let (server, mut events) = ProxyServer::start_unix(
            &path,
            ProxyConfig::new(vec![allowed_host()]),
            resolver.clone(),
            connector.clone(),
        )
        .await
        .unwrap();
        let mode = std::fs::symlink_metadata(&path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        let (response, mut client) = unix_proxy_request(
            &server,
            b"CONNECT crates.io:443 HTTP/1.1\r\n\r\nthrough-tunnel",
        )
        .await;
        assert!(response.starts_with(b"HTTP/1.1 200 Connection Established"));
        let mut echoed = [0; 14];
        timeout(Duration::from_secs(1), client.read_exact(&mut echoed))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&echoed, b"through-tunnel");
        drop(client);
        assert_eq!(events.recv().await, Some(ConnectionOutcome::Allowed));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);

        let (response, _) =
            unix_proxy_request(&server, b"CONNECT sub.crates.io:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 403"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::Denied(DenyReason::HostNotAllowed))
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);

        server.shutdown().await;
        assert!(!path.exists());
        echo_task.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_listener_rejects_existing_and_symlink_paths_without_removing_them() {
        let existing_path = fresh_socket_path();
        std::fs::write(&existing_path, b"keep").unwrap();
        let resolver = Arc::new(FixedResolver::new(vec![public_ip()]));
        let connector = Arc::new(LocalConnector::new("127.0.0.1:1".parse().unwrap()));
        let result = ProxyServer::start_unix(
            &existing_path,
            ProxyConfig::default(),
            resolver.clone(),
            connector.clone(),
        )
        .await;
        assert!(matches!(result, Err(StartError::UnixPathExists(_))));
        assert_eq!(std::fs::read(&existing_path).unwrap(), b"keep");
        std::fs::remove_file(&existing_path).unwrap();

        let target = fresh_socket_path();
        std::fs::write(&target, b"keep").unwrap();
        let link = fresh_socket_path();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let result =
            ProxyServer::start_unix(&link, ProxyConfig::default(), resolver, connector).await;
        assert!(matches!(result, Err(StartError::InvalidUnixPath)));
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        std::fs::remove_file(link).unwrap();
        std::fs::remove_file(target).unwrap();
    }

    #[tokio::test]
    async fn empty_allowlist_denies_every_hostname() {
        let (unused, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(unused));
        let (server, mut events, resolver) =
            start_with(vec![public_ip()], connector.clone(), Vec::new()).await;
        let (response, _) = proxy_request(&server, b"CONNECT crates.io:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 403"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::Denied(DenyReason::HostNotAllowed))
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
        server.shutdown().await;
        echo_task.abort();
    }

    #[tokio::test]
    async fn denies_ip_literal_without_resolution_or_connection() {
        let (unused, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(unused));
        let (server, mut events, resolver) =
            start_with(vec![public_ip()], connector.clone(), vec![allowed_host()]).await;
        let (response, _) = proxy_request(&server, b"CONNECT 8.8.8.8:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 403"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::Denied(DenyReason::IpLiteral))
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
        server.shutdown().await;
        echo_task.abort();
    }

    #[tokio::test]
    async fn rejects_every_unsafe_or_mixed_resolution_without_connecting() {
        let unsafe_ips: Vec<IpAddr> = [
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fc00::1",
            "::ffff:127.0.0.1",
        ]
        .into_iter()
        .map(|ip| ip.parse().unwrap())
        .collect();
        for ip in unsafe_ips
            .into_iter()
            .chain(std::iter::once("192.168.1.1".parse().unwrap()))
        {
            let (unused, echo_task) = echo_server().await;
            let connector = Arc::new(LocalConnector::new(unused));
            let (server, mut events, _) =
                start_with(vec![ip], connector.clone(), vec![allowed_host()]).await;
            let (response, _) =
                proxy_request(&server, b"CONNECT crates.io:443 HTTP/1.1\r\n\r\n").await;
            assert!(response.starts_with(b"HTTP/1.1 403"), "{ip}");
            assert_eq!(
                events.recv().await,
                Some(ConnectionOutcome::Denied(DenyReason::UnsafeResolvedAddress))
            );
            assert_eq!(connector.calls.load(Ordering::SeqCst), 0, "{ip}");
            server.shutdown().await;
            echo_task.abort();
        }

        let (unused, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(unused));
        let (server, mut events, _) = start_with(
            vec![public_ip(), "10.0.0.1".parse().unwrap()],
            connector.clone(),
            vec![allowed_host()],
        )
        .await;
        let (response, _) = proxy_request(&server, b"CONNECT crates.io:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 403"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::Denied(DenyReason::UnsafeResolvedAddress))
        );
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
        server.shutdown().await;
        echo_task.abort();
    }

    #[tokio::test]
    async fn rejects_disallowed_port_and_malformed_requests() {
        let (unused, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(unused));
        let (server, mut events, _) =
            start_with(vec![public_ip()], connector.clone(), vec![allowed_host()]).await;
        let (response, _) =
            proxy_request(&server, b"CONNECT crates.io:8443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 403"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::Denied(DenyReason::PortNotAllowed))
        );
        for (wire, status) in [
            (&b"CONNECT crates.io HTTP/1.1\r\n\r\n"[..], 400),
            (&b"CONNECT crates.io:bad HTTP/1.1\r\n\r\n"[..], 400),
            (&b"CONNECT crates.io:443 HTTP/1.1\n\n"[..], 400),
            (
                &b"CONNECT crates.io:443 HTTP/1.1\r\nBad Header\r\n\r\n"[..],
                400,
            ),
            (&b"GET http://crates.io/ HTTP/1.1\r\n\r\n"[..], 405),
        ] {
            let (response, _) = proxy_request(&server, wire).await;
            assert!(
                response.starts_with(format!("HTTP/1.1 {status}").as_bytes()),
                "{response:?}"
            );
            let _ = events.recv().await;
        }
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
        server.shutdown().await;
        echo_task.abort();
    }

    #[tokio::test]
    async fn rejects_oversized_header_without_connecting() {
        let (unused, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(unused));
        let (server, mut events, _) =
            start_with(vec![public_ip()], connector.clone(), vec![allowed_host()]).await;
        let mut request = b"CONNECT crates.io:443 HTTP/1.1\r\nX-Pad: ".to_vec();
        request.extend(std::iter::repeat_n(b'a', MAX_HEADER_BYTES));
        request.extend_from_slice(b"\r\n\r\n");
        let (response, _) = proxy_request(&server, &request).await;
        assert!(response.starts_with(b"HTTP/1.1 400"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::Malformed(
                MalformedReason::HeaderTooLarge
            ))
        );
        assert_eq!(connector.calls.load(Ordering::SeqCst), 0);
        server.shutdown().await;
        echo_task.abort();
    }

    #[tokio::test]
    async fn maps_upstream_failure_to_502() {
        struct FailingConnector(AtomicUsize);
        impl Connector for FailingConnector {
            fn connect<'a>(
                &'a self,
                _address: SocketAddr,
            ) -> BoxFuture<'a, io::Result<Box<dyn ProxyIo>>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Err(io::Error::other("injected failure")) })
            }
        }
        let connector = Arc::new(FailingConnector(AtomicUsize::new(0)));
        let (server, mut events, _) =
            start_with(vec![public_ip()], connector.clone(), vec![allowed_host()]).await;
        let (response, _) = proxy_request(&server, b"CONNECT crates.io:443 HTTP/1.1\r\n\r\n").await;
        assert!(response.starts_with(b"HTTP/1.1 502"));
        assert_eq!(
            events.recv().await,
            Some(ConnectionOutcome::UpstreamFailure)
        );
        assert_eq!(connector.0.load(Ordering::SeqCst), 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn cancellation_closes_listener_and_active_tunnels() {
        let (upstream, echo_task) = echo_server().await;
        let connector = Arc::new(LocalConnector::new(upstream));
        let (server, _, _) = start_with(vec![public_ip()], connector, vec![allowed_host()]).await;
        let addr = server.addr();
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"CONNECT crates.io:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = [0; 128];
        let n = client.read(&mut response).await.unwrap();
        assert!(response[..n].starts_with(b"HTTP/1.1 200"));
        server.shutdown().await;
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(TcpStream::connect(addr).await.is_err());
        echo_task.abort();
    }
}
