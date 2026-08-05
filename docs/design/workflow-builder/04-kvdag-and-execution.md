# kvdag data model and execution

The kvdag primitive, the scheduler, how a node becomes a pane running a `claude`
teammate, how completion is decided, how steering works, the anti-stuck
watchdog, and the tier mapping.

**kvdags do not exist in this codebase today** (`00-overview.md` §0). Everything
here is new.

---

## 1. Module layout

```
src/workflow/
  mod.rs              // WorkflowRuntimeState, public entry points, feature gate
  model.rs            // kvdag types: Kvdag, KvdagNode, KvdagEdge, Condition, Demand
  tier.rs             // Tier, Effort, ModelAlias, resolve(tier, demand)
  engine/
    mod.rs            // Engine facade: apply(EngineInput) -> Vec<RunEffect>
    graph.rs          // RunGraph: run-time node/edge instances, ready set
    schedule.rs       // ready-set computation, admission, concurrency limits
    expand.rs          // expansion proposals, guardrails, commit/reject
    complete.rs       // completion evidence, output-schema validation, succession
    watchdog.rs       // no-progress streaks, materiality, escalation ladder
  binding/
    mod.rs            // RunEffect -> App calls; App facts -> EngineInput
    spawn.rs          // argv/env construction, pane creation, node dir layout
    observe.rs        // detection, hook, transcript-tail progress evidence
  store/              // SurrealDB (see 03-storage-schema.md)
  layout.rs           // layered DAG layout -> node rects + edge routes (TUI input)
```

`model.rs`, `tier.rs`, `engine/*`, and `layout.rs` have **no** dependency on
`App`, PTYs, or SurrealDB. They are pure and unit-testable in the same way
`AppState::test_new()` works today.

---

## 2. kvdag types (Rust sketch)

```rust
// ── definition (immutable, one per kvdag_version) ────────────────────────────

pub struct Kvdag {
    pub version_id: KvdagVersionId,
    pub workflow_id: WorkflowId,
    pub version: u32,
    pub parent: Option<KvdagVersionId>,
    pub contract: String,          // prepended to every node's system prompt
    pub growth: GrowthLimits,
    pub args: Vec<ArgSpec>,        // declared run-argument namespace (see below)
    pub nodes: Vec<KvdagNode>,     // topologically sorted at construction
    pub edges: Vec<KvdagEdge>,
    pub spec_digest: SpecDigest,   // sha256 of canonical (nodes, edges)
}

/// A run argument supplied at `workflow.run` time (`kvx workflow run start … --arg k=v`)
/// and materialised into `workflow_run.args`. Declaring the namespace is what makes
/// `{{goal}}` in a root node's prompt template resolvable.
pub struct ArgSpec {
    pub name: String,
    pub required: bool,
    pub default: Option<String>,
    pub description: String,
}

pub struct NodeKey(pub String);    // stable across versions

pub struct KvdagNode {
    pub key: NodeKey,
    pub label: String,
    pub role: String,
    pub kind: NodeKind,
    pub demand: Demand,
    pub runner: Runner,                // how the node's process is bound (see §4.2)
    pub command: Option<Vec<String>>,  // required iff runner == Command; argv, never a shell string
    pub prompt_template: String,   // {{name}} slots filled from inbound edge ports or run args
    pub system_contract: Option<String>,
    pub output_schema: OutputSchema,   // JSON Schema; validated before completion
    pub max_attempts: u8,
    pub timeout_ms: Option<u64>,
    pub isolation: Isolation,          // None | Worktree
    pub is_template: bool,             // only instantiated via an expand proposal
    pub expand_allow: Vec<NodeKey>,
    pub expand_max: u16,               // 0 = this node may not expand (the default)
}

pub enum NodeKind {
    Agent,     // a claude teammate in a pane — the normal case
    Internal,  // karvex-owned utility (summariser, interviewer); still a visible node
    Gate,      // no agent; evaluates conditions / waits for a human decision
    Monitor,   // polls a condition; "checked, unchanged" is a legal no-op result
}

/// Selects the *binding* the spawner uses. Orthogonal to `NodeKind`, and never
/// inferred from "are we in a test": a definition states it explicitly.
pub enum Runner {
    /// `claude` in a pane. Confirmed with `begin_managed_agent`, steered with
    /// `agent.prompt`. The default for `NodeKind::Agent`.
    Agent,
    /// A plain process in a pane, launched from `command`. No managed-agent
    /// confirmation, no agent detection, steered with `pane.send_text`.
    Command,
}

pub enum Demand { Peak, Critical, Standard, Light }

pub struct KvdagEdge {
    pub from: NodeKey,
    pub to: NodeKey,
    pub kind: EdgeKind,            // Sequence | Data | Conditional
    pub condition: Option<Condition>,
    pub payload: EdgePayload,      // None | Summary | Full
    pub port: Option<String>,      // template slot in the target's prompt
}

/// Total, loop-free, side-effect-free predicate over a node's validated output.
/// Deliberately NOT Turing-complete (see 02-mjs-workflow-evaluation.md §5).
pub enum Condition {
    Always,
    Exists { path: FieldPath },
    Eq { path: FieldPath, value: JsonScalar },
    Cmp { path: FieldPath, op: CmpOp, value: JsonScalar },  // Lt Le Gt Ge
    OneOf { path: FieldPath, values: Vec<JsonScalar> },
    Not(Box<Condition>),
    All(Vec<Condition>),
    Any(Vec<Condition>),
}

pub struct GrowthLimits { pub max_depth: u16, pub max_nodes: u16 }
```

