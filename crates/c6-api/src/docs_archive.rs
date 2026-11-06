//! Document cleanup: a fixed selection, its safety notes, and its result.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArchiveDocCandidate {
    pub id: String,
    pub title: String,
    pub kind: String,
    /// Empty means no known use outside the session family. These are
    /// warnings, not permission to silently add or remove a selection.
    pub reasons: Vec<String>,
}

/// Archive exactly these documents. Agents give a reason for the user.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ArchiveDocsIn {
    pub ids: Vec<String>,
    pub reason: String,
    #[serde(default)]
    pub realm: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArchiveDocsResult {
    pub archived: Vec<String>,
    pub skipped: Vec<ArchiveDocSkipped>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ArchiveDocSkipped {
    pub id: String,
    pub reason: String,
}
