# Phase 2 implementation plan — dynamic graphs, tiers, and the observability
# hardening that dynamic graphs depend on

Release target: **v0.10.0**.

This is the concrete plan, written against the code as it stands after v0.9.4.
Where `05-phase-plan.md` §5's Phase 2 outline is stale — and it is stale in six
places — this document supersedes it and says why. Every file, function, line
number, and `just` recipe named below was re-checked against the tree at
`497f247d`; where an earlier draft of *this* document named something that does
not exist, the correction is stated inline rather than quietly dropped, so a
reader coming from that draft knows which of the two to believe.

Phase 2's definition of done:

> **A node can propose new nodes, karvex decides, and the user always finds out
> what karvex decided.** An accepted proposal instantiates a template as a live
> teammate under its parent with inherited fan-in, visible in the DAG the moment
> it exists. A rejected or truncated proposal is surfaced on three independent
> surfaces — the API, the DAG overlay, and the CLI — of which at most one is
> config-gated. A run's `(model, effort)` per node is resolved once, from the
> run's tier and the node's measured history, persisted with its reason, and
> shown wherever the node is shown. And the six v0.9.4 regressions that make a
> misbehaving run hard to diagnose are closed, because dynamic growth multiplies
> every one of them by the node count it unlocks.

Explicitly **not** in Phase 2: the watchdog (Phase 4 owns `watchdog_interventions`),
the run-history browser and restore (Phase 3), the review cycle (Phase 4), a
non-`Off` default for `ui.toast.delivery` (roadmap — see §5 R-11), and a DAG
repaint tick (roadmap).

---

## 0. Reality check: what the outline predates

Six corrections, each of which changes the plan rather than the prose, plus a
seventh entry that names the hardening items the rest of the document cites.

1. **The slim build is a merge gate now.** Commit `61dc9544` added
   `just check-slim` (`--no-default-features` clippy + nextest, `justfile:49-52`)
   to `just ci`, and `just check` is `ci windows-lint` plus the maintenance
   script tests — so one `just check` already covers both feature legs and the
   MSVC lint. (There is no `just check-no-workflow`; earlier drafts invented it.)
   Only
   `src/workflow/store` is behind `#[cfg(feature = "workflow")]`; `model`,
   `tier`, `engine`, `layout`, `binding`, `definition` compile unconditionally
   (`src/workflow/mod.rs:20-27`). Every line of expansion logic therefore belongs
   in the pure engine, with only its persistence gated. Every workstream below
   states its slim-build posture.

2. **`engine/expand.rs` is 30 lines of types with zero references in the tree.**
   `ExpandProposal` and `ExpandRejection` are declared and never used; `mod.rs`
   imports nothing from `expand`. The "wiring the outline assumes exists" is one
   `pub mod expand;` line. Two structural gaps the stub does not anticipate are
   decided in §4 D2 and D3.

