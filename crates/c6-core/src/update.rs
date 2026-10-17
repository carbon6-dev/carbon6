//! Releases carbon6 updates itself to (docs/updates.md): the manifest each
//! GitHub release carries (`release.json`), its signature, and comparing
//! versions.
//!
//! A release is trusted only if `release.json.sig` is an Ed25519 signature
//! of `release.json` by one of [`RELEASE_KEYS`]; every file installed must
//! then have the checksum the manifest gives it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The public halves of carbon6's release keys (hex, raw Ed25519). More
/// than one so a key can be rotated: ship the next one a release early.
pub const RELEASE_KEYS: &[&str] = &["be179101f8177b670fbafb6a7589e035c7e1b253dd7cbb81aa48cdcae3a55b1e"];

/// `release.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub protocol_version: u32,
    /// The oldest version this one updates from on its own.
    #[serde(default)]
    pub upgrade_from: String,
    /// Steps a person must take; a release with any is never applied on its own.
    #[serde(default)]
    pub manual: Option<String>,
    #[serde(default)]
    pub notes: String,
    /// File name → `sha256:<hex>`.
    #[serde(default)]
    pub assets: BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("the release's signature is not by a carbon6 release key")]
    BadSignature,
    #[error("the release manifest doesn't parse: {0}")]
    BadManifest(String),
}

/// Checks `sig` over `manifest` against `keys` (hex), and parses it.
pub fn verify(manifest: &[u8], sig: &[u8], keys: &[impl AsRef<str>]) -> Result<Manifest, Error> {
    let ok = keys
        .iter()
        .filter_map(|k| hex::decode(k.as_ref().trim()).ok())
        .any(|k| ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, k).verify(manifest, sig).is_ok());
    if !ok {
        return Err(Error::BadSignature);
    }
    let m: Manifest = serde_json::from_slice(manifest).map_err(|e| Error::BadManifest(e.to_string()))?;
    if parse(&m.version).is_none() {
        return Err(Error::BadManifest(format!("{:?} is not a version", m.version)));
    }
    Ok(m)
}

/// `sha256:<hex>` of `bytes`, as the manifest writes checksums.
pub fn checksum(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

/// A version's release part: `0.3.4`, `v0.3.4`, and builds between
/// releases (`0.3.4+git…`, brew's `0.3.4.<time>`) are all 0.3.4.
pub fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let v = v.split(['+', '-']).next()?;
    let mut it = v.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

/// Whether `to` is a newer release than `from`.
pub fn newer(to: &str, from: &str) -> bool {
    matches!((parse(to), parse(from)), (Some(t), Some(f)) if t > f)
}

/// How big a step from `from` to `to` is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    Patch,
    Minor,
    Major,
}

pub fn step(from: &str, to: &str) -> Option<Step> {
    let (f, t) = (parse(from)?, parse(to)?);
    Some(if t.0 != f.0 {
        Step::Major
    } else if t.1 != f.1 {
        Step::Minor
    } else {
        Step::Patch
    })
}

/// Whether `from` may update to `m` without a person: no manual steps,
/// and not older than the oldest version it updates from.
pub fn unattended(from: &str, m: &Manifest) -> bool {
    m.manual.as_deref().is_none_or(|s| s.trim().is_empty()) && (m.upgrade_from.is_empty() || !newer(&m.upgrade_from, from))
}

/// The file of release `m` that installs `bin` (c6h, c6w, c6) by `method`
/// on this `os` and `arch` (`std::env::consts`), and its checksum.
pub fn asset(m: &Manifest, bin: &str, method: &str, os: &str, arch: &str) -> Option<(String, String)> {
    let v = &m.version;
    let name = match (method, os) {
        ("deb", "linux") => format!("{bin}_{v}_{}.deb", if arch == "aarch64" { "arm64" } else { "amd64" }),
        ("tarball", "linux") => format!("carbon6-{v}-linux-{}.tar.gz", if arch == "aarch64" { "arm64" } else { "amd64" }),
        ("tarball" | "brew", "macos") => format!("{bin}-{v}-darwin-{}.tar.gz", if arch == "aarch64" { "arm64" } else { "x86_64" }),
        _ => return None,
    };
    m.assets.get(&name).map(|sum| (name, sum.clone()))
}

/// How a binary was installed, which decides how it updates (docs/updates.md §6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// Under Homebrew's Cellar; `brew` is that Homebrew's own.
    Brew { brew: PathBuf, prefix: PathBuf },
    /// From the .deb, in /usr/bin: a root helper installs.
    Deb,
    /// A binary in a folder it can write: replaced in place.
    Tarball,
    /// Built from source (or anything else): never updated.
    Source,
}

