# Importing sessions from Claude Code and Codex — Design

Status: built (2026-10-17).

## 1. What it does

Sessions you had in Claude Code or Codex come into carbon6 as sessions:

- **In the Sessions list and search,** with their folder, branch, messages, tool calls and token usage, marked as imported from Claude Code or Codex.
- **In the memex,** their digests are written like those of carbon6's own sessions, so later sessions' briefs include them.
- **Continued in carbon6:**
  - A Claude Code session, through a Claude Code connection, keeps its id. It resumes with Claude Code's own memory of it, on the machine that has its file (if that machine no longer has it, Claude Code starts over from a transcript of the import).
  - Either kind can also be continued with any other connection: carbon6's agent continues from the converted history. Switching between Claude Code and other connections later is a handoff with a transcript (docs/architecture.md, "Model switching").

## 2. How

- **The worker reads; the head keeps.** A worker you turn imports on for looks every minute at `~/.claude/projects/**/*.jsonl` and `~/.codex/sessions/**/*.jsonl` (and `~/.codex/archived_sessions`). It reads only files that changed, and converts each into carbon6 session events:
  - Started;
  - your messages;
  - turns;
  - the assistant's text, thinking and tool calls;
  - tool results;
  - usage.

  The worker sends only the events the head doesn't have yet.
- **The head** creates the session the first time:
  - Its id is the tool's own (a Claude Code session's id is what Claude Code resumes by).
  - It belongs to the worker that read it, in the realm its folder maps to.
  - It has no connection until you continue it.

  The head stores the events and keeps the memex's digests. Imports never start triggers, notifications or budget counting: they are history.
- **Continuing** is sending a message. The worker gets the session from the head, as for any session it lacks, and the turn runs with the connection you pick.
- **Once continued in carbon6, a session is carbon6's.** Imports stop changing it, since Claude Code would write carbon6's own turns back into its file.

## 3. Settings

`session_imports` (Settings → System → Imports):

- `workers`: the workers that import (none at first);
- `sources`: `claude_code`, `codex`;
- `rules`: `[{folder, realm}]`, in order. A session whose folder is under `folder` (`~` is the worker's home) goes to `realm`; anything else goes to home.

The head takes imports only from the workers listed, and only into realms the worker is available to (Settings → Workers): a session whose folder maps to another realm is refused.

## 4. Not done

- Claude Code's subagents' side chains (`isSidechain`) are left out, as are attachments' bytes (images pasted into Claude Code).
- Codex's encrypted reasoning is kept only as its summary.
