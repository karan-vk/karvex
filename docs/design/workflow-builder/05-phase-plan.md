# Implementation plan

Phase 1 in detail against this codebase; Phases 2–4 in outline.

Phase 1's definition of done: **a saved kvdag definition runs end to end, each
node visibly executing as a `claude` teammate in its own kvx pane, with a live
in-TUI DAG view whose nodes are clickable and steerable, and the whole run
persisted in SurrealDB.** No dynamic growth, no tiers, no history browser, no
restore, no self-improvement, no watchdog — those are Phases 2–4.

---

## 1. Phase 1 workstreams

Seven workstreams, sized so that W1/W2/W3 start in parallel immediately, and
W4/W5/W6 join as their inputs land.

**Shared-file rule.** Module boundaries are drawn so two agents never edit the
same file *during a parallel step* — but two files are genuinely shared, because
every submodule has to be declared somewhere:

- `src/workflow/mod.rs` must carry a `mod` line for work delivered by W1
  (`store`), W2 (`model`, `tier`, `engine`), W4 (`binding`) and W6 (`layout`).
- `src/workflow/binding/mod.rs` must carry `mod spawn;` (3b) and `mod observe;`
  (3c), which are scheduled in parallel.

Both files are therefore **landed complete and empty first** — all `mod` lines
present, pointing at stub files — in step 1a and step 3a respectively, before any
parallel step that touches their submodules starts. Nobody edits them again.

```
W1 store  ──┐
W2 engine ──┼──▶ W4 binding ──┐
W3 api    ──┴──▶ W5 cli       ├──▶ W7 e2e
                 W6 tui       ─┘
```

### W1 — Dependency + store foundation
**Files:** `Cargo.toml`, `Cargo.lock`, `justfile`, `.github/workflows/ci.yml`,
`src/workflow/mod.rs` (new — landed complete, with a `mod` line and an empty stub
for every submodule any workstream will deliver), `src/main.rs` (register
`mod workflow;`), then `src/workflow/store/mod.rs`, `store/records.rs`,
`store/queries.rs`, `store/error.rs`, `store/migrations/0001_init.surql`.

The module skeleton and the build plumbing land in **step 1a**, before any
parallel step, so that steps 1b onward drop files into a crate that already
compiles and references them. The store itself is step 2a.

**Delivers:** the cargo feature `workflow` (default on); `surrealdb` +
`surrealdb-types` pinned per `03-storage-schema.md` §1; a `WorkflowStore` that
opens `SurrealKv` **lazily on first use** at `crate::config::state_dir().join("workflow")`
(or `Mem` for tests — and note `state_dir()` already bakes in `app_dir_name()`, so
no extra `karvex` segment is joined, and debug builds resolve to `karvex-dev`),
applies migrations
transactionally, and exposes typed create/read methods for every table in §4 of
that doc. **No update/delete methods for the append-only tables** — immutability
by construction. Store-locked path returns `StoreError::Unavailable { reason, holder }`.

**Tested:** `src/workflow/store/tests.rs` — the 11 `kv-mem` cases plus the 2
`#[ignore]`-by-default on-disk cases from `03-storage-schema.md` §10 (the
store-lock case and the round-trip case both need a real `SurrealKv` lock on
disk, so they cannot run against `kv-mem`).

**Watch out:** DB-facing structs derive `surrealdb_types::SurrealValue`, **not**
serde. The serde path in SurrealDB's own docs does not compile against 3.2.4.
Confirm `cc` + `cmake` are available before merging — on all four release
targets **and** for the `x86_64-pc-windows-msvc` target used by `just
windows-lint`, which `just check` runs and which is this phase's merge gate.
`cargo clippy --target` still builds dependency build scripts and native code for
that target, so `aws-lc-sys` (unconditional) has to link there too.

### W2 — kvdag model + engine (pure, no I/O)
**Files:** `src/workflow/model.rs`, `src/workflow/tier.rs`,
`src/workflow/engine/{mod,graph,schedule,complete}.rs`.
(`engine/expand.rs` and `engine/watchdog.rs` are stubbed with the types only in
Phase 1.)

**Delivers:** the types in `04-kvdag-and-execution.md` §2; `Kvdag::try_new`
invariant checking (acyclicity, edge endpoints, port/template coverage,
schema parse); `RunGraph::materialise(&Kvdag, tier)`; ready-set computation with
conditional/dead-edge propagation and `Skipped`; the completion state machine
including output-schema validation and `Succession`; `run_terminal_ready` as a
conjunction; `tier::resolve(tier, demand)` (the tables are written in Phase 1
even though the tier *prompt* ships in Phase 2 — the function is trivially
testable and its absence would force a rewrite later).

**Tested:** `#[cfg(test)] mod tests` beside each module. No PTY, no DB, no async.
Table-driven cases: diamond graph ready-set order; conditional false → `Skipped`
propagates; every `(tier, demand)` pair maps to the documented `(model, effort)`;
cycle rejected; missing port rejected; terminal-ready refuses while one node is
`NeedsAttention`; `SuccessionGap` raised when a node ends with no succession;
schema-invalid result triggers exactly one corrective re-prompt then
`NeedsAttention`.

