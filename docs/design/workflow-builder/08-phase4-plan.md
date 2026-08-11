# Phase 4 implementation plan — anti-stuck watchdog + self-improvement review cycle

Release target: **v0.13.0**.

This is the build contract for Phase 4, written against the tree at
`integration/oct-fixes` (`8e817282` — the four post-v0.12.0 workflow fixes,
combined `just check` green at 4,208 tests). It is written the way
`07-phase3-plan.md` was: every load-bearing premise re-checked against the
code, not the prose, and where `05-phase-plan.md` §5.3's Phase 4 outline or
`04-kvdag-and-execution.md` §6 is stale, this document supersedes it and says
why. The single most common defect class in the Phase 3 build was a premise
verified by reading another document instead of the tree (§8 E-17 there,
"verify premises against the tree, not the prose"); §0 below is that rule
applied up front.

Phase 4's definition of done:

> **A stuck node gets un-stuck or honestly surfaced, and a finished run can
> teach the workflow.** Every running node is classified each engine tick by
> materiality — real tool calls, token deltas, detection-snapshot changes,
> artifact changes — never "the screen redrew". A node with no material
> progress walks a bounded escalation ladder: nudge → structured re-prompt →
> restart from a `partial` checkpoint → `Blocked` and surfaced, every rung
> journalled and counted on `run_node.watchdog_interventions`. A node running
> past its idle budget with zero tool calls and zero usage is stuck no matter
> how busy the screen looks. `workflow.node.restart` finally seeds from the
> latest `partial` checkpoint, as `05` promised it would from Phase 4 on. And a
> finished run can be reviewed: one 1:1 interviewer node per interviewed
> teammate revives its target with `claude --resume <sid> --fork-session`
> (never mutating the source transcript), puts the measured record *to* it,
> and records the exchange as an `interrogation` row; a synthesis node
> classifies findings prompt-level vs structural; the human accepts or
> declines per finding; accepted findings compile into a new immutable
> `kvdag_version` with `origin: self_improvement` whose parent is the run's
> version. When a session cannot be revived, findings are stamped
> `interview_mode: "evidence_only"` and never presented as the teammate's own
> account. And a workflow node can **learn its own contract**: `kvx --skill`
> gains a workflow section teaching a node agent `kvx workflow`, the
> self-report contract, and its env vars — pinned against the real CLI by a
> parity test so it cannot drift into a fifth hand-maintained surface that
> lies.

