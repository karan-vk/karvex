# Phase 3 implementation plan — run history, end-of-run summaries, and restore

Release target: **v0.12.0**.

This is the build contract for Phase 3, written against the tree at v0.11.0
(`0fb68912`) the same way `06-phase2-plan.md` was written against v0.9.4. Where
`05-phase-plan.md` §5's Phase 3 outline is stale — and it is stale in seven
places — this document supersedes it and says why. Every file, function, and
line number below was re-checked against the current tree.

Phase 3's definition of done:

> **A run leaves something behind, and what it leaves behind is usable.** Every
> finished (succeeded or failed) run gets a token-budgeted `run_summary` written
> by a visible summariser node, and new runs of the same workflow read those
> summaries by default with a per-run opt-out. Past runs are browsable in the
> TUI as a list-and-detail overlay and openable as a read-only DAG view. A past
> node can be interrogated by reviving its Claude session with
> `--resume --fork-session` — never mutating the source transcript — and when
> the transcript is gone the caller gets a structured `transcript_unavailable`
> answer and an explicitly-labelled reconstructed fallback, never a silently
> failing pane. A new run can restore checkpointed results from a past run,
> across kvdag versions, with per-node compatibility decided by prompt and
> output-schema digests; restored nodes are `Restored`, pane-less, and feed
> their downstream edges exactly like succeeded ones. A pruned run surfaces as
> summary-only with restore disabled and a reason. And the two known
> durable-read-path field losses (`limit_value` → 0 for expand-max truncation,
> `at_unix_ms` restamped at store-flush time) are closed, because Phase 3 adds
> three new durable read paths and must not add three new instances of that
> bug class.

Explicitly **not** in Phase 3: the watchdog and `partial` checkpoints (Phase 4),
the review cycle and interviewer nodes (Phase 4 — the `interrogation` machinery
built here is deliberately shaped so Phase 4's interviews are "the same spawn
with a different prompt"), live resume of an interrupted run (04 §9's resume
offer; only the honest half — surfacing the interruption and offering
checkpoint restore — ships now), checkpoint payload spill-to-artifact as a
store feature (§4 D19 keeps it a flagged risk with an optional fold-in), and
any change to `ui.toast.delivery` defaults.

---

## 0. Reality check: what the outline predates

Seven corrections to `05-phase-plan.md` §5.3 and the design docs, each of which
changes the plan rather than the prose. Verified against the tree at v0.11.0.

1. **Every Phase 3 table already exists; Phase 3 ships writers, not schema.**
   Migration `0001_init.surql` shipped the whole `03-storage-schema.md` §4
   schema in Phase 1, including `run_summary` (`:227`), `interrogation` (`:243`),
   `review_cycle` (`:258`), and `review_finding` (`:269`). The row types exist
   (`store/records.rs:231`, `:247` — the latter under the comment *"schema
   present; Phase 1 has no writer"*), and so do the readers:
   `get_run_summary` (`store/queries.rs:659`), `list_run_events` (`:495`),
   `list_checkpoints` (`:509`), `find_restorable_checkpoints` (`:532`), and
   `prune_run_history` (`store/mod.rs:955`, with `prune_one_run` at `:984-1012`
   already implementing 03 §9's dangling-reference rules exactly). **None of
   these has a production caller.** The Phase 3 migration is `0004`, and it is
   small (§4 D15).

2. **`Restored` exists end-to-end with no producer.** `NodeStatus::Restored`
   is terminal (`model.rs:1049-1054`), fires `Sequence`/`Data`/`Conditional`
   edges (`engine/schedule.rs:89,94`), resolves succession like `Succeeded`
   (`complete.rs:522`), persists and parses (`store/mod.rs:1205,1220`), maps to
   the wire (`api/schema/workflows.rs:125`, `app/workflow.rs:2132`), and renders
   in the DAG view as teal `↺ restored` (`ui/workflow_dag.rs:1009,1023,1038`).
   `Evidence::Restored` likewise round-trips end to end. **Nothing in `src/`
   ever constructs either.** The same is true of `workflow_run.restore_from`
   (`0001_init.surql:113`, `records.rs:129`) and `run_node.restored_from`
   (`0001:167`, `records.rs:171`): columns and decoders with no writer. Restore
   is therefore mostly *plumbing to existing seams*, not new semantics.

3. **The run browser cannot be "backed by existing `workflow.run.list` /
   `workflow.run.get`" as the outline says — three gaps.**
   (a) `WorkflowRunListParams.workflow_id` is **required**
   (`api/schema/workflows.rs:222-227`), so a cross-workflow browser has no
   single call; (b) `WorkflowRunInfo` carries `workflow_id` but not the
   workflow's *name*, so a list row cannot be labelled without N extra calls;
   (c) pruned runs have **no `workflow_run` row at all** — `prune_one_run`
   deletes the run outright and preserves only `run_summary` — so no run-list
   method can ever return them. The browser needs `workflow_id` made optional,
   a `workflow_name` field on `WorkflowRunInfo`, and a summary-listing method
   (§4 D9, D10).

4. **A `running` run orphaned by a server restart stays `running` forever.**
   There is no engine rehydration: `Engine` is built fresh per `start()`
   (`app/workflow.rs:219,406`), nothing at store open reconciles non-terminal
   `workflow_run` rows, and `node_history` only dodges the problem by filtering
   to `CLOSED_RUN_STATUSES` (`queries.rs:30,588`). Phase 1–2 never displayed
   old runs, so this was invisible; the run browser makes it a lie on screen
   ("running" for a run whose panes died last week). 04 §9's "load into
   `Paused` and offer resume" presupposes resume machinery that does not exist
   and is out of scope; Phase 3 ships the honest subset (§4 D13).

5. **The stored `transcript_path` is a pre-launch estimate that is never
   corrected.** `spawn.rs:745-755` derives it from `(claude_dir, slug(cwd),
   session_id)`; its own docstring says to prefer the path the `SessionStart`
   hook reports "once it arrives" — and nothing reads the hook's value back.
   03 §4.4's stat-first rule absorbs most of the risk (a wrong estimate stats
   as absent → `transcript_unavailable`, not a broken pane), but interrogation
   should not report "transcript unavailable" for a session whose transcript
   exists at the *reported* path. §4 D6 adds the read-back.

6. **Two durable-read-path field losses from the 0.10.2 P1 family are still
   open, in exactly the code Phase 3 extends.**
   (a) A `Truncated { limit: ExpandMax }` rejection writes no `limit_value`
   key (`expand.rs:694-742` maps `ExpandLimit::ExpandMax => None`), so both the
   live path (`app/workflow.rs:1830-1837`) and the journal read path
   (`queries.rs:790-822`, `unwrap_or(0)`) report `limit_value: 0` instead of the
   node's `expand_max`. (b) `StoreWrite::RunEvent` (`model.rs:1393-1399`)
   carries **no timestamp**; `write_run_event` (`store/mod.rs:1615`) never binds
   `at`, so `run_event.at` is minted by `DEFAULT time::now()` at store-flush
   time — the same second-clock defect 0002 killed for `workflow_run.started_at`
   — and every journal-derived `at_unix_ms` (growth limits today; anything
   Phase 3 reads from the journal tomorrow) drifts from the live value by the
   write-queue latency, unboundedly under backlog. Both fixes are folded into
   Phase 3's foundation (§4 D14) because the summariser and restore read paths
   would otherwise inherit the bug class this plan is required to design
   against.

7. **The summariser cannot be a pre-seeded sink node, and `TerminalContext` is
   dead in production.** The zero-engine-change option — materialise the
   summariser with inbound `sequence` edges from every leaf — breaks on any
   failed leaf: a `Failed` source never resolves its outbound edge, the
   summariser sits `Pending` forever, and `run_terminal_ready`'s first conjunct
   pauses the run instead of failing it. Meanwhile the designed extension point
   for extra terminal conjuncts, `run_terminal_ready_with(TerminalContext)`
   (`schedule.rs:269-309`), is only ever called with `TerminalContext::default()`
   (`schedule.rs:272-274` via `Engine::settle`, `engine/mod.rs:1220-1271`) — its
   two existing conjuncts are dead. The summariser is therefore an
   **engine-owned epilogue** that runs *after* the user graph's terminal status
   is decided and can never change it (§4 D1) — a deliberate deviation from a
   literal reading of 04 §4.5.

Also inherited from Phase 2's plan and still true: `just check` covers both
feature legs and the MSVC lint (`check-slim` is in `ci`); only
`src/workflow/store` is feature-gated, everything else in `src/workflow/`
compiles unconditionally and must stay `App`/`surrealdb`/`ratatui`-free; the
schema artifact regenerates with
`KARVEX_UPDATE_API_SCHEMA=1 just test-one generated_protocol_schema_artifact_is_current`.

**The uncommitted session-name diff.** *(Superseded during the build — the
feature merged as `e10368b1` before Phase 3's step 2, so the protections below
became moot; kept for the record. See §8 E-4.)* The worktree at planning time
carried an uncommitted agent-session-name feature (new `src/agent_session_registry.rs` +
`src/app/agent_session_names.rs`, edits in `src/events.rs`, `src/app/{actions,
api,creation,mod,runtime}.rs`, `src/api/schema/agents.rs`, `src/main.rs`,
`src/server/headless.rs`, `src/config/sidebar.rs`, `src/integration/mod.rs`,
`src/terminal/state.rs`, `src/workspace/aggregate.rs`, `src/ui/sidebar*`,
`src/ui/mobile.rs`). **Phase 3 must not modify or revert any of it.** The
overlap analysis (verified per hunk):

- **Zero overlap** with every file Phase 3 owns heavily: `src/workflow/*`,
  `src/app/state.rs`, `src/app/workflow.rs`, `src/app/api/workflows.rs`,
  `src/api/schema/workflows.rs`, `src/ui/workflow_*`, `src/app/input/*`,
  `src/config/{model,keybinds}.rs`, `src/ui.rs`.
- **Adjacent-hunk risk only** in: `src/app/api.rs` (Phase 3 adds dispatch
  arms to the workflow block at `:1196-1234`; the session-name diff touches a
  different region), `src/events.rs` (Phase 3 needs **no** new `AppEvent`
  variant — §4 D8 routes interrogation-pane death through the existing
  `PaneDied` path precisely to keep this file untouched), `src/app/mod.rs`
  (untouched by Phase 3 — the epilogue rides the existing workflow tick),
  `src/main.rs` (one sample-config keybind line at `:218-219` — same region as
  the diff; do it last), and `docs/next/CHANGELOG.md` (guaranteed textual
  conflict; trivial).
- **Build-order rule:** the session-name feature is expected to be committed
  before the Phase 3 build starts. If it is not, the only files where a Phase 3
  workstream must rebase around it are `src/app/api.rs`, `src/main.rs`, and
  `docs/next/CHANGELOG.md`, and all three edits are additive one-liners.

---

## 1. Phase 3 workstreams

Nine workstream headings — eight owning streams plus WS-G, the short
shape-landing stage that afterwards merges into WS-F. (The letter I is unused,
to avoid I/1 confusion.) Three owners land shape-only files in step 1 —
step 1a first, then 1b and 1c in parallel — so the rest are genuinely parallel
afterwards.

**Shared-file rule.** No two workstreams edit the same file during a parallel
step. Files that multiple streams need are landed complete in step 1 by a
single owner and never touched again:

| File | Step-1 owner | Why shared |
|---|---|---|
| `src/workflow/model.rs` **plus the struct-literal sweep below** | WS-A | `StoreWrite`/`EngineInput`/`WorkflowEvent`/`RunGraph`/`RestoredSeed` shapes consumed by WS-B, WS-D |
| `src/app/state.rs` + the mode-registration stubs (`src/ui.rs`, `src/app/input/{mod,modal,overlays}.rs`) | WS-G | `Mode::WorkflowRuns`, `WorkflowRunsState`, `DagViewState` additions (WS-G, WS-H) |
| `src/api/schema/workflows.rs` + `schema.rs` + `response.rs` + `events.rs` + `src/api/subscriptions.rs` + `src/api/mod.rs` + `src/api/server.rs` | WS-C | wire types consumed by WS-D, WS-E, WS-G, WS-H |
| `src/workflow/store/migrations/0004_journal_time_and_interrogation.surql` | WS-B | one migration file, one owner |

**The step-1a struct-literal sweep.** `RunNode` and `RunGraph` have no
`Default`; step 1a adds fields (`RunNode.restored_from`, `RunGraph.epilogue`),
so every construction literal must be extended in the same step or the tree
stops compiling. The construction sites at v0.11.0 (re-verify with
`grep -rn "RunNode {" src/ | grep -v "//"` before starting; the Phase 2 sweep
list drifted and this one will too): `src/workflow/engine/graph.rs`,
`src/workflow/engine/tests_support.rs`, `src/workflow/layout.rs` (test
fixtures), `src/ui/workflow_dag.rs` (test fixtures), `src/app/workflow.rs`,
plus **`src/app/input/modal.rs:1581` and `src/app/input/navigate.rs:2726`,
which hold `RunGraph` (not `RunNode`) test literals** — `epilogue: None`
one-liners only, otherwise those files belong to WS-G (§8 E-1: an earlier
draft claimed zero literals there; the grep was `RunNode`-only), and
**`src/app/mod.rs`**, which carries headless mirrors outside every
workstream's list — the `AppState { … }` literal and
`handle_non_terminal_key_headless`'s mode dispatch (§8 E-4; app/mod.rs is
sweep-only territory for this phase — anything beyond a mechanical arm/field
routes through the design authority). Files in
that list that belong to another workstream are touched **only** for the sweep
in step 1a and then handed to their owner. Step 1a lands before 1b starts.
The standing rule this build proved three times over: **the compiler is the
site list** — every enumeration here is a starting hint, and `cargo check` on
both feature legs is the authority.

```
WS-A model+engine ──┬──▶ WS-B store ─────────┐
WS-C wire types   ──┼──▶ WS-D glue+handlers ─┼──▶ WS-J e2e+docs
WS-G ui shapes    ──┘    WS-E cli            │
                         WS-F run browser    │
                         WS-H dag history    ┘
```

