# Dynamic Workflows for OpenAI Codex — Engineering Spec

## 1. Summary

We are adding **Dynamic Workflows** to Codex at feature parity with Claude Code's Workflow runtime: a host-authored JavaScript program that begins with a pure `export const meta = {name, description, phases}` literal, is executed **once** to completion by a host JS engine (not as a chat turn), and orchestrates a fleet of subagents through a small deterministic hook surface — `agent()`, `parallel()`, `pipeline()`, `phase()`, `log()`, `args`, `budget`, `workflow()`. Runs are backgrounded, journaled per `runId`, and resumable by longest-unchanged-prefix replay.

**The chosen approach is to bridge two subsystems Codex already ships:** the `codex-rs/code-mode` V8 isolate becomes the deterministic workflow engine, and the native multi-agent runtime (`codex-rs/core/src/codex_delegate.rs` + `agent/control` + `agent/registry`) becomes the `agent()` fan-out backend. The workflow host is thin: it registers new isolate globals that dispatch through the *existing* promise/resolver async bridge, drives subagents through the *existing* one-shot delegate, meters tokens through the *existing* `RolloutBudget`, and adds two genuinely new layers — a determinism harden of the isolate and a `(prompt,opts)`-keyed journal + prefix-replay loop. We explicitly reject the out-of-core Node/Python SDK harness (`sdk/typescript/src/thread.ts`) because it cannot deliver single-isolate deterministic replay, an in-process hard token ceiling, or the "one program the host runs" contract that *defines* parity.

---

## 2. Goals / Non-goals

### Goals (v1 parity scope)

- **Authoring model**: parse `export const meta = {name, description, phases}` statically; run the body once as an ES module in a fresh V8 isolate.
- **Hooks**: `agent(prompt, opts?)`, `parallel(thunks[])`, `pipeline(items, ...stages)`, `phase(title)`, `log(msg)`, `args`, `budget{total, spent(), remaining()}`, `workflow(nameOrRef, args)`.
- **agent() opts**: `label`, `phase`, `schema` (forced StructuredOutput + validated object), `model`, `effort('low'..'max')`, `isolation:'worktree'`, `agentType`. `null` on death/skip.
- **Execution semantics**: concurrency cap `min(16, cores-2)` with excess queued; lifetime cap ~1000 agents; max 4096 items per `parallel`/`pipeline`; `pipeline` is no-barrier (item A in stage 3 while B in stage 1); `parallel` is a barrier.
- **Determinism**: `Date.now()`, argless `new Date()`, `Math.random()` disabled inside the isolate.
- **Resume**: `resumeFromRunId` replays the longest unchanged prefix of `agent()` calls from a `journal.jsonl` keyed by `(prompt, opts)`; first divergence onward runs live.
- **Budget**: hard token ceiling; `agent()` throws once `spent >= total`.
- **Surfacing**: background execution, completion notification, live phase/agent progress tree, script persisted and re-invocable by `scriptPath` or saved name.
- **Feature-gated** behind a new `Feature::Workflow`.

### Non-goals (out of scope for v1)

- Multi-level workflow nesting. `workflow()` is **one level deep** only (depth guard rejects deeper).
- `Promise.race`/first-wins branching on `parallel()` results across resume — v1 authoring model **forbids racing** on fan-out results (see §7). Barrier/no-barrier only.
- Distributed/multi-host execution. A run lives on one host.
- Journal compaction/retention tuning — uncompressed JSONL in v1.
- A `/workflow ps` browsing/cancel UI (completion-notify only in v1).
- Non-git isolation backends (only `isolation:'worktree'`).
- Byte-reproducible V8 (JIT-disabled) builds — determinism is at the `agent()`-result level, not machine-code level.

---

## 3. Chosen architecture

**Decision: reuse the `codex-rs/code-mode` V8 host as the deterministic workflow engine, driving the native multi-agent runtime via `codex_delegate::run_codex_thread_one_shot` for `agent()` fan-out.**

### Why the V8 host already fits

`codex-rs/code-mode/src/runtime/mod.rs::run_runtime` compiles the `source` string as an ES module (`module_loader.rs::evaluate_main_module` → compile → instantiate → evaluate), runs a microtask checkpoint, and drives the top-level-await promise to `Fulfilled`/`Rejected` via a `RuntimeCommand`/`RuntimeEvent` loop. Nothing in that path assumes the source was model-authored — `ExecuteRequest.source` is just a `String` (`code-mode-protocol`), so a host-authored workflow script is admitted identically. Tool calls are already deterministically ordered by a monotonic `RuntimeState.next_tool_call_id`, and the isolate runs on one dedicated OS thread. That is precisely the deterministic skeleton the parity spec demands.

### Why `agent()` is just another async bridge op

`callbacks.rs::tool_callback` is the template for every host-await hook: it mints a `v8::PromiseResolver`, stores the `Global` resolver in `RuntimeState.pending_tool_calls` under an id, emits `RuntimeEvent::ToolCall{id,...}`, and returns the promise to JS. The cell actor (`cell_actor/mod.rs::run_cell`) receives the event, spawns a tokio task that calls the host delegate, and feeds the answer back as `RuntimeCommand::ToolResponse{id,result}`, which `module_loader.rs::resolve_tool_response` resolves into the stored promise. **`agent()` is structurally identical**: a new `agent_callback` mints a resolver, stores it, and emits a new `RuntimeEvent::AgentCall{id, prompt, opts}`. The bridge is capability-agnostic; a "spawn subagent" op is just a different event variant flowing through the same resolver/response machinery. This is the single strongest reusable asset in the codebase.

### Why this beats the out-of-core SDK harness

The SDK path (`sdk/typescript/src/thread.ts` + `outputSchemaFile.ts`) can spawn schema-constrained threads over JSON-RPC, but it runs in Node/Python with **live `Date`/`Math`/IO**, no single-isolate replay, and no in-process budget governor. It structurally cannot deliver deterministic-resume, a hard token ceiling, or "one program the host runs." Adopting it partially would silently diverge from parity. We use the SDK/CLI only as a thin front door that forwards to the core tool.