impl Method {
    pub fn name(&self) -> &'static str {
        match self {
            Method::Brew { .. } => "brew",
            Method::Deb => "deb",
            Method::Tarball => "tarball",
            Method::Source => "source",
        }
    }
}

/// How `exe` was installed. `dev_build` (a debug build) is `Source`.
pub fn method(exe: &Path, dev_build: bool) -> Method {
    if dev_build {
        return Method::Source;
    }
    let s = exe.to_string_lossy();
    if let Some(i) = s.find("/Cellar/") {
        let prefix = PathBuf::from(&s[..i]);
        return Method::Brew { brew: prefix.join("bin/brew"), prefix };
    }
    if cfg!(target_os = "linux") && exe.starts_with("/usr/bin") {
        return Method::Deb;
    }
    let writable = exe.parent().is_some_and(|d| {
        let probe = d.join(format!(".c6-update-probe-{}", std::process::id()));
        let ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        ok
    });
    if writable { Method::Tarball } else { Method::Source }
}

/// What restarts the process after an install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// Exit with [`RESTART_CODE`]; the service manager starts the new one.
    Exit,
    /// The installer restarts it (the .deb's postinst).
    Installer,
}

/// The exit code that asks the service manager for a restart.
pub const RESTART_CODE: i32 = 75;

/// Where a .deb's daemon leaves an update for its root helper.
pub fn deb_dir(bin: &str) -> PathBuf {
    PathBuf::from(format!("/var/lib/{bin}/update"))
}

