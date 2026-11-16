#!/usr/bin/env python3
"""Step replay of a logged Claude Code session (docs/replay-eval.md).

At points late in a logged session, fork its Claude Code transcript twice,
as it was (baseline) and with a context policy applied, resume each with
`claude -p`, and record the next action Claude chooses. A PreToolUse hook
blocks every tool, so nothing runs and the tool list is unchanged. A judge
model then compares each replayed action with the one the session took.

Run it where the transcript is (a worker), with the `claude` that wrote it:

  scripts/replay-eval.py --transcript ~/.claude/projects/<dir>/<id>.jsonl \
      --points 10 --policy stub-old-tool-output --out results.jsonl

Only the step after each point is replayed: after a different choice the
logged tool outputs no longer apply. It costs one model call per point and
arm (the arm's whole context, written to the cache once) plus a judge call.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

STUB = "[Earlier output of {what} removed to save context. Run the tool again if you need it.]"
CONTINUE = "Continue."


def load(path):
    return [json.loads(line) for line in Path(path).read_text().splitlines() if line.strip()]


def blocks(line):
    content = (line.get("message") or {}).get("content")
    return content if isinstance(content, list) else []


def is_tool_result(line):
    return line.get("type") == "user" and any(b.get("type") == "tool_result" for b in blocks(line))


def context_of(line):
    """The context size of the call that wrote this assistant line."""
    u = (line.get("message") or {}).get("usage") or {}
    return u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0) + u.get("cache_creation_input_tokens", 0)


def next_action(lines, i):
    """The assistant reply that followed line i: its tool calls and text,
    from the lines of the next request, and that call's context size."""
    calls, text, request, ctx = [], [], None, 0
    for line in lines[i + 1 :]:
        if line.get("type") != "assistant":
            if request is not None and line.get("type") == "user":
                break
            continue
        rid = line.get("requestId") or line["message"].get("id")
        if request is None:
            request, ctx = rid, context_of(line)
        elif rid != request:
            break
        for b in blocks(line):
            if b.get("type") == "tool_use":
                calls.append({"name": b["name"], "input": b.get("input", {})})
            elif b.get("type") == "text" and b.get("text", "").strip():
                text.append(b["text"])
    return {"calls": calls, "text": "\n".join(text)}, ctx


def points(lines, n, min_context):
    """Up to n cut points, spread over the late part of the session: after a
    tool result, where the next call read at least min_context tokens and
    made a tool call."""
    found = []
    for i, line in enumerate(lines):
        if line.get("isSidechain") or not is_tool_result(line):
            continue
        # Only after a batch's last result: every call in the prefix
        # answered, as the model saw it.
        following = next((l for l in lines[i + 1 :] if l.get("type") in ("user", "assistant")), None)
        if following is None or following.get("type") != "assistant":
            continue
        action, ctx = next_action(lines, i)
        if ctx >= min_context and action["calls"]:
            found.append((i, action, ctx))
    if len(found) <= n:
        return found
    step = len(found) / n
    return [found[int(k * step)] for k in range(n)]


def tool_names(lines):
    """Each tool call's name and its main argument, by id (for stubs)."""
    out = {}
    for line in lines:
        for b in blocks(line):
            if b.get("type") == "tool_use":
                inp = b.get("input") or {}
                arg = inp.get("file_path") or inp.get("path") or inp.get("pattern") or inp.get("command") or ""
                out[b["id"]] = f"{b['name']} {str(arg)[:80]}".strip()
    return out


def result_text(block):
    c = block.get("content")
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "".join(x.get("text", "") for x in c if isinstance(x, dict))
    return ""


def stub_old_tool_output(prefix, keep, min_chars, keep_first=0):
    """The policy: tool outputs older than the last `keep`, and longer than
    `min_chars`, replaced by a stub naming the call. The calls and the
    agent's own words stay; so do the first `keep_first` outputs (what it
    read to learn its task)."""
    names = tool_names(prefix)
    results = [(i, j) for i, line in enumerate(prefix) for j, b in enumerate(blocks(line)) if b.get("type") == "tool_result"]
    old = results[keep_first : max(keep_first, len(results) - keep)]
    out = [json.loads(json.dumps(line)) for line in prefix]
    stubbed = 0
    for i, j in old:
        b = out[i]["message"]["content"][j]
        if len(result_text(b)) < min_chars:
            continue
        b["content"] = STUB.format(what=names.get(b.get("tool_use_id"), "a tool call"))
        if "toolUseResult" in out[i]:
            out[i]["toolUseResult"] = b["content"]
        stubbed += 1
    return out, stubbed


