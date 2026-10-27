//! Private certificate authority, node invites, and local node key storage.

use std::{
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use data_encoding::BASE32_NOPAD;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, CertifiedIssuer,
    DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ED25519,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const SERVICE_DNS_NAME: &str = "tagteam-hub";
const INVITE_PREFIX: &str = "tt1";
const FINGERPRINT_BYTES: usize = 32;
const INVITE_CODE_BYTES: usize = 32;
const INVITE_CODE_LEN: usize = 52;
const MAX_ADDRESS_BYTES: usize = 2_048;

#[derive(Debug, Error)]
pub enum PkiError {
    #[error("malformed invite: {0}")]
    MalformedInvite(&'static str),
    #[error("invite has expired")]
    InviteExpired,
    #[error("invite has already been used")]
    InviteReused,
    #[error("invite code does not match")]
    InviteCodeMismatch,
    #[error("certificate was not issued by the expected hub CA")]
    WrongCa,
    #[error("invalid certificate signing request: {0}")]
    InvalidCsr(String),
    #[error("certificate generation failed")]
    Certificate(#[from] rcgen::Error),
    #[error("key file permissions are broader than 0600")]
    InsecureKeyPermissions,
    #[error("key file contains no private key")]
    EmptyPrivateKey,
    #[error("system clock is before the Unix epoch")]
    InvalidClock(#[from] std::time::SystemTimeError),
    #[error("key storage I/O failed: {0}")]
    Io(#[from] io::Error),
}

/// A rustls server verifier pinned to the hub CA fingerprint from an enrollment invite.
///
/// The hub CA must be sent as the final certificate in the server's presented chain. Its
/// fingerprint establishes the trust anchor; WebPKI then validates the leaf, chain, validity
/// periods, and fixed `tagteam-hub` service name. TLS handshake signatures use WebPKI's
/// configured cryptographic provider.
#[derive(Debug)]
pub struct PinnedHubVerifier {
    fingerprint: CertificateFingerprint,
    signature_algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
    root_certificate_der: Mutex<Option<Vec<u8>>>,
}

impl PinnedHubVerifier {
    /// Creates a verifier for the hub CA fingerprint encoded in the enrollment invite.
    pub fn new(fingerprint: CertificateFingerprint) -> Result<Self, PkiError> {
        Ok(Self {
            fingerprint,
            signature_algorithms: rustls::crypto::ring::default_provider()
                .signature_verification_algorithms,
            root_certificate_der: Mutex::new(None),
        })
    }

    /// Returns the pinned CA DER retained from the most recently verified hub chain.
    ///
    /// This is `None` until a server certificate has passed chain and DNS validation.
    pub fn root_certificate_der(&self) -> Option<Vec<u8>> {
        self.root_certificate_der.lock().ok()?.clone()
    }
}

impl rustls::client::danger::ServerCertVerifier for PinnedHubVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use rustls::{RootCertStore, client::WebPkiServerVerifier, pki_types::CertificateDer};

        let has_service_name = matches!(server_name,
            rustls::pki_types::ServerName::DnsName(name)
                if name.as_ref() == SERVICE_DNS_NAME
        );
        if !has_service_name {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName,
            ));
        }

        let Some(root_certificate) = intermediates.last() else {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ));
        };
        if !self
            .fingerprint
            .matches_certificate(root_certificate.as_ref())
        {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::UnknownIssuer,
            ));
        }

        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(root_certificate.as_ref().to_vec()))
            .map_err(|_| {
                rustls::Error::InvalidCertificate(rustls::CertificateError::BadEncoding)
            })?;
        let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|_| {
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
            })?;

        // The pinned root is a trust anchor, not an intermediate in the path being validated.
        let verified = verifier.verify_server_cert(
            end_entity,
            &intermediates[..intermediates.len() - 1],
            server_name,
            ocsp_response,
            now,
        )?;
        self.root_certificate_der
            .lock()
            .map_err(|_| rustls::Error::General("hub verifier state is unavailable".into()))?
            .replace(root_certificate.as_ref().to_vec());
        Ok(verified)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            dss,
            &self.signature_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            dss,
            &self.signature_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.signature_algorithms.supported_schemes()
    }
}

