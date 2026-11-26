//! Conservative, in-place trimming of Claude Code's JSONL transcript.
//!
//! Preparation is read-only. The caller must stop Claude Code before applying
//! the prepared edit, and must not restart it until `apply` returns.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use c6_agent::{TRIM_KEEP_AFTER_INSTRUCTION as KEEP_AFTER_INSTRUCTION, TRIM_KEEP_LATEST as KEEP_LATEST, TRIM_MIN_CHARS as MIN_CHARS};
const MARK: &str = "\u{2060}";

#[derive(Debug)]
pub struct Prepared {
    /// Number of tool-result blocks replaced.
    pub count: usize,
    /// Serialized live message-content bytes, excluding sidecars and metadata.
    pub before: usize,
    pub after: usize,
    path: PathBuf,
    original: Vec<u8>,
    replacement: Vec<u8>,
}

impl Prepared {
    /// Atomically replace the same transcript path, keeping its session ID.
    ///
    /// Claude Code MUST be stopped: the byte check detects changes since
    /// preparation, but cannot lock out a writer racing the final rename.
    pub fn apply(self) -> Result<()> {
        let metadata = std::fs::symlink_metadata(&self.path).context("stat Claude Code transcript")?;
        ensure!(metadata.file_type().is_file(), "Claude Code transcript is not a regular file");
        let parent = self.path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let mut temp = tempfile::NamedTempFile::new_in(parent).context("create trimmed transcript")?;
        temp.as_file().set_permissions(metadata.permissions())?;
        temp.write_all(&self.replacement)?;
        temp.as_file().sync_all()?;
        ensure!(std::fs::read(&self.path)? == self.original, "Claude Code transcript changed since trim preparation");
        // Refuse a path changed into a symlink, even if its target is identical.
        ensure!(std::fs::symlink_metadata(&self.path)?.file_type().is_file(), "Claude Code transcript is not a regular file");
        temp.persist(&self.path).context("replace Claude Code transcript")?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }
}

