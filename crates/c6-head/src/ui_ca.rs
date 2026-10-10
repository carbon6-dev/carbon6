//! A private certificate authority for the web UI's HTTPS.
//!
//! Publicly trusted certificates (Let's Encrypt, Tailscale's) are recorded
//! in Certificate Transparency logs, which would publish the head's names.
//! Instead the head issues its own, and each device trusts this CA once.
//!
//! The CA is separate from the worker CA and name-constrained: it can only
//! vouch for the head's own DNS names (fixed when it is created) and private
//! IP ranges, so trusting it does not let a leaked key impersonate any other
//! site. Its leaf certificate is re-issued on every start.

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::Context;
use rcgen::{
    BasicConstraints, CertificateParams, CidrSubnet, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    NameConstraints, SanType,
};
use time::Duration;

use crate::pki::{dn, pem_to_der, random_serial, set_validity, write_private};

/// IP ranges the CA may vouch for: loopback, RFC 1918, CGNAT (tailnet), ULA.
const PRIVATE_NETS: [&str; 7] = ["127.0.0.0/8", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "::1/128", "fc00::/7"];

pub struct UiCa {
    dir: PathBuf,
    key: KeyPair,
    cert_pem: String,
    cert_der: Vec<u8>,
    /// DNS names the CA may vouch for (each also covers its subdomains).
    names: Vec<String>,
}

impl UiCa {
    /// Loads the CA from `dir`, creating it for `names` on first use.
    pub fn load_or_create(dir: &Path, names: &[String]) -> anyhow::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let (key_path, cert_path, names_path) = (dir.join("ui-ca.key"), dir.join("ui-ca.crt"), dir.join("ui-ca.names"));
        let (key, cert_pem, names) = if key_path.exists() && cert_path.exists() && names_path.exists() {
            let key = KeyPair::from_pem(&std::fs::read_to_string(&key_path)?).context("parsing UI CA key")?;
            let names: Vec<String> = serde_json::from_str(&std::fs::read_to_string(&names_path)?).context("parsing ui-ca.names")?;
            (key, std::fs::read_to_string(&cert_path)?, names)
        } else {
            let names: Vec<String> = dns_only(names);
            anyhow::ensure!(!names.is_empty(), "no host names for the web UI certificate authority");
            let key = KeyPair::generate()?;
            let mut params = CertificateParams::default();
            let host = names.iter().find(|n| *n != "localhost").unwrap_or(&names[0]);
            params.distinguished_name = dn(&format!("carbon6 head {host}"));
            params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            params.name_constraints = Some(NameConstraints {
                permitted_subtrees: names
                    .iter()
                    .map(|n| GeneralSubtree::DnsName(n.clone()))
                    .chain(PRIVATE_NETS.iter().map(|c| GeneralSubtree::IpAddress(c.parse::<CidrSubnet>().unwrap())))
                    .collect(),
                excluded_subtrees: Vec::new(),
            });
            set_validity(&mut params, Duration::days(3650));
            params.serial_number = Some(random_serial());
            let cert = params.self_signed(&key)?;
            write_private(&key_path, &key.serialize_pem())?;
            std::fs::write(&cert_path, cert.pem())?;
            std::fs::write(&names_path, serde_json::to_string(&names)?)?;
            tracing::info!(?names, "created the web UI certificate authority");
            (key, cert.pem(), names)
        };
        let cert_der = pem_to_der(&cert_pem)?;
        Ok(Self { dir: dir.to_owned(), key, cert_pem, cert_der, names })
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    pub fn cert_path(&self) -> PathBuf {
        self.dir.join("ui-ca.crt")
    }

    /// Whether the CA may vouch for `name`.
    fn permits(&self, name: &str) -> bool {
        self.names.iter().any(|p| name == p || name.ends_with(&format!(".{p}")))
    }