### Component diagram

```mermaid
flowchart TD
    SCRIPT["Workflow script<br/>export const meta = {...}<br/>+ body"] --> META["Manifest parser<br/>(parse_exec_source style)"]
    META --> HOST["Workflow host tool<br/>(core/src/tools/code_mode/execute_handler.rs clone)"]
    HOST --> V8["code-mode V8 isolate<br/>runtime/mod.rs::run_runtime"]
    V8 --> PRELUDE["Injected JS prelude<br/>parallel() / pipeline()<br/>+ determinism shims"]
    V8 --> GLOBALS["Native globals<br/>agent, workflow, phase,<br/>log, args, budget<br/>(runtime/globals.rs)"]
    GLOBALS -->|RuntimeEvent::AgentCall| BRIDGE["Async bridge<br/>callbacks.rs + cell_actor"]
    BRIDGE --> DELEGATE["SpawnAgent host handler<br/>codex_delegate::run_codex_thread_one_shot"]
    DELEGATE --> MULTI["Native multi-agent runtime<br/>agent/control + agent/registry<br/>ThreadManager::spawn_subagent"]
    MULTI -->|last_agent_message / structured JSON| BRIDGE
    MULTI --> BUDGET["RolloutBudget<br/>Arc shared across subagent tree<br/>rollout_budget.rs"]
    BRIDGE --> JOURNAL["Workflow journal<br/>journal.jsonl (codex-workflow-journal)"]
    BUDGET -.spent()/remaining().-> GLOBALS
    JOURNAL -.prefix replay.-> GLOBALS
    MULTI --> WORKTREE["git worktree isolation<br/>git-utils (isolation:'worktree')"]
    HOST --> EVENTS["workflow/* protocol events<br/>-> app-server -> TUI progress tree"]
```

---

## 4. Authoring API

The prelude + native globals expose exactly this surface. Signatures and semantics mirror Claude's Workflow runtime.

### `agent(prompt, opts?) -> Promise<string | object | null>`

Spawns a subagent, runs it to completion, returns its final assistant text. With `opts.schema` (a JSON Schema), forces a StructuredOutput final answer and returns the **validated parsed object**. Returns `null` if the agent dies or skips (turn aborted, spawn error, budget-abort). Never throws for agent failure — only throws when the budget ceiling is hit at admission (§8).

```ts
opts = {
  label?: string,        // progress-tree leaf label (cosmetic; excluded from cache key)
  phase?: string,        // progress grouping tag (cosmetic; excluded from cache key)
  schema?: JSONSchema,   // forces StructuredOutput; return is validated object
  model?: string,        // resolved against ModelsManager.list_models
  effort?: 'low'|'medium'|'high'|'xhigh'|'max',  // -> ReasoningEffort
  isolation?: 'worktree',// run in a fresh git worktree with its own cwd
  agentType?: string,    // -> role_name via apply_role_to_config
}
```

### `parallel(thunks: (() => Promise<T>)[]) -> Promise<(T|null)[]>`

Runs all thunks concurrently; **barrier** — awaits all. A thunk that throws resolves to `null` in the results array (position-preserving). Implemented purely in the JS prelude: `Promise.all(thunks.map(t => t().catch(() => null)))`. No new host op — it composes `agent()` promises. Concurrency is bounded host-side by the scheduler semaphore (§5); "excess queued" is automatic. Validates `thunks.length <= 4096`.

### `pipeline(items: T[], ...stages: ((x) => Promise<any>)[]) -> Promise<(any|null)[]>`

Runs each item through **all stages independently, with NO barrier between stages** — item A can be in stage 3 while item B is still in stage 1. Each item is its own promise chain: `items.map(i => stages.reduce((p, s) => p.then(s), Promise.resolve(i)).catch(() => null))`. A stage throw drops **that** item to `null` without blocking siblings. Global concurrency still bounded by the scheduler semaphore. Validates `items.length <= 4096`.

### `phase(title: string) -> void`

Progress grouping. Emits `RuntimeEvent::Phase` → `WorkflowPhaseBegin/End` protocol events for the live tree. Also journaled as a `phase` line so the tree reconstructs on resume.

### `log(msg: string) -> void`

Narrator line to the user. Thin alias over the existing `notify_callback` path (`globals.rs`), routed to a `WorkflowLog` event. Journaled.

### `args: JsonValue`

The JSON value passed into the workflow, injected read-only as a global via `value.rs::json_to_v8` in `install_globals`, exactly like `build_tools_object` injects tool metadata. `args` is also where any script-visible timestamps/seeds must come from (since time/random are disabled — §7). `workflow.runId` is exposed read-only here too.

### `budget: { total: number, spent(): number, remaining(): number }`

Native-backed object over the shared `RolloutBudget`. `total` from args. `spent()`/`remaining()` read the live weighted counter under the existing lock. `agent()` throws synchronously when `remaining() <= 0` **before** spawning (§8).

### `workflow(nameOrRef: string, args: JsonValue) -> Promise<any>`

Runs another **saved** workflow inline, **one level deep**. Loads the named script from the workflows directory and re-enters the runtime as a nested cell/subagent. Depth enforced by `agent/registry.rs::exceeds_thread_spawn_depth_limit` / `next_thread_spawn_depth`. Nesting deeper than one level throws.

---

## 5. Execution semantics

A single host-side **`WorkflowScheduler`** owns three independent governors, all backed by existing Codex primitives. Do not invent new token accounting.

### Concurrency cap — `min(16, cores-2)`, excess queued

