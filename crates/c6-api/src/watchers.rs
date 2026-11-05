//! Watchers (docs/watchers.md): standing agents with their own agenda that
//! follow one session, the sessions matching labels, or everything, and act
//! on what they see as things happen. Their definitions are settings: the
//! ones carbon6 ships are pre-installed, and yours to change.

use serde::{Deserialize, Serialize};

use crate::UnixMs;

/// What a watcher is: its agenda (an editable prompt), what it follows,
/// what wakes it, what it may do, and what it may spend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WatcherDef {
    /// Its name (letters, digits, `-`); it routes as `task:watch:<name>`.
    pub name: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// The prompt that is its agenda (Settings → Prompts).
    pub agenda: String,
    /// `session`: one watcher session per watched session, ending with it;
    /// `all`: one for everything it follows.
    #[serde(default = "session")]
    pub scope: String,
    /// Labels a watched session must all have; empty: every session.
    #[serde(default)]
    pub applies_to: Vec<String>,
    /// What wakes it: `turn_end`, `correction`, `commit`, `edits`,
    /// `tool_failures`, `loop`, `session_end`, `signals`, `hourly`,
    /// `daily`, `weekly`.
    #[serde(default)]
    pub wakes: Vec<String>,
    /// What it may do: `note` (to the watched agent), `mark` (on the
    /// watched session), `lesson` (memory notes), `card` (task cards),
    /// `directive` (here / audit / from now on), `change` (a proposed
    /// change to a prompt, policy or routing rule), `ledger`, `doc`.
    #[serde(default)]
    pub outputs: Vec<String>,
    /// Findings at least this severe go to the watched agent as a note
    /// (`info`, `important`, `urgent`); the rest are marks.
    #[serde(default = "important")]
    pub note_severity: String,
    /// Notes to one watched session per hour, at most.
    #[serde(default = "three")]
    pub notes_per_hour: u32,
    /// Everything goes to you (marks and cards), nothing to the agent.
    #[serde(default)]
    pub to_you: bool,
    /// Tokens it may spend a day (0: no limit).
    #[serde(default)]
    pub daily_tokens: u64,
    /// The guard: tool calls matching this pattern are judged before they
    /// run. The improvement watcher: how many times a signal must happen.
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub threshold: u32,
    #[serde(default)]
    pub enabled: bool,
}

fn session() -> String {
    "session".into()
}
fn important() -> String {
    "important".into()
}
fn three() -> u32 {
    3
}

/// `GET /api/v1/watchers`: a watcher as it is, with how it's doing today.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Watcher {
    #[serde(flatten)]
    pub def: WatcherDef,
    /// carbon6 ships it.
    pub preinstalled: bool,
    /// You changed it (an update to the shipped one then leaves it alone).
    pub edited: bool,
    /// The shipped definition differs from yours (an update you can take).
    #[serde(default)]
    pub newer_default: bool,
    /// Wakes that called its model, and those that didn't (nothing it
    /// wakes on, nothing new, or over budget).
    pub woke: i64,
    pub skipped: i64,
    pub tokens_today: i64,
    /// Its sessions (one per watched session, or one).
    pub sessions: i64,
    pub findings: i64,
}

/// What a watcher found, and what was done with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Finding {
    pub id: i64,
    pub watcher: String,
    /// The watched session, if one.
    #[serde(default)]
    pub watched: String,
    /// The watcher's session, and its event where this was found.
    pub session_id: String,
    pub seq: i64,
    pub severity: String,
    pub kind: String,
    pub text: String,
    /// What happened: `sent` (a note to the agent), `marked`, `applied`,
    /// `asked` (a decision), `carded` (a task card), `ledger`, `over limit`.
    pub done: String,
    #[serde(default)]
    pub reference: String,
    pub at: UnixMs,
}

/// `GET /api/v1/sessions/{id}/watch`: who watches a session and what they found.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionWatch {
    /// Its watcher sessions: (watcher, session id).
    pub watchers: Vec<(String, String)>,
    pub findings: Vec<Finding>,
}