**Boundary:** this workstream must not reference `App`, `TerminalRuntime`,
`surrealdb`, or `ratatui`. A grep test asserting that is cheap and worth adding.

### W3 — JSON API surface
**Files:** `src/api/schema/workflows.rs` (new), `src/api/schema.rs` (register
module + `Method` variants), `src/api/schema/response.rs` (result variants),
`src/api/schema/events.rs` (`Subscription`, `EventKind`, `EventData`),
`src/api/mod.rs` (`request_changes_ui`), `src/api/server.rs` (`api_method_name`
arms), `src/app/api/workflows.rs` (new handler module), `src/app/api.rs`
(`mod workflows;` + match arms), `docs/next/api/herdr-api.schema.json` (regenerated).

**Phase 1 methods** (`domain.verb` naming, neutral vocabulary):

| Method | Purpose |
|---|---|
| `workflow.list` | list workflows + head version |
| `workflow.get` | one workflow + its version chain summary |
| `workflow.create` | create a workflow and its v1 kvdag from a definition document |
| `workflow.version.create` | new authored version of an existing workflow from a definition document: `origin: "authored"`, `parent` = the current head, recomputed `spec_digest`, head pointer advanced. Without this, a saved workflow could never be hand-edited — only the Phase 4 self-improvement path could ever produce a v2 |
| `workflow.version.get` | full node/edge set of one version |
| `workflow.run` | start a run; returns immediately with `run_id` |
| `workflow.run.get` | run record + full run-graph projection |
| `workflow.run.list` | runs for a workflow, newest first |
| `workflow.run.cancel` | cancel a run and its panes |
| `workflow.node.get` | one `run_node` incl. pane binding and usage |
| `workflow.node.steer` | inject a steering message into a running node |
| `workflow.node.interrupt` | send an interrupt to a running node |
| `workflow.node.report` | **node self-report** (token-authenticated) |
| `workflow.node.restart` | restart a node: close its pane, `attempt += 1`, respawn. **In Phase 1 this always respawns from `task.md` alone**, because `partial` checkpoints are only written by the Phase 4 watchdog (`04` §4.4/§6.3) and none can exist yet. From Phase 4 it seeds from the node's latest `partial` checkpoint when one exists |

**Phase 1 events** (`EventKind` + `EventData` + `Subscription`):
`workflow.run.started`, `workflow.run.updated`, `workflow.run.finished`,
`workflow.node.created`, `workflow.node.updated`, `workflow.node.output_checkpoint`.
(Phase 2 adds `workflow.node.spawned`, `workflow.growth.limited`; Phase 4 adds
`workflow.node.watchdog`, `workflow.review.updated`.)

**Delivers:** all param/result structs deriving
`Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema`; handlers
that translate to engine inputs and back; `emit_event` calls after every
successful mutation. Mutating methods (`create`, `version.create`, `run`,
`run.cancel`, `node.steer`, `node.interrupt`, `node.report`, `node.restart`) go
into the `request_changes_ui` allowlist; the read-only ones do not.
`node.report` belongs there because it is the primary completion signal (`00` D7
precedence 1) and mutates node status, which the DAG view renders — exactly why
the existing `pane.report_agent`, `pane.report_agent_session`, and
`pane.report_metadata` are already in that allowlist.

**Feature-off behaviour:** the schema types compile **unconditionally** so
`docs/next/api/herdr-api.schema.json` has one canonical value. That imposes a
constraint worth stating, because violating it fails
`generated_protocol_schema_artifact_is_current` under `--no-default-features`:

- `src/api/schema/workflows.rs` is **self-contained** — it declares its own wire
  enums (node status, run status, demand, tier, succession, evidence) and
  contains **zero** `use crate::workflow::*`. The engine's vocabularies are
  duplicated on the wire deliberately; the wire type is a stable contract and the
  engine type is free to change.
- Every `From`/`TryFrom` between the wire types and the engine types lives in
  `src/app/api/workflows.rs` behind `#[cfg(feature = "workflow")]`.
- No schema change is needed for the error path: `ErrorBody.code` is a plain
  `String` (`src/api/schema/response.rs`), so `workflow_unavailable` is just a
  value. With `--no-default-features` the handlers return it.

**Tested:** `src/api/schema/tests.rs` regeneration test must pass with the
regenerated artifact committed; round-trip serde tests for each new param/result
type; a test asserting no new API identifier contains `sidebar|card|widget|row|panel`.

**No `PROTOCOL_VERSION` bump** — see §6.

### W4 — Runtime binding + Claude integration hook
**Files:** `src/workflow/binding/mod.rs` (landed complete in step 3a with both
`mod` lines and empty stubs, then not touched again), `binding/spawn.rs` (3b),
`binding/observe.rs` (3c), `src/app/workflow.rs`
(new: the `App` glue that owns `WorkflowRuntimeState` and pumps the engine),
`src/app/mod.rs` (add the field + a `Tick` arm in the select loop),
`src/events.rs` (new `AppEvent::Workflow*` variants),
`src/integration/claude_settings.rs` (register the `Stop` hook),
`src/integration/assets/claude/karvex-agent-state.sh` and `.ps1` (new `stop)`
arm), `src/integration/mod.rs` (`CLAUDE_INTEGRATION_VERSION` 7 → 8 — confirmed 7
at `src/integration/mod.rs:40`).