A `tokio::sync::Semaphore(cap)` where `cap = min(16, std::thread::available_parallelism().saturating_sub(2))`, further clamped to `effective_agent_max_threads` (`config/mod.rs:1428`) via the existing `normalize_concurrency` clamp (`agent_jobs.rs:130`). Each admitted `agent()` acquires a permit before `run_codex_thread_one_shot` and drops it on finalize. Excess `agent()` calls await a permit — that *is* "excess queued." This mirrors the working admit/reap loop in `agent_jobs.rs::run_agent_job_loop` (`:160-315`), but uses a semaphore instead of manual `HashMap` slot arithmetic because the workflow host is in-process and structured (not DB-persisted and crash-recoverable). `AgentRegistry::reserve_spawn_slot` (`registry.rs:82`) remains the hard backstop; on `CodexErr::AgentLimitReached` the scheduler requeues.

> **Cap override note:** `effective_agent_max_threads` defaults to 6 (V1) / `max_concurrent_threads_per_session - 1` (V2). To actually reach `min(16, cores-2)`, the workflow-owned subagent tree raises the clamp — the `min(16,cores-2)` policy is the intended ceiling for workflow runs, not the session default.

### Lifetime cap — ~1000 agents/run

A workflow-scoped `AtomicUsize lifetime_spawned` on `WorkflowScheduler`, ceiling 1000, CAS-incremented (copying `registry.rs:289 try_increment_spawned`) at `agent()` admission **before** the concurrency permit. It **never decrements** (lifetime, not active — distinct from `AgentRegistry.total_count` which decrements on release, `registry.rs:99`). Exceeding it throws `AgentCapReached`. Kept separate from the session registry so the cap is per-workflow-run.

### Item cap — 4096 per `parallel`/`pipeline`

Validated in the JS prelude before dispatch.

### `pipeline` no-barrier pipelining

Each item is an independent promise chain (§4). Because the tasks are independent and the single global semaphore bounds *total* concurrent agents (not per-stage), items advance at their own pace — the staggered-progress semantic falls out for free. No cross-stage barrier exists.

### Admission order inside host `agent()`

1. `budget.remaining() <= 0` → throw `BudgetExceeded`.
2. lifetime CAS to 1000 → throw `AgentCapReached` if over.
3. journal check (resume): if replaying and this ordinal's `(prompt,opts)` key matches → resolve from cache, skip spawn.
4. await concurrency permit.
5. spawn subagent via shared `AgentControl` (auto-metered into `RolloutBudget`).
6. await final status, reap, drop permit.
7. journal return value + token cost.

---

## 6. `agent()` → subagent mapping

**Build `agent()` on `codex_delegate::run_codex_thread_one_shot` (`codex_delegate.rs:196`), NOT the V2 `spawn_agent`/`wait_agent` tool pair.** The V2 pair is fire-and-mailbox — `wait_agent` (`multi_agents_v2/wait.rs`) only signals mailbox activity, it never returns the child's final text. The one-shot delegate returns final text directly, auto-shuts-down at `TurnComplete`/`TurnAborted`, and cascades approvals to the parent — exactly `agent()`'s blocking "run to completion, return final text" contract. This is the proven pattern in `tasks/review.rs::process_review_events` (`:126`).

Per `agent()` call, the host handler:

1. **Build child config**: `build_agent_spawn_config(base_instructions, parent_turn)` (`multi_agents_common.rs:161`) so the child inherits provider/model/reasoning/developer-instructions and runtime state.
2. **Apply opts in `spawn_agent` order**:
   - `opts.model` + `opts.effort` → `apply_requested_spawn_agent_model_overrides` (`multi_agents_common.rs:234`); validates effort against the model's `supported_reasoning_levels` via `validate_spawn_agent_reasoning_effort`. Map `'low'..'max'` onto Codex's `ReasoningEffort` enum; reject unsupported.
   - `opts.agentType` → `apply_role_to_config` (`agent/role.rs`), trimmed to `role_name`, `DEFAULT_ROLE_NAME` fallback (`multi_agents_v2/spawn.rs:82`).
   - service tier / approval / cwd / permissions inherited via `apply_spawn_agent_service_tier` (`:285`) and `apply_spawn_agent_runtime_overrides` (`:210`).
3. **Spawn** with `SubAgentSource::ThreadSpawn` (so depth + registry accounting apply) and `final_output_json_schema = opts.schema`.
4. **Consume the bridged event stream**: on `EventMsg::TurnComplete` return `TaskCompleteEvent.last_agent_message`; on `EventMsg::TurnAborted` or any spawn error return `None` → JS `null`.

### Structured output (`opts.schema`)

`opts.schema` passes straight into `final_output_json_schema` → `TurnContext.final_output_json_schema` (`turn_context.rs:138`) → `build_prompt` sets `Prompt.output_schema` + `output_schema_strict = true` (`session/turn.rs:1096`). The model is forced to emit schema-conformant JSON; `last_agent_message` is that JSON string. `agent()` does `serde_json::from_str` and returns the object via `value.rs::json_to_v8`. **Defense-in-depth**: re-validate against the JSON Schema with the `jsonschema` crate before returning (strict mode is engine-enforced for OpenAI providers but not guaranteed for all providers). This is the same path `exec --output-schema` (`exec/src/cli.rs:53`, `lib.rs::load_output_schema`) and `guardian/review_session.rs:806` rely on.

### Depth (`workflow()` one level)

`next_thread_spawn_depth` / `exceeds_thread_spawn_depth_limit(child_depth, agent_max_depth)` (`registry.rs:71`, `multi_agents/spawn.rs:66`) gate nested spawns automatically because `agent()`/`workflow()` spawn with `SubAgentSource::ThreadSpawn`. One-level `workflow()` maps directly onto `agent_max_depth`.

### `isolation:'worktree'` (the one genuinely NEW piece — open issue #18969)

No spawn path sets a per-child cwd today: `apply_spawn_agent_runtime_overrides` hard-copies `config.cwd = turn.cwd` (`multi_agents_common.rs:224`), and `SpawnAgentOptions` has no cwd field (`agent/control.rs:66`). Build:

1. Add `cwd: Option<AbsolutePathBuf>` to `SpawnAgentOptions` and make `apply_spawn_agent_runtime_overrides` **respect an override** instead of unconditionally copying `turn.cwd`.
2. A `git-utils` helper `worktree_add(repo_root, dest) -> WorktreeGuard` using `git worktree add --detach`, plus a `Drop`/finalizer that runs `git worktree remove` **iff the worktree is unchanged** versus a baseline snapshot (reuse `git-utils/src/baseline.rs` / `info.rs`).
3. The scheduler, for `opts.isolation == 'worktree'`, allocates a **deterministic** worktree dir (index-derived name — no `Date.now`/`Math.random`), sets it as the child cwd, and adds it to `permissions.workspace_roots` so the sandbox permits writes.

This makes parallel file-mutating agents conflict-free. Cleanup-on-unchanged must be robust to child crashes.

### `label` / `phase` progress attribution

Carry `opts.label` / `opts.phase` as metadata on the spawn source (`task_name` already threads through, `multi_agents_v2/spawn.rs:142`); surface them in the progress tree the same way `AgentMetadata.last_task_message` is surfaced. **Excluded from the cache key** (§7) so re-labeling doesn't bust cache.

> **Determinism caveat:** `AgentRegistry` nickname selection uses `rand::rng()` (`registry.rs:232`). For deterministic replay, pass a preferred nickname derived from the agent index via `reserve_agent_nickname_with_preference` to bypass randomness.

---

## 7. Determinism, journal & resume

Build a dedicated **`codex-workflow-journal`** crate that clones the rollout recorder's proven append-only JSONL machinery. Do **not** extend `RolloutItem` (`protocol.rs:3141`) — it is conversation-shaped and every rollout consumer would have to handle new variants. Reuse the *infrastructure*: `RolloutRecorder`'s background mpsc JSONL writer with monotonic `ordinal_state` (`rollout/src/recorder.rs`), `ReverseJsonlScanner` for tail/prefix reads (`rollout/src/reverse_jsonl_scanner.rs`), and the newline-terminated append discipline (`recorder.rs:1792`). The in-isolate `store`/`load` KV (`callbacks.rs:184-242`) is an unordered in-memory map — unsuitable as the journal, but fine as user scratch state.

### Determinism hardening (currently ABSENT — prerequisite)

`globals.rs::install_globals` today deletes only `console`/`Atomics`/`SharedArrayBuffer`/`WebAssembly` (`globals.rs:16-19`); `Date` and `Math` are fully live. A frozen JS bootstrap prelude, compiled as a classic `v8::Script` and run **before** `evaluate_main_module` (`mod.rs:202`), must:

- Replace `Math.random` with a **throwing stub** (opt-in `args.seed`-derived splitmix64 PRNG only if explicitly requested).
- Replace `Date.now` with a throw.
- Wrap the `Date` constructor so **argless** `new Date()`/`Date()` throw, while explicit-arg `new Date(x)` and `Date.parse` survive (scripts still parse timestamps handed in via `args`). Doing the argless-vs-args distinction in JS is far cleaner than in native V8.
- Extend the delete list with `WeakRef` and `FinalizationRegistry` (default-present in bare V8, GC-order nondeterministic).

`runId` is minted host-side in Rust with `uuid::Uuid::now_v7()` (`items.rs:414` pattern — safe because it runs outside the isolate) and injected read-only via `workflow.runId`. The script must never derive ids/time/random itself.

### Invocation ordinal (the spine of prefix-replay)

Add `agent_callback` modeled on `tool_callback`. It stamps `ordinal = state.next_agent_ordinal++` **synchronously before returning the promise**, exactly as `tool_callback` bumps `next_tool_call_id` (`callbacks.rs:61-62`). Because the isolate is single-threaded and `parallel`/`pipeline` fire thunks in array order up to the first `await`, the ordinal sequence is fully deterministic across concurrency. **We key cache on invocation ordinal, never completion order.**

### `(prompt, opts)` cache key

```
key = blake3(canonical_json({ prompt, model, effort, agentType, isolation, schema }))
```

Canonicalized with **sorted object keys** and a **stable JSON-Schema serialization**. `label` and `phase` are deliberately **excluded** so cosmetic re-labeling does not bust cache. The key algorithm is versioned and stored in `run_meta` so hash changes across Codex versions are detectable.

### Journal format (`journal.jsonl`)

Line 0 = run meta; subsequent lines share an envelope `{timestamp, ordinal, type, ...}` (timestamp from the Rust host, never the isolate):

```json
{"type":"run_meta","run_id":"...","parent_run_id":null,"script_hash":"...","args_hash":"...","name":"triage","budget_total":500000,"key_algo_version":1,"created_at":"..."}
{"type":"agent_call","ordinal":0,"key":"blake3:...","prompt_hash":"...","opts":{"model":"...","effort":"high","agentType":"reviewer","isolation":null,"schema_hash":"..."},"phase":"analyze","label":"file-a","child_thread_id":"th_...","status":"completed","return":{"...":"validated object or string or null"},"tokens_spent":8123,"completion_seq":2}
{"type":"phase","ordinal":null,"title":"analyze"}
{"type":"log","ordinal":null,"message":"narrator line"}
```

`status` is `completed | null | error`. `return` round-trips string, validated object, or `null` identically. `completion_seq` records the order concurrent agents finished (needed only if racing is later allowed; recorded defensively).

### Storage layout

```
$CODEX_HOME/workflows/runs/<runId>/
  journal.jsonl   # source of truth for replay
  script.js       # the executed program (re-invoke by scriptPath)
  meta.json
```

Mirrors rollout's per-run file layout. A `workflow_runs` SQLite index (new `codex-state` migration following `state/src/model/agent_job.rs` and `state/src/lib.rs:99-103` per-DB conventions) stores `{runId, name, scriptHash, scriptPath, parentRunId, status, created_at}` **purely for discovery-by-name**. Replay never needs SQLite; JSONL is authoritative, SQLite is a rebuildable projection. Each subagent still gets its own full-transcript rollout file, linked to the workflow root via a `thread_spawn_edge` in `agent-graph-store/src/local.rs` — that gives the progress tree its parent/child topology and per-agent token/tool counts for free.