    /// A fresh server certificate (chain PEM, key PEM) for the names and IPs
    /// the CA may vouch for. Names it may not are skipped with a warning.
    pub fn issue(&self, names: &[String], ips: &[IpAddr]) -> anyhow::Result<(String, String)> {
        let mut params = CertificateParams::default();
        params.distinguished_name = dn("carbon6 head web UI");
        for n in dns_only(names) {
            if self.permits(&n) {
                params.subject_alt_names.push(SanType::DnsName(n.try_into()?));
            } else {
                tracing::warn!(
                    name = %n,
                    "the web UI certificate authority does not cover this name; to add it, delete ui-ca.* in {} and trust the new CA on your devices",
                    self.dir.display()
                );
            }
        }
        params.subject_alt_names.extend(ips.iter().filter(|ip| crate::config::is_private(ip)).map(|ip| SanType::IpAddress(*ip)));
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        // Apple platforms reject server certificates valid for over 825 days.
        set_validity(&mut params, Duration::days(397));
        params.serial_number = Some(random_serial());
        let key = KeyPair::generate()?;
        let issuer = Issuer::from_ca_cert_pem(&self.cert_pem, &self.key)?;
        let cert = params.signed_by(&key, &issuer)?;
        Ok((format!("{}{}", cert.pem(), self.cert_pem), key.serialize_pem()))
    }

    /// An Apple configuration profile that installs the CA (iPhone, iPad,
    /// Mac). Identifiers derive from the CA, so reinstalling replaces it.
    pub fn mobileconfig(&self) -> String {
        use base64::Engine;
        let hash = c6_core::sha256_hex(&self.cert_der);
        let uuid = |salt: &str| {
            let h = c6_core::sha256_hex(format!("{salt}{hash}").as_bytes());
            format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]).to_uppercase()
        };
        let host = self.names.iter().find(|n| *n != "localhost").cloned().unwrap_or_else(|| "carbon6".into());
        let der = base64::engine::general_purpose::STANDARD.encode(&self.cert_der);
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>PayloadContent</key>
  <array>
    <dict>
      <key>PayloadCertificateFileName</key><string>carbon6-ca.cer</string>
      <key>PayloadContent</key><data>{der}</data>
      <key>PayloadDescription</key><string>Lets this device trust the carbon6 head at {host}. It can only vouch for that head's names and private addresses.</string>
      <key>PayloadDisplayName</key><string>carbon6 head CA ({host})</string>
      <key>PayloadIdentifier</key><string>dev.carbon6.ca.{payload}</string>
      <key>PayloadType</key><string>com.apple.security.root</string>
      <key>PayloadUUID</key><string>{payload}</string>
      <key>PayloadVersion</key><integer>1</integer>
    </dict>
  </array>
  <key>PayloadDisplayName</key><string>carbon6 head ({host})</string>
  <key>PayloadIdentifier</key><string>dev.carbon6.profile.{profile}</string>
  <key>PayloadRemovalDisallowed</key><false/>
  <key>PayloadType</key><string>Configuration</string>
  <key>PayloadUUID</key><string>{profile}</string>
  <key>PayloadVersion</key><integer>1</integer>
</dict>
</plist>
"#,
            host = xml_escape(&host),
            payload = uuid("payload"),
            profile = uuid("profile"),
        )
    }
}

/// Lower-cased DNS names from `names`, without IP literals.
fn dns_only(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = names
        .iter()
        .map(|n| n.trim_matches(['[', ']']).to_ascii_lowercase())
        .filter(|n| !n.is_empty() && n.parse::<IpAddr>().is_err())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constrained_to_its_names() {
        let dir = tempfile::tempdir().unwrap();
        let ca = UiCa::load_or_create(dir.path(), &["localhost".into(), "Head.example.ts.net".into(), "127.0.0.1".into()]).unwrap();
        let (chain, _) = ca.issue(&["head.example.ts.net".into(), "evil.example".into()], &["100.64.1.2".parse().unwrap()]).unwrap();
        let leaf = pem_to_der(&chain).unwrap();
        let (_, cert) = x509_parser::parse_x509_certificate(&leaf).unwrap();
        let sans: Vec<String> =
            cert.subject_alternative_name().unwrap().unwrap().value.general_names.iter().map(|n| n.to_string()).collect();
        assert!(sans.iter().any(|s| s.contains("head.example.ts.net")), "{sans:?}");
        assert!(!sans.iter().any(|s| s.contains("evil")), "names outside the CA are left out: {sans:?}");

        let (_, ca_cert) = x509_parser::parse_x509_certificate(&ca.cert_der).unwrap();
        let nc = ca_cert.name_constraints().unwrap().unwrap();
        assert!(nc.critical);
        // Reloading keeps the CA and its names.
        let again = UiCa::load_or_create(dir.path(), &["other.example".into()]).unwrap();
        assert_eq!(again.cert_pem(), ca.cert_pem());
        assert!(again.permits("head.example.ts.net") && !again.permits("other.example"));
        assert!(ca.mobileconfig().contains("com.apple.security.root"));
    }
}
