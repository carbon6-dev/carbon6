//! Worker-owned compiler caching. Never writes a repository's configuration.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const GIB: u64 = 1024 * 1024 * 1024;
const INSTALL: &str = "cargo install sccache --locked";
static PROBES: std::sync::LazyLock<std::sync::Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Semaphore::new(2)));

async fn probe<T: Send + 'static>(
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    let permit = slots.try_acquire_owned().context("compile cache filesystem probes are busy")?;
    let task = tokio::task::spawn_blocking(move || {
        // A timeout cannot interrupt a filesystem syscall. Retaining this
        // permit bounds abandoned probes across sessions and reconnects.
        let _permit = permit;
        work()
    });
    tokio::time::timeout(Duration::from_secs(2), task).await.context("compile cache filesystem probe timed out")??
}

fn absolute_state(state: &Path) -> anyhow::Result<PathBuf> {
    Ok(std::path::absolute(state)?)
}

fn validate_socket(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let limit = if cfg!(target_vendor = "apple") { 104 } else { 108 };
    anyhow::ensure!(
        path.as_os_str().as_bytes().len() < limit,
        "sccache socket path is too long; use a shorter worker state directory (cache disabled for commands)"
    );
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    pub enabled: bool,
    pub directory: Option<PathBuf>,
    pub max_bytes: Option<u64>,
    pub rust: bool,
    pub c_cpp: bool,
    pub archived_grace_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self { enabled: false, directory: None, max_bytes: None, rust: true, c_cpp: false, archived_grace_days: 3 }
    }
}