/// Installs release `m` of `bin` over `exe`. `asset` is the downloaded file
/// for tarball and deb installs (already checked against the manifest).
pub fn install(
    method: &Method,
    bin: &str,
    exe: &Path,
    m: &Manifest,
    signed: (&[u8], &[u8]),
    asset: Option<(&str, &[u8])>,
) -> Result<Restart, String> {
    match method {
        Method::Source => Err("built from source: update it yourself".into()),
        Method::Brew { brew, .. } => {
            let out = std::process::Command::new(brew)
                .args(["upgrade", &format!("carbon6-dev/carbon6/{bin}")])
                .env("HOMEBREW_NO_INSTALL_CLEANUP", "1")
                .env("HOMEBREW_NO_ENV_HINTS", "1")
                .output()
                .map_err(|e| format!("running brew: {e}"))?;
            if !out.status.success() {
                return Err(format!("brew upgrade: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            let listed = std::process::Command::new(brew).args(["list", "--versions", bin]).output().map_err(|e| e.to_string())?;
            let listed = String::from_utf8_lossy(&listed.stdout);
            if !listed.split_whitespace().any(|v| parse(v) == parse(&m.version)) {
                return Err(format!("brew didn't install {} (it has {})", m.version, listed.trim()));
            }
            Ok(Restart::Exit)
        }
        Method::Deb => {
            let (name, bytes) = asset.ok_or("no .deb for this machine in the release")?;
            let dir = deb_dir(bin);
            let write = |f: &str, b: &[u8]| std::fs::write(dir.join(f), b).map_err(|e| format!("writing {}: {e}", dir.join(f).display()));
            let _ = std::fs::remove_file(dir.join("result"));
            write(name, bytes)?;
            write("release.json", signed.0)?;
            write("release.json.sig", signed.1)?;
            // Last: the root helper's path unit watches for this file.
            write("request", name.as_bytes())?;
            // The helper installs, and the package restarts this process;
            // if it can't, it says why.
            for _ in 0..600 {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if let Ok(r) = std::fs::read_to_string(dir.join("result")) {
                    return if r.starts_with("ok") { Ok(Restart::Installer) } else { Err(r.trim().to_owned()) };
                }
            }
            Err(format!("nothing installed it in 10 minutes: is {bin}-update.path enabled?"))
        }
        Method::Tarball => {
            let (_, bytes) = asset.ok_or("no file for this machine in the release")?;
            let dir = exe.parent().ok_or("the binary has no folder")?;
            let work = dir.join(format!(".c6-update-{}", m.version));
            let _ = std::fs::remove_dir_all(&work);
            std::fs::create_dir_all(&work).map_err(|e| format!("creating {}: {e}", work.display()))?;
            let result = (|| {
                let mut tar = std::process::Command::new("tar")
                    .args(["-xzf", "-", "-C"])
                    .arg(&work)
                    .stdin(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("running tar: {e}"))?;
                std::io::Write::write_all(tar.stdin.as_mut().ok_or("tar's stdin")?, bytes).map_err(|e| format!("unpacking: {e}"))?;
                drop(tar.stdin.take());
                if !tar.wait().map_err(|e| e.to_string())?.success() {
                    return Err("the archive doesn't unpack".to_owned());
                }
                let new = work.join(bin);
                if !new.is_file() {
                    return Err(format!("the archive has no {bin}"));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
                }
                let previous = previous_of(exe);
                std::fs::rename(exe, &previous).map_err(|e| format!("keeping the old {bin}: {e}"))?;
                if let Err(e) = std::fs::rename(&new, exe) {
                    let _ = std::fs::rename(&previous, exe);
                    return Err(format!("putting the new {bin} in place: {e}"));
                }
                Ok(Restart::Exit)
            })();
            let _ = std::fs::remove_dir_all(&work);
            result
        }
    }
}

/// The root helper of a .deb install (`<bin> update-install <dir>`, run
/// by `<bin>-update.service` when the daemon leaves a request): checks the
/// request against the signed manifest itself, installs a private copy of
/// the .deb, and leaves `result` for the daemon. Never trusts the daemon.
pub fn install_requested(dir: &Path, version: &str, keys: &[impl AsRef<str>]) -> Result<String, String> {
    let request = dir.join("request");
    let name = std::fs::read_to_string(&request).map_err(|e| format!("reading {}: {e}", request.display()))?;
    // Taken: the path unit doesn't start this again.
    let _ = std::fs::remove_file(&request);
    let r = (|| {
        let name = name.trim();
        if name.is_empty() || name.contains('/') || !name.ends_with(".deb") {
            return Err(format!("{name:?} is not a .deb"));
        }
        let read = |f: &str| std::fs::read(dir.join(f)).map_err(|e| format!("reading {f}: {e}"));
        let m = verify(&read("release.json")?, &read("release.json.sig")?, keys).map_err(|e| e.to_string())?;
        if !newer(&m.version, version) {
            return Err(format!("{} is not newer than {version}", m.version));
        }
        let want = m.assets.get(name).ok_or_else(|| format!("{name} is not in the release"))?;
        let bytes = read(name)?;
        if &checksum(&bytes) != want {
            return Err(format!("{name} isn't the file the release signed"));
        }
        // What dpkg installs is the copy checked, where nobody else writes.
        let private = std::env::temp_dir().join(format!("c6-update-{}", std::process::id()));
        std::fs::create_dir_all(&private).map_err(|e| e.to_string())?;
        let deb = private.join(name);
        std::fs::write(&deb, &bytes).map_err(|e| e.to_string())?;
        let out = std::process::Command::new("dpkg").arg("-i").arg(&deb).output().map_err(|e| format!("running dpkg: {e}"))?;
        let _ = std::fs::remove_dir_all(&private);
        if !out.status.success() {
            return Err(format!("dpkg -i {name}: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(format!("installed {name}"))
    })();
    let _ = std::fs::write(
        dir.join("result"),
        match &r {
            Ok(m) => format!("ok: {m}\n"),
            Err(e) => format!("failed: {e}\n"),
        },
    );
    r
}

fn previous_of(exe: &Path) -> PathBuf {
    let mut p = exe.as_os_str().to_owned();
    p.push(".previous");
    PathBuf::from(p)
}

/// Puts back the version `pending` replaced. Err: it can't (say why).
pub fn roll_back(method: &Method, bin: &str, exe: &Path, pending: &Pending) -> Result<(), String> {
    match method {
        Method::Tarball => {
            let previous = previous_of(exe);
            if !previous.is_file() {
                return Err("the previous binary is gone".into());
            }
            std::fs::rename(&previous, exe).map_err(|e| format!("putting the previous {bin} back: {e}"))
        }
        Method::Brew { prefix, .. } => {
            let keg = prefix.join("Cellar").join(bin).join(&pending.from);
            if !keg.is_dir() {
                return Err(format!("Homebrew no longer has {bin} {}", pending.from));
            }
            let opt = prefix.join("opt").join(bin);
            let tmp = prefix.join("opt").join(format!(".{bin}.rollback"));
            let _ = std::fs::remove_file(&tmp);
            #[cfg(unix)]
            std::os::unix::fs::symlink(&keg, &tmp).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &opt).map_err(|e| format!("pointing {} back at {}: {e}", opt.display(), keg.display()))
        }
        Method::Deb => Err("a .deb isn't rolled back: install the previous one by hand".into()),
        Method::Source => Err("built from source".into()),
    }
}

/// An update in progress, kept across the restart (`<dir>/pending.json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub from: String,
    pub to: String,
    /// Starts of the new version that didn't reach healthy.
    #[serde(default)]
    pub tries: u32,
    /// "installed" (waiting for the new version to be healthy) or
    /// "rolled_back" (the old version is back; it says so, then forgets).
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub detail: String,
    /// A database backup to put back with the old version (the head's).
    #[serde(default)]
    pub backup: String,
    #[serde(default)]
    pub started_at: i64,
}

/// Starts of a new version that don't get healthy before it is rolled back.
pub const MAX_TRIES: u32 = 3;

impl Pending {
    fn path(dir: &Path) -> PathBuf {
        dir.join("pending.json")
    }

    pub fn load(dir: &Path) -> Option<Pending> {
        serde_json::from_slice(&std::fs::read(Self::path(dir)).ok()?).ok()
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join("pending.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, Self::path(dir))
    }

    pub fn clear(dir: &Path) {
        let _ = std::fs::remove_file(Self::path(dir));
    }
}

/// What a start finds about an update (called first thing in `main`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OnStart {
    /// No update in progress.
    Nothing,
    /// The new version is starting: call [`healthy`] once it is.
    Trying(Pending),
    /// It didn't get healthy; the old version is back in place: exit with
    /// [`RESTART_CODE`] so it runs.
    RolledBack(Pending),
    /// This is the old version again after a rollback: say so, then it's over.
    WasRolledBack(Pending),
}

pub fn on_start(dir: &Path, version: &str, method: &Method, bin: &str, exe: &Path) -> OnStart {
    let Some(mut p) = Pending::load(dir) else { return OnStart::Nothing };
    if p.state == "rolled_back" {
        if parse(version) == parse(&p.from) {
            return OnStart::WasRolledBack(p);
        }
        Pending::clear(dir);
        return OnStart::Nothing;
    }
    if parse(version) != parse(&p.to) {
        // Something else put another version in place: not ours to judge.
        Pending::clear(dir);
        return OnStart::Nothing;
    }
    p.tries += 1;
    if p.tries > MAX_TRIES {
        p.state = "rolled_back".into();
        p.detail = match roll_back(method, bin, exe, &p) {
            Ok(()) => format!("{} didn't start {MAX_TRIES} times; {} is back", p.to, p.from),
            Err(e) => {
                p.detail = format!("{} didn't start {MAX_TRIES} times, and rolling back failed: {e}", p.to);
                let _ = p.save(dir);
                return OnStart::Trying(p);
            }
        };
        let _ = p.save(dir);
        return OnStart::RolledBack(p);
    }
    let _ = p.save(dir);
    OnStart::Trying(p)
}

/// The new version is healthy: the update is done. Returns what it was.
pub fn healthy(dir: &Path) -> Option<Pending> {
    let p = Pending::load(dir).filter(|p| p.state != "rolled_back")?;
    Pending::clear(dir);
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    fn key() -> (Ed25519KeyPair, String) {
        let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        let k = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
        let public = hex::encode(k.public_key().as_ref());
        (k, public)
    }

    #[test]
    fn only_signed_manifests_verify() {
        let (k, public) = key();
        let body = br#"{"version":"0.3.4","assets":{"c6h_0.3.4_amd64.deb":"sha256:ab"}}"#;
        let sig = k.sign(body);
        let m = verify(body, sig.as_ref(), &[&public]).unwrap();
        assert_eq!(m.version, "0.3.4");
        // Another key, a changed byte, or no keys at all: refused.
        let (_, other) = key();
        assert_eq!(verify(body, sig.as_ref(), &[&other]), Err(Error::BadSignature));
        let mut changed = body.to_vec();
        changed[14] = b'5';
        assert_eq!(verify(&changed, sig.as_ref(), &[&public]), Err(Error::BadSignature));
        assert_eq!(verify(body, sig.as_ref(), &[] as &[&str]), Err(Error::BadSignature));
        // Rotation: any listed key will do.
        assert!(verify(body, sig.as_ref(), &[&other, &public]).is_ok());
    }

    /// The release key signs with `openssl pkeyutl -sign -rawin` in the
    /// publish job (packaging/release/manifest.sh); ring verifies that.
    #[test]
    fn openssl_signatures_by_the_release_key_verify() {
        let sig = hex::decode("562e5b2510bf4cb3a967352e3e0c8c30cae846772519c3a6dffbd4612d50660bdeba0bf3c877f43d6a7d6cb20aec89b7dcc2f66a87fb4734c8437fabdeb6c705").unwrap();
        assert_eq!(verify(br#"{"version":"0.0.1"}"#, &sig, RELEASE_KEYS).unwrap().version, "0.0.1");
    }

    #[test]
    fn versions() {
        assert!(newer("0.3.4", "0.3.3") && newer("0.4.0", "0.3.9") && newer("v1.0.0", "0.9.9"));
        assert!(!newer("0.3.3", "0.3.3") && !newer("0.3.3", "0.3.3+git1700000000.abc") && !newer("0.3.3", "0.3.3.1700000000"));
        assert!(newer("0.3.4", "0.3.3.1700000000"));
        assert_eq!(step("0.3.3", "0.3.4"), Some(Step::Patch));
        assert_eq!(step("0.3.3", "0.4.0"), Some(Step::Minor));
        assert_eq!(step("0.3.3", "1.0.0"), Some(Step::Major));
        let mut m = Manifest { version: "0.3.4".into(), upgrade_from: "0.3.0".into(), ..Default::default() };
        assert!(unattended("0.3.1", &m) && unattended("0.3.0", &m) && !unattended("0.2.2", &m));
        m.manual = Some("Stop c6m first.".into());
        assert!(!unattended("0.3.3", &m));
    }

    #[test]
    fn assets_by_install() {
        let m = Manifest {
            version: "0.3.4".into(),
            assets: [
                ("c6w_0.3.4_arm64.deb", "sha256:1"),
                ("carbon6-0.3.4-linux-amd64.tar.gz", "sha256:2"),
                ("c6h-0.3.4-darwin-arm64.tar.gz", "sha256:3"),
            ]
            .into_iter()
            .map(|(a, b)| (a.to_owned(), b.to_owned()))
            .collect(),
            ..Default::default()
        };
        assert_eq!(asset(&m, "c6w", "deb", "linux", "aarch64").unwrap().1, "sha256:1");
        assert_eq!(asset(&m, "c6h", "tarball", "linux", "x86_64").unwrap().1, "sha256:2");
        assert_eq!(asset(&m, "c6h", "brew", "macos", "aarch64").unwrap().1, "sha256:3");
        assert_eq!(asset(&m, "c6h", "source", "macos", "aarch64"), None);
    }

    #[test]
    fn tarball_install_and_roll_back() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("c6w");
        std::fs::write(&exe, b"old").unwrap();
        let src = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("c6w"), b"new").unwrap();
        let tgz = std::process::Command::new("tar").args(["-czf", "-", "-C"]).arg(src.path()).arg("c6w").output().unwrap().stdout;
        let m = Manifest { version: "0.3.4".into(), ..Default::default() };
        assert_eq!(method(&exe, false), Method::Tarball);
        assert_eq!(method(&exe, true), Method::Source);
        let r = install(&Method::Tarball, "c6w", &exe, &m, (b"", b""), Some(("x.tar.gz", &tgz))).unwrap();
        assert_eq!(r, Restart::Exit);
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");

        // The new one fails to start three times: the old one is put back.
        let state = d.path().join("updates");
        Pending { from: "0.3.3".into(), to: "0.3.4".into(), state: "installed".into(), ..Default::default() }.save(&state).unwrap();
        for _ in 0..MAX_TRIES {
            assert!(matches!(on_start(&state, "0.3.4", &Method::Tarball, "c6w", &exe), OnStart::Trying(_)));
        }
        assert!(matches!(on_start(&state, "0.3.4", &Method::Tarball, "c6w", &exe), OnStart::RolledBack(_)));
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        // The old one starts, hears it, and the update is over.
        assert!(matches!(on_start(&state, "0.3.3", &Method::Tarball, "c6w", &exe), OnStart::WasRolledBack(_)));
        Pending::clear(&state);
        assert_eq!(on_start(&state, "0.3.3", &Method::Tarball, "c6w", &exe), OnStart::Nothing);

        // A start that gets healthy ends it.
        Pending { from: "0.3.3".into(), to: "0.3.4".into(), state: "installed".into(), ..Default::default() }.save(&state).unwrap();
        assert!(matches!(on_start(&state, "0.3.4", &Method::Tarball, "c6w", &exe), OnStart::Trying(_)));
        assert_eq!(healthy(&state).unwrap().from, "0.3.3");
        assert_eq!(Pending::load(&state), None);
    }

    #[test]
    fn methods() {
        assert!(
            matches!(method(Path::new("/opt/homebrew/Cellar/c6h/0.3.3/bin/c6h"), false), Method::Brew { prefix, .. } if prefix == Path::new("/opt/homebrew"))
        );
        assert_eq!(method(Path::new("/nonexistent/c6h"), false), Method::Source);
    }
}