3. **`tier::resolve` is already fully implemented, including `auto`** — the
   §7.1/§7.2 tables and all seven §7.3 steps, with 11 unit tests. The outline's
   three clauses ("wired into spawn", "persisted per `run_node`", "`auto` over
   `NodeHistory`") are stale in three different directions: resolution happens at
   *materialise*, not spawn; `(model, effort)` is already persisted and exposed;
   and the real gap is that **`NodeHistory` has no producer and three of its five
   inputs have no truthful source**. §4 D8 and D9 replace the bullet.

4. **`workflow.node.spawned` is the wrong new event.** `workflow.node.created`
   already exists and already means "a node entered the run graph", and
   `WorkflowRunNodeInfo` already carries `parent_path` and `depth`, so a client
   can already distinguish an expansion child. §4 D5 keeps one new wire kind.

5. **The TUI has no workflow create-or-run entry point at all — but the
   authoring-time tier already exists end to end.** The only workflow binding is
   `keys.open_workflow_dag`, which raises a toast when no run exists
   (`src/app/workflow.rs:855-863`), so "tier prompt modal at run" presupposes a
   launch flow Phase 1 never built. What it does *not* presuppose is new tier
   storage: `Definition.default_tier` (`src/workflow/definition.rs:32`,
   accessor `tier()` at `:188`), the `workflow.default_tier` column
   (`0001_init.surql:23`), `create_workflow`'s bind (`store/mod.rs:248-262`),
   `WorkflowSummary.default_tier`, and the run-start default
   (`src/app/api/workflows.rs:586`) all shipped in Phase 1. §4 D17 is rewritten
   accordingly and the outline's "`default_tier` plumbing" step is deleted.

6. **The notice channel cannot carry the headline guarantee.** `AppState.toast`
   is a single slot and `take_pending_announcements` deliberately orders
   node-first so the run-level notice wins (`src/app/workflow.rs:220-223`). Every
   per-node growth rejection would be destroyed by the run-level notice that
   follows it. Two facts constrain the fix and are easy to get wrong:
   `show_workflow_notice` (`src/app/workflow.rs:1384`) only touches the slot
   under `ToastDelivery::Karvex` — the `Terminal`/`System` arms fire an OS
   notification per notice and have no slot contention — and the expiry clock
   `toast_deadline` lives on **`App`** (`src/app/mod.rs:111`), armed by
   `App::sync_toast_deadline` (`src/app/api.rs:711`), not on `AppState`. Fixing
   H4 is a **prerequisite**, not a nice-to-have: §4 D10.

7. **The six hardening items ("H1–H6") are the v0.9.4 regressions this phase
   closes.** They are cited by number throughout; the risk register in §5 uses a
   deliberately different `R-n` numbering. Each one below was re-verified against
   the tree at v0.9.4.

   | # | Defect, with the evidence | Owner |
   |---|---|---|
   | H1 | `create_run` (`store/mod.rs:555-609`) never binds `started_at`, so `workflow_run.started_at`'s `DEFAULT time::now()` (`0001_init.surql:121`) mints a second clock at queue-drain time | WS-C |
   | H2 | The closed-run guard exists only inside `handle_workflow_node_restart`: `RUN_CLOSED_CODE` is declared at `src/app/api/workflows.rs:118` and used exactly once, at `:899`, inside the handler that starts at `:876`. `steer` (`:789`) and `interrupt` (`:811`) have no guard | WS-E |
   | H3 | `workflow.get`'s `--json` path returns the raw response while the human path re-renders it through `print_workflow_show` (`src/cli/workflow.rs:1302`), so the two describe different field sets | WS-E + WS-I |
   | H4 | `AppState.toast` is one slot; a per-node notice is destroyed by the run-level notice queued behind it (§0.6) | WS-F |
   | H5 | `create_version` (`store/mod.rs:276`) never refreshes the `workflow` row, so a `workflow.update` that changes the document's `description` or `default_tier` leaves `workflow.get` reporting v1's metadata beside `head_version: 2`. Note `kvdag_version` has **no** `description` column (`0001_init.surql:30-49`), so "read it from the head version" is not implementable without inventing one | WS-C + WS-E |
   | H6 | `observe_workflow_pane_exit` is called from exactly one place, `src/app/api.rs:181` (the `AppEvent::PaneDied` path), so a pane closed rather than crashed leaves its node `running` | WS-F |

---

## 1. Phase 2 workstreams

Ten workstreams. Three land shape-only files in step 1 so the other seven are
genuinely parallel afterwards.

**Shared-file rule.** No two workstreams edit the same file during a parallel
step. Four files are genuinely shared and are therefore **landed complete in
step 1** by a single owner and never touched again:

| File | Step-1 owner | Why shared |
|---|---|---|
| `src/workflow/model.rs` **plus the struct-literal sweep below** | WS-A | `StoreWrite`/`EngineInput`/`WorkflowEvent`/`RunGraph` shapes consumed by WS-B, WS-C, WS-E, WS-F |
| `src/app/state.rs` | WS-F | `DagViewState`/`DagNodeView` fields (WS-G), `Mode::WorkflowLaunch` + `WorkflowLaunchState` (WS-H), `toast_queue` (WS-F) |
| `src/api/schema/workflows.rs` + `events.rs` + `schema.rs` + `response.rs` + `src/api/subscriptions.rs` | WS-D | wire types consumed by WS-E and WS-I |
| `src/workflow/store/migrations/0002_growth_and_history.surql` | WS-C | one migration file, one owner |

**The step-1a struct-literal sweep.** `RunNode` and `RunGraph` are plain structs
with no `Default`, so adding a field breaks **every** literal that constructs
them. Step 1a is therefore not "edit `model.rs`"; it is "edit `model.rs` and
mechanically extend every construction site", which at v0.9.4 is:
`src/workflow/engine/graph.rs:42`, `src/workflow/engine/tests_support.rs:77,85`,
`src/workflow/layout.rs:461,497`, `src/ui/workflow_dag.rs:930,968,1168,1227,1365`,
`src/app/workflow.rs:1864`, `src/app/input/modal.rs:1565`, and
`src/app/input/navigate.rs:2705`. Three of those files
(`layout.rs`, `tests_support.rs`, `navigate.rs`) belong to no workstream and are
touched **only** here; the rest are touched again by their owner at a later,
non-parallel step. Without this sweep the tree does not compile at the end of
step 1 and the "cargo build green" gate is vacuous.

```
WS-A model+expand ──┬──▶ WS-B engine wiring ──┐
WS-D wire types   ──┼──▶ WS-C store          ─┼──▶ WS-E handlers ──┬──▶ WS-J e2e+docs
WS-F app state    ──┘                         │    WS-G dag banner ┤
                                              └──▶ WS-H launcher   ┤
                                                   WS-I cli        ┘
```

### WS-A — Expansion core (pure, unconditional)

**Files:** `src/workflow/model.rs` + the step-1a struct-literal sweep listed
above, `src/workflow/engine/expand.rs`, `src/workflow/engine/graph.rs`,
`src/workflow/tier.rs` (the `HistoryIndex` alias only — see below).

**Slim posture:** unconditional. Zero `surrealdb`, `App`, or `ratatui`
references — the Phase 1 grep test that asserts this must still pass.

**Delivers:**

- `model.rs` step 1a: `StoreWrite::RunNodeCreated` and `StoreWrite::RunEdgeCreated`
  (the first create-shaped variants — see §4 D7); `EngineInput::ExpandProposed`;
  `WorkflowEvent::GrowthLimited`; `NodeAssignment { model, effort, reason }` (a
  new type — `tier::Assignment` at `tier.rs:157` carries no reason and stays as
  it is); `RunGraph.assignments: BTreeMap<NodeKey, NodeAssignment>`
  (§4 D9); `RunNode.assignment_reason: String`; `GrowthLimits::live_node_count`
  helper contract. Nothing else in `model.rs` changes.
- `tier.rs`: `pub type HistoryIndex = BTreeMap<NodeKey, NodeHistory>` **only**.
  It has to live in the pure layer: `graph::resolve_assignments` is unconditional
  and `store/queries.rs` is behind `#[cfg(feature = "workflow")]`, so an alias
  declared in the store cannot appear in an unconditional signature. WS-C owns
  the *query* that fills it, not the type.
- `expand.rs`: `ExpandOutcome`, the `ExpandRejection::Truncated` and
  `ExpandRejection::UnknownInput` variants, and `ExpandLimit` (§4 D2/D3);
  `evaluate(&RunGraph, &Kvdag, proposer: RunNodeIdx, &ExpandProposal)
  -> ExpandOutcome` — pure, no mutation, no effects; `commit(&mut RunGraph, proposer,
  &ExpandOutcome) -> Vec<RunEffect>` — instance paths, `spawned` provenance,
  parent→child `sequence` edge, inherited outbound edges (§4 D4), `NodeCreated`
  events, `expand_accepted`/`expand_rejected`/`growth_limited` journal writes.
  All four journal kinds already pass `0001_init.surql:193-198`'s `ASSERT` list
  and already decode in `store/queries.rs:658-661`.
- `graph.rs`: `resolve_assignments(&Kvdag, Tier, &HistoryIndex) -> BTreeMap<NodeKey, NodeAssignment>`
  — the **single** tier resolver for the whole subsystem (§4 D9), computed for
  every kvdag node **including templates** so an expansion child never needs a
  mid-run history lookup; `narrow_growth` gains an idempotence test.
- `graph.rs`, **without breaking step 3**: `RunGraph::materialise_with(kvdag,
  run_id, tier, &BTreeMap<NodeKey, NodeAssignment>)` is the new entry point, and
  the existing `RunGraph::materialise(kvdag, run_id, tier)` (`graph.rs:32`) stays
  as a thin wrapper that calls `resolve_assignments` with an empty
  `HistoryIndex`. Changing `materialise`'s arity in step 2a would break
  `src/app/api/workflows.rs:620` (WS-E, step 3a) and `src/app/workflow.rs:1865`
  (WS-F, step 3b) and leave the tree non-compiling between steps. WS-E switches
  its call site to `materialise_with` at step 3a; the wrapper is what the
  `graph.rs` and `layout.rs` unit tests keep using.

**Tested:** table-driven, no DB, no PTY.
`evaluate` returns each of the eight rejections (the six that ship today plus
`Truncated` and `UnknownInput`) for the exact condition that
produces it; `count: 4` with budget for 2 yields two accepted children **and** a
`Truncated` rejection (never accept-all, never reject-all); `expand_max` is
counted per proposing node and cumulative across proposals; depth guard is
`parent.depth + 1 <= max_depth` with static nodes at depth 0; `max_nodes` counts
every materialised `RunNode` regardless of status, so a failed child does not
refund budget; instance paths are `<parent>/<template>/<n>` with `n` 1-based per
`(parent, template)` and stable under re-proposal; inherited outbound edges
reproduce kind/payload/port/condition exactly; `narrow_growth(narrow_growth(g, t), t) == narrow_growth(g, t)`;
`resolve_assignments` covers every `(tier, demand)` pair and emits the §7.3
reason string for `auto`.

### WS-B — Engine wiring and the result channel

**Files:** `src/workflow/engine/mod.rs`, `src/workflow/engine/complete.rs`.

**Slim posture:** unconditional.

**Delivers:**

- `Engine::apply` gains the `EngineInput::ExpandProposed` arm (the match at
  `engine/mod.rs:300-319` is exhaustive, so the compiler enforces this one).
- **`expand` is stripped from a node result before validation** (§4 D6). In
  `Engine::report`, the top-level `expand` key is lifted out of the parsed JSON
  *before* `complete::validate` and *before* `complete::node_result` builds
  `NodeResult.payload`. It becomes zero or more `ExpandProposal`s routed through
  the same `expand::evaluate` path as the CLI verb. A malformed `expand` value
  (not an array of objects with a `template`) is a schema-class violation and
  consumes the node's one corrective re-prompt, with a message naming the field.
- **The two `NodeHistory` fact sources that can be truthful today** (§4 D8):
  `first_pass_succeeded` (attempt 1 reached `Succeeded` with no corrective
  re-prompt spent) and `schema_failures` (count of `SchemaViolation`s for the
  node) are recorded on the node and carried on `StoreWrite::RunNode`. Both facts
  already exist inside `Engine`; they simply had no field.
- **Write ordering invariant:** `commit` emits `RunNodeCreated` before any
  `RunNode` update for the same path, in the same `Vec<RunEffect>`.

**Tested:** a result carrying `expand` produces a checkpoint payload and `digest`
**byte-identical** to the same result without it (this is the Phase 3
restore-compatibility guard, and it is the single most important test in WS-B);
an `expand` array against a schema that does not mention it still validates;
malformed `expand` spends exactly one re-prompt then `NeedsAttention`;
`first_pass_succeeded` is false when the corrective re-prompt was used and true
when it was not; the effect vector for an accepted proposal has `RunNodeCreated`
at a lower index than every `RunNode` write for that path.

### WS-C — Store: create paths, growth invariants, node history

**Files:** `src/workflow/store/mod.rs`, `store/queries.rs`, `store/records.rs`,
`store/migrations/0002_growth_and_history.surql`, `store/tests.rs`.

**Slim posture:** entirely behind `#[cfg(feature = "workflow")]`.

**Delivers:**

- Migration `0002_growth_and_history.surql`: `run_node.assignment_reason string DEFAULT ""`,
  `run_node.first_pass_succeeded bool DEFAULT false`,
  `run_node.schema_failures int DEFAULT 0`, and a redefinition of
  `workflow_run.started_at` **without** `DEFAULT time::now()` (§4 D15) — which
  must be spelled `DEFINE FIELD OVERWRITE started_at ON workflow_run TYPE
  datetime;`, because a plain `DEFINE FIELD` over an existing schemafull field
  is an error, not an idempotent redefinition. The file must also be registered
  in the `MIGRATIONS` const (`store/mod.rs:95`), which is today a one-element
  array; `apply_migration` (`:234`) and `schema_meta` handle the rest. No
  `kvdag_version.default_tier` column: §0.5 and §4 D17 — the authoring tier
  already lives on the `workflow` row and a second copy would be a second
  authority. The `spawned` relation table (`0001_init.surql:180-185`),
  `run_node.watchdog_interventions` (`:167`), and every `run_event.kind` this
  phase emits (`:193-198`) already exist — no schema work needed for any of them.
- `write_run_node_created`: `CREATE run_node …` plus
  `RELATE $parent -> spawned -> $child SET run, template_key, proposal_id` in one
  batch. This is the **first** writer the `spawned` table has ever had; its only
  prior Rust reference is the `DELETE` in `prune_run_history` (`store/mod.rs:742`).
- `write_run_edge_created`: the create-shaped sibling of `write_run_edge`, which
  today is find-then-`UPDATE` and errors on a missing row.
- `create_run` gains the `run.growth <= version.growth` assertion its own
  `NewRun` doc comment (`store/mod.rs:140-142`) has claimed since Phase 1 and
  which has never existed, returning `StoreError::Invariant`; and binds
  `started_at` explicitly from `NewRun.started_at_unix_ms` (H1).
- `materialise_run_nodes` **stops calling `tier::resolve`** and writes
  `NewRun.assignments` verbatim, including `assignment_reason` (§4 D9). This
  removes the second resolver rather than making two resolvers agree.
- `queries::node_history(workflow, node_key, window) -> NodeHistory` — the
  aggregation that has never existed. Windowed to the workflow's most recent
  `window` runs that reached a terminal status, tolerant of `prune_run_history`
  having deleted whole runs (`runs` counts surviving rows only).
  `watchdog_interventions` is read from `run_node.watchdog_interventions`
  (`0001_init.surql:167`), the column Phase 4 will write, and is `0` until then —
  documented, not fabricated. `mean_tokens` is read from `total_tokens`, which
  `model.rs:1085-1095` documents as permanently `0`; it is
  carried because the field exists and is **not consulted by `resolve_auto`**
  (verified: `tier.rs:241-284` reads `runs`, `first_pass_successes`,
  `recent_first_pass_failures`, and `watchdog_interventions` only).
- **H5 — the workflow row tracks its head.** `create_version` (`store/mod.rs:276`)
  gains a metadata refresh that writes the new document's `description` and
  `default_tier` onto the `workflow` row it is versioning. This is the fix for
  H5 *and* for the same drift in `default_tier`, and it is deliberately not "add
  a `description` column to `kvdag_version`": that table is the immutable graph
  revision and has no description field today (`0001_init.surql:30-49`), so
  adding one would put two authorities behind one `workflow.get`.

**Tested:** `kv-mem` cases beside the Phase 1 suite — a created node round-trips
with its `spawned` relation and its parent/depth; a create followed by an update
for the same path in one queue drain lands the update (the FIFO ordering
guarantee); `create_run` rejects growth wider than the version's; `started_at`
read back after a simulated reload is byte-identical to what was bound (H1
regression test); `create_version` with a changed `description`/`default_tier`
leaves `select_workflow` reporting the new values (H5 regression test);
`node_history` over three synthetic runs fills `runs`, `first_pass_successes`,
`schema_failures`, and `recent_first_pass_failures` such that
`NodeHistory::first_pass_success_rate()` — today a private method on
`tier::NodeHistory`, which this test needs made `pub(crate)` — reads what the
runs say, and returns
`runs: 0` when every run has been pruned; migration `0002` applies on top of a
`0001`-only database.

### WS-D — Wire surface

**Files:** `src/api/schema/workflows.rs`, `src/api/schema.rs`,
`src/api/schema/response.rs`, `src/api/schema/events.rs`, `src/api/mod.rs`,
`src/api/server.rs`, `src/api/subscriptions.rs`,
`docs/next/api/herdr-api.schema.json` (regenerated).

`src/api/subscriptions.rs` is not optional: `ActiveSubscription`'s match over
`Subscription` (`:211-236`) is exhaustive, so a new `Subscription` variant is a
compile error until it gets its arm there. It was missing from the outline's
file list.

**Slim posture:** the schema types compile **unconditionally** — the artifact has
one canonical value under both feature settings, and
`generated_protocol_schema_artifact_is_current` runs on the slim leg. Zero
`use crate::workflow::*` in `src/api/schema/workflows.rs`, exactly as Phase 1
established.

**Delivers:**

| Addition | Shape |
|---|---|
| `Method::WorkflowNodeExpand` | `{ run_id, path, token, template, label, inputs: HashMap<String,String>, count: Option<u32> }`, mirroring `WorkflowNodeReportParams` (`workflows.rs:243-248`) for `run_id`/`path`/`token`. `count` is `u32` on the wire and `u16` in `ExpandProposal` (`expand.rs:17`), so WS-E converts with `u16::try_from(..)` and rejects an out-of-range value rather than truncating |
| `ResponseResult::WorkflowNodeExpanded` | `{ accepted: Vec<String>, rejected: Vec<WorkflowExpandRejection> }` |
| `WorkflowExpandRejection` | `{ template: String, reason: WorkflowExpandRejectionReason, limit: Option<WorkflowGrowthLimit>, requested: u32, accepted: u32, message: String }` |
| `WorkflowExpandRejectionReason` | one `#[serde(rename_all = "snake_case")]` unit-variant enum per `ExpandRejection` variant: `not_allowed`, `unknown_template`, `not_a_template`, `expand_max_reached`, `max_depth_reached`, `max_nodes_reached`, `truncated`, `unknown_input` |
| `WorkflowGrowthLimitKind` | `expand_max` \| `max_depth` \| `max_nodes` — the wire spelling of `ExpandLimit` |
| `WorkflowGrowthLimit` | `{ kind: WorkflowGrowthLimitKind, limit_value: u32, requested: u32, accepted: u32, at_unix_ms: u64, message: String }` — the one shape reused by `WorkflowRunInfo`, `WorkflowRunNodeInfo`, and the CLI renderers. It was referenced three times in the outline and defined nowhere |
| `EventKind::WorkflowGrowthLimited` → `workflow.growth.limited` | the **only** new event kind (§4 D5) |
| `EventData::WorkflowGrowthLimited` | `{ run_id, path, template, limit: WorkflowGrowthLimitKind, limit_value: u32, requested: u32, accepted: u32, message: String }` |
| `Subscription::WorkflowGrowthLimited {}` | plus `KNOWN_EVENT_KINDS` (`events.rs:282`) **and** the arm in `src/api/subscriptions.rs` |
| `WorkflowRunInfo` | `+ max_depth: u32, max_nodes: u32, nodes_live: u32, growth_limited: Option<WorkflowGrowthLimit>` (all `#[serde(default)]`) |
| `WorkflowRunNodeInfo` | `+ assignment_reason: String, delivery_failure: Option<String>, growth_limited: Option<WorkflowGrowthLimit>` |
| `WorkflowDetail` | the H3 shape: `{ workflow: WorkflowSummary, nodes, edges, args, versions }`. `ResponseResult::WorkflowGet` (`response.rs:255-258`) keeps its existing `workflow: WorkflowSummary` field and **gains** `detail: Option<WorkflowDetail>`; it must not be re-typed, because replacing a response payload is the one change in this phase that would be wire-incompatible and would invalidate §4 D20. `description` stays on `WorkflowSummary` and is kept correct by WS-C's H5 fix, not by reading a `kvdag_version.description` column that does not exist |
| `request_changes_ui` | `+ workflow.node.expand` (`src/api/mod.rs:22`) |
| `api_method_name` | one arm (`src/api/server.rs:344`) |

`delivery_failure` on the node info closes the retest's "delivery failures live
only in TUI state" as a side effect, which the runtime/client boundary guardrail
requires anyway: it is a shared runtime fact currently reachable only through the
private TUI path.

**Naming guard:** `src/api/schema/workflows.rs:878-900` bans
`sidebar|card|widget|row|panel` as whole words. "badge" and "banner" are not on
that list but are UI-surface words by the same rule and **must not** appear in
any API or event identifier — hence `growth_limited`, not `growth_banner`.

**Tested:** serde round-trip for every new type; the banned-word test
(`no_new_workflow_api_identifier_uses_banned_ui_surface_words`, `workflows.rs:878`)
extended to also ban `badge|banner|toast|modal` and to list the new identifiers;
the regenerated artifact committed and
`generated_protocol_schema_artifact_is_current` (`src/api/schema/tests.rs:153`,
refreshed with `KARVEX_UPDATE_API_SCHEMA=1 just test-one …`) green on **both**
feature legs; the Phase 1 catch-all sweep
`no_workflow_method_falls_through_to_not_implemented`
(`src/app/api/workflows.rs:1608`) extended with `workflow.node.expand` — note
that test lives in WS-E's file, so WS-D adds the `Method` variant and WS-E
extends the sweep at step 3a.

**No `PROTOCOL_VERSION` bump** — see §4 D20.

### WS-E — Handlers, guards, and projections

**Files:** `src/app/api/workflows.rs`, `src/app/api.rs`.

**Slim posture:** every `From`/`TryFrom` between wire and engine types stays
behind `#[cfg(feature = "workflow")]`; the slim arm returns `workflow_unavailable`
for `workflow.node.expand` like every other method.

**Delivers:**

- `workflow.node.expand` handler: token-authenticated exactly like
  `workflow.node.report` (an expand proposal is a node speaking, not an
  operator), translating to `EngineInput::ExpandProposed`. A *rejected* proposal
  is a **successful response carrying the rejection**, not an error — the run
  continues; only a bad run/path/token/closed run is an error.
- **H2 — `require_open_run`.** The closed-run guard currently inlined for
  `restart` (`RUN_CLOSED_CODE` declared at `:118`, its single production use at
  `:899` inside `handle_workflow_node_restart`, `:876`) is factored into one
  helper and applied to `steer`, `interrupt`, `restart`, **and** `expand`.
  Writing the expand guard and fixing H2 are the same edit; doing expand without
  it would ship a third instance of the bug.
- **D4 — one growth authority.** `src/app/api/workflows.rs:590`'s
  `let growth = kvdag.growth;` becomes
  `let growth = narrow_growth(kvdag.growth, tier);`, so `workflow_run.max_depth`/
  `max_nodes` persist the *effective* limits the `RunGraph` enforces. Today the
  divergence is invisible because nothing reads the persisted values; the first
  `MaxNodesReached` enforcement would make a `--tier low` run's banner disagree
  with its own database row.
- **D9 — one resolver.** Run start fetches the history index
  (`node_history` per node key, one query), calls
  `graph::resolve_assignments` once, and hands the result to **both** `NewRun`
  and `RunGraph::materialise_with` — this is the step that switches
  `src/app/api/workflows.rs:620` off the compatibility wrapper.
- **H3 — one projection.** `workflow.get`'s human and `--json` paths both read
  a single `workflow_detail(...)` helper, returned as `WorkflowGet.detail`.
  `description` comes from the `workflow` row, which WS-C's `create_version`
  refresh keeps equal to the head document (H5). The earlier "read it from the
  head version" wording was not implementable: `kvdag_version` has no
  `description` column.
- **H5 caller side.** `workflow.update`'s handler passes the parsed
  `Definition`'s `description` and `default_tier` into `create_version` so the
  refresh has something to write.
- Growth fields and `delivery_failure` on the run/node projections.

**Tested:** `steer`/`interrupt`/`expand` on `succeeded`, `failed`, and
`cancelled` runs each return `workflow_run_closed` naming the terminal status
(H2, three statuses × three verbs); a rejected proposal returns HTTP-success
shape with a populated `rejected` array; persisted `max_nodes` equals
`RunGraph.growth.max_nodes` for every tier (D4 regression test); `workflow.get`
JSON and human output describe the same node/edge/arg sets (H3); after an update
that changes the description, `workflow.get` reports v2's description alongside
`head_version: 2` (H5).

### WS-F — App glue: notices, pane reconciliation, event emission

**Files:** `src/app/state.rs` (step 1b, landed complete), `src/app/workflow.rs`,
`src/app/mod.rs`, `src/app/api/panes.rs`.

**Slim posture:** `src/app/workflow.rs` is already unconditional; the store
handle calls stay gated.

**Delivers:**

- **Step 1b, `state.rs` shape landing** (nothing else touches this file
  afterwards): `AppState.toast_queue: VecDeque<ToastNotification>`;
  `DagNodeView.growth_notice: Option<String>`, `.depth: u16`,
  `.parent: Option<RunNodeIdx>`; `DagViewState.banner: Option<String>` and
  `.banner_rect: Rect`; `Mode::WorkflowLaunch` plus its
  `mouse_motion_changes_view`/`wants_ascii_input` membership; and
  `ViewState.workflow_launch: WorkflowLaunchState`.
- **H4 — the notice queue (§4 D10).** `AppState` owns the data
  (`toast_queue`, cap 8, oldest evicted) and `AppState::push_toast(t)` decides
  slot-or-queue; **arming the clock stays on `App`**, because `toast_deadline` is
  an `App` field (`src/app/mod.rs:111`) and `App::sync_toast_deadline`
  (`src/app/api.rs:711`) is what sets it. `render` is unchanged — it still draws
  `state.toast`. `show_workflow_notice` (`src/app/workflow.rs:1384`) calls
  `push_toast` instead of assigning `state.toast` in its `ToastDelivery::Karvex`
  arm; the `Terminal`/`System` arms are untouched because they never contend for
  the slot. `take_pending_announcements` loses its defensive node-first ordering
  comment because ordering is no longer a workaround: node notices and the run
  notice both render, node first.
  - **The trap:** `sync_toast_deadline` only re-arms when
    `self.state.toast != previous_toast`. Popping the queue into the slot must
    therefore arm the deadline **unconditionally**, not by calling
    `sync_toast_deadline` — two notices with identical `kind`/`title`/`context`
    compare equal, the deadline would never re-arm, and the queue would stall
    permanently behind an immortal toast.
  - The pop happens where the expiry already happens, in `App::handle_scheduled_tasks`
    (the path `src/app/mod.rs:2454-2482` exercises), so a queue non-empty at
    expiry refills the slot instead of clearing it.
- **D7 — creates survive queue overflow.** The drop-oldest eviction is
  `src/app/workflow.rs:472-477` (`PENDING_WRITE_BUDGET = 4096` at `:57`), which
  is a WS-F file; the outline assigned D7's *shape* to WS-A/WS-C and left its
  only behavioural change unowned. WS-F changes the eviction to scan from the
  front for the first non-create `StoreWrite`, and to grow past the cap while
  marking `mark_persistence_degraded()` (`:504`) when the queue is all creates.
- **H6 — pane-close reconciliation (§4 D14).** `App::close_pane`
  (`src/app/api/panes.rs:1523`) calls `observe_workflow_pane_exit` before the
  pane is removed, mirroring what the `AppEvent::PaneDied` path already does at
  `src/app/api.rs:181`. That single edit also covers the TUI: `NavigateAction::ClosePane`
  routes through `close_focused_pane_via_api_requires_confirmation`
  (`src/app/input/navigate.rs:592`) → `runtime_pane_close` → `App::close_pane`.
  (`AppState::close_pane` at `src/app/actions.rs:2032` is `#[cfg(test)]` and is
  not a production path — the outline's justification for the backstop was
  wrong.) What `close_pane` genuinely does **not** cover is bulk removal:
  `handle_tab_close` (`src/app/api/tabs.rs:227`) and `handle_workspace_close`
  (`src/app/api/workspaces.rs:298`) drop every pane they own without ever calling
  it. That is what `App::reconcile_workflow_pane_bindings()` is for — synthesise
  `PaneExited` for every bound pane no longer present in the layout — called at
  the top of `tick_workflow_engine`. The engine already runs a fixed-cadence tick
  while a run is live (`WORKFLOW_TICK_INTERVAL = 20s`, `src/app/workflow.rs:51`;
  the deadline is re-armed in `settle`, `:414-433`), so detection is bounded at
  one tick for tab and workspace close and is immediate for the two direct paths.
- `emit_workflow_event` arm for `WorkflowEvent::GrowthLimited`; growth-limit
  notices raised through `push_toast`.

**Tested:** the H4 regression test **cannot** be written against
`AppState::test_new()` alone — "both notices render" is only observable across an
expiry, and the expiry clock is on `App`. The executable shape is the one
`notification_show_api_karvex_toast_expires` (`src/app/mod.rs:2454-2482`) already
uses: `test_app()` with `toast_config.delivery = Karvex`, push two workflow
notices in one batch, assert the node notice is in the slot and the run notice is
in `toast_queue`, then `app.handle_scheduled_tasks(app.toast_deadline.unwrap(), false)`
and assert the run notice is now in the slot **with a fresh
`toast_deadline`** — including the identical-content case that would stall a
naive `sync_toast_deadline` pop. Pure-`AppState` tests still cover the data:
`push_toast` evicts oldest at cap and never drops the head. Also: a bound pane
removed from the layout produces exactly one
`PaneExited` engine input on the next tick and none thereafter (H6); a tab close
and a workspace close each reconcile within one tick; a node whose
pane is closed reaches a terminal status rather than staying `running`; a
`pending_writes` queue at `PENDING_WRITE_BUDGET` full of creates never evicts a
`RunNodeCreated` and sets `persistence_degraded` (D7); `render`
still takes `&AppState`.

### WS-G — DAG overlay: run banner and per-node growth notice

**Files:** `src/ui/workflow_dag.rs`.

**Delivers:**

- A fifth overlay band. `overlay_areas` (`ui/workflow_dag.rs:128`) returns a
  4-tuple today (`header, graph, detail, footer`) and becomes a 5-tuple, so every
  call site and existing geometry test is **re-destructured** even though none of
  their pinned numbers change. `BANNER_HEIGHT: u16 = 1` is allocated **only when
  `banner.is_some()`**, and in the priority order footer → banner → graph →
  detail, so a short terminal loses the detail strip before it loses a guardrail
  breach. Because the band is zero-height when absent, **every existing geometry
  assertion keeps its exact pinned numbers** (`ui/workflow_dag.rs:1595-1602`,
  `a_tiny_area_still_partitions_without_panicking` at `:1605`); new assertions
  cover the banner-present case only.
- The banner text is the run's last growth limit:
  `growth limited · max_nodes 12 reached · 2 of 4 requested nodes created`.
- A per-node growth notice rendered inside the proposing node's box, in the
  palette's warning slot, truncated with the existing `truncate_end`.
- The 20x8 "terminal too small" branch is untouched; the banner is suppressed
  with the rest of the chrome.

**Non-negotiables (unchanged from Phase 1):** layout runs once in
`compute_view_internal`; `render_workflow_dag` only draws; hit-testing reads the
same stored rects; colours from `Palette` semantic slots. Selection survives
expansion for a better reason than the outline gave: `carried_selection`
(`:250-258`) re-finds the previous selection **by instance path**, not by index,
so it is correct under appends *and* reorders. It falls back to
`next.nodes.first()` when the path is gone, which is the behaviour a test should
pin alongside "appending nodes does not move the user's selection".

**Tested:** `overlay_areas` with `banner: None` returns byte-identical rects to
today; with `banner: Some` the detail strip yields first and the graph keeps at
least one row; hit-test still agrees with stored geometry when a banner is
present; selection survives appending five nodes to a graph.

### WS-H — TUI workflow launcher and the run-start tier prompt

**Files:** `src/ui/workflow_launch.rs` (new), `src/ui.rs`,
`src/app/input/workflow_launch.rs` (new), `src/app/input/mod.rs`,
`src/app/input/modal.rs`, `src/config/model.rs`, `src/config/keybinds.rs`,
`docs/next/website/src/data/config-reference.json`.

**Delivers** the flow that makes "tier prompt at run start" real (§4 D18). One
modal, three sections, reusing the existing modal language:

1. workflow list (from the in-process store path, same one `workflow.list` uses);
2. one single-line input per required arg with no default — the same input
   widget the DAG's steer line already uses, so there is no new text-entry model;
3. a tier row (`auto · max · high · medium · low`), pre-selected from
   `WorkflowSummary.default_tier` — which `workflow.list` already returns
   (`src/api/schema/workflows.rs:258`) — so the row needs no new storage and no
   new query (§4 D17).

Opened from `keys.open_workflow_dag` when no run exists — replacing today's
dead-end toast (`src/app/workflow.rs:855-863`) — and from a new
`keys.open_workflow_launcher` binding. Confirm starts the run through the same
in-process path `workflow.run` uses. `Esc` → `leave_modal`.

**Correction to the outline:** it names `centered_button_row`, which is
`pub(super)` (`src/ui/widgets.rs:252`) and unreachable outside `crate::ui`. The
reachable API is `modal_stack_areas` (`:78`, `pub(crate)`) plus
`action_button_row_rects` (`:151`) / `action_button_width` /
`action_button_text`. Use those. There is no `Modal` enum — modals are `Mode`
variants with per-mode handlers in `src/app/input/{modal,overlays,settings}.rs`,
and this follows that pattern.

**Tested:** `AppState::test_new()` mode round-trip in and out of
`Mode::WorkflowLaunch`; a workflow with a required, defaultless arg cannot be
confirmed until the arg is filled; the tier selection is what reaches the start
call; `python3 scripts/config_reference_check.py` green for the new keybind.

### WS-I — CLI

**Files:** `src/cli/workflow.rs`, `src/cli/spec.rs`, `src/cli/runtime.rs`,
`src/cli.rs`, `tests/cli/workflow.rs` (**new**), `tests/cli/mod.rs`.

**Delivers:**

```
kvx workflow node expand <run-id> <path> --template <key> --label <l> [--input k=v]... [--count N] [--json]
```

Six coupled surfaces, all hand-maintained: `VERB_PATHS`
(`src/cli/workflow.rs:33-47`, `cfg(test)`-only), the clap subcommand in
`workflow_node_command()` (`src/cli/spec.rs:940-976`), the `cli.rs` dispatch arm,
the `Method` variant, the manual parser, and the parity test
`workflow_verbs_match_between_manual_parser_and_spec` (`src/cli/spec.rs:1437`)
that compares the first two.

Plus the CLI half of "a rejection is always surfaced": `workflow run show` prints
a `growth:` line (`3 of 12 nodes · limited: max_nodes reached at 14:22`) whenever
the run has a growth limit recorded; `workflow node show` prints the node's last
rejection and its `delivery_failure` (closing the retest's finding 3 as a side
effect); and `--json` on both carries the same fields (H3 parity).

`kvx workflow node complete` needs **no** change: the in-result `expand` channel
posts `result.json` verbatim already.

**Tested:** in-crate — each verb parses to the right `Method`; the parity trio
green with the new verb; `--input k=v` with an `=` in the value parses whole.
End-to-end verb behaviour (real binary, real server) goes in a **new**
`tests/cli/workflow.rs`, which must be registered with a `mod workflow;` line in
`tests/cli/mod.rs` — there is no workflow module under `tests/cli/` today. The
`#![cfg(not(target_os = "macos"))]` that exempts this suite on macOS is on the
crate root `tests/cli.rs:1` and is inherited; do not re-declare it in the new
file.

### WS-J — End-to-end and docs

**Files:** `tests/workflow_headless.rs`, `tests/fixtures/workflow/expand.toml`,
`docs/next/website/src/content/docs/{,ja/,zh-cn/}workflows.mdx`,
`docs/next/CHANGELOG.md`, `docs/next/README.md`.

**The fixture uses `runner: "command"`**, per Phase 1's rule — a template node
whose command writes a `result.json` and calls `kvx workflow node complete`, and
a root node whose result carries `expand: [{ template: "worker", count: 4 }]`.
Deterministic, no network, no API cost, and it exercises the real engine path.

**Three e2e scenarios:**

1. **Accepted expansion.** Root proposes 2 workers; both are created, both run,
   both appear as `workflow.node.created` events with `parent_path` set and
   `depth: 1`, the downstream fan-in node waits for all three, and the run
   succeeds.
2. **Truncated expansion is surfaced.** A `--tier low` run (effective
   `max_nodes: 12`) proposes more workers than fit. Assert: some children are
   created, a `workflow.growth.limited` event names the exact limit and the
   requested/accepted counts, `workflow.run.get` reports `growth_limited`, and
   the run still succeeds. **This is the phase's headline guarantee and it needs
   an e2e, not a unit test.**
3. **Disallowed template is rejected without side effects.** A proposal naming a
   template outside `expand_allow` returns a rejection, creates zero nodes, emits
   `workflow.growth.limited`, and leaves `nodes_total` unchanged.

**Running the e2e from inside a live karvex session.** `tests/workflow_headless.rs`
spawns a real server and speaks to it over a socket, so an inherited karvex
environment silently points the harness at the *installed* server. Scrub it:

```bash
env -u KARVEX_SOCKET_PATH -u KARVEX_CLIENT_SOCKET_PATH -u KARVEX_SESSION \
    -u KARVEX_PANE_ID -u KARVEX_ROLE -u KARVEX_CONFIG_PATH \
    -u KARVEX_WORKFLOW_DB_PATH -u KARVEX_WORKFLOW_RUNS_DIR \
    -u KARVEX_WORKFLOW_NODE_DIR -u KARVEX_WORKFLOW_NODE_PATH \
    just test
```

The workflow ones matter twice over here: the fixture's node command runs under
`KARVEX_WORKFLOW_RUNS_DIR`, and an inherited `KARVEX_WORKFLOW_DB_PATH` would make
the test write into the developer's real workflow database. A failure that only
reproduces inside a karvex pane is this, not a bug in the change.

**Docs.** `docs/next/website/src/content/docs/workflows.mdx:63` currently reads
"`max_depth` and `max_nodes` … only start to matter once a later release adds
node-driven expansion." That sentence is Phase 2's doc trigger; it and the
surrounding paragraph are rewritten, plus a new section on templates,
`expand_allow`/`expand_max`, the tier narrowing table, and `default_tier`.
`just release-docs-check` requires `ja/` and `zh-cn/` counterparts with matching
heading outlines for **every** new heading — budget for it.

---

## 2. Ordered Phase 2 workplan

Steps sharing a number run in parallel.

| # | Step | Files | Delivers | Tested by |
|---|---|---|---|---|
| 1a | Shape landing: model | `src/workflow/model.rs`, `src/workflow/tier.rs` (alias only), **plus the struct-literal sweep**: `engine/graph.rs`, `engine/tests_support.rs`, `workflow/layout.rs`, `ui/workflow_dag.rs`, `app/workflow.rs`, `app/input/modal.rs`, `app/input/navigate.rs` | `StoreWrite::RunNodeCreated`/`RunEdgeCreated`, `EngineInput::ExpandProposed`, `WorkflowEvent::GrowthLimited`, `NodeAssignment`, `HistoryIndex`, `RunGraph.assignments`, `RunNode.assignment_reason` | `cargo build` + `just check-slim` green; the pure-layer grep test still passes |
| 1b | Shape landing: app state | `src/app/state.rs` | `toast_queue`, `DagNodeView`/`DagViewState` fields, `Mode::WorkflowLaunch`, `WorkflowLaunchState` | `AppState::test_new()` mode round-trip |
| 1c | Shape landing: wire | `src/api/schema/workflows.rs`, `schema.rs`, `response.rs`, `events.rs`, `src/api/mod.rs`, `src/api/server.rs`, `src/api/subscriptions.rs`, regenerated artifact | `workflow.node.expand`, `workflow.growth.limited`, `WorkflowGrowthLimit`, growth fields, `WorkflowDetail` (additive on `WorkflowGet`) | serde round-trip; artifact test green on both feature legs; extended banned-word test |
| 2a | Expansion core | `src/workflow/engine/expand.rs`, `engine/graph.rs` | `evaluate`/`commit`, `resolve_assignments`, `materialise_with` + compat wrapper, narrowing idempotence | table-driven unit tests (WS-A) |
| 2b | Store | `src/workflow/store/*`, `migrations/0002_*.surql` | create paths + `spawned` writer, growth assertion, `started_at` bind, `node_history` | `kv-mem` store tests incl. the H1 reload case |
| 2c | Engine wiring | `src/workflow/engine/mod.rs`, `engine/complete.rs` | `ExpandProposed` arm, `expand` stripping, first-pass/schema facts | digest-identity test; ordering-invariant test |
| 3a | Handlers + guards | `src/app/api/workflows.rs`, `src/app/api.rs` | expand handler, `require_open_run` (H2), narrowed growth (D4), one resolver + `materialise_with` (D9), `workflow_detail` (H3), version-metadata refresh caller (H5) | handler tests + extended `not_implemented` sweep |
| 3b | App glue | `src/app/workflow.rs`, `src/app/mod.rs`, `src/app/api/panes.rs` | notice queue (H4), create-safe queue eviction (D7), pane reconciliation (H6), `GrowthLimited` emission | `test_app()` notice/expiry tests + reconciliation + eviction tests |
| 3c | DAG banner + node notice | `src/ui/workflow_dag.rs` | fifth band (zero-height when absent), per-node notice | geometry tests incl. the unchanged no-banner case |
| 3d | CLI | `src/cli/workflow.rs`, `spec.rs`, `runtime.rs`, `cli.rs`, `tests/cli/{workflow.rs,mod.rs}` | `node expand`, growth/rejection rendering, `--json` parity | parser/spec/dispatch parity trio |
| 4 | Launcher + tier prompt | `src/ui/workflow_launch.rs`, `src/ui.rs`, `src/app/input/{workflow_launch,mod,modal}.rs`, `src/config/{model,keybinds}.rs`, config-reference JSON | `Mode::WorkflowLaunch`, arg lines, tier row seeded from `WorkflowSummary.default_tier` | mode round-trip + `config_reference_check.py` |
| 5 | E2E | `tests/workflow_headless.rs`, `tests/fixtures/workflow/expand.toml` | the three WS-J scenarios (API-observable assertions only) | itself, with the env scrub above |
| 6 | Docs | `docs/next/website/src/content/docs/{,ja/,zh-cn/}workflows.mdx`, `docs/next/CHANGELOG.md`, `docs/next/README.md` | templates/expansion/tiers docs in all three locales | `just release-docs-check` green |

**Merge gate:** `just check` green. That is the whole gate: on Unix `check` is
`ci windows-lint` plus the maintenance script tests, and `ci` already runs the
default-feature nextest **and** `just check-slim` (the `--no-default-features`
clippy + nextest leg). Step 1a additionally runs `just check-slim` on its own,
before any parallel work — not as a formality, but because the expansion core is
unconditional code that must not acquire a store dependency, and a step-1
regression would be found four steps later otherwise.

**Every step must leave the tree compiling.** Two places where the outline did
not: step 1a's new `RunNode`/`RunGraph` fields (fixed by the struct-literal
sweep, §1) and step 2a's `RunGraph::materialise` signature (fixed by adding
`materialise_with` and keeping the old entry point until WS-E migrates at 3a).