**Delivers:** `RunEffect` execution — pane spawn via
`Workspace::split_pane_argv_command` with the argv/env of
`04-kvdag-and-execution.md` §4.2 for both runners, `begin_managed_agent`
confirmation **for `runner: agent` nodes only**, node directory creation,
steering via `agent.prompt` (agent runner) or `pane.send_text` (command runner),
interrupt via `agent.send_keys`, node **restart** (close pane, `attempt += 1`,
respawn from `task.md` — no `partial` checkpoint exists in Phase 1), and pane
close; and the reverse direction — `AgentStatus` from `emit_pane_state_update`,
`TurnEnded` from the new `stop` hook, `PaneExited`, `NodeSelfReport` from
`workflow.node.report`.

**The Claude turn-end hook, against the real sites.** There is no
`CLAUDE_HOOK_EVENTS` constant — Claude does not use the per-agent hook-event
table pattern that Kimi/Devin/Droid/Copilot use. Its integration is a bespoke
module, and today it installs exactly **one** hook. The work is:

1. `src/integration/claude_settings.rs` — `install` currently calls
   `ensure_command_hook(hooks, "SessionStart", hook_command(hook_path, Some("session")), 10, Some("*"))`
   and nothing else. Add a second `ensure_command_hook` for event `"Stop"` with
   action `"stop"`, and a matching removal on the uninstall path.
2. **Reconcile with `HOOK_REMOVALS`,** which is the trap here. That table already
   contains `HookRemoval { event: "Stop", actions: &["idle"] }` — Claude was
   deliberately moved off lifecycle hooks onto screen detection, and every
   install/uninstall actively strips the old `Stop`/`idle` hook. The entry
   matches on **action**, so an action-`stop` hook is not touched by it; the new
   registration must keep the action distinct (`stop`, never `idle`) or it will
   remove itself on the next install. Add a test that asserts exactly this.
3. `src/integration/assets/claude/karvex-agent-state.{sh,ps1}` — the shell asset
   is `case "$action" in session) ;; *) exit 0 ;; esac`, so every action other
   than `session` currently exits silently. Add a `stop)` arm.
4. **Name the call the hook makes.** Claude has *no* state-reporting path today —
   unlike Kimi's `pane.report_agent` + `state = action`, the Claude asset only
   ever calls `pane.report_agent_session`, and its live status comes from
   `src/detect/`. So the `stop` arm calls the existing **`pane.report_agent`**
   with the turn-end state for the pane; `src/app/api/panes.rs` already handles
   that method, and the binder maps it to `EngineInput::TurnEnded`. No new API
   method, so no schema-artifact regeneration for this item.

This is not "an existing array gains an entry"; it is a hook registration, an
asset branch, and a reconciliation with a deliberate prior removal policy.

**Integration version rule:** `KARVEX_INTEGRATION_VERSION` is a *migration*
version relative to the latest released tag, not a per-commit counter. Bump the
claude asset **once** from the version in the latest release (7 in the current
source) to 8, and if the asset changes again before the next release, do not bump
again.

**Tested:** unit tests for argv/env construction and node-dir layout (pure
functions, no spawn); hook-asset tests extended in `src/integration/tests.rs`
(install adds the `stop` entry, uninstall removes it, version marker matches);
`just integration-assets-test` must stay green.

**Watch out:** `split_pane_argv_command` is already an in-process API with two
callers today — the plugin pane-open path (`src/app/api/plugins/panes.rs`) and
the built-in scrollback-editor launch (`src/app/input/navigate.rs`, via
`spawn_overlay_argv_command`). This is a third in-process caller. **Do not** add
an `argv` field to the public `pane.split` schema.

### W5 — CLI
**Files:** `src/cli/workflow.rs` (new), `src/cli/runtime.rs` (thin per-verb
wrappers), `src/cli.rs` (`mod workflow;` + dispatch arm), `src/cli/spec.rs`
(`workflow_command()` builder wired into the root `command()`).

**Subcommands:**

```
kvx workflow list
kvx workflow show <name|id>
kvx workflow create --file <definition.toml|json> [--name <n>]
kvx workflow update <name|id> --file <definition.toml|json>   # new authored version
kvx workflow run start <name|id> [--tier <t>] [--arg k=v]... [--json]
kvx workflow run list <name|id> [--limit N]
kvx workflow run show <run-id> [--json]
kvx workflow run cancel <run-id>
kvx workflow node show <run-id> <path> [--json]
kvx workflow node steer <run-id> <path> <text>
kvx workflow node interrupt <run-id> <path>
kvx workflow node restart <run-id> <path>
kvx workflow node complete [--result-file <path>]   # used BY a node, reads env
```

**Grammar note — why `run start` and not `run <name>`.** The obvious spelling
`kvx workflow run <name|id>` puts a positional workflow selector in the exact
slot where `run list|show|cancel` expect a subcommand. That makes `list`, `show`,
and `cancel` reserved words that are otherwise perfectly valid workflow names,
forces the hand-written parser to carry an arbitrary precedence rule, and creates
a silent-misdispatch class (`kvx workflow run list` could mean either thing).
`run` is therefore purely a namespace and `start` is the verb, so every position
is unambiguous. `kvx workflow update` is the CLI face of
`workflow.version.create`; without it a saved workflow could not be hand-edited.

