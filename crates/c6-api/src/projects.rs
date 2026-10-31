//! Projects (docs/projects.md): long-running efforts made of work items
//! that each finish quickly, plus what outlives them: documents, links,
//! phases. Sessions belong to a project only through its work.

use serde::{Deserialize, Serialize};

use crate::UnixMs;
use crate::work::{Told, WorkOffer};

/// A project's fields, as its record in the `projects` system collection
/// keeps them (its state is worked out when read).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ProjectFields {
    /// "Realms for the team".
    pub title: String,
    /// What it's for and what success means.
    #[serde(default)]
    pub purpose: String,
    /// active, paused, done or dropped (you set it).
    #[serde(default = "active")]
    pub status: String,
    /// Optional, in order; at most one is current.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<Phase>,
    /// The docs and decks it refers to, each with a role.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<ProjectDoc>,
    /// Records, decisions, outside URLs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ProjectLink>,
    /// Where we are: the current phase, what's moving, what's stuck, the
    /// next decision.
    #[serde(default)]
    pub note: Option<ProjectNote>,
    /// An agent's "looks done" for the project.
    #[serde(default)]
    pub offer: Option<WorkOffer>,
    /// When it or its work last moved.
    #[serde(default)]
    pub moved_at: UnixMs,
    /// When you were last nudged about it, and for which state.
    #[serde(default)]
    pub told: Option<Told>,
    #[serde(default)]
    pub closed_at: Option<UnixMs>,
}

fn active() -> String {
    "active".into()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Phase {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub done_when: String,
    /// A target date, YYYY-MM-DD (empty: none).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// upcoming, current or done.
    #[serde(default)]
    pub state: String,
    /// An agent's "phase looks done".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer: Option<WorkOffer>,
}

/// A document of the project, with its role.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ProjectDoc {
    /// The document's id.
    pub doc: String,
    /// plan, spec, notes, review, or empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// you, or session:<id>.
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub at: UnixMs,
}

/// A link: `record:<collection>:<id>`, `decision:<id>`, or a URL.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ProjectLink {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub at: UnixMs,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ProjectNote {
    /// A few short lines.
    pub text: String,
    #[serde(default)]
    pub at: UnixMs,
}

/// A piece of work in a project, as the project shows it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectWork {
    pub id: String,
    pub title: String,
    /// The work's state (needs_you, moving, …, done, dropped).
    pub state: String,
    #[serde(default)]
    pub why: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub phase: String,
    #[serde(default)]
    pub moved_at: UnixMs,
    #[serde(default)]
    pub closed_at: Option<UnixMs>,
}

/// A project document as it is now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LiveDoc {
    pub doc: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    pub title: String,
    /// doc or deck ("" when it's gone).
    pub kind: String,
    #[serde(default)]
    pub updated_at: UnixMs,
}

/// A project as the API gives it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectItem {
    pub id: String,
    pub realm: String,
    pub version: i64,
    #[serde(flatten)]
    pub fields: ProjectFields,
    /// needs_you, moving, stalled, quiet, on_track, paused, done or dropped.
    pub state: String,
    #[serde(default)]
    pub why: String,
    /// The current phase's target date has passed.
    #[serde(default)]
    pub late: bool,
    /// Its work, open first.
    #[serde(default)]
    pub work: Vec<ProjectWork>,
    /// Its documents as they are now, the plan first.
    #[serde(default)]
    pub docs: Vec<LiveDoc>,
    pub labels: Vec<String>,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
}

/// `POST /api/v1/projects`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectNew {
    pub title: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub labels: Vec<String>,
    /// Work items to bring in at once (their ids).
    #[serde(default)]
    pub work: Vec<String>,
    /// Start its plan doc from the prompt `project.plan` (default yes).
    #[serde(default = "yes")]
    pub plan: bool,
}

fn yes() -> bool {
    true
}

/// `POST /api/v1/projects/{id}/docs`: a document joins the project with a
/// role (posting it again changes the role). One plan at most: a new plan
/// makes the old one roleless.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectDocIn {
    /// The document's id (or doc:<id>).
    pub doc: String,
    /// plan, spec, notes, review, or empty.
    #[serde(default)]
    pub role: String,
}

/// `POST /api/v1/projects/{id}/links`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectLinkIn {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub title: String,
}

/// Roles a document plays in a project.
pub const ROLES: [&str; 4] = ["plan", "spec", "notes", "review"];

/// `PATCH /api/v1/projects/{id}`: what changes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub purpose: Option<String>,
    /// active, paused, done or dropped.
    #[serde(default)]
    pub status: Option<String>,
    /// Not done after all: forget the agent's "looks done".
    #[serde(default)]
    pub dismiss_offer: bool,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

/// `POST /api/v1/projects/{id}/work`: a work item moves into the project
/// (out of any other), in a phase or none.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectWorkIn {
    pub work: String,
    #[serde(default)]
    pub phase: String,
}

/// `POST /api/v1/projects/{id}/detach`: what leaves it (one of them).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectDetach {
    #[serde(default)]
    pub work: Option<String>,
    #[serde(default)]
    pub doc: Option<String>,
    #[serde(default, rename = "ref")]
    pub link: Option<String>,
}

/// `POST /api/v1/projects/{id}/merge`: `from` joins this one and is gone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProjectMerge {
    pub from: String,
}

/// The order projects are listed in, by state: what needs you first.
pub const STATES: [&str; 8] = ["needs_you", "stalled", "quiet", "moving", "on_track", "paused", "done", "dropped"];

/// The state's name as you read it.
pub fn state_label(state: &str) -> &'static str {
    match state {
        "needs_you" => "Needs you",
        "stalled" => "Stalled",
        "quiet" => "Gone quiet",
        "moving" => "Moving",
        "on_track" => "On track",
        "paused" => "Paused",
        "done" => "Done",
        "dropped" => "Dropped",
        _ => "On track",
    }
}
