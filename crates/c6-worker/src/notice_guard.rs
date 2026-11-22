//! Rebuild the unattended-wake guards from the session journal.

use std::collections::{BTreeMap, HashMap, HashSet};

use c6_proto::{SessionEvent, TaskNotice, session_event::Kind};

const HOUR_MS: i64 = 3_600_000;
pub(crate) const CONTINUED_TITLE: &str = "Notice wakes continued";

#[derive(Default)]
struct Chain<'a> {
    descriptions: HashSet<String>,
    failed: HashSet<String>,
    relaunches: BTreeMap<String, u32>,
    consecutive_losses: BTreeMap<String, u32>,
    seen_outcomes: HashSet<(&'static str, &'a str, &'a str)>,
    last_reports: HashSet<String>,
    no_progress: u32,
}

#[derive(Default)]
struct Wake {
    reports: HashSet<String>,
    new_description: bool,
}

impl<'a> Chain<'a> {
    fn reset(&mut self) {
        // A later receipt or delivery of an old report is not a fresh outcome.
        *self = Self { seen_outcomes: std::mem::take(&mut self.seen_outcomes), ..Self::default() };
    }

    fn finish(&mut self, wake: Wake) {
        let changed_report = wake.reports.iter().any(|report| !self.last_reports.contains(report));
        if !wake.reports.is_empty() {
            self.last_reports = wake.reports;
        }
        self.no_progress = if wake.new_description || changed_report { 0 } else { self.no_progress.saturating_add(1) };
    }

    fn launch(&mut self, wake: Option<&mut Wake>, description: &str) {
        let description = normalized(description);
        if description.is_empty() {
            return;
        }
        if let Some(wake) = wake {
            if self.failed.contains(&description) {
                let n = self.relaunches.entry(description.clone()).or_default();
                *n = n.saturating_add(1);
            }
            wake.new_description |= self.descriptions.insert(description);
        } else {
            // A user-started launch seeds the chain but isn't an unattended retry.
            self.descriptions.insert(description);
        }
    }

    fn failure(&mut self, description: &str) {
        let description = normalized(description);
        if !description.is_empty() {
            self.failed.insert(description);
        }
    }

    fn success(&mut self, description: &str) {
        let description = normalized(description);
        self.failed.remove(&description);
        self.relaunches.remove(&description);
    }

    fn subagent_outcome(&mut self, id: (&'static str, &'a str), description: &str, status: &'a str) {
        if !matches!(status, "lost" | "failed" | "error" | "completed" | "finished" | "interrupted" | "stopped" | "stopped_by_user") {
            return;
        }
        let description = normalized(description);
        if description.is_empty() || (!id.1.is_empty() && !self.seen_outcomes.insert((id.0, id.1, status))) {
            return;
        }
        if status == "lost" {
            let count = self.consecutive_losses.entry(description).or_default();
            *count = count.saturating_add(1);
        } else {
            self.consecutive_losses.remove(&description);
        }
    }

    fn subagent_notice(&mut self, command_id: &'a str, notice: &'a TaskNotice, children: &HashMap<&str, &str>) {
        let id = notice.session_id.as_str();
        if notice.kind == "subagent" || (!id.is_empty() && children.contains_key(id)) {
            let description = children.get(id).copied().unwrap_or(&notice.title);
            let incident = command_id.strip_prefix("task:").and_then(|s| s.rsplit_once(':'));
            let key = incident.map_or_else(|| report_id(command_id, id), |(task, _)| ("task", task));
            self.subagent_outcome(key, description, &notice.status);
        }
    }
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
        && let Some((stable_attrs, _)) = attrs.strip_suffix("\">").and_then(|s| s.rsplit_once(" elapsed=\""))
        && body.contains("</subagent>")
    {
        return normalized(&format!("<subagent status=\"{stable_attrs}>\n{body}"));
    }
    normalized(markdown)
}