`kvx workflow node complete` is what a teammate runs to self-report; it reads
`KARVEX_WORKFLOW_RUN_ID` / `NODE_PATH` / `NODE_DIR` / `NODE_TOKEN` from its own
environment, so the prompt contract is a single memorable command.

**Delivers:** manual arg parsing per the existing per-module convention, building
`Method::Workflow*` and calling `super::send_request`.

**Tested — and note where these tests can live.** `tests/cli/*` are black-box
integration tests: they spawn a real server and run the binary via
`env!("CARGO_BIN_EXE_kvx")`, and with no `[lib]` target in `Cargo.toml` they
cannot reach `Method`, `cli::spec::command()`, or the manual parser at all.
`tests/cli.rs` also carries `#![cfg(not(target_os = "macos"))]`, so anything put
there is skipped on the macOS CI leg. Therefore:

- **In-crate** `#[cfg(test)] mod tests` in `src/cli/workflow.rs` and
  `src/cli/spec.rs`: each verb parses to the right `Method`; and the parity test
  that every `kvx workflow` verb in the manual parser also exists in `spec.rs`
  and in the `cli.rs` dispatch match (this trio is hand-maintained and silently
  drifts otherwise). `src/cli/spec.rs` already has such a module with
  `command_path` / `collect_subcommand_paths` helpers to build on. The parity
  test must include a case asserting that a workflow named `list` is still
  reachable, which is the collision the grammar above exists to prevent.
- **`tests/cli/workflow.rs`**: end-to-end verb behaviour only (real binary, real
  server, JSON on stdout), knowing it does not run on macOS.

### W6 — TUI DAG view
**Files:** `src/workflow/layout.rs` (pure layered layout), `src/ui/line_cells.rs`
(new: the `pub(crate)` home for `LineCell` + `line_cell_symbol`),
`src/ui/panes.rs` (import them from their new home instead of defining them),
`src/ui/workflow_dag.rs` (new renderer), `src/ui.rs` (declare both modules, add
the `Mode` dispatch arm, call `compute_workflow_dag_view` from
`compute_view_internal`), `src/app/input/overlays.rs` (mouse arm + hit-test),
`src/app/input/mod.rs` (key arm), `src/app/input/modal.rs` (escape via
`leave_modal`).
(`Mode::WorkflowDag` and the `DagViewState` fields on `ViewState` land earlier,
in step 3a — see §2 — so that 4b and 4c are genuinely parallel.)

**Prerequisite, called out because it is not a drop-in import:** `struct LineCell`
(`src/ui/panes.rs:438`) and `fn line_cell_symbol` (`src/ui/panes.rs:667`) have no
visibility modifier — they are private to that one file — and `mod panes;` is
itself private in `src/ui.rs`. Step one of W6 is moving both into
`src/ui/line_cells.rs` as `pub(crate)` and repointing `panes.rs` at them. This is
a mechanical move with no behaviour change, and `src/ui/panes.rs`'s existing
tests cover it.

**Delivers:** a full-bleed overlay showing the run graph; node boxes as
`Block` + `Paragraph` at precomputed rects; edges as bitmask cell writes rendered
through the relocated `line_cell_symbol`; a detail strip for the selected node
(status, model, usage, last checkpoint summary, blocker); a footer hint bar;
keyboard graph-aware navigation; click/Enter → focus the node's pane via
`focus_pane_internal_via_api`; `s` → steer input line → `workflow.node.steer`;
`Esc` → `leave_modal`.

**Non-negotiables:** layout runs **once**, in `compute_view_internal`, storing
rects into `ViewState`; `render_workflow_dag(app: &AppState, ...)` only draws;
hit-testing reads the same stored rects. Colours come from `Palette` semantic
slots; selection contrast via `panel_contrast_fg`. No
`ratatui::widgets::canvas::Canvas`.

**Tested:** `src/workflow/layout.rs` unit tests (layer assignment for a diamond,
crossing reduction is stable under node insertion, rects never overlap, output is
deterministic for the same graph); a hit-test test asserting
`node_at(col,row)` agrees with the stored rect list for a generated graph; an
`AppState::test_new()`-based test that entering/leaving `Mode::WorkflowDag`
round-trips and that `render` is never given `&mut AppState`.

### W7 — Headless end-to-end
**Files:** `tests/workflow_headless.rs` (new, modelled on
`tests/server_headless.rs`), `tests/support/` additions, a fixture kvdag under
`tests/fixtures/workflow/`.

**The fixture uses `runner: "command"`,** not a fake `claude`. Its two nodes run a
tiny script that writes `result.json` and calls `kvx workflow node complete` —
deterministic, no network, no API cost. This is the plain-process binding path of
`04-kvdag-and-execution.md` §4.2, chosen by a declared node field, not by a
test-only escape hatch. That matters because the managed-agent path is closed to
a stub by construction: `begin_managed_agent` takes a `crate::detect::Agent`
(there is no generic variant) and `reconcile_managed_agent_at` clears the managed
agent at the deadline unless the pane resolves to that agent, so a stub would be
reported as a **spawn failure**; and `handle_agent_prompt` returns
`agent_not_ready` unless `effective_known_agent()` is `Some` *and*
`runtime_hosts_agent` matches the pane's foreground job, so `agent.prompt`
against a stub can never succeed. The `runner: agent` path is exercised by the
manual real-`claude` run only.