### WS-A — Model + engine: epilogue, restore semantics, validator budget

**Files:** `src/workflow/model.rs` (+ sweep), `src/workflow/engine/mod.rs`,
`engine/graph.rs`, `engine/schedule.rs`, `engine/complete.rs`,
`engine/expand.rs`, `engine/tests_support.rs`.

**Slim posture:** unconditional. The Phase 1 pure-layer grep test (no
`App`/`surrealdb`/`ratatui` in `src/workflow/{model,engine,layout,tier}`) must
still pass.

**Delivers, step 1a (shapes only, tree compiles):**

- `RunNode.restored_from: Option<RestoredRef>` where
  `RestoredRef { run: RunId, node_key: NodeKey, checkpoint_seq: u64 }` — the
  pure-layer mirror of the `run_node.restored_from` column (the store maps it
  to the checkpoint record id; the engine never sees a DB id).
- `RestoredSeed { node_key: NodeKey, payload: serde_json::Value, summary:
  String, artifact_paths: Vec<String>, digest: String, source: RestoredRef }` —
  what the handler hands to materialisation per restored node.
- `RunGraph.epilogue: Option<EpilogueState>` with
  `EpilogueState { node: RunNodeIdx, phase: EpiloguePhase }`,
  `EpiloguePhase { Pending, Running, Done, GaveUp }`.
- `StoreWrite::RunSummary { run, kvdag_version, text, outcome, highlights,
  open_gaps, per_node, token_estimate, generated_by_path: Option<InstancePath> }`.
- `StoreWrite::InterrogationStarted { id: InterrogationId, run, path,
  source_session_id, forked_session_id: Option<String>, transcript_path:
  Option<String>, cwd, pane_id, reconstructed: bool, seeded_from_seq:
  Option<u64>, note, started_at_unix_ms }` and
  `StoreWrite::InterrogationUpdate { id, forked_session_id: Option<String>,
  ended_at_unix_ms: Option<u64> }` (one update shape covers both the
  session-id learn and the end stamp; §4 D7).
- `StoreWrite::RunEvent` gains `at_unix_ms: u64` (§4 D14 — every existing
  producer stamps it at construction; the sweep is mechanical because every
  producer already has a clock in scope).
- `StoreWrite::RunNode` gains `restored_from: Option<RestoredRef>`.
- `WorkflowEvent::RunSummarized { text_len: usize, outcome: String }`,
  `WorkflowEvent::InterrogationStarted { id, path }`,
  `WorkflowEvent::InterrogationEnded { id, path }` (the app-side emitter
  re-reads the full projections, matching how `NodeUpdated` works today).
- `RunEventKind` gains `Summary` (journal spelling `"summary"`) — requires the
  `0004` migration to extend `run_event.kind`'s `ASSERT` list (§4 D15).
- No new `EngineInput` for restore: restored nodes are materialised, not
  applied (§4 D3). One new input `EngineInput::EpilogueReport { result:
  RawJson }`-shaped? **No** — the summariser self-reports through the existing
  `NodeSelfReport` path with its own node token; no new input is needed. The
  only new input is **none**. (Stated so nobody adds one.)

**Delivers, step 2a (behaviour):**

