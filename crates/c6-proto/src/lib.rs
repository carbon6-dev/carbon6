//! Generated wire-protocol types for the worker ↔ head connection.

#[allow(clippy::large_enum_variant)] // generated code
pub mod v1 {
    tonic::include_proto!("carbon6.v1");
}

pub use v1::*;

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;

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
}