/// `GET /api/v1/watchers/findings` query.
#[derive(Clone, Debug, Default, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct FindingsQuery {
    #[serde(default)]
    pub watcher: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `POST /api/v1/watchers/{name}/run`: wake it now (for a session, or its
/// whole scope).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct WatcherRun {
    #[serde(default)]
    pub session: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn def(
    name: &str,
    title: &str,
    description: &str,
    scope: &str,
    applies_to: &[&str],
    wakes: &[&str],
    outputs: &[&str],
    daily_tokens: u64,
    enabled: bool,
) -> WatcherDef {
    WatcherDef {
        name: name.into(),
        title: title.into(),
        description: description.into(),
        agenda: format!("watch.{name}"),
        scope: scope.into(),
        applies_to: applies_to.iter().map(|s| s.to_string()).collect(),
        wakes: wakes.iter().map(|s| s.to_string()).collect(),
        outputs: outputs.iter().map(|s| s.to_string()).collect(),
        note_severity: important(),
        notes_per_hour: 3,
        to_you: false,
        daily_tokens,
        pattern: String::new(),
        threshold: 0,
        enabled,
    }
}

/// The watchers carbon6 ships (docs/watchers.md §9): stuck detection,
/// continuous retrospectives and review on; the weekly roll-up on; the
/// consistency watcher, the guard and the carbon6 improvement watcher off.
pub fn preinstalled() -> Vec<WatcherDef> {
    let mut guard = def(
        "guard",
        "Guard",
        "Judges risky tool calls before they run (pushing to main, deleting outside the folder, secrets, destructive migrations); a risky one waits for your approval.",
        "all",
        &[],
        &[],
        &[],
        200_000,
        false,
    );
    guard.pattern = r"git\s+push|rm\s+-[a-zA-Z]*r|--force|DROP\s+(TABLE|DATABASE)|TRUNCATE|reset\s+--hard|mkfs|dd\s+if=".into();
    let mut improve = def(
        "improve",
        "Improving carbon6",
        "Follows carbon6's own failures (its log, the web UI's crash reports, failed calls to its tools) and proposes task cards to fix the ones that keep happening.",
        "all",
        &[],
        &["signals", "hourly"],
        &["card", "doc"],
        300_000,
        false,
    );
    improve.threshold = 3;
    vec![
        def(
            "stuck",
            "Stuck detection",
            "Notices an agent looping on the same error or the same call, and nudges it (or tells you).",
            "session",
            &[],
            &["tool_failures", "loop"],
            &["note", "mark"],
            200_000,
            true,
        ),
        def(
            "retro",
            "Continuous retrospective",
            "After you correct an agent, proposes what carbon6 should remember or change so it goes better next time.",
            "session",
            &[],
            &["correction", "session_end"],
            &["lesson", "card", "directive", "change", "mark"],
            300_000,
            true,
        ),
        def(
            "review",
            "Review as it goes",
            "Reviews each step of changes against the work's goal and your standards, and tells the agent what to fix now.",
            "session",
            &["repo:"],
            &["commit", "edits"],
            &["note", "mark"],
            400_000,
            true,
        ),
        def(
            "retro-weekly",
            "Weekly retrospective",
            "Once a week: what the retrospectives found, what was applied and whether it helped, and notes to retire.",
            "all",
            &[],
            &["weekly"],
            &["doc", "change"],
            200_000,
            true,
        ),
        def(
            "consistency",
            "Consistency across sessions",
            "Keeps a decisions ledger per project or realm and tells sessions (and you) when one goes against it.",
            "session",
            &[],
            &["turn_end"],
            &["ledger", "note", "mark"],
            300_000,
            false,
        ),
        guard,
        improve,
    ]
}

/// The severities, least first.
pub const SEVERITIES: [&str; 3] = ["info", "important", "urgent"];

/// How severe, as a rank (unknown: info).
pub fn severity_rank(s: &str) -> usize {
    SEVERITIES.iter().position(|x| *x == s).unwrap_or(0)
}