/// SHA-256 fingerprint of the exact DER-encoded certificate bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CertificateFingerprint([u8; FINGERPRINT_BYTES]);

impl CertificateFingerprint {
    pub fn from_certificate(certificate_der: &[u8]) -> Self {
        Self(Sha256::digest(certificate_der).into())
    }

    pub const fn as_bytes(&self) -> &[u8; FINGERPRINT_BYTES] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut result = String::with_capacity(FINGERPRINT_BYTES * 2);
        for byte in self.0 {
            result.push(HEX[(byte >> 4) as usize] as char);
            result.push(HEX[(byte & 0x0f) as usize] as char);
        }
        result
    }

    fn parse_hex(input: &str) -> Result<Self, PkiError> {
        if input.len() != FINGERPRINT_BYTES * 2
            || !input
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(PkiError::MalformedInvite(
                "certificate fingerprint must be 64 lowercase hex characters",
            ));
        }
        let mut bytes = [0; FINGERPRINT_BYTES];
        for (index, pair) in input.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        }
        Ok(Self(bytes))
    }

    /// Constant-time comparison for a fingerprint presented by a peer.
    pub fn matches_certificate(&self, certificate_der: &[u8]) -> bool {
        constant_time_eq(&self.0, &Sha256::digest(certificate_der))
    }

    /// Match the fingerprint of a presented TLS certificate against the enrolled node cert.
    pub fn matches_presented_client_certificate(&self, presented_certificate_der: &[u8]) -> bool {
        self.matches_certificate(presented_certificate_der)
    }
}

fn hex_nibble(value: u8) -> Result<u8, PkiError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(PkiError::MalformedInvite("invalid fingerprint hex")),
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

/// A decoded invite. The expiry and spent state are maintained by the hub journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub address: String,
    pub hub_fingerprint: CertificateFingerprint,
    code: [u8; INVITE_CODE_BYTES],
}

impl Invite {
    pub fn new(
        address: impl Into<String>,
        hub_fingerprint: CertificateFingerprint,
        code: [u8; INVITE_CODE_BYTES],
    ) -> Result<Self, PkiError> {
        let address = address.into();
        validate_address(&address)?;
        Ok(Self {
            address,
            hub_fingerprint,
            code,
        })
    }

    pub fn encode(&self) -> String {
        format!(
            "{INVITE_PREFIX}.{}.{}.{}",
            URL_SAFE_NO_PAD.encode(self.address.as_bytes()),
            self.hub_fingerprint.to_hex(),
            BASE32_NOPAD.encode(&self.code),
        )
    }

