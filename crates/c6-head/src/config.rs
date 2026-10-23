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
    /// Permit binding non-private addresses (all interfaces, or a public
    /// one). Sign-in still protects the UI, but use HTTPS (`ui_tls_*`) there.
    pub allow_public_bind: bool,
    /// OTLP/HTTP endpoint for traces (e.g. `http://127.0.0.1:4318`); falls
    /// back to `OTEL_EXPORTER_OTLP_ENDPOINT`.
    pub otlp_endpoint: Option<String>,
    pub otlp_headers: std::collections::HashMap<String, String>,
    /// Require sign-in (passkey or login link) for the web UI and API.
    pub auth: bool,
    /// HTTPS for the web UI from these PEM files (certificate chain, key).
    pub ui_tls_cert: Option<PathBuf>,
    pub ui_tls_key: Option<PathBuf>,
    /// HTTPS for the web UI with a certificate from the head's own,
    /// name-constrained CA (trust `/ca.pem` once per device). Keeps the
    /// head's names out of public Certificate Transparency logs.
    pub ui_tls_head_ca: bool,
    /// Where a memex that ran beside the head (`c6m`) kept its state on this
    /// machine, to move its knowledge in (docs/knowledge.md §8). Loading a
    /// config file fills in c6m's usual folders; none: don't look.
    pub memex_dirs: Option<Vec<PathBuf>>,
    /// Where releases are looked for (GitHub's API for the repository;
    /// docs/updates.md).
    pub update_source: String,
    /// Release keys (hex) instead of the ones built in (tests).
    pub update_keys: Vec<String>,
    /// The binary updates replace, instead of this one (tests).
    pub update_exe: Option<PathBuf>,
    /// How often to look for a release.
    pub update_check_secs: u64,
    /// The version updates take this head to run (tests).
    pub update_as_version: Option<String>,
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
            auth: true,
            ui_tls_cert: None,
            ui_tls_key: None,
            ui_tls_head_ca: false,
            memex_dirs: None,
            update_source: "https://api.github.com/repos/carbon6-dev/carbon6".into(),
            update_keys: Vec::new(),
            update_exe: None,
            update_check_secs: 15 * 60,
            update_as_version: None,
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
    /// Loads `path`, or defaults if it does not exist, then applies
    /// environment overrides: `C6H_ADVERTISE_HOST` and `C6H_EXTRA_NAMES`
    /// (comma-separated). Containers use these.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut cfg: Self = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        // A head run from its config looks where c6m kept its state.
        if cfg.memex_dirs.is_none() {
            let mut dirs = Vec::new();
            if let Some(d) = dirs::data_local_dir() {
                dirs.push(d.join("c6m"));
            }
            dirs.push(PathBuf::from("/var/lib/c6m"));
            cfg.memex_dirs = Some(dirs);
        }
        if let Some(h) = std::env::var("C6H_ADVERTISE_HOST").ok().filter(|h| !h.is_empty()) {
            cfg.advertise_host = Some(h);
        }
        if let Ok(names) = std::env::var("C6H_EXTRA_NAMES") {
            cfg.extra_names.extend(names.split(',').map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned));
        }
        Ok(cfg)
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

/// The addresses the head can be reached at: listen addresses, with an
/// unspecified one (`0.0.0.0`, `::`) replaced by every interface address of
/// its family. IPv6 link-local addresses are left out (they need a scope).
pub fn reachable_addrs(listen: &[SocketAddr]) -> anyhow::Result<Vec<SocketAddr>> {
    let mut out = Vec::new();
    for a in listen {
        if !a.ip().is_unspecified() {
            out.push(*a);
            continue;
        }
        for i in if_addrs::get_if_addrs().context("listing network interfaces")? {
            let ip = i.ip();
            let link_local_v6 = matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80);
            if ip.is_ipv4() == a.is_ipv4() && !link_local_v6 {
                out.push(SocketAddr::new(ip, a.port()));
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// This machine's Tailscale MagicDNS name and its short form (e.g.
/// `darkstar.example.ts.net` and `darkstar`), so the head answers to them
/// without configuration. Empty when Tailscale is not installed or not up.
pub async fn tailnet_names() -> Vec<String> {
    let Some(cli) = tailscale_cli().await else { return Vec::new() };
    let run = tokio::process::Command::new(cli).args(["status", "--json"]).kill_on_drop(true).output();
    let Ok(Ok(out)) = tokio::time::timeout(std::time::Duration::from_secs(3), run).await else { return Vec::new() };
    let status = serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap_or_default();
    let Some(full) = status["Self"]["DNSName"].as_str().map(|n| n.trim_end_matches('.')).filter(|n| !n.is_empty()) else {
        return Vec::new();
    };
    let mut names = vec![full.to_ascii_lowercase()];
    names.extend(full.split('.').next().filter(|s| *s != full).map(str::to_ascii_lowercase));
    names
}

/// The Tailscale CLI: on the PATH, or inside the macOS app.
pub async fn tailscale_cli() -> Option<&'static str> {
    for cli in ["tailscale", "/Applications/Tailscale.app/Contents/MacOS/Tailscale"] {
        let run = tokio::process::Command::new(cli).arg("version").kill_on_drop(true).output();
        if let Ok(Ok(out)) = tokio::time::timeout(std::time::Duration::from_secs(3), run).await
            && out.status.success()
        {
            return Some(cli);
        }
    }
    None
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

/// Loopback, or a tailnet address (Tailscale's CGNAT range and its IPv6
/// prefix), where traffic is already encrypted.
pub fn is_loopback_or_tailnet(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || is_cgnat_v4(v4),
        IpAddr::V6(v6) => v6.is_loopback() || v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
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
    fn wildcard_expands_to_interfaces() {
        let any: SocketAddr = "0.0.0.0:7700".parse().unwrap();
        let reach = reachable_addrs(&[any]).unwrap();
        assert!(reach.contains(&"127.0.0.1:7700".parse().unwrap()), "{reach:?}");
        assert!(reach.iter().all(|a| a.is_ipv4() && !a.ip().is_unspecified() && a.port() == 7700));
        let lo: SocketAddr = "127.0.0.1:7700".parse().unwrap();
        assert_eq!(reachable_addrs(&[lo]).unwrap(), vec![lo]);
    }

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
