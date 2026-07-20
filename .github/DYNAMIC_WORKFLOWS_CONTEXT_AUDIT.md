# Dynamic Workflows context-bound audit

Date: 2026-07-19  
Scope: fork issue #59, the final model-visible-context review, and workflow-selected agent roles.

## Result

Dynamic Workflows does not add a hidden or continuously refreshed parent-context fragment. The
parent model sees one static, saved-name-only `workflow_run` tool and one bounded function result
containing the host-minted run ID and initial status. Background progress, journals, saved source,
registry descriptions, child transcripts, and TUI monitor state remain on protocol, storage, and UI
channels rather than being copied into parent history.

Accepted model-authored `workflow_run` calls are capped at 4 KiB of raw arguments, with the nested
`args` value capped at 3 KiB of serialized JSON. A workflow-authored `agent()` prompt can become a
child's first user item only after the 8,192-token-estimate gate passes and its complete serialized
message envelope, including the fixed-width response-item ID and turn metadata assigned by Core,
fits within 8 KiB. Core validates the real finalized item again before history or persistence. An
optional response schema has a separate 8 KiB serialized/depth-64 gate. No accepted
workflow-specific context item can reach 10,000 tokens.

Between model-generated items, a workflow child's injected history is capped at 64 logical items
and 64 KiB of serialized response items. Before the supervisor's exact user prompt is recorded,
noncritical context is restricted to 63 items and 56 KiB, reserving one complete 8 KiB item slot.
Only the typed `ExactUserPrompt` path can consume that reservation: a message that merely claims a
`user` role remains noncritical. Noncritical messages and tool outputs are truncated or replaced by
bounded omission markers; the exact prompt is either preserved byte-for-byte or rejected before
the turn starts.

Workflow children are also isolated from generic collaboration context. Their ownership is
persisted as `ThreadSource::Feature("workflow")`, reconstructed after rollout resume, and treated as
sticky when SQLite, rollout metadata, or live state disagree. Generic collaboration tools,
`workflow_run`, and goal extension tools are absent from their advertised specs and have runtime
handler guards; workflow children are excluded from `list_agents` and `<subagents>` context; and
terminal child output is not delivered a second time through the parent mailbox. Direct external
response-item injection is forbidden. The same checks apply after registry loss, cold resume, and
fork. Generic collaboration and app-server lifecycle handlers preflight a captured ordinary
ancestor subtree and reject the complete operation when that snapshot contains an exact workflow
descendant. Generic V1 close/resume additionally share a manager-wide semaphore with fresh child
registration: close/resume acquire the complete permit budget across capture, preflight, and the
exact-snapshot mutation, while concurrent spawns each retain one permit through persisted-edge
registration. This closes the generic collaboration late-spawn race across distinct `AgentControl`
handles without holding a lock type forbidden across async suspension.

The app-server archive/delete subtree check remains fail-closed for every descendant present in the
captured live/persisted graph, but those endpoints do not yet acquire the Core semaphore. A workflow
child registered after app-server capture is not included in the immutable archive/delete list and
therefore is not archived, deleted, or notified; the residual P2 race is a possible late-spawn orphan
or cancellation caused by root shutdown. Final delivery should expose the same lifecycle gate through
`ThreadManager`, hold it across app-server capture, preflight, and mutation, and add a deterministic
barrier regression. A post-prepare recheck alone is insufficient.

Role-selected context and the MultiAgentV2 context forced by the workflow dependency are bounded:
each role-authored base/developer/compact lane is at most 4 KiB and their combined overridden
payload is at most 8 KiB; configurable MultiAgentV2 prompt fields are at most 4 KiB each and 8 KiB
combined; and the model-visible role catalog is at most 4 KiB, 32 entries, and 2 KiB per entry.
Role validation is atomic, so an oversized file-backed value cannot partially mutate the child
configuration. The standalone MultiAgentV2 usage hint is represented by a
`ContextualUserFragment`, rather than inserted as an untyped raw developer message.
Workflow child admission also checks every custom inherited base/developer/compact lane against an
8 KiB tokenizer-independent byte ceiling after role resolution and before child creation. Exact
model-selected base instructions use separate 32 KiB and 8,192-estimated-token backstops because
the reviewed built-in/model lane is larger than 8 KiB. Workflow-child developer fragments are
emitted as separate response items instead of aggregating inherited instructions with permissions,
skills, or extension context.