    pub fn decode(encoded: &str) -> Result<Self, PkiError> {
        let mut parts = encoded.split('.');
        if parts.next() != Some(INVITE_PREFIX) {
            return Err(PkiError::MalformedInvite("unsupported prefix"));
        }
        let address_part = parts
            .next()
            .ok_or(PkiError::MalformedInvite("missing address"))?;
        let fingerprint_part = parts
            .next()
            .ok_or(PkiError::MalformedInvite("missing fingerprint"))?;
        let code_part = parts
            .next()
            .ok_or(PkiError::MalformedInvite("missing code"))?;
        if parts.next().is_some() {
            return Err(PkiError::MalformedInvite("unexpected component"));
        }
        const MAX_ADDRESS_COMPONENT_LEN: usize = MAX_ADDRESS_BYTES.div_ceil(3) * 4;
        if address_part.is_empty() || address_part.len() > MAX_ADDRESS_COMPONENT_LEN {
            return Err(PkiError::MalformedInvite(
                "address component has invalid length",
            ));
        }
        let address_bytes = URL_SAFE_NO_PAD
            .decode(address_part)
            .map_err(|_| PkiError::MalformedInvite("address is not unpadded base64url"))?;
        if URL_SAFE_NO_PAD.encode(&address_bytes) != address_part {
            return Err(PkiError::MalformedInvite(
                "address encoding is not canonical",
            ));
        }
        let address = String::from_utf8(address_bytes)
            .map_err(|_| PkiError::MalformedInvite("address is not UTF-8"))?;
        validate_address(&address)?;
        let hub_fingerprint = CertificateFingerprint::parse_hex(fingerprint_part)?;
        if code_part.len() != INVITE_CODE_LEN {
            return Err(PkiError::MalformedInvite(
                "code must be 52 base32 characters",
            ));
        }
        let decoded_code = BASE32_NOPAD
            .decode(code_part.as_bytes())
            .map_err(|_| PkiError::MalformedInvite("code is not unpadded base32"))?;
        let code: [u8; INVITE_CODE_BYTES] = decoded_code
            .try_into()
            .map_err(|_| PkiError::MalformedInvite("code has invalid decoded length"))?;
        if BASE32_NOPAD.encode(&code) != code_part {
            return Err(PkiError::MalformedInvite("code encoding is not canonical"));
        }
        Ok(Self {
            address,
            hub_fingerprint,
            code,
        })
    }

    /// Check code, expiry, and single-use state. `now_unix` and `expires_at_unix`
    /// come from the hub journal; the code comparison itself is constant-time.
    pub fn validate_spend(
        &self,
        submitted_code: &[u8],
        now_unix: u64,
        expires_at_unix: u64,
        already_used: bool,
    ) -> Result<(), PkiError> {
        if now_unix >= expires_at_unix {
            return Err(PkiError::InviteExpired);
        }
        if already_used {
            return Err(PkiError::InviteReused);
        }
        let submitted_hash = Sha256::digest(submitted_code);
        let expected_hash = Sha256::digest(self.code);
        if !constant_time_eq(&expected_hash, &submitted_hash) {
            return Err(PkiError::InviteCodeMismatch);
        }
        Ok(())
    }

    pub fn code_bytes(&self) -> &[u8; INVITE_CODE_BYTES] {
        &self.code
    }

    pub fn code_hash(&self) -> [u8; 32] {
        Sha256::digest(self.code).into()
    }
}

fn validate_address(address: &str) -> Result<(), PkiError> {
    if address.is_empty()
        || address.len() > MAX_ADDRESS_BYTES
        || address.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(PkiError::MalformedInvite(
            "address is empty, too long, or contains control characters",
        ));
    }
    Ok(())
}

/// Private hub CA and the signing key kept in memory by the hub process.
pub struct HubCa {
    issuer: CertifiedIssuer<'static, KeyPair>,
}

impl HubCa {
    pub fn generate() -> Result<Self, PkiError> {
        let key_pair = KeyPair::generate_for(&PKCS_ED25519)?;
        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "Tag Team Private Hub CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let issuer = CertifiedIssuer::self_signed(params, key_pair)?;
        Ok(Self { issuer })
    }

    pub fn certificate_der(&self) -> &[u8] {
        self.issuer.der().as_ref()
    }

    pub fn fingerprint(&self) -> CertificateFingerprint {
        CertificateFingerprint::from_certificate(self.certificate_der())
    }

    pub fn issue_server_certificate(&self) -> Result<IssuedCertificate, PkiError> {
        let key_pair = KeyPair::generate_for(&PKCS_ED25519)?;
        let mut params = CertificateParams::new(vec![SERVICE_DNS_NAME.to_owned()])?;
        params
            .distinguished_name
            .push(DnType::CommonName, SERVICE_DNS_NAME);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let certificate = params.signed_by(&key_pair, &self.issuer)?;
        Ok(IssuedCertificate {
            certificate_der: certificate.der().as_ref().to_vec(),
            private_key_der: key_pair.serialize_der(),
        })
    }