### Resume algorithm

1. On `resumeFromRunId`, load the prior journal tail-first via `ReverseJsonlScanner` into `entries[0..M]`. Validate `script_hash`/`args_hash`/`key_algo_version` (a structural change simply produces early divergence).
2. Mint a **fresh** `runId` for the resumed run (itself resumable). Seed `RuntimeState` with `replay_entries`, `replay_active = true`.
3. In `agent_callback` at ordinal `i` with computed key `k`:
   - If `replay_active && i < M && entries[i].key == k && entries[i].status == completed`: resolve the promise from `entries[i].return` (reusing `module_loader::resolve_tool_response`'s resolve path, `:66-101`) and append the entry to the new journal. **Also re-add `entries[i].tokens_spent` to the budget counter** (replay-only `add_spent` path) so `spent()`/`remaining()` and the ceiling throw land at the identical ordinal.
   - Else: set `replay_active = false` (never re-enables), dispatch live via `RuntimeEvent::AgentCall`, append a fresh entry.
4. Concurrency: a parallel batch issues several ordinals synchronously; each is served from cache the instant its ordinal is issued — reproducing barrier/no-barrier semantics without special-casing.

This is precisely "longest unchanged prefix; first changed/new call and everything after runs live."

---

## 8. Budget & governance

Reuse `codex-rs/core/src/rollout_budget.rs::RolloutBudget` as the token governor — do not build new accounting.

### Real-time aggregation across subagents (already free)

`AgentControl.rollout_budget` is an `Arc<RolloutBudget>` "shared by the root thread and every cloned sub-agent control handle" (`control.rs:106-107`). Every subagent spawned through the workflow root's `AgentControl` shares the **same** Arc; `Session::record_rollout_budget_usage` runs after every turn's `TokenUsage` (`session/mod.rs:3696`), so the shared `weighted_tokens_used` counter is a live, tree-wide sum. Configure:

```
RolloutBudgetConfig {
  limit_tokens: args.budget.total,
  sampling_token_weight: 1.0,     // count output tokens
  prefill_token_weight: 0.0,      // ignore input -> pure output-token spend
  reminder_at_remaining_tokens: [],
}
```

so `weighted_tokens_used == pure output-token spend`. Add two public getters: `pub fn spent(&self) -> i64` and `pub fn remaining(&self) -> i64` (both read `weighted_tokens_used` under the existing lock; `remaining = (limit - weighted).max(0)`). JS `budget.spent()`/`remaining()`/`total` forward to these.

### Two-layer hard-ceiling enforcement

- **Pre-admission (new)**: host `agent()` checks `if remaining() <= 0 { throw BudgetExceeded }` **before** reserving a slot or spawning. This makes `agent()` throw deterministically at the ceiling.
- **In-flight backstop (existing)**: a turn that pushes the shared counter past `limit_tokens` makes `record_usage` return `true` → `CodexErr::SessionBudgetExceeded` → `TurnAbortReason::BudgetLimited` (`protocol.rs:4139`); the offending subagent's `agent()` resolves to `null` per the death-is-null contract.

The ceiling can overshoot by at most **one in-flight turn**, then all subsequent `agent()` calls throw — a hard ceiling in practice.

### Resume determinism of budget

Journaled `tokens_spent` per call is re-added during prefix replay (§7) so `spent()`/`remaining()` and the throw boundary are byte-identical between original and resumed runs.

### `RolloutBudget::configure` OnceLock caveat

`configure` uses `OnceLock` (`control.rs:120`), so a reused `AgentControl` cannot re-set `limit_tokens`. For nested `workflow()` calls or reused sessions, introduce a **resettable budget cell** instead of `OnceLock`.

### Reporting

Expose progress via the existing `ThreadGoal` channel — emit `ThreadGoal{token_budget: total, tokens_used: spent(), status}` (`protocol.rs:4006`) and set `ThreadGoalStatus::BudgetLimited` (`protocol.rs:3988`; `v2/thread.rs:734`) at the ceiling, so clients render budget state with no new protocol types.

---

## 9. Progress, background, entrypoint & persistence

### New protocol events

Add a workflow event cluster to `EventMsg` (`protocol.rs`, next to the `CollabAgent*` family at `:1457-1476`, the exact structural precedent), reusing `ReasoningEffortConfig` and `TokenUsage`:

- `WorkflowRunBegin{run_id, name, phases, args_digest}` / `WorkflowRunEnd{run_id, status, spent, total}`
- `WorkflowPhaseBegin/End{run_id, phase_index, title}`
- `WorkflowGroupBegin/End{run_id, group_id, kind: parallel|pipeline, item_count}`
- `WorkflowAgentBegin{run_id, node_id, parent_node_id, label, phase, model, effort}`
- `WorkflowAgentUpdated{run_id, node_id, token_usage, tool_call_count}` (streams the two numbers the tree needs, rolled up from the subagent's own thread `TokenCount`/tool events)
- `WorkflowAgentEnd{run_id, node_id, status, token_usage, tool_call_count, returned_null}`
- `WorkflowLog{run_id, message}`

`node_id` is a **deterministic per-run counter** (never `Date.now`/random) so it survives resume replay.

### App-server notifications

Bridge to `ServerNotification` (macro at `app-server-protocol/src/protocol/common.rs:1613`) under a `workflow/*` wire namespace: `workflow/started`, `workflow/phase/changed`, `workflow/agent/started|updated|completed`, `workflow/log`, `workflow/completed`. Payload structs go in a new `app-server-protocol/src/protocol/v2/workflow.rs` (modeled on `v2/notification.rs`, `#[serde(rename_all="camelCase")]`, `JsonSchema`, TS export), reusing the generated `CollabAgentStatus` enum for node status. Map events in `app-server/src/bespoke_event_handling.rs`. **Batch all workflow/* variants into one PR** to avoid repeated JSON+TS schema churn. The SDK gets typed progress for free via ts-rs export.

### TUI live progress tree

Add a `WorkflowProgressCell` in `tui/src/app/agent_status_feed.rs`, built like the existing `AgentStatusHistoryCell` ("Sub-agents running") but maintaining a real tree keyed by `run_id`: workflow name → phases → (group nodes →) agent leaves. Reuse `multi_agents.rs` helpers (`agent_picker_status_dot_spans` for the status dot, `format_agent_picker_item_name` for the `[role]` label) and `render/line_utils::prefix_lines` for indentation. Each leaf: dot, label, live tokens, tool-call count. **Bound height** (constants like `AGENT_STATUS_PREVIEW_*`) by collapsing finished phases to one summary line. Re-render on each `workflow/*` notification via `request_redraw`. Any spinner/elapsed uses only event-supplied `started_at_ms` (Date.now is disabled).

### Background execution + completion notification

Codex turns already run in the app-server off the TUI thread; the TUI is a pure notification consumer (`tui/src/app.rs:253-290`). The workflow host runs as a long-lived task emitting `workflow/*`; the TUI stays interactive. On completion, add `Notification::WorkflowComplete{name, status, agents, spent}` (`tui/src/chatwidget/notifications.rs`), raised on `workflow/completed`, reusing the coalesced desktop-notification path (`tui.notify()`, `tui/src/tui.rs:690`) and the `tui_notifications` allowlist. Give it **higher priority** than `AgentTurnComplete(0)` since the user is typically away.

### Entrypoint decision — ship all three, clear division of labor

1. **Primary (load-bearing): a model-callable `workflow_run` tool** registered alongside `multi_agents_v2.rs` spawn/wait handlers. This is the only surface that lets the authoring model launch/compose workflows mid-turn and is the native home of the JS `workflow(name, args)` hook.
2. **Human-interactive: ONE `SlashCommand::Workflow` variant** (`tui/src/slash_command.rs`). The slash enum is compile-time strum, order-sensitive ("DO NOT ALPHA-SORT") — so named workflows **cannot** each be a variant. `/workflow` with no arg opens a runtime-populated picker (exact `SlashCommand::Skills → open_skills_menu` pattern in `slash_dispatch.rs:421`); `/workflow <name> [json]` dispatches by name with the rest of the line as args.
3. **Non-interactive/CI: `codex workflow run <name|path> --args <json> [--resume <runId>]`** in the clap `Subcommand` enum (`cli/src/main.rs:124`), mirroring `Exec`/`Cloud`.

### Saved-workflow discovery

Clone the skills loader (`core-skills/src/loader.rs:280-410`) into a new `core-workflows` loader. Discover `*.js` / `*.workflow.js` and **statically parse only the leading `export const meta = {...}` literal for the picker WITHOUT executing the body** (fail-open, like `load_skill_metadata` at `loader.rs:760` — security-critical: never eval during discovery). Roots in precedence order:

- `<repo>/.codex/workflows` (project-scoped, checked in, **recommended default**)
- `$HOME/.agents/workflows` (personal)
- `$CODEX_HOME/workflows`

Dedupe by path/name with scope precedence (like `dedupe_skill_roots_by_path`). Wire directory changes to the existing file-watcher, emitting `WorkflowsChanged => "workflows/changed"` next to `SkillsChanged`.

### Config / feature gating

Add a `Feature::Workflow` in `codex-rs/features/src/feature_configs.rs` (alongside `CodeMode`/`CodeModeOnly`/`CodeModeHost`), gated Experimental. Because a workflow bridges `code-mode` and the multi-agent runtime, `Feature::Workflow` **transitively requires** `CodeMode` + `MultiAgentV2`, validated in config resolution (`core/src/config/mod.rs`).

---

## 10. File-level touch list

| Crate / file | Add / Modify | Purpose |
|---|---|---|
| `codex-rs/code-mode/src/runtime/globals.rs` | Modify | Register `agent`/`workflow`/`phase`/`log`/`args`/`budget`; disable `Date.now`/argless `Date`/`Math.random`/`WeakRef`/`FinalizationRegistry`; inject prelude |
| `codex-rs/code-mode/src/runtime/callbacks.rs` | Modify | New `agent_callback` (ordinal stamp, cache key, budget pre-check); `phase_callback` |
| `codex-rs/code-mode/src/runtime/mod.rs` | Modify | `RuntimeEvent::AgentCall`/`Phase`; `RuntimeState.next_agent_ordinal`, replay cache, budget accumulator |
| `codex-rs/code-mode/src/runtime/module_loader.rs` | Modify | Run frozen determinism prelude before `evaluate_main_module`; cached-agent resolve reuses `resolve_tool_response` |
| `codex-rs/code-mode/src/runtime/value.rs` | Reuse | `json_to_v8` for `args` + structured returns |
| `codex-rs/code-mode/src/cell_actor/mod.rs` | Modify | Spawn path for `AgentCall` (like `spawn_tool`) |
| `codex-rs/code-mode-protocol/src/description.rs` | Modify | Parse `export const meta` (like `parse_exec_source`) |
| `codex-rs/core/src/tools/code_mode/execute_handler.rs` | Clone | `workflow` handler: parse meta, submit body, wire SpawnAgent delegate |
| `codex-rs/core/src/tools/code_mode/delegate.rs` | Modify | `DispatchMessage::SpawnAgent`; journal read/write |
| `codex-rs/core/src/codex_delegate.rs` | Modify | `agent()` on `run_codex_thread_one_shot`; TokenCount plumbing to budget; worktree cwd override |
| `codex-rs/core/src/agent/control.rs` | Modify | `SpawnAgentOptions.cwd`; resettable budget cell (replace OnceLock) |
| `codex-rs/core/src/tools/handlers/multi_agents_common.rs` | Modify | Respect override cwd in `apply_spawn_agent_runtime_overrides` |
| `codex-rs/core/src/agent/registry.rs` | Modify | Deterministic nickname; keep spawn caps as backstop |
| `codex-rs/core/src/rollout_budget.rs` | Modify | Public `spent()`/`remaining()`; replay-only `add_spent` |
| `codex-rs/git-utils/src/*` | Add | `worktree_add` + `WorktreeGuard` (create/dirty-check/remove) |
| `codex-rs/workflow-journal/` (new crate) | Add | `JournalRecorder`/`JournalLine`/`WorkflowRunMeta`, `key.rs`, `replay.rs` |
| `codex-rs/core-workflows/` (new crate) | Add | Saved-workflow loader (static meta parse, layered roots) |
| `codex-rs/protocol/src/protocol.rs` | Modify | `Workflow*` EventMsg variants + From impls |
| `codex-rs/app-server-protocol/src/protocol/v2/workflow.rs` | Add | `workflow/*` notification payloads |
| `codex-rs/app-server-protocol/src/protocol/common.rs` | Modify | `ServerNotification` variants + `WorkflowsChanged` |
| `codex-rs/app-server/src/bespoke_event_handling.rs` | Modify | EventMsg → ServerNotification mapping |
| `codex-rs/tui/src/app/agent_status_feed.rs` | Add | `WorkflowProgressCell` |
| `codex-rs/tui/src/app.rs` | Modify | `workflow/*` notification match arms |
| `codex-rs/tui/src/chatwidget/notifications.rs` | Modify | `Notification::WorkflowComplete` |
| `codex-rs/tui/src/slash_command.rs` + `chatwidget/slash_dispatch.rs` | Modify | One `Workflow` variant + runtime picker dispatch |
| `codex-rs/cli/src/main.rs` | Modify | `Subcommand::Workflow` |
| `codex-rs/features/src/feature_configs.rs` | Modify | `Feature::Workflow` (requires CodeMode + MultiAgentV2) |
| `codex-rs/state/migrations/` + `state/src/model/` | Add | `workflow_runs` discovery index |

---

## 11. Implementation phasing

Each milestone is independently shippable behind `Feature::Workflow` (Experimental).

### Phase 0 — Foundations & feature gate
`Feature::Workflow` (requires CodeMode + MultiAgentV2). Meta manifest parser + persisted-script registry (`core-workflows` loader, static parse). **Exit:** a script with `meta` parses, saves, and runs its body once in the existing isolate; a trivial `log()`/`phase()`-only workflow runs end-to-end; existing code-mode tests green.

### Phase 1 — MVP orchestration (the 80/20 value core)
Bind `agent(prompt, opts)` on `run_codex_thread_one_shot` (final text / null). Wire `opts.schema` → `final_output_json_schema` (nearly free). Ship `parallel()`, item cap 4096, concurrency cap `min(16,cores-2)`, lifetime cap 1000, `args`, `log()`, `phase()`. **Exit:** fan-out workflows (map N prompts → N structured results) with live progress. Covers ~11 of the parity capabilities.

### Phase 2 — Advanced scheduling & governance
`pipeline()` no-barrier scheduler (prelude async chains + shared semaphore). `budget` hard-ceiling: expose `RolloutBudget` as a JS global; `agent()` throws pre-spawn on `remaining() <= 0`; resettable budget cell. `workflow()` nested one-level via the Phase-0 registry + depth guard. **Exit:** bounded, composable multi-stage workflows.

### Phase 3 — Determinism & resume (highest novelty; depends on 3a)
**3a** Neutralize `Date.now`/argless `Date`/`Math.random`/`WeakRef`/`FinalizationRegistry`. **3b** `journal.jsonl` per `runId` (reuse RolloutRecorder append + ReverseJsonlScanner). **3c** `resumeFromRunId` prefix-replay loop keyed by `(prompt,opts)` ordinal; budget re-add on replay. **Exit:** crash/edit-resume of long runs.

### Phase 4 — Background & progress UX + worktree isolation
`workflow/*` protocol events + app-server notifications + TUI `WorkflowProgressCell` + completion notification. Entrypoints (`workflow_run` tool, `/workflow`, `codex workflow run`) fully wired. `isolation:'worktree'` lands as an independent workstream (`SpawnAgentOptions.cwd` + `git-utils` worktree lifecycle + workspace_roots). **Exit:** full parity.

---

## 12. Parity matrix

| Capability | Claude behavior | Codex v1 | Effort | Notes |
|---|---|---|---|---|
| `meta`/phases authoring; run body once | JS module run once by host | V8 isolate runs source once; add meta parser + registry | M | `module_loader.rs`; strongest reusable asset |
| `agent(prompt) -> final text; null on death` | Blocking, returns final text | `run_codex_thread_one_shot` → `last_agent_message`; None→null | L | NOT `spawn_agent`/`wait_agent` (fire-and-mailbox) |
| `opts.schema` validated object | Forced StructuredOutput | `final_output_json_schema` + strict; `serde_json` + jsonschema recheck | S | `turn.rs:1096`; `exec --output-schema` precedent |
| `opts.model` / `opts.effort` | Model/effort override | `apply_requested_spawn_agent_model_overrides` | S | validate effort vs `supported_reasoning_levels` |
| `opts.agentType` | Agent role | `apply_role_to_config` role_name | S | fork spawns reject overrides |
| `opts.label`/`opts.phase` | Progress attribution | metadata on spawn source; excluded from cache key | S | surface like `last_task_message` |
| `isolation:'worktree'` | Fresh git worktree, auto-remove if unchanged | **NEW**: `SpawnAgentOptions.cwd` + `git-utils worktree_add` guard | L | open issue #18969 |
| `parallel(thunks)` barrier; fail→null | Concurrent, awaits all | Prelude `Promise.all` + `.catch(()=>null)` | S | no native op |
| `pipeline(items,...stages)` no-barrier | Independent per-item staging | Prelude per-item promise chains + shared semaphore | M | staggered progress falls out |
| `phase(title)` | Progress grouping | `RuntimeEvent::Phase` → `WorkflowPhase*` | S | journaled for resume tree |
| `log(msg)` | Narrator line | Alias over `notify_callback` → `WorkflowLog` | S | already present |
| `args` | JSON injected | `json_to_v8` global | S | one `set_global` |
| `budget{total,spent(),remaining()}` hard ceiling | agent() throws at ceiling | `RolloutBudget` (output weight 1.0) + pre-admission throw | M | soft→hard is new; one-turn overshoot |
| `workflow(name,args)` one level | Nested inline run | Registry load + re-enter runtime; depth guard | M/L | `agent_max_depth` |
| Concurrency `min(16,cores-2)`, queued | Cap + queue | `tokio::Semaphore(cap)` clamp; registry backstop | S/M | raise `effective_agent_max_threads` clamp |
| Lifetime cap ~1000 | Monotonic run cap | New `AtomicUsize`, never decrements | S | separate from registry `total_count` |
| Item cap 4096 | Per parallel/pipeline | Prelude guard | S | |
| Subagent returns raw data | Final text is return value | Prompt/preamble convention + `opts.schema` for typing | S | prefer schema for consumers |
| MCP reachable from subagents | On demand | Inherited `mcp_manager`/skills/plugins | S | already first-class |
| Determinism: disable time/random | Date/Math off | **NEW** frozen prelude in `install_globals` | M | prerequisite for resume |
| journal.jsonl per return | Records each agent return | **NEW** `codex-workflow-journal` (reuse RolloutRecorder) | M/L | subagents keep own rollout files |
| Resume by prefix (`resumeFromRunId`) | Longest unchanged prefix replays | **NEW** ordinal + `(prompt,opts)` key + replay loop | XL/L | depends on determinism |
| Background + progress tree + persisted/re-invocable | Backgrounded, live tree, saved | events + TUI cell + notify + saved dir + subcommand | L | data plane largely exists |

---

## 13. Risks & open questions

### Risks (ranked)

- **R1 — Resume determinism is load-bearing and fragile (Critical).** Prefix replay is meaningless if the script is nondeterministic. `Date.now`/`Math.random`/`WeakRef`/`FinalizationRegistry` are *not* currently disabled (`globals.rs:16-19`); shipping the journal without the harden makes resume unsound. Mitigation: Phase 3 explicitly depends on 3a; key by `(prompt,opts)` content hash + invocation ordinal (not wall-clock); ship resume opt-in/experimental; version the key algorithm.
- **R2 — Blocking a JS promise on a long-running subagent in a single isolate (High).** The isolate is single-threaded; `agent()` must suspend via the existing yield/wait cell mechanism and support **many** concurrent suspended `agent()` promises through `cell_actor` without starving the yield timer or exhausting the command channel (up to 4096 items). Mitigation: prototype `agent()` against `yield_control`/`wait` plumbing early in Phase 1.
- **R3 — Budget is a soft reminder today, not a hard gate (High).** `record_usage` is post-turn; a synchronous pre-spawn throw introduces staleness/races and can overshoot by one in-flight turn. `configure` is `OnceLock` (can't reconfigure a reused `AgentControl`). Mitigation: reserve optimistically at spawn, reconcile on completion; resettable budget cell; keep per-agent effort/output caps low.
- **R4 — Worktree isolation intersects sandbox + concurrent cwd (Medium).** Many concurrent worktrees multiply disk usage; cleanup-if-unchanged can leak dirs or corrupt the repo on child crash. Mitigation: independent workstream, dirty-check before removal, namespaced index-derived dirs, robust `Drop` guard.
- **R5 — Progress event volume at 1000-agent scale (Medium).** Can flood the app-server/TUI. Mitigation: aggregate per phase, throttle, collapse finished phases, reuse `TokenCount` batching, bound like `AGENT_STATUS_PREVIEW_*`.
- **R6 — Bridging two flag-gated subsystems (Medium).** A workflow inherits both `CodeMode` and `MultiAgentV2` flag matrices. Mitigation: single `Feature::Workflow` that transitively requires both, validated at config resolution.
- **R7 — Hash stability across Codex versions (Medium).** Nondeterministic JSON/schema serialization silently busts the whole prefix. Mitigation: canonical sorted-key JSON + stable schema encoding; store `key_algo_version` in `run_meta`.
- **R8 — Two source-of-truth (Low).** journal.jsonl vs SQLite index. Mitigation: JSONL authoritative for replay; SQLite is a rebuildable projection.

### Open questions

1. **Isolate concurrency**: does `cell_actor`/`runtime.rs` `exec`/`wait` support many simultaneously-suspended `agent()` promises, or does it assume one outstanding cell? Verify before committing the Phase 1 `agent()` design.
2. **Structured output provider coverage**: does `TurnComplete.last_agent_message` reliably carry strict-schema JSON for **non-OpenAI** providers, or only those honoring `output_schema_strict`? Determines how load-bearing the belt-and-suspenders `jsonschema` recheck is.
3. **Budget token source**: aggregate child `TokenCount` events at `TurnComplete`, or read the child session's final usage snapshot? And is partial output of a budget-aborted turn metered before or after the abort (affects replay reproducibility of the exact throw boundary)?
4. **Nickname randomness**: can `reserve_agent_nickname_with_preference` fully bypass `rand::rng()` (`registry.rs:232`) for indexed agents?
5. **Racing**: does v1 truly forbid `Promise.race`/first-wins on `parallel` results? If later allowed, `completion_seq` journaling becomes mandatory for deterministic replay.
6. **Execution location**: in-process app-server vs `code-mode-host` sidecar for background runs — decides whether `workflow/*` events originate as core `EventMsg` or are injected at the app-server layer.
7. **Cap policy**: override `effective_agent_max_threads` for workflow-owned trees, or enforce `min(16,cores-2)` purely in the orchestration layer? And do nested `workflow()` runs share or nest the concurrency/lifetime/budget caps?
8. **Worktree cleanup ownership**: the `AgentCall` host handler on completion, or a session-scoped cleanup at `session_runtime` shutdown?