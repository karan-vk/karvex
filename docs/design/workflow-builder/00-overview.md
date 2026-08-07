# Workflow Builder — Design Overview

Status: **design locked for Phase 1** (Phase 0 deliverable).
Scope: the transparent, steerable, DAG-based workflow builder described in
`herdr-workflow-builder-prompt.md`.

This document is the entry point. It states the goals, the architecture, the
decisions and their rationale, and maps the design onto the spec's Features 1–5.
Companion documents:

| Doc | Subject |
|---|---|
| `01-acp-evaluation.md` | Final call on `@agentclientprotocol/claude-agent-acp` |
| `02-mjs-workflow-evaluation.md` | Final call on Claude Code's `.mjs` workflow mechanism |
| `03-storage-schema.md` | Embedded SurrealDB schema (definitions, versions, runs, checkpoints, summaries) |
| `04-kvdag-and-execution.md` | kvdag data model, scheduler, execution binding, steering, watchdogs, tiers |
| `05-phase-plan.md` | Phase 1 implementation plan for this codebase, Phases 2–4 in outline |

---

## 0. Correction to the spec's assumptions

The spec (assumption 1) says *"`kvx panes` and `kvdags` already exist in this
fork; you should extend them, not reinvent them."*

**Half of that is wrong and the design depends on saying so plainly.**

- **kvx panes exist.** `Workspace`/`Tab`/`PaneId` → `TerminalId` → `TerminalState`
  (pure data) + `TerminalRuntime`/`PaneRuntime` (PTY handles) are real, mature,
  and this design builds directly on them. Agent detection
  (`src/detect/manifests/*.toml`), `agent.prompt`, `agent.send_keys`,
  `pane.read`, `pane.split`, `events.subscribe` are all reused as-is.
- **kvdags do not exist.** A repo-wide search for `kvdag` returns hits only in
  `herdr-workflow-builder-prompt.md` itself — zero occurrences anywhere in
  `src/`, `tests/`, or `docs/`. There is no graph primitive in this codebase.
  `src/layout.rs`'s `TileLayout`/`Node` is a **binary split tree for pane
  tiling**, not a general DAG, and is not reusable as one.

**Therefore this design introduces kvdag as a new, server-owned primitive.**
Nothing is being "extended"; the graph model, its storage, its scheduler, and
its projection into the TUI are all new. The rest of the spec is unaffected —
kvdags still are the compilation target for workflows, exactly as the spec
intends; they simply have to be built first.

Definition adopted for the rest of these documents:

> A **kvdag** is an immutable, versioned, declarative directed acyclic graph of
> work nodes and typed edges, owned by the karvex server, stored in embedded
> SurrealDB, and interpreted by a Rust scheduler. A **run** materialises a
> kvdag version into a *run graph* whose nodes bind to kvx panes and which may
> grow beyond the definition through bounded, guardrailed dynamic expansion.

---

## 1. Goals

1. **Transparency by construction.** Every unit of agent work a workflow
   performs is a real `claude` process in a real kvx pane the user can watch,
   scroll, and type into. Nothing runs where the user cannot see it, except
   explicitly-labelled internal utility nodes (summariser, interviewer), which
   are still visible as nodes with their full output stored.
2. **Steerability at any node, at any time.** Selecting a node focuses its
   pane. Sending a steering message to a running node is a first-class,
   server-side operation, not a UI trick.
3. **The graph is the artifact.** Workflow definitions are data, not code:
   inspectable before running, diffable per node, immutably versioned, and
   renderable without executing anything.
4. **Runs are fully reconstructible.** An append-only per-run event journal plus
   per-node checkpoints make the DAG view a *projection*, run history browsable,
   and both restore modes (interrogation, checkpoint) natural reads rather than
   bolt-ons.
