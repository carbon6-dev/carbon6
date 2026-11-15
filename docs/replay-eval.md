# Step replay of logged sessions

Status: a script (`scripts/replay-eval.py`), first run 2026-11-15.

## Why

To find out whether a change to what an agent sees costs it accuracy, without building or running anything. The first use: stubbing old tool output in long Claude Code subagents, whose contexts reached 400–970k tokens. Every call re-read them, and that drained the Claude Code plan.

## How it works

It runs where the transcript is, a worker, with the `claude` that wrote it:
1. It reads the Claude Code transcript (`~/.claude/projects/<folder>/<session>.jsonl`).
2. **Points:** it picks some late in the session. Each comes right after a batch of tool results (every call answered) and before a call that read at least `--min-context` tokens and made a tool call. The context size comes from that call's logged usage.
3. **Arms:** for each point it forks the history up to there into new Claude Code sessions in the same project folder: as it was (baseline), and with each `--policy` applied.
4. **Replay:** it resumes each fork with `claude -p "Continue." --resume <fork>` in the session's own folder, with the session's model. A PreToolUse hook blocks every tool and records the call, so nothing runs and the tools offered are unchanged. The first tool call (or text) is the arm's next step. Forks are deleted afterwards.
5. **Judging:** an identical call (same tool, same whole input) counts as "same". Otherwise a judge model (`--judge-model`, default Sonnet) compares it with the logged step: same, equivalent, or worse.
6. **Output:** results go to `--out` as JSON lines, one per point, with each arm's step, its usage and the verdict.

## Policies

- **`stub-old-tool-output`:** tool outputs older than the latest `--keep` (8), and longer than `--min-chars` (400), become "[Earlier output of ‹tool› ‹argument› removed to save context. Run the tool again if you need it.]". The calls and the agent's words stay.
- **`stub-old-keep-start`:** the same, but the first five outputs stay too (what the agent read to learn its task).

## Limits

- **One step only:** after a different choice the logged outputs no longer apply.
- **Baseline differs too:** the baseline doesn't always repeat the original step (the model isn't deterministic). Its agreement with the original is the noise floor a policy is measured against.
- **Uses the same plan:** a point costs each arm's whole context, written to the cache once, plus a judge call.
- **No MCP tools:** the replay runs with `--strict-mcp-config`, so carbon6's tools aren't offered. A logged step that called one can't be repeated exactly.

## First run (2026-11-15)

**What was run:**
- **Points:** 10, five each from two Falcon backlog subagents (SQL, execution), with logged contexts of 200–950k tokens.
- **Model:** Claude Opus, as logged.
- **Judge:** Sonnet.

| Arm | Judged same or equivalent | Judged worse | Context read (10 points) |
|---|---|---|---|
| baseline (as it was) | 4 | 6 | 5.3M |
| `stub-old-tool-output` | 1 | 9 | 2.1M (−62%) |
| `stub-old-keep-start` | 5 | 5 | 2.3M (−57%) |

**What it shows:**
- **The baseline's drift is the noise floor:** it rarely repeated the logged step exactly.
- **Plain stubbing loses the task:** it drops what the agent read at the start (its instruction and report files), and the agent goes back to re-read them.
- **Keeping the first five outputs** stayed within the baseline's noise while reading 57% less.

**Next:** about 30 points, judging each policy against the baseline from the same point rather than against the logged step, and passing the logged effort.

## Second run (2026-11-15): 30 points, judged against the baseline

**What was run:**
- **Points:** 30, six each from five Falcon backlog subagents (SQL, execution, OS, planner, KQL), with logged contexts of 200–950k tokens.
- **Arms:** baseline and `stub-old-keep-start`.
- **Effort:** as logged (medium).
- **Judge:** `--judge-against baseline`, pairwise and in random order.

| | Count |
|---|---|
| Policy judged better than the baseline | 11 |
| Equivalent | 9 |
| Worse | 10 |
| Context read: baseline vs policy | 14.3M vs 6.4M (−56%) |

**What it shows:**
- **No measurable loss in step quality** at less than half the context.
- **Six of the ten "worse" points** are the agent re-reading instruction files it was handed mid-run (`FIX_PROMPT.md`, `red1-*.md`, `WAVE2_PROMPT.md`). They came after the first five outputs, so they were stubbed. Keeping the first few outputs after every new message, not only at the start, should cover them.
- **By area:** planner 0/6 worse, KQL 4/6.
- **The judge** sees only the task's opening instructions, so it can't tell when a later instruction made a read relevant.