def is_instruction(line):
    """A message to the agent (its task, or a later one), not a tool result."""
    if line.get("type") != "user" or line.get("isMeta"):
        return False
    c = (line.get("message") or {}).get("content")
    if isinstance(c, str):
        return bool(c.strip())
    return isinstance(c, list) and any(b.get("type") == "text" for b in c) and not any(b.get("type") == "tool_result" for b in c)


def stub_old_keep_turn_starts(prefix, keep, min_chars, keep_first=5):
    """stub-old-tool-output, keeping the first `keep_first` outputs after
    every message to the agent: what it read to learn each instruction
    (an instruction handed over mid-run points at files too)."""
    kept = set()
    count = None
    for i, line in enumerate(prefix):
        if is_instruction(line):
            count = 0
        for j, b in enumerate(blocks(line)):
            if b.get("type") == "tool_result" and count is not None and count < keep_first:
                kept.add((i, j))
                count += 1
    names = tool_names(prefix)
    results = [(i, j) for i, line in enumerate(prefix) for j, b in enumerate(blocks(line)) if b.get("type") == "tool_result"]
    old = [r for r in results[: max(0, len(results) - keep)] if r not in kept]
    out = [json.loads(json.dumps(line)) for line in prefix]
    stubbed = 0
    for i, j in old:
        b = out[i]["message"]["content"][j]
        if len(result_text(b)) < min_chars:
            continue
        b["content"] = STUB.format(what=names.get(b.get("tool_use_id"), "a tool call"))
        if "toolUseResult" in out[i]:
            out[i]["toolUseResult"] = b["content"]
        stubbed += 1
    return out, stubbed


POLICIES = {
    "stub-old-tool-output": stub_old_tool_output,
    "stub-old-keep-start": lambda prefix, keep, min_chars: stub_old_tool_output(prefix, keep, min_chars, keep_first=5),
    "stub-old-keep-turn-starts": stub_old_keep_turn_starts,
}


def fork(prefix, project_dir):
    """The prefix as a new Claude Code session in the transcript's project
    folder; returns its id."""
    sid = str(uuid.uuid4())
    with open(Path(project_dir) / f"{sid}.jsonl", "w") as f:
        for line in prefix:
            line = dict(line)
            if "sessionId" in line:
                line["sessionId"] = sid
            f.write(json.dumps(line) + "\n")
    return sid


def drop_fork(project_dir, sid):
    for p in [Path(project_dir) / f"{sid}.jsonl", Path(project_dir) / sid]:
        if p.is_dir():
            shutil.rmtree(p, ignore_errors=True)
        elif p.exists():
            p.unlink()