5. **Token efficiency without quality loss.** Nodes exchange schema-validated
   structured summaries, never raw transcripts. Cross-node messages are
   mention-gated (a node receives context only when an edge fires or a human
   steers it). Quality is protected by the tier system and by refusing to accept
   weak completion evidence.
6. **Fits karvex.** Server owns runtime facts and exposes them over the JSON API
   and event bus; the TUI is one client. `render()` stays pure. No `unwrap()` in
   production code. `tracing` for logs. Platform code stays in `src/platform/`.

## 2. Non-goals

- **Not a general workflow engine.** No cron, no external triggers, no
  multi-tenant scheduling, no distributed execution. One user, one machine.
- **Not a scripting runtime.** No embedded JavaScript engine, no Turing-complete
  user-supplied control flow inside karvex. See `02-mjs-workflow-evaluation.md`.
- **Not an ACP client (as the primary path).** See `01-acp-evaluation.md`.
- **Not a replacement for Claude Code's own subagents/teams *inside* a node.** A
  node's `claude` session may freely use `Task`, subagents, or Claude's own team
  features for work internal to that node. karvex just does not delegate
  *workflow-level* orchestration to it.
- **Not a Claude-transcript renderer.** The pane already renders Claude's real
  TUI. karvex will not reimplement it.
- **Phase 1 does not include** dynamic growth, tiers, history browsing, restore,
  self-improvement, or watchdogs. Those are Phases 2–4. Phase 1 ships a static
  kvdag executed end-to-end with a live DAG view.

---

## 3. Architecture

```
                      ┌───────────────────────────────────────────────┐
                      │  karvex server process (one App, src/app)     │
                      │                                               │
  kvx TUI client ◀────┤  AppState (pure data)                         │
  (binary render      │    ├── workspaces / tabs / panes / terminals  │
   protocol, frames)  │    ├── workflow: WorkflowRuntimeState  ← NEW  │
                      │    └── view / mode / selection (presentation) │
                      │                                               │
                      │  TerminalRuntimeRegistry (PTYs)               │
                      │                                               │
                      │  ┌─────────────────────────────────────────┐  │
  CLI / plugins  ◀────┤  │ workflow engine (src/workflow)   ← NEW  │  │
  (JSON API,          │  │   scheduler · binder · watchdog · tiers │  │
   events.subscribe)  │  └───────────────┬─────────────────────────┘  │
                      │                  │                            │
                      │  ┌───────────────▼─────────────────────────┐  │
                      │  │ workflow store (embedded SurrealDB)     │  │
                      │  │   kvdag versions · runs · checkpoints   │  │
                      │  └─────────────────────────────────────────┘  │
                      └───────────────────────────────────────────────┘
                                         │ spawns / prompts / reads
                                         ▼
                    kvx panes, each running one interactive `claude`
```

Four layers, with hard boundaries:

### 3.1 Store (`src/workflow/store/`)
Embedded SurrealDB (`SurrealKv`), owning workflow definitions, immutable kvdag
versions, run records, the per-run event journal, per-node checkpoints, run
summaries, and review cycles. Pure async I/O over typed records; knows nothing
about panes, PTYs, or the TUI. Fully testable against the in-memory `Mem`
engine. Schema in `03-storage-schema.md`.

### 3.2 Engine (`src/workflow/engine/`)
The kvdag scheduler. Pure state machine over an in-memory `RunGraph` plus a
narrow `RunEffect` output channel. Computes the ready set, applies node results,
evaluates edge conditions, admits or rejects expansion proposals against growth
guardrails, and drives the anti-stuck watchdog. **Contains no PTY, no I/O, no
`App` reference** — the same discipline as `AppState`: it is advanced by
`apply(event) -> Vec<RunEffect>` and is unit-testable without a terminal.
Semantics in `04-kvdag-and-execution.md`.

