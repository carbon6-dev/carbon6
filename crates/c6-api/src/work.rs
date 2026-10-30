//! Work: what you've left open (docs/work.md). A work item is something
//! you mean to finish, with the sessions, requests, conversations,
//! decisions and outside things that belong to it. Its state is worked
//! out from them; you close it.

use serde::{Deserialize, Serialize};

use crate::UnixMs;

/// A work item's fields, as its record in the `work` system collection
/// keeps them (`state` and `why` are worked out when read).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkFields {
    /// A few words: what you mean to get done.
    pub title: String,
    /// What finished looks like, one line.
    #[serde(default)]
    pub done_when: String,
    /// open, done or dropped (only you close it).
    #[serde(default = "open")]
    pub status: String,
    #[serde(default)]
    pub attached: Vec<Attached>,
    /// Where was I: what's done, what's left, the next step.
    #[serde(default)]
    pub note: Option<WorkNote>,
    /// An agent's "looks done", waiting for you to close it or not.
    #[serde(default)]
    pub offer: Option<WorkOffer>,
    /// Out of the way until then.
    #[serde(default)]
    pub snoozed_until: Option<UnixMs>,
    /// When something attached last moved.
    #[serde(default)]
    pub moved_at: UnixMs,
    /// When you were last nudged about it, and for which state.
    #[serde(default)]
    pub told: Option<Told>,
    #[serde(default)]
    pub closed_at: Option<UnixMs>,
}

fn open() -> String {
    "open".into()
}

/// Something that belongs to a work item, by reference:
/// `session:<id>`, `request:<id>`, `thread:<account>:<thread>`,
/// `chat:<workspace>:<channel>[:<ts>]`, `decision:<id>`, or a URL.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Attached {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub title: String,
    /// you, guess, or session:<id>.
    #[serde(default)]
    pub by: String,
    #[serde(default)]
    pub at: UnixMs,
    /// For outside things, as a session last saw them: running, waiting,
    /// done or failed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkNote {
    #[serde(default)]
    pub done: String,
    #[serde(default)]
    pub left: String,
    #[serde(default)]
    pub next: String,
    #[serde(default)]
    pub at: UnixMs,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct WorkOffer {
    /// Why it looks done.
    pub evidence: String,
    #[serde(default)]
    pub at: UnixMs,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Told {
    pub state: String,
    pub at: UnixMs,
}

/// A work item as the API gives it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkItem {
    pub id: String,
    pub realm: String,
    pub version: i64,
    #[serde(flatten)]
    pub fields: WorkFields,
    /// needs_you, moving, waiting, quiet, open, snoozed, done or dropped.
    pub state: String,
    /// Why it's in that state, one line.
    #[serde(default)]
    pub why: String,
    /// What's attached, as it is now.
    #[serde(default)]
    pub live: Vec<LiveAttached>,
    pub labels: Vec<String>,
    pub created_at: UnixMs,
    pub updated_at: UnixMs,
}

/// An attached thing as it is now.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct LiveAttached {
    #[serde(rename = "ref")]
    pub reference: String,
    pub title: String,
    /// What it's doing: a session's running, idle, needs_you or failed; a
    /// request's open, snoozed, done or dropped; a decision's open or
    /// answered; an outside thing's status as last seen.
    pub state: String,
    /// When it last moved (0: unknown).
    #[serde(default)]
    pub at: UnixMs,
}

/// `POST /api/v1/work`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkNew {
    pub title: String,
    #[serde(default)]
    pub done_when: String,
    /// Things to attach at once.
    #[serde(default)]
    pub attach: Vec<WorkRef>,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// `PATCH /api/v1/work/{id}`: what changes.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub done_when: Option<String>,
    /// open (reopen), done or dropped.
    #[serde(default)]
    pub status: Option<String>,
    /// Snooze until then; 0 wakes it.
    #[serde(default)]
    pub snooze_until: Option<UnixMs>,
    /// Not done after all: forget the agent's "looks done".
    #[serde(default)]
    pub dismiss_offer: bool,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
}

/// A thing to attach (`POST /api/v1/work/{id}/attach`), or to detach.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkRef {
    #[serde(rename = "ref")]
    pub reference: String,
    #[serde(default)]
    pub title: String,
}

/// `POST /api/v1/work/{id}/merge`: `from` joins this one and is gone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkMerge {
    pub from: String,
}

/// A piece of work an agent found in what's open (the one-time offer,
/// docs/work.md §9): yours to accept or skip.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkProposal {
    pub title: String,
    #[serde(default)]
    pub done_when: String,
    pub attach: Vec<WorkRef>,
}

/// How work behaves in a realm (`/api/v1/settings/work`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WorkSettings {
    /// An agent picks the work each new session belongs to.
    #[serde(default = "yes")]
    pub guess: bool,
    /// An agent keeps "where was I" and offers "looks done".
    #[serde(default = "yes")]
    pub notes: bool,
    /// Days without movement before work has gone quiet.
    #[serde(default = "quiet_days")]
    pub quiet_days: u32,
    /// Notifications when work goes quiet, or has needed you this long.
    #[serde(default = "yes")]
    pub nudge: bool,
    #[serde(default = "needs_hours")]
    pub needs_hours: u32,
}

fn yes() -> bool {
    true
}

fn quiet_days() -> u32 {
    3
}

fn needs_hours() -> u32 {
    4
}

impl Default for WorkSettings {
    fn default() -> Self {
        Self { guess: true, notes: true, quiet_days: quiet_days(), nudge: true, needs_hours: needs_hours() }
    }
}

/// The order work is listed in, by state: what needs you first.
pub const STATES: [&str; 8] = ["needs_you", "quiet", "moving", "waiting", "open", "snoozed", "done", "dropped"];

/// The state's name as you read it.
pub fn state_label(state: &str) -> &'static str {
    match state {
        "needs_you" => "Needs you",
        "quiet" => "Gone quiet",
        "moving" => "Moving",
        "waiting" => "Waiting on others",
        "open" => "Open",
        "snoozed" => "Snoozed",
        "done" => "Done",
        "dropped" => "Dropped",
        _ => "Open",
    }
}
