//! Generated wire-protocol types for the worker ↔ head connection.

mod compat;

#[allow(clippy::large_enum_variant)] // generated code
pub mod v1 {
    tonic::include_proto!("carbon6.v1");
}

pub use compat::decode_stored_command;
pub use v1::*;

/// Hello capability required to send a nonzero session ownership epoch.
pub const SESSION_MOVE_CAPABILITY: &str = "session_move_v1";
/// The destination honors an explicit folder and worktree plan. Older fenced
/// movers ignore these fields and must not receive a complete-target request.
pub const SESSION_MOVE_TARGET_CAPABILITY: &str = "session_move_target_v1";
/// The worker journals derived configuration and applies it between turns.
pub const SESSION_SPEC_REFRESH_CAPABILITY: &str = "session_spec_refresh_v1";

impl CreateSession {
    /// Deliberate allowlist: a configuration refresh cannot change identity,
    /// placement, model, history, delegation, labels or provider cache state.
    /// Keep in sync with docs/session-specs.md when adding wire fields.
    pub fn apply_derived(&mut self, fresh: &Self) {
        macro_rules! copy {
            ($($field:ident),* $(,)?) => { $(self.$field.clone_from(&fresh.$field);)* };
        }
        copy!(
            base_prompt,
            project_contract_json,
            project_contract_prompt,
            mcp_servers_json,
            skills_json,
            hooks_json,
            agents_json,
            secrets_json,
            web_search_json,
            memories_json,
            policies_json,
            memex_tools,
            memex_mail,
            memex_calendar,
            memex_chat,
            prompts_json,
            max_subagent_depth,
            max_running_subagents,
            subagent_connections_json,
            tools,
            subagent_templates_json,
            auto_turns_per_hour,
            claude_code_idle_minutes,
            claude_code_tools,
            claude_code_tools_chosen,
            claude_code,
            notice_quiet_seconds,
            notice_max_wait_seconds,
            notice_no_progress_limit,
            notice_failure_relaunch_limit,
            notice_tokens_per_hour,
            session_recovery_seconds,
            notice_loop_backoff_rounds,
            notice_loop_backoff_max_seconds,
            disk_recovery_initial_seconds,
            disk_recovery_max_seconds,
            disk_warning_percent,
            disk_warning_bytes,
            claude_code_trim_enabled,
            claude_code_trim_at,
            keep_warm_json,
        );
        // Old creation specs can carry a model-only explicit choice without
        // the newer provenance bit or a subsequent choice event.
        if !self.subagent_target_explicit && self.subagent_model.is_empty() {
            self.subagent_target.clone_from(&fresh.subagent_target);
        }
    }

    /// A configuration-only wire snapshot, also used for no-op detection.
    pub fn derived(&self) -> Self {
        let mut derived = Self::default();
        derived.apply_derived(self);
        derived
    }
}

