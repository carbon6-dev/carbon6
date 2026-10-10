//! `c6h.toml` bootstrap configuration and listen-address policy.
//!
//! Everything beyond bootstrap (connections, models, workspaces, ...) lives in
//! the database and is edited through the UI.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeadConfig {
    /// Database, PKI and blobs live here.
    pub data_dir: PathBuf,
    /// Addresses to bind. Empty means every private (RFC 1918, CGNAT/tailnet,
    /// IPv6 ULA) and loopback interface address.
    pub bind: Vec<IpAddr>,
    pub ui_port: u16,
    pub worker_port: u16,
    /// Host or IP that workers should dial; goes into enrollment tokens.
    /// Defaults to a tailnet address if one exists, else a private one.
    pub advertise_host: Option<String>,
    /// Extra DNS names for the head's server certificate (e.g. MagicDNS names).
    pub extra_names: Vec<String>,
    /// Permit binding non-private addresses. The head has no login; only set
    /// this if something else controls access.
    pub allow_public_bind: bool,
    /// OTLP/HTTP endpoint for traces (e.g. `http://127.0.0.1:4318`); falls
    /// back to `OTEL_EXPORTER_OTLP_ENDPOINT`.
    pub otlp_endpoint: Option<String>,
    pub otlp_headers: std::collections::HashMap<String, String>,
}

impl Default for HeadConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            bind: Vec::new(),
            ui_port: 7700,
            worker_port: 7701,
            advertise_host: None,
            extra_names: Vec::new(),
            allow_public_bind: false,
            otlp_endpoint: None,
            otlp_headers: Default::default(),
        }
    }
}

fn default_data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("c6h")
}

pub fn default_config_path() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("c6h").join("c6h.toml")
}

impl HeadConfig {
    /// Loads `path`, or defaults if it does not exist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// The IPs to bind, after applying the private-only policy.
    pub fn bind_ips(&self) -> anyhow::Result<Vec<IpAddr>> {
        let ips = if self.bind.is_empty() { private_interface_ips()? } else { self.bind.clone() };
        if !self.allow_public_bind
            && let Some(ip) = ips.iter().find(|ip| !is_private(ip))
        {
            bail!("refusing to bind non-private address {ip}; set allow_public_bind = true to override");
        }
        if ips.is_empty() {
            bail!("no bind addresses");
        }
        Ok(ips)
    }

    pub fn advertise_host(&self, bound: &[SocketAddr]) -> String {
        if let Some(h) = &self.advertise_host {
            return h.clone();
        }
        let pick = |pred: fn(&IpAddr) -> bool| bound.iter().map(|a| a.ip()).find(pred);
        pick(is_cgnat)
            .or_else(|| pick(|ip| is_private(ip) && !ip.is_loopback()))
            .or_else(|| bound.first().map(|a| a.ip()))
            .map(|ip| match ip {
                IpAddr::V6(v6) => format!("[{v6}]"),
                v4 => v4.to_string(),
            })
            .unwrap_or_else(|| "127.0.0.1".into())
    }
}

fn private_interface_ips() -> anyhow::Result<Vec<IpAddr>> {
    let mut ips: Vec<IpAddr> =
        if_addrs::get_if_addrs().context("listing network interfaces")?.into_iter().map(|i| i.ip()).filter(is_private).collect();
    ips.sort();
    ips.dedup();
    Ok(ips)
}

/// Loopback, RFC 1918, CGNAT (Tailscale's 100.64.0.0/10) or IPv6 ULA.
/// IPv6 link-local is excluded because it needs a scope id to bind.
pub fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || is_cgnat_v4(v4),
        IpAddr::V6(v6) => v6.is_loopback() || is_ula(v6),
    }
}

fn is_cgnat(ip: &IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v4) if is_cgnat_v4(v4))
}

fn is_cgnat_v4(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0b1100_0000) == 64
}

fn is_ula(ip: &Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        for ok in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "100.64.0.1", "100.127.255.1", "::1", "fd7a:115c:a1e0::1"] {
            assert!(is_private(&ok.parse().unwrap()), "{ok}");
        }
        for bad in ["0.0.0.0", "8.8.8.8", "100.128.0.1", "::", "2001:db8::1", "fe80::1"] {
            assert!(!is_private(&bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn rejects_public_bind() {
        let cfg = HeadConfig { bind: vec!["0.0.0.0".parse().unwrap()], ..Default::default() };
        assert!(cfg.bind_ips().is_err());
        let cfg = HeadConfig { allow_public_bind: true, ..cfg };
        assert!(cfg.bind_ips().is_ok());
    }

    #[test]
    fn prefers_tailnet_for_advertise() {
        let cfg = HeadConfig::default();
        let bound: Vec<SocketAddr> =
            vec!["127.0.0.1:7701".parse().unwrap(), "192.168.1.5:7701".parse().unwrap(), "100.100.1.2:7701".parse().unwrap()];
        assert_eq!(cfg.advertise_host(&bound), "100.100.1.2");
    }
}
