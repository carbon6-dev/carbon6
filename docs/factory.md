# The software factory — Design

Status: proposed (2026-11-28).

## 1. Motivation

Parallel coding needs explicit orchestration policy. Without it, independent
sessions can duplicate validation, contend for shared caches, miss established
procedures, or wait without observing progress. A project should define its own
build roles, integration order, authorization boundaries and resource limits.

The factory is a **control plane**: project policy that carbon6 holds and enforces, explicit stage state, resource-aware admission, and agents that improve the process from measurements — not a bigger swarm or another chatty watcher. Motivation and examples here are generic; private session evidence stays outside this repository.

## 2. The pieces

### 2.1 The project contract (specialized per project)
A versioned, editable document attached to a project (docs/projects.md), structured, with provenance (who decided, when) and inheritance into every session, subagent, watcher and task card of the project. carbon6 defines only the **schema**; every value is the project's data:
- **Build & test policy:** the project's validation commands and when each is allowed, which roles may build, target/cache namespaces, environment variables.
- **Capabilities:** named procedures the project defines (e.g. one it calls "check CI"), each with where it runs (a template or worker), its inputs and a result schema. When an agent's task matches a capability, it is used — never an agent's guess.
- **Integration:** branch, rebase and ordering rules, what needs approval (pushes, external runs).
- **Architecture rules** the integrator enforces.
- **Decisions ledger:** product decisions carried to all descendants.

A project's contract is the user's data, kept in carbon6's store, never in this repository.

A policy change is **propagated and acknowledged**: running sessions get it at their next turn (docs/session-specs.md refresh), and stale instructions are flagged. An agent's advice ("don't build in subagents") becomes policy only when the user accepts it — one click on a card, not copy-paste into five sessions.

**First slice implemented:** a project has accepted contract revisions in its head-owned project record. The project page includes a structured JSON editor and revision history; `projects.contract.get`, `set`, `history` and `propose` expose the same workflow. Direct writes and contract-card acceptance require human-origin requests. Revisions compare `expected_version`, stamp every new or changed item with user/time provenance, and preserve unchanged items' provenance. Suggestions never become policy until the user accepts their card; stale proposals must be proposed again against the current version.

The schema (`c6-api::project_contract`) contains validation commands with allowed conditions and roles, environment and namespace hints, capabilities, integration rules, architecture rules and decisions. It ships no project policy. A capability names a saved template or an exact worker/folder; the latter uses the caller's connection/model with the same placement and delegation limits as a template subagent. Template invocation keeps existing template authorization checks. Input and result-detail schemas use the head's supported object-schema subset, with unsupported keywords rejected. Starting `capability` returns `queued` with a run ID; `capability {run_id}` reads a structured outcome. Completion without valid terminal JSON is `unknown`, not success. Terminal results and the accepted schema snapshot are retained with the invocation. This is procedure invocation, not the future evidence-based stage gate.

Every ordinary project session, its descendants and per-session watchers resolve the latest same-realm contract. Editable prompts (`project.contract`, `project.capability`, `project.contract_reminder`, `project.contract_failure`) render the guidance. Session-spec refresh carries both the data and rendered text, including to named delegates. A contract-only edit shows `project contract updated (vN)` without starting a new turn. Sessions without a contract receive neither the tool nor a policy section.

The built-in agent's guardian deterministically matches author-declared, case-insensitive literal trigger phrases and command fragments. A matching proposed tool call is deferred once per capability per turn with an explicit reminder that it did not run; subsequent retries still use normal tool policy. Actual covered tool failures can propose a clarification card, never an automatic policy edit. Native external-agent sessions receive the same contract and capability bridge, plus separate documented command hooks: `PreToolUse` denies the first matching call per capability per native turn and returns the editable reminder as feedback; `UserPromptSubmit` adds context when the message matches a declared trigger phrase. Retried or unrelated tools return no permission verdict, leaving the normal permission and lease hooks intact. The capability tool itself is exempt. Hook deduplication uses locked state in the process's private lifetime-managed folder, separately for native session/agent IDs; it never inspects native transcripts or CLI internals. Matching arbitrary memory notes remains follow-up work; no semantic classifier or provider-spending evaluation is used.

### 2.2 Lines: the stage graph and its ledger
A **line** is the project's pipeline: stages with roles (templates: model, tools, place), rules and exit gates — all the project's data. carbon6 offers a **starter line** the user can copy and edit (stages named only generically: design → implement → integrate → review → validate → release), never a fixed sequence in code. Work items flow through it; the **ledger** records per item: stage, owner, branch, exact tree/commit snapshot, evidence (commands run, results, logs), and failures by class (source, flake, environment, cache, capacity, unknown). Gates are checked by carbon6 from evidence (a command's exit, CI's API), not from an agent's claim.

### 2.3 The scheduler and build slots (deterministic, no model calls)
- **Build slots** per worker (from Machines: CPU, memory, disk) and per target/cache namespace. Expensive commands run under a **lease** (command, inputs, owner, cancel rules). Two sessions never compile into the same target at once; a third build that would OOM the machine waits, visibly ("queued: worker build slot busy, ETA 40 min").
- **Queue and dependencies:** which item integrates next, rebase order ("onto what the previous one lands as"), batching of ready items into one validation run when the contract allows.
- **Pipelining:** while checkpoint N validates (immutable snapshot), N+1 and N+2 are implemented in their own worktrees; when N finishes, its failures and review findings are folded into the ready batches before they build.