The outline's separate "`default_tier` plumbing" step is **deleted**: the
document-level tier, its column, its bind, its wire field, and its use as the
run-start default all shipped in Phase 1 (§0.5). What was actually missing —
refreshing the `workflow` row on `create_version` — is H5 and lives in steps 2b
and 3a. Step 4 therefore has no upstream dependency beyond 1b.

---

## 3. Interfaces frozen at the start of Phase 2

Decided up front; changing them mid-phase means coordinating edits across
parallel agents.

1. **`ExpandProposal` / `ExpandOutcome` / `ExpandRejection` / `ExpandLimit`** —
   WS-A owns, WS-B and WS-E consume. The six existing `ExpandRejection` variants
   (`expand.rs:23-30`) keep their exact shapes; **two** are additive —
   `Truncated` (§4 D2) and `UnknownInput` (§4 D3) — for eight in total.
   `ExpandProposal.count` stays `Option<u16>`; the wire's `u32` is narrowed with
   `u16::try_from` at the handler boundary.
2. **`expand::evaluate` / `expand::commit` signatures** — `evaluate` is pure and
   takes `&RunGraph`; `commit` takes `&mut RunGraph` and returns effects. No
   variant of expansion mutates state during validation.
3. **`StoreWrite::RunNodeCreated` / `RunEdgeCreated` field sets** — WS-A owns,
   WS-C consumes. Creates are **never evicted** by queue overflow (§4 D7).
