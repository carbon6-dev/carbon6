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