### 2.4 Subscriptions instead of polling
Long commands and CI runs report **state changes** (compile phase, lock wait, test progress, CI stage) as notices; agents subscribe and stop polling. Keep-warm covers the waits. "Observer unavailable" is a state, not a reason to give up.

### 2.5 The management agents (watchers with a job, not generic retrospectives)
Each reads the **ledger and measurements**, not raw transcripts, and acts through cards and line rules:
- **Throughput agent.** Watches stage durations, queue lengths, slot contention, idle stages, rework loops, and validation invocations per change. Proposes process changes with an expected saving, from a generic catalogue of levers (batching, pipelining, sharing identical builds, consolidating fragmented validation runs, scoping diagnostics, slot limits): e.g. batch these five items; start N+1 while N validates; two sessions build the same commit — share; these three narrow runs each rebuilt the same dependencies — use the contract's consolidated run; this stage is 70% of wall clock. Each proposal is a card; accepted ones become line rules; it then **measures** the result against the line's own history and reports whether the saving happened (and reverts the rule if it didn't).
- **Resource guardian (one per worker).** Forecasts disk and memory from the sampler and build-folder growth; before limits, asks each session on the worker to reclaim what it owns (its targets' incremental caches, stale worktrees, finished build folders), ranked by priority and last use — sessions know what they still need; carbon6-owned leftovers are cleaned per policy; anything else becomes a cleanup card. Admission uses it: no new build slot when the disk can't hold it. Capacity changes (more cores, more disk) update the slots and are announced.
- **Knowledge guardian.** Notices when an agent misses something the contract, memory or memex holds (e.g. it looked for CI in the wrong place while a capability or note said where) and corrects the session at once; if it recurs, it proposes moving the fact into the contract so it is injected, not searched for.
- **Integrator.** The serial validation role: rebase, build, test, fix by root cause, push, confirm CI and release.
- **Architect.** At admission: overlap map and shared invariants across the batch; flags duplicate implementations before expensive verification.

### 2.6 How optimizations get discovered automatically
1. **Observe:** every stage run becomes a ledger row with timings split into queue / lock / compile / test / review / CI / interaction (unknown stays unknown).
2. **Model:** the throughput agent keeps a small model of the line — stage durations, failure rates by class, contention, cache hits.
3. **Hypothesize:** known levers (batching, pipelining, consolidated validation, shared caches, slot limits, scoped diagnostics) are evaluated against the model; new levers can be proposed in plain language.
4. **Experiment:** an accepted proposal runs on the next few items; carbon6 compares them with the line's history, not with a guess. No provider-spending evals — this is measurement of real work.
5. **Adopt or revert:** measured wins become rules of the line, with their evidence; losses are reverted and recorded.

For example, the line could propose pipelining independent edits during validation or consolidating builds under one integration role.

### 2.7 The project view
A project page, live:
- **The line board:** items as cards moving through stages; each stage shows its queue, what runs where, and its wait.
- **The machine strip:** each worker's CPU, memory and disk, with the builds (leases) on it and its slots.
- **The timeline:** the last day or week as lanes per item — builds as bars, waits as gaps, gate failures marked — so you see where the wall clock went.
- **Metrics:** lead time per item (request to release), first-pass yield at integration, validation runs per change, rework count, cost per item (Usage), resource headroom.
- **The management agents' open proposals** with expected and measured savings; the contract's recent changes.

## 3. What exists already
Projects, work items, task cards, decisions with Discuss, templates, subagents with groups and liveness, watchers, Machines and its sampler, disk warnings and build-folder cleanup, the compile cache, keep-warm and trimming, the session spec refresh, edge releases. The factory composes them: the contract and the ledger are new; the rest are roles and rules over existing parts.

## 4. Nothing project-specific in carbon6

- carbon6's code, defaults, built-in prompts and starter line name **no project, repository, tool, command, CI system or host**. Everything specific is in a project's contract (data the user owns and edits, versioned, exportable), in templates, or in memory.
- The management agents' instructions are editable prompts written in generic terms; their levers are a generic catalogue; what they know about a project comes from its contract and ledger.
- Tests use synthetic projects (fake commands, mock CI capabilities), never a real project's names.
- A project without a contract behaves exactly as today.

## 5. Build order
1. **The project contract** with capabilities (CI first) and propagation to sessions; the knowledge guardian's "inject, don't search" path.
2. **The resource guardian** per worker (asks sessions to reclaim; admission by disk and memory).
3. **The ledger and build slots** with leases; subscriptions for long commands and CI.
4. **Lines** with the default stages and enforced gates; the integrator role as a template.
5. **The project view** (board, machine strip, timeline, metrics).
6. **The throughput agent** with proposals, experiments and adopt-or-revert; the architect at admission.

Tests (docs/testing.md): deterministic, with mock models and injected clocks; never provider-spending evals.
