//! Compatibility for the head's durable commands, not the live RPC protocol.

use prost::encoding::{DecodeContext, WireType, decode_key, encode_varint, skip_field};
use prost::{DecodeError, Message};

use crate::SessionCommand;

/// Decodes a command from the head's command store, including deployed fork bytes.
///
/// The fork used `UserInput` tag 16 for `human_origin` (varint); upstream uses
/// it for `prompt` (string). Only that legacy varint, directly inside input
/// tag 11, is moved to human_origin's canonical tag 18. An explicitly encoded
/// tag 18 wins, including false. All other fields reach prost unchanged.
/// Malformed wire data or prompts return an error; there is no lossy fallback.
pub fn decode_stored_command(data: &[u8]) -> Result<SessionCommand, DecodeError> {
    SessionCommand::decode(rewrite_command(data)?.as_slice())
}

fn rewrite_command(mut data: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(data.len());
    // Repeated occurrences of the same message-valued oneof merge. Remember
    // explicit provenance across input fragments, until a different kind wins.
    let mut canonical_human = false;
    while !data.is_empty() {
        let (tag, wire, raw) = take_field(&mut data)?;
        if tag == 11 && wire == WireType::LengthDelimited {
            let mut input = raw;
            decode_key(&mut input)?;
            let key_len = raw.len() - input.len();
            prost::decode_length_delimiter(&mut input)?;
            // take_field already checked the length and isolated this payload.
            if let Some(rewritten) = rewrite_input(input, &mut canonical_human)? {
                out.extend_from_slice(&raw[..key_len]);
                encode_varint(rewritten.len() as u64, &mut out);
                out.extend_from_slice(&rewritten);
            } else {
                out.extend_from_slice(raw);
            }
        } else {
            // Ask the generated schema which fields select a different kind,
            // rather than duplicate its oneof tag list here. Metadata and
            // unknown fields do not reset the current input's merge state.
            if canonical_human && SessionCommand::decode(raw)?.kind.is_some() {
                canonical_human = false;
            }
            out.extend_from_slice(raw);
        }
    }
    Ok(out)
}

fn rewrite_input(data: &[u8], canonical_human: &mut bool) -> Result<Option<Vec<u8>>, DecodeError> {
    let mut fields = data;
    let mut legacy = false;
    while !fields.is_empty() {
        let (tag, wire, _) = take_field(&mut fields)?;
        legacy |= tag == 16 && wire == WireType::Varint;
        *canonical_human |= tag == 18;
    }
    if !legacy {
        return Ok(None);
    }

    let mut out = Vec::with_capacity(data.len());
    fields = data;
    while !fields.is_empty() {
        let (tag, wire, raw) = take_field(&mut fields)?;
        if tag == 16 && wire == WireType::Varint {
            if !*canonical_human {
                let mut value = raw;
                decode_key(&mut value)?;
                out.extend_from_slice(b"\x90\x01"); // tag 18, varint
                out.extend_from_slice(value);
            }
        } else {
            out.extend_from_slice(raw);
        }
    }
    Ok(Some(out))
}