**Delivers:** spawn a real headless karvex server, `workflow.create` the two-node
kvdag, `workflow.run`, subscribe to `events.subscribe`, assert the event
sequence, assert both panes were created, assert the run reaches `succeeded`, and
assert both `run_node`s report `evidence: self_report`.

**Where each assertion lives.** karvex has no library target — `Cargo.toml`
declares only `[[bin]] name = "kvx"` and there is no `src/lib.rs` — so an
integration test cannot link `WorkflowStore`, and none of the Phase 1 methods
exposes `run_event` or `node_checkpoint` (`workflow.run.get` returns the run
record plus the run-graph projection). So:

- `tests/workflow_headless.rs` asserts only **API-observable** facts: run status,
  node statuses, `evidence`, pane creation, and the event stream from
  `events.subscribe` (including that its sequence is contiguous).
- Checkpoint contents and the `run_event` journal's contiguous `seq` are asserted
  in in-crate `#[cfg(test)]` store tests (`src/workflow/store/tests.rs`), which
  can link the store directly.

Second e2e: a node that goes idle **without** writing `result.json` must end in
`NeedsAttention`, and the run must **not** report success — this is the single
most important behavioural guarantee in the design (`00-overview.md` D7) and it
needs an e2e, not just a unit test.

Third e2e: `workflow.node.steer` on a running node delivers text into the pane
(assert via `pane.read`). For a `runner: command` node the binder delivers via
`pane.send_text` and journals `delivery: "raw"` (`04` §5) — `agent.prompt` is not
attempted, so this passes without the stub having to impersonate a detected
agent.

**Watch out:** the stub path must not require the `claude` binary; the real
`claude` path is exercised manually, not in CI.

---

## 2. Ordered Phase 1 workplan

Each step: files, deliverable, test. Steps at the same number can run in
parallel.

| # | Step | Files | Delivers | Tested by |
|---|---|---|---|---|
| 1a | Cargo feature + deps + module skeleton + build plumbing | `Cargo.toml`, `Cargo.lock`, `src/workflow/mod.rs` (all `mod` lines, empty stub files), `src/main.rs` (`mod workflow;`), `justfile`, `.github/workflows/ci.yml` | `workflow` feature; surrealdb pinned; the module tree exists and is reachable so every later step lands into a compiling crate; `just check-no-workflow` recipe wired into `just ci`; `windows-lint` switched to `--no-default-features`; cmake/ninja install steps on the ubuntu and windows CI legs; check-job `timeout-minutes` raised | `cargo build`, `just check-no-workflow`, and `just windows-lint` all green |
| 1b | kvdag model | `src/workflow/model.rs` | types + `Kvdag::try_new` invariants | unit: cycle/port/arg/schema rejection |
| 1c | API schema types | `src/api/schema/workflows.rs`, `schema.rs`, `response.rs`, `events.rs` | all Phase 1 methods/results/events as types | serde round-trip + regenerated artifact |
| 2a | Store | `src/workflow/store/*`, migrations | typed append-only store on `SurrealKv`/`Mem` | the 11 `kv-mem` + 2 `#[ignore]` on-disk store tests (`03` §10) |
| 2b | Engine | `src/workflow/engine/{graph,schedule,complete}.rs` | ready set, conditions, completion, succession, terminal-ready | table-driven unit tests |
| 2c | Tier function | `src/workflow/tier.rs` | `resolve(tier, demand)` | exhaustive `(tier, demand)` table test |
| 2d | API handlers (stubbed engine) | `src/app/api/workflows.rs`, `src/app/api.rs`, `src/api/mod.rs`, `src/api/server.rs` | every method routed, `request_changes_ui` updated | handler unit tests + the `not_implemented` sweep below |
| 3a | `App` glue + shared state | `src/app/workflow.rs`, `src/app/mod.rs`, `src/events.rs`, **`src/app/state.rs`**, `src/workflow/binding/mod.rs` (all `mod` lines + empty stubs) | `WorkflowRuntimeState` on `AppState`, engine pumped from the select loop, effects dispatched; **plus** `Mode::WorkflowDag`, its `mouse_motion_changes_view`/`wants_ascii_input` membership, and the `DagViewState` fields on `ViewState` | `AppState::test_new()` tests for state transitions and mode round-trip |
| 3b | Spawn binding | `src/workflow/binding/spawn.rs` | argv/env/node-dir construction + pane spawn; `begin_managed_agent` on the `agent` runner only | pure unit tests on argv/env/dirs for both runners |
| 3c | Observe binding | `src/workflow/binding/observe.rs` | status/turn-end/exit → `EngineInput` | unit tests over synthetic events |
| 3d | Claude turn-end hook | `src/integration/claude_settings.rs`, `src/integration/assets/claude/karvex-agent-state.{sh,ps1}`, `src/integration/mod.rs` | `Stop` hook registration with action `stop`, asset `stop)` arm, `CLAUDE_INTEGRATION_VERSION` 7→8 | `src/integration/tests.rs` (install adds it, uninstall removes it, the `Stop`/`idle` `HOOK_REMOVALS` entry does **not** strip it, version marker matches) + `just integration-assets-test` |
| 3e | Layout | `src/workflow/layout.rs`, `src/ui/line_cells.rs`, `src/ui/panes.rs` | layered layout → rects + edge bits; `LineCell`/`line_cell_symbol` lifted to a `pub(crate)` home | determinism/overlap/stability unit tests; existing `panes.rs` tests still green |
| 3f | Config | `src/config/model.rs`, `docs/next/website/src/data/config-reference.json` | the `[workflow]` block: `max_parallel_nodes` (4), `retention_runs` (50), `stuck_threshold` (3), `drift_threshold` (5) — the last two are read by Phase 4 but declared now so the reference table lands once | `python3 scripts/config_reference_check.py` |
| 4a | CLI | `src/cli/workflow.rs`, `runtime.rs`, `cli.rs`, `spec.rs` | all `kvx workflow` verbs incl. `run start`, `update`, `node complete` | in-crate parser/spec/dispatch parity tests + `tests/cli/workflow.rs` |
| 4b | DAG view render | `src/ui/workflow_dag.rs`, `src/ui.rs` | the overlay, node boxes, edges, detail strip | render-purity + geometry tests |
| 4c | DAG view input | `src/app/input/{overlays,mod,modal}.rs` | click/keys/steer/escape | hit-test agreement test |
| 5 | E2E | `tests/workflow_headless.rs`, fixtures | the three scenarios in W7 (API-observable assertions only) | itself |
| 6 | Docs | `docs/next/website/src/content/docs/{,ja/,zh-cn/}<file>.mdx`, `docs/next/api/herdr-api.schema.json` | user-facing docs for `kvx workflow` and the DAG view, in all three locales | `just release-docs-check` green (translation parity + heading-outline parity); schema artifact test green |