### 3.3 Binder (`src/workflow/binding/`)
The only part that touches the runtime. Translates `RunEffect`s into calls on
`App`: spawn a pane running `claude` with the node's argv/env, inject a steering
prompt via the verified `agent.prompt` path, send an interrupt, read the pane,
close a pane. Translates runtime facts back into engine events: agent status
transitions, node self-reports, hook signals, pane death.

### 3.4 API + TUI
`workflow.*` JSON-API methods and `workflow.*` events (server-owned runtime
facts) plus `Mode::WorkflowDag` in the TUI (presentation only: layout, node
rects, selection, scroll). The DAG view is a **read-only projection** of engine
state; clicking a node routes through the existing
`focus_pane_internal_via_api` path, never through a parallel focus mechanism.

### 3.5 What lives where (runtime/client boundary guardrail)

| Fact | Owner | Exposure |
|---|---|---|
| Workflow definitions, kvdag versions | server / store | `workflow.*` JSON API |
| Run status, node status, node→pane binding, usage, checkpoints | server / engine | JSON API + `workflow.*` events |
| Growth guardrail limits and breaches | server / engine | JSON API + event |
| Steering, interrupt, restart, cancel | server (composes `agent.prompt`/`agent.send_keys`) | JSON API |
| DAG layout geometry, node rects, edge glyph cells | TUI presentation | `AppState.view` only |
| Selected node, hover, scroll/pan offset, collapsed subgraph | TUI presentation | `AppState` only, never in the API |

Neutral naming is enforced in the API surface: `workflow`, `kvdag_version`,
`node`, `edge`, `run`, `run_node`, `checkpoint`, `usage`, `status`. No
`sidebar`, `card`, `widget`, `row`, or `panel` in any API identifier.

---

## 4. Key decisions

Each decision states the call and why. Alternatives that were rejected are named
so the rationale survives.

> **Addendum — 2026-08-07:** owner directive supersedes D4 below. Karvex ships
> one slim binary per platform with the workflow subsystem always included,
> rather than a SurrealDB-bearing variant next to a lean default. The store is
> reimplemented on `redb` (pure Rust, ~1-2 MiB) behind the same `WorkflowStore`
> public API; the `workflow` cargo feature is removed. D4's SurrealDB
> evaluation and cost accounting are kept below for historical record — see
> `03-storage-schema.md` for the matching note on the schema doc.

### D1 — karvex owns the spawner; Claude Code Agent Teams is not the orchestration engine
**Call:** every kvdag node is its own kvx pane running its own interactive
`claude` session, spawned by karvex. Claude's native Agent Teams feature is not
used to orchestrate the workflow.

**Rationale (three independently sufficient reasons, all measured in Phase 0):**
1. **Teams cannot nest.** Anthropic's docs are explicit: *"teammates cannot
   spawn their own teammates. Only the lead can manage the team."* Spec Feature 1
   requires exactly that. Under karvex-owned spawning it falls out for free.
2. **Teams cannot render into kvx panes.** `teammateMode` accepts only
   `auto | tmux | iterm2 | in-process`; split-pane display is documented as
   unsupported outside tmux/iTerm2, and `strings` on the 2.1.222 binary confirms
   hard-coded `createTeammatePaneWithLeader` / `[ITermBackend]` backends. There
   is no third-party multiplexer extension point, and karvex is one. Delegating
   spawning to Claude's lead would put every teammate inside the *lead's single
   pane's* agent panel — the opposite of the transparency requirement.
3. **Teams have no programmatic surface.** `claude agents --json` explicitly
   excludes teammates and subagents; no headless/SDK path forms a team at all
   (verified: no team dir under ACP or under `claude -p`, while a concurrent
   interactive session did create one). The only external observability is
   file-watching `~/.claude/teams/**` and `~/.claude/tasks/**`, which are
   documented as "don't hand-edit, overwritten on next state update."

**What is kept from Agent Teams:** the *shape* — a lead coordinating named
teammates with per-teammate roles, dependency-gated task claiming, and direct
messaging — reimplemented as karvex-owned concepts (`run_node` roles, typed
edges, engine-mediated messages). And a node's own session may still use
`Task`/subagents/teams **internally**; karvex simply does not read them as
workflow structure.

