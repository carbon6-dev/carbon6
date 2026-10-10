//! `c6w enroll`: trust the head via the token's CA fingerprint, then trade
//! the one-time secret and a CSR for a client certificate.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use c6_core::enrollment::EnrollmentToken;
use c6_proto::{EnrollRequest, enrollment_client::EnrollmentClient};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio::net::TcpStream;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

use crate::identity::{Identity, IdentityMeta};

pub async fn enroll(token: &str) -> anyhow::Result<Identity> {
    let token = EnrollmentToken::decode(token)?;
    let ca_pem = fetch_pinned_ca(&token.url, &token.ca_sha256).await?;

    let channel = Endpoint::from_shared(token.url.clone())?
        .tls_config(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(&ca_pem)))?
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await
        .with_context(|| format!("connecting to head at {}", token.url))?;

    let key = rcgen::KeyPair::generate()?;
    let csr = rcgen::CertificateParams::default().serialize_request(&key)?.pem()?;
    let resp = EnrollmentClient::new(channel)
        .enroll(EnrollRequest {
            secret: token.secret,
            csr_pem: csr,
            hostname: crate::hostname(),
            os: c6_core::host_os().into(),
            arch: c6_core::host_arch().into(),
            version: c6_core::VERSION.into(),
        })
        .await
        .map_err(|s| anyhow::anyhow!("enrollment rejected: {}", s.message()))?
        .into_inner();

    if pem_der(&resp.ca_pem)? != pem_der(&ca_pem)? {
        bail!("head returned a different CA than the one pinned by the token");
    }
    Ok(Identity {
        meta: IdentityMeta {
            worker_id: resp.worker_id.parse().context("bad worker id from head")?,
            worker_name: resp.worker_name,
            head_url: token.url,
        },
        key_pem: key.serialize_pem(),
        cert_pem: resp.cert_pem,
        ca_pem,
    })
}

/// Completes a TLS handshake without verification only to read the server's
/// chain, and returns the certificate whose SHA-256 matches the token. All
/// real traffic is then verified against that CA.
async fn fetch_pinned_ca(url: &str, ca_sha256: &str) -> anyhow::Result<String> {
    let uri: tonic::codegen::http::Uri = url.parse().context("bad head URL in token")?;
    let host = uri.host().context("head URL has no host")?.trim_start_matches('[').trim_end_matches(']').to_owned();
    let port = uri.port_u16().unwrap_or(443);

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(CaptureOnly(provider)))
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let server_name = ServerName::try_from(host.clone()).context("bad head host")?;

    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host.as_str(), port)))
        .await
        .context("timed out connecting to head")?
        .with_context(|| format!("connecting to {host}:{port}"))?;
    let tls = connector.connect(server_name, tcp).await.context("TLS handshake with head")?;
    let chain = tls.get_ref().1.peer_certificates().context("head sent no certificates")?;

    let ca = chain
        .iter()
        .find(|c| c6_core::sha256_hex(c.as_ref()) == ca_sha256.to_ascii_lowercase())
        .context("head's certificate chain does not contain the CA pinned by the token")?;
    Ok(pem::encode(&pem::Pem::new("CERTIFICATE", ca.as_ref().to_vec())))
}

fn pem_der(s: &str) -> anyhow::Result<Vec<u8>> {
    Ok(pem::parse(s).context("parsing PEM")?.into_contents())
}

/// Accepts any certificate. Used only to read the chain for pinning.
#[derive(Debug)]
struct CaptureOnly(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for CaptureOnly {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