Construction invariants, checked once in `Kvdag::try_new` and covered by tests:
acyclicity; every edge endpoint exists; **every `{{name}}` in a prompt template
resolves to either an inbound edge `port` on that node or a declared `ArgSpec`
name** (unresolved names are rejected — this is what lets a root node reference
`{{goal}}`); `expand_allow` references only `is_template` nodes; at least one
root; no unreachable non-template node; `output_schema` parses; `command` is
`Some` and non-empty iff `runner == Command`. `workflow.run` separately rejects a
run that omits a required `ArgSpec` with no default.

```rust
// ── run graph (mutable during a run, pure data) ─────────────────────────────

pub struct RunGraph {
    pub run_id: RunId,
    pub version_id: KvdagVersionId,
    pub tier: Tier,
    pub growth: GrowthLimits,
    pub nodes: Vec<RunNode>,               // index == RunNodeIdx
    pub edges: Vec<RunEdge>,
    pub status: RunStatus,
    pub seq: u64,                          // journal cursor
}

pub struct InstancePath(pub String);       // "research/2/verify" — unique per run

pub struct RunNode {
    pub idx: RunNodeIdx,
    pub key: NodeKey,
    pub path: InstancePath,
    pub parent: Option<RunNodeIdx>,
    pub depth: u16,
    pub status: NodeStatus,
    pub assignment: Assignment,            // model + effort, resolved from tier
    pub attempt: u8,
    pub binding: Option<NodeBinding>,      // pane + claude session, once spawned
    pub result: Option<NodeResult>,        // schema-validated output
    pub usage: NodeUsage,
    pub progress: ProgressTracker,         // watchdog state
    pub succession: Option<Succession>,
}

pub enum NodeStatus {
    Pending, Ready, Running, NeedsAttention, Blocked,
    Succeeded, Failed, Skipped, Restored, Cancelled,
}

pub struct NodeBinding {
    pub pane_id: PublicPaneId,
    pub terminal_id: TerminalId,
    pub agent_session_id: Uuid,      // karvex-assigned via `claude --session-id`
    pub transcript_path: PathBuf,    // derived, known before the process starts
    pub node_dir: PathBuf,
    pub cwd: PathBuf,
}

pub enum Succession {
    Satisfied,                                   // outbound edges got a valid result
    Blocked { reason: String, resume_when: String },
    NoFollowup { evidence: String },
}
```

```rust
// ── engine interface: pure state machine ────────────────────────────────────

pub enum EngineInput {
    Start { graph: RunGraph },
    NodeSelfReport { path: InstancePath, token: NodeToken, result: RawJson },
    TurnEnded { pane: PublicPaneId },                 // Claude `stop` hook
    AgentStatus { pane: PublicPaneId, state: AgentState, at: Instant },
    ProgressObserved { path: InstancePath, delta: ProgressDelta },
    PaneExited { pane: PublicPaneId, code: Option<i32> },
    Steer { path: InstancePath, text: String },
    Interrupt { path: InstancePath },
    RestartNode { path: InstancePath },
    CancelRun,
    Tick { now: Instant },
}

pub enum RunEffect {
    SpawnNode { path: InstancePath, spec: SpawnSpec },
    PromptNode { pane: PublicPaneId, text: String },
    SendKeys   { pane: PublicPaneId, keys: Vec<String> },
    ClosePane  { pane: PublicPaneId },
    Persist(StoreWrite),                 // run_event / run_node / checkpoint
    Emit(WorkflowEvent),                 // JSON-API event
    Notify(UserNotice),                  // toast / DAG banner
}

impl Engine {
    pub fn apply(&mut self, input: EngineInput) -> Vec<RunEffect>;
}
```