// Keep raw field slices, including opaque unknown groups and non-minimal
// varints. Prost supplies bounds, wire-type, group and recursion validation.
fn take_field<'a>(data: &mut &'a [u8]) -> Result<(u32, WireType, &'a [u8]), DecodeError> {
    let start = *data;
    let (tag, wire) = decode_key(data)?;
    skip_field(wire, tag, data, DecodeContext::default())?;
    Ok((tag, wire, &start[..start.len() - data.len()]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApiMessageOrigin, Attachment, ModelSpec, UserInput, UserMessage, session_command::Kind};

    // Captured with protoc --encode=carbon6.v1.{UserInput,SessionCommand}
    // against the pre-rebase fork schema (human16/task17), not production rows.
    // Keep literals: encoding the current generated types hides the collision.
    const OLD_INPUT: &[u8] = b"\x0a\x07Status?\x80\x01\x01";
    const OLD_COMMAND: &[u8] = b"\x0a\x07cmd-old\x12\x0bsession-old\x5a\x96\x01\
        \x0a\x0dReview @notes\x12\x16\x0a\x05local\x12\x04test\x18\x80\x02\x22\x04high\
        \x1a\x21\x0a\x09notes.txt\x12\x0atext/plain\x1a\x08\x00\x80\xffnotes\
        \x22\x06review\x2a\x02[]\x32\x09repo:test\x40\x01\x4a\x06call-1\
        \x52\x06parent\x5a\x06Parent\x60\x02\x70\x01\x7a\x0f\x0a\x0dfixture-token\
        \x80\x01\x01\x88\x01\x02";

    fn envelope(input: &[u8]) -> Vec<u8> {
        let mut data = vec![0x5a];
        encode_varint(input.len() as u64, &mut data);
        data.extend_from_slice(input);
        data
    }

    fn input(data: &[u8]) -> UserInput {
        let Some(Kind::Input(input)) = decode_stored_command(data).unwrap().kind else {
            panic!("expected input");
        };
        input
    }

    #[test]
    fn captured_fork_bytes_retain_the_complete_input_and_outer_ids() {
        assert!(UserInput::decode(OLD_INPUT).is_err());
        assert!(SessionCommand::decode(OLD_COMMAND).is_err());
        assert_eq!(input(&envelope(OLD_INPUT)), UserInput { text: "Status?".into(), human_origin: true, ..Default::default() });

        let decoded = decode_stored_command(OLD_COMMAND).unwrap();
        assert_eq!(decoded.command_id, "cmd-old");
        assert_eq!(decoded.session_id, "session-old");
        let Some(Kind::Input(actual)) = decoded.kind else { panic!("expected input") };
        assert_eq!(
            actual,
            UserInput {
                text: "Review @notes".into(),
                model: Some(ModelSpec {
                    connection_id: "local".into(),
                    model: "test".into(),
                    max_output_tokens: 256,
                    effort: "high".into(),
                    ..Default::default()
                }),
                attachments: vec![Attachment { name: "notes.txt".into(), mime: "text/plain".into(), data: b"\x00\x80\xffnotes".to_vec() }],
                command: "review".into(),
                command_candidates_json: "[]".into(),
                labels: vec!["repo:test".into()],
                wake: true,
                delegated_by: "call-1".into(),
                from_session_id: "parent".into(),
                from_title: "Parent".into(),
                hops: 2,
                send_now: true,
                api_origin: Some(ApiMessageOrigin { token_name: "fixture-token".into() }),
                human_origin: true,
                task_card_origin: 2,
                ..Default::default()
            }
        );
    }

    #[test]
    fn canonical_prompt_and_mixed_legacy_human_are_independent() {
        let prompt = b"\x82\x01\x03ctx";
        let canonical = envelope(&[b"\x0a\x07Status?".as_slice(), prompt].concat());
        assert_eq!(rewrite_command(&canonical).unwrap(), canonical);
        assert_eq!(decode_stored_command(&canonical).unwrap(), SessionCommand::decode(canonical.as_slice()).unwrap());
        assert_eq!(input(&canonical).prompt, "ctx");
        assert!(!input(&canonical).human_origin);
        for fields in [[OLD_INPUT, prompt.as_slice()], [prompt.as_slice(), OLD_INPUT]] {
            let actual = input(&envelope(&fields.concat()));
            assert_eq!(actual.text, "Status?");
            assert_eq!(actual.prompt, "ctx");
            assert!(actual.human_origin);
        }
    }

    #[test]
    fn explicit_canonical_false_beats_legacy_true_in_either_order() {
        for fields in [[b"\x80\x01\x01".as_slice(), b"\x90\x01\x00".as_slice()], [b"\x90\x01\x00".as_slice(), b"\x80\x01\x01".as_slice()]] {
            let actual = input(&envelope(&fields.concat()));
            assert!(!actual.human_origin);
        }
        assert!(input(&envelope(b"\x80\x01\x00\x90\x01\x01")).human_origin);
        // Without canonical provenance, legacy duplicates retain protobuf's last-wins rule.
        assert!(!input(&envelope(b"\x80\x01\x01\x80\x01\x00")).human_origin);
    }

    #[test]
    fn unknown_wire_data_and_task17_are_copied_not_reencoded() {
        // Unknown varint (non-minimal key/value), fixed64, length-delimited,
        // group containing a tag16 varint, and fixed32. Embedded tag16 bytes
        // are opaque, not another UserInput to rewrite.
        let unknown = b"\xa0\x86\x00\x81\x00\xa9\x06abcdefgh\xb2\x06\x03\x80\x01\x01\
            \xbb\x06\x80\x01\x01\xbc\x06\xc5\x06wxyz";
        let legacy = b"\x80\x01\x81\x00"; // valid non-minimal true
        let payload = [unknown.as_slice(), b"\x88\x01\x02", legacy].concat();
        let expected = [unknown.as_slice(), b"\x88\x01\x02", b"\x90\x01\x81\x00"].concat();
        let data = [unknown.as_slice(), &envelope(&payload)].concat();
        let expected = [unknown.as_slice(), &envelope(&expected)].concat();
        assert_eq!(rewrite_command(&data).unwrap(), expected);
        let actual = input(&data);
        assert!(actual.human_origin);
        assert_eq!(actual.task_card_origin, 2);
    }

    #[test]
    fn only_direct_input_fields_are_rewritten() {
        // Interrupt (tag12) contains an unknown tag16 varint; it stays opaque.
        let other = b"\x0a\x01c\x12\x01s\x62\x03\x80\x01\x01";
        assert_eq!(rewrite_command(other).unwrap(), other);
        assert_eq!(decode_stored_command(other).unwrap(), SessionCommand::decode(other.as_slice()).unwrap());
        assert_eq!(decode_stored_command(b"").unwrap(), SessionCommand::default());
        // With no legacy field, even a non-minimal input key/length is untouched.
        let nonminimal = b"\xda\x00\x86\x00\x82\x01\x03ctx";
        assert_eq!(rewrite_command(nonminimal).unwrap(), nonminimal);
        assert_eq!(input(nonminimal).prompt, "ctx");
    }

    #[test]
    fn canonical_presence_survives_merged_input_fragments_not_oneof_switches() {
        let canonical_false = envelope(b"\x90\x01\x00");
        let legacy_true = envelope(OLD_INPUT);
        let metadata_and_unknown = b"\x0a\x01c\xa0\x06\x01";
        for fragments in [
            [canonical_false.as_slice(), metadata_and_unknown.as_slice(), legacy_true.as_slice()],
            [legacy_true.as_slice(), metadata_and_unknown.as_slice(), canonical_false.as_slice()],
        ] {
            assert!(!input(&fragments.concat()).human_origin);
        }
        let switched = [canonical_false.as_slice(), b"\x62\x00", legacy_true.as_slice()].concat();
        assert!(input(&switched).human_origin);
        let ended = [canonical_false.as_slice(), legacy_true.as_slice(), b"\x62\x00"].concat();
        assert!(matches!(decode_stored_command(&ended).unwrap().kind, Some(Kind::Interrupt(_))));
    }

    #[test]
    fn malformed_input_never_falls_back_to_text_only() {
        let bad_fields: &[&[u8]] = &[
            // Zero tag, incomplete key, incomplete prompt length, truncated prompt.
            b"\x00",
            b"\x82",
            b"\x82\x01\x80",
            b"\x82\x01\x03x",
            // Invalid UTF-8 prompt, wrong prompt wire type, prompt encoded as a group.
            b"\x82\x01\x01\xff",
            b"\x85\x01abcd",
            b"\x83\x01\x84\x01",
            // Invalid wire type, unexpected end group, mismatched group, truncated fixed64.
            b"\x86\x01",
            b"\x84\x01",
            b"\xa3\x06\xac\x06",
            b"\xa1\x06abc",
            // Incomplete/overflowing legacy values; incomplete/wrong-wire canonical values.
            b"\x80\x01\x80",
            b"\x80\x01\xff\xff\xff\xff\xff\xff\xff\xff\xff\x02",
            b"\x90\x01\x80",
            b"\x92\x01\x01\x00",
            // Attachment with an invalid UTF-8 name.
            b"\x1a\x03\x0a\x01\xff",
        ];
        for field in bad_fields {
            let payload = [OLD_INPUT, *field].concat();
            assert!(decode_stored_command(&envelope(&payload)).is_err(), "accepted {field:02x?}");
        }
    }

    #[test]
    fn malformed_commands_and_excessive_groups_are_rejected() {
        for data in [b"\x80".as_slice(), b"\x5a\x80", b"\x5a\x05\x0a\x01x", b"\x58\x01", b"\x62\x02\x0a"] {
            assert!(decode_stored_command(data).is_err(), "accepted {data:02x?}");
        }
        let mut deep = b"\xa3\x06".repeat(101);
        deep.extend_from_slice(&b"\xa4\x06".repeat(101));
        assert!(decode_stored_command(&envelope(&deep)).is_err());
    }

    #[test]
    fn worker_journal_user_message_human_tag13_is_unchanged() {
        // Worker journals persist UserMessage events, not raw UserInput. Their
        // provenance tag did not collide and needs no compatibility migration.
        let recorded = b"\x12\x07Status?\x68\x01";
        let event = UserMessage::decode(recorded.as_slice()).unwrap();
        assert_eq!(event.text, "Status?");
        assert!(event.human_origin);
        assert_eq!(event.encode_to_vec(), recorded);
        assert!(!UserMessage::decode(b"\x12\x07Status?".as_slice()).unwrap().human_origin);
    }
}