**Step 6 is not free.** `just release-docs-check` requires a `ja/` and `zh-cn/`
counterpart for **every** `docs/next/website/src/content/docs/*.mdx`, then runs
`scripts/docs_translation_parity.py`, which compares heading outlines per file.
So both a new `.mdx` *and* new headings added to an existing one (e.g.
`cli-reference.mdx`) break it without matching `ja` and `zh-cn` edits.

Merge gate for the whole phase: `just check` green (fmt, clippy `-D warnings`,
nextest, Windows target lint, maintenance script tests).

**The merge gate is why step 1a owns `justfile` and `ci.yml`.** On Unix
`check: ci windows-lint`, and `windows-lint` runs
`cargo clippy --bin kvx --locked --target x86_64-pc-windows-msvc`. `--target`
still builds dependency build scripts and native code for that target, so a
default-on `workflow` feature drags surrealdb and (unconditionally, per `03` §1)
`aws-lc-sys` → `cc` + `cmake` into an MSVC cross-build from Linux. Step 1a
therefore:

- switches `windows-lint` to `--no-default-features` (the Windows lint exists to
  catch `cfg(windows)` compile errors in karvex's own code, not to build
  SurrealDB for MSVC);
- adds `check-no-workflow` (`cargo clippy --locked --no-default-features
  --all-targets -- -D warnings` then `cargo nextest run --locked
  --no-default-features`) and calls it from `just ci`, so the feature-off build
  the whole D4 dependency decision rests on is actually enforced instead of
  rotting within one PR — today **no** recipe and **no** CI job builds with
  `--no-default-features`;
- adds cmake/ninja install steps to the ubuntu and windows CI matrix legs
  (`ci.yml` installs them only on the macOS leg today, `if: runner.os == 'macOS'`);
- raises the check job's `timeout-minutes: 15`, which is tight against `03`'s own
  "+2.5–4 min clean build, ~+257 crates" estimate.

---

## 3. Interfaces frozen at the start of Phase 1

These are the contracts the parallel workstreams code against; changing them
mid-phase means coordinating edits, so they are decided up front:

1. `EngineInput` / `RunEffect` enums (`04` §2) — W2 owns them, W4 consumes them.
2. `WorkflowStore` trait method signatures — W1 owns, W3/W4 consume.
3. The `workflow.*` `Method` / `ResponseResult` / `EventKind` names (W3 §above).
4. `DagLayout { nodes: Vec<(RunNodeIdx, Rect)>, edge_cells: HashMap<(u16,u16), EdgeBits> }`
   — W6's layout output, consumed by both render and hit-test. `EdgeBits` is a
   workflow-local `{ up, down, left, right }`, deliberately **not**
   `crate::ui::panes::LineCell`: `src/workflow/layout.rs` is a pure layer and
   must not depend on `src/ui`. The renderer converts `EdgeBits → LineCell` at
   draw time (`04` §8).
5. `KvdagNode.runner` (`agent | command`) and `KvdagNode.command` — W2 delivers
   them in step 1b, W4 consumes them in step 3b. This is the only thing that
   selects the spawn/steer binding, and the e2e fixture depends on it.