    /// Verify the CSR signature, discard requested privilege-bearing extensions, and
    /// sign the CSR public key as a client-only node certificate.
    pub fn sign_node_csr(&self, csr_der: &[u8]) -> Result<Vec<u8>, PkiError> {
        let mut csr = CertificateSigningRequestParams::from_der(&csr_der.into())
            .map_err(|error| PkiError::InvalidCsr(error.to_string()))?;
        let params = &mut csr.params;
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.name_constraints = None;
        params.crl_distribution_points.clear();
        params.custom_extensions.clear();
        params.use_authority_key_identifier_extension = true;
        let issuer = Issuer::from_ca_cert_der(self.issuer.der(), self.issuer.key())
            .map_err(PkiError::Certificate)?;
        let certificate = csr
            .signed_by(&issuer)
            .map_err(|error| PkiError::InvalidCsr(error.to_string()))?;
        Ok(certificate.der().as_ref().to_vec())
    }

    /// Check that a certificate chains to this hub CA using rustls WebPKI validation.
    pub fn verify_issued_certificate(&self, certificate_der: &[u8]) -> Result<(), PkiError> {
        verify_chain(certificate_der, self.certificate_der())
    }
}

/// DER certificate and PKCS#8 private key, returned when generating a server identity.
#[derive(Clone, Debug)]
pub struct IssuedCertificate {
    pub certificate_der: Vec<u8>,
    pub private_key_der: Vec<u8>,
}

/// Locally generated node key and its CSR. Only the CSR should be sent to the hub.
#[derive(Debug)]
pub struct NodeKeyRequest {
    pub private_key_der: Vec<u8>,
    pub csr_der: Vec<u8>,
}

impl NodeKeyRequest {
    pub fn generate(common_name: &str) -> Result<Self, PkiError> {
        let key_pair = KeyPair::generate_for(&PKCS_ED25519)?;
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, common_name);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let csr = params.serialize_request(&key_pair)?;
        Ok(Self {
            private_key_der: key_pair.serialize_der(),
            csr_der: csr.der().as_ref().to_vec(),
        })
    }
}

fn verify_chain(certificate_der: &[u8], ca_der: &[u8]) -> Result<(), PkiError> {
    use rustls::{RootCertStore, pki_types::CertificateDer, server::WebPkiClientVerifier};
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(ca_der.to_vec()))
        .map_err(|_| PkiError::WrongCa)?;
    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|_| PkiError::WrongCa)?;
    let certificate = CertificateDer::from(certificate_der.to_vec());
    verifier
        .verify_client_cert(&certificate, &[], rustls::pki_types::UnixTime::now())
        .map(|_| ())
        .map_err(|_| PkiError::WrongCa)
}

/// Verifies an enrolled node certificate against the exact CA pinned by its invite.
pub fn verify_node_certificate(
    certificate_der: &[u8],
    ca_der: &[u8],
    fingerprint: CertificateFingerprint,
) -> Result<(), PkiError> {
    if !fingerprint.matches_certificate(ca_der) {
        return Err(PkiError::WrongCa);
    }
    verify_chain(certificate_der, ca_der)
}

/// Pluggable boundary for future macOS Keychain and Linux file-backed storage.
pub trait KeyStore {
    fn store_private_key(&self, key_id: &str, private_key_der: &[u8]) -> Result<(), PkiError>;
    fn load_private_key(&self, key_id: &str) -> Result<Vec<u8>, PkiError>;
}

/// Root-only filesystem keystore suitable for Linux and development.
#[derive(Clone, Debug)]
pub struct FileKeyStore {
    directory: PathBuf,
}

impl FileKeyStore {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    fn path_for(&self, key_id: &str) -> Result<PathBuf, PkiError> {
        if key_id.is_empty()
            || !key_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
        {
            return Err(PkiError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "key ID must contain only ASCII letters, digits, '-' or '_'",
            )));
        }
        Ok(self.directory.join(format!("{key_id}.key")))
    }
}