/// Prepare a trim only when it saves at least 20% of the live context.
///
/// All lines are parsed, including the compacted prefix; malformed JSON or a
/// missing final newline fails closed. Only text-only tool results in the main
/// conversation after the latest compact boundary can change. Unknown result
/// shapes, ambiguous sidecars, and media-bearing results are left untouched.
pub fn prepare(path: &Path, stub: &str) -> Result<Option<Prepared>> {
    let metadata = std::fs::symlink_metadata(path).context("stat Claude Code transcript")?;
    ensure!(metadata.file_type().is_file(), "Claude Code transcript is not a regular file");
    let original = std::fs::read(path).context("read Claude Code transcript")?;
    if original.is_empty() {
        return Ok(None);
    }
    ensure!(original.ends_with(b"\n"), "truncated Claude Code JSONL (missing final newline)");
    let raw: Vec<&[u8]> = original.split_inclusive(|b| *b == b'\n').collect();
    let mut lines = Vec::with_capacity(raw.len());
    for (i, bytes) in raw.iter().enumerate() {
        let line: Value = serde_json::from_slice(bytes).with_context(|| format!("invalid Claude Code JSONL line {}", i + 1))?;
        ensure!(line.is_object() && line.get("type").and_then(Value::as_str).is_some(), "invalid Claude Code record on line {}", i + 1);
        if matches!(line["type"].as_str(), Some("user" | "assistant")) {
            ensure!(line["message"].is_object(), "invalid Claude Code message on line {}", i + 1);
            ensure!(
                line["message"]["content"].is_string() || line["message"]["content"].is_array(),
                "invalid Claude Code content on line {}",
                i + 1
            );
        }
        lines.push(line);
    }
    let start = lines.iter().rposition(|line| line["isSidechain"] != true && line["subtype"] == "compact_boundary").unwrap_or(0);
    let before_bytes = live_bytes(&lines[start..]);
    let mut calls = HashMap::new();
    let mut results = Vec::new();
    let mut since_instruction = None::<usize>;
    for (i, line) in lines.iter().enumerate().skip(start) {
        if line["isSidechain"] == true {
            continue;
        }
        if instruction(line) {
            since_instruction = Some(0);
        }
        let Some(blocks) = line["message"]["content"].as_array() else { continue };
        for (j, block) in blocks.iter().enumerate() {
            if line["type"] == "assistant"
                && block["type"] == "tool_use"
                && let (Some(id), Some(name)) = (block["id"].as_str(), block["name"].as_str())
            {
                let arg = ["file_path", "path", "pattern", "command", "cmd", "query", "url"]
                    .iter()
                    .find_map(|key| block["input"][*key].as_str())
                    .unwrap_or("");
                let arg: String = arg.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect();
                calls.insert(id.to_owned(), if arg.is_empty() { name.to_owned() } else { format!("{name} {arg}") });
            }
            if line["type"] == "user" && block["type"] == "tool_result" {
                let keep = since_instruction.is_some_and(|n| n < KEEP_AFTER_INSTRUCTION);
                if let Some(n) = since_instruction.as_mut() {
                    *n += 1;
                }
                results.push((i, j, keep));
            }
        }
    }
    let mut changed = vec![false; lines.len()];
    let mut count = 0;
    for &(i, j, keep) in results.iter().take(results.len().saturating_sub(KEEP_LATEST)) {
        if keep {
            continue;
        }
        let line = &mut lines[i];
        let block = &line["message"]["content"][j];
        let Some(text) = result_text(&block["content"]) else { continue };
        if text.starts_with(MARK) || text.chars().count() < MIN_CHARS {
            continue;
        }
        // Keep unpaired/unknown calls and media, including media stored only in
        // a sidecar. A sidecar belongs to a whole line, not an individual block.
        let Some(call) = block["tool_use_id"].as_str().and_then(|id| calls.get(id)) else { continue };
        if has_media(line) {
            continue;
        }
        if line.get("toolUseResult").is_some()
            && line["message"]["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_result").count() != 1
        {
            continue;
        }
        let replacement = format!("{MARK}{}", stub.replace("{{call}}", call));
        if replacement.len() >= text.len() {
            continue;
        }
        line["message"]["content"][j]["content"] = Value::String(replacement.clone());
        if let Some(sidecar) = line.get_mut("toolUseResult") {
            // Claude Code accepts string sidecars as well as structured tool
            // data; never leave a second, untrimmed copy in toolUseResult.
            *sidecar = Value::String(replacement);
        }
        changed[i] = true;
        count += 1;
    }
    let after_bytes = live_bytes(&lines[start..]);
    if count == 0 || (after_bytes as u128) * 5 > (before_bytes as u128) * 4 {
        return Ok(None);
    }
    let mut replacement = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if changed[i] {
            serde_json::to_writer(&mut replacement, line)?;
            // Preserve the changed line's newline convention as well.
            replacement.extend_from_slice(if raw[i].ends_with(b"\r\n") { b"\r\n" } else { b"\n" });
        } else {
            replacement.extend_from_slice(raw[i]);
        }
    }
    Ok(Some(Prepared { count, before: before_bytes, after: after_bytes, path: path.to_owned(), original, replacement }))
}

fn instruction(line: &Value) -> bool {
    if line["type"] != "user" || line["isMeta"] == true {
        return false;
    }
    let content = &line["message"]["content"];
    if let Some(text) = content.as_str() {
        return !text.trim().is_empty();
    }
    content
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "text") && !blocks.iter().any(|b| b["type"] == "tool_result"))
}

fn result_text(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let blocks = content.as_array()?;
    let mut text = String::new();
    for block in blocks {
        // Do not discard images, documents, or future/unknown block types.
        if block["type"] != "text" {
            return None;
        }
        text.push_str(block["text"].as_str()?);
    }
    Some(text)
}

fn has_media(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.get("isImage").and_then(Value::as_bool) == Some(true)
                || matches!(
                    map.get("type").and_then(Value::as_str),
                    Some("image" | "image_url" | "document" | "pdf" | "audio" | "input_audio" | "video")
                )
                || map.values().any(has_media)
        }
        Value::Array(items) => items.iter().any(has_media),
        _ => false,
    }
}