## Boundary inventory

| Fragment | Can reach a model? | Hard bound | Enforcement and failure behavior |
| --- | --- | --- | --- |
| `workflow_run` tool schema | Parent tool list | Static closed schema | Accepts only `name`, `args`, and `resumeFromRunId`; there is no source or path field. |
| Model-authored `workflow_run` call | Parent response/history and handler | 4 KiB raw arguments; 3 KiB nested serialized `args` | Raw size is checked before parsing and nested size before execution. Rejection returns a bounded error and starts no run. |
| `workflow_run` result | Parent tool history | Fixed `{runId,status}` shape | Returned once after durable initialization. Live progress and final output are not appended later. |
| Model-visible workflow errors | Parent tool history | 768 bytes UTF-8 | Truncated at a character boundary by `bound_model_error`; parser excerpts are independently capped. The byte cap keeps even adversarial tokenization below 1,000 tokens. |
| MultiAgentV2 configurable hints | Parent or ordinary subagent context/tool schema | 4 KiB per field; 8 KiB combined | Config load rejects oversized `usage_hint_text`, root/subagent hints, or custom mode text without echoing content. Workflow children receive none of these hints. |
| Role catalog | `spawn_agent` tool schema | 4 KiB total; 32 entries; 2 KiB/entry | Deterministic user-first ordering, UTF-8-safe truncation, and an explicit omission marker. Workflow children do not receive the collaboration tool. |
| Role-selected instructions | Child base/developer context and later compact prompt | 4 KiB per overridden lane; 8 KiB combined | Effective inline and file-backed values are validated before assignment. Failure leaves the original `Config` unchanged and spawns no child. |
| Custom inherited workflow-child instructions | Child base/developer context and later compact prompt | 8 KiB UTF-8 per lane | Effective inherited lanes are checked after role resolution and before child creation; oversize rejects the child without echoing content. Developer fragments are emitted as standalone response items. |
| Model-selected workflow-child base instructions | Child request `instructions` field | 32 KiB UTF-8 and 8,192 estimated tokens | The lane must exactly match the current parent model/personality instructions; custom config or rollout overrides use the smaller 8 KiB ceiling. |
| Generic subagent environment context | Ordinary parent developer context | 32 entries; 2 KiB rendered | Workflow-managed children are filtered by both live metadata and persisted thread source. Remaining text is UTF-8 safely truncated. |
| Saved workflow name | Tool call, registry lookup, events/UI | 256 bytes | Checked in TUI/app-server/core entrypoints and again at nested dispatch. |
| Saved description | Registry and UI only | 4 KiB | Static manifest validation; TUI renders a smaller bounded projection. Never inserted into model history. |
| Static phase list | Events/UI only | 256 items; 512 bytes/title | Static manifest validation happens without evaluating the body. |
| Workflow source | Isolate only | 1 MiB execution read; 256 KiB manifest scan | Source must resolve from saved workflow roots. It is never accepted from the model tool or injected into history. |
| Trusted CLI/app-server invocation `args` | Isolate only, unless workflow code derives a prompt | 32 KiB serialized JSON | Checked at TUI, app-server, core, and V8 execution boundaries. A derived child prompt must separately pass the prompt gate. |
| `agent()` prompt | Child first user item and child rollout | 8,192 estimated tokens; 8 KiB complete serialized `ResponseItem` | V8 rejects before host dispatch. Core preflights the exact fixed-width ID/turn-metadata envelope before hashing, journaling, budget reservation, worktree creation, or spawn, then revalidates the finalized item before history/persistence. |
| Trailing injected workflow-child history segment | Child model request and rollout | 64 logical items; 64 KiB serialized; 8 KiB/item | Finalized appends are charged by their complete serialized envelope. Before the exact prompt, noncritical items are limited to 63 items/56 KiB. Noncritical text/tool output is truncated or omitted with a bounded marker; model output and existing history are never rewritten. |
| External `thread/injectItems` | None for workflow children | Forbidden | App-server rejects the request before parsing or submitting injected items. Trusted internal typed lanes remain subject to the finalized-history bounds. |
| `agent()` label and phase | Events, journal, UI only | 512 bytes each | Rejected before host dispatch and revalidated before journal context construction. |
| `agent()` model, effort, role, isolation | Child configuration and journal hashes | 256 bytes each | Rejected before host dispatch and revalidated before configuration/journal copies. |
| `agent()` output schema | Child response-format context | 8 KiB serialized JSON; depth 64 | V8 rejects before host dispatch; core revalidates before configuration, compilation, or spawn. |
| `agent()` return | Workflow isolate; possibly later `text()` output | 32 KiB serialized JSON | Checked before promise resolution and journal append. Oversized live output becomes the deterministic `null` failure result. |
| Nested `workflow()` return | Parent workflow isolate; possibly later `text()` output | 32 KiB serialized JSON | The terminal nested result is joined, checked, then resolved. Oversized results reject before entering the parent isolate. |
| Workflow `text()` output | Background result or nested return | 32 KiB aggregate serialized text; 256 items | Workflow-mode V8 accounts each item before emission. Aggregate/count overflow terminates the run without a partial nested result. |
| `log()` | Journal, progress, CLI, UI only | 4 KiB/event; 4,000 events | V8 rejects before journal/progress emission. CLI and TUI apply smaller display caps. |
| Dynamic `phase()` | Journal, progress, CLI, UI only | 512 bytes/event; 1,000 events | V8 rejects before journal/progress emission. |
| Topology | Progress, app-server, TUI only | 4,000 runtime nodes | Durable progress has independent 10,000-node/2 MiB file caps; app-server/TUI apply smaller collection/display limits. |
| Progress errors | Progress, app-server, TUI only | 2 KiB | Bounded before durable or live terminal emission. |
| Journal record | Resume/inspection only | 128 KiB/record; 60,000 records; 192 MiB/run | Writer rejects before append; replay rejects before materializing an oversized record or scanning an oversized file. |
| Resume replay entry | Workflow isolate only | Agent-return bound plus journal record/file/count bounds | Corrupt data fails closed. A legacy return over the current cap truncates the replayable prefix and executes from that ordinal live. |
| CLI watch narration | Terminal only | 256 lines; 32 KiB output; 512 KiB source read | Never enters model context. |
| TUI picker/monitor | UI only | Bounded pages, cursors, items, nodes, phases, logs, and rendered text | Oversized app-server values are rejected or projected into bounded display strings. |