- **Restore materialisation.** `RunGraph::materialise_with_restored(kvdag,
  run_id, tier, assignments, restored: &[RestoredSeed]) -> RunGraph`; the
  existing `materialise_with` becomes a thin wrapper passing `&[]` (same
  pattern as Phase 2's `materialise`/`materialise_with` split, for the same
  reason — no mid-step signature break). A seeded node starts
  `status: Restored`, `result: Some(NodeResult { payload, summary,
  artifact_paths, digest, evidence: Evidence::Restored })`,
  `succession: Some(Succession::Satisfied)`, `binding: None`,
  `restored_from: Some(source)`, `started_at/ended_at: Some(now)` (the restore
  instant, not the source run's — §4 D4). `Engine::apply(Start)`'s existing
  settle then fires the restored nodes' outbound edges with zero new engine
  code — that is the payoff of seam §0.2.
- **Epilogue.** After `finish` (`engine/mod.rs:1586-1603`) sets a terminal
  status in `{Succeeded, Failed}` and the engine was configured with
  `EngineConfig.summary_enabled` (default true; `Cancelled` never summarises —
  §4 D2), the engine appends one summariser `RunNode`: reserved instance path
  `.summary` (§4 D5), `kind: Internal`, `runner: Agent` (or the configured
  override argv — §4 D2), demand `Light`, assignment resolved from the run's
  tier, a karvex-authored prompt spec (below), status `Ready`,
  `epilogue: Some(..)`. It emits `NodeCreated` for it, so the DAG view shows
  it live. `settle`'s status gate (`bails unless Running|Paused`) gains one
  epilogue-aware branch: while `epilogue.phase` is `Pending|Running`, node-level
  inputs for the epilogue node (`NodeSelfReport`, `TurnEnded`, `AgentStatus`,
  `PaneExited`, `Tick`) are processed and settled **without** re-deciding the
  run status — the run's terminal status is already final and `finish` is never
  re-entered. The branch extends to `admissions()` too: the epilogue node must
  be admitted although the run status is terminal — the `settle` early-return
  for non-`Running|Paused` runs (`engine/mod.rs:1230-1232`) is exactly why
  this branch has to exist, and a test pins that a post-`finish` epilogue node
  actually reaches `admissions()`. Epilogue completion validates against the
  built-in summary schema (below); acceptance emits `StoreWrite::RunSummary` +
  a `StoreWrite::RunEvent { kind: Summary }` journal entry (the producer for
  the new kind) + `WorkflowEvent::RunSummarized` + `ClosePane` for the
  summariser pane, and sets `phase: Done`. The failure ladder is bounded: one corrective re-prompt
  (the existing `complete.rs` path), then `GaveUp` — journalled as
  `RunEventKind::Error` with `{"reason": "summary_failed"}`, notified once,
  pane closed, **run status untouched**. A `PaneExited`/`SpawnFailed` before a
  result also lands in `GaveUp`. `Engine::epilogue_pending() -> bool` is the
  app-facing accessor that keeps the tick alive (WS-D).
- **Built-in summary schema and prompt.** A `pub(crate) fn summary_output_schema()
  -> serde_json::Value` in `engine/mod.rs`: object requiring `text` (string,
  `maxLength` 4000), `outcome` (string, `maxLength` 200), `highlights` /
  `open_gaps` (arrays of strings), `per_node` (array of objects requiring
  `node_key`, `verdict`, `one_liner`). A `summary_task_spec(&RunGraph,
  &Kvdag) -> EpilogueTaskSpec` pure function producing the fixed what-to-cover
  prompt text plus the per-node evidence block (path, status, attempts,
  succession, blocker, and each node's checkpoint `summary` — already ≤ 1,200
  chars each) that WS-D renders into the node dir. The prompt states the 4,000
  char budget explicitly; the schema enforces it.
- **`maxLength` in the schema subset validator.** `complete::check`
  (`complete.rs:630`) implements `type/required/properties/items` only. Add
  `maxLength` for strings (violation message names the field and the limit).
  Without it the summary budget is a hope, not a schema property — and 03 §7
  requires "over-budget output fails schema validation and is retried once".
- **The two durability fixes (§0.6).** `expand.rs`: `Truncated` carries the
  proposing node's `expand_max` so `limit_value` is present for every
  rejection kind (write path `expand.rs:694-742` loses its
  `ExpandMax => None` arm; the live report in `app/workflow.rs` needs no
  change — it reads the same payload). `RunEvent.at_unix_ms` per step 1a.

**Tested (table-driven, no DB, no PTY):** restored node fires
`Sequence`/`Data`/`Conditional` edges and `Skipped` propagation interacts
correctly (a conditional-false downstream of a restored node still dies); a
restored node is excluded from ready-set admission and never spawns; a diamond
with 2 of 4 nodes restored runs only the other 2; `run_terminal_ready` holds
with restored nodes present; epilogue appends exactly one node with path
`.summary` and it never counts toward `nodes_total`-style user counts (§4 D5);
a summariser result over 4,000 chars fails `maxLength`, consumes the one
re-prompt, then `GaveUp` without touching run status; summariser `PaneExited`
pre-result → `GaveUp`; a `Cancelled` run appends no epilogue; `CancelRun`
arriving mid-epilogue closes the summariser pane and lands `GaveUp`;
`Truncated{ExpandMax}` journal payload contains `limit_value == expand_max`
(regression for §0.6a); every `StoreWrite::RunEvent` producer stamps a nonzero
`at_unix_ms` (grep-style test over a full engine run's effects); digest of a
result is unchanged by the presence of `restored` metadata (restore
compatibility guard, mirroring WS-B's Phase 2 digest-identity test).

### WS-B — Store: writers, digest compatibility, retention wiring, sweep

**Files:** `src/workflow/store/mod.rs`, `store/queries.rs`, `store/records.rs`,
`store/error.rs`, `store/migrations/0004_journal_time_and_interrogation.surql`,
`store/tests.rs`.

**Slim posture:** entirely behind `#[cfg(feature = "workflow")]`.

**Delivers:**

- **Migration `0004_journal_time_and_interrogation.surql`** (§4 D15), seven
  statements (§8 WS-B: the seventh — an explicit backfill
  `UPDATE workflow SET pruned_runs = 0 WHERE pruned_runs = NONE;` — was found
  by WS-B's own migration test: `DEFAULT` applies to new rows only, and
  pre-0004 rows made the first prune's counter bump fail on NONE+int; 0002's
  own backfill UPDATEs were the precedent this enumeration missed): (1) `DEFINE FIELD OVERWRITE at ON run_event TYPE datetime;`
  (drops `DEFAULT time::now()` — the second clock, per the 0002 `started_at`
  precedent); (2) `DEFINE FIELD OVERWRITE forked_session_id ON interrogation
  TYPE option<string>;` (Claude allocates the forked id at fork time unless
  the pre-assign combo verifies — §4 D7 — so the record must be creatable
  before the id is known); (3) `DEFINE FIELD OVERWRITE kind ON run_event TYPE
  string ASSERT $value IN [...existing list..., "summary"];`;
  (4) `DEFINE FIELD OVERWRITE kvdag_node ON run_node TYPE
  option<record<kvdag_node>>;` — the epilogue node is a `run_node` row with
  **no kvdag definition behind it**, and 0001 declares the field non-optional
  (`0001_init.surql:133`; non-`Option` in `RunNodeRow` at `records.rs:147`;
  both existing writers resolve it by key), so without this the epilogue row
  cannot be written at all; the row structs and decoders go `Option` in the
  same change; (5) `DEFINE FIELD pruned_runs ON workflow TYPE int DEFAULT 0;`
  — the `workflow` table is SCHEMAFULL, so §4 D12's counter write fails
  without the column; (6) `DEFINE INDEX run_summary_version ON run_summary
  FIELDS kvdag_version, created_at;` — `run_summary` is the one never-pruned
  table (unbounded growth by design) and §4 D9's summary listing filters it
  through `kvdag_version.workflow` (M9 below), so the cross-record filter
  needs an index rather than a table scan that degrades with history.
  **Invariant on (4):** only the reserved `.summary` path may ever write a
  NULL `kvdag_node` — `write_epilogue_node_created` asserts the reserved path
  before binding `NONE`, and a store test proves a non-reserved-path create
  with no kvdag node is rejected with `StoreError::Invariant`, so the
  loosened column can never be abused by an ordinary node write. Registered
  in `MIGRATIONS` (`store/mod.rs:98-108`). A test applies `0004` over a
  `0001..0003` database (the `open_with_migrations` prefix helper exists for
  exactly this).
- **Write arms** for the new `StoreWrite` variants in `write()`
  (`store/mod.rs:822`): `write_run_summary` (INSERT with `generated_by`
  resolved from `(run, path)` → `run_node` id, `None` tolerated),
  `write_interrogation_started` / `write_interrogation_update`, the
  `restored_from` bind in `write_run_node` and `materialise_run_nodes`
  (resolving `RestoredRef` → the source checkpoint's record id via
  `(run, node_key, seq)` lookup; a pruned-away checkpoint at write time is a
  decode-side `None`, never an error), and the explicit `at` bind in
  `write_run_event`. Plus, for the epilogue (the store half of §4 D5 and B1):
  a dedicated `write_epilogue_node_created` (creates the `.summary` `run_node`
  with `kvdag_node: NONE` — neither existing create path can, both resolve a
  kvdag node by key), and reserved-path filtering in `refresh_nodes_done`
  (`store/mod.rs:1796`) and `refresh_run_node_counters` (`:1824`) so
  `.`-prefixed rows never count toward `nodes_total`/`nodes_done`.
  `parse_run_event_kind` (`queries.rs:995-1021`) gains the `"summary"` arm —
  it is a closed match, and an unparseable kind on the read path is exactly
  the 0.10.2 P1 class.
- **Restored-node persistence is complete at create.** `materialise_run_nodes`
  today writes only label/depth/status/model/effort/demand/assignment_reason
  (`store/mod.rs:1502-1506`) and relies on later engine updates — which a
  `Restored` node never produces. For seeded nodes it therefore persists the
  full terminal shape up front: `status: "restored"`, `evidence: "restored"`,
  `succession: "satisfied"`, `started_at`/`ended_at` (the restore instant,
  app clock), and `restored_from`; and it re-persists the seed as the restored
  node's own seq-1 `result` checkpoint in the **new** run, so the new run's
  durable projection is self-contained and survives later pruning of the
  source run. The re-persisted checkpoint carries `schema_valid: true` for a
  digest-equal restore and `schema_valid: false` for an `allow_changed`
  cross-version restore — the seed was never validated against the *target*
  version's schema, and `false` correctly blocks onward restore of
  unvalidated data through `find_restorable_checkpoints`' `schema_valid`
  filter. Its `kvdag_version` is the **new** run's version, keeping the
  `checkpoint_lookup` index coherent (a checkpoint is addressed by the
  version it lives under, with provenance in `restored_from`).
- **Queries:** `list_run_summaries(workflow: Option<&WorkflowId>, limit)
  -> Vec<RunSummaryRecord>` newest-first (per-run `get_run_summary` exists).
  **The workflow filter cannot go through `run_summary.run`** — that
  reference *dangles* by design once the run is pruned, which is exactly the
  row the browser most needs listed. The surviving route is
  `WHERE kvdag_version.workflow = $workflow`: `kvdag_version` rows are never
  pruned and carry `workflow` (`0001_init.surql:32`), and the 0004 index
  (`run_summary_version`) makes the cross-record filter cheap on an
  unbounded table. Write it as the two-step form the index accelerates —
  `SELECT VALUE id FROM kvdag_version WHERE workflow = $w`, then
  `SELECT * FROM run_summary WHERE kvdag_version IN $ids ORDER BY created_at
  DESC` — not as a per-row link traversal, which would walk the record graph
  per summary instead of hitting the index. Each record carries `run_pruned: bool` computed by
  checking whether the referenced `workflow_run` row still resolves (one
  batched lookup, not N).
  `restore_source(run, selectors) -> Vec<CheckpointRecord>` wrapping the
  existing `find_restorable_checkpoints` and additionally returning, per
  checkpoint, the source node's `kvdag_node` identity so the handler can
  digest-compare. `run_summaries_for_context(workflow, limit)` — the injection
  feed, excluding the run being started. `list_runs` grows the cross-workflow
  form §4 D9 needs: `list_runs(workflow: Option<&WorkflowId>, limit)` — today
  it is per-workflow only (`queries.rs:378`) — with the workflow *name* joined
  in one batched lookup to fill `WorkflowRunInfo.workflow_name`.
  `list_interrogations(run) -> Vec<InterrogationRecord>` — no SELECT for
  `InterrogationRow` exists today; WS-H's `load_historical_run` and the
  interrogate handler's active-interrogation check both consume it.
- **The durable node projection gains the columns the writer already persists
  but the reader drops** (M2 — this is a live instance of the P1 class):
  `transcript_path` is written at `store/mod.rs:1765` but absent from
  `RunNodeRecord` (`queries.rs:100-147`) and `run_node_record` (`:908-947`);
  it and the new `restored_from` are added to both, and WS-D maps them in
  `wire_run_node_record` (`app/api/workflows.rs:1985-2033`). Without this,
  historical interrogation after a server restart always answers
  `transcript_unavailable` no matter what is on disk.
- **Digest helpers** (§4 D11): `node_compat_digests(&KvdagNodeRow) ->
  (String, String)` = `sha256(prompt_template)` and
  `sha256(canonical(output_schema))`, reusing `complete::canonical` via a
  pure re-export in `model.rs` (the store must not depend on `engine/`
  internals; `canonical` moves to `model.rs` in step 1a with a re-export shim
  left in `complete.rs`). **No digest columns are added** — both inputs live on
  immutable `kvdag_node` rows and are recomputed on demand; a stored digest
  would be a cache with an invalidation story for no measurable win at ≤ tens
  of nodes per version.
- **Retention wiring** (§4 D12): `prune_run_history` gains its first caller —
  WS-D invokes it post-run-close; the store side adds the workflow-level
  journal write 03 §9 requires (a `run_event` cannot outlive its run, so the
  prune record goes on the *surviving newest run* of the workflow? No — 03 §9
  says "journalled at the workflow level": add a `pruned_runs: int` counter
  bump plus `updated_at` refresh on the `workflow` row, and a
  `tracing::info!` span; a dedicated journal table is over-build for a
  counter. Stated as §4 D12's decision).
- **Orphan sweep** (§4 D13): `mark_interrupted_runs(now_unix_ms) -> u64` —
  one UPDATE setting `status = "failed"`, `failure = { reason: "interrupted",
  detail: "server restarted while the run was live" }`, and `ended_at` bound
  from the caller-supplied app clock (`time::now()` here would reintroduce the
  store-flush second clock §4 D14 exists to kill), on every `workflow_run`
  with status in `["pending","running","paused"]`,
  plus the matching `run_node` sweep (non-terminal node statuses →
  `"cancelled"`, evidence untouched). Called once per store open, before any
  read (`app/workflow_store.rs` open path, WS-D). Safe because the store's
  exclusive `LOCK` guarantees no other server is executing those runs, and the
  current server opens the store before it can start a run.
- **Field-for-field durability tests** (§4 D16 — the 0.10.2 defect-class
  guard, named explicitly): for each of `run_summary`, `interrogation`, a
  restored `run_node`, and a `context_runs`/`restore_from`-bearing run row —
  write through the store API, read back through the *production* read path
  (`get_run_summary`, `list_run_summaries`, `list_run_nodes`,
  `run_record`), and assert **every field individually** equals what was
  written, including `at_unix_ms` equality between the value stamped by the
  producer and the value decoded after a simulated reload. A test that
  round-trips the struct wholesale hides a field the decoder dropped; these
  assert per field, by name, so a new column that decode forgets fails with
  the field's name in the assertion message.

**Tested (beyond the above):** `list_run_summaries` orders newest-first,
respects limit, flags pruned, and — the M9 pin — still returns a pruned run's
summary when filtered by workflow (the `kvdag_version.workflow` route; a
`run`-reference filter would silently drop exactly these rows); summary write is idempotent-rejected on the
UNIQUE `run_summary_run` index (second write for a run errors, surfaced not
panicked); `restore_source` returns only `kind = "result"`,
`schema_valid = true`, latest-seq-per-node rows; `mark_interrupted_runs`
leaves terminal runs untouched and is a no-op on second call; prune after
interrogation + summary still passes the Phase 1 no-dangling-reference tests
(they exist — re-run them against rows created through the *new writers*
rather than raw-SQL seeds, which is what the Phase 1 tests used).

### WS-C — Wire surface

**Files:** `src/api/schema/workflows.rs`, `src/api/schema.rs`,
`src/api/schema/response.rs`, `src/api/schema/events.rs`,
`src/api/subscriptions.rs`, `src/api/mod.rs`, `src/api/server.rs`,
`docs/next/api/herdr-api.schema.json` (regenerated).

**Slim posture:** unconditional, zero `use crate::workflow::*`, one canonical
artifact value on both feature legs — unchanged Phase 1 rules.

**Delivers (all additive; §4 D20 — no `PROTOCOL_VERSION` bump):**

| Addition | Shape |
|---|---|
| `Method::WorkflowNodeInterrogate` → `workflow.node.interrogate` | `WorkflowNodeInterrogateParams { run_id, path, mode: WorkflowInterrogationMode (#[serde(default)]), note: Option<String> }` |
| `WorkflowInterrogationMode` | `#[serde(rename_all = "snake_case")]` `Resumed` (default) \| `Reconstructed` |
| `ResponseResult::WorkflowNodeInterrogated` | `{ interrogation: WorkflowInterrogationInfo }` |
| `WorkflowInterrogationInfo` | `{ id, run_id, path, source_session_id, forked_session_id: Option<String>, pane_id: Option<String>, reconstructed: bool, transcript_path: Option<String>, cwd, started_at_unix_ms, ended_at_unix_ms: Option<u64>, note }` |
| `Method::WorkflowSummaryGet` → `workflow.summary.get` | `WorkflowRunTarget` (reused) |
| `ResponseResult::WorkflowSummaryGet` | `{ summary: Option<WorkflowRunSummaryInfo> }` — `None` is "not written", never an error |
| `Method::WorkflowSummaryList` → `workflow.summary.list` | `WorkflowSummaryListParams { workflow_id: Option<String>, limit: Option<u32> }` |
| `ResponseResult::WorkflowSummaryList` | `{ summaries: Vec<WorkflowRunSummaryInfo> }` |
| `WorkflowRunSummaryInfo` | `{ run_id, workflow_id, workflow_name, version_id, text, outcome, highlights: Vec<String>, open_gaps: Vec<String>, per_node: Vec<WorkflowSummaryNodeLine>, token_estimate: u32, generated_by_path: Option<String>, created_at_unix_ms, run_pruned: bool }` |
| `WorkflowSummaryNodeLine` | `{ node_key, verdict, one_liner }` |
| `WorkflowRunParams` | `+ restore_from: Option<WorkflowRestoreRequest>`, `+ include_prior_summaries: Option<bool>` (both `#[serde(default)]`; absent `include_prior_summaries` means true — §4 D21) |
| `WorkflowRestoreRequest` | `{ run_id, nodes: Vec<String>, allow_changed: bool (#[serde(default)]) }` |
| `ResponseResult::WorkflowRunStarted` | `+ restore: Option<WorkflowRestoreReport>` (`#[serde(default)]`) |
| `WorkflowRestoreReport` | `{ restored: Vec<String>, skipped: Vec<WorkflowRestoreSkip> }` |
| `WorkflowRestoreSkip` | `{ selector, reason: WorkflowRestoreSkipReason, message }` with reason enum `definition_changed` \| `no_checkpoint` \| `payload_truncated` |
| `WorkflowRunListParams.workflow_id` | `String` → `Option<String>` (`None` = all workflows; additive — every existing caller sends it) |
| `WorkflowRunInfo` | `+ workflow_name: String (#[serde(default)])`, `+ context_runs: Vec<String> (#[serde(default)])`, `+ restore_from_run: Option<String>` |
| `WorkflowRunNodeInfo` | `+ transcript_path: Option<String>`, `+ restored_from: Option<WorkflowRestoredFrom>` |
| `WorkflowRestoredFrom` | `{ run_id, node_key, checkpoint_seq: u64 }` |
| `EventKind::WorkflowRunSummarized` → `workflow.run.summarized` | `EventData::WorkflowRunSummarized { run_id, summary: WorkflowRunSummaryInfo }` |
| `EventKind::WorkflowInterrogationStarted` → `workflow.interrogation.started`, `EventKind::WorkflowInterrogationEnded` → `workflow.interrogation.ended` | `EventData::{..} { interrogation: WorkflowInterrogationInfo }` |
| `Subscription::{WorkflowRunSummarized, WorkflowInterrogationStarted, WorkflowInterrogationEnded} {}` | + `KNOWN_EVENT_KINDS` + the exhaustive arm in `src/api/subscriptions.rs:299-328` |
| `request_changes_ui` (`src/api/mod.rs:80-88`) | `+ WorkflowNodeInterrogate` only — it spawns a pane. `summary.get`/`summary.list` are reads and stay out |
| `api_method_name` (`src/api/server.rs:440-454`) | three new arms (exhaustive match enforces) |

**Error codes** (registered here as constants in WS-D's handler file, named
here because they are contract): `workflow_transcript_unavailable` (carries the
node path and the stat-failed path in the message; the *reason* — missing
transcript vs missing cwd — is in the message text, matching the existing
single-code style), `workflow_run_pruned`, `workflow_restore_unknown_selector`,
`workflow_interrogation_active` (§4 D7 — one interrogation pane per source node
at a time).

A dedicated API method for listing a run's interrogations is explicitly
**not** in Phase 3: the TUI reads in-process (WS-B's `list_interrogations`),
and external clients see `WorkflowInterrogationInfo` through the two events.

**Naming guard:** every identifier above is appended to the hand-maintained
list in `no_new_workflow_api_identifier_uses_banned_ui_surface_words`
(`workflows.rs:1196-1463`) under a `// Phase 3 additions` comment. None of the
names contains a banned UI-surface word — `summary`, `interrogation`,
`restore` are runtime vocabulary. (`history` is deliberately absent from the
API: the neutral nouns are `run`, `summary`, `interrogation`.)

**Tested:** serde round-trip per new type; `WorkflowRunListParams` decodes both
with and without `workflow_id` (back-compat pin); `include_prior_summaries`
absent ⇒ `None` ⇒ documented-true default (pinned in a test so the default
lives in exactly one place, the handler); regenerated artifact committed and
green on both feature legs; the naming-guard list extended; the
`not_implemented` sweep (`src/app/api/workflows.rs:2189`) extended with the
three new methods (WS-D's file — WS-C adds variants, WS-D extends the sweep,
same split Phase 2 used).

**The step-1c struct-literal sweep (§8 E-3 — added during the build; this
section originally omitted the sweep its own changes force).** WS-C's params
and info changes break exhaustive literals and matches in files other streams
own, and the §2 gate ("every step compiles, both legs") is absolute, so WS-C
sweeps them all in 1c as a scoped ownership exception on the 1a precedent —
literals/arms only, re-grepped immediately before sweeping, noted in the step
report. Three site families: (1) `WorkflowRunParams`/`WorkflowRunListParams`
literals (~17 sites: `cli/workflow.rs`, `input/workflow_launch.rs`,
`app/api/workflows.rs`) — `restore_from: None, include_prior_summaries: None`,
`workflow_id: Some(..)`; (2) `WorkflowRunInfo`/`WorkflowRunNodeInfo`/
`WorkflowRunStarted` literals in `app/api/workflows.rs` + `app/workflow.rs`
(~15 sites) — behavior-preserving placeholders (`String::new()`,
`Vec::new()`, `None`) that WS-D replaces with real projections in 2c, §4
D16's field-for-field wire tests being the net that catches a forgotten one;
(3) the exhaustive `EventData` match in `app/api/plugins/context.rs` — three
arms following the file's own `GrowthLimited` precedent.

### WS-D — Handlers, app glue, spawn binding

**Files:** `src/app/api/workflows.rs`, `src/app/api.rs` (dispatch arms),
`src/app/workflow.rs`, `src/app/workflow_store.rs`,
`src/workflow/binding/spawn.rs`, `src/workflow/binding/observe.rs`.

**Slim posture:** wire↔engine conversions and store calls stay behind
`#[cfg(feature = "workflow")]`; slim arms answer `workflow_unavailable`.

**Delivers:**

- **`workflow.node.interrogate` handler.** Resolve `(run, path)` from the live
  run or the durable projection (`stored_run` composition already exists at
  `workflows.rs:1340`). Preconditions, in order: **the node's `runner` is
  `Agent`** — resolved from the live definition, or the kvdag version row for
  a historical run (§8 E-14: the original gate, "has an `agent_session_id`",
  rested on a false premise — `workflow_spawn_plan` derives and persists a
  session id for EVERY node including Command runners, so id-presence gating
  would pass for a command node and hand `--resume` an id claude never
  created; a command node answers `workflow_transcript_unavailable` with the
  message saying it ran as a command, exactly as intended); no live
  interrogation already bound to this
  `(run, path)` (`workflow_interrogation_active`); source run row exists — a
  summary-only pruned run answers `workflow_run_pruned`; for `mode: resumed`,
  **stat the transcript path** (the hook-reported path when recorded, else the
  stored estimate — §4 D6) and stat the recorded `cwd`; either missing ⇒
  `workflow_transcript_unavailable` naming which one. For
  `mode: reconstructed`, require instead a stored `result` checkpoint for the
  node (else `workflow_transcript_unavailable` — nothing to seed from).
  On success: spawn per §4 D7, enqueue `StoreWrite::InterrogationStarted`,
  emit `workflow.interrogation.started` (interrogation events are emitted at
  their two call sites directly, consistent with C-4's exclusion of them from
  the refresh hook — see the §8 dead-variant note), answer
  `WorkflowNodeInterrogated`. A pane-spawn failure *after* the transcript and
  cwd verified answers the dedicated `workflow_interrogation_spawn_failed`
  code (§8 E-15 — reusing `transcript_unavailable` there would lie: the
  transcript is present, that is how we got as far as spawning). The reconstructed path spawns a *fresh* Claude
  seeded via a karvex-authored task file (checkpoint summary + payload +
  the node's original `task.md` if its node dir survives) and the pane title
  carries `reconstructed` (§4 D7).
- **Interrogation lifetime glue** (`src/app/workflow.rs`).
  `WorkflowRuntimeState.interrogations: Vec<LiveInterrogation { id, run, path,
  pane, forked_session_id }>`. The existing `AppEvent::PaneDied` path
  (`app/api.rs:181`) and `App::close_pane` already route into workflow
  observation; the interrogation tracker hooks the same two call sites (no new
  `AppEvent` — §0's uncommitted-diff rule) and, on pane death/close, enqueues
  `InterrogationUpdate { ended_at_unix_ms }` + emits
  `workflow.interrogation.ended`. `reconcile_workflow_pane_bindings`
  (`:835-856`) is **not** extended to interrogations by accident — it sweeps
  run-node bindings only; a parallel `reconcile_interrogation_panes` sweep in
  the same tick covers bulk tab/workspace closes. The forked session id, when
  not pre-assigned (§4 D7), is learned from the pane's agent-session report
  (the same source the sidebar session-name feature reads) and persisted via
  `InterrogationUpdate`.
- **Transcript-path read-back** (§4 D6): when a workflow pane's session report
  carries a transcript path (`observe.rs` gains the accessor; the report
  plumbing exists), update `NodeBinding.transcript_path` and enqueue the
  `StoreWrite::RunNode` refresh. One field, one write, closes §0.5.
- **Restore in `handle_workflow_run`** (`workflows.rs:578-741`). After the
  definition loads and before `create_run`: if `restore_from` is present,
  load the source run (absent row + surviving summary ⇒ `workflow_run_pruned`;
  absent both ⇒ `workflow_run_not_found`); `restore_source(run, selectors)`;
  resolve each selector against the **target version's** non-template node
  keys (unknown ⇒ `workflow_restore_unknown_selector` — a typo must not
  silently re-run a node the caller believed restored); per node compare
  `node_compat_digests` between source-version node and target-version node —
  equal ⇒ seed; differing ⇒ skip with `definition_changed` unless
  `allow_changed`; checkpoint payload carrying the store's
  `{"truncated": true}` stub ⇒ skip with `payload_truncated` (§4 D19), not
  restorable even with `allow_changed`; no valid checkpoint ⇒ `no_checkpoint`
  skip. Then: `NewRun` gains `restore_from` (persisted into the existing
  column) and `context_runs`; `materialise_with_restored` with the seeds;
  the response carries `WorkflowRestoreReport`. An all-skipped restore is a
  **successful run start with a fully-populated `skipped` list** — the report
  is the surface, mirroring how a rejected expand proposal is a success
  carrying the rejection.
- **Prior-summary injection** (§4 D21). At run start, when
  `include_prior_summaries` resolves true: `run_summaries_for_context(workflow,
  workflow.history_context_runs)` (new config field, default 3 — §4 D22);
  record their run ids in `NewRun.context_runs`; write
  `<run dir>/context/prior-runs.md` (one `## Run <short-id> — <outcome>`
  section per summary: text, highlights, open gaps) via a new
  `spawn::write_run_context` helper; and pass the context flag into the spawn
  plan so `TaskDocument` renders its new optional `## Prior runs` section —
  two lines: the file path and "read it if the task benefits from history"
  (§4 D21 keeps per-node token cost at ~2 lines). `workflow_spawn_plan`
  (`app/workflow.rs:1496-1660`) threads it through.
- **Epilogue glue.** `drive_workflow_spawns` already spawns whatever
  `admissions()` yields; the epilogue node arrives through the same path with
  one special-case in the spawn plan: **its task, prompt, runner AND argv come
  exclusively from the engine's `summary_task_spec()`** (§8 D-1: the env
  override is read once, into `EngineConfig.summary_command` via the single
  `workflow_runtime_config` source — §8 E-16 — never at the spawn site; a
  malformed value warns at config-read, DISABLES summaries for the server,
  and surfaces once through the notice path at the first run start that would
  have summarised — §8 E-11; falling back to claude is forbidden, and a
  hard-fail would let an env typo brick the server). The app keeps the
  workflow tick alive while `is_live() || epilogue_pending() ||
  !interrogations.is_empty()` — the third term (§8 E-8) exists because
  interrogations outlive runs (D8) and the bulk-close reconcile sweep needs a
  cadence with no run live; `is_live()` itself is unchanged so nothing else
  re-opens. The live wire projection uses `EPILOGUE_DEMAND` for the
  `.summary` node's demand (§8 E-13 — the last definition-derived field; any
  field derived from the definition is wrong for a definition-less node). **And the run-start guard gains the
  same disjunct** (M7): a `Succeeded` run is not `is_live()`
  (`app/workflow.rs:386-390`), so without it a `workflow.run` arriving during
  the epilogue would pass the in-flight check, `start()` a fresh engine,
  clear `node_tokens`, and silently orphan the summariser's report — the
  summary would be lost with no surface. `handle_workflow_run` refuses while
  `is_live() || epilogue_pending()` with `workflow_run_in_flight` and a
  message naming the pending summary. **The guard alone is not enough**: an
  accepted summary can be sitting in `pending_writes` after the epilogue is
  `Done` — and `start()` clears `pending_writes` outright
  (`app/workflow.rs:414`), so the enqueued `StoreWrite::RunSummary` would die
  with the engine swap even though the guard passed. `start()` therefore
  **flushes before replacing**: it drains the previous run's `pending_writes`
  to the store task (the existing `take_pending_writes` →
  `drain_workflow_store_writes` handoff, after which the store task owns them
  and the engine swap cannot touch them) before resetting any per-run state;
  it never discards unflushed writes. Chosen over refuse-until-flushed
  because a refusal keyed on queue emptiness can block indefinitely under a
  backlogged store task, while the handoff is immediate and deterministic.
  Silent summary loss is the one outcome ruled out, and it is tested: enqueue
  a `RunSummary` write, call the next `start()`, assert the summary lands in
  the store. On `RunSummarized`, the emitter
  re-reads the stored summary and publishes the full
  `workflow.run.summarized` event.
- **`workflow.summary.get` / `workflow.summary.list` handlers** — store reads,
  wire mapping, nothing else. `workflow.run.get` on a pruned run whose summary
  survives answers `workflow_run_pruned` with a message pointing at
  `workflow.summary.get` — not the bare not-found — so the surviving surface
  is named instead of implied.
- **Retention + sweep wiring.** `mark_interrupted_runs()` on store open
  (`app/workflow_store.rs` open path, once, before first read);
  `prune_run_history(workflow, config.workflow.retention_runs)` fired
  fire-and-forget on the store task after a run reaches a closed status *and*
  its epilogue resolved (`Done`/`GaveUp`/absent) — never on a read path.
- **`run cancel` / `run list` `--json` parity fix** is CLI-side (WS-E) but the
  handler needs nothing; noted here so nobody adds a handler change for it.

**Tested:** handler tests with `AppState::test_new()` + kv-mem store:
interrogate on a command-runner node ⇒ `workflow_transcript_unavailable`, **no
pane exists afterwards** (the "never a silent pane" pin, asserted against the
workspace pane count); interrogate with a fabricated transcript file at the
expected path ⇒ spawn argv is exactly
`["claude", "--resume", <sid>, "--fork-session", ...]` and the transcript
file's bytes are untouched after the call (mutation pin at the seam CI can
reach — the real-fork non-mutation check is in the manual validation list,
§5); second interrogate on the same node while the first pane lives ⇒
`workflow_interrogation_active`; restore across versions: v1 run → author v2
with one node's prompt changed → restore both nodes ⇒ one restored, one
`definition_changed` skip, and with `allow_changed` ⇒ both restored; restore
from a pruned run ⇒ `workflow_run_pruned`; unknown selector ⇒ error, run not
created (no orphan `workflow_run` row — assert row count); the epilogue
spawns after user-graph failure as well as success; a `workflow.run` arriving
while the epilogue is pending is refused with `workflow_run_in_flight` and
accepted after `Done`/`GaveUp` (the M7 race, pinned); opt-out flag ⇒ no
`## Prior runs` section, no `context_runs`; the `not_implemented` sweep
extended **and** its feature-off companion
(`every_workflow_method_reports_workflow_unavailable_with_the_feature_off`,
`workflows.rs:2275`) extended with the three new methods; every new read path
covered by WS-B's field-for-field pattern at the wire layer too
(`WorkflowRunSummaryInfo` from a written summary equals the written values
field by field).

### WS-E — CLI

**Files:** `src/cli/workflow.rs`, `src/cli/spec.rs`, `src/cli/runtime.rs`,
`src/cli.rs`, `tests/cli/workflow.rs`, `tests/cli/mod.rs` (registration only
if the module line is missing).

**Delivers:**

```
kvx workflow node interrogate <run-id> <path> [--reconstructed] [--note <text>] [--json]
kvx workflow summary show <run-id> [--json]
kvx workflow summary list [<workflow>] [--limit N] [--json]
kvx workflow run start <name|id> ... [--restore-from <run-id>]
                                     [--restore <selector>]...
                                     [--restore-allow-changed]
                                     [--no-prior-summaries]
```

Grammar notes: `summary` is a new namespace under `workflow` (like `run` and
`node`), so `VERB_PATHS` (`src/cli/workflow.rs:33-48`) gains three paths and
the parity trio (`spec.rs:1459`) covers them. `--restore` is repeatable and
requires `--restore-from`; `--restore-from` without `--restore` means "restore
every compatible succeeded node of that run" (§4 D18 — the common case gets
the short spelling). `--restore-allow-changed` maps to `allow_changed`.
Human rendering: `run start` prints the restore report
(`restored: plan, implement · skipped: review (definition changed)`);
`summary show` prints outcome, text, highlights, gaps, per-node lines;
`summary list` prints one row per summary with a `(pruned)` marker;
`node show` gains `transcript:` (present/absent) and `restored from:` lines;
`node interrogate` prints the pane id and mode. All timestamps through the
existing `format_unix_ms` (UTC-labelled).

**The `--json` inconsistency is closed, not replicated** (§4 D17): every new
verb takes `--json` (raw envelope via `print_response`, the sibling
convention), **and** `run cancel`, `run list`, `node steer`, `node interrupt`,
`node restart`, and `workflow list` gain the flag in the same sweep — six
parsers plus their clap mirrors (`spec.rs:873-991`). One changelog line.

**Tested:** in-crate parser→`Method` tests per verb; parity trio green;
`--restore` repeatable with `=`-containing selectors; `run cancel --json` now
parses (regression pin naming the old behaviour); e2e verb behaviour in
`tests/cli/workflow.rs` (real binary, real server; inherits the macOS cfg from
`tests/cli.rs`).

### WS-F — Run browser overlay

**Files:** `src/ui/workflow_runs.rs` (new), `src/app/input/workflow_runs.rs`
(new), `src/config/model.rs`, `src/config/keybinds.rs`,
`src/app/input/navigate.rs` (keybind table + dispatch arm),
`src/ui/keybind_help.rs`, `src/main.rs` (sample-config line — last, §0),
`docs/next/website/src/data/config-reference.json`.
(`src/app/state.rs`, `src/ui.rs`, `src/app/input/{mod,modal,overlays}.rs`
shape/stub edits land in step 1b and are not touched here.)

**Delivers:** `Mode::WorkflowRuns` — Navigator's silhouette, the launcher's
mechanics (per the shipped precedent: `compute_*_view(app, area, carried)`
returning `default()` when the mode is inactive, pure geometry fn, stored
rects on `ViewState`, one `*_target_at` hit-test, `windowed_rows` list). The
overlay:

- **List** (left/topmost): one row per run — status glyph + colour (the DAG
  view's status vocabulary), workflow name, tier, started-at, `done/total`,
  and a `summary` marker; **plus one row per pruned summary** (from
  `workflow.summary.list`) rendered dimmed with a `pruned` tag. Data loads
  on open via the in-process dispatch pattern the launcher uses
  (`dispatch_api_request("tui.workflow.run.list", ...)` with `workflow_id:
  None`, then `"tui.workflow.summary.list"`), and refreshes on
  `workflow.run.*`/`workflow.run.summarized` event arrivals while open — no
  polling loop, so `src/app/runtime.rs` stays untouched. **The mechanism (§8
  C-4):** one line in `emit_workflow_event` (app/workflow.rs, WS-D's) calls
  `self.refresh_workflow_runs_overlay(kind)` after run-level emits only
  (RunStarted/RunUpdated/RunFinished/RunSummarized — never node-level, so the
  batch-coalesced RunUpdated bounds refresh frequency without a debounce
  flag); the method lives in WS-F's `input/workflow_runs.rs`, early-returns
  unless the browser is open, re-dispatches the two list loads, and
  re-anchors selection by `run_id`, never by index. Interrogation events are
  deliberately excluded — they change no list row.
- **Detail** (bottom strip, 4–6 rows): the selected run's args, limits,
  failure/blocker line, and its summary outcome + first highlight when one
  exists; for a pruned run, the summary text and the fixed line
  `history pruned — restore and interrogation unavailable` (03 §9's "with a
  reason", verbatim requirement).
- **Actions** (footer hints + keys): `Enter` — open the run in the DAG view
  (live run → the existing live path; closed run → the historical projection,
  WS-H); `R` — restore-all into a new run (confirm dialog reusing the
  `ConfirmClose` modal shape; disabled with the reason line for pruned runs
  and when a run is already in flight — the single-live-run guard
  `workflow_run_in_flight` is surfaced as the disable reason); `Esc` —
  `leave_modal`. Per-node interrogation is deliberately **not** a browser
  action — it lives on the node in the DAG view (WS-H), where the node is.
- **Keybind** `keys.open_workflow_runs`, default **`prefix+shift+b`**
  ("browse runs" — §8 E-7: the original `prefix+shift+r` collided with
  `reload_config`'s long-standing default, and the "collision-checked at
  build time" clause of A10 described a guard that did not exist; WS-F adds
  `default_keybinds_have_no_duplicate_chords` to make it real), registered
  through all six touch points + `config-reference.json`
  (`scripts/config_reference_check.py` is the gate, and the JSON description
  must match the Rust doc comment verbatim).

**Tested:** `AppState::test_new()` mode round-trip; geometry: rects partition
the popup, no overlap, tiny-area degradation without panic (the launcher's
test shapes); hit-test agreement with stored rects; pruned rows never map to
`Enter`/`R` targets; `config_reference_check.py` green.

### WS-G — Shared UI shape landing (step 1b) — then merges into WS-F

**Files (step 1b only, landed complete, never touched in parallel steps):**
`src/app/state.rs`, `src/ui.rs`, `src/app/input/mod.rs`,
`src/app/input/modal.rs`, `src/app/input/overlays.rs`.

**Delivers:** `Mode::WorkflowRuns` + membership in `mouse_motion_changes_view`
and `wants_ascii_input` (both pinned lists, `state.rs:1138,1160`, test at
`:2845`); `WorkflowRunsState` on `ViewState` (entries, selection, scroll,
rects, confirm sub-state); **the historical snapshot shapes (§8 C-1 — omitted
from this list originally, named only in §WS-H):** `HistoricalRunSnapshot
{ graph: Box<RunGraph>, workflow_name, interrogations:
Vec<HistoricalInterrogation> }`, `HistoricalInterrogation { id, path,
pane_id, reconstructed, ended }`, and `AppState.historical_run:
Option<HistoricalRunSnapshot>` with get/set/clear accessors beside
`run_graph` — landed inert, filled by WS-H in 2f; **the two stub files (§8
E-2/C-2 — WS-F's "(new)" attribution was imprecise):**
`src/ui/workflow_runs.rs` (a `compute_workflow_runs_view(app, area, carried)`
returning `default()` when the mode is inactive + an empty
`render_workflow_runs`) and `src/app/input/workflow_runs.rs` (key/mouse/paste
handler stubs mirroring the workflow_launch siblings, mouse returning true,
**Esc→leave_modal working in the stub** so the mode is never an input trap),
created by WS-G with all `mod`/`use`/dispatch wiring, bodies filled by WS-F
in 2e, signature changes routed through the design authority; the **app/mod.rs
headless mirrors (§8 E-4)**: the `AppState` literal gains
`historical_run: None` and `handle_non_terminal_key_headless` gains the
`Mode::WorkflowRuns` arm calling the same key-handler stub;
`DagViewState` additions for WS-H:
`historical: bool`, `interrogation_nodes: Vec<DagInterrogationView { id, path,
label, pane_id, rect, ended: bool }>` (own vec, own hit-test namespace — never
mixed into `nodes`); the render-dispatch arm in `src/ui.rs:465-497` (stub →
WS-F's renderer), the key-dispatch arm in `input/mod.rs:101-126`, the paste
arm, the `WORKFLOW_RUNS_ACTIONS` table in `modal.rs` (Esc→Close, Enter→Open),
and the mouse guard in `overlays.rs` (delegating to WS-F's handler). Stubs
compile and no-op until WS-F/WS-H fill them.

This is one short sequential step by one owner — the same trick Phase 2's
step 1b used and for the same reason: 22-variant `Mode` matches are exhaustive
in five files, and landing the variant late would serialise every UI stream.

### WS-H — DAG view: historical runs and detached interrogation nodes

**Files:** `src/ui/workflow_dag.rs`, `src/app/workflow_history.rs` (new — the
historical projection loader; its `mod` line in app/mod.rs is an E-4-class
mechanical add), `src/workflow/layout.rs` (one additive fn), **plus (§8 E-5,
granted during the build — the behaviors below live in pre-existing handler
code the original list omitted): `src/app/input/mod.rs` and
`src/app/input/overlays.rs`, scoped to the WorkflowDag-mode regions only**
(`handle_workflow_dag_key` + its helpers; the DAG mouse block). The
historical-snapshot clear (`set_historical_run(None)`) lives in the DAG Close
action arm immediately before `leave_modal` — never inside the generic
`leave_modal`, or a stale snapshot hijacks the next DAG open under C-1's
precedence rule. **Loader data contract (§8 C-3):** run + graph via
in-process wire dispatch of `workflow.run.get` (the launcher precedent —
inherits `stored_run`'s projection authority without touching WS-D's file);
interrogation rows via a direct store call to `list_interrogations` (the one
sanctioned exception — no wire method exists by design); WS-H owns the
empty-until-WS-B-lands seam and flips it itself.

**Delivers:**

- **Historical projection.** `app/workflow_history.rs`:
  `load_historical_run(&mut App, run_id) -> Result<..>` builds the
  `DagViewState` inputs from the durable projection (the same store reads
  `stored_run` composes) into a read-only snapshot stored on
  `AppState` (`historical_run: Option<Box<RunGraph>>`-shaped, landed in 1b),
  plus its interrogation rows. The DAG compute path renders whichever of
  live/historical is active; `historical: true` disables steer (`s` answers a
  notice), and `Enter` focuses a pane only when the node still has a live one
  (restored/historical nodes have none — the hint line drops `focus`).
  Restart/steer/interrupt need no special-casing beyond that: for a
  historical run — which is not the active run — the server answers with the
  existing not-the-active-run guard (`workflow_run_not_active`,
  `NO_ACTIVE_RUN_CODE`); `workflow_run_closed` covers only the just-finished
  run that is still the active one.
- **Interrogation affordance (§8 E-9 — the original two-step replaced during
  the build; it had no landed state carrier, and keying it off the toast slot
  was silently broken under terminal/system notice delivery):** `i` on a
  selected node calls `workflow.node.interrogate` (mode resumed); **`Shift+I`
  sends `mode: reconstructed`** — a distinct, explicit keypress, never a
  hidden second meaning of the same key. A `workflow_transcript_unavailable`
  answer to `i` raises the guide notice ("transcript unavailable — press
  Shift+I for a reconstructed session"); the same class of answer to
  `Shift+I` itself (nothing to seed from) is a plain refusal, no further
  escalation. Explicit-keypress escalation serves 00 Feature 3's "degraded
  path never presented as the original" strictly better than the two-step,
  and zero state means zero staleness. Footer hint carries `Shift+I` (first
  to drop on narrow terminals); keybind-help lists both keys (WS-J).
- **Detached interrogation nodes.** Interrogation panes render as boxes in a
  dedicated lane **below the deepest layer** of the graph band — computed by a
  new additive `layout::detached_lane(area, count) -> Vec<LayoutRect>` (pure;
  the layered algorithm is untouched, so an edgeless interrogation box can
  never be confused with a root: interrogations are not `RunGraph` nodes at
  all, so the layered algorithm's edgeless-node-goes-to-layer-0 default
  (`assign_layers`, `layout.rs:228`) never applies to them). Box: `⌕`
  glyph, `interrogate · <source path>`, teal border while live, `overlay0`
  when ended; selected/click → focus its pane; hit-testing through the
  separate `interrogation_nodes` rects. The lane allocates zero rows when
  empty, preserving every pinned geometry number (the banner precedent,
  frozen-interface rule 10 of the Phase 2 plan).
- **Summariser visibility:** the `.summary` epilogue node arrives as a normal
  `NodeCreated` and renders like any node (`Internal` needs no special glyph;
  its label is `summary`). The run-status header line appends
  `· summarising…` while the epilogue is pending and `· summary failed` on
  `GaveUp` (peach), so the epilogue state is visible without a new band.

**Tested:** historical snapshot renders a stored run byte-stably (fixture
graph → same rects across two computes); detached lane geometry: zero rows
when empty (pinned numbers unchanged — re-assert the existing pins), rects
don't overlap the graph, hit-test agreement; `i`-flow state machine
(unavailable → offer → reconstructed) as a pure key-handler test; steer
disabled on historical.

### WS-J — End-to-end and docs

**Files:** `tests/workflow_headless.rs`, `tests/fixtures/workflow/` (new
fixtures), `docs/next/website/src/content/docs/{,ja/,zh-cn/}workflows.mdx`,
`docs/next/CHANGELOG.md`.

**E2E scenarios** (stub `runner: command` fixtures throughout; env scrub per
the Phase 2 block — `KARVEX_WORKFLOW_DB_PATH` etc.; the summariser runs under
`KARVEX_WORKFLOW_SUMMARY_COMMAND` pointing at a script that writes a valid
summary `result.json` and calls `kvx workflow node complete` — §4 D2):

1. **Summary lifecycle.** A two-node run succeeds → `workflow.run.finished`
   arrives **before** `workflow.run.summarized` (ordering pin — the 0.10.2
   event-ordering fix made the stream globally ordered, lean on it);
   `workflow.summary.get` returns the written summary; a second run of the
   same workflow has `context_runs` naming run 1 and its root node's
   `task.md` contains the `## Prior runs` section; a third run with
   `--no-prior-summaries` has neither. A run whose summary command writes an
   over-budget `text` ends with `summary.get` returning `None`, the run still
   `succeeded`, and an `error` journal entry — the "summariser can never
   wedge or flip a run" guarantee, as an e2e.
2. **Restore.** Run 1 (diamond) succeeds → `run start --restore-from <run1>
   --restore plan` → `workflow.run.started` response carries
   `restored: ["plan"]`; the `plan` node's `workflow.node.*` event stream
   shows `restored` and **no pane** was created for it (pane-count
   assertion); the downstream node's `task.md` `## Inputs` carries run 1's
   payload; the run succeeds. Then author v2 changing `plan`'s
   `prompt_template` → restore ⇒ `skipped: definition_changed`;
   `--restore-allow-changed` ⇒ restored. (The "restore into a *different*
   kvdag version" test required by 05 §5.3, both polarities.)
3. **Interrogation refusal is never a silent pane.** `node interrogate` on a
   command-runner node → `workflow_transcript_unavailable` on stdout,
   exit non-zero, and `pane.list` shows no new pane. (The resumed-fork happy
   path cannot run in CI — no `claude` — and is in the manual checklist, §5.)
4. **Pruned-run surface.** With `retention_runs` forced low via config, run
   N+1 prunes run 1 → `workflow.summary.list` still returns run 1's summary
   with `run_pruned: true`; `run start --restore-from <run1>` ⇒
   `workflow_run_pruned`; `workflow.run.get` on it ⇒ `workflow_run_pruned`
   too (the row is gone; the error's message points at `workflow.summary.get`
   as the surviving surface).
5. **Restart-fidelity field coverage** (the B2 gap). The existing restart
   e2e's `run_shape` projection (`tests/workflow_headless.rs:1657-1717`)
   compares a shape that today omits exactly the field classes the 0.10.2 P1
   hit — timestamps, `growth_limited`, `label`/`inputs`. Extend it to include
   those **and** every Phase 3 field (`transcript_path`, `restored_from`,
   `context_runs`, `restore_from_run`, `workflow_name`, the summary linkage),
   asserted equal across a server restart — the e2e face of §4 D16.

None of these uses wall-clock races (the two known flaky styles —
`pane_graphics_stream` inactive-owner and `terminal metadata` TTL — are
timing-window tests; every wait here is event-stream-driven with the
existing subscription helpers).

**Docs.** `workflows.mdx` gains three sections — `## Run history and
summaries`, `## Restoring from a past run`, `## Interrogating a past node` —
plus `### The run browser` under the DAG-view section and the new CLI verbs in
`## Commands`; `ja/` and `zh-cn/` counterparts with matching heading outlines
(`just release-docs-check` is the gate). `config-reference.json` entries for
`workflow.history_context_runs`, `workflow.summary_enabled`,
`keys.open_workflow_runs` (WS-F owns the file write; WS-J verifies the doc
build). CHANGELOG entries per shipped surface.

---

## 2. Ordered Phase 3 workplan

Steps sharing a number run in parallel. Every step leaves the tree compiling
and `just check-slim` green.

| # | Step | Owner | Files | Delivers |
|---|---|---|---|---|
| 0 | **Claude-flag verification spike** (manual, before or during step 1) | WS-D owner | none (throwaway session) | Verify on the installed `claude`: (a) `claude --resume <sid> --fork-session` from a cwd whose project dir holds the transcript; (b) whether `--session-id <new>` combines with `--resume --fork-session` (pre-assigns the forked id — §4 D7's preferred path); (c) whether the `SessionStart` hook payload carries `transcript_path` for resumed sessions. Record results in the build log; D6/D7 have designed fallbacks for every "no" |
| 1a | Model shapes + sweep | WS-A | `model.rs`, sweep files (§1), `engine/tests_support.rs` | every new type/variant of WS-A step 1a; `canonical` moved to `model.rs` with shim; tree compiles |
| 1b | UI shapes + mode stubs | WS-G | `state.rs`, `ui.rs`, `input/{mod,modal,overlays}.rs` | `Mode::WorkflowRuns`, `WorkflowRunsState`, `DagViewState` additions, stub arms |
| 1c | Wire surface | WS-C | schema files + artifact | the full §WS-C table, regenerated artifact, naming guard |
| 2a | Engine behaviour | WS-A | `engine/*` | epilogue, `materialise_with_restored`, `maxLength`, `Truncated.limit_value` fix |
| 2b | Store writers + queries + 0004 | WS-B | `store/*` | writers, digest helpers, sweep, retention journal, durability tests |
| 2c | Handlers + glue + binding | WS-D | `app/api/workflows.rs`, `app/api.rs`, `app/workflow.rs`, `app/workflow_store.rs`, `binding/{spawn,observe}.rs` | interrogate, restore, injection, epilogue glue, sweep/prune wiring, transcript read-back |
| 2d | CLI | WS-E | `cli/*`, `tests/cli/*` | new verbs, restore flags, `--json` parity sweep |
| 2e | Run browser | WS-F | `ui/workflow_runs.rs`, `input/workflow_runs.rs`, config/keybind files | the overlay + keybind + config reference |
| 2f | DAG history + interrogation lane | WS-H | `ui/workflow_dag.rs`, `app/workflow_history.rs`, `workflow/layout.rs` | historical view, detached lane, `i` flow, summariser header state |
| 3 | E2E | WS-J | `tests/workflow_headless.rs`, fixtures | the four scenarios |
| 4 | Docs | WS-J | `workflows.mdx` ×3 locales, CHANGELOG, config-reference verify | sections + parity |

Sequencing inside step 1: 1a lands first, then 1b and 1c run in parallel
(1b's stub files and 1a's sweep are disjoint after the stale-entry drop, but
1b's `state.rs` shapes reference 1a's model types, so the order is a real
dependency, not just caution). Dependencies inside step 2: 2c consumes 2a's
engine API and 2b's store API through the frozen interfaces (§3), so all six
2x steps start together; 2c is the largest and the natural place for the most
capable agent. Steps 3–4 start
when 2a–2d land (2e/2f can trail — the e2e does not exercise the TUI).

**Merge gate:** `just check` green (covers fmt, clippy `-D warnings`, both
feature legs, MSVC lint, maintenance tests) **plus** `just release-docs-check`
for step 4, **plus** the env-scrubbed `just test` run from inside a karvex
session if the builder works inside one (the Phase 2 scrub block, verbatim).

**Suggested agent models per workstream** (implementation/testing agents are
restricted to opus/sonnet): WS-A **opus** (epilogue state-machine surgery is
the phase's riskiest code), WS-D **opus** (widest blast radius, most
cross-cutting), WS-B **sonnet** (mechanical writers + a precise test
discipline), WS-C **sonnet** (large but rote; the artifact test catches
drift), WS-E **sonnet**, WS-F **sonnet** (strong template to copy), WS-H
**opus** (geometry + projection subtleties), WS-J **sonnet** for docs /
**opus** for the e2e scenarios.

---

## 3. Interfaces frozen at the start of Phase 3

1. **`RestoredSeed`, `RestoredRef`, `RunGraph::materialise_with_restored(kvdag,
   run_id, tier, assignments, restored, restored_at_unix_ms)`** — WS-A owns;
   WS-B (write side) and WS-D (caller) consume. `materialise_with` stays as
   the `&[]` wrapper. *(Amended by §8 D-B: the final parameter replaced an
   internal `current_unix_ms()` call — a second clock AND a latent purity
   violation, since the engine is deterministic-given-a-supplied-clock by
   contract (04 §2). WS-D threads the run-start stamp it already mints once;
   WS-B binds seeded rows from the same `NewRun::started_at_unix_ms`.)*
2. **The epilogue contract** *(amended by D-1, §8)*:
   `EngineConfig.summary_enabled: bool` **and `summary_command:
   Option<Vec<String>>`** (populated from `KARVEX_WORKFLOW_SUMMARY_COMMAND`
   via the single `workflow_runtime_config` source — E-16); `begin_epilogue`
   computes the effective runner once and records it on `EpilogueState`, and
   `runner_of()` returns it for the reserved path (override ⇒ Command
   semantics: self-report only, sustained-idle inadmissible; none ⇒ Agent
   semantics with the bounded ladder); `Engine::epilogue_pending() -> bool`;
   the epilogue node appears through the normal `admissions()` path with
   instance path `.summary` and completes through the normal
   `NodeSelfReport`/token path; `summary_output_schema()` and
   `summary_task_spec()` — the free fn now three-arg, carrying the argv —
   are the only prompt/schema/runner sources. WS-A owns; WS-D consumes.
   `Engine::record_transcript_path(path, transcript) -> bool` (E-10) is the
   one other engine mutator added this phase — narrow by design, because
   `bind_node` would restart a finished node.
3. **The reserved-path rule:** authored node keys starting with `.` are
   rejected by `Kvdag::try_new` from this phase on; engine-owned nodes live
   under `.`-prefixed instance paths. WS-A owns the check; everyone relies on
   non-collision.
4. **`StoreWrite` additions** (`RunSummary`, `InterrogationStarted`,
   `InterrogationUpdate`, `RunEvent.at_unix_ms`, `RunNode.restored_from`) —
   WS-A owns shapes, WS-B owns persistence. Creates-never-evicted extends to
   `InterrogationStarted` (it is a create — `is_create_write` gains the arm,
   WS-D's file, step 2c).
5. **Digest compatibility rule:** restorable ⇔ `sha256(prompt_template)` and
   `sha256(canonical(output_schema))` both equal between source and target
   version's node, computed on demand, no stored digest columns. WS-B owns the
   helper; WS-D owns the decision; WS-A owns `canonical`'s home in `model.rs`.
6. **The wire table of §WS-C** — names, field sets, three new methods, three
   new event kinds, all `#[serde(default)]`-tolerant and additive; no existing
   field changes type except `WorkflowRunListParams.workflow_id: String →
   Option<String>`, which is compatible in the direction clients actually use.
7. **Interrogation argv** *(step 0(b) VERIFIED — the pre-mint branch is the
   shipped one)*: `["claude", "--session-id", <minted fork uuid>, "--resume",
   <source_sid>, "--fork-session"]`, spawned in the recorded cwd (V2: resume
   resolves globally, but the fork's transcript lands under the invoking
   cwd's project slug — recorded-cwd spawning is what keeps fork transcripts
   beside the source project's), `begin_managed_agent(Agent::Claude)`
   confirmation, `forked_session_id: Some(minted)` and the fork's own
   `transcript_path` recorded at creation, env **without**
   `KARVEX_WORKFLOW_NODE_TOKEN` (an interrogation is not a node and must not
   be able to self-report — §4 D7). A9's async-learn stays as belt-and-braces.
8. **Error codes:** `workflow_transcript_unavailable`, `workflow_run_pruned`,
   `workflow_restore_unknown_selector`, `workflow_interrogation_active`, and
   (§8 E-15) `workflow_interrogation_spawn_failed` for a pane-spawn failure
   after transcript+cwd verified — spelled once here; WS-C documents, WS-D
   implements, WS-E renders.
9. **`TaskDocument`'s optional `## Prior runs` section** — rendered only when
   context exists; absent-when-absent so every Phase 1–2 task.md is
   byte-identical (the frozen-contract change is this one section, made
   deliberately — WS-D owns `spawn.rs`).
10. **The run browser's data contract** is the wire API (`run.list` with
    `workflow_id: None`, `summary.list`), *not* private store reads — the
    runtime/client boundary guardrail applied to the new overlay.
11. **Cross-workflow listing:** `list_runs(workflow: Option<&WorkflowId>,
    limit)` with the batched `workflow_name` join — WS-B owns the query;
    WS-D (handler) and, through the wire, WS-F consume it.

---

## 4. Decisions

**D1 — The summariser is an epilogue, not a graph node, and can never change
the run's outcome.** A summariser inside the terminal-ready conjunction wedges
failed runs (§0.7); a summariser that runs after `finish` cannot. The run's
terminal status and `workflow.run.finished` are emitted exactly as today;
`workflow.run.summarized` follows when the summary lands. Every failure mode
of the epilogue (schema-invalid twice, spawn failure, pane death, cancel,
disabled config, missing `claude`) converges on `GaveUp`: journalled,
notified once, run status untouched, summary absent. `summary.get` returning
`None` is a normal answer, and every consumer (browser detail, injection,
docs) treats it as "no summary", never as an error.

**After `RunFinished`, the contract is (M8):** the run's wire `status` is
final and never changes again; `workflow.run.get` on such a run returns the
terminal status *and* a graph containing the `.summary` node in its live
state (`running`, then `succeeded` or the give-up), so a client sees a
succeeded run with a still-working summariser and that is the truthful
picture; `nodes_total`/`nodes_done` never include it (D5), so the counts read
complete at the instant `run.finished` fires; the summary's arrival is
signalled only by `workflow.run.summarized` — no second `run.finished`, no
`run.updated`. **What clients may assume (amended by §8 E-12, which an e2e
forced):** `run.finished` finalizes the OUTCOME — the status will never
change and every user-node fact is durable. It does NOT promise instant
ADMISSION of the next run: a `workflow_run_in_flight` refusal while the
epilogue resolves is normal, bounded (the ladder gives up on its own; spawn
failure resolves in one tick), self-describing (the message says why and that
retry will succeed), and retryable — the same refusal any racing client must
already handle, which is why a client needing a dedicated resolution signal
was already broken. `workflow.run.summarized` signals the success branch
only; a both-branches resolution signal is an explicit Phase 4 follow-up.
Clients must also tolerate reserved-node (`.`-prefixed) events after
`run.finished` — `run.summarized` remains the only run-level follow-up. 04 §3.2's `run_terminal_ready`
conjunction is evaluated over
user nodes only; the epilogue lives outside it by construction (the deviation
A1 flags). And 04 §4.5's steerability applies to the pane, not the API:
`workflow.node.steer` on `.summary` is **not** supported in Phase 3 —
`require_open_run` stays unchanged, direct typing into the visible summariser
pane works as it does for every pane (04 §5's documented gap), and a wedged
summariser resolves through the bounded ladder to `GaveUp` rather than
waiting for rescue.

**D2 — The summariser binding is `claude` in production and an argv override
in tests, and the override is honest.** `NodeKind::Internal` + `runner: Agent`
per 04 §4.5 — a real, visible, focusable pane the user can watch and type
into directly. It is deliberately **not** API-steerable: `require_open_run`
answers `workflow_run_closed` for `steer`/`interrupt`/`restart` on a
terminal-status run and stays that way (the H2 guard story is not weakened
for one node), so the epilogue's only interventions are direct pane input
(never intercepted, 04 §5) and the bounded ladder ending in `GaveUp` — see
D1's post-`RunFinished` contract. CI cannot run `claude`, and the
Phase 1 rule ("the stub path is a declared binding, not a test hook") is
preserved by making the override a declared configuration:
`workflow.summary_enabled: bool` (config, default true) and
`KARVEX_WORKFLOW_SUMMARY_COMMAND` (env, argv as JSON array) which swaps the
epilogue's runner to `Command` with that argv — the same first-class command
binding every fixture already uses. *(As built — §8 D-1/E-11/E-16: the env
var is read once into `EngineConfig.summary_command` via the single
`workflow_runtime_config` source; the engine records the effective runner on
`EpilogueState` so signal gating is truthful; a malformed value warns at
config-read, disables summaries for the server, and surfaces once through the
notice path at the first run start that would have summarised — never a
silent claude fallback, never a boot failure.)* The env override is
documented in `workflows.mdx`'s testing note beside
`KARVEX_WORKFLOW_DB_PATH`. Demand is `Light` via the `EPILOGUE_DEMAND`
single authority (§8 E-13); the assignment comes from the run's tier through
the existing resolver, so a `--tier low` run summarises cheaply by
construction.

**D3 — Restore is materialisation, not an engine input.** Restored nodes are
facts about how the run *begins*, not events that happen to it. Seeding them
in `materialise_with_restored` means `Engine::apply(Start)`'s existing settle
fires their edges with zero new transition code, and there is no window where
a restored node is `Pending`. No `EngineInput::Restore` exists, and none may
be added — an input would create a second path to `Restored` that the
invariants don't cover.

**D4 — A restored node's timestamps are the restore instant, and the restore
instant IS the run's start instant.** Copying the
source run's stamps would make the new run's timeline lie (a node "finished"
before its run started). *(Sharpened by §8 D-B: "now" was two clocks — the
engine minted one inside materialisation, the store another at create, with
observable skew. D3 already says restored nodes are "facts about how the run
begins", so the single authority is `started_at_unix_ms`, minted once by the
handler and threaded to both sides — H1/D14's delete-the-second-clock rule
applied one level down.)* `started_at = ended_at = the run's
started_at_unix_ms`, `duration_ms = 0`; the
provenance lives in `restored_from`, which names the source run, node, and
checkpoint seq. The wire carries it (`WorkflowRestoredFrom`), the CLI prints
it, the DAG detail strip shows it.

**D5 — The epilogue node is excluded from `nodes_total`/`nodes_done` and from
growth accounting.** Those counters mean "the run's declared work". Including
the summariser would make every summarised run report `nodes_done <
nodes_total` at the moment `run.finished` fires (the epilogue hasn't run yet)
— an unfixable ordering lie. It is likewise not a growth-budget consumer
(`max_nodes` governs expansion proposals; the epilogue is engine-owned). It
**is** a `run_node` row (so `run_summary.generated_by` resolves and the DAG
shows it) with the reserved path keeping it out of every selector namespace.
*(As built — §8 E-6: the exclusion has TWO halves that must move together —
the store's `refresh_nodes_done`/`refresh_run_node_counters` filter and the
live projection's filter in `workflow_run_info`; all three wire counters
share the one reserved-path predicate, `nodes_live` through
`live_node_count`. The DAG header's `run_counts` and the node box stay
deliberately UNfiltered — excluded from counts, never from sight.)*

**D6 — Transcript paths: stat the best-known path, and start recording the
truth.** Interrogation stats before spawning, per 03 §4.4 — that ships
regardless. Additionally, when a workflow pane's session report carries a
transcript path, it is written back to `NodeBinding.transcript_path` and
persisted, closing §0.5: from then on the stored path is the reported one,
and the pre-launch estimate is only the fallback for sessions that never
reported. *(Premise upgraded during the build — §8 E-10: source tracing
showed the hook payload carries `transcript_path` end-to-end and was dropped
only at the last layer, `agent_resume::session_ref_from_report`'s claude arm
— so the read-back lands LIVE, not dormant, via the two scoped observe calls
in `app/api/panes.rs` and `Engine::record_transcript_path`. §0.5 narrows
accordingly. The V4 spike additionally found the estimate formula matched the
real path in every tested case — do not "fix" the estimate. Stat-first
remains the arbiter either way.)*

**D7 — Interrogation mechanics.** One live interrogation per source node
(`workflow_interrogation_active` otherwise) — forking the same session twice
concurrently is a footgun with no use case; sequential re-interrogation is
fine (each is its own record). *(As built — step 0(b) VERIFIED the pre-mint
combination, so the shipped record is created with `forked_session_id:
Some(minted)`; the async-learn fallback below remains implemented as
belt-and-braces and 0004's `option<string>` stands for it.)* The fallback
design: the record created with
`forked_session_id: None` (hence 0004's `option<string>` — 03 §4.4's
non-optional column assumed the id is knowable at record time, which is only
true if step 0(b) verifies the `--session-id`+`--resume --fork-session`
combination; when it does, the id is pre-assigned and recorded immediately,
which is the preferred path). Otherwise the id is learned from the pane's
session report and written via `InterrogationUpdate`. The pane gets **no node
token** — an interrogation must not be able to call `node complete` or
`node expand` on the source node's behalf. Pane title:
`interrogate · <workflow> · <path>`; the reconstructed variant is titled
`reconstructed · …` and its seed task states, in its first line, that it is a
reconstruction from stored outputs, not the original session (00 Feature 3's
"never presented as the original teammate" made mechanical). Ended-at is
stamped from pane close/death through the existing `PaneDied`/`close_pane`
call sites — no new `AppEvent`, keeping `src/events.rs` untouched (§0).

**D8 — Interrogations are not run nodes, anywhere.** Not in `RunGraph.nodes`,
not in layout's layered graph, not in `reconcile_workflow_pane_bindings`'s
sweep (which would report a dead node pane), not in counters. They are a
parallel small collection with their own store table (shipped), their own
lifecycle glue, their own DAG lane, their own events. Phase 4's interviews
reuse all of it.

**D9 — `workflow.run.list` grows to cross-workflow; the browser composes two
lists.** `workflow_id: Option<String>` (None = all, newest-first, same limit
semantics), `workflow_name` denormalised onto `WorkflowRunInfo` (one batched
name lookup server-side, not N client calls). Pruned history is a different
resource with a different lifetime, so it is a different method
(`workflow.summary.list`) rather than a `deleted: bool` row shape in
`run.list` — a client that only understands runs keeps working unchanged.

**D10 — Summaries get read methods, not event replay.** The browser and the
injection path read `run_summary` through `summary.get`/`summary.list`. The
journal's `summary` `run_event` kind exists for replay/audit (D3 of
`00-overview.md`: the journal is the audit trail, the wire is the contract),
not as the read path — reading summaries out of journal payloads would
recreate the growth-limit-projection pattern that §0.6 shows is easy to get
subtly wrong, for a table that already has a UNIQUE-indexed row.

**D11 — Cross-version compatibility is two per-node digests, computed on
demand.** Exactly 03 §5's rule. `spec_digest` is graph-wide and useless per
node; the two inputs are immutable columns; recomputation is microseconds at
this scale. `allow_changed` restores a `definition_changed` node anyway —
03 §5's "offered with an explicit warning, defaults to re-run" maps to:
default skip-with-reported-reason, flag to force. A selector that matches *no
target node* is a hard error (typo protection; see WS-D's restore bullet),
while a matching
selector with no usable checkpoint is a reported skip — the line between
"you asked for something that doesn't exist" and "it exists but can't be
restored".

**D12 — Retention runs after run close, journalled as a workflow-row fact.**
`prune_run_history` fires on the store task after a run closes and its
epilogue resolves, keeping `retention_runs` per workflow. 03 §9's "journalled
at the workflow level" is implemented as a `workflow.pruned_runs` counter +
`updated_at` refresh (0004 adds the column) rather than a new journal table —
a prune is one number, and the summary rows it leaves behind are their own
record. Never on a read path; a browser open never mutates.

**D13 — Orphaned runs are marked `failed { reason: "interrupted" }` at store
open, not `Paused`.** 04 §9 wanted `Paused` + a resume offer; resume does not
exist and `Paused` renders as "waiting", which is false. `failed/interrupted`
is honest, terminal (so `node_history`, retention, and the browser treat it
consistently), and the recovery 04 §9 actually promised — checkpoint restore
into a new run — is exactly what Phase 3 ships and what the browser's `R`
offers on such a run. Non-terminal nodes sweep to `cancelled` (not `failed`:
the node didn't fail, the server left). This is a deliberate, flagged
deviation (§6 A2).

**D14 — The two open durability defects are fixed in the foundation, not
alongside.** `at_unix_ms` on `StoreWrite::RunEvent` (delete the second clock,
the 0002/`started_at` precedent applied to the journal) and
`Truncated.limit_value = expand_max`. Phase 3's summariser, restore, and
interrogation read paths are all journal- or row-projection paths; building
them on a foundation with a known field-loss pattern would guarantee
re-occurrence. Both fixes are small, both get regression tests naming the
0.10.2 class.

**D15 — Migration `0004` is six statements, nothing more.** The six of
WS-B's migration bullet: `run_event.at` OVERWRITE, `interrogation.
forked_session_id` → `option<string>`, the `run_event.kind` ASSERT extension
(`"summary"`), `run_node.kvdag_node` → `option<record<kvdag_node>>` (B1 — the
epilogue row is a `run_node` with no kvdag definition behind it, and 0001
declares the field non-optional; only the reserved `.summary` path may write
the NULL), the new `workflow.pruned_runs` column (D12's counter — the table
is SCHEMAFULL, so the write fails without it), and the `run_summary_version`
index (M9 — the never-pruned table's workflow filter runs through
`kvdag_version`, so the traversal gets an index). No
new tables (§0.1), no new checkpoint fields, no digest columns.
`DEFINE FIELD OVERWRITE` is required for the redefinitions — a plain
`DEFINE FIELD` over an existing schemafull field errors (the 0002 lesson,
restated so it is not relearned).

**D16 — Every new durable read path ships with field-for-field restart
tests.** Named as a rule because it is the required design-against for the
0.10.2 P1 class: write via the production writer, read via the production
reader after a simulated reload, assert each field by name. WS-B owns the
store-layer set, WS-D the wire-layer set. A reviewer should be able to point
at one test per new field.

**D17 — The `--json` gap is closed everywhere, not avoided in new verbs.**
Six existing verbs gain the flag in one sweep (WS-E). The alternative —
consistency-for-new-verbs-only — leaves the CLI permanently half-consistent
and makes the docs' "add `--json` for the raw envelope" sentence false for
exactly the verbs users script first (`cancel`). `node complete` is
deliberately excluded from the sweep: it is the node-side reporting verb,
whose contract is its exit code and its env, not an output envelope.
*(As built — §8 WS-E: `--json` also forces raw-envelope rendering on
REFUSALS for the mutation verbs, previously always humanized regardless of
flag — success paths and exit codes unchanged; and `cli/runtime.rs` needed no
edits, since the list verbs' wrappers already emit raw JSON unconditionally —
the file mention narrows to "only if a wrapper is touched".)*

**D18 — Bare `--restore-from <run>` means "everything restorable".** The
common case is "re-run this, keeping what succeeded"; forcing an explicit
selector list for it would make the short spelling useless. Explicit
`--restore` selectors narrow it. The report always lists both sets, so the
bare form is never silent about what it skipped.

**D19 — Truncated checkpoint payloads are not restorable, and spill stays
optional.** The store discards over-256KB payloads and stores a
`{"truncated": true}` stub instead (`enforce_payload_budget`,
`store/mod.rs:1298-1307`) — 03 §7's "spills to a file" was never implemented. Restoring the `{"truncated": true}` stub would
hand downstream nodes a lie labelled as data; such checkpoints are skipped
with `payload_truncated`. Implementing real spill (app-glue side: write the
payload under the node's `artifacts/` before enqueueing the checkpoint write,
path into `artifact_paths`) is a contained follow-up — noted as the fold-in
option, not required for Phase 3, because summaries (≤1,200 chars, always
inline) are the payload edges actually pass by default.

**D20 — No `PROTOCOL_VERSION` bump (it is 19; verify at build start against
`src/protocol/wire.rs`).** Everything is additive JSON on
`Method`/`ResponseResult`/`Subscription`/`EventKind`; the binary frame
protocol is untouched; the DAG view (historical included) renders server-side
into the existing frame stream. The one type change
(`WorkflowRunListParams.workflow_id` optional) loosens a *request* field,
which every published client satisfies. No integration asset changes → no
`CLAUDE_INTEGRATION_VERSION` bump.

**D21 — Prior summaries are a pointer, not a payload.** Injecting N×4,000
chars into every node's prompt is a token tax on exactly the runs history is
supposed to make cheaper. The run gets one `context/prior-runs.md`; each
node's `task.md` gets a two-line optional section pointing at it. Agents read
it when the task warrants (the file is inside the run dir the pane can
already reach); command-runner nodes ignore it for free. Injection is
per-run-recorded (`context_runs`), default-on, opt-out via
`include_prior_summaries: false` / `--no-prior-summaries` — spec assumption 3
verbatim.

**D22 — New config: `workflow.history_context_runs` (default 3) and
`workflow.summary_enabled` (default true).** Both in the existing
`[workflow]` block (`retention_runs` finally gains its consumer, D12), both
in `config-reference.json`, both doc-comment-synced for
`config_reference_check.py`.

---

## 5. Manual validation checklist (real `claude`, not CI)

Run after step 3, from a debug build (`karvex-dev` store — a debug build uses
a different database than the installed binary; create the test workflow
fresh):

1. A two-node agent run completes; the `.summary` node appears in the DAG,
   runs as a real pane, and `kvx workflow summary show <run>` prints a
   sane summary within budget.
2. A second run's root teammate can be asked "what did the last run leave
   open?" and answers from `context/prior-runs.md`.
3. `kvx workflow node interrogate <run> <path>` on a finished agent node opens
   a forked session that answers questions about its work; after closing it,
   the source `<sid>.jsonl` is **byte-identical** (checksum before/after) and
   a second interrogation still works.
4. Delete the transcript file, interrogate again → structured
   `transcript_unavailable`; `i` twice in the DAG view → reconstructed pane,
   visibly labelled.
5. Restore a run into an edited (v2) workflow; confirm the changed node
   re-runs and the unchanged node restores; `kvx workflow run show` renders
   `restored from:` provenance.
6. Kill the server mid-run; restart; the run browser shows the run as
   `failed · interrupted`, `R` offers restore, and the restored run completes.

---

## 6. Assumptions flagged for review

- **A1 (epilogue deviation).** The summariser runs *after* the run's terminal
  status per §4 D1, so `run.finished` precedes the summary and a summariser
  failure never changes a run's outcome — deviating from a literal reading of
  04 §4.5's "no kind is exempt from §3.3/terminal gating". Rationale in §0.7.
- **A2 (orphan sweep).** Interrupted runs are marked `failed{interrupted}` at
  store open instead of 04 §9's `Paused`+resume, because resume machinery does
  not exist and Phase 3's restore is the designed recovery (§4 D13).
- **A3 (reserved key prefix).** Authored node keys beginning with `.` become
  invalid at `Kvdag::try_new` from this phase (needed for the `.summary`
  path). Existing stored versions are unaffected; only new
  `create`/`version.create` calls reject. Judged zero-impact in practice.
- **A4 (summary command override).** `KARVEX_WORKFLOW_SUMMARY_COMMAND` exists
  so the epilogue is e2e-testable without `claude` (§4 D2). It is a declared,
  documented binding override, not a hidden test hook — but it is new surface.
- **A5 (counters).** The epilogue node is excluded from
  `nodes_total`/`nodes_done` (§4 D5).
- **A6 (TUI restore scope).** The browser's restore action is restore-all with
  a confirm; per-node selector restore is CLI/API-only in Phase 3.
- **A7 (browser refresh model).** Event-driven + on-open refresh; no polling
  loop (also keeps `src/app/runtime.rs` out of the diff, §0).
- **A8 (injection shape).** Prior summaries as a run-dir file + two-line
  task.md pointer (§4 D21), not inline prompt text.
- **A9 (forked-session-id learning).** *(Resolved: step 0(b) VERIFIED — the
  pre-mint path shipped; async-learn remains as belt-and-braces.)* If the
  mint is ever ignored, the id arrives asynchronously via the session report
  and may be absent for a never-reporting pane; the record then keeps
  `forked_session_id: null`, which downstream (Phase 4 interviews) must
  tolerate.
- **A10 (keybind default).** *(Corrected during the build — §8 E-7.)*
  `keys.open_workflow_runs` defaults to `prefix+shift+b`; the originally
  planned `prefix+shift+r` collided with `reload_config`, and the "build-time
  collision check" this assumption cited did not exist — it does now
  (`default_keybinds_have_no_duplicate_chords`).

## 7. Risk register

| # | Risk | Mitigation |
|---|---|---|
| R-1 | The epilogue's settle special-case destabilises the terminal state machine (the run finishes twice, or never stops ticking) | WS-A is the sole engine owner; `finish` is asserted re-entrancy-safe by test; the tick disjuncts (`epilogue_pending()`, plus E-8's interrogation term) are pinned by the rewritten `a_finished_run_releases_the_tick_and_accepts_the_next_run` e2e — a `GaveUp` epilogue lets the deadline lapse and the next run is admitted (pin shipped, §8) |
| R-2 | `claude --resume --fork-session` behaves differently than assumed (cwd-sensitivity, `--session-id` combo, hook payload) | Step 0 spike **before** parallel work; D6/D7 carry designed fallbacks for every unverified behaviour; the CI-reachable seams (argv, stat-first, no-silent-pane) don't depend on any of them |
| R-3 | New durable read paths repeat the 0.10.2 field-loss class | D14 fixes the two known instances first; D16 mandates per-field restart tests; the summary read path is a UNIQUE row, not a journal projection (D10) |
| R-4 | Restore hands downstream nodes truncated or stale data | `payload_truncated` skip (D19); digest gate (D11); the WS-A test that restored `Data` edges carry the seeded payload verbatim |
| R-5 | The run browser shows orphaned `running` runs as live | D13 sweep at store open, sequenced in WS-B/WS-D before WS-F's e2e-visible behaviour |
| R-6 | Mode plumbing breadth (7 exhaustive match sites) serialises the UI streams | WS-G lands every variant + stub in step 1b, the Phase 2-proven pattern |
| R-7 | `TaskDocument` change ripples into Phase 1/2 prompt expectations | The section is absent-when-absent; a byte-identity test pins the no-context rendering against a Phase 2 fixture |
| R-8 | Summariser cost surprises users on every run | Demand `Light` through the tier table; `summary_enabled` config off-switch; the epilogue never retries more than the one corrective re-prompt |
| R-9 | Docs translation parity blocks release | WS-J step 4 is a named step gated on `just release-docs-check`, with three new headings listed up front |
| R-10 | Uncommitted session-name diff conflicts | §0's overlap table: three files, all additive one-liners, sequenced last; `src/events.rs` untouched by design (D7/D8) |
| R-11 | Single-live-run guard makes restore-while-busy confusing | Surfaced as the disable reason in the browser and the existing `workflow_run_in_flight` error on the CLI; lifting the one-run limit is explicitly out of scope |
| R-12 | Scope creep toward Phase 4 (watchdog, partial checkpoints, review UI) | Interrogation record + spawn shape are built Phase 4-ready but no interview prompt, no review tables writer, no `partial` checkpoints ship now |

## 8. Amendment log (build round)

Every change applied to this document after its freeze, mapped to the build
ledger's ids so the audit can diff plan-vs-built mechanically. "In-place"
means the named section's text was amended; entries without an in-place note
are recorded here as the authoritative correction. All amendments were
adjudicated by the design authority and counter-signed by the team lead
during the build; the full rationale trail lives in the build ledger.

| Id | What changed | Where |
|---|---|---|
| E-1 | Sweep-site polarity: modal.rs:1581 / navigate.rs:2726 hold `RunGraph` (not `RunNode`) literals; `epilogue: None` one-liners | §1 sweep paragraph (in-place) |
| E-2/C-2 | `src/ui/workflow_runs.rs` + `src/app/input/workflow_runs.rs` created by WS-G as compiling stubs (Esc→leave_modal live in the stub), WS-F fills bodies only | §WS-G delivers (in-place) |
| E-3 | The step-1c struct-literal/match sweep §WS-C omitted: three site families (params literals, info/response literals with placeholders, `context.rs` EventData arms) | §WS-C, new sweep paragraph (in-place) |
| E-4 | app/mod.rs headless mirrors (AppState literal, headless mode dispatch) added to the 1b sweep; §0's uncommitted-diff protections superseded (feature merged as e10368b1); app/mod.rs declared sweep-only; **the compiler is the site list** adopted as a standing rule | §0 note, §1 (in-place) |
| C-1 | `HistoricalRunSnapshot`/`HistoricalInterrogation`/`AppState.historical_run` shapes named in WS-G's 1b delivers (were only implied by §WS-H) | §WS-G delivers (in-place) |
| C-3 | Historical loader data contract: wire dispatch of `run.get` for run+graph; direct store `list_interrogations` as the one sanctioned exception; WS-H owns the seam flip | §WS-H files paragraph (in-place) |
| C-4 | Browser event-driven refresh mechanism: one-line hook in `emit_workflow_event` (run-level kinds only) → `refresh_workflow_runs_overlay` in WS-F's file; selection re-anchored by run_id | §WS-F refresh bullet (in-place) |
| E-5 | WS-H granted `input/mod.rs` + `input/overlays.rs` scoped to WorkflowDag regions; snapshot-clear placement (DAG Close arm, never generic `leave_modal`) | §WS-H files paragraph (in-place) |
| E-6 | D5's live-projection half (was unassigned): reserved-path filter in `workflow_run_info` counters, landed by WS-A under a region-scoped grant; one predicate across all three wire counters; DAG `run_counts`/node box deliberately unfiltered; the two counter e2es are zero-edit acceptance tests. Ordering-invariant narrowing: "no USER-node event follows run_finished" | §4 D5 note (in-place); e2e wording |
| E-7 | `keys.open_workflow_runs` default `prefix+shift+r` → `prefix+shift+b` (collision with `reload_config`); `default_keybinds_have_no_duplicate_chords` test makes A10's fictional check real | §WS-F keybind bullet, §6 A10 (in-place) |
| E-8 | Tick deadline gains the third disjunct `!interrogations.is_empty()` — interrogations outlive runs (D8); the bulk-close reconcile sweep needs a cadence with no run live | §WS-D epilogue-glue bullet (in-place) |
| E-9 | Two-step `i` offer replaced by `i` = resumed / `Shift+I` = reconstructed (no landed state carrier; toast-slot keying broken under terminal/system delivery; explicit-keypress escalation strictly better) | §WS-H interrogation bullet (in-place) |
| E-10 | `Engine::record_transcript_path` narrow mutator; `app/api/panes.rs` two scoped observe calls (WS-D grant); D6 premise upgrade — hook payload carries transcript_path end-to-end, dropped only at `agent_resume`'s claude arm, so the read-back lands LIVE; §0.5 narrows to "dropped at the last layer" | §3 item 2, §4 D6 (in-place) |
| E-11 | Malformed `KARVEX_WORKFLOW_SUMMARY_COMMAND` ⇒ warn at config-read + DISABLE summaries + one notice at first affected run start; claude fallback forbidden; boot hard-fail rejected | §4 D2, §WS-D (in-place) |
| E-12 | M8 contract split: `run.finished` finalizes OUTCOME, not ADMISSION; `workflow_run_in_flight` during the epilogue window is normal/bounded/retryable; both-branches resolution signal queued for Phase 4; reserved-node events after run.finished are legal, `run.summarized` the only run-level follow-up | §4 D1 M8 paragraph (in-place) |
| E-13 | `.summary` demand live-vs-durable mismatch (D-1's root cause, second instance): `EPILOGUE_DEMAND` single authority. ~~"Verified `demand` was the last definition-derived field"~~ — **that claim was FALSE (audit MAJOR): a FOURTH instance existed** — `interrogation_runner` (app/api/workflows.rs:2095-2114) resolved the runner via definition/version lookup with no reserved-path branch, so interrogating `.summary` under a command-bound epilogue answered `workflow_transcript_unavailable` instead of the runner-based refusal, the exact wrong-answer shape that handler's own comment argues against; fixed in the audit wave. The family's checklist rule is therefore strengthened: any field the live projection derives from the definition is wrong for a definition-less node, **and the sweep for the family must be MECHANICAL** (grep `workflow_*_info` + `*_for_pane` + every `definition().node(...)` read; check what a definition-less node receives) — never an assertion of completeness. Three separate agents each believed they had found the last one; the count went 1 (D-1), 2 (E-13), 3 (D-C), 4 (audit) | §4 D2/D5, §WS-D (in-place) |
| E-14 | Interrogate precondition keys on the node's RUNNER (live definition / kvdag version row), not `agent_session_id` presence — every node gets a persisted sid, so id-gating was a false premise | §WS-D interrogate bullet (in-place) |
| E-15 | Pane-spawn failure after transcript+cwd verified answers a dedicated error code rather than lying with `transcript_unavailable`. Wire string CONFIRMED: `workflow_interrogation_spawn_failed` — single code, reason in the message (house style). Its first cut reused the node-spawn codes and self-corrected for D8's own reason: an interrogation is not a node, and `workflow_node_spawn_failed` would lie. Error codes were never covered by the naming guard (no phase's are); WS-D pinned the convention locally — all five Phase 3 codes `workflow_`-prefixed snake_case, banned-words-free, distinct from node-spawn codes — and a full cross-phase error-code inventory test is a post-phase backlog item (ruled: worth having as contract; the local pin is sufficient to ship Phase 3) | §3 item 8, §WS-D (in-place) |
| E-16 | `summary_command` read once via a single `workflow_runtime_config` source (WS-D's E-11 work caught a second two-readers shape — the same single-authority principle as D-1/D9) | §3 item 2, §WS-D (in-place) |
| D-1 | The build's one pre-implementation defect: the summary-command override never reached engine signal gating; `runner_of()` defaulted `.summary` to Agent, so under the override a sustained idle would have typed the seed prompt into a shell pane. Fix: `EngineConfig.summary_command`, effective runner on `EpilogueState`, three-arg `summary_task_spec` carrying the argv as single authority | §3 item 2, §4 D2 (in-place) |
| D15+1 | Migration 0004 is SEVEN statements — the `pruned_runs` backfill UPDATE (DEFAULT applies to new rows only; caught by WS-B's migration test; 0002's backfills were the missed precedent) | §WS-B migration bullet (in-place; D15's §4 text reads "six" and is corrected by this entry) |
| WS-B | `NewRun.restore_from` widened to the full request object `{run_id, nodes, allow_changed}` (what-was-ASKED durable beside per-node what-HAPPENED); restored timestamps from the run's clock; `schema_valid` recomputed from digest compare; `restore_source` is the public restore-read path | §WS-B (recorded here) |
| WS-E | `cli/runtime.rs` untouched (list wrappers already raw-JSON-only — file mention narrowed); `--json` forces raw envelope on refusals too, success paths/exit codes unchanged | §4 D17 note (in-place) |
| Fixture policy | `summary_enabled=false` in the default test config via `spawn_workflow_server_with_config`; true only for the summary suite and the six deliberately-pinned e2es (GaveUp-on-missing-claude); scenarios state their posture; `a_node_whose_pane_exits…` and `a_finished_run_releases_the_tick…` are the two named timing-sensitive tests; the latter is §7 R-1's pin as rewritten | §WS-J (recorded here) |
| Spike (step 0) | V1: fork-resume non-mutating, byte-identical source (verified). V2: resume resolves globally; fork transcript lands under invoking cwd's slug (recorded-cwd spawning vindicated). V3: pre-mint verified — D7's preferred branch shipped. V4: estimate formula matched reality in every case; hook payload carries the path (see E-10) | §2 step 0, §3 item 7, §4 D6/D7, §6 A9 (in-place) |
| E-17 | The dead-variant decision: **REMOVAL** (WS-A deletes `WorkflowEvent::InterrogationStarted/Ended`; WS-D's granted None arms follow). Wire-up was proven unfaithful three ways: the variants carry `{run, id, path}`, insufficient to build `WorkflowInterrogationInfo`; widening them would make model.rs depend on api::schema, forbidden by the pure-layer test; and the Ended case is structurally impossible to route through the engine, since tracker-entry removal IS the idempotence mechanism. Root erratum — the third plan-premise error of the build: the §1 WS-A step-1a bullet specifying those variants assumed the interrogation projection is engine-held; it is app-held (interrogation is handler action, D8). Joins E-14/D-1 in the Phase 4 checklist: verify premises against the tree, not the prose | §WS-A step 1a (recorded here) |

| D-B | Post-freeze P1-class defect (caught by scenario 5, the B2 acceptance test): restored-node timestamps had TWO clock authorities — `current_unix_ms()` inside `materialise_with_restored` vs the store's create stamp (~388ms observed skew); no channel existed to thread one value (`RestoredSeed` had no timestamp field). Fix: the restore instant := the run's start instant (D3's "facts about how the run begins" makes this semantics, not fudge); `materialise_with_restored` gains `restored_at_unix_ms: u64` replacing the internal mint (also fixing a latent purity violation — the engine is deterministic-given-a-clock by 04 §2 contract); WS-D threads the once-minted run-start stamp; WS-B binds seeded rows from `NewRun::started_at_unix_ms`. Partly an adjudication gap: the build-round approval accepted two descriptions ("run's started_at" / "now at materialise") that could diverge without pinning the threaded value | §3 item 1, §4 D4 (in-place) |

**Audit evidence (verified clean — recorded as evidence, not errata):** D1's
outcome-immutability holds on three independent structural legs (settle's
terminal early-return; `run_terminal_ready` evaluated over user nodes only;
succeed/needs_attention short-circuiting before run-level machinery); the
interrogation pane passes `Vec::new()` env — stricter than §3 item 7
requires; `allow_changed` provably cannot reach the truncation gate (D19's
check runs before and independently of the compatibility branch); §3 item 9's
prior-runs byte-identity pin is structural equality, stronger than described.
The navigate-keybind test flake is CLOSED as pre-existing: a 2-second
wall-clock `wait_for_file` deadline at HEAD with 7 pre-existing callers,
reproduced under CPU oversubscription alongside two sibling callers at
~2.03s — the new keybind is uninvolved; noted as a latent CI risk on
oversubscribed runners, dropped from the audit probe list.

Unamended and still authoritative: every §3 interface not named above, D3/D4/
D8–D14/D18–D22, the workstream boundaries, and §5's manual validation
checklist (now including: verify the admission contract — a script that
retries on `workflow_run_in_flight` after `run.finished` proceeds within the
epilogue window).