fn live_bytes(lines: &[Value]) -> usize {
    lines
        .iter()
        .filter(|line| line["isSidechain"] != true && matches!(line["type"].as_str(), Some("user" | "assistant")))
        .map(|line| serde_json::to_vec(&line["message"]["content"]).expect("JSON value serializes").len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const STUB: &str = "[Earlier output of {{call}} removed. Run the tool again if needed.]";

    fn instruction_line(text: &str) -> Value {
        json!({"type":"user", "uuid":text, "message":{"role":"user", "content":text}})
    }

    fn round(n: usize, text: &str) -> [Value; 2] {
        let id = format!("tool-{n}");
        [
            json!({"type":"assistant", "uuid":format!("call-{n}"), "message":{"role":"assistant", "content":[
                {"type":"text", "text":"Assistant explanation stays"},
                {"type":"tool_use", "id":id, "name":"Read", "input":{"file_path":format!("/src/file-{n}")}}
            ]}}),
            json!({"type":"user", "uuid":format!("result-{n}"), "parentUuid":format!("call-{n}"), "sessionId":"same-session", "message":{"role":"user", "content":[
                {"type":"tool_result", "tool_use_id":id, "content":text, "is_error":false}
            ]}}),
        ]
    }

    fn fixture() -> Vec<Value> {
        let mut lines = vec![instruction_line("First instruction")];
        for n in 0..24 {
            lines.extend(round(n, &"output ".repeat(1000)));
        }
        lines
    }

    fn write(dir: &tempfile::TempDir, lines: &[Value]) -> PathBuf {
        let path = dir.path().join("same-session.jsonl");
        let bytes: Vec<u8> = lines
            .iter()
            .flat_map(|line| {
                let mut bytes = serde_json::to_vec(line).unwrap();
                bytes.push(b'\n');
                bytes
            })
            .collect();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn read(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
    }

    fn trimmed(line: &Value) -> bool {
        line["message"]["content"].as_array().is_some_and(|blocks| {
            blocks.iter().any(|b| b["type"] == "tool_result" && b["content"].as_str().is_some_and(|s| s.starts_with(MARK)))
        })
    }

    #[test]
    fn preserves_first_five_latest_eight_calls_text_ids_and_exact_unchanged_lines() {
        let dir = tempfile::tempdir().unwrap();
        let lines = fixture();
        let path = write(&dir, &lines);
        // Deliberately noncanonical spacing/CRLF on unchanged instruction.
        let original = std::fs::read(&path).unwrap();
        let mut custom = b"{ \"type\": \"user\", \"uuid\": \"First instruction\", \"message\": {\"role\":\"user\",\"content\":\"First instruction\"} }\r\n".to_vec();
        custom.extend_from_slice(&original[original.iter().position(|b| *b == b'\n').unwrap() + 1..]);
        std::fs::write(&path, &custom).unwrap();
        let prepared = prepare(&path, STUB).unwrap().unwrap();
        assert_eq!(prepared.count, 11);
        assert!(prepared.after * 5 <= prepared.before * 4);
        assert_eq!(std::fs::read(&path).unwrap(), custom, "prepare is read-only");
        prepared.apply().unwrap();
        let after = read(&path);
        let bytes = std::fs::read(&path).unwrap();
        let before_raw: Vec<_> = custom.split_inclusive(|b| *b == b'\n').collect();
        let after_raw: Vec<_> = bytes.split_inclusive(|b| *b == b'\n').collect();
        for (i, line) in after.iter().enumerate() {
            if trimmed(line) {
                assert_eq!(line["uuid"], lines[i]["uuid"]);
                assert_eq!(line["parentUuid"], lines[i]["parentUuid"]);
                assert_eq!(line["sessionId"], "same-session");
                let n = (i - 2) / 2;
                assert!((5..16).contains(&n));
                assert_eq!(line["message"]["content"][0]["tool_use_id"], format!("tool-{n}"));
                assert!(line["message"]["content"][0]["content"].as_str().unwrap().contains(&format!("Read /src/file-{n}")));
            } else {
                assert_eq!(before_raw[i], after_raw[i], "line {i} changed");
            }
        }
        assert!(prepare(&path, "An edited stub should not replace existing markers").unwrap().is_none());
    }

    #[test]
    fn every_instruction_protects_its_first_five_and_meta_does_not_reset() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        lines.insert(1 + 12 * 2, instruction_line("New instruction"));
        let mut meta = instruction_line("Metadata is not an instruction");
        meta["isMeta"] = json!(true);
        lines.insert(1 + 7 * 2, meta);
        let path = write(&dir, &lines);
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        let after = read(&path);
        for n in 12..17 {
            let result = after.iter().find(|line| line["uuid"] == format!("result-{n}")).unwrap();
            assert!(!trimmed(result));
        }
        assert!(after.iter().any(|line| line["uuid"] == "result-7" && trimmed(line)));
    }

    #[test]
    fn compacted_prefix_is_byte_identical_and_not_used_for_savings_or_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        lines.push(json!({"type":"system", "subtype":"compact_boundary", "uuid":"boundary"}));
        let start = lines.len();
        lines.extend(fixture());
        let path = write(&dir, &lines);
        let prepared = prepare(&path, STUB).unwrap().unwrap();
        assert_eq!(prepared.count, 11);
        assert_eq!(prepared.before, live_bytes(&lines[start..]));
        prepared.apply().unwrap();
        assert_eq!(read(&path)[..start], lines[..start]);
    }

    #[test]
    fn short_and_unicode_outputs_are_kept_by_character_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        lines[2 + 6 * 2]["message"]["content"][0]["content"] = json!("x".repeat(399));
        lines[2 + 7 * 2]["message"]["content"][0]["content"] = json!("界".repeat(399));
        lines[2 + 8 * 2]["message"]["content"][0]["content"] = json!("界".repeat(400));
        let path = write(&dir, &lines);
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        let after = read(&path);
        assert_eq!(after[14], lines[14]);
        assert_eq!(after[16], lines[16]);
        assert!(trimmed(&after[18]));
    }

    #[test]
    fn media_unknown_blocks_and_unpaired_results_are_never_removed() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        lines[14]["message"]["content"][0]["content"] = json!([
            {"type":"text", "text":"x".repeat(7000)},
            {"type":"image", "source":{"type":"base64", "data":"image-bytes"}}
        ]);
        lines[16]["message"]["content"][0]["content"] = json!([
            {"type":"text", "text":"x".repeat(7000)}, {"type":"future-block", "data":"keep"}
        ]);
        lines[18]["toolUseResult"] = json!({"nested":{"type":"image", "data":"keep"}});
        lines[20]["message"]["content"][0]["tool_use_id"] = json!("missing-call");
        let path = write(&dir, &lines);
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        let after = read(&path);
        for i in [14, 16, 18, 20] {
            assert_eq!(after[i], lines[i]);
        }
    }

    #[test]
    fn native_is_image_sidecar_flag_preserves_the_whole_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        lines[14]["toolUseResult"] = json!({"isImage":true, "stdout":"image bytes", "stderr":""});
        lines[16]["toolUseResult"] = json!({"isImage":false, "stdout":"ordinary output", "stderr":""});
        let path = write(&dir, &lines);
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        let after = read(&path);
        assert_eq!(after[14], lines[14], "native isImage does not require a nested type:image block");
        assert!(trimmed(&after[16]), "the ordinary isImage:false shape is still eligible");
        assert_eq!(after[16]["toolUseResult"], after[16]["message"]["content"][0]["content"]);
    }

    #[test]
    fn other_media_in_sidecars_preserves_the_whole_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        for n in 24..28 {
            lines.extend(round(n, &"output ".repeat(1000)));
        }
        for (i, kind) in ["image_url", "document", "pdf", "audio", "input_audio", "video"].into_iter().enumerate() {
            lines[14 + i * 2]["toolUseResult"] = json!({"nested":{"type":kind, "data":"media bytes"}});
        }
        let path = write(&dir, &lines);
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        let after = read(&path);
        for i in (14..26).step_by(2) {
            assert_eq!(after[i], lines[i], "sidecar media must not be replaced by a text stub");
        }
    }

    #[test]
    fn sidecars_are_replaced_for_string_object_and_array_and_absent_stays_absent() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        lines[14]["toolUseResult"] = json!("old output".repeat(1000));
        lines[16]["toolUseResult"] = json!({"stdout":"old output".repeat(1000), "stderr":"old error"});
        lines[18]["toolUseResult"] = json!([{"type":"text", "text":"old output".repeat(1000)}]);
        lines[20]["message"]["content"][0]["content"] = json!([{ "type":"text", "text":"array text".repeat(1000) }]);
        let path = write(&dir, &lines);
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        let after = read(&path);
        for i in [14, 16, 18] {
            assert!(trimmed(&after[i]));
            assert_eq!(after[i]["toolUseResult"], after[i]["message"]["content"][0]["content"]);
        }
        assert!(trimmed(&after[20]));
        assert!(after[20].get("toolUseResult").is_none());
    }

    #[test]
    fn mixed_user_text_and_multi_result_sidecars_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        lines[14]["message"]["content"].as_array_mut().unwrap().push(json!({"type":"text", "text":"User text must remain"}));
        let second = lines[16]["message"]["content"][0].clone();
        lines[16]["message"]["content"].as_array_mut().unwrap().push(second);
        lines[16]["toolUseResult"] = json!({"stdout":"ambiguous"});
        let path = write(&dir, &lines);
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        let after = read(&path);
        assert!(trimmed(&after[14]));
        assert_eq!(after[14]["message"]["content"][1], lines[14]["message"]["content"][1]);
        assert_eq!(after[16], lines[16]);
    }

    #[test]
    fn sidechains_including_their_compaction_are_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        let mut side = instruction_line("Sidechain instruction");
        side["isSidechain"] = json!(true);
        side["subtype"] = json!("compact_boundary");
        lines.insert(15, side);
        lines[17]["isSidechain"] = json!(true);
        let path = write(&dir, &lines);
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        let after = read(&path);
        assert_eq!(after[15], lines[15]);
        assert_eq!(after[17], lines[17]);
        assert!(after.iter().any(trimmed));
    }

    #[test]
    fn low_savings_and_larger_stubs_do_not_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let mut lines = fixture();
        lines.push(instruction_line(&"text context ".repeat(100_000)));
        let path = write(&dir, &lines);
        assert!(prepare(&path, STUB).unwrap().is_none());
        let path = write(&dir, &fixture());
        assert!(prepare(&path, &"giant stub".repeat(10_000)).unwrap().is_none());
        let path = write(&dir, &[instruction_line("Short session")]);
        assert!(prepare(&path, STUB).unwrap().is_none());
    }

    #[test]
    fn malformed_or_truncated_jsonl_fails_closed_even_before_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jsonl");
        for bytes in [
            b"{\"type\":\"user\",\"message\":{\"content\":\"complete JSON without newline\"}}".as_slice(),
            b"{\"type\":\"user\"\n",
            b"{}\n",
            b"[]\n",
            b"\n",
            b"{\"type\":\"user\",\"message\":{}}\n",
            b"{broken}\n{\"type\":\"system\",\"subtype\":\"compact_boundary\"}\n",
            b"{\"type\":\"system\",\"data\":\"\xff\"}\n",
        ] {
            std::fs::write(&path, bytes).unwrap();
            assert!(prepare(&path, STUB).is_err(), "accepted {bytes:?}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        std::fs::write(&path, []).unwrap();
        assert!(prepare(&path, STUB).unwrap().is_none());
    }

    #[test]
    fn apply_refuses_any_changed_original_even_equal_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, &fixture());
        let prepared = prepare(&path, STUB).unwrap().unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let index = bytes.iter().position(|b| *b == b'F').unwrap();
        bytes[index] = b'f';
        std::fs::write(&path, &bytes).unwrap();
        assert!(prepared.apply().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "temporary file cleaned up");
    }

    #[cfg(unix)]
    #[test]
    fn apply_preserves_permissions_and_rejects_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = write(&dir, &fixture());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        prepare(&path, STUB).unwrap().unwrap().apply().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        let path = write(&dir, &fixture());
        let prepared = prepare(&path, STUB).unwrap().unwrap();
        let other = dir.path().join("other.jsonl");
        std::fs::rename(&path, &other).unwrap();
        symlink(&other, &path).unwrap();
        assert!(prepare(&path, STUB).is_err());
        assert!(prepared.apply().is_err());
    }
}