`apply` is the whole engine contract: no async, no I/O, deterministic given a
supplied clock. Everything below is expressed in these terms.

---

## 3. Scheduler semantics

### 3.1 Ready set

A node is **Ready** when every inbound edge has *resolved*:

| Edge kind | Resolved when |
|---|---|
| `Sequence` | source is `Succeeded` or `Restored` |
| `Data` | source is `Succeeded`/`Restored` **and** its result validated against its output schema |
| `Conditional` | source terminal **and** `condition` evaluated; `true` ⇒ edge fires, `false` ⇒ edge is *dead* |

A node all of whose inbound edges are dead becomes `Skipped` (not `Failed`), and
`Skipped` propagates the same way: a `Sequence`/`Data` edge from a `Skipped`
source is dead. This gives conditional branches without scripting.

Admission: at most `max_parallel_nodes` (default 4, config, tier-influenced) are
`Running` at once. Ready nodes queue in `(depth, path)` order so breadth stays
predictable and the DAG view does not reshuffle.

Root selection: nodes with no inbound edges start `Ready` at run start, except
`is_template` nodes, which are never scheduled directly.

### 3.2 Terminal state of a run

A run finishes only when the **conjunction** holds — never when "no node is
runnable", which is the classic false-completion bug:

```
run_terminal_ready =
      no node in {Pending, Ready, Running, NeedsAttention}
  AND every non-Skipped node has a Succession
  AND no unaccepted expansion proposal is outstanding
  AND no Monitor node has an unsatisfied resume condition
```