pub(crate) fn load(state: &Path) -> anyhow::Result<Config> {
    match std::fs::read(state.join("compile-cache.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

fn directory(config: &Config, state: &Path) -> PathBuf {
    config.directory.clone().unwrap_or_else(|| state.join("sccache"))
}

fn limit(config: &Config, total: Option<u64>) -> u64 {
    config.max_bytes.unwrap_or_else(|| (100 * GIB).min(total.map_or(100 * GIB, |n| (n / 10).max(1))))
}

fn server_socket(state: &Path, directory: &Path, max_bytes: u64) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    directory.hash(&mut hash);
    max_bytes.hash(&mut hash);
    // Isolate this worker from the user's or another worker's default server.
    // Changed settings start a new server on demand; the previous one can
    // finish its live compiles and exit at sccache's ordinary idle timeout.
    state.join(format!("sccache-{:016x}.sock", hash.finish()))
}

fn partition_total(path: &Path) -> Option<u64> {
    let mut path = path;
    while !path.exists() {
        path = path.parent()?;
    }
    crate::disk::sample(path).map(|s| s.total)
}

fn find_sccache(paths: Option<std::ffi::OsString>, home: Option<PathBuf>) -> Option<PathBuf> {
    let paths = paths.into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>());
    paths
        .map(|p| p.join("sccache"))
        .chain(home.map(|h| h.join(".cargo/bin/sccache")))
        .find(|p| {
            use std::os::unix::fs::PermissionsExt;
            p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .and_then(|p| if p.is_absolute() { Some(p) } else { Some(std::env::current_dir().ok()?.join(p)) })
}

fn executable() -> Option<PathBuf> {
    find_sccache(std::env::var_os("PATH"), dirs::home_dir())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn wrapper(state: &Path, executable: &Path, compiler: &str) -> anyhow::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let root = state.join("compile-wrappers");
    c6_core::create_private_dir(&root)?;
    let path = root.join(compiler);
    let content = format!("#!/bin/sh\nexec {} {compiler} \"$@\"\n", shell_quote(executable));
    if std::fs::read_to_string(&path).ok().as_deref() != Some(&content) {
        let mut file = tempfile::NamedTempFile::new_in(&root)?;
        use std::io::Write;
        file.write_all(content.as_bytes())?;
        file.as_file().set_permissions(std::fs::Permissions::from_mode(0o700))?;
        file.persist(&path).map_err(|e| e.error)?;
    }
    Ok(path.to_string_lossy().into_owned())
}

fn environment(
    config: &Config,
    state: &Path,
    executable: Option<&Path>,
    total: Option<u64>,
    inherited_wrapper: bool,
    inherited_compilers: [bool; 2],
) -> anyhow::Result<Vec<(String, String)>> {
    let Some(executable) = executable.filter(|_| config.enabled) else { return Ok(Vec::new()) };
    let state = absolute_state(state)?;
    let state = state.as_path();
    let socket = server_socket(state, &directory(config, state), limit(config, total));
    validate_socket(&socket)?;
    let mut env = vec![
        ("SCCACHE_DIR".into(), directory(config, state).to_string_lossy().into_owned()),
        ("SCCACHE_CACHE_SIZE".into(), limit(config, total).to_string()),
        ("SCCACHE_SERVER_UDS".into(), socket.to_string_lossy().into_owned()),
    ];
    if config.rust && !inherited_wrapper {
        env.push(("RUSTC_WRAPPER".into(), executable.to_string_lossy().into_owned()));
    }
    if config.c_cpp {
        for ((key, compiler), inherited) in [("CC", "cc"), ("CXX", "c++")].into_iter().zip(inherited_compilers) {
            if !inherited {
                env.push((key.into(), wrapper(state, executable, compiler)?));
            }
        }
    }
    Ok(env)
}

/// Resolve once per turn, away from Tokio's executor. Missing sccache is a
/// status, not permission to install anything or break an ordinary build.
pub(crate) async fn session_environment(state: PathBuf) -> Vec<(String, String)> {
    probe(PROBES.clone(), move || {
        let state = absolute_state(&state)?;
        let config = load(&state)?;
        if !config.enabled {
            return Ok(Vec::new());
        }
        let Some(executable) = executable() else { return Ok(Vec::new()) };
        let total = config.max_bytes.is_none().then(|| partition_total(&directory(&config, &state))).flatten();
        environment(
            &config,
            &state,
            Some(&executable),
            total,
            std::env::var_os("RUSTC_WRAPPER").is_some(),
            [std::env::var_os("CC").is_some(), std::env::var_os("CXX").is_some()],
        )
    })
    .await
    .unwrap_or_else(|error: anyhow::Error| {
        tracing::warn!(%error, "compile cache environment unavailable");
        Vec::new()
    })
}

#[derive(Debug, Default, Serialize)]
struct Stats {
    cache_size_bytes: Option<u64>,
    cache_hits: Option<u64>,
    cache_misses: Option<u64>,
    hit_rate: Option<f64>,
    compile_time_saved_seconds: Option<f64>,
    compile_time_saved_estimated: bool,
}

fn count(value: &Value) -> Option<u64> {
    // sccache reports both per-language and per-compiler breakdowns of the
    // same requests. Counting both would double the displayed hit count.
    if let Some(counts) = value.get("counts") {
        return count(counts);
    }
    value.as_u64().or_else(|| value.as_object()?.values().try_fold(0_u64, |sum, value| Some(sum.saturating_add(count(value)?))))
}

fn seconds(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| Some(value.get("secs")?.as_u64()? as f64 + value.get("nanos").and_then(Value::as_u64).unwrap_or(0) as f64 / 1e9))
}

fn parse_stats(value: &Value) -> Stats {
    let stats = &value["stats"];
    let cache_hits = count(&stats["cache_hits"]);
    let cache_misses = count(&stats["cache_misses"]);
    let requests = cache_hits.zip(cache_misses).map(|(hits, misses)| hits.saturating_add(misses));
    let saved = seconds(&stats["compile_time_saved"]).or_else(|| seconds(&stats["compilation_time_saved"]));
    let estimated = || {
        let compilations = stats["compilations"].as_u64().filter(|n| *n > 0)?;
        Some(
            (seconds(&stats["compiler_write_duration"])? / compilations as f64 * cache_hits? as f64
                - seconds(&stats["cache_read_hit_duration"])?)
            .max(0.0),
        )
    };
    Stats {
        cache_size_bytes: value["cache_size"].as_u64(),
        cache_hits,
        cache_misses,
        hit_rate: requests.filter(|n| *n > 0).and_then(|n| Some(cache_hits? as f64 / n as f64)),
        // Current sccache reports aggregate compile and read durations, not
        // the original duration of each hit. Label this estimate explicitly.
        compile_time_saved_seconds: saved.or_else(estimated),
        compile_time_saved_estimated: saved.is_none(),
    }
}

fn state_name(config: &Config, installed: bool) -> &'static str {
    if !config.enabled {
        "disabled"
    } else if installed {
        "ready"
    } else {
        "missing"
    }
}

pub(crate) async fn status(state: PathBuf) -> anyhow::Result<Value> {
    let (config, directory, max_bytes, socket, executable) = probe(PROBES.clone(), move || {
        let state = absolute_state(&state)?;
        let config = load(&state)?;
        let directory = directory(&config, &state);
        let executable = executable();
        let total = (config.max_bytes.is_none() && executable.is_some()).then(|| partition_total(&directory)).flatten();
        let max_bytes = limit(&config, total);
        let socket = server_socket(&state, &directory, max_bytes);
        Ok::<_, anyhow::Error>((config, directory, max_bytes, socket, executable))
    })
    .await?;
    let mut result = json!({
        "status": state_name(&config, executable.is_some()),
        "config": config, "directory": directory, "max_bytes": max_bytes,
        "install_command": INSTALL, "stats": null,
    });
    if config.enabled
        && let Some(executable) = executable
    {
        if let Err(error) = validate_socket(&socket) {
            result["status"] = json!("unavailable");
            result["stats_error"] = json!(error.to_string());
            return Ok(result);
        }
        let mut command = c6_tools::process::session_command(executable);
        command
            .args(["--show-stats", "--stats-format=json"])
            .env("SCCACHE_DIR", &directory)
            .env("SCCACHE_CACHE_SIZE", max_bytes.to_string())
            .env("SCCACHE_SERVER_UDS", socket)
            .kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(3), command.output()).await {
            Ok(Ok(output)) if output.status.success() => match serde_json::from_slice::<Value>(&output.stdout) {
                Ok(stats) => result["stats"] = serde_json::to_value(parse_stats(&stats))?,
                Err(error) => result["stats_error"] = json!(error.to_string()),
            },
            Ok(Ok(_)) => result["stats_error"] = json!("sccache stats unavailable; server starts on first compilation"),
            Ok(Err(error)) => result["stats_error"] = json!(error.to_string()),
            Err(_) => result["stats_error"] = json!("sccache statistics timed out"),
        }
    }
    Ok(result)
}