impl KeyStore for FileKeyStore {
    fn store_private_key(&self, key_id: &str, private_key_der: &[u8]) -> Result<(), PkiError> {
        if private_key_der.is_empty() {
            return Err(PkiError::EmptyPrivateKey);
        }
        fs::create_dir_all(&self.directory)?;
        let path = self.path_for(key_id)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            io::Write::write_all(&mut file, private_key_der)?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            io::Write::write_all(&mut file, private_key_der)?;
            file.sync_all()?;
        }
        Ok(())
    }

    fn load_private_key(&self, key_id: &str) -> Result<Vec<u8>, PkiError> {
        let path = self.path_for(key_id)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = fs::metadata(&path)?.permissions().mode() & 0o777;
            if permissions & !0o600 != 0 {
                return Err(PkiError::InsecureKeyPermissions);
            }
        }
        let key = fs::read(path)?;
        if key.is_empty() {
            return Err(PkiError::EmptyPrivateKey);
        }
        Ok(key)
    }
}

/// Unix timestamp helper for journal-backed invite expiry.
pub fn unix_time_now() -> Result<u64, PkiError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

/// Compare an enrolled node fingerprint with the raw DER client certificate from TLS.
pub fn presented_client_matches(enrolled: &CertificateFingerprint, presented_der: &[u8]) -> bool {
    enrolled.matches_presented_client_certificate(presented_der)
}

/// Derive the SHA-256 hash stored by the hub journal for an invite code.
pub fn hash_invite_code(code: &[u8]) -> [u8; 32] {
    Sha256::digest(code).into()
}