### D2 — ACP: ADOPT-PARTIALLY, narrowed; not on the Phase 1–3 path
**Call:** adopt ACP's *data model* as karvex's internal vocabulary immediately
(free). Do not adopt ACP as the integration layer. Defer the optional
feature-gated headless executor to Phase 4 at the earliest. Full reasoning and
the re-evaluation triggers in `01-acp-evaluation.md`.

### D3 — Workflow definitions are declarative data, not `.mjs` scripts
**Call:** hybrid, declarative-primary. kvdag = nodes + typed edges as data;
dynamic behaviour comes from two constrained primitives (typed edge conditions,
and a bounded `expand` proposal) rather than general scripting. No JS engine is
embedded. Full reasoning in `02-mjs-workflow-evaluation.md`.

**Carried over from the `.mjs` mechanism verbatim:** forced JSON-Schema
structured output per node; background execution with completion notification
and a live progress view; and an append-only run journal. Two distinct keys,
which the `.mjs` mechanism conflates into one content hash and this design
deliberately separates:

- the **journal** (`run_event`) is keyed `(run, seq)` — a monotonic per-run
  sequence, with an optional `run_node` reference on each entry, so a run can be
  replayed exactly;
- the **checkpoint index** (`node_checkpoint`) is keyed
  `(kvdag_version, node_key, kind)` plus `(run_node, seq)` — *topological*, which
  is what makes restore "resume from node X" rather than "replay the first N
  calls of a script".

The `.mjs` prefix hash can only answer "has this exact call run before?"; the
topological checkpoint key answers "which nodes have a checkpointed result at
this kvdag version?", which is what spec Feature 3 actually needs.

### D4 — Embedded SurrealDB with `SurrealKv`, and the dependency cost is accepted explicitly
**Call:** `surrealdb = { version = "3", default-features = false, features = ["kv-surrealkv", "kv-mem"] }` plus
`surrealdb-types = "3"`; all DB-facing structs derive
`surrealdb_types::SurrealValue`, **not** serde (the serde path does not compile
against 3.2.4 — this is a live docs/API drift trap that was hit empirically).

**Cost, stated plainly because it must be signed off rather than discovered:**
~+257 net-new crates (≈2.76× the current crate count), +2.5–4 min clean build,
double-digit-MB binary growth, and a mandatory `cc` + `cmake` toolchain on every
release target (transitively via `aws-lc-sys` ← `jsonwebtoken`, which is *not*
feature-gated in `surrealdb-core`).

**Mitigation:** the store and engine live behind a default-on cargo feature
`workflow`. API schema types are **always** compiled (they are plain
serde/schemars structs with no SurrealDB dependency) so
`docs/next/api/herdr-api.schema.json` has exactly one canonical value; with the
feature off, the handlers return a structured `workflow_unavailable` error. This
bounds the blast radius and keeps a working fallback build if a release target's
C toolchain regresses.