4. **`NodeAssignment { model, effort, reason }`,
   `graph::resolve_assignments(&Kvdag, Tier, &HistoryIndex)`, and
   `RunGraph::materialise_with(..)` beside the retained
   `RunGraph::materialise(kvdag, run_id, tier)` wrapper** — WS-A owns; WS-C
   and WS-E consume. There is exactly one caller of `tier::resolve` in the
   subsystem after this phase (`store/mod.rs:1220` is the one that goes).
5. **`HistoryIndex = BTreeMap<NodeKey, NodeHistory>` lives in
   `src/workflow/tier.rs`** — WS-A owns the *type*, because
   `resolve_assignments` is unconditional and cannot name a type declared inside
   the `#[cfg(feature = "workflow")]` store. WS-C owns
   `queries::node_history(workflow, node_key, window)`, the only thing that
   fills it; WS-E consumes both.
6. **The wire additions in WS-D's table** — names, field sets, and the single new
   event kind. Every one is `#[serde(default)]`-tolerant and additive, including
   `WorkflowGet.detail`: no existing response field changes type.
7. **`workflow.node.expand` authentication = the node token**, same as
   `workflow.node.report`. A rejection is a success response, not an error.
8. **Instance-path grammar for expansion children:** `<parent>/<template>/<n>`,
   `n` 1-based per `(parent, template)`, monotone within a run. The DAG's
   selection anchoring and the store's `run_node_instance` unique index both
   depend on it.
