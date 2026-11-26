//! Machine telemetry shared by workers, head and browser.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineSample {
    /// Sampler-local timestamp on transport; head inventory/history normalize
    /// accepted observations to head receipt time.
    pub at_ms: i64,
    pub cpu_percent: f64,
    pub cpu_count: usize,
    pub load: [f64; 3],
    pub memory_used: u64,
    pub memory_available: u64,
    pub memory_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub oom_kills: Option<u64>,
    #[serde(default)]
    pub pressure: BTreeMap<String, String>,
    #[serde(default)]
    pub disks: Vec<MachineDisk>,
    #[serde(default)]
    pub sessions: Vec<MachineSessionUsage>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineDisk {
    pub mount: String,
    pub filesystem: String,
    pub total: u64,
    pub available: u64,
    pub used: u64,
    pub inodes_used: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineSessionUsage {
    pub session_id: String,
    /// Percent of one logical CPU; a multithreaded session may exceed 100.
    pub cpu_percent: f64,
    pub rss: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineFolderUsage {
    pub path: String,
    pub bytes: u64,
    pub modified_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineDiskScan {
    pub at_ms: i64,
    pub entries: usize,
    /// A limit was reached or some paths could not be read.
    pub partial: bool,
    /// Largest directories, in descending byte order. Sizes include descendants.
    pub folders: Vec<MachineFolderUsage>,
    /// Recognized Rust build directories. Unlike `folders`, these include
    /// attribution and are not restricted to the largest generic directories.
    #[serde(default)]
    pub build_folders: Vec<MachineBuildFolder>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineBuildFolder {
    pub path: String,
    pub bytes: u64,
    pub modified_ms: Option<i64>,
    /// Most recent recorded use or observed descendant modification. This is
    /// not filesystem atime (which is commonly disabled).
    pub last_used_ms: Option<i64>,
    pub carbon6_owned: bool,
    pub active: bool,
    /// Only set when every attributed session has archived; the latest archive
    /// starts the grace period for shared build directories.
    pub archived_at_ms: Option<i64>,
    #[serde(default)]
    pub session_ids: Vec<String>,
    #[serde(default)]
    pub repositories: Vec<String>,
    /// An incremental subdirectory can be explicitly removed independently.
    pub incremental: bool,
    /// Size or attribution could not be fully determined; never auto-cleaned.
    pub partial: bool,
}

// Inventory and history contracts (head-owned).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub role: String,
    pub version: Option<String>,
    pub online: bool,
    /// Effective sampling interval in seconds, including the minimum of one
    /// second. Older heads omit this field and sampled every fifteen seconds.
    #[serde(default = "default_sample_secs")]
    pub sample_secs: u64,
    pub last_sample: Option<MachineSample>,
    pub warnings: Vec<String>,
    pub worker: Option<crate::Worker>,
    pub head: Option<HeadMachineInfo>,
}

fn default_sample_secs() -> u64 {
    15
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct HeadMachineInfo {
    pub uptime_secs: u64,
    pub data_dir: String,
    /// Filesystem holding the head's database, matched by device where possible.
    #[serde(default)]
    pub data_disk: Option<MachineDisk>,
    pub databases: Vec<MachineDatabase>,
    pub connected_workers: usize,
    pub update: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineDatabase {
    pub name: String,
    pub bytes: u64,
}

/// Machine history uses peak envelopes for compacted/response-limited points:
/// maxima of CPU/used memory, minima of available memory/disk, and the latest
/// contributing timestamp. Envelope fields need not have occurred together.
/// Pressure strings are the latest raw observation, not a pressure peak. OOM
/// counters retain a bucket maximum, so reboot/reset boundaries and precise
/// kill timing cannot be reconstructed from coarse history. Session envelopes
/// are capped to the top 512 CPU peaks; disk envelopes to the 128 fullest disks.
/// Accepted observation timestamps are head receipt times, not worker clocks.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct MachineSamplesQuery {
    /// Inclusive start; defaults to the last 24 hours (or shorter retention).
    pub from_ms: Option<i64>,
    /// Inclusive end; defaults to the head's current time.
    pub to_ms: Option<i64>,
    /// Defaults to 480; capped at 2000. Zero is invalid.
    pub limit: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_sampling_interval_defaults_for_older_heads_and_roundtrips() {
        let value = serde_json::json!({
            "id": "head", "name": "Head", "hostname": "host", "role": "head",
            "version": null, "online": true, "last_sample": null, "warnings": [],
            "worker": null, "head": null
        });
        let mut machine: Machine = serde_json::from_value(value).unwrap();
        assert_eq!(machine.sample_secs, 15);
        machine.sample_secs = 120;
        let decoded: Machine = serde_json::from_value(serde_json::to_value(&machine).unwrap()).unwrap();
        assert_eq!(decoded.sample_secs, 120);
    }
}