fn report(reports: &mut HashSet<String>, notice: &TaskNotice) {
    // Display markdown omits the delivery wrapper's task ID, sender and
    // elapsed time, but background subagent output still wraps its result.
    let markdown = report_markdown(&notice.markdown, &notice.status);
    if !markdown.is_empty() {
        // Different audit slices can all honestly report "no issues". Their
        // descriptions distinguish completed work without treating fresh task
        // IDs or delivery wrappers as progress in a retry loop.
        reports.insert(format!("{}\n{markdown}", normalized(&notice.title)));
    }
}

/// Explain why the *next* unattended wake should be held, if any. `events`
/// are in journal order. A direct input resets progress/retry accounting, not
/// rolling spending. A quiet Continue grants a fresh spending baseline:
/// usage before its journal position is not charged again.
///
/// Checkpoint events name commits, not tree hashes. Repeated snapshots of an
/// unchanged tree can have different commits, so they are not a progress
/// signal without a tree ID or diff in the journal.
pub(crate) fn evaluate(
    events: &[SessionEvent],
    now_ms: i64,
    no_progress_limit: u32,
    failure_relaunch_limit: u32,
    tokens_per_hour: u64,
) -> Option<String> {
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
    let mut child_descriptions = HashMap::new();
    let mut background_descriptions = HashMap::new();
    let mut launches = HashSet::new();
    let mut notice_input = false;
    let mut direct_input = false;
    let mut pending_reports = HashSet::new();
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
            Some(Kind::NoticeQueued(n)) if !n.command_id.is_empty() => {
                notices.insert(n.command_id.as_str(), n);
                // The guard runs before a queued notice becomes model input.
                // Count the outcome here so the second loss cannot itself wake
                // the model into another retry.
                if let Some(notice) = &n.task_notice {
                    chain.subagent_notice(&n.command_id, notice, &child_descriptions);
                }
            }
            Some(Kind::User(u)) if u.blocked.is_empty() => {
                let queued = notices.get(u.command_id.as_str());
                // The CLI logs its own task notifications without NoticeQueued.
                // Only a notification before the turn can establish its start.
                let native_report = u.command == "task-notification"
                    && !u.human_origin
                    && u.api_origin.is_none()
                    && (u.origin_recorded || u.task_notice.is_some());
                if queued.is_some() || native_report {
                    if active.is_none() {
                        notice_input = true;
                    }
                    if let Some(n) = u.task_notice.as_ref().or_else(|| queued.and_then(|q| q.task_notice.as_ref())) {
                        chain.subagent_notice(&u.command_id, n, &child_descriptions);
                        if failed(&n.status) {
                            chain.failure(child_descriptions.get(n.session_id.as_str()).copied().unwrap_or(n.title.as_str()));
                        } else if matches!(n.status.as_str(), "completed" | "finished") {
                            chain.success(child_descriptions.get(n.session_id.as_str()).copied().unwrap_or(n.title.as_str()));
                        }
                        if active.is_none() {
                            report(&mut pending_reports, n);
                        } else if let Some(wake) = &mut wake {
                            report(&mut wake.reports, n);
                        }
                    }
                } else {
                    chain.reset();
                    wake = None;
                    if active.is_none() {
                        direct_input = true;
                    }
                }
            }
            Some(Kind::TurnStarted(t)) => {
                // A start also bounds an older incomplete turn in legacy logs.
                if let Some(previous) = wake.take() {
                    chain.finish(previous);
                }
                active = Some(t.turn);
                notice_started = notice_input && !direct_input;
                let reports = std::mem::take(&mut pending_reports);
                wake = notice_started.then_some(Wake { reports, ..Default::default() });
                notice_input = false;
                direct_input = false;
            }
            Some(Kind::TurnEnded(t)) if active == Some(t.turn) => {
                if let Some(wake) = wake.take() {
                    chain.finish(wake);
                }
                active = None;
                notice_started = false;
            }
            Some(Kind::Usage(u)) if notice_started && event.ts_ms > cutoff && event.ts_ms <= now_ms => {
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
            Some(Kind::SubagentReported(r)) => {
                if let Some(description) = child_descriptions.get(r.child_session_id.as_str()) {
                    chain.subagent_outcome(report_id(&r.command_id, &r.child_session_id), description, &r.status);
                    if failed(&r.status) {
                        chain.failure(description);
                    } else if r.status == "completed" {
                        chain.success(description);
                    }
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
                if t.kind == "agent" || t.native_type == "local_agent" {
                    chain.subagent_outcome(("task", &t.task_id), description, status);
                }
                if failed(status) {
                    chain.failure(description);
                } else if status == "finished" {
                    chain.success(description);
                }
            }
            _ => {}
        }
    }

    let repeated =
        chain.consecutive_losses.iter().find(|(_, count)| **count >= 2).map(|(description, _)| ("loss", description)).or_else(|| {
            chain.relaunches.iter().find(|(_, count)| **count > failure_relaunch_limit).map(|(description, _)| ("failure", description))
        });
    if let Some((kind, description)) = repeated {
        let short: String = description.chars().take(120).collect();
        let suffix = if short.len() < description.len() { "…" } else { "" };
        return Some(format!("paused: repeated {kind} of {short}{suffix}"));
    }
    if chain.no_progress != 0 && chain.no_progress >= no_progress_limit {
        return Some(format!("paused: no progress in {} wakes", chain.no_progress));
    }
    if tokens != 0 && tokens >= tokens_per_hour {
        return Some(format!("paused: spending limit ({tokens} tokens this hour)"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use c6_proto::{
        BackgroundTask, Notice, NoticeQueued, Rewound, SubagentReported, SubagentSpawned, TurnEnded, TurnStarted, Usage, UserMessage,
    };

    const NOW: i64 = 10 * HOUR_MS;

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
                    ..Default::default()
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

        fn reason(&self, no_progress: u32, retries: u32, tokens: u64) -> Option<String> {
            evaluate(&self.0, NOW, no_progress, retries, tokens)
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
        assert_eq!(log.reason(5, 3, 100), None, "the notice and receipt are one loss");
        log.queue_subagent("subagent:liveness:lost:child:remote:incident-b", "child:remote", "Check server", "lost");
        assert_eq!(log.reason(5, 3, 100).as_deref(), Some("paused: repeated loss of Check server"));
    }

    #[test]
    fn second_subagent_loss_holds_the_notice_before_delivery() {
        let mut log = Log::default();
        log.user("start");
        log.start();
        log.spawn("first", "Check  server", "");
        log.end();

        let first = "subagent:subagent:first:7:first-call";
        log.queue_subagent(first, "first", "Different display title", "lost");
        assert_eq!(log.reason(5, 3, 100), None, "the first loss can wake the retry");
        log.receipt("subagent:first:7:first-call", "first", "lost");
        log.receipt("subagent:first:7:another-waiter", "first", "lost");
        log.queue_subagent(first, "first", "Different display title", "lost");
        log.user(first); // Delivery of a queued notice is logged as User.
        assert_eq!(log.reason(5, 3, 100), None, "the queue, receipt and delivery are one loss");

        log.start();
        log.spawn("retry", "Check server", "");
        log.end();
        log.queue_subagent("subagent:local:retry:4", "retry", "Another title", "lost");
        assert_eq!(log.reason(5, 3, 100).as_deref(), Some("paused: repeated loss of Check server"));

        log.user("intervene");
        log.receipt("subagent:local:retry:4", "retry", "lost");
        log.queue_subagent("subagent:local:third:4", "third", "Check server", "lost");
        assert_eq!(log.reason(5, 3, 100), None, "direct input clears the streak even if an old receipt arrives later");
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
        assert_eq!(log.reason(5, 3, 100), None, "different waiters reported the same incident");

        log.queue_subagent(
            "subagent:liveness:lost:child:remote:d59d8516-eeaf-44e5-ab4a-9d4d3cb9d09b:first-call",
            child,
            "Different display title",
            "lost",
        );
        assert_eq!(log.reason(5, 3, 100).as_deref(), Some("paused: repeated loss of Check server"));
    }

    #[test]
    fn only_consecutive_losses_of_the_same_description_hold_wakes() {
        for intervening in ["completed", "failed"] {
            let mut log = Log::default();
            log.spawn("first", "Check server", "");
            log.receipt("subagent:local:first:2", "first", "lost");
            log.queue_subagent("subagent:local:other:4", "other", "Check database", "lost");
            assert_eq!(log.reason(5, 3, 100), None, "different descriptions have separate streaks");

            log.spawn("retry", "Check server", "");
            log.receipt("subagent:local:retry:3", "retry", intervening);
            log.queue_subagent("subagent:local:retry:4", "retry", "Check server", "lost");
            // The receipt came first; its late delivery cannot end a new streak.
            log.queue_subagent("subagent:local:retry:3", "retry", "Check server", intervening);
            assert_eq!(log.reason(5, 3, 100), None, "{intervening} breaks the first loss streak");

            log.queue_subagent("subagent:local:next:4", "next", "Check server", "lost");
            assert_eq!(log.reason(5, 3, 100).as_deref(), Some("paused: repeated loss of Check server"));
        }
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
        assert_eq!(log.reason(5, 3, 100), None);
        log.push(Kind::BackgroundTask(lost_task("two")));
        assert_eq!(log.reason(5, 3, 100).as_deref(), Some("paused: repeated loss of Check server"));
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
            assert_eq!(log.reason(2, 3, 100), None);
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
            log.end();
            assert_eq!(log.reason(5, 3, 100), None, "distinct user-requested slices are productive even with identical findings");
        }
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
                assert_eq!(log.reason(5, 3, 100), None);
            }
        }
        assert_eq!(log.reason(5, 3, 100).as_deref(), Some("paused: no progress in 5 wakes"));
        log.user("user intervenes");
        assert_eq!(log.reason(5, 3, 100), None);
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
                    assert_eq!(log.reason(5, 10, 100), None);
                }
            }
            assert_eq!(log.reason(5, 10, 100).as_deref(), Some("paused: no progress in 5 wakes"));
        }
    }

    #[test]
    fn failed_relaunch_limit_ignores_changing_reports_and_deduplicates_launch_pairs() {
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
            if i <= 3 {
                assert_eq!(log.reason(5, 3, 100), None);
            }
        }
        assert_eq!(log.reason(5, 3, 100).as_deref(), Some("paused: repeated failure of Check server"));
        log.continuance(true);
        assert_eq!(log.reason(5, 3, 100), None);
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
        assert_eq!(log.reason(5, 3, 10), None);
        log.notice("job", "finished", "next result");
        log.start();
        log.usage(4, 3, 2, 1);
        log.end();
        assert_eq!(log.reason(5, 3, 10).as_deref(), Some("paused: spending limit (10 tokens this hour)"));
        log.user("direct again");
        assert_eq!(log.reason(5, 3, 10).as_deref(), Some("paused: spending limit (10 tokens this hour)"));
        assert_eq!(evaluate(&log.0, NOW + HOUR_MS, 5, 3, 10), None);
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
        assert_eq!(log.reason(5, 3, 1), None);

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
        assert_eq!(log.reason(5, 3, 100).as_deref(), Some("paused: no progress in 5 wakes"));
        assert_eq!(log.reason(6, 3, 11).as_deref(), Some("paused: spending limit (11 tokens this hour)"));

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
        assert_eq!(log.reason(6, 3, 100).as_deref(), Some("paused: no progress in 6 wakes"));
        log.user("direct again");
        assert_eq!(log.reason(5, 3, 14).as_deref(), Some("paused: spending limit (14 tokens this hour)"));
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
            assert_eq!(log.reason(5, 3, 100), None, "a successful retry is not a continuing failure loop");
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
        assert_eq!(log.reason(5, 3, 10).as_deref(), Some("paused: spending limit (100 tokens this hour)"));
        log.continuance(true);
        let to_seq = log.0.len() as u64 + 1;
        log.notice("next job", "finished", "");
        log.start();
        log.usage(20, 0, 0, 0);
        log.end();
        assert_eq!(log.reason(5, 3, 10).as_deref(), Some("paused: spending limit (20 tokens this hour)"));
        let recovered = log.0.clone();
        assert_eq!(evaluate(&recovered, NOW, 5, 3, 10), log.reason(5, 3, 10));
        log.push(Kind::Rewound(Rewound { to_seq, ..Default::default() }));
        assert_eq!(log.reason(1, 3, 10), None);
    }
}
