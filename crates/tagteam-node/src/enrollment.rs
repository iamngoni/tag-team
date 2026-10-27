//! Invite consumption and locally generated node identity enrollment.
//!
//! The invite code and private key remain process-local; only the CSR is sent over the
//! pinned TLS connection. Durable storage is an injected boundary and must protect keys.

use std::str::FromStr;

use tagteam_core::NodeId;
use tagteam_pki::{CertificateFingerprint, Invite, NodeKeyRequest, PkiError};
use tonic::Code;

use crate::transport;

/// Identity material required by the node session client.
///
/// The private key is deliberately excluded from `Debug`; callers must treat it as a secret.
pub struct NodeIdentity {
    pub node_id: NodeId,
    pub certificate_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
    pub hub_address: String,
    pub hub_fingerprint: CertificateFingerprint,
}

/// Narrow persistence boundary for implementations such as a protected file store.
/// This interface makes no claim about platform keychain integration.
pub trait IdentityStore {
    fn persist(&self, identity: &NodeIdentity) -> Result<(), IdentityStoreError>;
}

#[derive(Debug, thiserror::Error)]
#[error("node identity could not be persisted")]
pub struct IdentityStoreError;

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("enrollment invite is invalid")]
    Invite,
    #[error("hub connection could not be verified")]
    Transport,
    #[error("node key or certificate is invalid")]
    Pki(#[from] PkiError),
    #[error("hub rejected enrollment ({0:?})")]
    EnrollmentRejected(Code),
    #[error("hub returned an invalid node identity")]
    InvalidIdentity,
    #[error("node identity storage failed")]
    Storage(#[from] IdentityStoreError),
}

/// Performs node enrollment through the caller-supplied one-time invite.
pub struct NodeEnrollment;

impl NodeEnrollment {
    pub async fn enroll(
        encoded_invite: &str,
        name: impl Into<String>,
        platform: tagteam_proto::v1::Platform,
    ) -> Result<NodeIdentity, NodeError> {
        let invite = Invite::decode(encoded_invite).map_err(|_| NodeError::Invite)?;
        let target = EnrollmentTarget {
            address: invite.address.clone(),
            fingerprint: invite.hub_fingerprint,
        };
        let (channel, verifier) = transport::enrollment_channel(&target)
            .await
            .map_err(|_| NodeError::Transport)?;
        let key = NodeKeyRequest::generate("tagteam-node")?;
        let mut client = tagteam_proto::v1::hub_client::HubClient::new(channel);
        let reply = client
            .enroll(tagteam_proto::v1::EnrollRequest {
                code: encoded_invite.to_owned(),
                csr_der: key.csr_der,
                name: name.into(),
                platform: Some(platform),
            })
            .await
            .map_err(|status| NodeError::EnrollmentRejected(status.code()))?
            .into_inner();
        let ca_der = verifier
            .root_certificate_der()
            .ok_or(NodeError::InvalidIdentity)?;
        tagteam_pki::verify_node_certificate(&reply.cert_der, &ca_der, invite.hub_fingerprint)?;
        let node_id = NodeId::from_str(&reply.node_id).map_err(|_| NodeError::InvalidIdentity)?;
        Ok(NodeIdentity {
            node_id,
            certificate_der: reply.cert_der,
            private_key_der: key.private_key_der,
            hub_address: invite.address,
            hub_fingerprint: invite.hub_fingerprint,
        })
    }

    pub async fn enroll_and_persist(
        encoded_invite: &str,
        name: impl Into<String>,
        platform: tagteam_proto::v1::Platform,
        store: &impl IdentityStore,
    ) -> Result<NodeIdentity, NodeError> {
        let identity = Self::enroll(encoded_invite, name, platform).await?;
        store.persist(&identity)?;
        Ok(identity)
    }
}

pub(crate) struct EnrollmentTarget {
    pub(crate) address: String,
    pub(crate) fingerprint: CertificateFingerprint,
}