def replay(sid, cwd, model, effort, work, timeout):
    """Resumes a fork until Claude picks its next action; the hook blocks
    and records the tool call, then the process is stopped. Returns the
    action (tool call, or text if it chose none) and the call's usage."""
    called = Path(work) / f"{sid}.tool"
    settings = {
        "hooks": {
            "PreToolUse": [
                {"matcher": "*", "hooks": [{"type": "command", "command": f"cat > {called}; echo 'replay: not run' >&2; exit 2"}]}
            ]
        }
    }
    cmd = [
        "claude", "-p", CONTINUE, "--resume", sid, "--model", model, *(["--effort", effort] if effort else []),
        "--output-format", "stream-json", "--verbose",
        "--settings", json.dumps(settings), "--setting-sources", "", "--strict-mcp-config",
    ]
    proc = subprocess.Popen(cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    action, usage, deadline = None, {}, time.time() + timeout
    try:
        for raw in proc.stdout:
            try:
                ev = json.loads(raw)
            except ValueError:
                continue
            if ev.get("type") == "assistant":
                msg = ev.get("message") or {}
                usage = usage or msg.get("usage") or {}
                calls = [{"name": b["name"], "input": b.get("input", {})} for b in msg.get("content", []) if b.get("type") == "tool_use"]
                text = "".join(b.get("text", "") for b in msg.get("content", []) if b.get("type") == "text")
                if calls:
                    action = {"calls": calls, "text": text}
                    break
                if text.strip() and action is None:
                    action = {"calls": [], "text": text}
            if ev.get("type") == "result" or time.time() > deadline:
                break
    finally:
        proc.kill()
        proc.wait()
    if called.exists():
        called.unlink()
    return action or {"calls": [], "text": "(no reply)"}, usage


def key(call):
    """A call as compared: its tool and its whole input (an Edit's text
    too), whitespace normalised."""
    inp = json.dumps(call.get("input") or {}, sort_keys=True)
    return call["name"], re.sub(r"\s+", " ", inp).strip()


def same(a, b):
    return bool(a["calls"]) and [key(c) for c in a["calls"]] == [key(c) for c in b["calls"]]


def show(action):
    calls = "; ".join(f"{c['name']}({json.dumps(c['input'])[:600]})" for c in action["calls"])
    return (calls or "(no tool call)") + (f"\nSaying: {action['text'][:400]}" if action["text"].strip() else "")


def judge(task, recent, original, other, model):
    """Whether `other` is as good a next step as `original` (JSON)."""
    prompt = (
        "An agent is doing a coding task. At one point in its log, it took the ORIGINAL next step. "
        "Replayed from the same point, it chose the OTHER step. Decide whether OTHER is as good a step toward the task: "
        "the same step, an equivalent one (e.g. reading the same thing another way), or a reasonable alternative. "
        "It is worse if it repeats finished work, loses track of the task, or re-does something the log shows was done.\n\n"
        f"Its instructions:\n{task[:2600]}\n\nThe last steps before this point:\n{recent}\n\n"
        f"ORIGINAL next step:\n{show(original)}\n\nOTHER next step:\n{show(other)}\n\n"
        'Answer with JSON only: {"verdict": "same" | "equivalent" | "worse", "why": one line}.'
    )
    out = subprocess.run(
        ["claude", "-p", prompt, "--model", model, "--tools", "", "--setting-sources", "", "--strict-mcp-config"],
        capture_output=True, text=True, timeout=600, cwd=tempfile.gettempdir(),
    ).stdout
    m = re.search(r"\{.*\}", out, re.S)
    try:
        return json.loads(m.group(0)) if m else {"verdict": "unknown", "why": out[:200]}
    except ValueError:
        return {"verdict": "unknown", "why": out[:200]}


def judge_pair(task, recent, reference, other, model):
    """Pairwise: which of two next steps from the same point is better, shown
    in random order. Returns the verdict for `other` against `reference`:
    same, equivalent, better or worse."""
    import random

    first_is_ref = random.random() < 0.5
    one, two = (reference, other) if first_is_ref else (other, reference)
    prompt = (
        "An agent is doing a coding task. From the same point in its log, two runs chose these next steps. "
        "Which is the better step toward the task? Equal if they are the same step, equivalent ones, or both reasonable. "
        "A step is worse if it repeats finished work, loses track of the task, or re-reads what the log shows it already has.\n\n"
        f"Its instructions:\n{task[:2600]}\n\nThe last steps before this point:\n{recent}\n\n"
        f"STEP 1:\n{show(one)}\n\nSTEP 2:\n{show(two)}\n\n"
        'Answer with JSON only: {"better": "1" | "2" | "equal", "why": one line}.'
    )
    out = subprocess.run(
        ["claude", "-p", prompt, "--model", model, "--tools", "", "--setting-sources", "", "--strict-mcp-config"],
        capture_output=True, text=True, timeout=600, cwd=tempfile.gettempdir(),
    ).stdout
    m = re.search(r"\{.*\}", out, re.S)
    try:
        v = json.loads(m.group(0)) if m else {}
    except ValueError:
        v = {}
    b = str(v.get("better", "")).strip()
    if b == "equal":
        verdict = "equivalent"
    elif b in ("1", "2"):
        other_won = (b == "2") == first_is_ref
        verdict = "better" if other_won else "worse"
    else:
        verdict = "unknown"
    return {"verdict": verdict, "why": v.get("why", out[:200])}


def recent_steps(prefix, n=6):
    out = []
    for line in prefix[-40:]:
        for b in blocks(line):
            if b.get("type") == "tool_use":
                out.append(f"- called {b['name']}({json.dumps(b.get('input'))[:200]})")
            elif b.get("type") == "tool_result":
                out.append(f"  -> {result_text(b)[:200]!r}")
            elif b.get("type") == "text" and line.get("type") == "assistant":
                out.append(f"- said: {b['text'][:200]!r}")
    return "\n".join(out[-n * 2 :])


def after_compaction(prefix):
    """What the model sees of a prefix: from its last compaction on."""
    last = max((i for i, l in enumerate(prefix) if l.get("subtype") == "compact_boundary"), default=0)
    return prefix[last:]


def instructions_of(prefix):
    """The task as the judge should know it at a point: the first message to
    the agent and, if one came later, the latest."""
    texts = []
    for line in prefix:
        if is_instruction(line):
            c = line["message"]["content"]
            texts.append(c if isinstance(c, str) else "".join(b.get("text", "") for b in c if b.get("type") == "text"))
    if not texts:
        return ""
    if len(texts) == 1:
        return texts[0][:1500]
    return f"{texts[0][:1000]}\n\n[Later, the latest instruction it was given:]\n{texts[-1][:1500]}"


def task_of(lines):
    for line in lines:
        if line.get("type") == "user":
            c = (line.get("message") or {}).get("content")
            if isinstance(c, str):
                return c
    return ""


def prior_arms(prior, prefix, a):
    """The earlier run's policy arms at this point, with the history each sent."""
    return [(name, POLICIES[name](prefix, a.keep, a.min_chars)[0]) for name in POLICIES if prior.get(name)]


def compact_fork(sid, cwd, model, effort, timeout):
    """Claude Code's own compaction of a fork (`/compact`), in place: the
    fork's transcript then ends with its compact boundary and summary.
    Returns the compaction call's usage, if it reported one."""
    cmd = ["claude", "-p", "/compact", "--resume", sid, "--model", model, *(["--effort", effort] if effort else []),
           "--output-format", "stream-json", "--verbose", "--setting-sources", "", "--strict-mcp-config"]
    out = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout).stdout
    usage = {}
    for raw in out.splitlines():
        try:
            ev = json.loads(raw)
        except ValueError:
            continue
        if ev.get("type") == "result":
            usage = ev.get("usage") or {}
    return usage