9. **`AppState.toast_queue` + `AppState::push_toast`, with the deadline staying
   on `App`** — WS-F owns; every workflow notice producer goes through
   `push_toast`. `render` continues to draw `state.toast` only. The queue-pop
   arms `toast_deadline` directly rather than through `App::sync_toast_deadline`,
   whose inequality guard would skip an identical successor.
10. **`DagViewState.banner` / `banner_rect` and the zero-height-when-absent
    rule** — WS-F lands the fields, WS-G owns the geometry, and `overlay_areas`
    becomes a 5-tuple. Existing pinned geometry *numbers* must not change; the
    destructuring in those tests does.
11. **The node prompt/output contract is unchanged.** `task.md`,
    `output_schema.json`, `result.json`, `kvx workflow node complete` all keep
    their Phase 1 meaning; `expand` is an *optional additional* top-level key
    that never reaches the schema, the payload, or the digest (§4 D6).

---

## 4. Decisions

**D1 — Expansion logic is unconditional; only its persistence is gated.**
`expand.rs` and `graph.rs` are pure and must compile under
`--no-default-features`. The store writes are behind `#[cfg(feature = "workflow")]`.
A slim build can run an expanding workflow in memory; it just does not persist
it, which is exactly the existing Phase 1 posture.

**D2 — Partial acceptance is the interesting case, so it is a first-class
outcome.** `ExpandProposal.count` defaults to 1. A proposal for 4 with budget for
2 **creates 2 and reports the shortfall**: `ExpandOutcome { accepted: Vec<..>,
rejected: Vec<ExpandRejection> }` where the shortfall is
`ExpandRejection::Truncated { template, requested, accepted, limit: ExpandLimit }`.
Accept-all violates the limit; reject-all wastes budget and is precisely the
"silent truncation" §3.4 forbids — reporting the truncation is what makes
partial acceptance legitimate.