If the graph stalls without satisfying this, the run enters `Paused` with a
surfaced reason naming the specific unmet conjunct, rather than reporting
success. (Adapted from LoopX's `terminal_ready()`.)

### 3.3 Succession — every closing node resolves

On reaching a terminal state a node must record exactly one `Succession`:

- **Satisfied** — validated result present, outbound edges resolved from it.
- **Blocked** — a structured blocker with an explicit `resume_when`. The run may
  continue on other branches; the blocker is rendered on the node and in the run
  banner.
- **NoFollowup** — explicit terminal evidence that nothing follows (e.g. a
  conditional branch that legitimately produced nothing).

A terminal node with no succession is an engine error (`SuccessionGap`) that
transitions the node to `NeedsAttention` and emits an event. This is what stops a
branch from quietly evaporating while the run reports success.

### 3.4 Dynamic expansion (Phase 2)

Proposal → guardrail → commit. A node **cannot create nodes**; it proposes.

1. **Propose.** The node's validated result may include
   `expand: [{ template: NodeKey, label, inputs, count? }]`, or the node calls
   `kvx workflow node expand --template <key> --label <l> --input k=v` mid-run.
   Either way the engine records `expand_proposed` in the journal.
2. **Validate.** Reject unless: `template` ∈ the proposing node's `expand_allow`;
   the template node exists and `is_template`; accepted children for this node
   `< expand_max`; `parent.depth + 1 ≤ growth.max_depth`; live node count
   `< growth.max_nodes`.
3. **Commit or reject.** Accepted → a new `RunNode` at
   `<parent path>/<template key>/<n>`, a `spawned` relation from parent to child,
   inherited edges (the child's outbound edges default to the parent's outbound
   edges so the fan-in point is preserved), and a `node_created` event → the DAG
   view shows it live. Rejected → `expand_rejected` + `growth_limited` events
   carrying the exact limit hit, rendered as a badge on the proposing node and a
   run-level banner. **Never silently truncated** (spec Feature 1 requirement).
4. **Teammate-spawned teammates** are the same mechanism at depth ≥ 2. Because
   karvex is the spawner, there is no nesting ceiling to work around — the
   ceiling is `max_depth`, which is karvex's own, configurable, and visible.

**Defaults and the narrowing rule.** `kvdag_version` carries the authoritative
limits, defaulting to `max_depth = 3`, `max_nodes = 24` (`03-storage-schema.md`
§4.1). `expand_max` defaults to **0** — expansion is opt-in per node, matching
`expand_allow`'s empty default; a node that is allowed to fan out must say so
explicitly, and `4` is the suggested value when it does.

A run's effective limits are **always ≤ the version's** — a run narrows, never
widens. The tier's influence is therefore purely a narrowing one (§7.4):

| Tier | Effective `max_nodes` |
|---|---|
| `max` / `high` | the version's `max_nodes` (no narrowing) |
| `medium` | `min(version.max_nodes, 24)` |
| `low` | `min(version.max_nodes, 12)` |

An author who wants a wider graph raises `max_nodes` **on the version**, which is
an authoring edit that creates a new `kvdag_version` — not a run-time override.
This keeps `workflow_run.max_nodes <= kvdag_version.max_nodes` a true invariant
that the store can assert.

---

## 4. Execution binding: node → pane → `claude` teammate

### 4.1 Node directory

Each `run_node` gets `<run dir>/<instance path>/` containing:

| File | Written by | Purpose |
|---|---|---|
| `task.md` | karvex | rendered prompt: contract + role + filled `{{port}}` slots |
| `inputs/*.json` | karvex | upstream checkpoint summaries (or full payloads for `payload: full` edges) |
| `output_schema.json` | karvex | the JSON Schema the result must satisfy |
| `result.json` | the node | the node's structured result |
| `artifacts/` | the node | anything large; indexed into the checkpoint |

### 4.2 Spawn

The binder calls `Workspace::split_pane_argv_command` **in-process** — the
function is already used in-process by two callers today, the plugin pane-open
path (`src/app/api/plugins/panes.rs`) and the built-in scrollback-editor launch
path (`src/app/input/navigate.rs`, via `spawn_overlay_argv_command`) — so there
is no shell wrapper, no typed-keystroke race, and env is injected
deterministically.

**Two binding paths, selected by the node's `runner` field — never by
test-vs-production:**

| `runner` | argv | Spawn confirmation | Steering / delivery | Completion |
|---|---|---|---|---|
| `Agent` | the `claude` argv below | `terminal.begin_managed_agent(name, Agent::Claude, …)`; failing to come up inside the launch window is a **spawn failure**, not a stuck node | `agent.prompt` (verifies the live foreground process still matches the expected agent and handles the Enter-submit race) | all three signals of §4.3 |
| `Command` | the node's `command` argv verbatim, plus the same `KARVEX_WORKFLOW_*` env | none — the pane is a plain process; confirmation degrades to "process started" | `pane.send_text` on the node's pane; the journal records `delivery: "raw"` | **self-report only** (`kvx workflow node complete`); no hook signal, no detection signal |

`begin_managed_agent` is called **only** when the resolved argv maps to a known
`crate::detect::Agent` variant. A `Command` node is by construction not a
detected agent, so `agent.prompt` would return `agent_not_ready` for it and is
never used on that path.

`runner: Command` is what the Phase 1 e2e fixture uses (`05-phase-plan.md` W7);
it is also the honest binding for any node whose work is a script rather than a
teammate. It is a first-class part of the model, not a test hook.

The `Agent` argv:

```
argv = [
  "claude",
  "--session-id", <uuid assigned by karvex>,
  "--model",  <alias from tier mapping>,       # fable | opus | sonnet
  "--effort", <level from tier mapping>,       # low | medium | high | xhigh | max
  "--name",   <node label>,                    # shows in prompt box + terminal title
  "--append-system-prompt", <contract text>,   # kvdag contract + node role
  "--add-dir", "<node dir>",
  <seed prompt: "Read ./task.md and follow it.">
]

env += {
  KARVEX_WORKFLOW_RUN_ID:    <run id>,
  KARVEX_WORKFLOW_NODE_PATH: <instance path>,
  KARVEX_WORKFLOW_NODE_DIR:  <node dir>,
  KARVEX_WORKFLOW_NODE_TOKEN:<per-node capability token>,
}
```

All of these are documented options of `claude` 2.1.222 and were read directly
off `claude --help`: `--session-id <uuid>`, `--model <alias>` (`fable|opus|sonnet`
or a full model name), `--effort <level>` (`low|medium|high|xhigh|max`),
`--name <name>`, `--append-system-prompt <prompt>`, `--add-dir <dirs...>`,
`--resume <sid>`, `--fork-session`, `--worktree`, `--permission-mode`,
`--mcp-config`, `--agents <json>`.

One **Phase 1 verification item**: `claude --help` mentions an
`--append-system-prompt[-file]` variant inside the `--bare` description but does
not list `--append-system-prompt-file` as its own option. Passing the contract
inline via `--append-system-prompt` is the documented form and is what the argv
above uses; if the `-file` variant turns out to exist, switch to it to keep the
argv short (contracts can be long, and long argv shows up in `ps`). Verify with a
real spawn before relying on it.

Because karvex assigns the session id, `transcript_path` is **derivable before
the process starts** — no waiting on a hook to learn where the transcript is.
After spawn of an `Agent`-runner node the binder calls
`terminal.begin_managed_agent(name, Agent::Claude, …)` so the existing manifest
detector confirms the agent actually came up, exactly as `agent.start` does
today. `Command`-runner nodes skip this entirely (see the table above).

`isolation: Worktree` routes the cwd through the existing `src/worktree.rs` path
rather than duplicating worktree logic.

### 4.3 Completion contract (three signals, strict precedence)

Restating `00-overview.md` D7 in operational terms:

| Precedence | Signal | Mechanism | Recorded as |
|---|---|---|---|
| 1 | **Self-report** | node writes `result.json`, then runs `kvx workflow node complete` (auth: `KARVEX_WORKFLOW_NODE_TOKEN`) → `workflow.node.report` | `evidence: self_report` |
| 2 | **Turn end** | new `stop` action in the bundled Claude hook reports the pane's turn ended; engine then reads `result.json` | `evidence: hook` |
| 3 | **Detection** | `AgentState::Idle` sustained ≥ 3 detector ticks **and** a valid `result.json` exists | `evidence: detection` |

Signals 2 and 3 exist only for `runner: Agent` nodes — they are the Claude hook
and the manifest detector respectively. A `runner: Command` node has signal 1
only; its pane exiting before a valid result is a `Failed`, per the rule below.

In all three cases the result must **validate against `output_schema`** before
the node may succeed. Invalid ⇒ one automatic corrective re-prompt quoting the
validation errors; still invalid ⇒ `NeedsAttention`.

**Idle with no valid result never completes a node.** It transitions to
`NeedsAttention` and wakes the watchdog. This single rule is what makes the whole
system robust to the turn-state edge cases that plague every other integration
path, and it is a direct application of LoopX's materiality principle.

`PaneExited` before a valid result ⇒ `Failed` with the exit code, subject to the
node's retry policy.

### 4.4 Checkpointing

On acceptance the engine writes an immutable `node_checkpoint { kind: "result" }`
with the validated payload, a ≤1,200-char `summary`, and any artifact paths. A
`kind: "partial"` checkpoint is written on each watchdog escalation so a restart
resumes from real progress instead of zero. **The watchdog is Phase 4**, so in
Phase 1 no `partial` checkpoint is ever written — see the restart row in §5.

### 4.5 Non-`Agent` node kinds

`NodeKind` selects *what the node is*; `Runner` selects *how it is bound*. §4.2
covers `Agent`. The other three kinds execute as follows, and each one still
produces a schema-validated `result.json` and records a `Succession` — no kind is
exempt from §3.3 or from the `output_schema` gate.

| Kind | Process | Who writes `result.json` | Evidence recorded | Phase |
|---|---|---|---|---|
| `Internal` | a pane, exactly like `Agent` — the summariser (Feature 3) and the interviewers (Feature 4) are real `claude` sessions with a karvex-authored prompt, visible and steerable. `runner` is `Agent` for these. Nothing runs where the user cannot see it. | the node itself, via `kvx workflow node complete` | `self_report` | 3 / 4 |
| `Gate` | **none.** No pane, no process. The node sits in `NeedsAttention` with a `gate` payload naming the decision and its options. | the engine, from the human's answer | `self_report` (the API call is the report) | 2 |
| `Monitor` | **none in-process.** The engine re-evaluates the node's `condition` on a declared cadence; a poll that changes nothing writes no checkpoint. | the engine, from the evaluated condition | `self_report` | 4 |

- **`Gate` human decision.** `workflow.node.decide { run, path, choice, note? }`
  (Phase 2, added to the method table then, not in Phase 1). The TUI affordance
  is the existing modal shape: with a gate node selected, `Enter` opens a
  `modal_stack_areas` + `centered_button_row` prompt listing the declared
  choices, and the chosen option is written as the node's result. A gate node
  never auto-resolves and never times out silently: until it is decided,
  `run_terminal_ready` (§3.2) refuses to report success because the node is
  still in `NeedsAttention`.
- **`Monitor` cadence and resume condition.** Declared on the node as
  `poll_every_ms` and a `resume_when: Condition` over the monitored node's or
  the run's state. The engine evaluates it on `EngineInput::Tick`; a
  still-unsatisfied condition is the "external wait" class of §6.2 (no streak
  increment, backoff), and it is the conjunct `run_terminal_ready` names when it
  refuses to finish a run. A monitor whose condition becomes true records
  `Succession::Satisfied`; one that exhausts its declared budget records
  `Succession::Blocked` with the unmet condition as `resume_when`.

---

## 5. Steering, interruption, and inspection

| Action | API | Underneath |
|---|---|---|
| Focus a node's teammate | TUI click / Enter | `focus_pane_internal_via_api(ws_idx, pane_id)` — the existing path, not a parallel one |
| Steer mid-run | `workflow.node.steer { run, path, text }` | `runner: Agent` ⇒ `agent.prompt` on the node's pane — the only injection primitive that verifies the live foreground process still matches the expected agent *and* handles the Enter-submit race. `runner: Command` ⇒ `pane.send_text`, journalled with `delivery: "raw"` |
| Interrupt | `workflow.node.interrupt` | `agent.send_keys [Escape]` |
| Read | *(no dedicated method)* | the existing `pane.read` against the `pane_id` returned by `workflow.node.get`: `source: ReadSource::Detection` (cheap, bounded) or `source: Recent` with `format: ReadFormat::Ansi` for full fidelity. `Ansi` is a **format**, not a source. No `workflow.node.read` is added — it would only re-wrap `pane.read` |
| Restart | `workflow.node.restart` | close pane, `attempt += 1`, respawn. **Phase 1:** a fresh pane seeded from `task.md` alone, because no `partial` checkpoint can exist before the Phase 4 watchdog writes them (§4.4). **From Phase 4:** seeded from the node's latest `partial` checkpoint when one exists, falling back to `task.md` when none does |
| Cancel run | `workflow.run.cancel` | cascade: cancel descendants first, then close panes, then mark the run `Cancelled` |

Every steer and interrupt is journalled (`run_event { kind: "steer" | "interrupt" }`)
with the text, so the self-improvement routine can distinguish "the node did well"
from "the human rescued it three times".

**Direct typing into the pane still works and is not intercepted.** The user is
always free to talk to the teammate as a human; karvex simply cannot journal what
it does not mediate, and that is an acceptable, documented gap.

### 5.1 Cross-node messages

Mention-gated, never broadcast. A node receives content only when an inbound edge
fires, a human steers it, or the watchdog nudges it. Delivery uses the same
runner-selected primitive as steering (§5): `agent.prompt` for `runner: Agent`,
`pane.send_text` for `runner: Command`. The payload is the upstream checkpoint's
**summary** (or the full payload on a `payload: full` edge) rendered into a fixed
frame:

```
[karvex · from <source node label>]
<summary>
Continue with ./task.md. Reply only through result.json.
```

Deliveries are journalled as `message_delivered`. If the target is mid-turn the
delivery is **queued and surfaced as pending on the node**, never dropped —
explicitly rejecting the silent-drop dedup behaviour observed in buzz-acp, which
would violate the transparency requirement.

---

## 6. Anti-stuck watchdog (Phase 4; its evidence plumbing lands in Phase 1)

### 6.1 Materiality — what counts as progress

`ProgressTracker` advances **only** on:

- a new tool-call entry appended to the node's transcript JSONL,
- a change in the pane's `ReadSource::Detection` snapshot digest,
- a usage delta (tokens or tool uses),
- a new or modified file under the node's `artifacts/`.

Explicitly **not** progress: the agent produced text; the process is alive; the
screen redrew; an exit code was 0; `result.json` exists but fails schema
validation.

### 6.2 Classification

Every watchdog tick classifies the node into one of four states, mirroring
LoopX's taxonomy — only the last is a bug:

| State | Signal | Action |
|---|---|---|
| Legitimate iteration | progress observed | reset streak |
| External wait | node declared a blocker with `resume_when`, or is a `Monitor` reporting "checked, unchanged" | no streak increment, backoff |
| Goal drift | progress observed but no movement toward the output schema for `drift_threshold` ticks | structured re-prompt naming the missing schema fields |
| Local loop | no material progress for `stuck_threshold` consecutive ticks | escalate (§6.3) |

Streaks are scoped per `(run_node, failure identity)` so one stuck branch neither
starves nor resets healthy siblings.

### 6.3 Escalation ladder

1. **Nudge** — a short steering prompt asking for the current state and the next
   concrete step. Cheap; often enough.
2. **Structured re-prompt** — re-send `task.md` plus the exact unfilled fields of
   `output_schema` and the last partial checkpoint.
3. **Restart** — write a `partial` checkpoint, close the pane, respawn a fresh
   `claude` at `attempt + 1` seeded with the partial checkpoint. Bounded by
   `max_attempts`.
4. **Blocked + surface** — `NeedsAttention` → `Blocked` with a reason, a
   TUI notification, and a badge on the node. The run continues on other
   branches; `run_terminal_ready` will refuse to report success.

Every step emits `run_event { kind: "watchdog" }` and increments
`run_node.watchdog_interventions`, which is direct measured evidence for the
Feature 4 review.

Defaults (config): `stuck_threshold = 3` ticks at a 20 s tick, `drift_threshold =
5`, `max_attempts = 2`. LoopX's own thresholds (2 no-progress turns, 6 stalled
monitor polls) are tuned for multi-day heartbeat cadence and are used as a shape,
not as constants.