# Arms that are not a transform of the history but something Claude Code
# does to a fork before the replay.
FORK_ARMS = {"claude-compact"}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--transcript", required=True)
    ap.add_argument("--points", type=int, default=10)
    ap.add_argument("--min-context", type=int, default=200_000, help="only points whose next call read this many tokens")
    ap.add_argument("--policy", action="append", choices=sorted(POLICIES) + sorted(FORK_ARMS),
                    help="one or more (default: stub-old-tool-output); claude-compact: Claude Code's own /compact of the history")
    ap.add_argument("--keep", type=int, default=8, help="tool outputs kept in full (the latest)")
    ap.add_argument("--min-chars", type=int, default=400, help="shorter tool outputs are kept")
    ap.add_argument("--model", help="default: the session's own")
    ap.add_argument("--judge-model", default="sonnet")
    ap.add_argument("--effort", help="default: the session's own (as logged)")
    ap.add_argument("--judge-against", choices=["original", "baseline"], default="original",
                    help="compare each policy's step with the logged one, or with the baseline's from the same point")
    ap.add_argument("--cwd", help="default: the session's own folder")
    ap.add_argument("--timeout", type=int, default=900)
    ap.add_argument("--out", default="replay-results.jsonl")
    ap.add_argument("--dry-run", action="store_true", help="choose points and apply the policy; call no model")
    ap.add_argument("--noise-control", action="store_true",
                    help="also replay the unchanged history again (arm baseline-again) and judge it like a policy: the noise floor")
    ap.add_argument("--judge-twice", action="store_true", help="judge every pair a second time (the judge's own consistency)")
    ap.add_argument("--reuse-baselines", help="an earlier --out: its baseline steps for the same points are used, not replayed again")
    a = ap.parse_args()

    lines = load(a.transcript)
    project_dir = Path(a.transcript).parent
    model = a.model or next((l["message"].get("model") for l in lines if l.get("type") == "assistant"), "opus")
    cwd = a.cwd or next((l["cwd"] for l in lines if l.get("cwd")), os.getcwd())
    effort = a.effort or next((l.get("effort") for l in lines if l.get("type") == "assistant" and l.get("effort")), None)
    task = task_of(lines)
    chosen = points(lines, a.points, a.min_context)
    print(f"{len(lines)} lines; model {model}; {len(chosen)} points with context >= {a.min_context}", file=sys.stderr)
    work = tempfile.mkdtemp(prefix="c6-replay-")
    earlier = {}
    if a.reuse_baselines:
        for l in open(a.reuse_baselines):
            r = json.loads(l)
            if r.get("baseline"):
                earlier[(os.path.basename(r["transcript"]), r["line"])] = r
    policies = a.policy or ["stub-old-tool-output"]
    with open(a.out, "a") as out:
        for n, (i, original, ctx) in enumerate(chosen, 1):
            prefix = lines[: i + 1]
            est = lambda p: sum(len(json.dumps(b)) for l in after_compaction(p) for b in blocks(l)) // 4  # noqa: E731
            row = {"transcript": a.transcript, "line": i, "logged_context": ctx, "original": original, "est_tokens_baseline": est(prefix)}
            arms = [("baseline", prefix)]
            if a.noise_control:
                arms.append(("baseline-again", prefix))
            for name in policies:
                if name in FORK_ARMS:
                    arms.append((name, prefix))
                    continue
                shrunk, stubbed = POLICIES[name](prefix, a.keep, a.min_chars)
                row[f"{name}_stubbed"], row[f"{name}_est_tokens"] = stubbed, est(shrunk)
                arms.append((name, shrunk))
            prior = earlier.get((os.path.basename(a.transcript), i))
            if prior and not a.dry_run:
                row["baseline"], row["baseline_usage"] = prior["baseline"], prior.get("baseline_usage", {})
                row["baseline_reused"] = True
                arms = [x for x in arms if x[0] != "baseline"]
            if not a.dry_run:
                replayed = {}
                for arm, p in arms:
                    digest = json.dumps(p, sort_keys=True)
                    if arm == "baseline-again" and prior and prior.get(arm):
                        # The earlier run's second replay: judged again.
                        action, usage = prior[arm], prior.get(f"{arm}_usage", {})
                        row[f"{arm}_reused"] = True
                    elif arm == "baseline-again":
                        # A fresh replay of the unchanged history.
                        sid = fork(p, project_dir)
                        try:
                            action, usage = replay(sid, cwd, model, effort, work, a.timeout)
                        finally:
                            drop_fork(project_dir, sid)
                    elif arm in FORK_ARMS and not (prior and prior.get(arm)):
                        # Compacted by Claude Code itself, then replayed.
                        sid = fork(p, project_dir)
                        try:
                            row[f"{arm}_compact_usage"] = compact_fork(sid, cwd, model, effort, a.timeout)
                            action, usage = replay(sid, cwd, model, effort, work, a.timeout)
                        finally:
                            drop_fork(project_dir, sid)
                    elif prior and prior.get(arm):
                        # The same policy's step from the earlier run: judged again, not replayed.
                        action, usage = prior[arm], prior.get(f"{arm}_usage", {})
                        row[f"{arm}_reused"] = True
                    elif digest in replayed:
                        # Another arm sent exactly this history: the same step.
                        action, usage = replayed[digest]
                        row[f"{arm}_reused"] = True
                    elif prior and any(prior.get(x) and json.dumps(q, sort_keys=True) == digest for x, q in prior_arms(prior, prefix, a)):
                        x = next(x for x, q in prior_arms(prior, prefix, a) if prior.get(x) and json.dumps(q, sort_keys=True) == digest)
                        action, usage = prior[x], prior.get(f"{x}_usage", {})
                        row[f"{arm}_reused"] = x
                    else:
                        sid = fork(p, project_dir)
                        try:
                            action, usage = replay(sid, cwd, model, effort, work, a.timeout)
                        finally:
                            drop_fork(project_dir, sid)
                    if arm != "baseline-again":
                        replayed[digest] = (action, usage)
                    row[arm] = action
                    row[f"{arm}_usage"] = usage
                    # Against the logged step; or, for a policy, against the
                    # baseline's from the same point (its drift cancels out).
                    if a.judge_against == "baseline" and arm != "baseline":
                        reference = row["baseline"]
                        row[f"{arm}_same"] = same(action, reference)
                        verdicts = []
                        for _ in range(2 if a.judge_twice else 1):
                            verdicts.append({"verdict": "same", "why": "identical call"} if row[f"{arm}_same"] else judge_pair(
                                instructions_of(prefix), recent_steps(prefix), reference, action, a.judge_model))
                        row[f"{arm}_judge"] = verdicts[0]
                        if len(verdicts) > 1:
                            row[f"{arm}_judge2"] = verdicts[1]
                        continue
                    row[f"{arm}_same"] = same(action, original)
                    if a.judge_against == "baseline":
                        continue
                    row[f"{arm}_judge"] = {"verdict": "same", "why": "identical call"} if row[f"{arm}_same"] else judge(
                        instructions_of(prefix), recent_steps(prefix), original, action, a.judge_model)
            out.write(json.dumps(row) + "\n")
            out.flush()
            v = lambda arm: row.get(f"{arm}_judge", {}).get("verdict", "-")  # noqa: E731
            arms_said = ", ".join(f"{arm} {v(arm)}" for arm, _ in arms)
            print(f"point {n}/{len(chosen)} line {i}: context {ctx}; {arms_said}", file=sys.stderr)
    shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