/// Constant-time comparison for journal-stored and submitted invite-code hashes.
pub fn invite_code_hash_matches(expected_hash: &[u8; 32], submitted_code: &[u8]) -> bool {
    constant_time_eq(expected_hash, &hash_invite_code(submitted_code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::{
        ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection,
        pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
        server::WebPkiClientVerifier,
    };

    #[test]
    fn invite_round_trip_with_dotted_address_and_spend_checks() {
        let ca = HubCa::generate().expect("CA");
        let invite = Invite::new(
            "https://tagteam-hub.tailnet.example:443",
            ca.fingerprint(),
            [0x5a; 32],
        )
        .expect("invite");
        let decoded = Invite::decode(&invite.encode()).expect("round trip");
        assert_eq!(decoded, invite);
        decoded
            .validate_spend(&[0x5a; 32], 10, 20, false)
            .expect("valid spend");
        assert!(matches!(
            decoded.validate_spend(&[0; 32], 10, 20, false),
            Err(PkiError::InviteCodeMismatch)
        ));
        assert!(matches!(
            decoded.validate_spend(&[0x5a; 32], 20, 20, false),
            Err(PkiError::InviteExpired)
        ));
        assert!(matches!(
            decoded.validate_spend(&[0x5a; 32], 10, 20, true),
            Err(PkiError::InviteReused)
        ));
    }

    #[test]
    fn invite_rejects_malformed_components() {
        let fingerprint = "a".repeat(64);
        let code = BASE32_NOPAD.encode(&[7; 32]);
        for bad in [
            format!("xx.YQ.{fingerprint}.{code}"),
            format!("tt1.YQ==.{fingerprint}.{code}"),
            format!("tt1.YQ.{}.{}", "g".repeat(64), code),
            format!("tt1.YQ.{fingerprint}.{}", "A".repeat(51)),
            format!("tt1.YQ.{fingerprint}.{code}.extra"),
            format!("tt1.!!!!.{fingerprint}.{code}"),
        ] {
            assert!(
                matches!(Invite::decode(&bad), Err(PkiError::MalformedInvite(_))),
                "accepted {bad}"
            );
        }
    }

    #[test]
    fn ca_server_and_client_certificates_validate_and_csr_is_signed() {
        let ca = HubCa::generate().expect("CA");
        let server = ca.issue_server_certificate().expect("server cert");
        let node_request = NodeKeyRequest::generate("node-1").expect("CSR");
        let client_certificate = ca.sign_node_csr(&node_request.csr_der).expect("signed CSR");
        ca.verify_issued_certificate(&client_certificate)
            .expect("client chain");
        let other_ca = HubCa::generate().expect("other CA");
        assert!(matches!(
            other_ca.verify_issued_certificate(&client_certificate),
            Err(PkiError::WrongCa)
        ));

        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca.certificate_der().to_vec()))
            .expect("root");
        let client_config = ClientConfig::builder()
            .with_root_certificates(roots.clone())
            .with_client_auth_cert(
                vec![CertificateDer::from(client_certificate.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(node_request.private_key_der)),
            )
            .expect("client config");
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .expect("client verifier");
        let server_config = ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![CertificateDer::from(server.certificate_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(server.private_key_der)),
            )
            .expect("server config");
        let name = ServerName::try_from(SERVICE_DNS_NAME).expect("server name");
        let mut client = ClientConnection::new(Arc::new(client_config), name).expect("client");
        let mut server = ServerConnection::new(Arc::new(server_config)).expect("server");
        while client.is_handshaking() || server.is_handshaking() {
            if client.wants_write() {
                let mut bytes = Vec::new();
                client.write_tls(&mut bytes).expect("client write");
                server.read_tls(&mut bytes.as_slice()).expect("server read");
                server
                    .process_new_packets()
                    .expect("server validates client and CA");
            }
            if server.wants_write() {
                let mut bytes = Vec::new();
                server.write_tls(&mut bytes).expect("server write");
                client.read_tls(&mut bytes.as_slice()).expect("client read");
                client
                    .process_new_packets()
                    .expect("client validates server and CA");
            }
        }
    }

    #[test]
    fn pinned_hub_verifier_checks_root_dns_and_retains_the_verified_root() {
        use rustls::{
            client::danger::ServerCertVerifier,
            pki_types::{ServerName, UnixTime},
        };

        let ca = HubCa::generate().expect("CA");
        let server = ca.issue_server_certificate().expect("server cert");
        let verifier = PinnedHubVerifier::new(ca.fingerprint()).expect("pinned verifier");
        let leaf = rustls::pki_types::CertificateDer::from(server.certificate_der);
        let root = rustls::pki_types::CertificateDer::from(ca.certificate_der().to_vec());
        let name = ServerName::try_from(SERVICE_DNS_NAME).expect("server DNS name");

        assert!(verifier.root_certificate_der().is_none());
        verifier
            .verify_server_cert(&leaf, &[root], &name, &[], UnixTime::now())
            .expect("pinned server chain and DNS name");
        assert_eq!(
            verifier.root_certificate_der().as_deref(),
            Some(ca.certificate_der())
        );
    }

    #[test]
    fn pinned_hub_verifier_rejects_wrong_root_missing_root_and_wrong_dns() {
        use rustls::{
            client::danger::ServerCertVerifier,
            pki_types::{ServerName, UnixTime},
        };

        let ca = HubCa::generate().expect("CA");
        let other_ca = HubCa::generate().expect("other CA");
        let server = ca.issue_server_certificate().expect("server cert");
        let verifier = PinnedHubVerifier::new(other_ca.fingerprint()).expect("pinned verifier");
        let leaf = rustls::pki_types::CertificateDer::from(server.certificate_der);
        let root = rustls::pki_types::CertificateDer::from(ca.certificate_der().to_vec());
        let name = ServerName::try_from(SERVICE_DNS_NAME).expect("server DNS name");
        let wrong_name = ServerName::try_from("other-service").expect("other DNS name");
        let now = UnixTime::now();

        assert!(
            verifier
                .verify_server_cert(&leaf, std::slice::from_ref(&root), &name, &[], now)
                .is_err()
        );
        assert!(
            verifier
                .verify_server_cert(&leaf, &[], &name, &[], now)
                .is_err()
        );
        assert!(
            verifier
                .verify_server_cert(&leaf, &[root], &wrong_name, &[], now)
                .is_err()
        );
        assert!(verifier.root_certificate_der().is_none());
    }

    #[test]
    fn node_certificate_verification_checks_pinned_ca_and_client_auth_chain() {
        let ca = HubCa::generate().expect("CA");
        let other_ca = HubCa::generate().expect("other CA");
        let node_key = NodeKeyRequest::generate("node-1").expect("node CSR");
        let node_certificate = ca
            .sign_node_csr(&node_key.csr_der)
            .expect("node certificate");
        let server_certificate = ca.issue_server_certificate().expect("server certificate");

        verify_node_certificate(&node_certificate, ca.certificate_der(), ca.fingerprint())
            .expect("pinned node client certificate");
        assert!(
            verify_node_certificate(
                &server_certificate.certificate_der,
                ca.certificate_der(),
                ca.fingerprint()
            )
            .is_err()
        );
        assert!(matches!(
            verify_node_certificate(
                &node_certificate,
                other_ca.certificate_der(),
                ca.fingerprint()
            ),
            Err(PkiError::WrongCa)
        ));
        assert!(matches!(
            verify_node_certificate(
                &node_certificate,
                ca.certificate_der(),
                other_ca.fingerprint()
            ),
            Err(PkiError::WrongCa)
        ));
    }

    #[test]
    fn loopback_tls_rejects_a_hub_with_a_different_pinned_ca() {
        use rustls::{
            ClientConfig, ClientConnection, ServerConfig, ServerConnection,
            pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
        };
        use std::{
            net::{TcpListener, TcpStream},
            thread,
        };

        let server_ca = HubCa::generate().expect("server CA");
        let pinned_ca = HubCa::generate().expect("pinned CA");
        let identity = server_ca.issue_server_certificate().expect("server cert");
        let server_config = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![
                        CertificateDer::from(identity.certificate_der),
                        CertificateDer::from(server_ca.certificate_der().to_vec()),
                    ],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.private_key_der)),
                )
                .expect("server TLS config"),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let server_thread = thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept loopback client");
            let mut connection = ServerConnection::new(server_config).expect("server connection");
            while connection.is_handshaking() {
                if connection.complete_io(&mut socket).is_err() {
                    break;
                }
            }
        });

        let verifier = PinnedHubVerifier::new(pinned_ca.fingerprint()).expect("pinned verifier");
        let client_config = ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth();
        let mut socket = TcpStream::connect(address).expect("connect loopback server");
        let name = ServerName::try_from(SERVICE_DNS_NAME).expect("server DNS name");
        let mut connection =
            ClientConnection::new(Arc::new(client_config), name).expect("client connection");
        let mut handshake_error = None;
        while connection.is_handshaking() {
            if let Err(error) = connection.complete_io(&mut socket) {
                handshake_error = Some(error);
                break;
            }
        }
        server_thread.join().expect("server thread");
        assert!(handshake_error.is_some(), "wrong pinned CA completed TLS");
    }

    #[test]
    fn fingerprints_match_only_the_same_certificate() {
        let ca = HubCa::generate().expect("CA");
        let certificate = ca.issue_server_certificate().expect("server cert");
        let fingerprint = CertificateFingerprint::from_certificate(&certificate.certificate_der);
        assert!(presented_client_matches(
            &fingerprint,
            &certificate.certificate_der
        ));
        assert!(!presented_client_matches(
            &fingerprint,
            ca.certificate_der()
        ));
        assert_eq!(fingerprint.to_hex().len(), 64);
    }

    #[cfg(unix)]
    #[test]
    fn file_keystore_uses_and_requires_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("tempdir");
        let store = FileKeyStore::new(directory.path());
        store
            .store_private_key("node-1", &[1, 2, 3])
            .expect("store");
        let path = directory.path().join("node-1.key");
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(store.load_private_key("node-1").expect("load"), [1, 2, 3]);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("broaden mode");
        assert!(matches!(
            store.load_private_key("node-1"),
            Err(PkiError::InsecureKeyPermissions)
        ));
    }
}
