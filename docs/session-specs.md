# Session state and current configuration

A persisted `CreateSession` is both a creation plan and a cache of configuration
resolved by the head. Restarting must preserve the former without freezing the
latter at the version which created the session. `CreateSession::apply_derived`
in `c6-proto` is the explicit allowlist separating the two; new fields must be
classified here and deliberately added to that allowlist if appropriate.

## Field classification

**State** belongs to the session or its recorded history; a refresh cannot
replace it, even if the refresh envelope contains a value. **Derived** is
resolved again from current realm/instance settings, catalogs, grants, prompts,
and saved routing constraints. **Mixed** has explicit provenance.

| Tag | `CreateSession` field | Class and refresh rule |
| --- | --- | --- |
| 1 | `model` | State: connection, model, effort and context choices; switches use recorded events. |
| 2 | `cwd` | State: placement; changes use `PlaceChanged`. |
| 3 | `base_prompt` | Derived: current realm/side-chat prompt. A local delegated child's explicit Task instructions are retained. |
| 4 | `worktree` | State: initial worktree creation plan. |
| 5 | `mcp_servers_json` | Derived: permitted external MCP servers. |
| 6 | `skills_json` | Derived: current permitted skills and their instructions. |
| 7 | `seed` | State: forked/seeded conversation. |
| 8 | `hooks_json` | Derived: matching hooks. |
| 9 | `agents_json` | Derived: current named-agent catalog. |
| 10 | `labels` | State: session labels and routing inputs, not replaced by a configuration snapshot. |
| 11 | `secrets_json` | Derived: available secret names; never the secret values. |
| 12 | `ssh_target` | State: execution placement. |
| 13 | `web_search_json` | Derived: permitted search configuration. |
| 14 | `memories_json` | Derived: matching configured memory. |
| 15 | `policies_json` | Derived: matching policies; built-in protections still apply. |
| 16 | `subagent_model` | State: legacy explicit default, superseded through `SubagentModelSet`. |
| 17 | `memex_brief` | State/context: generated session context; not erased by a blank refresh. |
| 18 | `memex_tools` | Derived: memex availability. |
| 19 | `memex_mail` | Derived: mail availability. |
| 20 | `memex_calendar` | Derived: calendar availability. |
| 21 | `prompts_json` | Derived: current editable prompt catalog. |
| 22 | `delegation` | State: parent/root/call identity, depth and selected named agent. |
| 23 | `max_subagent_depth` | Derived: current depth limit. |
| 24 | `max_running_subagents` | Derived: current concurrency limit. |
| 25 | `memex_chat` | Derived: chat availability. |
| 26 | `subagent_target` | Mixed: current connection default unless an explicit session choice is recorded. An explicit choice does not authorize a revoked connection. |
| 27 | `subagent_connections_json` | Derived: current realm/SSH-filtered connection catalog. Present `[]` is authoritative; only absent legacy `""` uses the compatibility fallback. |
| 28 | `tools` | Derived inventory constraint from the saved route. Refresh does not reroute a session or widen a local child's narrowed cap. |
| 29 | `depth` | State: remote delegation tree position. |
| 30 | `subagent_templates_json` | Derived: currently available worker templates. |
| 31 | `auto_turns_per_hour` | Derived: retained legacy setting. |
| 32 | `claude_code_idle_minutes` | Derived: native idle policy. |
| 33 | `claude_code_tools` | Derived: legacy bridge-tool selection. |
| 34 | `claude_code_tools_chosen` | Derived: provenance of the legacy tool selection. |
| 35 | `claude_code` | Derived: current native/bridge settings. |
| 36 | `worktree_spec` | State: structured initial worktree plan. |
| 37 | `notice_quiet_seconds` | Derived: notice timing. |
| 38 | `notice_max_wait_seconds` | Derived: notice timing. |
| 39 | `notice_no_progress_limit` | Derived: notice loop guard. |
| 40 | `notice_failure_relaunch_limit` | Derived: failure guard. |
| 41 | `notice_tokens_per_hour` | Derived: automatic-work token budget. |
| 42 | `session_recovery_seconds` | Derived: recovery timing. |
| 43 | `notice_loop_backoff_rounds` | Derived: backoff policy. |
| 44 | `notice_loop_backoff_max_seconds` | Derived: backoff policy. |
| 45 | `disk_recovery_initial_seconds` | Derived: disk retry policy. |
| 46 | `disk_recovery_max_seconds` | Derived: disk retry policy. |
| 47 | `disk_warning_percent` | Derived: disk warning threshold. |
| 48 | `disk_warning_bytes` | Derived: disk warning threshold. |
| 49 | `keep_warm_json` | Derived: current cache-maintenance policy. |
| 50 | `keep_warm_snapshot_json` | State: worker-local provider prefix/move snapshot. |
| 51 | `claude_code_trim_enabled` | Derived: native context maintenance. |
| 52 | `claude_code_trim_at` | Derived: native context maintenance threshold. |
| 53 | `subagent_target_explicit` | State: provenance of an explicit session choice; old choice events reconstruct it. Clearing the choice follows current defaults. |