### 6.4 Productive-use check

A node `Running` for longer than `idle_budget` with **zero** tool calls and zero
usage delta is treated as stuck regardless of how busy the screen looks — this
catches the "agent is politely waiting for input that will never come" failure,
which screen detection alone reads as `Working`.

---

## 7. Tier → per-node model and effort

Two axes: the run's **tier** (chosen by the user at create and at run time) and
the node's **demand** (declared in the kvdag). One pure function.

```rust
pub fn resolve(tier: Tier, demand: Demand, history: Option<&NodeHistory>) -> Assignment;
pub struct Assignment { pub model: ModelAlias, pub effort: Effort }
```

### 7.1 Model — exactly the spec's mapping

| | **Peak** (most demanding) | **Critical** | **Standard** | **Light** |
|---|---|---|---|---|
| **max** | `fable` | `opus` | `opus` | `opus` |
| **high** | `opus` | `opus` | `opus` | `sonnet` |
| **medium** | `opus` | `opus` | `sonnet` | `sonnet` |
| **low** | `sonnet` | `sonnet` | `sonnet` | `sonnet` |

Reading it back against the spec: **max** = Fable on the most demanding tasks,
Opus everywhere else. **high** = Opus for most tasks, Sonnet elsewhere.
**medium** = Opus for critical tasks only, Sonnet everywhere else. **low** =
Sonnet everywhere.