## Required manual review for items that can exceed 1,000 tokens

The following accepted maxima can exceed 1,000 tokens under an adversarial tokenizer but remain
below the repository's 10,000-token per-item ceiling. They were reviewed explicitly rather than
treated as incidental configuration:

- `agent()` prompt: 8,192 estimated tokens and an 8 KiB complete serialized message envelope;
  required to carry a useful delegated task, checked before child creation and after finalization,
  and isolated to the child. Its 8 KiB/one-item reservation cannot be consumed by untrusted
  user-shaped context.
- One trailing injected workflow-child segment: 64 KiB/64 logical items, with no item over 8 KiB.
  The segment resets after model output, so the aggregate is bounded rather than growing across
  the complete transcript.
- `agent()` output schema: 8 KiB/depth 64; required for useful structured delegation and sent only
  as that child's response format.
- One role-selected instruction lane: 4 KiB, with an 8 KiB combined ceiling; rejected atomically
  and never copied into the parent.
- One custom inherited workflow-child base/developer/compact lane: at most 8 KiB. These values
  already belong to the parent session, are checked again after role resolution, and reject child
  creation rather than being truncated or copied into parent history. Developer fragments remain
  standalone items rather than being aggregated with other context.
- Exact model-selected base instructions: at most 32 KiB and 8,192 estimated tokens. This preserves
  the reviewed product/model instruction lane (the repository default is roughly 21 KiB) while a
  config or rollout override receives the smaller tokenizer-independent 8 KiB ceiling.
- One configurable MultiAgentV2 hint or the bounded role catalog: 4 KiB, with an 8 KiB aggregate
  hint ceiling. Workflow children receive neither collaboration surface.
- Ordinary `<subagents>` environment context: 2 KiB/32 entries. It contains identifiers and
  nicknames only, and workflow-managed children are excluded.
- Accepted raw `workflow_run` arguments: 4 KiB (nested args 3 KiB). This is model-authored function
  output rather than a Codex-injected fragment; the accepted size is still bounded before parsing.