**D3 — `inputs` is an override channel, not a second unvalidated renderer.**
`Kvdag::try_new` already proves every `{{slot}}` in a node's `prompt_template` —
templates included — resolves to a declared arg or an inbound edge port. So
`inputs` may only *override* a slot that already exists: a key naming no `{{slot}}`
in the template is `ExpandRejection::UnknownInput { template, name }`. This keeps
one validated renderer and makes the dynamic channel strictly narrower than the
static one, rather than a hole beside it.

**D4 — Inherited edges, and what happens when two children share a port.**
An accepted child gets (a) a `sequence` run_edge parent→child, so it cannot start
before its parent settles, and (b) a copy of each of the parent's **outbound**
edges with `from = child`, preserving kind/payload/port/condition — the fan-in
point survives, as §3.4 requires. When ≥2 inbound data edges settle on the same
port of a downstream node, that port's `{{slot}}` renders as a **JSON array** in
child-creation order. A port with exactly one settled edge renders the scalar
payload exactly as today, so no existing behaviour changes; duplicate static
ports remain rejected by `Kvdag::try_new`, so an array can only ever arise from
expansion.

**D5 — One new wire event, not two.** `workflow.node.created` already exists and
already means "a node entered the run graph"; `WorkflowRunNodeInfo` already
carries `parent_path` and `depth`. Adding `workflow.node.spawned` would give
clients two events for one fact and a reconciliation problem. Expansion children
therefore emit `workflow.node.created`, and Phase 2 adds exactly
**`workflow.growth.limited`** — the one thing no client can derive. The journal
keeps all four `expand_*` kinds (`RunEventKind` and the `0001_init.surql`
`ASSERT` list already permit them); the journal is the audit trail, the wire is
the contract, and they are allowed to differ.

