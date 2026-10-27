#![cfg(any(target_os = "macos", target_os = "linux"))]

//! Exercises proxy egress and direct-network denial through the OS sandbox.
//!
//! This test owns a local TLS fixture and proxy; all assertions exercise the platform sandbox.

use std::{
    error::Error,
    future::Future,
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(target_os = "linux")]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use tagteam_core::{Bytes, Limits};
#[cfg(any(target_os = "macos", target_os = "linux"))]
use tagteam_proxy::ProxyServer;
use tagteam_proxy::{
    AllowedHost, ConnectionOutcome, Connector, DenyReason, ProxyConfig, ProxyIo, Resolver,
};
use tagteam_sandbox::{
    CancellationToken, ProxyEndpoint, ResourceLimits, SandboxProfile, spawn_confined,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

const PUBLIC_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
const MARKER: &[u8] = b"tagteam-egress-upstream-marker";
#[cfg(target_os = "linux")]
const LINUX_PROXY_BRIDGE: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 38_417);

struct FixedResolver;

impl Resolver for FixedResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
    ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
        Box::pin(async move {
            if host == "allowed.example" {
                Ok(vec![PUBLIC_IP])
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "unexpected DNS lookup",
                ))
            }
        })
    }
}

struct LocalTlsConnector {
    upstream: SocketAddr,
    validated: Arc<Mutex<Vec<SocketAddr>>>,
}

impl Connector for LocalTlsConnector {
    fn connect<'a>(
        &'a self,
        address: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = io::Result<Box<dyn ProxyIo>>> + Send + 'a>> {
        Box::pin(async move {
            self.validated
                .lock()
                .map_err(|_| io::Error::other("validated address log poisoned"))?
                .push(address);
            let stream = TcpStream::connect(self.upstream).await?;
            Ok(Box::new(stream) as Box<dyn ProxyIo>)
        })
    }
}

struct TlsUpstream {
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl TlsUpstream {
    async fn start() -> Result<Self, Box<dyn Error + Send + Sync>> {
        let identity = rcgen::generate_simple_self_signed(vec!["allowed.example".into()])?;
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![identity.cert.der().clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                    identity.signing_key.serialize_der(),
                )),
            )?;
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let (stop, mut stopping) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopping => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let acceptor = acceptor.clone();
                        handlers.spawn(async move {
                            let Ok(Ok(mut stream)) = timeout(
                                Duration::from_secs(5),
                                acceptor.accept(stream),
                            ).await else { return };
                            let mut request = [0; 2048];
                            let _ = timeout(Duration::from_secs(5), stream.read(&mut request)).await;
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                MARKER.len(),
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                            let _ = stream.write_all(MARKER).await;
                            let _ = stream.shutdown().await;
                        });
                    }
                }
            }
            handlers.abort_all();
            while handlers.join_next().await.is_some() {}
        });
        Ok(Self {
            address,
            stop: Some(stop),
            task,
        })
    }

    async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        let _ = timeout(Duration::from_secs(3), &mut self.task).await;
        self.task.abort();
    }
}