6. `Kvdag.args` (the run-argument namespace) and the rule that a `{{name}}`
   resolves to an inbound edge port **or** a declared arg — W2 owns, W5's
   `--arg k=v` and the W7 fixture both depend on it.
7. `Mode::WorkflowDag` and the `DagViewState` field shape on `ViewState` —
   landed in step 3a so that 4b (render) and 4c (input) are genuinely parallel;
   4c cannot compile until the variant and the fields exist.
8. The node prompt/output contract: `task.md`, `output_schema.json`,
   `result.json`, `kvx workflow node complete`. Changing it invalidates every
   node prompt template.

---

## 4. Definition document format (Phase 1)

`workflow.create` takes a definition document — TOML or JSON, the same shape as
the kvdag types. TOML keeps it hand-editable, matching the repo's config
conventions:

```toml
name = "ship-feature"
description = "plan → implement → review"
contract = """
Reply only through result.json. Never edit files outside your node dir unless the task says so.
"""
max_depth = 3
max_nodes = 24

# Declared run-argument namespace. Without this, `{{goal}}` below has no inbound
# edge port to resolve against and `Kvdag::try_new` rejects the document.
[[arg]]
name = "goal"
required = true
description = "what to build"

[[node]]
key = "plan"
label = "Plan"
kind = "agent"
runner = "agent"          # `claude` teammate; "command" runs `command` as a plain process
demand = "critical"
prompt_template = "Produce an implementation plan for: {{goal}}"
output_schema = { type = "object", required = ["plan"], properties = { plan = { type = "string" } } }

[[node]]
key = "implement"
label = "Implement"
kind = "agent"
demand = "peak"
prompt_template = "Implement this plan:\n{{plan}}"
output_schema = { type = "object", required = ["changed_files","report"], properties = { changed_files = { type = "array" }, report = { type = "string" } } }

[[edge]]
from = "plan"
to = "implement"
kind = "data"
payload = "summary"
port = "plan"
```

Run it with `kvx workflow run start ship-feature --arg goal="add dark mode"`.
`workflow.run` rejects a run that omits a required arg with no default, and
`Kvdag::try_new` rejects a template referencing a `{{name}}` that is neither an
inbound edge port nor a declared arg — so the two failure modes are caught at
authoring time and at run time respectively, never at prompt-render time.

Authoring a definition **by hand or by asking an agent to write one** are both
supported; there is no separate authoring DSL and no code generation, because the
definition is already data (`02-mjs-workflow-evaluation.md`). Editing a saved
workflow goes through `kvx workflow update` / `workflow.version.create`, which
writes a new immutable `kvdag_version` with `origin: "authored"` — the definition
is never mutated in place.

---

## 5. Phases 2–4 (outline)

### Phase 2 — Dynamic graphs + tiers
- `src/workflow/engine/expand.rs`: proposal → validation → commit/reject
  (`04` §3.4), `spawned` relations, inherited outbound edges.
- `kvx workflow node expand` CLI verb + `workflow.node.expand` method; `expand`
  field accepted in node results.
- New events `workflow.node.spawned`, `workflow.growth.limited`; DAG view badges
  and run banner for guardrail breaches.
- Tier prompt modal at create and at run (reusing `modal_stack_areas` +
  `centered_button_row`); `--tier` already exists on the CLI from Phase 1.
- `tier::resolve` wired into spawn; resolved `(model, effort)` persisted per
  `run_node`; `auto` policy over `NodeHistory`.
- Tests: expansion at depth limits, node-count limits, template allowlist
  violations, and that a rejection is always surfaced (never silent).

### Phase 3 — History, summaries, restore
- Run browser overlay (Navigator-shaped list + detail), backed by
  `workflow.run.list` / `workflow.run.get`.
- End-of-run summariser as an `Internal` node with a fixed what-to-cover
  template and a hard token budget (`03` §7); `run_summary` written; prior-run
  summaries injected into new runs by default with a per-run opt-out.
- `workflow.node.interrogate` → `claude --resume <sid> --fork-session`, an
  `interrogation` record, and a detached node in the DAG view.
- Checkpoint restore: `kvx workflow run start <wf> --restore-from <run>
  --restore <selector>`; restored nodes get `status: Restored` with no pane;
  cross-version compatibility decided by output-schema + prompt digests (`03` §5).
- `transcript_unavailable` handling: interrogate stats
  `run_node.transcript_path` first and offers the **reconstructed** fallback
  (checkpoint + `task.md` seeded, flagged as such) when the source session is
  gone (`00` Feature 3, `03` §4.4). Pruned runs (`03` §9) surface as
  summary-only, with both restore actions disabled and a reason.
- Tests: restore into a *different* kvdag version; restore of a subgraph;
  interrogation does not mutate the source transcript; a missing transcript
  yields `transcript_unavailable` and never a silently-failing pane.

### Phase 4 — Self-improvement + reliability
- `src/workflow/engine/watchdog.rs`: materiality-based progress, four-way
  classification, the escalation ladder, the productive-use check (`04` §6).
- Review cycle as its own small kvdag: per-teammate 1:1 interviewer nodes →
  synthesis node → `review_finding` records classified prompt-level vs
  structural; `verdict: replace` structurally requires a `replacement`.