**D6 — `expand` is stripped before the result becomes a result.**
`complete::check` implements only `type`/`required`/`properties`/`items` and
never rejects unknown top-level keys, so `{"plan": …, "expand": [...]}` validates
today by accident. Left alone it would flow into `NodeResult.payload`,
`summarise`, `digest`, and the persisted checkpoint — and `digest` is what Phase
3's restore uses for cross-version compatibility. A quiet Phase 2 choice would
become a Phase 3 correctness bug. So `Engine::report` lifts `expand` out **before**
validation and **before** `complete::node_result`, and WS-B's headline test
asserts the digest is byte-identical with and without it. The proposal survives
in the `expand_proposed` journal payload, which is its correct home.

**D7 — Create-shaped store writes, and creates are never dropped.**
`StoreWrite::RunNode` → `write_run_node` is find-then-`UPDATE` and errors on a
missing row; there is no create path outside `create_run`. Phase 2 adds
`RunNodeCreated` and `RunEdgeCreated`. Because `pending_writes`
is a bounded, drop-oldest queue (`src/app/workflow.rs:144`, cap
`PENDING_WRITE_BUDGET = 4096` at `:57`, `pop_front` on overflow at `:472-477`), a
naive overflow could drop a create while
keeping its update — producing a permanent decode error for that node. So
eviction scans from the front for the first **non-create** entry; if the queue is
all creates it grows past the cap and marks the run `persistence_degraded`
(`mark_persistence_degraded`, `:504`). The
in-memory `RunGraph` stays authoritative either way.
**Ownership:** the *shape* is WS-A's and the *store side* is WS-C's, but the
eviction rule is a change to `src/app/workflow.rs` and therefore **WS-F's**, at
step 3b. The outline left it unassigned, which is how a decision ships as prose.

**D8 — `NodeHistory` gets a producer and two truthful inputs; the other three are
declared inert.** Shipping "auto over `NodeHistory`" against an all-zero record
would be a silently inert feature. Phase 2 therefore adds the aggregation query
*and* the two facts that can be truthful today — `first_pass_succeeded` and
`schema_failures`, both already known inside `Engine` and both needing only a
column. `watchdog_interventions` stays 0 until Phase 4 writes it (its
`resolve_auto` clause is correct but dormant, and `recent_first_pass_failures`
covers the same escalation rung). `mean_tokens` is carried but **not consulted**
by `resolve_auto` (`tier.rs:241-284` never reads it), because
`run_node.total_tokens` is documented as permanently 0
(`model.rs:1085-1095`). This is written into the doc comment on `NodeHistory` so
nobody later reads a zero as a measurement.

**D9 — One resolver, not two agreeing resolvers.** `tier::resolve` is called
today from `engine/graph.rs:49` (in-memory, drives the DAG and `SpawnSpec`) and
`store/mod.rs:1220` (durable, drives `node show`), both passing `None`, so they
agree by accident. Rather than add a cross-check test, Phase 2 **removes the
second caller**: `graph::resolve_assignments` runs once at run start, its
`BTreeMap<NodeKey, NodeAssignment>` is carried on `NewRun` and on
`RunGraph.assignments`, and `materialise_run_nodes` writes it verbatim. The table
covers **every** kvdag node including templates, so an expansion child resolves
from the same table with no mid-run history query. The §7.3 reason string gets
the `run_node.assignment_reason` column it has always needed.

**D10 — The minimal notice design that lets node-level and run-level notices both
surface.** `state.toast` stays the single rendered slot; `AppState` gains
`toast_queue: VecDeque<ToastNotification>` (cap 8) and a `push_toast` helper.
Free slot → render immediately and arm the deadline; occupied → enqueue; expiry →
pop. `render` is untouched, existing non-workflow toast producers are untouched,
and `take_pending_announcements`'s node-first ordering stops being a workaround
and becomes just an ordering. This is the smallest change that makes "a rejection
is always surfaced" true rather than aspirational.

**D11 — Three independent surfaces, only one config-gated.** Because
`ToastDelivery::default() == Off`, the toast channel alone cannot carry the
guarantee. A growth rejection therefore lands on: the API (`workflow.growth.limited`
event + `WorkflowRunInfo.growth_limited` + `WorkflowRunNodeInfo.growth_limited`),
the DAG overlay (run banner + per-node notice), and the CLI (`run show` /
`node show`). The toast is the fourth and the only optional one. Changing the
toast default is **out of scope** (§5 R-11).

**D12 — The growth budget is monotone.** `max_nodes` counts every materialised
`RunNode` in the run regardless of status. A failed, skipped, or cancelled child
does **not** refund budget. Otherwise a node could fan out indefinitely by
failing, and the ceiling would stop being a ceiling.

**D13 — Static nodes stay at depth 0.** `STATIC_NODE_DEPTH = 0`
(`store/mod.rs:72`, used at `:1237`, with the reasoning in the
`materialise_run_nodes` doc comment at `:1186-1201`) is load-bearing and
correct: expansion depth is not topological depth, and numbering a five-node
chain 0..4 would report a legal graph as past its own ceiling. First-generation
children are depth 1, so `max_depth = 3` permits three generations. `layout()`
ignores `RunNode.depth` and places children by edge topology — which is right,
and is called out here so nobody "fixes" it.

**D14 — Pane-close detection is a direct call plus a reconciliation backstop.**
`AppEvent::PaneDied` (`src/app/api.rs:181`) covers process exit only.
`App::close_pane` (`src/app/api/panes.rs:1523`) gets a direct
`observe_workflow_pane_exit` call, and that one edit covers both the API verb and
the TUI keybinding, because `NavigateAction::ClosePane` reaches it through
`close_focused_pane_via_api_requires_confirmation` (`src/app/input/navigate.rs:592`)
→ `runtime_pane_close`. The outline claimed the TUI path was unreachable from
`App` and cited `AppState::close_pane` (`src/app/actions.rs:2032`); that function
is `#[cfg(test)]` and is not a production path at all. The paths that genuinely
bypass `close_pane` are the bulk ones — `handle_tab_close`
(`src/app/api/tabs.rs:227`) and `handle_workspace_close`
(`src/app/api/workspaces.rs:298`) — plus any future one. So
`App::reconcile_workflow_pane_bindings()` runs on the existing 20-second live-run
tick as the backstop: bounded at one tick for bulk closes, immediate for the two
direct paths, and future-proof without chasing N call sites.

**D15 — One authority for `started_at`.** The app stamps
`started_at_unix_ms` once; `NewRun` carries it; `create_run` binds it explicitly;
migration `0002` drops `DEFAULT time::now()` from `workflow_run.started_at` so
the database can no longer mint a competing value. The 2–3 ms reload drift is a
second clock, and the fix is to delete the second clock rather than reconcile it.

