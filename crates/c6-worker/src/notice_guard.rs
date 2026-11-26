//! Rebuild the unattended-wake guards from the session journal.

use std::collections::{HashMap, HashSet, VecDeque};

use c6_proto::{SessionEvent, TaskNotice, session_event::Kind};

const HOUR_MS: i64 = 3_600_000;
pub(crate) const CONTINUED_TITLE: &str = "Notice wakes continued";
pub(crate) const WORKSPACE_TREE_TITLE: &str = "Notice guard workspace fingerprint";

#[derive(Default, Clone, Copy)]
pub(crate) struct Limits {
    pub tokens_per_hour: u64,
    pub evidence_observations: u32,
    pub failure_observations: u32,
    pub rounds: u32,
    pub max_seconds: u32,
}

impl Limits {
    fn observations(self, evidence: &'static str) -> u32 {
        if evidence == RETRY_EVIDENCE {
            if self.failure_observations == 0 { c6_api::DEFAULT_NOTICE_FAILURE_RELAUNCH_LIMIT } else { self.failure_observations }
        } else if self.evidence_observations == 0 {
            c6_api::DEFAULT_NOTICE_NO_PROGRESS_LIMIT
        } else {
            self.evidence_observations
        }
    }

    fn rounds(self) -> u32 {
        if self.rounds == 0 { c6_api::DEFAULT_NOTICE_LOOP_BACKOFF_ROUNDS } else { self.rounds.min(10) }
    }

    pub(crate) fn max_delay_ms(self) -> i64 {
        let cap = if self.max_seconds == 0 { c6_api::DEFAULT_NOTICE_LOOP_BACKOFF_MAX_SECONDS } else { self.max_seconds.min(3600) };
        i64::from(cap) * 1000
    }
}

const RETRY_EVIDENCE: &str = "the same failure after relaunching a task";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Guard {
    Proceed,
    Warn(String),
    Backoff { until_ms: i64, evidence: String },
    Pause(String),
}

#[derive(Clone)]
struct Report {
    source: String,
    body: String,
    failed: bool,
    peer: bool,
}

impl Report {
    fn repeats(&self, other: &Self) -> bool {
        self.source == other.source && self.failed == other.failed && similar(&self.body, &other.body)
    }
}

#[derive(Default)]
struct Chain<'a> {
    descriptions: HashSet<String>,
    seen_outcomes: HashSet<(&'static str, &'a str, &'a str)>,
    recent: VecDeque<Vec<Report>>,
    effects: HashSet<String>,
    failure_signatures: HashMap<String, String>,
    last_launches: HashSet<String>,
    previous_output: Option<String>,
    evidence: Option<(&'static str, u32, i64, String)>,
}

#[derive(Default)]
struct Wake {
    reports: Vec<Report>,
    distinct_input: bool,
    launches: HashSet<String>,
    new_description: bool,
    new_effect: bool,
    replied_to: HashSet<String>,
    output: String,
}

impl<'a> Chain<'a> {
    fn reset(&mut self) {
        // A later receipt or delivery of an old report is not a fresh outcome.
        *self = Self { seen_outcomes: std::mem::take(&mut self.seen_outcomes), ..Self::default() };
    }

    fn finish(&mut self, wake: Wake, ts_ms: i64) {
        let Wake { reports, distinct_input, launches, new_description, new_effect, replied_to, output } = wake;
        if !distinct_input && !new_effect && !new_description {
            return; // Another waiter delivered the same outcome, not another observation.
        }
        let output = normalized(&output);
        let unchanged_output = self.previous_output.as_ref().is_some_and(|old| similar(old, &output));
        let novel = |r: &Report| !self.recent.iter().flatten().any(|old| r.repeats(old));
        let duplicate = reports.iter().any(|r| !r.body.is_empty() && !novel(r));
        let retry_match = |r: &Report| {
            r.failed
                && (launches.contains(&r.source) || self.last_launches.contains(&r.source))
                && self.failure_signatures.get(&r.source).is_some_and(|old| old == &r.body)
        };
        let relaunch = reports.iter().any(retry_match);
        let new_report = reports.iter().any(|r| !r.body.is_empty() && novel(r) && !retry_match(r));
        let peer_reply = reports.iter().any(|r| r.peer && !novel(r) && replied_to.contains(&r.source));
        // A-B-A-B is also a loop when both sides report through different sessions.
        let alternating = if let [r] = reports.as_slice()
            && let (Some([a]), Some([b]), Some([c])) = (
                self.recent.back().map(Vec::as_slice),
                self.recent.iter().rev().nth(1).map(Vec::as_slice),
                self.recent.iter().rev().nth(2).map(Vec::as_slice),
            ) {
            (r.peer && a.peer && r.source != a.source && r.repeats(b) && a.repeats(c)).then(|| {
                let mut peers = [r.source.as_str(), a.source.as_str()];
                peers.sort_unstable();
                peers.join(" ↔ ")
            })
        } else {
            None
        };
        let evidence = if new_effect || new_description || new_report {
            None
        } else if relaunch {
            Some(RETRY_EVIDENCE)
        } else if peer_reply || alternating.is_some() {
            Some("repeated back-and-forth session messages")
        } else if duplicate && unchanged_output {
            Some("near-identical reports from the same source")
        } else if reports.iter().all(|r| r.body.is_empty()) && (output.is_empty() || unchanged_output) {
            Some("notice turns with no new result or observable effect")
        } else {
            None
        };
        self.evidence = evidence.map(|reason| {
            let report = reports
                .iter()
                .find(|r| (reason == RETRY_EVIDENCE && r.failed) || (reason != RETRY_EVIDENCE && !novel(r)))
                .or_else(|| reports.first());
            let source = alternating.unwrap_or_else(|| report.map_or_else(String::new, |r| r.source.clone()));
            let streak = self
                .evidence
                .as_ref()
                .filter(|(prior, _, _, old_source)| *prior == reason && old_source == &source)
                .map_or(1, |(_, n, _, _)| n.saturating_add(1));
            (reason, streak, ts_ms, source)
        });
        for report in &reports {
            if report.failed && !report.body.is_empty() {
                self.failure_signatures.insert(report.source.clone(), report.body.clone());
            } else {
                self.failure_signatures.remove(&report.source);
            }
        }
        self.recent.push_back(reports);
        self.last_launches = launches;
        self.previous_output = Some(output);
        if self.recent.len() > 8 {
            self.recent.pop_front();
        }
    }

    fn launch(&mut self, wake: Option<&mut Wake>, description: &str) {
        let description = normalized(description);
        if description.is_empty() {
            return;
        }
        if let Some(wake) = wake {
            wake.launches.insert(description.clone());
            wake.new_description |= self.descriptions.insert(description);
        } else {
            // A user-started launch seeds the chain but isn't an unattended retry.
            self.descriptions.insert(description);
        }
    }

    fn success(&mut self, wake: Option<&mut Wake>, description: &str) {
        let description = normalized(description);
        if let Some(wake) = wake
            && (self.failure_signatures.contains_key(&description) || wake.reports.iter().any(|r| r.source == description && r.failed))
        {
            wake.new_effect = true;
            wake.reports.retain(|r| r.source != description || !r.failed);
        }
        self.failure_signatures.remove(&description);
    }
}

fn outcome_key<'a>(command_id: &'a str, notice: &'a TaskNotice) -> (&'static str, &'a str, &'a str) {
    let (kind, id) = command_id
        .strip_prefix("task:")
        .and_then(|s| s.rsplit_once(':'))
        .map_or_else(|| report_id(command_id, &notice.session_id), |(task, _)| ("task", task));
    (kind, id, &notice.status)
}

