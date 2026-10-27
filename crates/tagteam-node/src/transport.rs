//! Pinned outbound TLS transport shared by enrollment and sessions.
//!
//! The invite address selects only the TCP destination. TLS always authenticates the
//! fixed `tagteam-hub` service name and the invite-pinned CA before tonic can send an RPC.

use std::{future::Future, io, pin::Pin, sync::Arc};

use hyper_util::rt::TokioIo;
use rustls::pki_types::{PrivateKeyDer, ServerName};
use tokio::net::TcpStream;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tonic::{
    codegen::Service,
    transport::{Channel, Endpoint, Uri},
};

use crate::{NodeIdentity, enrollment::EnrollmentTarget};
use tagteam_pki::{CertificateFingerprint, PinnedHubVerifier, SERVICE_DNS_NAME};

#[derive(Clone)]
struct TlsService {
    connector: TlsConnector,
}

impl Service<Uri> for TlsService {
    type Response = TokioIo<TlsStream<TcpStream>>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let connector = self.connector.clone();
        Box::pin(async move {
            let authority = uri.authority().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "hub address is invalid")
            })?;
            let host = authority.host();
            let port = authority.port_u16().unwrap_or(443);
            let socket = TcpStream::connect((host, port)).await?;
            let server_name = ServerName::try_from(SERVICE_DNS_NAME.to_owned()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "hub service name is invalid")
            })?;
            connector
                .connect(server_name, socket)
                .await
                .map(TokioIo::new)
                .map_err(io::Error::other)
        })
    }
}

pub(crate) async fn enrollment_channel(
    target: &EnrollmentTarget,
) -> Result<(Channel, Arc<PinnedHubVerifier>), TransportError> {
    let (connector, verifier) = tls_connector(target.fingerprint, None)?;
    let channel = channel_for_address(&target.address, connector).await?;
    Ok((channel, verifier))
}

pub(crate) async fn session_channel(identity: &NodeIdentity) -> Result<Channel, TransportError> {
    let (connector, _) = tls_connector(identity.hub_fingerprint, Some(identity))?;
    channel_for_address(&identity.hub_address, connector).await
}

fn tls_connector(
    fingerprint: CertificateFingerprint,
    identity: Option<&NodeIdentity>,
) -> Result<(TlsConnector, Arc<PinnedHubVerifier>), TransportError> {
    let verifier =
        Arc::new(PinnedHubVerifier::new(fingerprint).map_err(|_| TransportError::Verifier)?);
    let verifier_for_config: Arc<dyn rustls::client::danger::ServerCertVerifier> = verifier.clone();
    let builder = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
    .map_err(|_| TransportError::Verifier)?
    .dangerous()
    .with_custom_certificate_verifier(verifier_for_config);
    let config = match identity {
        Some(identity) => {
            let certificate =
                rustls::pki_types::CertificateDer::from(identity.certificate_der.clone());
            let private_key = PrivateKeyDer::try_from(identity.private_key_der.clone())
                .map_err(|_| TransportError::Identity)?;
            builder
                .with_client_auth_cert(vec![certificate], private_key)
                .map_err(|_| TransportError::Identity)?
        }
        None => builder.with_no_client_auth(),
    };
    Ok((TlsConnector::from(Arc::new(config)), verifier))
}

async fn channel_for_address(
    address: &str,
    connector: TlsConnector,
) -> Result<Channel, TransportError> {
    let uri = address
        .parse::<Uri>()
        .map_err(|_| TransportError::Address)?;
    if uri.scheme_str() != Some("https")
        || uri.path_and_query().is_some_and(|path| path.path() != "/")
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
    {
        return Err(TransportError::Address);
    }
    let authority = uri.authority().ok_or(TransportError::Address)?;
    let endpoint = Endpoint::from_shared(format!("http://{authority}"))
        .map_err(|_| TransportError::Address)?
        .connect_timeout(std::time::Duration::from_secs(10));
    endpoint
        .connect_with_connector(TlsService { connector })
        .await
        .map_err(|_| TransportError::Connect)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TransportError {
    #[error("hub address is invalid")]
    Address,
    #[error("could not establish a verified hub connection")]
    Connect,
    #[error("node identity is invalid")]
    Identity,
    #[error("pinned hub verifier could not be initialized")]
    Verifier,
}