**D16 — One projection for `workflow.get`, and one authority for the metadata it
shows.** The human renderer and `--json` both read `workflow_detail(...)`: that
is H3, and it is a rendering fix. H5 is a *storage* fix and is separate, contrary
to the outline's claim that they are the same defect. `workflow.get`'s
`description` genuinely is stale after an update, but not because two renderers
disagree — because `create_version` (`store/mod.rs:276`) never writes the new
document's `description` or `default_tier` back to the `workflow` row, and
`kvdag_version` has no `description` column to read instead
(`0001_init.surql:30-49`). The fix is to make the `workflow` row track its head:
one write in `create_version`, one projection in `workflow_detail`.

**D17 — The authoring-time tier already exists; Phase 2 only consumes it.**
`04` §7's "chosen by the user at create and at run time" has no coherent TUI
reading, because authoring is file-based — but the storage half already shipped
in Phase 1 and the outline missed it. `Definition.default_tier`
(`definition.rs:32`) parses a document-level `default_tier`, `Definition::tier()`
(`:188`) defaults it to `high`, `create_workflow` persists it to
`workflow.default_tier` (`store/mod.rs:248-262`, column at `0001_init.surql:23`),
`WorkflowSummary.default_tier` exposes it, and run start already prefers it over
the config default (`src/app/api/workflows.rs:586`). Phase 2 therefore adds **no**
tier storage and **no** `kvdag_version.default_tier` column — a second copy would
be a second authority. It adds exactly two things: the launcher's tier row seeded
from `WorkflowSummary.default_tier`, and the H5 refresh so a `workflow.update`
that changes the document's `default_tier` is not silently ignored.

**D18 — The launcher is the smallest thing that makes the run-start tier prompt
real.** Without it there is nothing to attach a prompt to, and the retest's
"no run → toast" dead end stays. It is scoped hard: list, required-arg lines
reusing the DAG steer input, tier row, confirm. No editing, no version picking,
no scheduling.

**D19 — Slim-build posture is stated per workstream and enforced per step.**
`just check-slim` (via `just ci`, via `just check`) is a merge gate, and the API schema artifact is
generated on that leg too.

**D20 — `PROTOCOL_VERSION` is not bumped, and one addition has to be shaped so
that stays true.** It is 19 (`src/protocol/wire.rs:16`). Every Phase 2 addition
is an additive
`Method`/`ResponseResult`/`Subscription`/`EventKind`/field on self-describing
JSON; the binary render/input wire format is untouched. The one thing that would
break this is re-typing `ResponseResult::WorkflowGet.workflow` from
`WorkflowSummary` to `WorkflowDetail` — an existing field changing shape, which
*is* an incompatible change to a published protocol. So `WorkflowDetail` arrives
as a new sibling field (WS-D), not a replacement. Per CLAUDE.md a bump is
for *incompatible* changes to an already-published protocol, and
`src/cli/protocol_guard.rs` enforces exact equality — a gratuitous bump forces
every user to restart their server for nothing.

**D21 — No integration-asset change, so no `*_INTEGRATION_VERSION` bump.**
Phase 2 touches no `src/integration/assets/`.

---

## 5. Risk register for Phase 2

| # | Risk | Mitigation in the plan |
|---|---|---|
| R-1 | `NodeHistory` ships inert: three of five inputs have no truthful source, so `auto` behaves exactly like today's `None` | D8 — build the aggregation **and** the two facts that can be truthful (`first_pass_succeeded`, `schema_failures`), document the other three as dormant on the type itself, and do not let `resolve_auto` consult `mean_tokens` |
| R-2 | Two `tier::resolve` call sites drift, so a node's DB row and the DAG view disagree about which model it ran on | D9 — delete the second caller rather than test that they agree; `materialise_run_nodes` writes `NewRun.assignments` verbatim |
| R-3 | Growth narrowing diverges: `RunGraph` enforces narrowed limits while `workflow_run` persists the version ceiling, so a `--tier low` run's banner contradicts its own row | WS-E narrows once at `src/app/api/workflows.rs:590`; WS-A proves `narrow_growth` idempotent; WS-C adds the `run.growth <= version.growth` assertion `NewRun`'s doc has falsely claimed since Phase 1; WS-E has a per-tier regression test |
| R-4 | An expansion child exists in memory before its row exists, and its first status write races or outlives its own create write | D7 — creates are enqueued first in the same FIFO and are never evicted by overflow; WS-B has an effect-ordering test, WS-F owns the eviction change in `src/app/workflow.rs:472-477` and its overflow test, and WS-C a create-then-update queue-drain test |
| R-5 | An `expand` key silently lands in `NodeResult.payload` → `digest` → `node_checkpoint.digest`, breaking Phase 3 restore compatibility | D6 — stripped before validation and before `node_result`; WS-B's headline test asserts digest identity with and without the field |
| R-6 | The headline guarantee ("a rejection is always surfaced") is shipped on a channel that is a single slot and off by default, making it untestable and in practice false | D10 fixes the slot (H4 is a **prerequisite**, sequenced at step 3b before the CLI and DAG surfaces are believed); D11 puts the guarantee on three surfaces of which only the toast is config-gated; WS-J scenario 2 is an e2e, not a unit test |
| R-7 | A `count: 4` proposal with budget for 2 has only accept-all (violates the limit) or reject-all (wastes budget and truncates silently) | D2 — `Truncated` is a first-class outcome carrying requested/accepted/limit |
| R-8 | `ExpandProposal.inputs` becomes a second, authoring-unvalidated channel into the template renderer, bypassing the port coverage `Kvdag::try_new` enforces | D3 — `inputs` may only override an existing `{{slot}}`; unknown keys are a rejection |
| R-9 | Adding a DAG banner breaks every pinned geometry test and the 20x8 too-small path | WS-G — the band is zero-height when absent, so no-banner geometry is byte-identical (the 4→5-tuple change re-destructures those tests without changing a number); the too-small branch is untouched and has its own test |
| R-10 | The TUI bullets assume surfaces that do not exist (`centered_button_row` is `pub(super)`; there is no create-or-run flow; `DagNodeView` has no growth-notice or depth field) | WS-F lands every field in step 1b; WS-H uses `action_button_row_rects`; D18 scopes the launcher; D17 drops the imaginary create wizard **and** the imaginary new tier storage. Note `DagNodeView.delivery_failure` already exists (`src/app/state.rs:830`) — the gap there is on the API side, not in the view |
| R-11 | Notices remain default-off, so a stock config still shows nothing | **Accepted and out of scope.** The guarantee rests on the API/DAG/CLI surfaces (D11). Changing `ToastDelivery::default()` is a cross-cutting UX decision affecting non-workflow notices and belongs in its own change |
| R-12 | Expansion multiplies H6: every child is another pane that can vanish and leave a node `running` forever | D14 — a direct `observe_workflow_pane_exit` in `App::close_pane` covers the API verb and the TUI keybinding, and reconciliation on the live-run tick covers tab/workspace close; both land at step 3b before the e2e |
| R-17 | A step lands that does not compile on its own — a new `RunNode`/`RunGraph` field with unswept struct literals, or a `materialise` signature change whose call sites belong to a later step's owner | §1's step-1a struct-literal sweep names all ten construction sites; WS-A adds `materialise_with` and keeps `materialise` as a wrapper so step 2a is self-contained; §2's "every step must leave the tree compiling" makes this a gate, not a hope |
| R-18 | The queue pop stalls because `App::sync_toast_deadline` skips re-arming for an equal toast, and the "always surfaced" guarantee silently reverts to one slot | WS-F arms the deadline directly on pop and pins the identical-content case in a `test_app()` test (§WS-F "Tested") |
| R-13 | The slim build breaks late, after expansion logic has quietly acquired a store dependency | Step 1a runs `just check-slim` before any parallel work; every workstream states its posture; the Phase 1 pure-layer grep test still guards `src/workflow/{model,engine,layout,tier}` |
| R-14 | Docs translation parity blocks the release: new headings in `workflows.mdx` need `ja/` and `zh-cn/` counterparts | Step 6 is a named step with `just release-docs-check` as its test, not an afterthought; `workflows.mdx:63`'s "a later release adds node-driven expansion" sentence is the explicit trigger |
| R-15 | Six coupled CLI surfaces drift (`VERB_PATHS`, clap spec, dispatch, `Method`, parser, handler) | WS-I owns all of them in one step and the existing `src/cli/spec.rs:1437` parity test is extended with the new verb |
| R-16 | Scope creep from Phase 3/4 — restore, run browser, watchdog, review, blocker taxonomy | Explicitly out. Phase 2 touches `watchdog_interventions` only as a read that returns 0, and emits no `Succession::Blocked` beyond what exists |