fn report_id<'a>(command_id: &'a str, child_id: &'a str) -> (&'static str, &'a str) {
    // Local deliveries and receipts share an ID. Remote deliveries add one
    // more `subagent:` prefix and may report one turn to multiple waiters.
    if command_id.is_empty() {
        return ("child", child_id);
    }
    let id = command_id.trim_start_matches("subagent:");
    if let Some(incident) = liveness_report_id(id, child_id) {
        return ("report", incident);
    }
    if !child_id.is_empty()
        && let Some(tail) = id.strip_prefix(child_id).and_then(|s| s.strip_prefix(':'))
        && let Some((seq, _)) = tail.split_once(':')
        && seq.parse::<u64>().is_ok()
    {
        return ("report", &id[..child_id.len() + 1 + seq.len()]);
    }
    ("report", id)
}

/// A liveness command's stable incident ID, without its waiter suffix or
/// the worker's notice prefix. Keep the child ID intact (it may contain ':').
pub(crate) fn liveness_report_id<'a>(command_id: &'a str, child_id: &str) -> Option<&'a str> {
    let id = command_id.trim_start_matches("subagent:");
    let prefix = "liveness:lost:";
    let tail = id.strip_prefix(prefix)?.strip_prefix(child_id)?.strip_prefix(':')?;
    let incident = tail.split_once(':').map_or(tail, |(incident, _)| incident);
    (!child_id.is_empty() && !incident.is_empty()).then(|| &id[..prefix.len() + child_id.len() + 1 + incident.len()])
}

