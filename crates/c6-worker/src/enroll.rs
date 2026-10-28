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
    enroll_via(token, None).await
}

/// Enrolls, dialing `head` (`host` or `host:port`) instead of the token's
/// address. The certificate is still checked against the token's host name,
/// which the head always lists, and the CA is pinned by the token's hash.
pub async fn enroll_via(token: &str, head: Option<&str>) -> anyhow::Result<Identity> {
    let token = EnrollmentToken::decode(token)?;
    let (url, tls_domain) = match head {
        Some(h) => override_head(&token.url, h)?,
        None => (token.url.clone(), None),
    };
    let ca_pem = fetch_pinned_ca(&url, &token.ca_sha256).await?;

    let mut tls = ClientTlsConfig::new().ca_certificate(Certificate::from_pem(&ca_pem));
    if let Some(d) = &tls_domain {
        tls = tls.domain_name(d.clone());
    }
    let channel = Endpoint::from_shared(url.clone())?
        .tls_config(tls)?
        .connect_timeout(Duration::from_secs(10))
        .connect()
        .await
        .with_context(|| format!("connecting to head at {url}"))?;

    let key = rcgen::KeyPair::generate()?;
    let csr = rcgen::CertificateParams::default().serialize_request(&key)?.pem()?;
    let resp = EnrollmentClient::new(channel)
        .enroll(EnrollRequest {
            secret: token.secret,
            csr_pem: csr,
            hostname: crate::hostname(),
            os: c6_core::host_os().into(),
            arch: c6_core::host_arch().into(),
            version: c6_core::build().into(),
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
            head_url: url,
            tls_domain,
        },
        key_pem: key.serialize_pem(),
        cert_pem: resp.cert_pem,
        ca_pem,
    })
}

/// The URL to dial for `--head host[:port]` (the token's port when none is
/// given) and the token's host, to check the certificate against.
fn override_head(token_url: &str, head: &str) -> anyhow::Result<(String, Option<String>)> {
    let uri: tonic::codegen::http::Uri = token_url.parse().context("bad head URL in token")?;
    let domain = uri.host().context("head URL has no host")?.trim_start_matches('[').trim_end_matches(']').to_owned();
    let given: tonic::codegen::http::uri::Authority = head.parse().with_context(|| format!("bad --head {head:?}; use host or host:port"))?;
    let port = given.port_u16().or_else(|| uri.port_u16());
    let host = given.host();
    let url = match port {
        Some(p) => format!("https://{host}:{p}"),
        None => format!("https://{host}"),
    };
    Ok((url, Some(domain)))
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

#[cfg(test)]
mod tests {
    use super::override_head;

    #[test]
    fn override_keeps_token_port_and_name() {
        let (url, domain) = override_head("https://10.0.0.5:7701", "203.0.113.9").unwrap();
        assert_eq!(url, "https://203.0.113.9:7701");
        assert_eq!(domain.as_deref(), Some("10.0.0.5"));
        let (url, _) = override_head("https://head.lan:7701", "head.example.com:443").unwrap();
        assert_eq!(url, "https://head.example.com:443");
    }
}
