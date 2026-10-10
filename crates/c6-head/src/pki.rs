//! The head's private CA: issues its own server certificate and worker client
//! certificates.
//!
//! The CA key and certificate persist in `<data>/pki/`. The server certificate
//! is re-issued on every start so its SANs track the current bind addresses.

use std::net::IpAddr;
use std::path::Path;

use anyhow::Context;
use c6_core::WorkerId;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use time::{Duration, OffsetDateTime};
use x509_parser::prelude::{FromDer, GeneralName, X509Certificate};

pub struct Pki {
    ca_key: KeyPair,
    ca_cert_pem: String,
    ca_sha256: String,
    /// Server certificate followed by the CA certificate, so clients can see
    /// the CA during enrollment pinning.
    server_chain_pem: String,
    server_key_pem: String,
}

pub struct IssuedCert {
    pub cert_pem: String,
    pub serial_hex: String,
}

impl Pki {
    /// Loads the CA from `dir`, creating it on first run, and issues a fresh
    /// server certificate covering `ips` and `dns_names`.
    pub fn load_or_create(dir: &Path, ips: &[IpAddr], dns_names: &[String]) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let key_path = dir.join("ca.key");
        let cert_path = dir.join("ca.crt");

        let (ca_key, ca_cert_pem) = if key_path.exists() && cert_path.exists() {
            let key = KeyPair::from_pem(&std::fs::read_to_string(&key_path)?).context("parsing CA key")?;
            (key, std::fs::read_to_string(&cert_path)?)
        } else {
            let key = KeyPair::generate()?;
            let cert = ca_params().self_signed(&key)?;
            write_private(&key_path, &key.serialize_pem())?;
            std::fs::write(&cert_path, cert.pem())?;
            (key, cert.pem())
        };

        let ca_der = pem_to_der(&ca_cert_pem)?;
        let ca_sha256 = c6_core::sha256_hex(&ca_der);

        let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, &ca_key).context("loading CA certificate")?;
        let server_key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params.distinguished_name = dn("carbon6 head");
        params.subject_alt_names = ips.iter().map(|ip| SanType::IpAddress(*ip)).collect();
        for name in dns_names {
            params.subject_alt_names.push(SanType::DnsName(name.clone().try_into()?));
        }
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        set_validity(&mut params, Duration::days(397));
        params.serial_number = Some(random_serial());
        let server_cert = params.signed_by(&server_key, &issuer)?;

        Ok(Self {
            server_chain_pem: format!("{}{}", server_cert.pem(), ca_cert_pem),
            server_key_pem: server_key.serialize_pem(),
            ca_key,
            ca_cert_pem,
            ca_sha256,
        })
    }

    pub fn ca_cert_pem(&self) -> &str {
        &self.ca_cert_pem
    }

    pub fn ca_sha256(&self) -> &str {
        &self.ca_sha256
    }

    pub fn server_identity(&self) -> tonic::transport::Identity {
        tonic::transport::Identity::from_pem(&self.server_chain_pem, &self.server_key_pem)
    }

    /// Issues a client certificate for `worker_id` from a worker's CSR.
    ///
    /// Only the CSR's public key is used. Names, usages and constraints come
    /// from us, never from the request.
    pub fn issue_worker_cert(&self, csr_pem: &str, worker_id: WorkerId) -> anyhow::Result<IssuedCert> {
        let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("invalid CSR")?;
        let issuer = Issuer::from_ca_cert_pem(&self.ca_cert_pem, &self.ca_key)?;

        let serial = random_serial();
        let serial_hex = hex::encode(serial.as_ref());
        let mut params = CertificateParams::default();
        params.distinguished_name = dn(&format!("c6w {worker_id}"));
        params.subject_alt_names = vec![SanType::URI(worker_id.uri().try_into()?)];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.use_authority_key_identifier_extension = true;
        set_validity(&mut params, Duration::days(3650));
        params.serial_number = Some(serial);

        let cert = params.signed_by(&csr.public_key, &issuer)?;
        Ok(IssuedCert { cert_pem: cert.pem(), serial_hex })
    }
}

/// Worker identity from a verified client certificate: its `c6w://<id>` URI
/// SAN, plus the serial (hex) to match against the enrolled one.
pub fn worker_identity(cert_der: &[u8]) -> Option<(WorkerId, String)> {
    let (_, cert) = X509Certificate::from_der(cert_der).ok()?;
    let serial_hex = hex::encode(cert.raw_serial());
    let san = cert.subject_alternative_name().ok()??;
    san.value.general_names.iter().find_map(|name| match name {
        GeneralName::URI(uri) => WorkerId::from_uri(uri).map(|id| (id, serial_hex.clone())),
        _ => None,
    })
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = dn("carbon6 head CA");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    set_validity(&mut params, Duration::days(3650 * 3));
    params.serial_number = Some(random_serial());
    params
}

pub(crate) fn dn(common_name: &str) -> DistinguishedName {
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, common_name);
    dn
}

pub(crate) fn set_validity(params: &mut CertificateParams, lifetime: Duration) {
    let now = OffsetDateTime::now_utc();
    // Backdate to tolerate clock skew between nodes.
    params.not_before = now - Duration::hours(1);
    params.not_after = now + lifetime;
}

pub(crate) fn random_serial() -> SerialNumber {
    let mut bytes: [u8; 16] = rand::random();
    bytes[0] &= 0x7f; // keep it positive
    SerialNumber::from_slice(&bytes)
}

pub(crate) fn pem_to_der(pem: &str) -> anyhow::Result<Vec<u8>> {
    Ok(::pem::parse(pem).context("parsing PEM")?.into_contents())
}

pub(crate) fn write_private(path: &Path, contents: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issues_worker_cert_with_identity() {
        let dir = tempfile::tempdir().unwrap();
        let pki = Pki::load_or_create(dir.path(), &["127.0.0.1".parse().unwrap()], &["localhost".into()]).unwrap();

        // A malicious CSR asking to be a CA for some other identity.
        let worker_key = KeyPair::generate().unwrap();
        let mut req = CertificateParams::new(vec!["evil.example".into()]).unwrap();
        req.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let csr = req.serialize_request(&worker_key).unwrap().pem().unwrap();

        let id = WorkerId::new();
        let issued = pki.issue_worker_cert(&csr, id).unwrap();
        let der = pem_to_der(&issued.cert_pem).unwrap();
        let (got_id, serial) = worker_identity(&der).unwrap();
        assert_eq!(got_id, id);
        assert_eq!(serial, issued.serial_hex);

        let (_, cert) = X509Certificate::from_der(&der).unwrap();
        assert!(!cert.is_ca());

        // Reloading keeps the same CA.
        let again = Pki::load_or_create(dir.path(), &[], &[]).unwrap();
        assert_eq!(again.ca_sha256(), pki.ca_sha256());
    }
}