Worker command environment and compile-cache configuration are not
`CreateSession` fields. Existing command-environment and launch checks continue
to refresh those settings independently.

## Delivery and compatibility

Workers advertise `session_spec_refresh_v1`. On attach the head sends each
owned journal's current derived snapshot on the exact attaching connection,
fenced by ownership epoch, before replaying durable commands. A worker-wide
`SpecsReady` marker follows those snapshots. Recovery may reconstruct actors
before connecting, but cannot dispatch queued/recovered turns until their
ordered marker arrives. `Welcome.session_spec_refresh` makes this two-sided:
a new worker releases its gate for an old head, and an old worker receives no
new refresh commands or startup marker.

Each reconnect advances a local configuration generation. A previous stream's
marker cannot release the new attach, nor can the worker-wide release bypass
an actor's still-queued refresh. Actor registration and marker publication are
serialized so a concurrently loaded actor cannot miss the release. Recovery
continuation prompts are rendered only after the new snapshot is installed.

The head also refreshes before durable input, delegation completion and notice
release, and refreshes pending create/restore specs inline. Successful relevant
HTTP mutations broadcast new snapshots: settings, prompts, secrets, connection
availability, memories, app lifecycle, realms/grants, labels and backing
configuration/memory collections. The builder runs in each session's own realm,
not the mutation request's realm. A failed delivery does not undo a committed
settings mutation; subsequent attach/input recomputes it.

Direct programmatic database writes which bypass these HTTP hooks are observed
on the next attach or durable input, not through a storage changefeed. Head
assistant and observed/import-only sessions do not run this worker refresh
path. Saved route caps remain saved: editing routes does not silently move or
widen an existing session.

## Quiet application and replay

An actor coalesces pending snapshots and applies the latest only between turns.
A running turn and its tool continuations keep the runtime they captured.
Queued inputs wait behind pending configuration. The worker rebuilds its own
prompt, tools, hooks, policies, delegation defaults and limits without replacing
conversation history, model choice, placement, labels or delegation identity.
Revoked skills/agents disappear from advertisements and execution lookup;
present connection catalogs fail closed. Empty secret catalogs clear previously
captured tool secrets on the next turn.

`SpecRefreshed` records only the derived whitelist and effective prompt/tool
inventory before publication. Replay applies local snapshots after the import
boundary: historical snapshots cannot override a current restore envelope.
Rewind preserves configuration changes, just as it preserves placement, rather
than resurrecting revoked tools. A loaded runtime is rebuilt even when its
cached spec equals the head's snapshot, since historical `Started` may have an
older prompt. Equivalent snapshots produce neither another event nor a notice.
Changed snapshots add one quiet “Session configuration refreshed” notice; the
snapshot event itself is not a transcript message.

## Claude Code

Native configuration is applied only when the process is genuinely quiet:
no native background work, unread output or completion notification still
owned by the old process. Already-started native work may continue and be
recorded while configuration and queued new inputs wait. The supervised
maintenance slot retires the old process and resumes the same native
conversation with fresh guidance, bridge inventory and external MCP clients.
It retains the existing process-group absence, replacement-admission and Rewind
fences; refresh is not permission to kill active work or seed another history.

The bridge currently advertises `tools.listChanged: false`. Rather than assume
an old CLI will discover changed tools, refresh uses a conservative quiet
relaunch. A derived-spec fingerprint detects schema/catalog/prompt changes even
when tool names are identical. A no-op attach does not relaunch a healthy
native process merely because it attached.

## Regression coverage

- Protocol unit tests: explicit allowlist preserves state, legacy empty specs
  decode, and additive command/event wire tags remain stable.
- Worker integration `session_specs`: real journal restart/reattach after
  offline settings edits; queued mid-turn refresh and tool-continuation boundary;
  skill/agent revocation; legacy peer compatibility over mTLS.
- Worker unit tests: pre-catalog Claude Code journals gain current delegation
  guidance without changing native conversation identity; authoritative empty
  connection catalogs revoke legacy fallbacks; import boundaries and rewind
  retain current configuration; named children keep their original instructions
  after catalog removal; reconnect generations fence queued dispatch.
- Native stand-in process lifecycle: a changed spec fingerprint cannot replace
  a process while background work or its completion notification is owned.

These tests are written as part of this change; the implementing worktree does
not run builds or tests. Integration must compile and execute them.
