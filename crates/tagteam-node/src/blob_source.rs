//! Authenticated hub downloads for content-addressed workspace bundles.
//!
//! The source reuses the node's pinned mTLS transport and returns bytes only after the
//! complete stream matches the requested digest and size within the shared blob limit.

use std::{future::Future, pin::Pin};

use sha2::{Digest, Sha256};
use tagteam_core::MAX_BLOB_BYTES;
use tagteam_proto::v1::{self as proto, chunk::Body};
use tonic::transport::Channel;

use crate::{BundleSource, NodeIdentity, transport};

/// Hub's streamed blob endpoint accepts data frames up to one mebibyte each.
const MAX_CHUNK_BYTES: usize = 1024 * 1024;

/// Fetches and verifies workspace bundles from the hub over the enrolled node identity.
pub struct HubBundleSource {
    channel: Channel,
}

impl HubBundleSource {
    /// Creates a blob source using the hub address, pinned CA, and client certificate in the
    /// persisted node identity.
    pub async fn connect(identity: &NodeIdentity) -> Result<Self, String> {
        let channel = transport::session_channel(identity)
            .await
            .map_err(|_| "hub connection could not be verified".to_owned())?;
        Ok(Self { channel })
    }

    /// Alias for [`Self::connect`].
    pub async fn new(identity: &NodeIdentity) -> Result<Self, String> {
        Self::connect(identity).await
    }
}

impl BundleSource for HubBundleSource {
    fn bundle_bytes(
        &self,
        reference: &proto::BlobRef,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send + '_>> {
        let channel = self.channel.clone();
        let reference = reference.clone();
        Box::pin(async move {
            let expected_digest: [u8; 32] = reference
                .sha256
                .as_slice()
                .try_into()
                .map_err(|_| "bundle reference digest is invalid".to_owned())?;
            if reference.size > MAX_BLOB_BYTES {
                return Err("bundle exceeds the allowed size".to_owned());
            }

            let mut client = proto::hub_client::HubClient::new(channel);
            let mut stream = client
                .get_blob(reference.clone())
                .await
                .map_err(|_| "hub bundle download failed".to_owned())?
                .into_inner();

            let first = stream
                .message()
                .await
                .map_err(|_| "hub bundle stream is invalid".to_owned())?
                .ok_or_else(|| "hub bundle stream is empty".to_owned())?;
            let Some(Body::Header(header)) = first.body else {
                return Err("hub bundle stream header is missing".to_owned());
            };
            let header_digest: [u8; 32] = header
                .sha256
                .as_slice()
                .try_into()
                .map_err(|_| "hub bundle stream header is invalid".to_owned())?;
            if header.size > MAX_BLOB_BYTES
                || header_digest != expected_digest
                || header.size != reference.size
            {
                return Err("hub bundle stream header does not match the reference".to_owned());
            }

            let capacity = usize::try_from(header.size)
                .map_err(|_| "hub bundle size is invalid".to_owned())?;
            let mut bytes = Vec::with_capacity(capacity);
            while let Some(chunk) = stream
                .message()
                .await
                .map_err(|_| "hub bundle stream is invalid".to_owned())?
            {
                match chunk.body {
                    Some(Body::Data(data)) if !data.is_empty() => {
                        let new_size = bytes
                            .len()
                            .checked_add(data.len())
                            .ok_or_else(|| "hub bundle exceeds its declared size".to_owned())?;
                        match u64::try_from(new_size) {
                            Ok(size) if size <= header.size => {}
                            _ => {
                                return Err("hub bundle exceeds its declared size".to_owned());
                            }
                        }
                        bytes.extend_from_slice(&data);
                    }
                    Some(Body::Data(_)) => {
                        return Err("hub bundle contains an empty data chunk".to_owned());
                    }
                    Some(Body::Header(_)) => {
                        return Err("hub bundle contains a second header".to_owned());
                    }
                    None => return Err("hub bundle contains an empty frame".to_owned()),
                }
            }

            if u64::try_from(bytes.len()) != Ok(reference.size) {
                return Err("hub bundle size does not match the reference".to_owned());
            }
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            if digest != expected_digest {
                return Err("hub bundle digest does not match the reference".to_owned());
            }
            Ok(bytes)
        })
    }

    fn store_bundle(
        &self,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<proto::BlobRef, String>> + Send + '_>> {
        let channel = self.channel.clone();
        Box::pin(async move {
            if u64::try_from(bytes.len()).map_or(true, |size| size > MAX_BLOB_BYTES) {
                return Err("bundle exceeds the allowed size".to_owned());
            }
            let size =
                u64::try_from(bytes.len()).map_err(|_| "bundle size is invalid".to_owned())?;
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            let header = proto::Chunk {
                body: Some(Body::Header(proto::BlobHeader {
                    sha256: digest.to_vec(),
                    size,
                })),
            };
            let mut chunks = Vec::with_capacity(bytes.len().div_ceil(MAX_CHUNK_BYTES) + 1);
            chunks.push(header);
            chunks.extend(bytes.chunks(MAX_CHUNK_BYTES).map(|data| proto::Chunk {
                body: Some(Body::Data(data.to_vec())),
            }));
            let mut client = proto::hub_client::HubClient::new(channel);
            let reference = client
                .put_blob(tokio_stream::iter(chunks))
                .await
                .map_err(|_| "hub bundle upload failed".to_owned())?
                .into_inner();
            let returned_digest: [u8; 32] = reference
                .sha256
                .as_slice()
                .try_into()
                .map_err(|_| "hub returned an invalid bundle reference".to_owned())?;
            if returned_digest != digest || reference.size != size {
                return Err("hub returned a mismatched bundle reference".to_owned());
            }
            Ok(reference)
        })
    }
}