### 7.2 Effort — pinned at the endpoint tiers, demand-varying in between

| | Peak | Critical | Standard | Light |
|---|---|---|---|---|
| **max** | `max` | `max` | `max` | `max` |
| **high** | `xhigh` | `high` | `high` | `medium` |
| **medium** | `high` | `high` | `medium` | `low` |
| **low** | `low` | `low` | `low` | `low` |

**The endpoint tiers are pinned, deliberately.** The spec defines `max` as
"Highest effort" and `low` as "Lowest cost/effort" — statements about the *tier*,
not about a per-demand curve inside it. So `max` means `max` effort on every
node, and `low` means `low` effort on every node. Anything else would make a
max-tier run give some nodes less than the highest effort, and a low-tier run
give some nodes more than the lowest, which is not what the user asked for when
they picked the endpoint. Per-demand effort variation is exactly what the two
middle tiers are for.

Note that `max` still varies the *model* by demand (`fable` on Peak, `opus`
elsewhere) — that is the spec's own table in §7.1. Pinning applies to effort
only.

Bound at spawn as `claude --model <alias> --effort <level>`; both verified on
2.1.222 (`--effort` accepts `low|medium|high|xhigh|max`; `--model` accepts the
`fable`/`opus`/`sonnet` aliases).

### 7.3 `auto`