An invalid oversized function call has already been emitted by the model before its handler can
reject it. The handler deliberately does not rewrite or truncate that historical response item,
because Codex history is incremental and must not be rewritten. The policy signoff here covers the
accepted workflow call boundary and its bounded error; it does not claim that a tool handler can
retroactively cap arbitrary invalid model output.

## No partial context or history rewrite

- V8 rejects oversized prompts, schemas, execution arguments, labels, phases, and logs before
  creating a resolver or sending host dispatch.
- Core repeats model-sensitive checks before journal construction; a hostile or mismatched process
  host cannot place an oversized accepted value into the journal, progress projection, or child
  request.
- Core charges the complete finalized serialized envelope for every workflow-child history append.
  It preserves the trusted supervisor prompt exactly, while truncating or omitting only newly
  injected noncritical items. Existing history and model-generated output are never rewritten.
- Normal and compacting provider requests validate the complete projected history immediately
  before sampling. Remote v2 compaction appends its `CompactionTrigger` first and validates that
  final request input, so the trigger cannot bypass the aggregate ceiling.
- A rejected child return is represented as `null`; only `null`, never a truncated semantic value,
  is journaled and replayed.
- The journal reader validates bounded data before replay. A legacy return over the newer cap is an
  unavailable cache entry, not model input.
- Background workflow events never call the context manager. Workflow isolates omit `notify()`.
- Generic collaboration, `workflow_run`, and goal extension tools are omitted from workflow child
  planning and rejected again by their runtime handlers. Direct external injection, global MCP
  refresh, and global user-config reload also exclude workflow children. Persisted sticky ownership
  keeps those rules after resume/restart even when live and durable metadata disagree.
- The only parent-history mutation is the ordinary incremental `workflow_run` call/result. Resume
  reconstructs isolate promises from a durable prefix and never rewrites prior items.

## Verification anchors

- Shared workflow bounds: `code-mode-protocol/src/workflow_bounds.rs`
- V8 pre-dispatch checks: `code-mode/src/runtime/workflow_bounds_tests.rs`
- Core prompt/schema/option checks: `core/src/tools/code_mode/workflow_context_bounds.rs`
- Finalized prompt reservation, append accounting, compaction, and provider-bound validation:
  `core/src/context/{workflow_child_history.rs,workflow_child_history_tests.rs}` and
  `core/src/session/workflow_child_boundaries_tests.rs`
- Model-call argument checks: `core/src/tools/code_mode/workflow_handler/{bounds.rs,adapter_tests.rs}`
- Saved-only tool: `core/src/tools/code_mode/workflow_spec.rs`
- Role bounds/catalog: `core/src/agent/{role_context_bounds.rs,role_context_bounds_tests.rs}`
- MultiAgentV2 hint bounds: `core/src/config/{multi_agent_v2_bounds.rs,multi_agent_v2_bounds_tests.rs}`
- Typed usage-hint fragment: `core/src/context/multi_agent_usage_hint.rs`
- Durable workflow-child ownership and collaboration gates:
  `core/src/{agent/control.rs,agent/control/generic_collaboration.rs,agent/agent_resolver.rs,session/session.rs,session/turn.rs,thread_manager.rs}`
- External mutation and injection gates:
  `app-server/src/request_processors/{thread_lifecycle.rs,thread_processor.rs,thread_delete.rs,turn_processor.rs,mcp_processor.rs}`
- Sticky durable source and completion-delivery authority:
  `thread-store/src/{local/read_thread.rs,thread_metadata_sync.rs}`, `state/src/extract.rs`, and
  `rollout/src/state_db_tests.rs`
- Restart/resume isolation test:
  `core/src/agent/control_tests.rs::workflow_managed_ownership_survives_registry_rebuild_and_resume`
- Cross-host real child isolation:
  `core/tests/suite/workflow_uat.rs::{workflow_narration_stays_out_of_parent_context_across_hosts,workflow_child_cannot_inject_parent_mailbox_across_hosts}`
- Child return and pre-journal checks:
  `core/src/tools/code_mode/delegate/{agent_execution,agent_journal,agent_output}.rs`
- Nested terminal bound: `core/src/tools/code_mode/workflow_handler/nested.rs`
- Journal write/replay bounds: `workflow-journal/src/{lib.rs,recorder.rs,replay.rs}`
