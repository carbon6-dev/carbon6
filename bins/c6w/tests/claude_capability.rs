//! A native CLI stand-in invokes the real worker command hook on documented
//! JSON inputs. No provider, running head, or native CLI installation is used.
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

const POLICY: &str = r#"{"capabilities":[{"name":"inspect widget","trigger_phrases":["widget status"],"command_patterns":["widget-check"]}],"reminder":"Use capability {{capabilities}}."}"#;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "c6-hook-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn native_standin_receives_prompt_feedback_and_blocks_only_first_matching_tool_per_turn() {
    let dir = Scratch::new();
    let script = dir.0.join("claude");
    std::fs::write(&script, r#"#!/bin/sh
set -eu
read -r prompt
for turn in 1 2; do
  printf '%s\n' '{"hook_event_name":"UserPromptSubmit","session_id":"widget","prompt":"widget status"}' | eval "$C6_TEST_HOOK"
  for attempt in 1 2; do
    feedback=$(printf '%s\n' '{"hook_event_name":"PreToolUse","session_id":"widget","tool_name":"Bash","tool_input":{"command":"widget-check"}}' | eval "$C6_TEST_HOOK")
    if printf '%s' "$feedback" | grep -q '"permissionDecision":"deny"'; then
      printf '%s\n' "$feedback"
      printf 'blocked %s %s\n' "$turn" "$attempt" >> actions
    else
      printf 'executed %s %s\n' "$turn" "$attempt" >> actions
    fi
  done
done
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let hook = format!("{} claude-capability {}", quote(env!("CARGO_BIN_EXE_c6w")), quote(POLICY));
    let mut child = Command::new(&script)
        .current_dir(&dir.0)
        .env("C6_TEST_HOOK", hook)
        .env(c6_worker::claude_capability::STATE_ENV, dir.0.join("state"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"inspect the widget\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let feedback = String::from_utf8(output.stdout).unwrap();
    assert_eq!(feedback.matches("\"additionalContext\":\"Use capability inspect widget.\"").count(), 2);
    assert_eq!(feedback.matches("\"permissionDecision\":\"deny\"").count(), 2);
    assert_eq!(std::fs::read_to_string(dir.0.join("actions")).unwrap(), "blocked 1 1\nexecuted 1 2\nblocked 2 1\nexecuted 2 2\n");
}

#[test]
fn native_unrelated_tool_has_no_permission_verdict_and_invalid_state_fails_closed() {
    let dir = Scratch::new();
    let invoke = |tool: &str, state: PathBuf| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_c6w"))
            .args(["claude-capability", POLICY])
            .env(c6_worker::claude_capability::STATE_ENV, state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(tool.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let unrelated = r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"other"}}"#;
    let allowed = invoke(unrelated, dir.0.join("state"));
    assert!(allowed.status.success());
    assert!(allowed.stdout.is_empty(), "must not return allow and override another guard");
    let missing = invoke(unrelated, dir.0.join("absent").join("state"));
    assert_eq!(missing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&missing.stderr).contains("this action was not run"));
}