**Rejected:** `kv-rocksdb` (C++ toolchain on every cross-compile target for no
benefit here); relying on SurrealKV's `.versioned()` storage-level record
versioning for kvdag versions (engine-specific, non-portable, and unqueryable in
the way the spec's "browse every revision" requirement needs).

### D5 — Nodes are spawned by direct argv, not by typing into a shell
**Call:** the binder calls the existing `Workspace::split_pane_argv_command`
in-process to launch `claude … <seed prompt>` directly. No shell wrapper, no
keystroke race, and env (`KARVEX_WORKFLOW_*`) is injected deterministically at
spawn. The function is already an in-process caller's API today, used by both the
plugin pane-open path (`src/app/api/plugins/panes.rs`) and the built-in
scrollback-editor launch path (`src/app/input/navigate.rs`, via
`spawn_overlay_argv_command`) — so this is a third in-process caller, not a new
kind of use.

**A node states how it is bound.** `KvdagNode.runner` is `agent` (a `claude`
teammate: managed-agent confirmation on spawn, `agent.prompt` for steering, all
three completion signals) or `command` (a plain process from the node's own
`command` argv: no agent detection, `pane.send_text` for delivery, self-report
completion only). This is declared in the definition, never inferred from
whether we happen to be running a test. Full contract in
`04-kvdag-and-execution.md` §4.2.

**Consequence:** no new `argv` field on the public `pane.split` schema, so **no
`PROTOCOL_VERSION` bump is required** for Phase 1. Adding `argv` to the public
`pane.split` would also widen the remote-command surface for every API caller —
not worth it when the engine runs in-process.

**Rejected:** `agent.start` (types `claude …\n` into an idle shell). It works,
but it races the shell, cannot inject per-node env, and offers no way to pass
`--session-id`.

### D6 — Node identity is pre-assigned; the Claude session id is chosen by karvex
**Call:** karvex generates the node run's UUID and passes it as
`claude --session-id <uuid>`. This makes the transcript path deterministic and
known *before* the process starts, so run history, interrogation restore, and
transcript-derived progress evidence do not depend on catching a hook callback.

`--resume <uuid> --fork-session` is then the exact mechanism for interrogation
restore, and it is non-destructive: the original run's transcript is never
mutated. Both flags verified present on `claude` 2.1.222.

### D7 — Completion is evidence-based and never inferred from "the agent went idle"
**Call:** a node completes only when a **result artifact** exists and validates
against the node's declared output schema. Three signals, in precedence order:

1. **Self-report (authoritative).** The node writes `result.json` in its node
   directory and runs `kvx workflow node complete` (authenticated by
   `KARVEX_WORKFLOW_NODE_TOKEN` in its env). Instant, unambiguous.
2. **Turn-end hook.** A new `stop` action in the bundled Claude integration hook
   reports "the main turn ended" for the pane; the engine then checks for the
   artifact.
3. **Detection fallback.** `AgentState::Idle` sustained across N detector ticks
   *and* a valid artifact present → accept, tagged `evidence: detection`.

Idle **without** a valid artifact never completes a node. It transitions the
node to `NeedsAttention` and wakes the watchdog. This is the LoopX materiality
principle: "agent replied", "process idle", "exit code 0", "file exists" are not
progress. It also sidesteps every accepted-residual turn-state bug documented in
the ACP adapter (#825, #864, #866, #773).

### D8 — Cross-node communication is mention-gated and summary-shaped
**Call:** a node's context is assembled at spawn time from the *summary* fields
of its upstream nodes' checkpoints, never from raw transcripts, and never
broadcast. A running node receives new content only when (a) an inbound edge
fires, (b) a human steers it, or (c) the watchdog nudges it. Every delivery is
recorded in the run journal — nothing is silently dropped (explicitly rejecting
the buzz-acp default dedup behaviour, which drops in-flight messages).

An edge may declare `payload: full` to pass a complete checkpoint when a
downstream node genuinely needs it; that is an explicit, visible, per-edge
decision rather than a default.

### D9 — The DAG view is a projection, and layout is computed once per frame
**Call:** node/edge/status data flows server→TUI through engine state and
`workflow.*` events; the TUI never holds a second mutable graph. Layered layout
runs inside `compute_view_internal` (the one place allowed to mutate `AppState`)
and stores node rects + pre-accumulated edge direction bits into `ViewState`;
`render_workflow_dag()` and the mouse hit-test both read that same stored
geometry, so they can never disagree, and layout never runs twice per frame.

Edges reuse the existing `LineCell{up,down,left,right}` bitmask +
`line_cell_symbol` technique from `src/ui/panes.rs` so crossings join as
`┼`/`┬`/`┤` correctly. Both are module-private there today, so reuse begins with
a small preparatory move into a `pub(crate)` home shared by `panes.rs` and the
DAG renderer (`04-kvdag-and-execution.md` §8). `ratatui::widgets::canvas::Canvas`
is rejected: nothing in the repo uses it, it is Braille-subcell oriented, and its
coordinates are not cell-addressable in the way hit-testing requires.

### D10 — One workflow store per user, opened lazily, with an explicit lock story
**Call:** the store lives at `crate::config::state_dir().join("workflow")` — the
existing user-level state helper, which already bakes in `app_dir_name()`, so no
extra `karvex` path segment is joined and no `$HOME` is hand-rolled. Explicitly
**not** `crate::session::data_dir()`, which is the *per-session* directory and
would silently break the reusable-across-sessions requirement.

The store is opened **lazily, on the first `workflow.*` call**, not at server
startup, so a karvex that never touches workflows never pays the open cost or the
lock. The SurrealKv lock is therefore acquired at that first use; if another
karvex server already holds it, the subsystem transitions to
`Unavailable { reason: "store_locked", holder }` at that point, every `workflow.*`
method returns that structured error, and the TUI surfaces it once rather than
failing silently. `KARVEX_WORKFLOW_DB_PATH` overrides the location for users who
genuinely want per-session isolation.

Note for manual validation: `app_dir_name()` returns `karvex-dev` under
`debug_assertions`, so `cargo run` and a release install use **separate**
databases. That is the right behaviour, but it means a workflow created in a
debug build will not appear in the installed binary.

**Rejected:** per-session stores (fragments the reusable-definition requirement);
silent fallback to an in-memory store (would look like data loss).

### D11 — Tier assignment is a pure function of (tier, node demand), recorded per run
**Call:** every kvdag node declares a `demand` (`peak | critical | standard | light`).
The tier chosen at run time maps `(tier, demand) → (model, effort)` through a
single pure function, exactly per the spec's table. `auto` is a deterministic
policy function over run history, and the resolved model/effort is written into
each `run_node` record so a run is always auditable and reproducible. Full table
in `04-kvdag-and-execution.md` §7.

### D12 — Growth is a proposal, not an action
**Call:** a node cannot create nodes. It *proposes* expansion (structured output
field, or a `kvx workflow node expand` call); the engine validates the proposal
against `max_depth`, `max_nodes`, and template allowlists, then commits or
rejects it. Rejections emit `workflow.growth.limited` and are shown in the DAG
view attached to the proposing node — never silently truncated. This is the
LoopX Proposal → Effect → Commit boundary, and it is what makes
"teammates spawn teammates" safe.

### D13 — Every closing node must resolve its succession
**Call:** when a node reaches a terminal state it must resolve to exactly one of:
(a) satisfy its outbound edges with a validated result, (b) record an explicit
blocker with a resume condition, or (c) record explicit `no_followup` terminal
evidence. A node that ends with none of these is an engine-level error
(`SuccessionGap`), surfaced in the TUI. This prevents the classic failure where
a branch quietly evaporates and the run reports success.

---

## 5. Mapping to the spec's Features

### Feature 1 — Live DAG view (in-TUI)
- New `Mode::WorkflowDag` full-bleed overlay; layered layout; node boxes as
  `Block` + `Paragraph` at precomputed rects; edges via the `LineCell` bitmask
  router; selected-node detail strip (Navigator's `render_detail` pattern).
- Click / Enter on a node → `focus_pane_internal_via_api(ws_idx, pane_id)`.
  Mouse routing gets its own arm in `App::handle_overlay_mouse`.
- Steering: with a node selected, a steer input line composes
  `workflow.node.steer` → `agent.prompt` on that node's pane. Interrupt →
  `agent.send_keys [Escape]`.
- Dynamic growth: engine-side expansion (D12) emits `workflow.node.spawned`;
  the view re-runs layout on the next `compute_view` and the node appears live.
- Guardrails: `max_depth` / `max_nodes` from run config, breaches rendered as a
  badge on the proposing node plus a run-level banner. (Phase 2.)

### Feature 2 — Model & effort tiers
- `workflow.run` requires a `tier` (`auto|max|high|medium|low`); the TUI prompts
  with a small modal reusing `modal_stack_areas` + `centered_button_row`, and
  the same prompt appears at workflow *create* time to set the default.
- `(tier, demand) → (model, effort)` per D11 / §7 of `04`. The model table is the
  spec's table verbatim. Effort follows the spec's tier adjectives: the endpoint
  tiers are **pinned** — `max` ⇒ effort `max` on every node ("Highest effort"),
  `low` ⇒ effort `low` on every node ("Lowest cost/effort") — and only `high`,
  `medium`, and `auto` vary effort by the node's demand. Bound to the pane at
  spawn via `claude --model <alias> --effort <level>`; both flags verified on
  `claude` 2.1.222 (`--effort` accepts `low|medium|high|xhigh|max`; `--model`
  accepts `fable|opus|sonnet` aliases). (Phase 2.)

### Feature 3 — Run history, summaries, restore
- Every run writes an append-only `run_event` journal and per-node
  `node_checkpoint` records (D3, `03-storage-schema.md`).
- End-of-run summariser is itself a node (internal role) producing a
  schema-validated, token-budgeted `run_summary` using a fixed
  what-to-cover template (adapted from buzz's context-handoff compaction).
- Run browser: a list/detail overlay reusing the Navigator shape, backed by
  `workflow.run.list` / `workflow.run.get`.
- **Interrogation restore:** `workflow.node.interrogate` spawns a pane running
  `claude --resume <node session uuid> --fork-session` in the recorded cwd; the
  forked session appears in the DAG as a detached interrogation node bound to
  the original `run_node`, and the original transcript is never mutated (D6).
  **Claude owns the transcript file, so it can disappear** (Claude-side cleanup
  or compaction, a `~/.claude` reset, a different machine) — and run history is
  retained for 50 runs, which can outlive Claude's session retention. So the
  engine **verifies `run_node.transcript_path` exists before spawning the fork**.
  If it does not, `workflow.node.interrogate` returns a structured
  `transcript_unavailable` error that the TUI surfaces, and offers the degraded
  path instead: a fresh node seeded with the stored `node_checkpoint`
  payload/summary plus the node's `task.md`, labelled **reconstructed**, not
  resumed. A reconstructed node is never presented as the original teammate.
- **Checkpoint restore:** `workflow.run` accepts
  `restore_from: { run_id, nodes: [selector] }`. Selected nodes are created in
  the new run with `status: Restored` and a `checkpoint_ref` to the source run's
  checkpoint; no pane is spawned; downstream nodes consume their outputs
  normally. Addressable because node identity is topological, not positional.
- Prior-run summaries are injected into a new run's context by default and can
  be excluded per run (spec assumption 3). (Phase 3.)

### Feature 4 — Self-improvement routine
- After a run finishes the TUI asks (never automatic — spec assumption 2).
- On accept, a review cycle runs as its own small kvdag: one interviewer node
  per teammate (1:1), then a synthesis node that classifies each finding as
  **prompt-level** or **structural**.
- **The 1:1 is a real two-party interview, not a one-sided post-mortem.** The
  interviewee has to be in the room, so each interviewer node **revives its
  target** by spawning `claude --resume <run_node.agent_session_id>
  --fork-session` — the same non-destructive fork the interrogation-restore path
  uses (D6), so the original transcript stays byte-identical and remains valid
  evidence. The interviewer asks a fixed question set (what were you asked to do;
  what did you actually do; what blocked you; what did you need from upstream and
  not get; what would you change about your own brief), and the full exchange is
  recorded as an `interrogation` row referenced by the resulting
  `review_finding`s.
- **Measured data is the interviewer's evidence, not its only input.** Attempts,
  watchdog interventions, token/tool/duration usage, schema-validation failures,
  and downstream rework are put *to* the teammate during the interview and
  recorded alongside its answers, so a finding is grounded in numbers rather than
  vibes — and the teammate gets to explain them.
- **Fallback when the session cannot be resumed.** If the target's transcript is
  gone (same failure mode as Feature 3's interrogation restore), the interviewer
  runs an **evidence-only** interview over the journal, checkpoints, and usage
  data, and every finding it produces is flagged `interview: evidence_only` so a
  reader can tell a teammate's own account from an inference about it. This is a
  degraded mode, never the default.
- Underperformance → an explicit fire-and-replace proposal that must include a
  concrete replacement role definition in the same finding.
- Accepted findings are compiled into a **new immutable `kvdag_version`** whose
  `parent_version` is the run's version and whose `change_summary` lists the
  per-node diff. The old version is never mutated. (Phase 4.)

### Feature 5 — Reliability / anti-stuck
- Per-node no-progress streak with a **materiality** test: progress means new
  tool calls in the transcript, a detection-text delta, or a usage delta — never
  "the agent produced text".
- Escalation ladder: nudge → structured re-prompt → restart the node in a fresh
  pane from its last checkpoint → mark `Blocked` and surface in the TUI. Every
  step is journalled.
- Streaks are scoped per `(run_node, failure identity)` so one stuck branch
  neither starves nor resets healthy siblings.
- Productivity check: a node that has been running with zero tool calls and zero
  usage delta beyond a threshold is treated as stuck even if the screen looks
  busy. (Phase 4; the detection/journal plumbing it needs lands in Phase 1.)

---

## 6. Risks and how the design absorbs them

| Risk | Absorption |
|---|---|
| SurrealDB dependency blast radius (+257 crates, cc/cmake) | Default-on `workflow` cargo feature; schema types always compiled; `kv-mem` for fast tests (D4) |
| SurrealKv is labelled beta by SurrealDB | Store trait is narrow and typed; all access goes through `src/workflow/store`, so an engine swap is a single-module change |
| Claude Code is experimental and moves fast (behaviour shifted materially across 2.1.178→2.1.222) | Depend only on stable CLI flags (`--session-id`, `--resume`, `--fork-session`, `--model`, `--effort`) and karvex's own hook/detection channel; never parse `~/.claude/teams/**` |
| Screen detection is fragile as a completion signal | Detection is only the *third* completion signal and is never sufficient alone (D7) |
| Dynamic growth runaway (cost, pane explosion) | Proposal → guardrail → commit with max depth/nodes, surfaced not silent (D12) |
| A branch silently disappearing | Mandatory succession resolution (D13) |
| Layout cost as the graph grows | Layout runs once per frame in `compute_view_internal`, from-scratch is fine to the low hundreds of nodes; no premature incremental layout |
| A second async runtime | Avoided in Phases 1–3 by not adopting `agent-client-protocol` (D2) |

---

## 7. Conventions this subsystem must follow

- No `unwrap()` / `expect()` in production paths. Store and engine return typed
  errors; the binder degrades to a surfaced node state rather than panicking.
- `tracing` spans per run and per node run (`workflow.run`, `workflow.node`),
  with the run id and node key as fields.
- `AppState`-style purity: `WorkflowRuntimeState` is data; `RunGraph` transitions
  are pure functions returning effects; every engine behaviour is unit-testable
  without a PTY.
- New DB-facing structs derive `surrealdb_types::SurrealValue`; new API structs
  derive `Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema`.
- Any change to `Method` / `ResponseResult` / `EventKind` requires regenerating
  `docs/next/api/herdr-api.schema.json` in the same change.
- `PROTOCOL_VERSION` is **not** bumped for this work (see D5 and
  `05-phase-plan.md` §6).