/// A structured loss already represents these exact background waiters.
/// Their later task terminal events are lifecycle records, not new outcomes.
pub(crate) fn liveness_covered_tasks(events: &[SessionEvent]) -> HashSet<&str> {
    let mut notices = HashSet::new();
    let mut links = HashMap::new();
    let mut not_yet_linked = HashSet::new();
    let mut covered = HashSet::new();
    for e in events {
        match &e.kind {
            Some(Kind::SubagentSpawned(s)) if !s.call_id.is_empty() => {
                let call = (s.child_session_id.as_str(), s.call_id.as_str());
                let task = s.background_task_id.as_str();
                links.insert(call, task);
                if not_yet_linked.remove(&call) && !task.is_empty() {
                    covered.insert(task);
                }
            }
            Some(Kind::NoticeQueued(n)) => {
                if let Some(t) = &n.task_notice
                    && t.status == "lost"
                    && let Some(id) = liveness_report_id(&n.command_id, &t.session_id)
                {
                    notices.insert(id);
                }
            }
            Some(Kind::SubagentReported(r))
                if r.status == "lost"
                    && !r.call_id.is_empty()
                    && liveness_report_id(&r.command_id, &r.child_session_id).is_some_and(|id| notices.contains(id)) =>
            {
                let call = (r.child_session_id.as_str(), r.call_id.as_str());
                match links.get(&call) {
                    Some(&task) if !task.is_empty() => {
                        covered.insert(task);
                    }
                    None => {
                        not_yet_linked.insert(call);
                    } // The head replied before the launch link was journaled.
                    _ => {}
                }
            }
            _ => {}
        }
    }
    covered
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn failed(status: &str) -> bool {
    matches!(status, "failed" | "lost" | "error")
}

fn failure_signature(text: &str) -> String {
    let mut skip_pid = false;
    text.split_whitespace()
        .filter(|word| {
            let word = word.trim_matches(['[', ']', '"', '\'']);
            if std::mem::take(&mut skip_pid) {
                return false;
            }
            let lower = word.to_ascii_lowercase();
            if matches!(lower.as_str(), "pid" | "pid:") {
                skip_pid = true;
                return false;
            }
            let bytes = word.as_bytes();
            let date = bytes.len() >= 10 && bytes[..4].iter().all(u8::is_ascii_digit) && bytes[4] == b'-' && bytes[7] == b'-';
            let time = bytes.len() >= 8 && bytes[..2].iter().all(u8::is_ascii_digit) && bytes[2] == b':' && bytes[5] == b':';
            !date && !time && !lower.starts_with("pid=") && !lower.starts_with("elapsed=") && uuid::Uuid::parse_str(word).is_err()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn report_markdown(markdown: &str, status: &str) -> String {
    let markdown = markdown.trim();
    if matches!(status, "completed" | "finished")
        && let Some((body, footer)) = markdown.rsplit_once("\n\n[subagent session ")
        && footer.strip_suffix(']').is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
    {
        return normalized(body);
    }
    if failed(status)
        && let Some((header, body)) = markdown.split_once('\n')
        && let Some((id, attrs)) = header.strip_prefix("<subagent id=\"").and_then(|s| s.split_once("\" status=\""))
        && uuid::Uuid::parse_str(id).is_ok()
        && attrs.contains(" elapsed=\"")
        && body.contains("</subagent>")
    {
        return normalized(&format!("{status}\n{body}"));
    }
    normalized(markdown)
}

fn similar(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // Small edits to long reports should not manufacture new work. Preserve
    // number changes (test counts, exit codes, versions) as possible progress.
    let words = |s: &str| s.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase()).collect::<Vec<_>>();
    let (a, b) = (words(a), words(b));
    if a == b {
        return true;
    }
    let significant =
        |w: &&String| w.chars().any(|c| c.is_ascii_digit()) || matches!(w.as_str(), "no" | "not" | "never" | "failed" | "succeeded");
    if a.iter().filter(significant).ne(b.iter().filter(significant)) {
        return false;
    }
    // Compare adjacent word pairs rather than treating reordered text as identical.
    let (a, b): (HashSet<_>, HashSet<_>) = (a.windows(2).collect(), b.windows(2).collect());
    a.len().min(b.len()) >= 3 && a.intersection(&b).count() * 200 >= (a.len() + b.len()) * 80
}

fn notice_report(
    command_id: &str,
    source_session: &str,
    text: &str,
    notice: Option<&TaskNotice>,
    children: &HashMap<&str, &str>,
    backgrounds: &HashMap<&str, &str>,
) -> Option<Report> {
    if let Some(n) = notice {
        let task = command_id.strip_prefix("task:").and_then(|id| id.rsplit_once(':'));
        let source = children.get(n.session_id.as_str()).or_else(|| task.and_then(|(id, _)| backgrounds.get(id)));
        let title = if n.title.is_empty() { n.session_id.as_str() } else { n.title.as_str() };
        let body = report_markdown(&n.markdown, &n.status);
        Some(Report {
            source: normalized(source.copied().unwrap_or(title)),
            body: if failed(&n.status) { failure_signature(&body) } else { body },
            failed: failed(&n.status),
            peer: false,
        })
    } else if !source_session.is_empty() {
        Some(Report { source: normalized(source_session), body: normalized(text), failed: false, peer: true })
    } else {
        None
    }
}

/// Explain why the *next* unattended wake should be held, if any. `events`
/// are in journal order. A direct input resets progress/retry accounting, not
/// rolling spending. A quiet Continue grants a fresh spending baseline:
/// usage before its journal position is not charged again.
///
/// A newly observed checkpoint tree is evidence of progress since the last
/// snapshot. Commit IDs alone are never evidence (legacy logs may lack trees).
pub(crate) fn evaluate(events: &[SessionEvent], now_ms: i64, limits: Limits, eligible: &HashSet<String>) -> Guard {
    let effective;
    let events = if events.iter().any(|e| matches!(&e.kind, Some(Kind::Rewound(_)))) {
        effective = c6_agent::effective_events(events);
        effective.as_slice()
    } else {
        events
    };
    // A side chat can inherit history. Its parent is not spending this chat's budget.
    let events = &events[events.iter().rposition(|e| matches!(&e.kind, Some(Kind::Started(_)))).unwrap_or(0)..];
    let structured_losses = liveness_covered_tasks(events);

    let mut chain = Chain::default();
    let mut notices = HashMap::new();
    let mut consumed = HashSet::new();
    let mut child_descriptions = HashMap::new();
    let mut background_descriptions = HashMap::new();
    let mut launches = HashSet::new();
    let mut notice_input = false;
    let mut direct_input = false;
    let mut pending_reports = Vec::new();
    let mut pending_distinct_input = false;
    let mut pending_tools = HashMap::new();
    let mut checkpoint_tree = None;
    let mut active = None;
    let mut notice_started = false;
    let mut wake: Option<Wake> = None;
    let mut tokens = 0u64;
    let cutoff = now_ms.saturating_sub(HOUR_MS);

    for event in events {
        match &event.kind {
            Some(Kind::Notice(n)) if n.quiet && n.title == CONTINUED_TITLE => {
                chain.reset();
                wake = None;
                tokens = 0;
            }
            Some(Kind::Notice(n)) if n.quiet && n.title == WORKSPACE_TREE_TITLE && !n.message.is_empty() => {
                if checkpoint_tree.replace(n.message.as_str()).is_none_or(|old| old != n.message) {
                    chain.evidence = None;
                    if let Some(wake) = &mut wake {
                        wake.new_effect = true;
                    }
                }
            }
            Some(Kind::NoticeQueued(n)) if !n.command_id.is_empty() => {
                notices.insert(n.command_id.as_str(), n);
            }
            Some(Kind::Checkpoint(c)) if c.error.is_empty() && !c.tree_id.is_empty() => {
                if checkpoint_tree.replace(c.tree_id.as_str()).is_some_and(|old| old != c.tree_id) {
                    chain.evidence = None;
                    if let Some(wake) = &mut wake {
                        wake.new_effect = true;
                    }
                }
            }
            Some(Kind::PlaceChanged(_)) => checkpoint_tree = None,
            Some(Kind::User(u)) if u.blocked.is_empty() => {
                let queued = notices.get(u.command_id.as_str());
                consumed.insert(u.command_id.as_str());
                // The CLI logs its own task notifications without NoticeQueued.
                // Only a notification before the turn can establish its start.
                let native_report = u.command == "task-notification"
                    && !u.human_origin
                    && u.api_origin.is_none()
                    && (u.origin_recorded || u.task_notice.is_some());
                // Explicit automation wins over absent or ambiguous provenance
                // (including legacy records). API input with no automation
                // markers is still an intentional direct session input.
                let notice = queued.is_some() || native_report || !u.from_session_id.is_empty();
                if notice {
                    if active.is_none() {
                        notice_input = true;
                    }
                    let source_session = queued.map_or(u.from_session_id.as_str(), |q| q.from_session_id.as_str());
                    let task_notice = u.task_notice.as_ref().or_else(|| queued.and_then(|q| q.task_notice.as_ref()));
                    if let Some(n) = task_notice
                        && let key = outcome_key(&u.command_id, n)
                        && !key.1.is_empty()
                        && !chain.seen_outcomes.insert(key)
                    {
                        continue;
                    }
                    let report =
                        notice_report(&u.command_id, source_session, &u.text, task_notice, &child_descriptions, &background_descriptions);
                    if active.is_none() {
                        pending_distinct_input = true;
                        pending_reports.extend(report);
                    } else if let Some(wake) = &mut wake {
                        wake.distinct_input = true;
                        wake.reports.extend(report);
                    }
                } else if u.delegated_by.is_empty() && u.command != "stop-hook" {
                    chain.reset();
                    wake = None;
                    pending_reports.clear();
                    pending_distinct_input = false;
                    if active.is_none() {
                        direct_input = true;
                    }
                }
            }
            Some(Kind::TurnStarted(t)) => {
                // A start also bounds an older incomplete turn in legacy logs.
                if let Some(previous) = wake.take() {
                    chain.finish(previous, event.ts_ms);
                }
                active = Some(t.turn);
                notice_started = notice_input && !direct_input;
                let reports = std::mem::take(&mut pending_reports);
                let distinct_input = std::mem::take(&mut pending_distinct_input);
                wake = notice_started.then_some(Wake { reports, distinct_input, ..Default::default() });
                pending_tools.clear();
                notice_input = false;
                direct_input = false;
            }
            Some(Kind::TurnEnded(t)) if active == Some(t.turn) => {
                if let Some(wake) = wake.take() {
                    chain.finish(wake, event.ts_ms);
                }
                active = None;
                notice_started = false;
            }
            Some(Kind::Usage(u)) if !u.keep_warm && notice_started && event.ts_ms > cutoff && event.ts_ms <= now_ms => {
                // Cache reads/writes are separate usage fields; add them as
                // context_tokens_from_events does. Reasoning is part of output.
                tokens = tokens
                    .saturating_add(u.input_tokens)
                    .saturating_add(u.cache_read_tokens)
                    .saturating_add(u.cache_write_tokens)
                    .saturating_add(u.output_tokens);
            }
            Some(Kind::SubagentSpawned(s)) => {
                if !s.child_session_id.is_empty() && !s.description.trim().is_empty() {
                    child_descriptions.insert(s.child_session_id.as_str(), s.description.as_str());
                }
                // A backgrounded subagent also has a BackgroundTask start.
                // Count the pair once. A new foreground call can relaunch an
                // existing child, so prefer its call ID over the child ID.
                let key = if !s.background_task_id.is_empty() {
                    ("background", s.background_task_id.as_str())
                } else if !s.call_id.is_empty() {
                    ("call", s.call_id.as_str())
                } else {
                    ("child", s.child_session_id.as_str())
                };
                if !s.description.trim().is_empty() && (key.1.is_empty() || launches.insert(key)) {
                    chain.launch(wake.as_mut(), &s.description);
                }
            }
            Some(Kind::BackgroundTask(t)) if t.state == "started" => {
                if !t.description.trim().is_empty() {
                    background_descriptions.insert(t.task_id.as_str(), t.description.as_str());
                    if t.task_id.is_empty() || launches.insert(("background", t.task_id.as_str())) {
                        chain.launch(wake.as_mut(), &t.description);
                    }
                }
            }
            Some(Kind::BackgroundTask(t)) => {
                if structured_losses.contains(t.task_id.as_str()) {
                    continue;
                }
                let description = background_descriptions.get(t.task_id.as_str()).copied().unwrap_or(t.description.as_str());
                let status = if t.state == "finished" && t.exit_code != 0 { "failed" } else { t.state.as_str() };
                if status == "finished" {
                    chain.success(wake.as_mut(), description);
                }
            }
            Some(Kind::Assistant(a)) if wake.is_some() => {
                if let Some(wake) = &mut wake {
                    for b in &a.blocks {
                        match &b.block {
                            Some(c6_proto::content_block::Block::ToolUse(t)) => {
                                pending_tools.insert(t.id.as_str(), t);
                            }
                            Some(c6_proto::content_block::Block::Text(text)) if wake.output.len() < 8192 => {
                                wake.output.push(' ');
                                wake.output.push_str(&text[..text.floor_char_boundary(text.len().min(8192 - wake.output.len()))]);
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some(Kind::ToolResult(result)) if !result.is_error && !result.truncated => {
                if let Some(wake) = &mut wake
                    && let Some(call) = pending_tools.remove(result.call_id.as_str())
                {
                    let name = call.name.strip_prefix("mcp__carbon6__").unwrap_or(&call.name);
                    if matches!(name, "MessageSession" | "message_session") {
                        if let Ok(input) = serde_json::from_str::<serde_json::Value>(&call.input_json)
                            && let Some(peer) = input["to"].as_str()
                        {
                            wake.replied_to.insert(peer.into());
                        }
                    } else if effectful_tool(name, &call.input_json) {
                        wake.new_effect |= chain.effects.insert(format!("{name}\n{}", call.input_json));
                    }
                }
            }
            _ => {}
        }
    }

    if limits.tokens_per_hour > 0 && tokens >= limits.tokens_per_hour {
        return Guard::Pause(format!("paused: spending limit ({tokens} tokens this hour)"));
    }
    let Some((reason, count, ts_ms, source)) = chain.evidence else { return Guard::Proceed };
    let Some(level) = count.checked_sub(limits.observations(reason)) else { return Guard::Proceed };
    // Fresh, substantive work can wake an otherwise stalled session. A queued
    // snapshot is an input until its User receipt, not a separate observation.
    if notices.values().any(|n| {
        eligible.contains(n.command_id.as_str())
            && n.wake
            && !consumed.contains(n.command_id.as_str())
            && !n.task_notice.as_ref().is_some_and(|notice| chain.seen_outcomes.contains(&outcome_key(&n.command_id, notice)))
            && notice_report(
                &n.command_id,
                &n.from_session_id,
                &n.text,
                n.task_notice.as_ref(),
                &child_descriptions,
                &background_descriptions,
            )
            .is_some_and(|r| !r.body.is_empty() && !chain.recent.iter().flatten().any(|old| r.repeats(old)))
    }) {
        return Guard::Proceed;
    }
    let source: String = source.chars().filter(|c| !c.is_control()).take(96).collect();
    let source = if source.is_empty() { String::new() } else { format!("; source: {source}") };
    let plural = if count == 1 { "" } else { "s" };
    let evidence = format!("{reason} ({count} observation{plural}{source})");
    if level == 0 {
        Guard::Warn(evidence)
    } else if level <= limits.rounds() {
        let delay = (60_000i64 << (level - 1)).min(limits.max_delay_ms());
        Guard::Backoff { until_ms: ts_ms.saturating_add(delay), evidence }
    } else {
        Guard::Pause(format!("paused: still repeating {evidence}"))
    }
}

fn effectful_tool(name: &str, input: &str) -> bool {
    let name: String = name.chars().filter(char::is_ascii_alphanumeric).flat_map(char::to_lowercase).collect();
    match name.as_str() {
        "edit" | "write" | "multiedit" | "applypatch" | "recordsput" | "recordsdelete" | "seriesrecord" | "claimadd" | "claimretract"
        | "thingcreate" | "memexnote" | "remember" | "forget" | "knowledgerecord" | "publishfile" | "appcall" => true,
        "work" | "doc" | "carbon6" => {
            let Ok(input) = serde_json::from_str::<serde_json::Value>(input) else { return false };
            input["action"].as_str().is_some_and(|action| {
                matches!(
                    action,
                    "create"
                        | "edit"
                        | "attach"
                        | "todo"
                        | "todo_done"
                        | "todo_edit"
                        | "todo_remove"
                        | "propose"
                        | "comment"
                        | "archive"
                        | "unarchive"
                )
            })
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c6_proto::{
        BackgroundTask, Notice, NoticeQueued, Rewound, SubagentReported, SubagentSpawned, TurnEnded, TurnStarted, Usage, UserMessage,
    };

    const NOW: i64 = 10 * HOUR_MS;
    const TEST_LIMITS: Limits = Limits { evidence_observations: 1, failure_observations: 1, rounds: 0, max_seconds: 0, tokens_per_hour: 0 };

    #[test]
    fn keep_warm_does_not_spend_or_reset_an_incomplete_notice_turn() {
        let mut log = Log::default();
        log.notice("Build", "completed", "The build finished.");
        log.start();
        log.usage(3, 0, 0, 0);
        let before = log.guard(Limits { tokens_per_hour: 10, ..TEST_LIMITS });
        log.push(Kind::Usage(Usage { input_tokens: 100_000, cache_read_tokens: 1_000_000, keep_warm: true, ..Default::default() }));
        for title in ["Cache maintenance attempted", "Kept cache warm"] {
            log.push(Kind::Notice(Notice { title: title.into(), quiet: true, ..Default::default() }));
        }
        assert_eq!(log.guard(Limits { tokens_per_hour: 10, ..TEST_LIMITS }), before);
        log.usage(10, 0, 0, 0);
        assert!(log.reason(10).is_some_and(|reason| reason.contains("spending limit")));
    }

    #[test]
    fn error_signatures_ignore_timestamps_and_process_ids_but_keep_exit_codes() {
        let first = failure_signature("[2026-11-23T18:00:00Z] pid=1234 compiler failed with exit code 101 elapsed=3s");
        let second = failure_signature("[2026-11-23T18:01:00Z] pid=5678 compiler failed with exit code 101 elapsed=9s");
        assert_eq!(first, second);
        assert_ne!(first, failure_signature("compiler failed with exit code 137"));
        assert_eq!(failure_signature("PID: 123 compiler failed"), "compiler failed");
    }

    #[test]
    fn distinct_successful_record_writes_are_effects_not_a_notice_loop() {
        let mut log = Log::default();
        for i in 0..12 {
            log.notice("watcher", "finished", "The same recurring event.");
            log.start();
            log.output("Recorded the event.");
            log.push(Kind::Assistant(c6_proto::AssistantMessage {
                blocks: vec![c6_proto::ContentBlock {
                    block: Some(c6_proto::content_block::Block::ToolUse(c6_proto::ToolUse {
                        id: format!("write-{i}"),
                        name: "records_put".into(),
                        input_json: format!(r#"{{"fields":{{"event":{i}}}}}"#),
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }));
            log.push(Kind::ToolResult(c6_proto::ToolResult {
                call_id: format!("write-{i}"),
                output: "saved".into(),
                ..Default::default()
            }));
            log.end();
            assert_eq!(log.guard(TEST_LIMITS), Guard::Proceed);
        }
    }

    #[derive(Default)]
    struct Log(Vec<SessionEvent>);

    impl Log {
        fn push(&mut self, kind: Kind) {
            self.0.push(SessionEvent { seq: self.0.len() as u64 + 1, ts_ms: NOW, kind: Some(kind) });
        }

        fn user(&mut self, id: &str) {
            self.push(Kind::User(UserMessage { command_id: id.into(), ..Default::default() }));
        }

        fn notice(&mut self, title: &str, status: &str, markdown: &str) {
            let id = format!("notice-{}", self.0.len());
            self.push(Kind::NoticeQueued(NoticeQueued {
                command_id: id.clone(),
                task_notice: Some(TaskNotice {
                    title: title.into(),
                    status: status.into(),
                    markdown: markdown.into(),
                    ..Default::default()
                }),
                ..Default::default()
            }));
            self.push(Kind::User(UserMessage { command_id: id.clone(), text: format!("Changing wrapper for {id}"), ..Default::default() }));
        }

        fn start(&mut self) {
            self.push(Kind::TurnStarted(TurnStarted::default()));
        }

        fn end(&mut self) {
            self.push(Kind::TurnEnded(TurnEnded::default()));
        }

        fn output(&mut self, text: &str) {
            self.push(Kind::Assistant(c6_proto::AssistantMessage {
                blocks: vec![c6_proto::ContentBlock {
                    block: Some(c6_proto::content_block::Block::Text(text.into())),
                    ..Default::default()
                }],
                ..Default::default()
            }));
        }

        fn usage(&mut self, input: u64, output: u64, cache_read: u64, cache_write: u64) {
            self.push(Kind::Usage(Usage {
                input_tokens: input,
                output_tokens: output,
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
                ..Default::default()
            }));
        }

        fn spawn(&mut self, id: &str, description: &str, background: &str) {
            self.push(Kind::SubagentSpawned(SubagentSpawned {
                child_session_id: id.into(),
                description: description.into(),
                background_task_id: background.into(),
                ..Default::default()
            }));
        }

        fn queue_subagent(&mut self, id: &str, child: &str, title: &str, status: &str) {
            self.push(Kind::NoticeQueued(NoticeQueued {
                command_id: id.into(),
                wake: true,
                task_notice: Some(TaskNotice {
                    kind: "subagent".into(),
                    session_id: child.into(),
                    title: title.into(),
                    status: status.into(),
                    markdown: "Worker stopped before an outcome was recorded.".into(),
                }),
                ..Default::default()
            }));
        }

        fn receipt(&mut self, id: &str, child: &str, status: &str) {
            self.push(Kind::SubagentReported(SubagentReported {
                command_id: id.into(),
                child_session_id: child.into(),
                status: status.into(),
                ..Default::default()
            }));
        }

        fn background(&mut self, id: &str, description: &str, state: &str, exit_code: i32) {
            self.push(Kind::BackgroundTask(BackgroundTask {
                task_id: id.into(),
                description: description.into(),
                state: state.into(),
                exit_code,
                ..Default::default()
            }));
        }

        fn continuance(&mut self, quiet: bool) {
            self.push(Kind::Notice(Notice { title: CONTINUED_TITLE.into(), quiet, ..Default::default() }));
        }

        fn guard(&self, limits: Limits) -> Guard {
            evaluate(&self.0, NOW, limits, &HashSet::new())
        }

        fn reason(&self, tokens_per_hour: u64) -> Option<String> {
            match self.guard(Limits { tokens_per_hour, ..TEST_LIMITS }) {
                Guard::Pause(reason) => Some(reason),
                _ => None,
            }
        }
    }

    #[test]
    fn structured_loss_covers_only_its_waiter_and_survives_generic_terminal_event() {
        let mut log = Log::default();
        log.user("start");
        let link = |task: &str| {
            Kind::SubagentSpawned(SubagentSpawned {
                child_session_id: "child:remote".into(),
                call_id: "call".into(),
                background_task_id: task.into(),
                description: "Check server".into(),
                ..Default::default()
            })
        };
        log.push(link("bg1"));
        log.push(link("bg2"));
        log.queue_subagent("subagent:liveness:lost:child:remote:incident-a", "child:remote", "Check server", "lost");
        log.push(Kind::SubagentReported(SubagentReported {
            command_id: "liveness:lost:child:remote:incident-a:call".into(),
            child_session_id: "child:remote".into(),
            call_id: "call".into(),
            status: "lost".into(),
        }));
        log.push(Kind::BackgroundTask(BackgroundTask {
            task_id: "bg2".into(),
            kind: "agent".into(),
            description: "Check server".into(),
            state: "finished".into(),
            exit_code: 1,
            ..Default::default()
        }));
        assert_eq!(liveness_covered_tasks(&log.0), HashSet::from(["bg2"]));
        assert_eq!(log.reason(100), None, "the notice and receipt are one loss");
        log.queue_subagent("subagent:liveness:lost:child:remote:incident-b", "child:remote", "Check server", "lost");
        assert_eq!(log.guard(TEST_LIMITS), Guard::Proceed, "queued losses do not hard-stop the scheduler");
    }

    #[test]
    fn loss_retry_warns_after_matching_failure_and_never_hard_stops() {
        let mut log = Log::default();
        log.user("start");
        log.start();
        log.spawn("first", "Check  server", "");
        log.end();

        let first = "subagent:subagent:first:7:first-call";
        log.queue_subagent(first, "first", "Different display title", "lost");
        assert_eq!(log.reason(100), None, "the first loss can wake the retry");
        log.receipt("subagent:first:7:first-call", "first", "lost");
        log.receipt("subagent:first:7:another-waiter", "first", "lost");
        log.queue_subagent(first, "first", "Different display title", "lost");
        log.user(first); // Delivery of a queued notice is logged as User.
        assert_eq!(log.reason(100), None, "the queue, receipt and delivery are one loss");

        log.start();
        log.spawn("retry", "Check server", "");
        log.end();
        let retry = "subagent:local:retry:4";
        log.queue_subagent(retry, "retry", "Another title", "lost");
        assert_eq!(log.guard(TEST_LIMITS), Guard::Proceed);
        log.user(retry);
        log.start();
        log.end();
        assert_eq!(log.guard(TEST_LIMITS), Guard::Warn(format!("{RETRY_EVIDENCE} (1 observation; source: Check server)")));

        log.user("intervene");
        log.receipt("subagent:local:retry:4", "retry", "lost");
        log.queue_subagent("subagent:local:third:4", "third", "Check server", "lost");
        assert_eq!(log.reason(100), None, "direct input clears the streak even if an old receipt arrives later");
    }

    #[test]
    fn liveness_loss_counts_once_across_waiters() {
        let mut log = Log::default();
        let child = "child:remote";
        log.spawn(child, "Check server", "");
        log.receipt("liveness:lost:child:remote:ad1887cb-6c3d-435f-a642-269ce4fcabc2:first-call", child, "lost");
        log.queue_subagent(
            "subagent:liveness:lost:child:remote:ad1887cb-6c3d-435f-a642-269ce4fcabc2:second-call",
            child,
            "Different display title",
            "lost",
        );
        assert_eq!(log.reason(100), None, "different waiters reported the same incident");

        log.user("subagent:liveness:lost:child:remote:ad1887cb-6c3d-435f-a642-269ce4fcabc2:second-call");
        log.start();
        log.end();
        let other_waiter = "subagent:liveness:lost:child:remote:ad1887cb-6c3d-435f-a642-269ce4fcabc2:third-call";
        log.queue_subagent(other_waiter, child, "Different display title", "lost");
        log.user(other_waiter);
        log.start();
        log.end();
        assert_eq!(log.guard(TEST_LIMITS), Guard::Proceed, "a second delivery of the incident is not a second observation");
    }

    #[test]
    fn background_subagent_loss_and_its_notice_count_once() {
        let mut log = Log::default();
        let lost_task = |id: &str| BackgroundTask {
            task_id: id.into(),
            description: "Check server".into(),
            kind: "claude_code".into(),
            native_type: "local_agent".into(),
            state: "lost".into(),
            ..Default::default()
        };
        log.push(Kind::BackgroundTask(lost_task("one")));
        log.queue_subagent("task:one:lost", "", "Check server", "lost");
        assert_eq!(log.reason(100), None);
        log.push(Kind::BackgroundTask(lost_task("two")));
        assert_eq!(log.guard(TEST_LIMITS), Guard::Proceed, "lifecycle events alone are not a loop");
    }

    #[test]
    fn productive_fanout_is_not_limited_by_wake_count() {
        let mut log = Log::default();
        log.user("start");
        log.start();
        log.spawn("initial", "root", "");
        log.end();
        for i in 0..20 {
            log.notice("root", "completed", "Same report body");
            log.start();
            log.spawn(&format!("child-{i}"), &format!("Investigate component {i}"), "");
            log.end();
            assert_eq!(log.guard(Limits::default()), Guard::Proceed);
        }
    }

    #[test]
    fn distinct_expected_slices_with_identical_reports_are_progress() {
        let mut log = Log::default();
        log.user("audit every component");
        log.start();
        for i in 0..20 {
            log.spawn(&format!("child-{i}"), &format!("Audit component {i}"), "");
        }
        log.end();
        for i in 0..20 {
            log.notice(&format!("Audit component {i}"), "completed", "No issues found.");
            log.start();
            log.usage(9_000_000, 1_000_000, 0, 0);
            log.output("Done.");
            log.end();
            assert_eq!(
                log.guard(Limits::default()),
                Guard::Proceed,
                "distinct user-requested slices are productive even with identical findings"
            );
        }
    }

    #[test]
    fn near_identical_input_and_output_warn_then_back_off_then_pause() {
        const EVIDENCE: &str = "near-identical reports from the same source";
        let mut log = Log::default();
        for i in 0..=5 {
            let prefix = if i % 2 == 0 { "Found" } else { "Observed" };
            log.notice("same source", "finished", &format!("{prefix} no changes to the running service during the scheduled check."));
            log.start();
            log.output(&format!("{prefix} nothing new for the service and no action is needed right now."));
            log.end();
            let guard = log.guard(TEST_LIMITS);
            let plural = if i == 1 { "" } else { "s" };
            let evidence = format!("{EVIDENCE} ({i} observation{plural}; source: same source)");
            match i {
                0 => assert_eq!(guard, Guard::Proceed),
                1 => assert_eq!(guard, Guard::Warn(evidence)),
                2..=4 => {
                    let expected = Guard::Backoff { until_ms: NOW + (60_000 << (i - 2)), evidence: evidence.clone() };
                    assert_eq!(guard, expected);
                    assert_eq!(
                        evaluate(&log.0, NOW + 50_000, TEST_LIMITS, &HashSet::new()),
                        expected,
                        "rescheduling must not restart the delay"
                    );
                    let capped = Limits { max_seconds: 80, ..TEST_LIMITS };
                    assert_eq!(log.guard(capped), Guard::Backoff { until_ms: NOW + (60_000 << (i - 2)).min(80_000), evidence });
                }
                _ => assert_eq!(guard, Guard::Pause(format!("paused: still repeating {evidence}"))),
            }
        }
        let mut direct = Log(log.0.clone());
        direct.user("new user input");
        assert_eq!(direct.guard(TEST_LIMITS), Guard::Proceed);

        let queue = |title: &str, body: &str| {
            Kind::NoticeQueued(NoticeQueued {
                command_id: format!("pending-{title}"),
                wake: true,
                task_notice: Some(TaskNotice {
                    title: title.into(),
                    status: "finished".into(),
                    markdown: body.into(),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };
        log.push(queue("same source", "Found no changes to the running service during the scheduled check."));
        let mut eligible = HashSet::from(["pending-same source".into()]);
        assert!(
            matches!(evaluate(&log.0, NOW, TEST_LIMITS, &eligible), Guard::Pause(_)),
            "an eligible but repeated pending report does not bypass the guard"
        );
        log.push(queue("new work", "New service deployed successfully."));
        assert!(
            matches!(evaluate(&log.0, NOW, TEST_LIMITS, &eligible), Guard::Pause(_)),
            "a new report behind a group barrier cannot bypass the guard"
        );
        eligible.insert("pending-new work".into());
        assert_eq!(
            evaluate(&log.0, NOW, TEST_LIMITS, &eligible),
            Guard::Proceed,
            "eligible new work releases the hold before it is delivered"
        );
    }

    #[test]
    fn automated_continuation_is_neither_a_direct_reset_nor_a_notice_wake() {
        let mut log = Log::default();
        for _ in 0..2 {
            log.notice("review", "finished", "Same result.");
            log.start();
            log.output("Same reply.");
            log.end();
        }
        let warning = log.guard(TEST_LIMITS);
        assert!(matches!(warning, Guard::Warn(_)));
        log.push(Kind::User(UserMessage {
            command_id: "delegated-continuation".into(),
            delegated_by: "parent-call".into(),
            origin_recorded: true,
            ..Default::default()
        }));
        assert_eq!(log.guard(TEST_LIMITS), warning);
        log.start();
        log.usage(1, 1, 0, 0);
        log.end();
        assert_eq!(log.guard(Limits { tokens_per_hour: 2, ..TEST_LIMITS }), warning);
        log.user("direct-input");
        assert_eq!(log.guard(TEST_LIMITS), Guard::Proceed);
    }

    #[test]
    fn duplicate_input_with_different_output_is_not_the_same_loop() {
        let mut log = Log::default();
        for i in 0..8 {
            log.notice("same source", "finished", "Report unchanged");
            log.start();
            log.output(&format!("Now working on component {i}."));
            log.end();
            assert_eq!(log.guard(Limits::default()), Guard::Proceed);
        }
    }

    #[test]
    fn changed_checkpoint_tree_breaks_repetition_but_new_commit_does_not() {
        let mut log = Log::default();
        let checkpoint = |commit: &str, tree_id: &str| {
            Kind::Checkpoint(c6_proto::Checkpoint { commit: commit.into(), tree_id: tree_id.into(), ..Default::default() })
        };
        log.push(checkpoint("commit-1", "tree-1"));
        for _ in 0..2 {
            log.notice("same source", "finished", "Report unchanged");
            log.start();
            log.output("Done.");
            log.end();
        }
        let warning = log.guard(TEST_LIMITS);
        assert!(matches!(warning, Guard::Warn(_)));
        log.push(checkpoint("commit-2", "tree-1"));
        assert_eq!(log.guard(TEST_LIMITS), warning);
        log.push(checkpoint("legacy-commit", ""));
        assert_eq!(log.guard(TEST_LIMITS), warning);
        log.push(checkpoint("commit-3", "tree-2"));
        assert_eq!(log.guard(TEST_LIMITS), Guard::Proceed);
    }

    #[test]
    fn ping_pong_and_empty_turns_have_distinct_diagnostics() {
        let mut log = Log::default();
        for i in 0..4 {
            let from = if i % 2 == 0 { "alpha" } else { "beta" };
            let command_id = format!("peer-{i}");
            let text = format!("Waiting for confirmation from {from}");
            log.push(Kind::NoticeQueued(NoticeQueued {
                command_id: command_id.clone(),
                from_session_id: from.into(),
                ..Default::default()
            }));
            log.push(Kind::User(UserMessage { command_id, text, ..Default::default() }));
            log.start();
            log.output(&format!("Forwarded message {i}."));
            log.end();
        }
        assert_eq!(
            log.guard(TEST_LIMITS),
            Guard::Warn("repeated back-and-forth session messages (1 observation; source: alpha ↔ beta)".into())
        );

        log.continuance(true);
        log.notice("job", "finished", "");
        log.start();
        log.end();
        assert_eq!(
            log.guard(TEST_LIMITS),
            Guard::Warn("notice turns with no new result or observable effect (1 observation; source: job)".into())
        );
    }

    #[test]
    fn normalized_reports_and_repeated_launches_do_not_hide_no_progress() {
        let mut log = Log::default();
        for i in 0..=5 {
            let report = if i % 2 == 0 { "Same report\n" } else { "Same   report" };
            log.notice("job", "finished", report);
            log.start();
            log.background(&format!("bg-{i}"), "Run same job", "started", 0);
            log.end();
            if i < 5 {
                assert_eq!(log.reason(100), None);
            }
        }
        assert_eq!(
            log.reason(100).as_deref(),
            Some("paused: still repeating near-identical reports from the same source (5 observations; source: job)")
        );
        log.user("user intervenes");
        assert_eq!(log.guard(Limits::default()), Guard::Proceed);
    }

    #[test]
    fn background_subagent_report_wrappers_do_not_hide_no_progress() {
        for status in ["finished", "failed"] {
            let mut log = Log::default();
            log.user("start");
            log.start();
            log.background("initial", "Same task", "started", 0);
            log.end();
            for i in 0..=5 {
                let child = format!("01900000-0000-7000-8000-{i:012}");
                let report = if status == "finished" {
                    format!("Nothing changed.\n\n[subagent session {child}]")
                } else {
                    format!(
                        "<subagent id=\"{child}\" status=\"failed\" turns=\"1\" tool_calls=\"0\" elapsed=\"{i}s\">\n\
                         Last message: Nothing changed.\n</subagent>\nCheck its work."
                    )
                };
                log.notice("Same task", status, &report);
                log.start();
                log.background(&format!("bg-{i}"), "Same task", "started", 0);
                log.end();
                if i < 5 {
                    assert_eq!(log.reason(100), None);
                }
            }
            let evidence = if status == "failed" { RETRY_EVIDENCE } else { "near-identical reports from the same source" };
            assert_eq!(log.reason(100), Some(format!("paused: still repeating {evidence} (5 observations; source: Same task)")));
        }
    }

    #[test]
    fn changing_failure_signatures_are_not_a_retry_loop() {
        let mut log = Log::default();
        log.user("start");
        log.start();
        log.background("initial", "Check server", "started", 0);
        log.end();
        log.background("initial", "", "finished", 1);
        for i in 1..=4 {
            let bg = format!("bg-{i}");
            log.notice("Check server", if i % 2 == 0 { "lost" } else { "failed" }, &format!("Different error {i}"));
            log.start();
            log.spawn(&format!("child-{i}"), "Check server", &bg);
            log.background(&bg, "Check   server", "started", 0);
            log.end();
            assert_eq!(log.guard(Limits::default()), Guard::Proceed);
        }
        log.continuance(true);
        assert_eq!(log.guard(Limits::default()), Guard::Proceed);
    }

    #[test]
    fn spending_excludes_user_started_and_unattributed_turns() {
        let mut log = Log::default();
        log.notice("job", "finished", "result");
        log.user("direct");
        log.start();
        log.notice("another job", "finished", "mid-turn result");
        log.usage(100, 50, 30, 20);
        log.end();
        log.start();
        log.usage(100, 50, 30, 20);
        log.end();
        assert_eq!(log.reason(10), None);
        log.notice("job", "finished", "next result");
        log.start();
        log.usage(4, 3, 2, 1);
        log.end();
        assert_eq!(log.guard(Limits::default()), Guard::Proceed, "the hard cap is off by default");
        assert_eq!(log.reason(10).as_deref(), Some("paused: spending limit (10 tokens this hour)"));
        log.user("direct again");
        assert_eq!(log.reason(10).as_deref(), Some("paused: spending limit (10 tokens this hour)"));
        assert_eq!(evaluate(&log.0, NOW + HOUR_MS, Limits { tokens_per_hour: 10, ..Default::default() }, &HashSet::new()), Guard::Proceed);
    }

    #[test]
    fn native_self_wakes_keep_progress_and_spending_history() {
        let native_report = |log: &mut Log| {
            log.push(Kind::User(UserMessage {
                command_id: format!("task:native-{}", log.0.len()),
                command: "task-notification".into(),
                task_notice: Some(TaskNotice {
                    title: "Job".into(),
                    status: "completed".into(),
                    markdown: "Same report".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }));
        };
        let mut log = Log::default();
        native_report(&mut log);
        log.user("direct");
        log.start();
        native_report(&mut log);
        log.usage(100, 0, 0, 0);
        log.end();
        assert_eq!(log.reason(1), None);

        log.notice("Job", "completed", "Same report");
        log.start();
        log.usage(1, 0, 0, 0);
        log.end();
        for _ in 0..5 {
            native_report(&mut log);
            log.start();
            log.usage(2, 0, 0, 0);
            log.end();
        }
        assert_eq!(
            log.reason(100).as_deref(),
            Some("paused: still repeating near-identical reports from the same source (5 observations; source: Job)")
        );
        assert_eq!(log.reason(11).as_deref(), Some("paused: spending limit (11 tokens this hour)"));

        log.push(Kind::User(UserMessage {
            command_id: "task:native-no-metadata".into(),
            command: "task-notification".into(),
            text: "A background task ended.".into(),
            origin_recorded: true,
            ..Default::default()
        }));
        log.start();
        log.usage(3, 0, 0, 0);
        log.end();
        assert_eq!(log.guard(TEST_LIMITS), Guard::Warn("notice turns with no new result or observable effect (1 observation)".into()));
        log.user("direct again");
        assert_eq!(log.reason(14).as_deref(), Some("paused: spending limit (14 tokens this hour)"));
    }

    #[test]
    fn a_successful_retry_clears_its_failure_history() {
        let mut log = Log::default();
        log.user("start");
        log.start();
        log.background("initial", "Check server", "started", 0);
        log.end();
        log.background("initial", "Check server", "failed", 1);
        for i in 1..=8 {
            let id = format!("retry-{i}");
            log.notice("Check server", "failed", &format!("Error {i}"));
            log.start();
            log.background(&id, "Check server", "started", 0);
            log.background(&id, "Check server", "finished", 0);
            log.end();
            assert_eq!(log.guard(Limits::default()), Guard::Proceed, "a successful retry is not a continuing failure loop");
        }
    }

    #[test]
    fn journal_replay_honors_only_quiet_continue_and_ignores_rewound_usage() {
        let mut log = Log::default();
        log.notice("job", "finished", "");
        log.start();
        log.usage(100, 0, 0, 0);
        log.end();
        log.continuance(false);
        assert_eq!(log.reason(10).as_deref(), Some("paused: spending limit (100 tokens this hour)"));
        log.continuance(true);
        let to_seq = log.0.len() as u64 + 1;
        log.notice("next job", "finished", "");
        log.start();
        log.usage(20, 0, 0, 0);
        log.end();
        assert_eq!(log.reason(10).as_deref(), Some("paused: spending limit (20 tokens this hour)"));
        let recovered = log.0.clone();
        let limits = Limits { tokens_per_hour: 10, ..Default::default() };
        assert_eq!(evaluate(&recovered, NOW, limits, &HashSet::new()), log.guard(limits));
        log.push(Kind::Rewound(Rewound { to_seq, ..Default::default() }));
        assert_eq!(log.guard(limits), Guard::Proceed);
    }
}