pub(crate) async fn configure(state: PathBuf, value: Value) -> anyhow::Result<Value> {
    let state = absolute_state(&state)?;
    let config: Config = serde_json::from_value(value)?;
    anyhow::ensure!(config.max_bytes != Some(0), "cache size must be positive");
    if let Some(path) = &config.directory {
        anyhow::ensure!(path.is_absolute(), "cache directory must be absolute");
    }
    let save_state = state.clone();
    probe(PROBES.clone(), move || {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new_in(&save_state)?;
        file.write_all(&serde_json::to_vec_pretty(&config)?)?;
        file.as_file().sync_all()?;
        file.persist(save_state.join("compile-cache.json")).map_err(|e| e.error).context("saving compile cache settings")?;
        Ok::<_, anyhow::Error>(())
    })
    .await?;
    status(state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opt_in_and_user_wrapper_are_respected() {
        let state = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        let exe = Path::new("/bin/sccache");
        assert!(environment(&config, state.path(), Some(exe), Some(200 * GIB), false, [false; 2]).unwrap().is_empty());
        config.enabled = true;
        let env = environment(&config, state.path(), Some(exe), Some(200 * GIB), false, [false; 2]).unwrap();
        assert!(env.contains(&("RUSTC_WRAPPER".into(), "/bin/sccache".into())));
        assert!(env.contains(&("SCCACHE_CACHE_SIZE".into(), (20 * GIB).to_string())));
        assert!(!env.iter().any(|(k, _)| k == "CC" || k == "CXX"));
        assert!(!environment(&config, state.path(), Some(exe), None, true, [false; 2]).unwrap().iter().any(|(k, _)| k == "RUSTC_WRAPPER"));
        assert!(environment(&config, state.path(), None, None, false, [false; 2]).unwrap().is_empty());
        assert!(!state.path().join("compile-wrappers").exists());
    }

    #[test]
    fn executable_discovery_does_not_install() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        assert!(find_sccache(Some("".into()), Some(home.path().into())).is_none());
        let config = Config { enabled: true, ..Default::default() };
        assert_eq!(state_name(&config, false), "missing");
        assert!(!home.path().join(".cargo").exists());
        let bin = home.path().join(".cargo/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("sccache");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(find_sccache(Some("".into()), Some(home.path().into())), Some(exe));
    }

    #[test]
    fn stats_support_language_counters_and_unknown_saved_time() {
        let stats = parse_stats(&json!({"cache_size":42,"stats":{"cache_hits":{"Rust":8,"C/C++":2},"cache_misses":{"Rust":5}}}));
        assert_eq!(stats.cache_size_bytes, Some(42));
        assert_eq!(stats.cache_hits, Some(10));
        assert_eq!(stats.cache_misses, Some(5));
        assert_eq!(stats.hit_rate, Some(2.0 / 3.0));
        assert_eq!(stats.compile_time_saved_seconds, None);
        let stats = parse_stats(&json!({"stats":{"compile_time_saved":{"secs":12,"nanos":500000000}}}));
        assert_eq!(stats.compile_time_saved_seconds, Some(12.5));
        assert_eq!(stats.hit_rate, None);
        let stats = parse_stats(&json!({"stats":{
            "cache_hits":{"counts":{"Rust":4},"adv_counts":{"rust":4}},
            "cache_misses":{"counts":{"Rust":2},"adv_counts":{"rust":2}},
            "compilations":2,"compiler_write_duration":{"secs":20,"nanos":0},
            "cache_read_hit_duration":{"secs":1,"nanos":0}
        }}));
        assert_eq!(stats.cache_hits, Some(4));
        assert_eq!(stats.cache_misses, Some(2));
        assert_eq!(stats.compile_time_saved_seconds, Some(39.0));
        assert!(stats.compile_time_saved_estimated);
    }

    #[test]
    fn missing_or_unsupported_counters_stay_unknown_instead_of_zero() {
        for value in [json!({}), json!({"stats":{"cache_hits":"unsupported","cache_misses":null}})] {
            let stats = parse_stats(&value);
            assert_eq!(stats.cache_hits, None);
            assert_eq!(stats.cache_misses, None);
            assert_eq!(stats.hit_rate, None);
            assert_eq!(stats.compile_time_saved_seconds, None);
            let wire = serde_json::to_value(stats).unwrap();
            assert!(wire["cache_hits"].is_null() && wire["cache_misses"].is_null());
        }
        let stats = parse_stats(&json!({"stats":{"cache_hits":{"counts":{}},"cache_misses":0}}));
        assert_eq!(stats.cache_hits, Some(0));
        assert_eq!(stats.cache_misses, Some(0));
        assert_eq!(stats.hit_rate, None);
        let stats = parse_stats(&json!({"stats":{"cache_hits":5}}));
        assert_eq!(stats.cache_hits, Some(5));
        assert_eq!(stats.cache_misses, None);
        assert_eq!(stats.hit_rate, None);
    }

    #[test]
    fn defaults_cap_at_partition_tenth_and_100_gib() {
        let mut config = Config::default();
        assert_eq!(limit(&config, Some(2_000 * GIB)), 100 * GIB);
        assert_eq!(limit(&config, Some(100 * GIB)), 10 * GIB);
        config.max_bytes = Some(7 * GIB);
        assert_eq!(limit(&config, Some(100 * GIB)), 7 * GIB);
    }

    #[test]
    fn server_is_worker_owned_and_setting_changes_do_not_stop_running_builds() {
        let a = Path::new("/worker/a");
        let b = Path::new("/worker/b");
        let cache = Path::new("/cache");
        assert_eq!(server_socket(a, cache, 100), server_socket(a, cache, 100));
        assert_ne!(server_socket(a, cache, 100), server_socket(b, cache, 100));
        assert_ne!(server_socket(a, cache, 100), server_socket(a, cache, 200));
        assert!(server_socket(a, cache, 100).starts_with(a));
    }

    #[test]
    fn c_cpp_wrappers_are_explicit_and_only_written_to_worker_state() {
        let state = tempfile::tempdir().unwrap();
        let config = Config { enabled: true, c_cpp: true, ..Default::default() };
        let env = environment(&config, state.path(), Some(Path::new("/tools/it's sccache")), None, false, [false; 2]).unwrap();
        for key in ["CC", "CXX"] {
            let path = Path::new(&env.iter().find(|(k, _)| k == key).unwrap().1);
            assert!(path.starts_with(state.path().join("compile-wrappers")));
            let script = std::fs::read_to_string(path).unwrap();
            assert!(script.contains("'/tools/it'\\''s sccache'"));
            assert!(script.ends_with("\"$@\"\n"));
        }
        let env = environment(&config, state.path(), Some(Path::new("/tools/sccache")), None, false, [true; 2]).unwrap();
        assert!(!env.iter().any(|(key, _)| key == "CC" || key == "CXX"), "keep inherited toolchain selections");
    }

    #[test]
    fn relative_state_exports_absolute_paths_and_long_sockets_fail_open() {
        let config = Config { enabled: true, ..Default::default() };
        let env = environment(&config, Path::new("worker-state"), Some(Path::new("/tools/sccache")), None, false, [false; 2]).unwrap();
        for key in ["SCCACHE_DIR", "SCCACHE_SERVER_UDS", "RUSTC_WRAPPER"] {
            assert!(Path::new(&env.iter().find(|(name, _)| name == key).unwrap().1).is_absolute());
        }
        let long = PathBuf::from(format!("/{}", "long-worker-state".repeat(10)));
        assert!(environment(&config, &long, Some(Path::new("/tools/sccache")), None, false, [false; 2]).is_err());
        assert!(!long.exists(), "validation must not create a cache or socket");
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_filesystem_probe_times_out_and_keeps_its_slot_until_finished() {
        let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(probe(slots.clone(), move || {
            let _ = entered.send(());
            wait.recv()?;
            Ok(())
        }));
        ready.await.unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        assert!(task.await.unwrap().is_err());
        assert_eq!(slots.available_permits(), 0, "abandoning a wait cannot admit another stuck syscall");
        assert!(probe(slots.clone(), || Ok(())).await.is_err());
        release.send(()).unwrap();
        drop(slots.acquire().await.unwrap());
        assert_eq!(slots.available_permits(), 1);
    }

    #[tokio::test]
    async fn settings_persist_and_disabled_never_install_or_create_cache() {
        let state = tempfile::tempdir().unwrap();
        let result = configure(state.path().into(), json!({"enabled":false,"archived_grace_days":7})).await.unwrap();
        assert_eq!(result["status"], "disabled");
        assert_eq!(load(state.path()).unwrap().archived_grace_days, 7);
        assert!(!state.path().join("sccache").exists());
        assert!(!state.path().join("compile-wrappers").exists());
        assert!(configure(state.path().into(), json!({"max_bytes":0})).await.is_err());
        assert!(configure(state.path().into(), json!({"directory":"relative"})).await.is_err());
        assert_eq!(load(state.path()).unwrap().archived_grace_days, 7);
    }
}