A deterministic policy, not a model call, so it is reproducible and auditable:

1. Start from the **high** row.
2. For each node, look up `NodeHistory` for that `node_key` across the workflow's
   last N runs: first-pass success rate, schema-validation failures, watchdog
   interventions, mean tokens.
3. **Downgrade** `Standard` → `sonnet` (effort `high`) when the node's first-pass
   success rate at `sonnet` over ≥ 3 prior runs is ≥ 0.8.
4. **Upgrade** one model step and one effort step when the node's last two runs
   at the current assignment failed on the first pass, or the node has ≥ 2
   watchdog interventions per run on average. Effort steps walk the ordered
   ladder `low < medium < high < xhigh < max`.
5. Never downgrade a `Peak` or `Critical` node below `opus`.
6. Never exceed the **max** row: models are capped by the §7.1 `max` row, and
   effort is capped at `max` (the §7.2 `max` row is pinned to `max` for every
   demand, so this is a single ceiling rather than a per-demand one).
7. Never go below the **low** row either: `sonnet` at effort `low` is the floor,
   so `auto` can never be cheaper than explicitly choosing `low`.

The resolved `(model, effort)` and the reason (`policy: "auto/downgrade-standard"`)
are written into `run_node` so any run can be explained after the fact and
replayed identically.