impl Drop for TlsUpstream {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

#[tokio::test]
async fn egress_is_proxy_only_inside_the_os_sandbox() -> Result<(), Box<dyn Error + Send + Sync>> {
    if cfg!(target_os = "linux")
        && (!Path::new("/usr/bin/systemd-run").exists() || !Path::new("/usr/bin/bwrap").exists())
    {
        eprintln!(
            "/usr/bin/systemd-run or /usr/bin/bwrap unavailable; skipping confined egress test"
        );
        return Ok(());
    }

    let upstream = TlsUpstream::start().await?;
    let validated = Arc::new(Mutex::new(Vec::new()));
    let connector = Arc::new(LocalTlsConnector {
        upstream: upstream.address,
        validated: Arc::clone(&validated),
    });
    let policy = ProxyConfig::with_ports(
        vec![AllowedHost::new("allowed.example")?],
        [443, upstream.address.port()].into_iter().collect(),
    );
    let workspace = tempfile::tempdir()?;
    for directory in ["repo", "home", "tmp"] {
        std::fs::create_dir(workspace.path().join(directory))?;
    }

    #[cfg(target_os = "macos")]
    let (proxy, mut outcomes) =
        ProxyServer::start(policy, Arc::new(FixedResolver), connector).await?;
    #[cfg(target_os = "linux")]
    let (proxy, mut outcomes) = {
        let socket = workspace.path().join("tmp/proxy.sock");
        ProxyServer::start_unix(&socket, policy, Arc::new(FixedResolver), connector).await?
    };

    #[cfg(target_os = "macos")]
    let proxy_address = proxy.addr();
    #[cfg(target_os = "linux")]
    let proxy_address = LINUX_PROXY_BRIDGE;
    let allowed = curl_argv("https://allowed.example/marker", &[]);
    let allowed_result = run_confined(workspace.path(), proxy_address, &allowed).await?;
    assert_eq!(
        allowed_result.exit_code,
        Some(0),
        "curl failed: {}",
        String::from_utf8_lossy(&allowed_result.stderr)
    );
    assert!(
        allowed_result
            .stdout
            .windows(MARKER.len())
            .any(|bytes| bytes == MARKER),
        "successful proxied request did not return the upstream marker: {allowed_result:?}"
    );
    assert_eq!(
        *validated
            .lock()
            .map_err(|_| "validated address log poisoned")?,
        vec![SocketAddr::new(PUBLIC_IP, 443)],
        "proxy connector did not receive the fake resolver's validated public address"
    );
    assert_eq!(
        next_outcome(&mut outcomes).await?,
        ConnectionOutcome::Allowed
    );

    let denied = curl_argv("https://denied.example/marker", &[]);
    let denied_result = run_confined(workspace.path(), proxy_address, &denied).await?;
    assert_ne!(
        denied_result.exit_code,
        Some(0),
        "denied host unexpectedly succeeded"
    );
    assert!(
        String::from_utf8_lossy(&denied_result.stderr).contains("403"),
        "denied curl did not report HTTP 403: {}",
        String::from_utf8_lossy(&denied_result.stderr)
    );
    assert_eq!(
        next_outcome(&mut outcomes).await?,
        ConnectionOutcome::Denied(DenyReason::HostNotAllowed)
    );

    let ip_literal_url = format!("https://127.0.0.1:{}/", upstream.address.port());
    let ip_literal = curl_argv(&ip_literal_url, &[]);
    let ip_literal_result = run_confined(workspace.path(), proxy_address, &ip_literal).await?;
    assert_ne!(
        ip_literal_result.exit_code,
        Some(0),
        "IP-literal CONNECT unexpectedly succeeded"
    );
    assert_eq!(
        next_outcome(&mut outcomes).await?,
        ConnectionOutcome::Denied(DenyReason::IpLiteral)
    );

    let direct = curl_argv(&ip_literal_url, &["--noproxy", "*"]);
    let direct_result = run_confined(workspace.path(), proxy_address, &direct).await?;
    assert_ne!(
        direct_result.exit_code,
        Some(0),
        "direct connection reached the local TLS upstream"
    );
    let direct_error = String::from_utf8_lossy(&direct_result.stderr);
    assert!(
        direct_error.contains("Failed to connect") || direct_error.contains("Couldn't connect"),
        "direct curl failed for a reason other than blocked egress: {direct_error}"
    );

    proxy.shutdown().await;
    upstream.shutdown().await;
    drop(workspace);
    Ok(())
}

fn curl_argv(url: &str, extra: &[&str]) -> Vec<String> {
    let mut argv = vec![
        "/usr/bin/curl".to_owned(),
        "--silent".to_owned(),
        "--show-error".to_owned(),
        "--fail".to_owned(),
        "--insecure".to_owned(),
        "--connect-timeout".to_owned(),
        "2".to_owned(),
        "--max-time".to_owned(),
        "6".to_owned(),
        "--noproxy".to_owned(),
        String::new(),
    ];
    argv.extend(extra.iter().map(|value| (*value).to_owned()));
    argv.push(url.to_owned());
    argv
}

async fn run_confined(
    workspace: &Path,
    proxy_address: SocketAddr,
    argv: &[String],
) -> Result<tagteam_sandbox::SandboxOutcome, Box<dyn Error + Send + Sync>> {
    let limits = Limits {
        wall_clock: Duration::from_secs(20),
        command_timeout: Duration::from_secs(12),
        ..Limits::default()
    };
    let resource_limits = ResourceLimits::new(
        Bytes::mib(512),
        std::num::NonZeroU32::new(32).ok_or("invalid process limit")?,
        Bytes::mib(128),
        Duration::from_millis(100),
    )?;
    let command = sandbox_command(argv)?;
    let proxy_endpoint = ProxyEndpoint::new(proxy_address, workspace.join("tmp/proxy.sock"))?;
    let profile = SandboxProfile::new(workspace, command, proxy_endpoint, limits, resource_limits)?;
    #[cfg(target_os = "macos")]
    let profile = profile
        // macOS curl's LibreSSL setup reads this system configuration before TLS.
        .with_toolchain_root(Path::new("/private/etc/ssl").to_path_buf())?;
    #[cfg(target_os = "linux")]
    let profile = {
        let executable = Path::new(env!("CARGO_BIN_EXE_tagteam-node"));
        let toolchain_root = executable
            .parent()
            .ok_or("node test executable has no parent directory")?;
        profile
            .with_toolchain_root(toolchain_root.to_path_buf())?
            .with_environment("TAGTEAM_PROXY_SOCKET".into(), "/run/proxy.sock".into())?
    };
    let (cancel, token) = CancellationToken::new();
    match timeout(Duration::from_secs(30), spawn_confined(profile, token)).await {
        Ok(result) => Ok(result?),
        Err(error) => {
            cancel.cancel();
            Err(Box::new(error))
        }
    }
}

async fn next_outcome(
    outcomes: &mut tokio::sync::mpsc::Receiver<ConnectionOutcome>,
) -> Result<ConnectionOutcome, Box<dyn Error + Send + Sync>> {
    Ok(timeout(Duration::from_secs(3), outcomes.recv())
        .await?
        .ok_or("proxy outcome channel closed")?)
}

#[cfg(target_os = "macos")]
fn sandbox_command(argv: &[String]) -> Result<String, Box<dyn Error + Send + Sync>> {
    Ok(argv
        .iter()
        .map(|value| shell_quote(value))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(target_os = "linux")]
fn sandbox_command(argv: &[String]) -> Result<String, Box<dyn Error + Send + Sync>> {
    let request = tagteam_node::fsop::ExecRequest {
        argv: argv.to_vec(),
        cwd: String::new(),
        env: Default::default(),
        stdin: String::new(),
    };
    let encoded = STANDARD.encode(serde_json::to_vec(&request)?);
    let executable = Path::new(env!("CARGO_BIN_EXE_tagteam-node"));
    let filename = executable
        .file_name()
        .ok_or("node test executable has no filename")?
        .to_string_lossy();
    Ok(format!("/toolchains/0/{filename} __exec {encoded}"))
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