Explicitly **not** in Phase 4: `Monitor` node execution and `Gate`/
`workflow.node.decide` (see §0.4 — both are less built than `04` §4.5 claims,
and neither is on the self-improvement or reliability path), edge-level
structural surgery in compiled review versions (§4 D14 — node-level changes
only in v1), the optional ACP headless executor (§4 D20 — no re-evaluation
trigger has fired), any `workflow delete` (the cheap `archived` toggle ships
instead, §4 D17), multi-run concurrency, and live resume of interrupted
runs. The `kvx --skill` workflow gap, originally descoped with the
documentation work, was **re-scoped into this phase by Karan** ("if it
can't learn then make it a feature part of phase 4") — it is WS-K, a
deliverable with its own tests, not prose; §0.11 says why it is product
behaviour and §4 D21 gives the shape.

---

## 0. Reality check: what the outline predates

Fourteen corrections and confirmations, each verified against
`integration/oct-fixes`. Where a design doc disagrees with the tree, the tree
wins.

1. **`src/workflow/engine/watchdog.rs` already exists — as a 35-line type
   stub.** `ProgressClass` (four-way) and `Escalation` (four-rung ladder) are
   there (`watchdog.rs:9-35`), landed in Phase 1 with the evidence types.
   `05` §5.3's "add `src/workflow/engine/watchdog.rs`" is really "fill it":
   the classifier, the ladder driver, and the tick integration are all new.

2. **The watchdog's evidence plumbing is consumer-only; every producer is
   missing.** The engine handles `EngineInput::ProgressObserved`
   (`engine/mod.rs:1409-1444` — accumulates `ProgressTracker`, resets
   `no_progress_streak` on material delta) and the observe-side gate and
   digest helper exist (`observe::progress_observed` at
   `binding/observe.rs:235-243`, `observe::screen_digest` at `:248`). **Zero
   production callers feed any of it** — a repo grep finds only tests. `00`
   §Feature 5's "the detection/journal plumbing it needs lands in Phase 1"
   was true of the engine half only. Phase 4 builds the sampler (§WS-D).

3. **Two of `04` §6.1's four materiality sources have no source at all, and
   the doc's own model comment says so.** `NodeUsage`'s doc
   (`model.rs:1274-1288`): tokens/tool-uses have "no real source… wiring a
   real source would be a new subsystem (transcript tailing/parsing)".
   `usage` is only ever incremented from `ProgressObserved`
   (`engine/mod.rs:1441-1442`), which nothing sends. The bundled claude hook
   cannot substitute: it registers **only** `session` and `stop` actions
   (`integration/assets/claude/karvex-agent-state.sh:15-18`); no
   `PostToolUse` hook is installed for claude, and the other integrations'
   per-tool-use lifecycle hooks sit in `*_REMOVED_LIFECYCLE_HOOK_EVENTS`
   lists — the codebase deliberately moved away from per-tool-call hook
   processes. Phase 4 therefore gets tool calls and tokens from a **bounded
   incremental transcript-delta read** (§4 D3): the transcript path is
   already stored per node and hook-corrected (Phase 3 D6), and a per-tick
   tail read is ~100 lines, not a subsystem. No integration asset changes,
   no `CLAUDE_INTEGRATION_VERSION` bump (it is 8 at HEAD and 8 at v0.12.0,
   `integration/mod.rs:41`).

4. **The four-way classification is effectively three-way for `Running`
   nodes, and `04` §4.5's supporting cast is thinner than written.**
   "External wait" is defined by a declared blocker with `resume_when` — but
   such a node is `NeedsAttention`, not `Running`
   (`complete.rs:57-73`, `missing_result` at `:427-439`), and the `Monitor`
   kind exists only as an enum variant (`model.rs:212`) with no execution
   path. Worse, `04` §4.5's "Gate… Phase 2" never happened: there is **no
   `workflow.node.decide` method anywhere** (the `Method` enum's 18
   `workflow.*` variants at `src/api/schema.rs:240-274` do not include it)
   and no gate modal. The classifier therefore maps `ExternalWait` to the
   skip-set (non-`Running` nodes with a declared resume condition are backed
   off, never escalated) and classifies `Running` nodes three ways; the
   enum stays four-way because the taxonomy is right even where one class is
   currently only reachable by skip (§4 D2).

5. **Restart and `partial` checkpoints: the seams are exactly as designed —
   present, unwired.** `Engine::restart` (`engine/mod.rs:1628-1685`) closes
   the pane, bumps `attempt`, resets binding/result/progress, and reads **no
   checkpoint of any kind**; respawn re-renders `task.md` into the same node
   dir (`spawn::materialise_node_dir` at `binding/spawn.rs:344-356`, doc
   comment "restart reuses the node directory", called from
   `app/workflow.rs:2007-2016`). `CheckpointKind::Partial` round-trips
   (`model.rs:1544-1553`, writer `store/mod.rs:1547-1551`, parser
   `queries.rs:1499-1503`, `0001_init.surql:212`) and the sole production
   checkpoint constructor is `succeed`'s `kind: Result`
   (`engine/mod.rs:1851-1862`). `find_restorable_checkpoints` filters
   `kind = "result" AND schema_valid = true` (`queries.rs:687-707`), so a
   `partial` can never leak into cross-run restore. Nothing to fight;
   everything to write.

6. **Review storage: schema and the replace-guard shipped in Phase 1; zero
   Rust exists.** `review_cycle` (`0001_init.surql:258-267`, incl.
   `interviews: array<record<interrogation>>`) and `review_finding`
   (`:269-294`, incl. `interview_mode ASSERT IN ["resumed","evidence_only"]`)
   plus the DB-side table event `review_finding_replace_requires_replacement`
   (`:306-308` — a table event because SurrealDB skips field ASSERTs on NONE
   options). There are **no row structs** (`records.rs:248` is a comment:
   "review (schema present; no writer)"), no `StoreWrite` variants, no
   queries, no wire types. `VersionOrigin::SelfImprovement` already exists in
   the enum (`store/mod.rs:129-134`) and the DB assert
   (`0001_init.surql:35-36`) — note the real enum is
   `Authored | Imported | SelfImprovement | RestoreRewrite`; the "expanded"
   origin some earlier prose implied never existed. `create_version_with_
   metadata` already supports an **explicit parent override**, reserved "for
   a future restore_rewrite" (`store/mod.rs:488-500`) — the review compiler
   uses it to parent the new version on the *run's* version rather than the
   head. `change_summary` is a flat string (`0001:37`); `05`'s "per-node
   `change_summary`" maps to readable text plus the per-finding
   `applied_in` links, not a schema change (§4 D15).

7. **An interrogation pane is report-incapable by design, so an interviewer
   is a run node, not an interrogation.** The interrogation spawn passes an
   **empty env vec** (`binding/interrogate.rs:322`, doc `:297-303` — all four
   `KARVEX_WORKFLOW_*` vars withheld), mints no token, and has no `RunGraph`
   entry; `kvx workflow node complete` cannot even resolve a target from
   inside it (`cli/workflow.rs:1286,1358-1362`). The epilogue is the explicit
   contrast: a real node token precisely so it completes through
   `NodeSelfReport` (`app/workflow.rs:2697-2699`). `04` §4.5's table already
   says interviewers are `Internal`-kind nodes completing via `self_report`;
   the storage schema agrees (`review_finding.interview` is *provenance*, an
   optional link to the interrogation row). **The interviewer is therefore an
   ordinary run node** — node dir, token, output schema, corrective
   re-prompt, all of the machinery the oct-fixes hardened — whose *argv* is
   the fork-resume shape and which additionally records an `interrogation`
   row (§4 D6).

8. **"Its own small kvdag" cannot be a stored workflow or a normal run; the
   landing that fits every invariant is the post-terminal reserved subgraph
   — the epilogue, generalised.** Three walls: `materialise_run_nodes`
   resolves `kvdag_node` by key and only the reserved-path create may write
   `NONE` (the 0004 invariant); `run_terminal_ready` evaluates **user nodes
   only** (`engine/mod.rs:1771-1780` filters `is_reserved_path`), so a "run"
   containing only review nodes would finish instantly; and a hidden stored
   workflow per review is unbounded rows in a store with no delete. The
   epilogue already proves the alternative end-to-end: engine-appended
   reserved-path nodes (`begin_epilogue`, `engine/mod.rs:496-585`),
   post-terminal admission and settling that never re-decides the run
   status, live `NodeCreated` events, store rows with NULL `kvdag_node`,
   counter exclusion by the reserved-path predicate (`model.rs:133-134` —
   the rule is the prefix, pinned by test at `model.rs:2086-2101`). Phase 4
   generalises that machinery from one node to a small edged subgraph
   (`.review.<key>` interviewers → `.review.synthesis`), with the review
   lifecycle carried by `review_cycle.status`, never by the run status
   (§4 D5).

9. **The single-run guard and tick machinery are exactly where the review
   phase hooks in.** `handle_workflow_run` refuses on
   `is_live() || epilogue_pending()` (`app/api/workflows.rs:724-734`);
   `needs_tick()` is `is_live() || epilogue_pending() ||
   !interrogations.is_empty()` (`app/workflow.rs:759-761`); pruning gates on
   the same pair (`prune_run_history_if_settled`,
   `app/workflow.rs:1641-1668`). Each gains a `review_pending()` disjunct —
   the M7 pattern, fourth verse (§4 D8).

10. **Phase 3's restore, interrogation-refusal, and retention code all
    exists as specified — and none of it has been verified live.** The
    verification agent assigned to them today never reported. Code
    confirmed present: `materialise_with_restored`
    (`engine/graph.rs:128-215`), `resolve_restore` with unknown-selector
    hard error / `payload_truncated` / digest gating
    (`app/api/workflows.rs:1854-2024`, wired at `:912-919`),
    `--restore-allow-changed` (`cli/spec.rs:929`, `cli/workflow.rs:653-679`),
    the interrogate precondition ladder (`workflows.rs:1372-1505`),
    `prune_run_history` + caller (`store/mod.rs:1095-1120`,
    `app/workflow.rs:1626,1641-1668`), `mark_interrupted_runs` at store open
    (`store/mod.rs:1207-1240`, `app/workflow_store.rs:367`). Phase 4's
    review cycle sits directly on the fork/interrogation machinery, so
    **verifying this stack is step 0 of the workplan, a blocking gate, not
    an assumption** (§2 step 0a).

11. **`kvx --skill` contains zero workflow content — confirmed, and it is a
    functional defect, not a documentation gap.** The embedded skill is
    `skills/karvex/SKILL.md` (225 lines, served from `src/main.rs:446,
    726-729`); it has no mention of `kvx workflow`, `node complete`, or
    `KARVEX_WORKFLOW_NODE_TOKEN`. The reclassification (Karan, today,
    reversing the earlier docs descope for this one piece): the skill file
    is **runtime input to agents**, and a workflow node is an agent that
    reads it — a node agent that cannot learn `kvx workflow node complete`
    from its environment's own discovery surface cannot finish its node.
    That is the summariser bug one level up: `fbf62cf3`'s summariser failed
    because it was never told how to report; a node whose `task.md` is
    lost, truncated, or misread and whose skill file teaches nothing about
    workflows is in the same position with no fallback at all. Phase 4 adds
    interviewer and synthesis nodes — more agents on the same lifeline. So
    the section is a Phase 4 deliverable (WS-K) with a parity test, the
    task document remains the **primary** contract carrier (§3 item 9), and
    the skill section becomes the taught fallback. A ~27-line draft already
    exists (uncommitted on `docs/workflow-gaps`,
    `../karvex-worktrees/workflow-docs`) and has been verified against the
    post-oct-fixes tree: every verb and flag in it is real; it omits
    `--input KEY=VALUE` on `node expand` and the optional `--result-file`
    on `node complete` (`cli/workflow.rs:1020,2287-2290`). WS-K reuses it
    (§4 D21).

12. **The oct-fixes lesson is a standing constraint, not a memory.** The
    summariser could never finish under the real `claude` runner because its
    `task.md` was hand-built and bypassed `TaskDocument::render()` — it was
    never told about `result.json` or `kvx workflow node complete`
    (`fbf62cf3`; the fix makes `EpilogueTaskSpec` carry a *body* and the
    binder wrap it: `epilogue_task_markdown`, `app/workflow.rs:1163-1173`,
    with a test forbidding the spec from restating the reporting contract —
    "the reporting contract has exactly one author"). Four more sites named
    files relative to the wrong cwd (`96674e80`; render now absolute-only,
    pinned by `!contains("./")` tests). **Every new agent-facing document in
    Phase 4 — interviewer task, synthesis task, evidence file, previous-
    attempt seed — is composed as a body handed to `TaskDocument::render()`,
    and every path in any body is absolute** (§3 item 9). Watchdog nudges
    and re-prompts follow the `corrective_prompt` precedent instead
    (`complete.rs:466-500` — names `result.json` without `./` because
    `task.md` carries the absolute paths; `engine/mod.rs:1060-1108` shows
    the full deliver-and-give-the-strike-back pattern, including the no-pane
    branch).

13. **Config and guards, current state.** `WorkflowConfig` has exactly six
    fields (`config/model.rs:1001-1025`, defaults `:1221-1232`):
    `max_parallel_nodes` 4, `retention_runs` 50, `stuck_threshold` 3,
    `drift_threshold` 5, `history_context_runs` 3, `summary_enabled` true.
    **No `idle_budget`, no watchdog kill switch** — both are Phase 4 config
    (§4 D4). The tick is the hardcoded `WORKFLOW_TICK_INTERVAL = 20s`
    (`app/workflow.rs:54`), matching `04` §6.3's "3 ticks at a 20 s tick".
    The engine's `tick()` today does nothing but settle
    (`engine/mod.rs:1731-1735`) — idle streaks are `AgentStatus`-driven
    (`SUSTAINED_IDLE_TICKS = 3`, `complete.rs:34`), a *detector*-tick
    concept the watchdog must not be confused with. Also standing:
    `PROTOCOL_VERSION` is 19 (`protocol/wire.rs:16`; everything here is
    additive JSON — no bump); the error-code inventory test pins the code
    count at 26 (`app/api/workflows.rs:5859-5864`) and must be extended; the
    banned-word list includes `badge` (`api/schema/workflows.rs:1728-1730`),
    so `04` §6.3's "badge on the node" is TUI vocabulary only — no wire
    identifier may use it; the ten current workflow event kinds are listed
    at `api/schema/events.rs:286-295`.

14. **Loose ends inherited from Phase 3, recorded so nobody rediscovers
    them.** (a) `observe_interrogation_session_id` — the A9 async
    session-id learn path — has **no caller** (`app/workflow.rs:2389-2404`;
    the pane session-report handlers call only
    `observe_workflow_transcript_path`). Harmless while pre-minting works;
    Phase 4 pre-mints fork ids too, so this stays dormant and flagged
    (§6 A8). (b) `KvdagNode.timeout_ms` is authored and persisted
    (`model.rs:274`, `records.rs:97`) but **never read by the engine** — the
    watchdog is its natural enforcement point and Phase 4 wires it (§4 D11).
    (c) The `workflow` table has carried `archived: bool DEFAULT false`
    since 0001 (`0001_init.surql:25`) and the launcher already filters on it
    (`input/workflow_launch.rs:305`) — **no API mutates it**. Since
    self-improvement mints versions and there is no delete, Phase 4 ships
    the two-line archive toggle (§4 D17). (d) A pre-existing wall-clock
    flake family in `terminal::state::metadata` (fifth member found today)
    can fail unrelated CI runs; it is not workflow code and not fixed here
    (§7 R-9).

Also inherited and still true: `just check` covers both feature legs and the
MSVC lint; only `src/workflow/store` is feature-gated — everything else in
`src/workflow/` compiles unconditionally and stays
`App`/`surrealdb`/`ratatui`-free (the Phase 1 pure-layer grep test); the
schema artifact regenerates with
`KARVEX_UPDATE_API_SCHEMA=1 just test-one generated_protocol_schema_artifact_is_current`;
tests run from inside a live karvex session need the `KARVEX_*` env scrub.

---

## 1. Phase 4 workstreams

Nine workstream headings — eight owning streams plus WS-G, the short UI
shape-landing stage that afterwards merges into WS-F (the Phase 2/3 trick,
third use). The letter I is unused.

**Shared-file rule** (unchanged from Phase 3): no two workstreams edit the
same file during a parallel step. Files multiple streams need are landed
complete in step 1 by a single owner:

| File | Step-1 owner | Why shared |
|---|---|---|
| `src/workflow/model.rs` + the struct-literal sweep | WS-A | `ReviewState`, `StoreWrite`/`WorkflowEvent`/`RunEffect`/`EngineInput`/`ProgressDelta` shapes consumed by WS-B, WS-D |
| `src/app/state.rs` + mode-registration stubs (`src/ui.rs`, `src/app/input/{mod,modal,overlays}.rs`, `src/app/mod.rs` headless mirrors) | WS-G | `Mode::WorkflowReview`, `WorkflowReviewState` (WS-F) |
| `src/api/schema/*` + `src/api/{mod,server,subscriptions}.rs` | WS-C | wire types consumed by WS-D, WS-E, WS-F |
| `src/config/model.rs` | WS-D (step 1d, config block only) | three new `[workflow]` fields read by WS-A (via `EngineConfig`) and WS-J docs |

**The step-1a struct-literal sweep.** `RunGraph` gains `review:
Option<ReviewState>` and `ProgressDelta` gains fields; neither has `Default`
exemptions everywhere, so every construction literal extends in the same
step. Known sites (re-grep `RunGraph {`, `RunNode {`, `ProgressDelta {`
immediately before starting — the Phase 3 list drifted and this one will
too): `engine/graph.rs`, `engine/tests_support.rs`, `workflow/layout.rs`,
`ui/workflow_dag.rs` (fixtures), `app/workflow.rs`, `app/workflow_history.rs`,
`app/input/{modal,navigate,mod}.rs` test literals, `app/mod.rs` headless
mirrors. **The compiler is the site list** — the standing rule, adopted from
Phase 3's §8 E-4.

```
WS-A model+engine ──┬──▶ WS-B store ─────────┐
WS-C wire types   ──┼──▶ WS-D glue+handlers ─┼──▶ WS-J e2e+docs
WS-G ui shapes    ──┘    WS-E cli            │
WS-K skill+parity ──────▶ (before 2c)        │
                         WS-F review ui+dag  ┘
```

### WS-A — Model + engine: watchdog, review phase, restart seeding

**Files:** `src/workflow/model.rs` (+ sweep), `src/workflow/engine/mod.rs`,
`engine/watchdog.rs`, `engine/complete.rs`, `engine/schedule.rs`,
`engine/graph.rs`, `engine/tests_support.rs`.

**Slim posture:** unconditional; the pure-layer grep test must still pass.
The engine remains file-system-blind: every impure fact (digests, tool
counts, drafts, partial payloads) arrives as an input or leaves as an effect.

**Delivers, step 1a (shapes only, tree compiles):**

- `ProgressDelta` gains `schema_progress: Option<u32>` (count of the output
  schema's required fields present in the current `result.json` draft,
  computed app-side — the engine cannot read files, §0.3) and keeps its
  existing fields; `ProgressTracker` gains `ticks_since_progress` bookkeeping
  needs nothing new (`no_progress_streak`/`drift_streak` exist,
  `model.rs:1292-1301`) plus `last_schema_progress: Option<u32>` and
  `ladder: u8` (rung cursor for the current attempt; reset by restart's
  existing progress reset).
- `ReviewState { cycle: ReviewCycleId, phase: ReviewPhase, synthesis:
  RunNodeIdx, interviews: Vec<RunNodeIdx> }` on `RunGraph` beside `epilogue`;
  `ReviewPhase { Running, Synthesizing, Done, Failed }`. `ReviewCycleId`
  newtype beside `InterrogationId`.
- `InterviewSeed { target_path: InstancePath, node_key: NodeKey, mode:
  InterviewMode, source_session_id: Option<String> }`,
  `InterviewMode { Resumed, EvidenceOnly }` — what the handler hands
  `EngineInput::StartReview`.
- `EngineInput::StartReview { cycle: ReviewCycleId, interviews:
  Vec<InterviewSeed> }` and `EngineInput::Tick` unchanged. **No other new
  input** (stated so nobody adds one; review completion flows through the
  existing `NodeSelfReport` path with the review nodes' own tokens).
- `RunEffect::CapturePartial { path: InstancePath, seq: u64 }` — the
  watchdog's restart rung emits it *instead of* restarting directly; the app
  captures the node dir's best evidence, persists the `partial` checkpoint,
  and feeds `EngineInput::RestartNode` back (§4 D10). This is the only new
  effect.
- `StoreWrite::ReviewCycleStarted { id, run, kvdag_version,
  started_at_unix_ms }`, `StoreWrite::ReviewCycleUpdate { id, status:
  ReviewCycleStatus, ended_at_unix_ms: Option<u64>, resulting_version:
  Option<KvdagVersionId>, interview: Option<InterrogationId> }` (one update
  shape; `interview` appends to `review_cycle.interviews`, the D7-style
  merge), `StoreWrite::ReviewFindings { cycle, findings:
  Vec<ReviewFindingSeed> }` with `ReviewFindingSeed { node_key, run_path:
  Option<InstancePath>, interview: Option<InterrogationId>, interview_mode,
  level, verdict, rationale, evidence: serde_json::Value, proposed_change:
  serde_json::Value, replacement: Option<serde_json::Value> }`.
  `ReviewCycleStatus { Running, AwaitingUser, Applied, Declined, Failed }`
  mirrors the 0001 ASSERT list exactly.
- `WorkflowEvent::NodeWatchdog { run, path, class: ProgressClass, rung:
  Escalation, interventions: u16 }`, `WorkflowEvent::ReviewStarted { run,
  cycle }`, `WorkflowEvent::ReviewReady { run, cycle }`,
  `WorkflowEvent::ReviewClosed { run, cycle, status }` (the app-side emitter
  re-reads full projections, the `NodeUpdated` pattern).
- No new `RunEventKind`: `Watchdog` has been in the journal ASSERT since
  0001 (`0001_init.surql:197`) and finally gets its producer; review
  lifecycle is recorded in `review_cycle` rows, not the journal (§4 D16 —
  **no new migration exists in this phase at all** unless WS-B's
  `watchdog_interventions` bind audit says otherwise).

**Delivers, step 2a (behaviour):**

- **The watchdog pass.** `engine/watchdog.rs` grows
  `pub fn classify(node: &RunNode, cfg: &WatchdogConfig) -> ProgressClass`
  and `pub fn next_rung(node: &RunNode, max_attempts: u8) -> Escalation` —
  pure, table-tested. `Engine::tick(now)` (currently settle-only,
  `engine/mod.rs:1731-1735`) gains a pass over classifiable nodes — user
  nodes of a `Running|Paused` run, plus review-phase nodes post-terminal,
  **never** the epilogue (it has its own bounded ladder,
  `EpiloguePhase`) and never `Restored`/terminal nodes. Per node per tick:
  if no material progress arrived since the last tick, `no_progress_streak
  += 1`, else the existing reset already happened in `progress()`. At
  `streak >= stuck_threshold` → `LocalLoop` → escalate at the node's
  current `ladder` rung; each rung bumps `progress.interventions`
  (already wire-mapped at `app/workflow.rs:3178`), journals
  `RunEventKind::Watchdog` with `{class, rung, streak}`, emits
  `WorkflowEvent::NodeWatchdog`, resets the streak (the intervention gets
  its window), and advances `ladder`. Drift: when material progress *was*
  observed but `schema_progress` did not increase, `drift_streak += 1`; at
  `drift_threshold` → one structured re-prompt naming the unfilled required
  fields (reusing `required_fields()` and the `corrective_prompt` shape from
  `complete.rs:466-500` — same author, same no-`./` rule), counted as an
  intervention, `drift_streak` reset. **Productive-use check** (`04` §6.4):
  a node `Running` for ≥ `idle_budget_ticks` engine ticks whose
  `progress.tool_calls == 0 && tokens == 0 && artifact_changes == 0` is
  classified `LocalLoop` regardless of digest changes — the check exists
  precisely to override "the screen looks busy". **Timeout**: a node
  `Running` past its authored `timeout_ms` (§0.14b) escalates directly to
  `Blocked { reason: timeout }`, skipping earlier rungs — a declared budget
  is a contract, not an inference (§4 D11).
- **The ladder rungs.** Nudge → `RunEffect::PromptNode` with the fixed
  engine-authored nudge text (state + next concrete step; references
  `task.md`/`result.json` by bare name per the `corrective_prompt`
  precedent). Structured re-prompt → `PromptNode` with the re-send framing:
  task pointer, exact unfilled `output_schema` required fields, the last
  partial checkpoint's summary when one exists. Restart →
  `RunEffect::CapturePartial { path, seq }` (engine increments
  `checkpoint_seq`, journals, marks a pending-restart flag; the app closes
  the loop with `RestartNode` — §4 D10); bounded by the node's
  `max_attempts` — at the ceiling the rung is skipped straight to Blocked.
  Blocked → `NodeStatus::Blocked` (its first production producer;
  `run_terminal_ready` and the failed-run classification already handle it,
  `engine/mod.rs:1771-1780`), `Notify` once, run continues on other
  branches. Rungs deliver through the existing runner-selected primitive
  (`dispatch_workflow_effect` → `agent.prompt` / `pane.send_text`,
  `app/workflow.rs:1784-1846`); a node with no pane gets the
  give-the-strike-back treatment (`engine/mod.rs:1097-1106` precedent) so a
  rung is never spent on an undelivered message.
- **Restart seeding.** `EngineInput::RestartNode` behaviour is unchanged in
  the engine (§0.5 — the seeding is a *spawn* concern); the contract change
  is WS-D's: the spawn plan consults the latest `partial` checkpoint (see
  WS-D). The engine's only addition is that `restart` no longer clears
  `progress.interventions` across attempts is **wrong** — it does and must
  keep doing so per-attempt for the ladder, while the durable
  `run_node.watchdog_interventions` accumulates monotonically across
  attempts (WS-B owns the accumulate-on-write rule; the review evidence
  wants the run-lifetime count).
- **The review phase.** `Engine::begin_review(cycle, interviews)` — the
  `begin_epilogue` pattern (`engine/mod.rs:496-585`) generalised: refuses
  unless the run status is `Succeeded | Failed`, the epilogue is resolved
  (`Done`/`GaveUp`/absent), and no `ReviewState` exists. Appends one
  `RunNode` per interview at path `.review.<node_key>` (reserved namespace —
  the prefix rule, `model.rs:133-134`; `kind: Internal`, `runner` decided by
  the review override exactly as `epilogue_runner()` decides the
  summariser's, demand `Light` through the run's tier) plus
  `.review.synthesis`, with `Data` edges interviewer → synthesis. Emits
  `NodeCreated` per node (live DAG visibility, the epilogue precedent) and
  `StoreWrite::RunNodeCreated` through the reserved-path create. The
  post-terminal settle branch that today admits and settles the epilogue
  node extends to review nodes: node-level inputs for `.review.*` paths are
  processed and settled **without re-deciding the run status** — `finish` is
  never re-entered (the D1 outcome-immutability contract holds by the same
  three structural legs the Phase 3 audit verified). Interview completion is
  the ordinary `succeed` path (result checkpoint, succession `NoFollowup`);
  synthesis completion validates against the synthesis schema, fires
  `ReviewPhase::Done`, and hands the findings payload out via a
  `StoreWrite::ReviewFindings` + `ReviewCycleUpdate { status: AwaitingUser }`
  + `WorkflowEvent::ReviewReady`. Failure ladder per node: the watchdog
  covers review nodes (above); an interviewer that lands `Blocked`/`Failed`
  resolves its edge as dead and the synthesis task body lists it as
  "interview unavailable — evidence only"; a synthesis that fails schema
  twice or dies lands `ReviewPhase::Failed` → `ReviewCycleUpdate { Failed }`
  + `ReviewClosed`, run status untouched. `CancelRun` semantics do not
  apply (the run is already terminal); a dedicated
  `Engine::cancel_review()` closes review panes and fails the cycle —
  reached from the API only.
- **Prompt/schema single authorities**, all in `engine/mod.rs` beside their
  summary siblings: `interview_task_spec(&RunNode evidence block, questions)
  -> ReviewTaskSpec` and `synthesis_task_spec(...)` produce **bodies**
  (`task_body`, the `fbf62cf3` shape — a test forbids either from containing
  `## Reporting` or a `./` path); `interview_output_schema()` requires the
  five fixed answers (`account`, `what_happened`, `blockers`,
  `upstream_gaps`, `brief_changes` — mirroring `00` Feature 4's question
  set) with `maxLength` budgets; `synthesis_output_schema()` requires
  `findings: array` of objects requiring `node_key`, `level`
  (`prompt|structural` via the schema's string constraint — enforced
  handler-side since the subset validator has no enum, with the store's
  ASSERT as the final gate), `verdict`, `rationale`, `proposed_change`;
  `replacement` required-when-`replace` is validated by the **handler**
  (the subset validator has no conditionals) with the 0001 table event as
  backstop.

**Tested (table-driven, no DB, no PTY):** streak arithmetic — progress
resets, no-progress increments, thresholds fire at exactly N; the ladder
walks nudge → re-prompt → capture+restart → blocked in order and never
skips except max-attempts (→ blocked) and timeout (→ blocked directly); an
intervention resets the streak; drift fires the structured re-prompt with
the exact unfilled field names; productive-use overrides a changing digest;
the epilogue node is never classified; a `Paused` run's nodes are not
escalated; `CapturePartial` carries the incremented seq and a follow-up
`RestartNode` respawns at `attempt + 1`; `begin_review` refuses on a live
run / unresolved epilogue / double-start; review nodes admit and settle
post-terminal without re-entering `finish` (the R-1 pin, re-asserted);
a blocked interviewer dead-ends its edge and synthesis still admits;
synthesis double-failure lands `ReviewPhase::Failed` with the run status
untouched (assert before/after); reserved-path counters remain excluded
(re-run the Phase 3 counter pins over a graph with five `.review.*` nodes);
the interview/synthesis specs contain no `./` and no reporting section.

### WS-B — Store: review writers, evidence queries, version compile support

**Files:** `src/workflow/store/mod.rs`, `store/queries.rs`,
`store/records.rs`, `store/error.rs`, `store/tests.rs`. **No new migration
file** unless the audit below demands one.

**Slim posture:** entirely behind `#[cfg(feature = "workflow")]`.

**Delivers:**

- **The `watchdog_interventions` bind audit — first, before anything else.**
  The column exists (`0001_init.surql:168`), the row struct decodes it
  (`records.rs:175`), the history query consumes it
  (`queries.rs:762,831-832`), and the wire maps the *live* value
  (`app/workflow.rs:3178`). Verify whether `write_run_node` actually binds
  it; if not, that is a live instance of the 0.10.2 P1 field-loss class and
  the fix + per-field restart test land here **before** the watchdog gets a
  producer. Same audit for durable `NodeUsage` (`tool_uses`/`total_tokens`
  now gain real values from the sampler; a reader that drops them makes the
  review evidence lie). The accumulate rule: the durable
  `watchdog_interventions` is monotonic across attempts (bind
  `existing + delta` or write the graph's running total — pick one, test
  both restart polarities).
- **Row structs + writers**: `ReviewCycleRow`, `ReviewFindingRow`
  (`records.rs`, closing §0.6's gap); write arms for
  `ReviewCycleStarted` / `ReviewCycleUpdate` (append-merge on `interviews`,
  `None` = no change, the `write_interrogation_update` SurrealQL pattern at
  `store/mod.rs:2920-2944`) / `ReviewFindings` (batch insert; the
  `replace`-without-`replacement` case surfaces the DB event error as a
  typed `StoreError`, never a panic). `is_create_write` gains
  `ReviewCycleStarted` and `ReviewFindings` (creates-never-evicted,
  `app/workflow.rs:268-280` — WS-D's file, step 2c, noted here as contract).
- **Queries**: `get_review_cycle(run) -> Option<ReviewCycleRecord>` (newest
  per run), `list_review_findings(cycle) -> Vec<ReviewFindingRecord>`
  (ordered by the `finding_by_cycle` index), and — the evidence feed —
  `node_evidence(run, path) -> NodeEvidenceRecord { attempts,
  watchdog_interventions, tool_uses, total_tokens, duration_ms,
  schema_failures, steer_count, downstream_restarts }`, computed from the
  `run_node` row plus journal counts (`kind IN
  ["watchdog","steer","node_output"]` with `schema_valid: false` payload
  filter; `downstream_restarts` = restart-payload journal entries for nodes
  whose inbound edges source from this node — one query, not N).
  `finding_mark_applied(cycle, node_keys, version)` sets
  `accepted = true, applied_in = $version` in one UPDATE.
- **Review interruption sweep**: `mark_interrupted_reviews()` beside
  `mark_interrupted_runs` at store open — `status: "running"` cycles go
  `failed` with `ended_at`; **`awaiting_user` cycles survive** (their
  findings are durable and `review.apply` is store-only, §4 D13). Called
  from the same open path (`app/workflow_store.rs:367` region — WS-D wires,
  WS-B implements).
- **Version compile support**: nothing new — `create_version_with_metadata`
  with `VersionOrigin::SelfImprovement` and the explicit-parent override
  (`store/mod.rs:488-500`) plus `set_head_version` (`:679`) already cover
  the write; a store test proves a `self_improvement` version round-trips
  with `parent` = an arbitrary (non-head) version and the no-op-revision
  dedup **skipped** (the explicit-parent branch, per the existing comment).
- **Archive toggle**: `set_workflow_archived(id, bool)` — one UPDATE on the
  existing column (§0.14c).
- **Field-for-field durability tests** (Phase 3 D16 discipline, restated as
  a rule): for `review_cycle`, `review_finding`, a `partial` checkpoint, and
  the interventions/usage-bearing `run_node` — write via the production
  writer, read via the production reader after a simulated reload, assert
  every field individually by name.

**Tested (beyond the above):** replace-without-replacement is a typed error
surfaced to the caller; prune of the reviewed run still nulls
`review_finding.interview` (re-run the Phase 1 dangling-reference test
against rows created through the **new** writers); `awaiting_user` survives
`mark_interrupted_reviews` and `running` does not; `node_evidence` counts
match a hand-built journal; a partial checkpoint never appears in
`find_restorable_checkpoints` (the existing pin, re-asserted against a
watchdog-written row).

### WS-C — Wire surface

**Files:** `src/api/schema/workflows.rs`, `src/api/schema.rs`,
`src/api/schema/{response,events}.rs`, `src/api/subscriptions.rs`,
`src/api/mod.rs`, `src/api/server.rs`, regenerated
`docs/next/api/herdr-api.schema.json`.

**Slim posture:** unconditional, zero `use crate::workflow::*`, one artifact
value on both legs. All additive; **no `PROTOCOL_VERSION` bump** (§0.13).

| Addition | Shape |
|---|---|
| `Method::WorkflowReviewStart` → `workflow.review.start` | `WorkflowRunTarget` (reused) |
| `Method::WorkflowReviewGet` → `workflow.review.get` | `WorkflowRunTarget` |
| `Method::WorkflowReviewApply` → `workflow.review.apply` | `WorkflowReviewApplyParams { run_id, accept: Vec<String> /* finding node_keys */ }` — everything not accepted is declined; an empty `accept` declines the cycle |
| `Method::WorkflowArchive` → `workflow.archive` | `WorkflowArchiveParams { workflow_id, archived: bool }` |
| `ResponseResult::WorkflowReviewStarted` / `WorkflowReviewGet` / `WorkflowReviewApplied` | `{ review: WorkflowReviewInfo }` / `{ review: Option<WorkflowReviewInfo>, findings: Vec<WorkflowReviewFindingInfo> }` / `{ review: WorkflowReviewInfo, version_id: Option<String> }` |
| `ResponseResult::WorkflowArchived` | `{ workflow: WorkflowInfo }` |
| `WorkflowReviewInfo` | `{ id, run_id, workflow_id, version_id, status, started_at_unix_ms, ended_at_unix_ms: Option<u64>, resulting_version_id: Option<String>, interview_paths: Vec<String>, evidence_only_count: u32 }` |
| `WorkflowReviewFindingInfo` | `{ node_key, run_path: Option<String>, interrogation_id: Option<String>, interview_mode, level, verdict, rationale, evidence: serde_json::Value, proposed_change: serde_json::Value, replacement: Option<serde_json::Value>, accepted, applied_in_version: Option<String> }` |
| `WorkflowRunNodeInfo` | *(no change — `watchdog_interventions` is already on the wire at `workflows.rs:567`)* |
| `EventKind::WorkflowNodeWatchdog` → `workflow.node.watchdog` | `EventData::WorkflowNodeWatchdog { run_id, path, class, rung, interventions }` — `class`/`rung` as snake_case strings |
| `EventKind::WorkflowReviewStarted/Ready/Closed` → `workflow.review.started` / `workflow.review.ready` / `workflow.review.closed` | `EventData::{..} { run_id, review: WorkflowReviewInfo }` |
| `Subscription::{WorkflowNodeWatchdog, WorkflowReviewStarted, WorkflowReviewReady, WorkflowReviewClosed} {}` | + `KNOWN_EVENT_KINDS` + the exhaustive subscriptions arm |
| `request_changes_ui` | + `WorkflowReviewStart`, `WorkflowReviewApply`, `WorkflowArchive` (they mutate / spawn panes); `review.get` stays out |
| `api_method_name` | four new arms |

**Error codes** (spelled once, implemented in WS-D, inventory test extended
from 26): `workflow_review_in_flight`, `workflow_review_not_found`,
`workflow_review_not_awaiting`, `workflow_review_run_not_resident` (message
names the constraint and the remedy: "start the review before launching
another run"). All `workflow_`-prefixed snake_case, banned-word-free
(`review`, `finding`, `watchdog`, `archive` are runtime vocabulary; `badge`
never appears — §0.13).

**Naming guard:** every identifier above joins
`no_new_workflow_api_identifier_uses_banned_ui_surface_words` under a
`// Phase 4 additions` comment.

**Tested:** serde round-trip per type; regenerated artifact green on both
legs; the `not_implemented` sweep and feature-off sweep extended with the
four methods (WS-D's file, same split as Phase 3); event kinds' dot-name
round-trip.

**The step-1c sweep:** `EventData` exhaustive matches
(`app/api/plugins/context.rs` and siblings) gain arms; params/info literals
in `cli/`/`app/` get placeholder sweeps exactly as Phase 3's E-3 — literals
and arms only, re-grepped before sweeping.

### WS-D — Handlers, app glue, sampler, spawn binding

**Files:** `src/app/api/workflows.rs`, `src/app/api.rs` (dispatch),
`src/app/workflow.rs`, `src/app/workflow_store.rs`,
`src/workflow/binding/spawn.rs`, `src/workflow/binding/observe.rs`,
`src/workflow/binding/interrogate.rs`, `src/workflow/binding/progress.rs`
(new), `src/config/model.rs` (step 1d, the `[workflow]` block only) +
`docs/next/website/src/data/config-reference.json`.

**Delivers:**

- **Config (step 1d, landed before the parallel steps):**
  `watchdog_enabled: bool` default true (kill switch — a brand-new
  intervention system ships with an off switch, the `summary_enabled`
  precedent), `idle_budget_ticks: usize` default 9 (≈3 min at the 20 s
  tick; tick-count unit matches `stuck_threshold`/`drift_threshold` house
  style), `review_max_interviews: usize` default 6 (§4 D7). All three into
  `workflow_runtime_config` → `EngineConfig`/`WorkflowPolicy`
  (`app/workflow.rs:1213-1238` — the single config authority, E-16
  precedent), doc-comment-synced for `config_reference_check.py`.
- **The progress sampler** — the missing producer half (§0.2, §0.3).
  `binding/progress.rs`: a pure-ish delta reader with per-node cursors:
  `TranscriptCursor { byte_offset }` — each workflow tick, for every
  `Running` node with a binding, read the transcript file's bytes from the
  cursor (capped at 512 KiB per tick; malformed lines skipped), count
  tool-use entries and sum usage token deltas; stat the `artifacts/` dir
  (entry count + newest mtime); hash the detection snapshot via the
  existing `observe::screen_digest` (the terminal-state read follows the
  `sample_workflow_agent_states` shape, `app/workflow.rs:1429-1456`); parse
  the `result.json` draft if present and count filled required fields
  (`schema_progress`). Feed `observe::progress_observed` (its first
  production caller) **before** `EngineInput::Tick` in
  `tick_workflow_engine` (`app/workflow.rs:1356-1369`), so the tick's
  classification sees the fresh evidence. Cursors live on
  `WorkflowRuntimeState`, keyed by instance path, cleared on restart/run
  start. Cost note: this is bounded I/O on ≤ `max_parallel_nodes` nodes
  every 20 s — stat + tail-read, no full-file parse (§7 R-5).
- **Partial capture** (§4 D10): on `RunEffect::CapturePartial { path, seq }`
  — read the node's `result.json` draft (any parse state; `schema_valid:
  false`), list `artifacts/`, enqueue `StoreWrite::Checkpoint { kind:
  Partial, seq, payload: draft-or-`{}`, summary: watchdog-authored
  one-liner naming the rung and streak, artifact_paths, digest }`, then
  apply `EngineInput::RestartNode { path }`. Capture failure (no draft, no
  artifacts) still restarts — the checkpoint is best-effort evidence, never
  a gate.
- **Restart seeding**: `workflow_spawn_plan` — before rendering, the glue
  passes the node's latest `partial` checkpoint (store lookup by
  `(run, path)`, newest seq) into a new `TaskDocument.previous_attempt:
  Option<PreviousAttempt { summary, payload_path: Option<PathBuf> }>`
  section — rendered by `render()` (the ONE author, §0.12), absent-when-
  absent so every existing `task.md` stays byte-identical (Phase 3 frozen-
  interface rule 9's discipline; the payload is written to
  `<node_dir>/inputs/.previous-attempt.json` at materialise time and named
  absolutely). Applies to watchdog restarts and manual
  `workflow.node.restart` alike — `05`'s Phase 4 promise for the restart
  row, kept.
- **Review orchestration** (`handle_workflow_review_start`): preconditions
  in order — run resolves; run is terminal (`Succeeded|Failed`); run is
  **resident** (the engine's current graph is this run — else
  `workflow_review_not_resident`; rationale §4 D13); epilogue resolved; no
  cycle already `Running` for this run (`workflow_review_in_flight`); no
  live interrogation on any interview target (`workflow_interrogation_active`
  naming the pane — concurrent forks of one session are the D7 footgun).
  Then: select interview targets — executed, non-reserved, `Agent`-runner
  nodes (the E-14 runner gate, never id-presence: every node has a derived
  `agent_session_id`, Command nodes included, `workflows.rs:1408-1418`),
  ranked by trouble score (attempts, interventions, schema failures,
  steers — from `node_evidence`), capped at `review_max_interviews` (§4
  D7); per target, stat transcript + cwd exactly as `interrogation_seed`
  does (`workflows.rs:2239-2331`) → `Resumed` or `EvidenceOnly`. Mint
  `ReviewCycleId`, enqueue `ReviewCycleStarted`, apply
  `EngineInput::StartReview`, answer `WorkflowReviewStarted`, emit
  `workflow.review.started`.
- **Interviewer spawn** — the reserved-path spawn branch
  (`workflow_spawn_plan`'s `is_reserved_path` divert,
  `app/workflow.rs:2478-2484`) grows a `.review.*` arm beside the
  `.summary` arm. E-13's mechanical rule applies: **every field the plan
  derives from the definition needs a reserved-path source** — demand from
  the engine constant, prompt/schema from `interview_task_spec` /
  `synthesis_task_spec`, runner from the review override. The interviewer's
  spec: full node env (it IS a node — token, node dir, the lot,
  `spawn.rs:34-42`), argv for `Resumed` mode from a new
  `interrogate::interview_argv(source_sid, fork_sid, node_dir, seed)` —
  `claude --session-id <minted fork> --resume <source_sid> --fork-session
  --add-dir <node_dir> "Read <abs>/task.md and follow it."` (the resumed
  six-token core plus the node contract's add-dir and seed; the frozen
  interrogation `resumed_argv` is untouched; the combination is step 0b's
  spike); for `EvidenceOnly`, the ordinary `agent_argv` (fresh session).
  Task body: the five questions, the `node_evidence` numbers put *to* the
  teammate, the target's checkpoint summary; the evidence-only variant
  opens with the reconstruction-style first line ("you are reviewing
  evidence, not resuming the session" — the `ReconstructedSeed` honesty
  rule, `interrogate.rs:249-290`). At spawn, enqueue
  `StoreWrite::InterrogationStarted` for resumed interviews (forked id
  pre-minted; `reconstructed: false`; note: `"review interview"`), append
  the id to the cycle via `ReviewCycleUpdate { interview }`, and register
  the pane in the interrogation tracker (conflict prevention + `ended_at`
  for free — the tracker already outlives runs,
  `app/workflow.rs:238-244`). The review override env
  `KARVEX_WORKFLOW_REVIEW_COMMAND` (one var, both node types; argv-as-JSON,
  read once in `workflow_runtime_config`, malformed ⇒ reviews disabled +
  one notice — the E-11 rules verbatim) swaps the runner to `Command` for
  CI (§4 D18).
- **Synthesis acceptance glue**: on `ReviewFindings` effects, validate
  `replace`-has-`replacement` (refuse the report through the ordinary
  corrective re-prompt if violated — the schema subset cannot express the
  conditional), persist, flip status, emit `workflow.review.ready`, notify
  once ("review ready — N findings").
- **`handle_workflow_review_apply`** — store-only (no engine dependency, §4
  D13): cycle must be `AwaitingUser` (`workflow_review_not_awaiting`);
  empty `accept` ⇒ `ReviewCycleUpdate { Declined }` + `ReviewClosed`.
  Otherwise: load the run's version spec, apply accepted findings in
  `node_key` order — `level: prompt` merges `proposed_change`'s
  `{prompt_template?, role?, system_contract?}`; `level: structural` with
  `verdict: improve` merges `{demand?, max_attempts?, timeout_ms?}`;
  `verdict: replace` swaps the node's definition for `replacement`
  (validated as a full node object); **no edge surgery in v1** (§4 D14) —
  then `Kvdag::try_new` (a compile failure fails the *apply* with the
  validation message; the cycle stays `AwaitingUser` so the human can
  accept a smaller set), `create_version_with_metadata(SelfImprovement,
  parent = run's version, change_summary = one line per accepted finding)`,
  `set_head_version` (§4 D15), `finding_mark_applied`,
  `ReviewCycleUpdate { Applied, resulting_version }`, `ReviewClosed`,
  answer with the new version id.
- **Guards + lifecycle**: `review_pending()` on `WorkflowRuntimeState`
  (delegating to the engine's `ReviewState`), added as the fourth disjunct
  to the run-start refusal (`workflows.rs:724-734`, message naming the
  review), `needs_tick()` (`workflow.rs:759-761`), and
  `prune_run_history_if_settled` (`workflow.rs:1641-1646`).
  `mark_interrupted_reviews` wired at store open beside
  `mark_interrupted_runs` (`workflow_store.rs:367`). `workflow.archive`
  handler: store call, wire mapping; archiving does not touch runs or the
  head (the launcher filter already handles presentation).
- **Watchdog emit glue**: `WorkflowEvent::NodeWatchdog` → the wire event +
  a once-per-rung-4 user notice ("node <path> blocked — <reason>"), matching
  the notify discipline of the epilogue ladder.

**Tested:** handler tests with `AppState::test_new()` + kv-mem store:
review.start refuses non-resident / non-terminal / epilogue-pending /
double-start, each with its named code; target selection excludes Command
runners and reserved paths, respects the cap and the trouble ranking;
resumed-vs-evidence mode decided by transcript stat (fabricated file);
apply on a declined cycle refuses; apply compiles a v2 whose parent is the
run's version — not the head — and advances the head; a replace finding
without replacement never reaches the store; empty-accept declines; run
start during a review refuses and succeeds after `ReviewClosed`; the
sampler feeds `ProgressObserved` before `Tick` (order pinned); partial
capture writes `schema_valid: false` and restart re-renders `task.md`
containing the `## Previous attempt` section with only absolute paths; the
interrogation tracker carries interview panes (a second interrogate on a
target under interview refuses); config-reference check green.

### WS-E — CLI

**Files:** `src/cli/workflow.rs`, `src/cli/spec.rs`, `src/cli.rs`,
`tests/cli/workflow.rs`.

```
kvx workflow review start <run-id> [--json]
kvx workflow review show <run-id> [--json]
kvx workflow review apply <run-id> [--accept <node_key>]... [--decline-all] [--json]
kvx workflow archive <name|id> [--restore] [--json]
```

`review` is a new namespace under `workflow` (`VERB_PATHS` + the parity
trio); `--accept` is repeatable; `--decline-all` and `--accept` are mutually
exclusive; bare `apply` with neither is an error (an irreversible
version-mint never defaults). `archive --restore` clears the flag
(un-archive; `--restore` here is the existing restore vocabulary applied to
the workflow row — rendered as "unarchived"). Human rendering: `review show`
prints status, one block per finding (key, mode-flagged `evidence-only`
when applicable, level/verdict, rationale, the proposed change summarised);
`node show` gains `interventions:` and `blocked:` lines (the wire field
exists). All new verbs take `--json` (the D17 sweep left nothing to
close). Timestamps via `format_unix_ms`.

**Tested:** parser→`Method` tests per verb; parity trio; repeatable
`--accept`; the mutual-exclusion refusal; e2e verbs in
`tests/cli/workflow.rs`.

### WS-K — Node contract learnability: the `kvx --skill` workflow section

**Files:** `skills/karvex/SKILL.md`, `src/cli/skill_parity.rs` (new,
`#[cfg(test)]`-only module; its one `mod` line in `src/cli.rs` is an
E-4-class mechanical add granted from WS-E, landed in this step so the
files never contend).

**Why it is a workstream and not a docs task:** §0.11 — the skill file is
runtime input to agents; the missing section is a node-contract defect in
the summariser-bug family. Product behaviour gets tests.

**Delivers:**

- **The `## Workflows and running as a workflow node` section**, seeded
  from the `docs/workflow-gaps` draft (§0.11 — verified accurate; adopt it,
  add the two missing flags: `--input KEY=VALUE` on `node expand`, the
  optional `--result-file` on `node complete`) plus `kvx workflow` in the
  discovery command list. Content is scoped to what a **node agent** needs
  to operate, at the file's existing density and voice: discovering
  `kvx workflow` / `run` / `node`; the self-report contract (the four
  `KARVEX_WORKFLOW_*` env vars by exact name, `result.json` into
  `KARVEX_WORKFLOW_NODE_DIR`, `kvx workflow node complete` reads the env
  and takes no positional arguments, idle-without-a-valid-result stalls as
  `needs_attention` — never assume an idle pane is done); `node expand`
  with its token auth and refused-is-not-an-error semantics; and the Phase
  4 additions — a short paragraph stating that `.`-prefixed reserved-path
  nodes (`.summary`, `.review.*`) are karvex-owned agents under the **same**
  reporting contract, and that `[karvex · …]`-framed messages arriving in
  the pane (nudges, structured re-prompts, cross-node deliveries) are the
  runtime steering the node — follow them, report through `result.json`.
  It is explicitly **not** a CLI reference: operator verbs (`run start`,
  `node show`, `node steer`) get the draft's two-line mention and no more.
- **The parity pin.** The Phase 1 risk register named "CLI help/completion
  drift" across three hand-maintained places and pinned them with a parity
  test; the skill file is a fourth hand-maintained surface describing the
  same commands and gets the same treatment. `skill_parity.rs`
  `include_str!`s the skill (the same asset `src/main.rs:446` serves),
  extracts the workflow section, and asserts: every `kvx workflow …` verb
  path it names resolves against `VERB_PATHS`
  (`src/cli/workflow.rs:33-51`); every `--flag` token it names for a verb
  appears in that verb's usage string; every `KARVEX_WORKFLOW_*` name it
  uses equals the constants in `binding/spawn.rs:34-42` (unconditional
  module — no feature gate needed); and the section exists at all (the
  regression that recreates today's gap fails with the section name in the
  message). The test is deliberately direction-agnostic: renaming an env
  var or removing a verb without updating the skill fails the same pin.

**Tested:** the parity module itself, plus a negative fixture check (a
fabricated section naming a nonexistent verb fails). Runs in `just check`
like any in-crate test.

**Sequencing:** lands in **step 1e** — complete before step 2c, because
interviewer and synthesis nodes depend on agents being able to learn their
contract, and because the reserved-path paragraph states contract facts
frozen in §3 (items 5, 6, 9) rather than implementation details that could
still move. If a step-2 amendment changes a contract fact the section
states, the amendment names WS-K's line as part of its blast radius —
that is what the parity test cannot catch (it pins syntax, not semantics).

### WS-F — TUI: review offer, findings overlay, watchdog visibility

**Files:** `src/ui/workflow_review.rs` (new, stub landed by WS-G),
`src/app/input/workflow_review.rs` (new, stub landed by WS-G),
`src/ui/workflow_dag.rs`, `src/config/keybinds.rs`, `src/main.rs`
(sample-config line, last), `src/ui/keybind_help.rs`,
`docs/next/website/src/data/config-reference.json` (keybind row — WS-D owns
the config rows; coordinate the one file through step ordering: WS-D lands
its rows in 1d, WS-F appends in 2e).

**Delivers:**

- **The ask, never automatic** (`00` Feature 4): on `workflow.review.*`-free
  run finish of a run with ≥1 Agent node, the DAG header gains a passive
  `· review available (V)` segment and a one-shot notice; no modal
  interrupts (§4 D19). `V` in the DAG view (and `V` in the run browser on
  the resident run's row) calls `workflow.review.start`; the review nodes
  then render live through the existing `NodeCreated` path — zero new DAG
  plumbing, the `.summary` precedent.
- **`Mode::WorkflowReview`** — the findings overlay, WorkflowRuns
  silhouette (list + detail + footer): opens on `workflow.review.ready` via
  a notice + `keys.open_workflow_review` (default `prefix+shift+v`;
  `default_keybinds_have_no_duplicate_chords` is the collision authority),
  lists findings with per-row accept toggle (Space), `Enter` → confirm
  modal (the `WorkflowRunsConfirmRestore` shape, `state.rs:1145-1152`) →
  `workflow.review.apply`; `d` declines all (same confirm); `Esc` closes
  without deciding (the cycle stays `AwaitingUser` — durable, resumable
  from CLI or reopen). Data loads via in-process wire dispatch
  (`workflow.review.get`) — the runtime/client boundary rule; refresh on
  `workflow.review.*` events only.
- **Watchdog visibility in the DAG**: the node detail strip appends
  `watchdog: N interventions` when nonzero and the blocker line for
  `Blocked` nodes (status colour already exists); the header shows
  `· reviewing…` while the phase runs (the `· summarising…` precedent).
  No new lanes, no geometry changes — the pinned numbers stay.

**Tested:** mode round-trip; geometry partition/degradation (launcher test
shapes); toggle state machine as pure key-handler tests; pruned/closed
cycles never map to apply targets; keybind collision test green;
config-reference check green.

### WS-G — Shared UI shape landing (step 1b) — then merges into WS-F

**Files (step 1b only):** `src/app/state.rs`, `src/ui.rs`,
`src/app/input/{mod,modal,overlays}.rs`, `src/app/mod.rs` (headless
mirrors), plus the two stub files above.

`Mode::WorkflowReview` + membership in `mouse_motion_changes_view` and
`wants_ascii_input` (pinned lists, `state.rs:1271-1310`);
`WorkflowReviewState` on `ViewState` (entries, selection, accept-set,
scroll, rects, confirm sub-state); stub render/key/mouse/paste arms with
**Esc→leave_modal live in the stub** (never an input trap); the headless
`AppState` literal and key-dispatch mirrors. One short sequential step, one
owner — the exhaustive-`Mode`-match files stay out of every parallel step.

### WS-J — End-to-end and docs

**Files:** `tests/workflow_headless.rs`, `tests/fixtures/workflow/`,
`docs/next/website/src/content/docs/{,ja/,zh-cn/}workflows.mdx`,
`docs/next/CHANGELOG.md`.

**E2E scenarios** (stub `runner: command` fixtures; env isolation per the
five-var block in §5; `summary_enabled=false` default posture per the Phase
3 fixture policy, watchdog on only where the scenario says so):

1. **The ladder, end to end.** A command node that produces no material
   progress (a sleep-loop stub) under `stuck_threshold=1,
   idle_budget_ticks=2` walks nudge → re-prompt → capture+restart →
   blocked, asserted from the `workflow.node.watchdog` event stream and the
   journal (`kind: watchdog` entries with ascending rungs); the `partial`
   checkpoint exists with `schema_valid: false`; the respawned attempt's
   `task.md` contains `## Previous attempt` with absolute paths only; the
   run ends `failed` with the node `blocked`; durable
   `watchdog_interventions` equals the rung count after a server restart
   (the field-loss guard, e2e face).
2. **Productive-use vs busy screen.** A stub that redraws (changing
   detection digest) but makes zero tool calls/artifacts past
   `idle_budget_ticks` still escalates — the §6.4 pin.
3. **Watchdog off.** Same stub, `watchdog_enabled=false`: zero watchdog
   events, node sits `Running` (the kill switch is real).
4. **Review, synthesis-only.** An all-command run (no Agent nodes → zero
   interview targets) + `KARVEX_WORKFLOW_REVIEW_COMMAND` stub: review.start
   → `.review.synthesis` only → findings recorded → `review.ready` event →
   `review apply --accept <key>` mints a version with
   `origin: self_improvement`, parent = the run's version, head advanced;
   the run's own status and events never change post-`run.finished` except
   reserved-node and review events (ordering pin). A `workflow.run` during
   the cycle is refused with `workflow_review_in_flight`'s run-start
   message and admitted after `review.closed`.
5. **Interviewer plumbing without claude.** A mixed fixture where the
   review command override runs interviewer stubs: interviewer nodes spawn
   with node tokens, complete via `node complete`, their interrogation rows
   land linked from `review_cycle.interviews`, a blocked interviewer
   (stub exits nonzero repeatedly) degrades that node to evidence-only in
   the synthesis body, and the cycle still reaches `awaiting_user`.
6. **Apply is store-only.** Kill the server after `review.ready`; restart;
   the cycle is still `awaiting_user` (`mark_interrupted_reviews` spares
   it); `review apply` succeeds with no run resident.
7. **Archive.** `workflow archive` hides the workflow from the launcher
   list and `archive --restore` returns it; runs/history untouched.

No wall-clock races: every wait is event-stream-driven (the two known flaky
styles stay quarantined; §0.14d's metadata flake is pre-existing and not
touched).

**Docs.** `workflows.mdx` gains `## The anti-stuck watchdog` (classes,
ladder, config, the kill switch) and `## Reviewing a run` (the ask, the
interview honesty rules, apply/decline, `origin: self_improvement`), plus
the new CLI verbs and config keys; `ja`/`zh-cn` heading parity
(`just release-docs-check` gates). CHANGELOG per shipped surface.

---

## 2. Ordered Phase 4 workplan

Steps sharing a number run in parallel. Every step leaves the tree
compiling and `just check-slim` green.

| # | Step | Owner | Files | Delivers |
|---|---|---|---|---|
| 0a | **Phase 3 verification gate** (blocking; throwaway server, §5 env block) | WS-D owner | none | Execute: checkpoint restore across versions incl. `--restore-allow-changed` and unknown-selector-creates-no-run; the interrogation refusal ladder (command node, missing transcript, live-pane conflict, reconstructed fallback); retention/pruning with a forced-low `retention_runs`. Record results in the build ledger. **Any failure becomes a fix-first work item before step 2c consumes the machinery** |
| 0b | **Fork-combination spike** (manual, real `claude`) | WS-D owner | none | Verify: (a) `--session-id <mint> --resume <sid> --fork-session --add-dir <dir> "<seed>"` yields a working revived session that reads the seed; (b) the bundled `stop` hook fires in the forked session (completion signal works); (c) `kvx workflow node complete` succeeds from inside it with the node env; (d) transcript-delta assumptions: tool-use entries and usage fields are present and parseable in the transcript JSONL tail. D3/D6 carry designed fallbacks for every "no" |
| 1a | Model shapes + sweep | WS-A | `model.rs`, sweep files, `tests_support.rs` | every §WS-A step-1a type/variant; tree compiles |
| 1b | UI shapes + mode stubs | WS-G | `state.rs`, `ui.rs`, `input/{mod,modal,overlays}.rs`, `app/mod.rs`, two stub files | `Mode::WorkflowReview` + stubs |
| 1c | Wire surface | WS-C | schema files + artifact | the §WS-C table, artifact, naming guard, sweep |
| 1d | Config block | WS-D | `config/model.rs`, `config-reference.json` | the three new `[workflow]` fields |
| 1e | Skill section + parity pin | WS-K | `skills/karvex/SKILL.md`, `src/cli/skill_parity.rs`, one `mod` line in `src/cli.rs` (granted) | the workflow section (draft-seeded, gap-corrected) + the parity test |
| 2a | Engine behaviour | WS-A | `engine/*` | watchdog pass + ladder, review phase, spec/schema authorities |
| 2b | Store | WS-B | `store/*` | bind audit first, then writers, queries, sweeps, durability tests |
| 2c | Glue + handlers + sampler + binding | WS-D | `app/api/workflows.rs`, `app/api.rs`, `app/workflow.rs`, `app/workflow_store.rs`, `binding/{spawn,observe,interrogate,progress}.rs` | sampler, capture, seeding, review orchestration, apply, guards, archive |
| 2d | CLI | WS-E | `cli/*`, `tests/cli/*` | review verbs, archive, renderings |
| 2e | Review overlay + DAG | WS-F | `ui/workflow_review.rs`, `input/workflow_review.rs`, `ui/workflow_dag.rs`, keybind files | the overlay, the ask, watchdog visibility |
| 3 | E2E | WS-J | `tests/workflow_headless.rs`, fixtures | the seven scenarios |
| 4 | Docs | WS-J | `workflows.mdx` ×3, CHANGELOG | sections + parity |

Sequencing: 0a/0b before or alongside step 1 but **complete before 2c**; 1a
first, then 1b/1c/1d/1e in parallel (1e touches nothing the others own
beyond the granted `cli.rs` mod line — coordinate that one line with WS-E,
whose step-2d work must not start until 1e lands); all 2x start together
consuming §3's frozen interfaces; 3–4 when 2a–2d land (2e trails, the e2e
does not drive the TUI). The WS-K-before-2c ordering is a real dependency,
not caution: the review workstream ships agents whose contract the skill
section teaches.

**Merge gate:** `just check` green (both legs, MSVC lint, maintenance
tests) + `just release-docs-check` for step 4 + the env-scrubbed `just
test` when building from inside a karvex session. The
`terminal::state::metadata` wall-clock family (§0.14d) is a known
pre-existing flake — a failure there alone is a rerun, not a block, and
must be reported, never "fixed" in passing.

**Suggested agent models** (implementation restricted to opus/sonnet): WS-A
**opus** (the post-terminal phase generalisation is the riskiest code),
WS-D **opus** (widest blast radius: sampler + orchestration + apply), WS-B
**sonnet**, WS-C **sonnet**, WS-E **sonnet**, WS-F **sonnet** (strong
template), WS-G **sonnet**, WS-K **sonnet** (short and precision-bound;
the draft does most of the writing), WS-J **opus** for the e2es /
**sonnet** for docs.

---

## 3. Interfaces frozen at the start of Phase 4

1. **`ProgressDelta` + `schema_progress`; `observe::progress_observed` is
   the single admission gate** for evidence (its all-`None` filter stays).
   WS-A owns shapes; WS-D's sampler is the only producer; sampler-before-
   Tick ordering is part of the contract.
2. **The watchdog contract:** `classify`/`next_rung` pure in
   `watchdog.rs`; ladder order Nudge → StructuredReprompt →
   CapturePartial+Restart → Blocked; interventions counted per rung on
   `progress.interventions`, accumulated monotonically on the durable
   column; `watchdog_enabled` kills the entire pass; timeout jumps to
   Blocked; the epilogue and `Restored`/terminal nodes are never
   classified. WS-A owns; WS-D consumes effects.
3. **`RunEffect::CapturePartial { path, seq }` → app capture →
   `EngineInput::RestartNode`** — the only partial-checkpoint producer path;
   the engine never fabricates a payload; capture failure still restarts.
4. **`EngineInput::StartReview { cycle, interviews }` +
   `Engine::begin_review` + `ReviewState`/`ReviewPhase`** — the review
   phase is engine-owned post-terminal state on the resident graph; run
   status is immutable throughout (the D1 contract extends verbatim);
   `Engine::review_pending() -> bool` is the app-facing accessor.
5. **Interview identity:** interviewer = run node at `.review.<node_key>`
   with full node env and token; interrogation row = provenance, created at
   spawn with a pre-minted fork id, linked via
   `ReviewCycleUpdate { interview }`; the interrogation tracker carries the
   pane. `interrogate::resumed_argv` stays frozen;
   `interrogate::interview_argv` is the new, separately-frozen shape
   (six-token core + `--add-dir` + seed), contingent on step 0b.
6. **Prompt/schema single authorities:** `interview_task_spec`,
   `synthesis_task_spec`, `interview_output_schema`,
   `synthesis_output_schema` in `engine/mod.rs` — bodies only, wrapped by
   `TaskDocument::render()`; nothing else authors agent-facing contract
   text (§0.12). The five-question set and the evidence block live in the
   spec, not the handler.
7. **`StoreWrite` additions** (`ReviewCycleStarted`, `ReviewCycleUpdate`,
   `ReviewFindings`) — WS-A shapes, WS-B persistence; `ReviewCycleStarted`
   and `ReviewFindings` join `is_create_write`.
8. **The wire table of §WS-C** — names, fields, four methods, four events,
   all additive; no `PROTOCOL_VERSION` bump; error codes as spelled there.
9. **The task-document constraint** (standing, from oct-fixes): every new
   agent-facing document is a body handed to `TaskDocument::render()`; every
   path in any body or prompt is absolute or bare-name-per-the-
   `corrective_prompt`-rule; tests forbid `./` and duplicate reporting
   sections. Owner: whoever writes the text; enforcement: WS-A's spec tests
   + WS-D's render tests.
10. **Apply semantics:** store-only; parent = the reviewed run's version via
    the explicit-parent override; origin `SelfImprovement`; head advances on
    apply; compile failure leaves the cycle `AwaitingUser`. WS-D owns the
    compiler; WS-B proves the store path.
11. **Review runner override:** `KARVEX_WORKFLOW_REVIEW_COMMAND`, read once
    in `workflow_runtime_config`, E-11 failure semantics (disable + one
    notice, never a silent claude fallback).
12. **The guard family:** `review_pending()` joins the run-start refusal,
    `needs_tick()`, and the prune gate — one predicate, three consumers,
    named in each place.
13. **The skill section's contract statements are pinned, and the pin is
    two-directional.** The exact `KARVEX_WORKFLOW_*` names, the
    `node complete` env-driven/no-positional contract, the
    `needs_attention` idle rule, and every verb/flag the section names are
    asserted against the CLI spec and spawn constants by WS-K's parity
    test. Any workstream that changes one of those facts owns the matching
    skill-section edit in the same change — the test makes forgetting a
    compile-visible failure, not a drift.

---

## 4. Decisions

**D1 — The watchdog acts on engine ticks, with evidence sampled app-side.**
The 20 s `WORKFLOW_TICK_INTERVAL` is the cadence `04` §6.3 designed the
thresholds for; the detector-tick idle machinery (`SUSTAINED_IDLE_TICKS`)
stays a *completion* concern and is not conflated with watchdog streaks.
The engine stays pure: everything it knows about progress arrives as
`ProgressObserved`, sampled by the app immediately before each tick. A
missed sample tick (server busy) degrades to a slower ladder, never a
wrong one.

**D2 — The four-way classification ships with an honest three-way hot
path.** `ExternalWait` maps to the skip-set (non-`Running` nodes holding a
declared `resume_when`; `Monitor` has no execution path to produce
"checked, unchanged" — §0.4). The enum keeps all four classes because the
taxonomy is the contract (`04` §6.2) and `Monitor` execution, when it
lands, slots into an existing class instead of forcing a new one.

**D3 — Tool calls and tokens come from bounded transcript-delta reads, not
hooks and not a tailing subsystem.** Per-tool-call hooks are a process
spawn per tool use and were deliberately removed from every integration
that had them (§0.3); a full tailing/parsing subsystem is the thing the
`NodeUsage` comment warns against. The middle path — a per-tick, cursor-
based, size-capped tail read of a file whose path karvex already stores
and hook-corrects — costs bounded I/O on ≤ `max_parallel_nodes` files,
fulfils `04` §6.1 as written (real tool-call entries, real usage deltas),
and finally gives the review evidence honest `tool_uses`/`tokens` numbers.
Fallback if step 0b(d) finds the format hostile: digest+artifact
materiality only, `NodeUsage` stays zero, and the productive-use check
narrows to tool-call-free signals — recorded as an amendment, not silently.

**D4 — Three new config fields, no more.** `watchdog_enabled` (a new
intervention system ships with a kill switch — the `summary_enabled`
precedent), `idle_budget_ticks` (default 9 ≈ 3 min; tick-count unit
matches the sibling thresholds), `review_max_interviews` (default 6).
`stuck_threshold`/`drift_threshold` keep their Phase 1 defaults — `04`'s
"LoopX thresholds are a shape, not constants" already happened when those
landed.

**D5 — The review cycle is the epilogue generalised, not a run and not a
stored workflow.** §0.8's three walls rule out the literal reading of `05`.
The post-terminal reserved subgraph reuses every proven invariant: reserved
namespace, NULL-`kvdag_node` reserved create, counter exclusion, live
`NodeCreated` DAG visibility, outcome immutability. The cost — reviews run
only on the resident finished run (D13) — buys zero new run-identity
machinery and zero risk to `run_terminal_ready`.

**D6 — The interviewer is a node; the interrogation row is provenance.**
§0.7 closes this: report capability requires the node contract, and the
storage schema was designed for exactly this split (`review_finding.
interview` optional, `interview_mode` on the finding). "The same spawn with
a different prompt" from Phase 3's framing holds precisely: the fork argv
is interrogation's, everything else is a node.

**D7 — Interviews are capped and ranked, not exhaustive.** A fan-out run
can hold dozens of Agent nodes; N forked panes at once is a cost and pane
explosion `00` §6 warns about. `review_max_interviews` (default 6) selects
by trouble score — attempts, interventions, schema failures, steers — which
is exactly the population the review exists to examine; unselected nodes
are still covered by the synthesis over measured evidence. The cap is
config, so "interview everyone" is one setting away.

**D8 — One more disjunct, three more call sites, no new guard machinery.**
`review_pending()` joins `is_live() || epilogue_pending()` in the run-start
refusal, the tick-liveness test, and the prune gate — the M7/E-8 pattern
applied a fourth time, in the same places, with the same tests.

**D9 — Watchdog covers review nodes; the epilogue keeps its own ladder.**
Interviews are the most novel spawn in the system and need anti-stuck most;
they are ordinary nodes, so the pass covers them for free. The epilogue's
bounded two-strike ladder predates the watchdog and already resolves every
failure into `GaveUp`; putting both systems on one node invites double
interventions. A `Blocked` interviewer degrades to evidence-only; a
`Blocked` synthesis fails the cycle.

**D10 — Partial capture is an app job the engine requests.** The engine
cannot read `result.json` (pure, §0.3), so the restart rung emits
`CapturePartial` and the app writes the checkpoint before feeding
`RestartNode` back. Chosen over "sampler streams drafts into the engine"
because draft payloads can be 256 KB and the engine needs them exactly
once, at restart — not every tick.

**D11 — `timeout_ms` finally does something, and what it does is Blocked.**
The field is authored, persisted, and silently ignored (§0.14b) — worse
than absent. A declared budget is an explicit contract, so exceeding it
skips the inference ladder and surfaces immediately; it is not a `Failed`
because the node did not fail — the human gets to decide, exactly like any
other blocker, and restart remains available.

**D12 — Findings are recorded wholesale, applied selectively.** The
synthesis reports everything; the store keeps everything (accepted or
not); apply marks the accepted subset. An unapplied finding is data for the
next review, not garbage.

**D13 — review.start needs the resident run; review.apply needs only the
store.** Starting a review needs the engine graph (nodes, edges, evidence
context) — rehydrating arbitrary historical runs into a review-capable
graph is real machinery for a marginal case (the ask happens at run end).
Apply, by contrast, reads rows and writes rows; making it store-only means
findings survive restarts (`awaiting_user` is spared by the interruption
sweep) and the human can decide days later, CLI or TUI, no run resident.
`workflow_review_run_not_resident`'s message names the constraint.

**D14 — v1 compiles node-level changes only.** Prompt-level merges and
whole-node replacement cover `00` Feature 4's actual requirements
(prompt rewrites; fire-and-replace). Edge surgery multiplies the compile's
failure modes (cycles, orphaned ports, condition references) for a change
class no design doc demands; a structural finding that wants new topology
is recorded and readable — a human applies it via `workflow.version.create`.
Revisit when a real cycle produces one.

**D15 — Accepted findings advance the head.** The human just accepted them;
minting a version nobody runs by default would make "accept" a no-op with
extra steps. The old version is immutable and one `--version` away.
`change_summary` is honest text (one line per finding); per-node
machine-readable provenance is `review_finding.applied_in`, which exists
for exactly this (§0.6).

**D16 — No new migrations, no new journal kinds.** Phase 1 shipped the
entire review schema, the `watchdog` journal kind, and the checkpoint kind
ASSERT (§0.5, §0.6). Review lifecycle facts live in `review_cycle` rows
(UNIQUE-ish per run by construction), not journal payloads — the D10
lesson from Phase 3 (journal-projection reads are the field-loss factory).
The one thing that could force a migration is WS-B's bind audit; if it
does, it is `0005` with exactly that fix and an amendment here.

**D17 — `workflow.archive` ships; `workflow delete` still does not.** The
column, the launcher filter, and the store plumbing all exist (§0.14c);
the toggle is two lines per layer and reversible. Delete stays out: runs,
summaries, checkpoints, and now review findings hang off workflows, and
`fca59489` just finished making create/adopt semantics sane — a delete
design deserves its own phase. Version growth from self-improvement is
user-gated (D12/D15: versions mint only on accept), so it is bounded by
human action, not automation.

**D18 — `KARVEX_WORKFLOW_REVIEW_COMMAND` is the one review override.** One
env var for both interviewer and synthesis nodes (the stub reads its env
and task to know which it is), with `KARVEX_WORKFLOW_SUMMARY_COMMAND`'s
exact semantics and failure rules (read once, disable on malformed, one
notice, no silent fallback — E-11/E-16). CI cannot run `claude`; the
declared-binding rule from Phase 1 holds.

**D19 — The ask is an affordance, not a modal.** `00` says "the TUI asks
(never automatic)"; a blocking modal after every run would train users to
dismiss it. A header segment + one-shot notice + `V` satisfies "asks",
stays out of the way, and the cycle's durability (D13) means "later" is a
real answer.

**D20 — ACP stays out; the assessment ran and no trigger fired.** Checked
against `01-acp-evaluation.md` §5 (2026-08-10): the `claude` binary has no
native ACP mode — ACP support remains the separate Node-based Zed adapter
(`@zed-industries/claude-agent-acp`), which is still pre-1.0 with no
documented stability policy for `_meta.claudeCode.*`; no team
infrastructure under SDK/ACP sessions; no standardised subagent node kind.
Independently, Phase 4's new agents are exactly the ones `04` §4.5 requires
to be *visible* panes ("nothing runs where the user cannot see it"), and
the fork-resume argv is a CLI-flag surface ACP does not expose — so even a
fired trigger would not put the executor on this phase's path. Re-evaluate
after Phase 4 only if §5's conditions change.

**D21 — The skill file's workflow section is product behaviour with a
parity test, seeded from the existing draft.** The reclassification (§0.11,
Karan's call): a workflow node is an agent that reads `kvx --skill`, so a
skill file with no workflow section is a node-contract defect — the
summariser bug one level up, and Phase 4's interviewer/synthesis nodes
widen the exposure. Three consequences. (a) It is a workstream (WS-K) with
tests, sequenced before the review work, not a docs task at the end. (b)
The content is scoped to node operation — discovery, the self-report
contract, the env vars, expand, reserved-path and steering-frame facts for
the Phase 4 node types — at the file's existing density; it is not a CLI
reference, and the operator verbs keep their two-line mention. (c) The
`docs/workflow-gaps` draft is adopted rather than rewritten: it was
verified against the post-oct-fixes tree line by line — every verb, flag,
env var, and the `needs_attention` claim are accurate — and only its two
omissions (`--input` on expand, `--result-file` on complete) are
corrected. The rest of that worktree's uncommitted `.mdx` edits stay
descoped; WS-K takes the skill section only, re-drafted in this tree,
never a branch merge. The parity test treats the skill as the fourth
hand-maintained surface describing the CLI (the Phase 1 "three
hand-maintained places" family) so it can never silently lie again — a
test, not a review habit, because today's gap proves review habits don't
hold this surface.

---

## 5. Manual validation checklist (real `claude`, not CI)

**Isolation first.** Any throwaway server for validation needs **all five**
vars — the socket alone does NOT isolate workflow state (agents leaked into
the real state dir today by assuming it did):

```bash
export XDG_CONFIG_HOME=/tmp/kvx-p4/config
export XDG_STATE_HOME=/tmp/kvx-p4/state
export KARVEX_SOCKET_PATH=/tmp/kvx-p4/karvex.sock
export KARVEX_WORKFLOW_DB_PATH=/tmp/kvx-p4/workflow-db
export KARVEX_WORKFLOW_RUNS_DIR=/tmp/kvx-p4/workflow-runs
```

Never touch the live session's socket
(`~/.config/karvex/karvex.sock`). When testing from inside a karvex
session, additionally clear inherited overrides:
`env -u KARVEX_SOCKET_PATH -u KARVEX_CLIENT_SOCKET_PATH cargo run -- …`.

1. **Watchdog on a real stall.** Run a two-node agent workflow whose second
   node's prompt instructs it to wait for input that never comes. Watch the
   ladder: nudge text arrives in the pane (visible, journalled), then the
   structured re-prompt naming the unfilled schema fields, then a restart
   whose fresh pane's `task.md` shows `## Previous attempt`, then
   `blocked` with a notice and the DAG blocker line. Confirm
   `kvx workflow node show` prints the intervention count and it survives a
   server restart.
2. **Productive-use.** A node told to "think out loud indefinitely without
   using tools" escalates past `idle_budget_ticks` even though the screen
   streams text.
3. **No false positives.** A legitimately slow node (long build via Bash)
   is never escalated while tool calls tick.
4. **The kill switch.** Same stall with `watchdog_enabled = false`: nothing
   fires.
5. **A real review.** After a run with ≥2 agent nodes: `V` (or
   `kvx workflow review start`) — interviewer panes open as *forked*
   sessions that demonstrably remember their run (ask one what it was
   asked to do); the source transcripts are byte-identical after
   (checksum before/after); synthesis lands; the findings overlay opens;
   accept a prompt-level finding; the new version appears in
   `kvx workflow get` with `origin: self_improvement`, parent = the run's
   version; run the workflow again and confirm the changed prompt.
6. **Evidence-only honesty.** Delete one target's transcript before
   `review start`: its interviewer runs fresh (no `--resume`), its findings
   show `evidence-only` in the overlay and `interview_mode:
   "evidence_only"` in `--json`.
7. **Late apply.** Start a review, reach `awaiting_user`, restart the
   server, `kvx workflow review apply` from the CLI — succeeds with no run
   resident.
8. **Guards.** `kvx workflow run start` during a live review is refused
   with a message naming the review; accepted after it closes.
9. **Learnability.** In a plain (non-workflow) karvex pane, ask a fresh
   `claude` with only `kvx --skill` output in context to explain how a
   workflow node finishes: it must name `result.json` in
   `KARVEX_WORKFLOW_NODE_DIR`, `kvx workflow node complete`, and the
   idle-is-not-done rule — the WS-K acceptance check, run against the
   built binary rather than the source file.

---

## 6. Assumptions flagged for review

- **A1 (skill section scope).** WS-K ships the node-operation section only
  (§4 D21); the task document remains the primary contract carrier and the
  skill the taught fallback. The rest of the `docs/workflow-gaps`
  worktree's uncommitted docs edits stay descoped — if Karan wants them,
  that is a separate docs pass, not a WS-K widening. The parity test pins
  syntax against the CLI; contract *semantics* stated in prose (the
  reserved-path and steering-frame paragraph) are protected only by §3
  item 13's blast-radius rule.
- **A2 (fork-argv combination).** `--resume --fork-session` composed with
  `--add-dir` and a seed prompt is unverified until step 0b; the
  evidence-only path is the designed fallback for a "no", and interview
  targets degrade rather than the phase blocking.
- **A3 (transcript format).** D3's delta reader assumes tool-use entries
  and usage fields are present and line-parseable in the transcript JSONL.
  Step 0b(d) verifies; the fallback narrows materiality and is recorded as
  an amendment.
- **A4 (resident-run constraint).** Reviews start only on the resident
  finished run (D13). Runs the browser shows from history are not
  reviewable in v1; the error message says so.
- **A5 (interview cap).** `review_max_interviews = 6` with trouble-score
  ranking; "interview everyone" is a config change, not a code change.
- **A6 (head advance).** Applying findings advances the workflow head
  (D15). If Karan prefers minted-but-not-head, it is a one-line change in
  the apply handler and a doc change.
- **A7 (timeout semantics).** `timeout_ms` → immediate `Blocked` (D11), not
  `Failed`. Flagged because `04` never specified it.
- **A8 (dormant learn path).** `observe_interrogation_session_id` stays
  uncalled (§0.14a); both interrogations and interviews pre-mint fork ids.
  If a future claude drops the `--session-id`+`--resume` combination, wire
  it then.
- **A9 (Phase 3 verification).** Step 0a assumes the unverified Phase 3
  machinery mostly works and only needs evidence; if it finds real defects,
  the fix-first items reshape step 2c's start date, not this plan's shape.

## 7. Risk register

| # | Risk | Mitigation |
|---|---|---|
| R-1 | The post-terminal review phase destabilises the terminal state machine (the Phase 3 R-1, now with N nodes and edges) | WS-A sole engine owner; the D1 outcome-immutability contract re-asserted by test before/after every review transition; review state is `ReviewState`, never run status; the Phase 3 audit's three structural legs re-verified in step 2a's test list |
| R-2 | Watchdog false positives spam or wreck healthy nodes | Kill switch (D4); nudge-first ladder; streak resets on every intervention; productive-use requires *three* zero-signals, not one; manual scenario 3 pins the slow-but-working case; thresholds are config |
| R-3 | The sampler's per-tick I/O degrades the main loop | Bounded: ≤ `max_parallel_nodes` nodes, cursor reads capped at 512 KiB, stat-first everywhere; measured in step 2c with a wide fixture before merge |
| R-4 | Unverified Phase 3 foundations (restore, refusal ladder, retention) crack under Phase 4 load | Step 0a is a blocking gate with named scenarios; failures become fix-first items before 2c |
| R-5 | Fork-spawn behaviour differs from assumption | Step 0b spike before parallel work; evidence-only fallback per target; the frozen interrogation argv is untouched either way |
| R-6 | Field-loss class recurs on the new durable paths (interventions, usage, findings) | WS-B's bind audit runs first; D16-style per-field restart tests mandatory; the e2e restart-fidelity scenario extends to every new field |
| R-7 | Review cost surprises (N forked panes) | Interview cap + trouble ranking (D7); Light demand through the run's tier; the ask is opt-in (D19) |
| R-8 | Compile-on-apply produces a broken version | `Kvdag::try_new` gates the mint; failure leaves the cycle `AwaitingUser` with the validation message; node-level-only scope (D14) keeps the failure space small |
| R-9 | Pre-existing `terminal::state::metadata` wall-clock flake muddies CI signal | Named in the merge gate: rerun-and-report, never fix-in-passing, never block on it alone |
| R-10 | Scope creep toward Monitor/Gate/edge-surgery/history-review | All four explicitly out (§preamble, D14, A4); the plan names where each would land later |
| R-11 | The skill section drifts from the CLI it describes, or collides with the uncommitted `docs/workflow-gaps` worktree | WS-K's parity test makes verb/flag/env drift a test failure in `just check`; the section is re-drafted in this tree (never a branch merge), and the draft worktree's other edits are explicitly out of scope (A1) |

## 8. Amendment log (build round)

*Empty at freeze. Every post-freeze change to this document gets an entry
here mapped to the build ledger, per the Phase 3 discipline. The known
candidates: step 0a findings (A9), step 0b outcomes (A2/A3), and WS-B's
bind audit (D16's migration escape hatch).*