### 7.4 Growth-limit influence

The tier **narrows** the version's growth limits; it never widens them (§3.4):
`max`/`high` ⇒ the version's `max_nodes` unchanged; `medium` ⇒
`min(version.max_nodes, 24)`; `low` ⇒ `min(version.max_nodes, 12)`. Rationale:
the spec says the tier may influence growth defaults, and a cheaper tier that
fans out wider is the worst of both worlds — while letting a run exceed its own
version's declared ceiling would break the
`workflow_run.max_nodes <= kvdag_version.max_nodes` invariant the store asserts.
Widening is an authoring change on the version, which by construction produces a
new `kvdag_version`.

---

## 8. Live DAG view input

`src/workflow/layout.rs` is pure: `layout(&RunGraph, area) -> DagLayout`.

- **Layer assignment** by longest path from roots (topological), templates
  excluded until instantiated.
- **Ordering within a layer** by a barycenter/median heuristic to reduce
  crossings, with a stability tiebreak on `instance_path` so nodes do not jump
  around as the graph grows.
- **Coordinates** at terminal-cell resolution; node boxes are small and
  fixed-height (label + status glyph + one-line status), similar density to the
  existing agent panel entries. The selected node's full detail goes in a
  detail strip, not in a bigger box.
- **Edges** are orthogonal (drop, jog, drop) routes accumulated as direction bits
  per cell, then stringified with the same `line_cell_symbol` logic
  `src/ui/panes.rs` uses today, so crossings and merges render as correct
  box-drawing glyphs. Arrowheads are a single `▾`/`▸` before the target box.

  **Prerequisite, not a drop-in import.** `struct LineCell` (`src/ui/panes.rs:438`)
  and `fn line_cell_symbol` (`src/ui/panes.rs:667`) are both module-private
  today, and `mod panes;` is itself private in `src/ui.rs`. Reuse requires a
  small preparatory move: lift both into a `pub(crate)` home (a new
  `src/ui/line_cells.rs`) that `src/ui/panes.rs` and the DAG renderer both
  import. `src/workflow/layout.rs` stays pure and `src/ui`-free — it emits a
  workflow-local `EdgeBits { up, down, left, right }`, and the renderer converts
  `EdgeBits → LineCell` at draw time. This keeps the layering of §1 intact.
- Direction-bit accumulation happens in `compute_view_internal` (the mutation
  pass); only glyph stringification happens at draw time (pure).
- Re-layout from scratch on every graph change is fine to the low hundreds of
  nodes. No incremental layout in Phase 1–2.

Output shape stored in `ViewState`: `Vec<(RunNodeIdx, Rect)>` plus the edge cell
map. **Hit-testing reads exactly this**, so the clickable rects can never
disagree with what was drawn — the property the Navigator gets by sharing one
data→lines function, achieved here via shared stored geometry because layout is
too expensive to run twice per input event.

Status → palette slots (semantic, never ad hoc colours):
`Running` → `yellow`, `NeedsAttention`/`Blocked` → `red`, `Succeeded` → `green`,
`Pending`/`Ready` → `subtext0`, `Skipped` → `overlay0`, `Restored` → `teal`,
selection highlight via `panel_contrast_fg`.

---

## 9. Concurrency and failure model

- The engine runs inside the server's existing single-threaded `App` event loop.
  All `EngineInput`s arrive as `AppEvent`/`ApiRequestMessage` variants, so there
  is no lock, no shared-mutable graph, and no second scheduler.
- Store writes are issued from a dedicated task and are **not** on the critical
  path of a node transition: the in-memory `RunGraph` is authoritative during a
  run; the journal is the durable record. A store write failure degrades the run
  to `persistence_degraded` (surfaced) rather than killing it.
- Server restart mid-run: panes survive (karvex's existing handoff/restore path),
  but the engine does not auto-resume. On restart, an interrupted run is loaded
  from the journal into `Paused`, its still-live panes re-bound by
  `agent_session_id`, and the user is offered resume or checkpoint-restore into a
  new run. Auto-resume is deliberately out of scope — silently reattaching to
  agents that may have drifted is worse than asking.
- Every failure path has a node status; there is no path where a node
  disappears. `SuccessionGap` exists precisely to make "it vanished" a loud,
  testable state.