- **The 1:1 is a real interview.** Each interviewer node revives its target with
  `claude --resume <run_node.agent_session_id> --fork-session` (the same
  non-destructive fork as interrogation restore), asks a fixed question set, and
  records the exchange as an `interrogation` row linked from `review_cycle.interviews`
  and `review_finding.interview`. Measured per-node data (attempts, watchdog
  interventions, tokens/tools/duration, downstream rework) is put *to* the
  teammate as evidence during the interview, not used as a substitute for it.
  When the source session cannot be resumed, the interviewer falls back to an
  evidence-only pass and every finding is stamped
  `interview_mode: "evidence_only"` so a reader can tell an account from an
  inference.
- Accept/decline UI; accepted findings compiled into a **new** `kvdag_version`
  with `origin: self_improvement` and a per-node `change_summary`.
- Optional, feature-gated ACP headless executor (`01-acp-evaluation.md` §4.3) —
  only if the re-evaluation triggers there have fired.
- Tests: a stuck stub node escalates through all four rungs (and the `partial`
  checkpoints it writes are what finally give `workflow.node.restart` something
  to resume from — see W3); a review cycle produces a new version whose parent is
  the run's version and whose unchanged nodes keep their `node_key`s and digests;
  a review cycle whose target session is missing produces `evidence_only`
  findings rather than failing.

---

## 6. Protocol, schema, and version policy

- **`PROTOCOL_VERSION` is not bumped.** The binary render/input wire format in
  `src/protocol/wire.rs` is untouched: no new `ClientMessage`/`ServerMessage`
  variants, no `FrameData` change. The DAG view is rendered server-side into the
  existing frame stream, and node clicks route to existing focus calls. Adding
  `Method`/`ResponseResult`/`Subscription`/`EventKind` variants is additive to
  self-describing JSON and does not require a bump. Because
  `src/cli/protocol_guard.rs` enforces **exact** version equality for every CLI
  call, a gratuitous bump would force every user to restart their server for no
  benefit.
- **`docs/next/api/herdr-api.schema.json` must be regenerated** in the same
  change as any `Method`/param/result/event addition:
  `KARVEX_UPDATE_API_SCHEMA=1 just test-one generated_protocol_schema_artifact_is_current`.
- **`CLAUDE_INTEGRATION_VERSION` 7 → 8, once**, for the `stop` hook action
  (W4). It is a migration version relative to the latest release, not a
  per-commit counter. (Confirmed 7 in `src/integration/mod.rs:40` and 7 at the
  latest release tag, so 8 is the correct next value.)
- **`request_changes_ui`** (`src/api/mod.rs`) gains the mutating `workflow.*`
  variants only: `create`, `version.create`, `run`, `run.cancel`, `node.steer`,
  `node.interrupt`, `node.report`, `node.restart`.
- **`api_method_name`** (`src/api/server.rs`) gains one arm per new method. Its
  own `match` is genuinely exhaustive — but **the compiler does not catch a
  forgotten handler**: the primary dispatch `match request.method` in
  `src/app/api.rs` ends in a catch-all arm returning
  `encode_error(request.id, "not_implemented", …)`, so a missing `workflow.*` arm
  compiles cleanly and silently answers `not_implemented`. The safety net is a
  test, not the type system: step 2d adds a sweep that dispatches a well-formed
  request for **every** `workflow.*` method against `AppState::test_new()` and
  asserts the error code is never `not_implemented`.

## 7. Risk register for Phase 1

| Risk | Mitigation in the plan |
|---|---|
| SurrealDB build cost blocks CI | W1 is step 1a, before anything depends on it; step 1a also **enforces** the feature-off escape hatch with a `just check-no-workflow` recipe wired into `just ci` and the ubuntu CI leg, so it cannot rot unnoticed |
| SurrealDB build cost blocks the **local merge gate** | `just check` runs `windows-lint`, which cross-builds native deps for `x86_64-pc-windows-msvc`. Step 1a switches that recipe to `--no-default-features` and raises the CI check-job timeout; cmake/ninja are installed on the ubuntu and windows legs, not just macOS |
| `cc`/`cmake` missing on a release target | Verified in W1 before merge, for all four release targets **and** the `x86_64-pc-windows-msvc` lint target; the feature flag gives a shippable fallback |
| Engine and binding drift apart | `EngineInput`/`RunEffect` frozen at §3 item 1 before W4 starts |
| CLI help/completion drift | Explicit parity test in W5 (three hand-maintained places) |
| DAG hit-test disagreeing with render | Single stored geometry + an explicit agreement test in W6 |
| E2E flakiness from real agents | `runner: command` stub nodes in CI — a first-class binding, not a test hook, so the e2e exercises real engine paths; real `claude` (`runner: agent`) exercised manually only |
| CLI verb collision (`workflow run list` vs a workflow named `list`) | `run` is a namespace and `start` is the verb, so no position is ambiguous; the parity test covers the collision case |
| Scope creep from Phases 2–4 | Growth, tiers, history, restore, review, watchdog are all explicitly out of Phase 1; only `tier::resolve` and the watchdog *evidence* plumbing land early, both because retrofitting them is more expensive than writing them now |