/// Maintenance is accounted and replicated, but is not evidence of task
/// progress. Quiet diagnostic titles are protocol markers, not agent prompts.
pub fn is_keep_warm_event(event: &SessionEvent) -> bool {
    match &event.kind {
        Some(session_event::Kind::Usage(usage)) => usage.keep_warm,
        Some(session_event::Kind::Notice(notice)) => {
            notice.quiet
                && matches!(
                    notice.title.as_str(),
                    "Cache maintenance attempted" | "Kept cache warm" | "Kept the cache warm" | "Cache maintenance failed"
                )
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

    #[test]
    fn derived_refresh_cannot_replace_session_state() {
        let model = ModelSpec { connection_id: "own".into(), model: "chosen".into(), ..Default::default() };
        let mut spec = CreateSession {
            model: Some(model.clone()),
            cwd: "/chosen".into(),
            worktree: true,
            ssh_target: "host".into(),
            labels: vec!["mine".into()],
            depth: 2,
            delegation: Some(Delegation { parent_session_id: "parent".into(), ..Default::default() }),
            seed: vec![SessionEvent { seq: 7, ..Default::default() }],
            memex_brief: "conversation context".into(),
            keep_warm_snapshot_json: Some("prefix".into()),
            subagent_model: "legacy choice".into(),
            subagent_target: Some(model.clone()),
            subagent_target_explicit: true,
            ..Default::default()
        };
        let state = spec.clone();
        let fresh = CreateSession {
            base_prompt: "today's prompt".into(),
            project_contract_json: Some(r#"{"version":1}"#.into()),
            project_contract_prompt: Some("Widget contract".into()),
            tools: vec!["Task".into(), "SendMessage".into()],
            subagent_connections_json: "[]".into(),
            subagent_target: Some(ModelSpec::default()),
            ..Default::default()
        };
        spec.apply_derived(&fresh);
        let mut expected = state;
        expected.base_prompt.clone_from(&fresh.base_prompt);
        expected.project_contract_json.clone_from(&fresh.project_contract_json);
        expected.project_contract_prompt.clone_from(&fresh.project_contract_prompt);
        expected.tools.clone_from(&fresh.tools);
        expected.subagent_connections_json.clone_from(&fresh.subagent_connections_json);
        assert_eq!(spec, expected);
        assert_eq!(spec.subagent_target, Some(model));
        let snapshot = spec.derived();
        assert!(snapshot.model.is_none() && snapshot.cwd.is_empty() && snapshot.seed.is_empty());
        assert!(snapshot.delegation.is_none() && snapshot.labels.is_empty());
        assert!(snapshot.keep_warm_snapshot_json.is_none() && snapshot.memex_brief.is_empty());
        assert!(!snapshot.subagent_target_explicit);
        assert_eq!(snapshot.project_contract_json, fresh.project_contract_json);
        assert_eq!(snapshot.project_contract_prompt, fresh.project_contract_prompt);
        spec.apply_derived(&CreateSession::default());
        assert!(spec.project_contract_json.is_none());
        assert!(spec.project_contract_prompt.is_none());
    }

    #[test]
    fn spec_refresh_uses_new_wire_tags_and_old_specs_still_decode() {
        assert_eq!(CreateSession::decode(&[][..]).unwrap(), CreateSession::default());
        let cmd = SessionCommand { kind: Some(session_command::Kind::RefreshSpec(CreateSession::default())), ..Default::default() };
        assert_eq!(cmd.encode_to_vec(), [0xc2, 0x01, 0]); // command tag 24
        assert_eq!(SessionCommand::decode(cmd.encode_to_vec().as_slice()).unwrap(), cmd);
        let event = SessionEvent { kind: Some(session_event::Kind::SpecRefreshed(SpecRefreshed::default())), ..Default::default() };
        assert_eq!(event.encode_to_vec(), [0x92, 0x02, 0]); // event tag 34
    }

    #[test]
    fn legacy_explicit_subagent_choice_is_not_replaced_by_a_default() {
        let chosen = ModelSpec { connection_id: "chosen".into(), ..Default::default() };
        let mut spec =
            CreateSession { subagent_model: "legacy explicit model".into(), subagent_target: Some(chosen.clone()), ..Default::default() };
        spec.apply_derived(&CreateSession { subagent_target: Some(ModelSpec::default()), ..Default::default() });
        assert_eq!(spec.subagent_target, Some(chosen));
        assert_eq!(spec.subagent_model, "legacy explicit model");
    }

    #[test]
    fn machine_sample_uses_fresh_worker_tag_nine() {
        let bytes = b"\x4a\x04\x0a\x02{}";
        let sample = WorkerMsg { msg: Some(worker_msg::Msg::MachineSample(MachineSample { sample_json: b"{}".to_vec() })) };
        assert_eq!(sample.encode_to_vec(), bytes);
        assert_eq!(WorkerMsg::decode(bytes.as_slice()).unwrap(), sample);
        // Historical Hello remains tag 1, not a new telemetry payload.
        assert!(matches!(WorkerMsg::decode(b"\x0a\x00".as_slice()).unwrap().msg, Some(worker_msg::Msg::Hello(_))));
    }

    #[test]
    fn welcome_machine_settings_are_optional_fresh_tags() {
        let old = Welcome::decode(&[][..]).unwrap();
        assert_eq!(old.machine_sample_secs, None);
        assert_eq!(old.machine_disk_hold_percent, None);
        assert_eq!(old.machine_disk_warn_percent, None);
        let welcome = Welcome {
            machine_sample_secs: Some(15),
            machine_disk_hold_percent: Some(5.0),
            machine_disk_warn_percent: Some(12.0),
            ..Default::default()
        };
        let bytes = welcome.encode_to_vec();
        assert_eq!(bytes[0..2], [0x30, 15]);
        assert_eq!(bytes[2], 0x39);
        assert_eq!(bytes[11], 0x41);
        assert_eq!(Welcome::decode(bytes.as_slice()).unwrap(), welcome);
    }

    #[test]
    fn narration_uses_a_fresh_tag_without_reclassifying_historical_blocks() {
        for (bytes, block) in [
            (b"\x0a\x06Update".as_slice(), content_block::Block::Text("Update".into())),
            (b"\x12\x06Update".as_slice(), content_block::Block::Thinking("Update".into())),
            (b"\x4a\x06Update".as_slice(), content_block::Block::Narration("Update".into())),
        ] {
            let decoded = ContentBlock::decode(bytes).unwrap();
            assert_eq!(decoded.block.as_ref(), Some(&block));
            assert_eq!(decoded.encode_to_vec(), bytes);
        }
    }

    #[test]
    fn notice_queued_accepts_both_historical_tag_10_encodings() {
        // Literal protobuf wire bytes, not generated with today's schema:
        // command_id="c", text="n", quiet=true (168f3248).
        let quiet = b"\x0a\x01c\x12\x01n\x50\x01";
        // Same notice with task_notice={task_id:"t"} at tag 10 (5384d8e2).
        let task = b"\x0a\x01c\x12\x01n\x52\x03\x0a\x01t";
        for bytes in [quiet.as_slice(), task.as_slice()] {
            let notice = NoticeQueued::decode(bytes).unwrap();
            assert_eq!(notice.command_id, "c");
            assert_eq!(notice.text, "n");
            assert!(notice.reason.is_empty());
            assert!(notice.task_notice.is_none(), "retired tag 10 falls back to the plain label");
            // SessionEvent seq=14378, ts_ms=1, notice_queued=tag 31.
            let mut event = vec![0x08, 0xaa, 0x70, 0x10, 0x01, 0xfa, 0x01, bytes.len() as u8];
            event.extend_from_slice(bytes);
            let event = SessionEvent::decode(event.as_slice()).unwrap();
            assert_eq!(event.seq, 14378);
            assert!(matches!(event.kind, Some(session_event::Kind::NoticeQueued(n)) if n == notice));
        }
    }

    #[test]
    fn notice_queued_writes_task_notice_at_fresh_tag_11() {
        let notice = NoticeQueued { task_notice: Some(TaskNotice::default()), ..Default::default() };
        assert_eq!(notice.encode_to_vec(), [0x5a, 0x00]);
        assert_eq!(NoticeQueued::decode(notice.encode_to_vec().as_slice()).unwrap(), notice);
    }

    #[test]
    fn notice_queued_writes_reason_at_fresh_tag_12() {
        let notice = NoticeQueued { reason: "paused".into(), ..Default::default() };
        let bytes = b"\x62\x06paused";
        assert_eq!(notice.encode_to_vec(), bytes);
        assert_eq!(NoticeQueued::decode(bytes.as_slice()).unwrap(), notice);
    }

    #[test]
    fn create_session_retains_legacy_notice_tag_with_new_settings_unset() {
        // auto_turns_per_hour=20, claude_code_idle_minutes=15 (tags 31–32).
        let bytes = b"\xf8\x01\x14\x80\x02\x0f";
        let spec = CreateSession::decode(bytes.as_slice()).unwrap();
        assert!(spec.keep_warm_json.is_none());
        assert_eq!(spec, CreateSession { auto_turns_per_hour: 20, claude_code_idle_minutes: 15, ..Default::default() });
        assert_eq!(spec.encode_to_vec(), bytes);
    }

    #[test]
    fn release_notices_carries_limits_but_accepts_older_empty_commands() {
        assert_eq!(ReleaseNotices::decode(&[][..]).unwrap().limits, None);
        let release = ReleaseNotices {
            limits: Some(NoticeLimits {
                notice_quiet_seconds: 3,
                notice_max_wait_seconds: 30,
                notice_no_progress_limit: 5,
                notice_failure_relaunch_limit: 3,
                notice_tokens_per_hour: 10_000_000_000,
                notice_loop_backoff_rounds: 3,
                notice_loop_backoff_max_seconds: 900,
                max_subagent_depth: Some(5),
                max_running_subagents: Some(0),
                claude_code_trim_enabled: Some(false),
                claude_code_trim_at: Some(10_000_000_000),
                keep_warm_json: Some(r#"{"enabled":false}"#.into()),
            }),
        };
        assert_eq!(ReleaseNotices::decode(release.encode_to_vec().as_slice()).unwrap(), release);
    }

    #[test]
    fn keep_warm_settings_round_trip_and_accept_legacy_limits() {
        assert!(NoticeLimits::decode(&[][..]).unwrap().keep_warm_json.is_none());
        let spec = CreateSession { keep_warm_json: Some(r#"{"enabled":false}"#.into()), ..Default::default() };
        assert_eq!(CreateSession::decode(spec.encode_to_vec().as_slice()).unwrap(), spec);
    }
}
