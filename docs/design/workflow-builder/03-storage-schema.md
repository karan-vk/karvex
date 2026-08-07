# Storage: embedded SurrealDB schema

Covers workflow definitions, immutable kvdag versioning, runs, per-node
checkpoints, run summaries, interrogation-restore records, and the
self-improvement review cycle.

> **Addendum — 2026-08-07:** owner directive supersedes the SurrealDB mandate
> below. Karvex ships one slim binary per platform with the workflow
> subsystem always included; the store is reimplemented on `redb` (pure
> Rust, ~1-2 MiB) behind the same `WorkflowStore` public API described here.
> The SurrealDB evaluation, schema, and rejected alternatives below are kept
> as-is for historical record — see `00-overview.md` D4 for the equivalent
> note on the decision list.

---

## 1. Engine and crate configuration

```toml
# Cargo.toml
[features]
default = ["workflow"]
workflow = ["dep:surrealdb", "dep:surrealdb-types"]

[dependencies]
surrealdb = { version = "3", default-features = false, features = ["kv-surrealkv", "kv-mem"], optional = true }
surrealdb-types = { version = "3", optional = true }
```

Verified versions: `surrealdb 3.2.4`, `surrealdb-core 3.2.4`,
`surrealdb-types 3.2.4`, `surrealkv 0.21.3`.

**Decisions:**

- **`SurrealKv` (`kv-surrealkv`) is the on-disk engine.** Pure-Rust storage
  layer, no C/C++ toolchain for storage itself, SurrealDB's own recommendation
  for embedded/local-first. Empirically verified end to end: schemafull DDL,
  insert, typed query, `RELATE` edge, graph traversal, and **close + reopen from
  disk with data intact**.
- **`kv-mem` is kept** solely so store and engine tests run against an in-memory
  database with no disk I/O, matching the project's "unit tests live next to the
  code, testable without PTYs" philosophy.
- **`default-features = false` is mandatory.** The crate default is
  `["protocol-ws", "rustls"]`, which drags in a WebSocket + TLS client stack for
  talking to a *remote* SurrealDB server. karvex is 100% in-process embedded.
- **`kv-rocksdb` is rejected** — a C++ build on every cross-compile target for no
  benefit at this scale.
- **`allocator` (jemalloc/mimalloc) is rejected** — another native dependency for
  a low-concurrency single-user TUI unlikely to be allocator-bound.
- **All DB-facing structs derive `surrealdb_types::SurrealValue`, not serde.**
  This is not a style preference: against 3.2.4, `Create::content()`/`Select`
  bound on `SurrealValue`, and `surrealdb::RecordId` no longer re-exports at the
  crate root. The serde-based examples still shown in SurrealDB's own docs **do
  not compile**. This trap was hit empirically during Phase 0 (8 compile errors)
  and is documented here so it is not rediscovered.

**Accepted costs** (see `00-overview.md` D4): ~+257 net-new crates (≈2.76×),
+2.5–4 min clean build, double-digit-MB stripped binary growth, and a mandatory
`cc` + `cmake` toolchain on every release target — transitively via
`aws-lc-sys` ← `jsonwebtoken`, which is **not** feature-gated in
`surrealdb-core` (nor are `axum`/`hyper`/`tonic` or the `diskann` vector-index
crates; there is no way to drop them).

## 2. Location, lifecycle, and locking

- Path: **`crate::config::state_dir().join("workflow")`**. `state_dir()`
  (`src/config/io.rs`) is the existing user-level persistent-state helper and
  already appends `app_dir_name()`, so the result is `~/.local/state/karvex/workflow`
  on Linux with no extra `karvex` segment joined and no hand-rolled `$HOME`.
  Explicitly **not** `crate::session::data_dir()` — despite the name, that is the
  *per-session* directory (`config_dir()/sessions/<name>`), and using it would
  silently break the reusable-across-sessions requirement. There is no "user data
  dir" helper in this codebase; `config_dir()` and `state_dir()` are the only two
  path roots, and persistent run history belongs under state, not config.
- Caveat worth knowing before manual validation: `app_dir_name()` returns
  `karvex-dev` under `debug_assertions`, so a debug build (`cargo run`) uses a
  **different database** from an installed release build.
- Override: `KARVEX_WORKFLOW_DB_PATH`.
- Namespace `karvex`, database `workflow`.
- The server opens the store lazily on first `workflow.*` use, not at startup, so
  a karvex that never touches workflows never pays the open cost.
- **Locking:** SurrealKv holds an exclusive `LOCK`. If another karvex server owns
  it, the subsystem enters `Unavailable { reason: "store_locked", holder }` and
  every `workflow.*` method returns that structured error. Surfaced once in the
  TUI, never silent, never falling back to an in-memory store (which would look
  like data loss).
- Migrations live in `src/workflow/store/migrations/NNNN_*.surql`, are embedded
  with `include_str!`, and are applied in order inside a transaction. Applied
  versions are recorded in `schema_meta`. A test asserts that applying all
  migrations to a fresh `Mem` database yields the expected `INFO FOR DB` shape,
  so schema drift fails CI rather than production.

## 3. Immutability policy

`kvdag_version`, `kvdag_node`, `kvdag_edge`, `node_checkpoint`, and `run_event`
are **append-only**. Enforcement is by construction, in three layers:

1. The Rust store exposes **no update or delete method** for those tables. There
   is no API to call.
2. A store test issues an `UPDATE`/`DELETE` against each and asserts the store
   API surface cannot express it (compile-level), plus a runtime test that the
   only mutating helpers are `create_*`.
3. Retention pruning (§9) is the one exception and goes through a single
   explicitly-named `prune_run_history` entry point that can only delete whole
   *runs*, never individual records inside a retained run.

Engine-level `READONLY` field flags are deliberately **not** relied on: their
behaviour was not verified against 3.2.4, and a silent behaviour change would
weaken the guarantee without failing a test.

SurrealKV's native record versioning (`Surreal::new::<SurrealKv>(path).versioned()`)
is **not** used for kvdag versioning. It is engine-specific, non-portable if the
storage engine is ever swapped, and its "list every version of X" surface is far
less queryable than an explicit version chain — which the spec's "browse every
revision" requirement needs.

---

## 4. Schema

Written as the actual migration DDL. `option<T>` marks nullable fields.

### 4.1 Definitions

```surql
-- ─── workflow: stable identity for a family of kvdag versions ───────────────
DEFINE TABLE workflow SCHEMAFULL;
DEFINE FIELD name           ON workflow TYPE string ASSERT string::len($value) > 0;
DEFINE FIELD description    ON workflow TYPE string DEFAULT "";
DEFINE FIELD head_version   ON workflow TYPE option<record<kvdag_version>>;
DEFINE FIELD default_tier   ON workflow TYPE string
       ASSERT $value IN ["auto","max","high","medium","low"] DEFAULT "auto";
DEFINE FIELD archived       ON workflow TYPE bool DEFAULT false;
DEFINE FIELD created_at     ON workflow TYPE datetime DEFAULT time::now();
DEFINE FIELD updated_at     ON workflow TYPE datetime DEFAULT time::now();
DEFINE INDEX workflow_name  ON workflow FIELDS name UNIQUE;

-- ─── kvdag_version: an immutable revision of the graph ──────────────────────
DEFINE TABLE kvdag_version SCHEMAFULL;
DEFINE FIELD workflow       ON kvdag_version TYPE record<workflow>;
DEFINE FIELD version        ON kvdag_version TYPE int ASSERT $value > 0;
DEFINE FIELD parent         ON kvdag_version TYPE option<record<kvdag_version>>;
DEFINE FIELD origin         ON kvdag_version TYPE string
       ASSERT $value IN ["authored","imported","self_improvement","restore_rewrite"];
DEFINE FIELD change_summary ON kvdag_version TYPE string DEFAULT "";
-- workflow-wide contract text prepended to every node's system prompt
DEFINE FIELD contract       ON kvdag_version TYPE string DEFAULT "";
-- declared run-argument namespace: [{name, required, default, description}].
-- A {{name}} in a prompt template resolves to an inbound edge port OR one of these.
DEFINE FIELD args           ON kvdag_version TYPE array<object> DEFAULT [];
-- authoritative growth guardrails. A run may narrow these but never widen them;
-- raising a ceiling is an authoring edit, which by construction creates a new version.
DEFINE FIELD max_depth      ON kvdag_version TYPE int DEFAULT 3;
DEFINE FIELD max_nodes      ON kvdag_version TYPE int DEFAULT 24;
-- sha256 over the canonical serialisation of nodes+edges; identifies "same graph"
DEFINE FIELD spec_digest    ON kvdag_version TYPE string;
DEFINE FIELD created_at     ON kvdag_version TYPE datetime DEFAULT time::now();
DEFINE INDEX kvdag_version_unique ON kvdag_version FIELDS workflow, version UNIQUE;

-- ─── kvdag_node: one unit of work in a specific version ─────────────────────
DEFINE TABLE kvdag_node SCHEMAFULL;
DEFINE FIELD version        ON kvdag_node TYPE record<kvdag_version>;
-- stable across versions: the identity self-improvement and checkpoint restore key on
DEFINE FIELD node_key       ON kvdag_node TYPE string ASSERT string::len($value) > 0;
DEFINE FIELD label          ON kvdag_node TYPE string;
DEFINE FIELD role           ON kvdag_node TYPE string DEFAULT "";
DEFINE FIELD kind           ON kvdag_node TYPE string
       ASSERT $value IN ["agent","internal","gate","monitor"];
-- how the node is bound at spawn (04 §4.2): "agent" = a `claude` teammate with
-- managed-agent confirmation and agent.prompt steering; "command" = a plain
-- process from `command`, self-report completion only. Declared, never inferred.
DEFINE FIELD runner         ON kvdag_node TYPE string
       ASSERT $value IN ["agent","command"] DEFAULT "agent";
-- argv (never a shell string); required iff runner = "command"
DEFINE FIELD command        ON kvdag_node TYPE option<array<string>>;
-- drives the tier → (model, effort) mapping
DEFINE FIELD demand         ON kvdag_node TYPE string
       ASSERT $value IN ["peak","critical","standard","light"] DEFAULT "standard";
DEFINE FIELD prompt_template ON kvdag_node TYPE string;
DEFINE FIELD system_contract ON kvdag_node TYPE option<string>;
-- JSON Schema the node's result must validate against before it may complete
DEFINE FIELD output_schema  ON kvdag_node TYPE object;
DEFINE FIELD max_attempts   ON kvdag_node TYPE int DEFAULT 2;
DEFINE FIELD timeout_ms     ON kvdag_node TYPE option<int>;
DEFINE FIELD isolation      ON kvdag_node TYPE string
       ASSERT $value IN ["none","worktree"] DEFAULT "none";
-- true = not scheduled directly; only instantiated by an accepted expand proposal
DEFINE FIELD is_template    ON kvdag_node TYPE bool DEFAULT false;
-- node_keys of templates this node is allowed to expand into
DEFINE FIELD expand_allow   ON kvdag_node TYPE array<string> DEFAULT [];
-- 0 = this node may not expand. Expansion is opt-in per node, matching
-- expand_allow's empty default; 4 is the suggested value when it is enabled.
DEFINE FIELD expand_max     ON kvdag_node TYPE int DEFAULT 0;
DEFINE FIELD position       ON kvdag_node TYPE option<object>; -- authoring hint only
DEFINE INDEX kvdag_node_key ON kvdag_node FIELDS version, node_key UNIQUE;

-- ─── kvdag_edge: typed dependency between two nodes of one version ──────────
DEFINE TABLE kvdag_edge SCHEMAFULL TYPE RELATION FROM kvdag_node TO kvdag_node;
DEFINE FIELD kind           ON kvdag_edge TYPE string
       ASSERT $value IN ["sequence","data","conditional"];
-- typed, total, loop-free predicate over the source node's validated output
DEFINE FIELD condition      ON kvdag_edge TYPE option<object>;
-- how much of the source's checkpoint is handed to the target
DEFINE FIELD payload        ON kvdag_edge TYPE string
       ASSERT $value IN ["none","summary","full"] DEFAULT "summary";
-- names the slot the payload lands in inside the target's prompt template
DEFINE FIELD port           ON kvdag_edge TYPE option<string>;
```

### 4.2 Runs

```surql
-- ─── workflow_run: one execution of one kvdag_version ───────────────────────
DEFINE TABLE workflow_run SCHEMAFULL;
DEFINE FIELD workflow       ON workflow_run TYPE record<workflow>;
DEFINE FIELD kvdag_version  ON workflow_run TYPE record<kvdag_version>;
DEFINE FIELD tier           ON workflow_run TYPE string
       ASSERT $value IN ["auto","max","high","medium","low"];
DEFINE FIELD status         ON workflow_run TYPE string
       ASSERT $value IN ["pending","running","paused","succeeded","failed","cancelled"];
DEFINE FIELD args           ON workflow_run TYPE object DEFAULT {};
-- prior runs whose summaries are injected as context (spec assumption 3)
DEFINE FIELD context_runs   ON workflow_run TYPE array<record<workflow_run>> DEFAULT [];
-- checkpoint restore source, if any
DEFINE FIELD restore_from   ON workflow_run TYPE option<object>;
-- effective guardrails for this run; always <= the version's, enforced on create
-- (tier narrowing only — see 04-kvdag-and-execution.md §3.4 / §7.4)
DEFINE FIELD max_depth      ON workflow_run TYPE int;
DEFINE FIELD max_nodes      ON workflow_run TYPE int;
-- where the run's panes live; public API ids, never internal indices
DEFINE FIELD workspace_id   ON workflow_run TYPE option<string>;
DEFINE FIELD tab_id         ON workflow_run TYPE option<string>;
DEFINE FIELD started_at     ON workflow_run TYPE datetime DEFAULT time::now();
DEFINE FIELD ended_at       ON workflow_run TYPE option<datetime>;
DEFINE FIELD total_tokens   ON workflow_run TYPE int DEFAULT 0;
DEFINE FIELD total_tool_uses ON workflow_run TYPE int DEFAULT 0;
DEFINE FIELD nodes_total    ON workflow_run TYPE int DEFAULT 0;
DEFINE FIELD nodes_done     ON workflow_run TYPE int DEFAULT 0;
DEFINE FIELD failure        ON workflow_run TYPE option<object>;
DEFINE INDEX run_by_workflow ON workflow_run FIELDS workflow, started_at;

-- ─── run_node: one materialised node instance in a run ──────────────────────
DEFINE TABLE run_node SCHEMAFULL;
DEFINE FIELD run            ON run_node TYPE record<workflow_run>;
DEFINE FIELD kvdag_node     ON run_node TYPE record<kvdag_node>;
DEFINE FIELD node_key       ON run_node TYPE string;
-- topological address, unique in a run: "review", "research/2", "research/2/verify"
DEFINE FIELD instance_path  ON run_node TYPE string;
DEFINE FIELD parent         ON run_node TYPE option<record<run_node>>;
DEFINE FIELD depth          ON run_node TYPE int DEFAULT 0;
DEFINE FIELD status         ON run_node TYPE string
       ASSERT $value IN ["pending","ready","running","needs_attention",
                         "blocked","succeeded","failed","skipped",
                         "restored","cancelled"];
DEFINE FIELD model          ON run_node TYPE string;
DEFINE FIELD effort         ON run_node TYPE string;
DEFINE FIELD demand         ON run_node TYPE string;
DEFINE FIELD attempt        ON run_node TYPE int DEFAULT 1;
-- pane binding: public API ids only (runtime/client boundary: neutral names)
DEFINE FIELD pane_id        ON run_node TYPE option<string>;
DEFINE FIELD terminal_id    ON run_node TYPE option<string>;
-- karvex-assigned Claude session identity (claude --session-id <uuid>)
DEFINE FIELD agent_session_id ON run_node TYPE option<string>;
DEFINE FIELD transcript_path  ON run_node TYPE option<string>;
DEFINE FIELD cwd            ON run_node TYPE option<string>;
DEFINE FIELD node_dir       ON run_node TYPE option<string>;
DEFINE FIELD started_at     ON run_node TYPE option<datetime>;
DEFINE FIELD ended_at       ON run_node TYPE option<datetime>;
DEFINE FIELD total_tokens   ON run_node TYPE int DEFAULT 0;
DEFINE FIELD tool_uses      ON run_node TYPE int DEFAULT 0;
DEFINE FIELD duration_ms    ON run_node TYPE int DEFAULT 0;
-- which completion signal was accepted (see 00-overview.md D7)
DEFINE FIELD evidence       ON run_node TYPE option<string>
       ASSERT $value == NONE OR $value IN ["self_report","hook","detection","restored"];
-- mandatory close-out resolution (see 00-overview.md D13)
DEFINE FIELD succession     ON run_node TYPE option<string>
       ASSERT $value == NONE OR $value IN ["satisfied","blocked","no_followup"];
DEFINE FIELD blocker        ON run_node TYPE option<object>;
DEFINE FIELD restored_from  ON run_node TYPE option<record<node_checkpoint>>;
DEFINE FIELD watchdog_interventions ON run_node TYPE int DEFAULT 0;
DEFINE INDEX run_node_instance ON run_node FIELDS run, instance_path UNIQUE;
DEFINE INDEX run_node_by_run   ON run_node FIELDS run, status;

-- ─── run_edge: a materialised dependency, with its firing record ────────────
DEFINE TABLE run_edge SCHEMAFULL TYPE RELATION FROM run_node TO run_node;
DEFINE FIELD run            ON run_edge TYPE record<workflow_run>;
DEFINE FIELD kind           ON run_edge TYPE string;
DEFINE FIELD kvdag_edge     ON run_edge TYPE option<record<kvdag_edge>>;
DEFINE FIELD condition_result ON run_edge TYPE option<bool>;
DEFINE FIELD fired_at       ON run_edge TYPE option<datetime>;

-- ─── spawned: dynamic growth provenance (teammate-spawned teammates) ────────
DEFINE TABLE spawned SCHEMAFULL TYPE RELATION FROM run_node TO run_node;
DEFINE FIELD run            ON spawned TYPE record<workflow_run>;
DEFINE FIELD template_key   ON spawned TYPE string;
DEFINE FIELD proposal_id    ON spawned TYPE string;
DEFINE FIELD accepted_at    ON spawned TYPE datetime DEFAULT time::now();
```

### 4.3 Journal, checkpoints, summaries

```surql
-- ─── run_event: append-only journal; the DAG view is a projection of this ───
DEFINE TABLE run_event SCHEMAFULL;
DEFINE FIELD run            ON run_event TYPE record<workflow_run>;
DEFINE FIELD seq            ON run_event TYPE int;             -- monotonic per run
DEFINE FIELD at             ON run_event TYPE datetime DEFAULT time::now();
DEFINE FIELD kind           ON run_event TYPE string
       ASSERT $value IN ["run_started","run_finished","node_created","node_started",
                         "node_status","node_output","tool_activity","plan",
                         "usage","message_delivered","steer","interrupt",
                         "expand_proposed","expand_accepted","expand_rejected",
                         "growth_limited","watchdog","checkpoint","succession",
                         "error"];
DEFINE FIELD run_node       ON run_event TYPE option<record<run_node>>;
DEFINE FIELD payload        ON run_event TYPE object DEFAULT {};
DEFINE INDEX run_event_seq  ON run_event FIELDS run, seq UNIQUE;

-- ─── node_checkpoint: immutable per-node output snapshots ───────────────────
DEFINE TABLE node_checkpoint SCHEMAFULL;
DEFINE FIELD run            ON node_checkpoint TYPE record<workflow_run>;
DEFINE FIELD run_node       ON node_checkpoint TYPE record<run_node>;
DEFINE FIELD node_key       ON node_checkpoint TYPE string;
DEFINE FIELD instance_path  ON node_checkpoint TYPE string;
DEFINE FIELD kvdag_version  ON node_checkpoint TYPE record<kvdag_version>;
DEFINE FIELD seq            ON node_checkpoint TYPE int;
DEFINE FIELD kind           ON node_checkpoint TYPE string
       ASSERT $value IN ["result","partial","artifact_index"];
DEFINE FIELD schema_valid   ON node_checkpoint TYPE bool DEFAULT false;
-- inline payload, capped; larger results spill to artifact_paths
DEFINE FIELD payload        ON node_checkpoint TYPE object DEFAULT {};
-- token-lean handoff text; this, not payload, is what "payload: summary" edges pass
DEFINE FIELD summary        ON node_checkpoint TYPE string DEFAULT "";
DEFINE FIELD artifact_paths ON node_checkpoint TYPE array<string> DEFAULT [];
DEFINE FIELD digest         ON node_checkpoint TYPE string;
DEFINE FIELD created_at     ON node_checkpoint TYPE datetime DEFAULT time::now();
DEFINE INDEX checkpoint_seq ON node_checkpoint FIELDS run_node, seq UNIQUE;
DEFINE INDEX checkpoint_lookup ON node_checkpoint FIELDS kvdag_version, node_key, kind;

-- ─── run_summary: the token-efficient end-of-run record ─────────────────────
DEFINE TABLE run_summary SCHEMAFULL;
DEFINE FIELD run            ON run_summary TYPE record<workflow_run>;
DEFINE FIELD kvdag_version  ON run_summary TYPE record<kvdag_version>;
DEFINE FIELD text           ON run_summary TYPE string;      -- budgeted, see §7
DEFINE FIELD outcome        ON run_summary TYPE string;
DEFINE FIELD highlights     ON run_summary TYPE array<string> DEFAULT [];
DEFINE FIELD open_gaps      ON run_summary TYPE array<string> DEFAULT [];
-- one line per node: {node_key, verdict, one_liner}
DEFINE FIELD per_node       ON run_summary TYPE array<object> DEFAULT [];
DEFINE FIELD token_estimate ON run_summary TYPE int DEFAULT 0;
DEFINE FIELD generated_by   ON run_summary TYPE option<record<run_node>>;
DEFINE FIELD created_at     ON run_summary TYPE datetime DEFAULT time::now();
DEFINE INDEX run_summary_run ON run_summary FIELDS run UNIQUE;
```

### 4.4 Interrogation restore

```surql
-- a forked, read-only-by-intent revival of a past node's Claude session
DEFINE TABLE interrogation SCHEMAFULL;
DEFINE FIELD run_node       ON interrogation TYPE record<run_node>;
DEFINE FIELD source_session_id ON interrogation TYPE string;
DEFINE FIELD forked_session_id ON interrogation TYPE string;  -- --fork-session
DEFINE FIELD transcript_path   ON interrogation TYPE option<string>;
DEFINE FIELD cwd            ON interrogation TYPE string;
DEFINE FIELD pane_id        ON interrogation TYPE option<string>;
DEFINE FIELD started_at     ON interrogation TYPE datetime DEFAULT time::now();
DEFINE FIELD ended_at       ON interrogation TYPE option<datetime>;
DEFINE FIELD note           ON interrogation TYPE string DEFAULT "";
```

The original run's transcript is never mutated: `--fork-session` allocates a new
session id, so the source `run_node.transcript_path` stays byte-identical and
remains valid evidence for the self-improvement routine.

**Transcript availability is not guaranteed, and the design says what happens
when it is gone.** Claude owns the transcript file (§7); it can vanish through
Claude-side cleanup or compaction, a `~/.claude` reset, or simply running on a
different machine — and run history is retained for 50 runs (§9), which can
outlive Claude's own session retention. Therefore:

- Before spawning the fork, the engine **stats `run_node.transcript_path`**. It
  is the precondition, checked every time, not assumed.
- If it is absent, `workflow.node.interrogate` returns a structured
  `transcript_unavailable` error (carrying the node path and the missing path)
  which the TUI surfaces, rather than spawning a `claude --resume` that would
  fail opaquely inside the pane.
- The offered degraded path is a **reconstructed** node: a fresh `claude` seeded
  with the node's stored `node_checkpoint` payload/summary and its `task.md`,
  recorded with `reconstructed = true` and never presented as the revived
  original. It answers "here is what that node produced", not "here is what that
  node was thinking".

```surql
-- distinguishes a genuine resumed fork from an evidence-seeded reconstruction
DEFINE FIELD reconstructed  ON interrogation TYPE bool DEFAULT false;
DEFINE FIELD seeded_from    ON interrogation TYPE option<record<node_checkpoint>>;
```

### 4.5 Self-improvement review cycle

```surql
DEFINE TABLE review_cycle SCHEMAFULL;
DEFINE FIELD run            ON review_cycle TYPE record<workflow_run>;
DEFINE FIELD kvdag_version  ON review_cycle TYPE record<kvdag_version>;
DEFINE FIELD status         ON review_cycle TYPE string
       ASSERT $value IN ["running","awaiting_user","applied","declined","failed"];
DEFINE FIELD started_at     ON review_cycle TYPE datetime DEFAULT time::now();
DEFINE FIELD ended_at       ON review_cycle TYPE option<datetime>;
DEFINE FIELD resulting_version ON review_cycle TYPE option<record<kvdag_version>>;

-- the 1:1 interviews this cycle conducted, one per reviewed teammate
DEFINE FIELD interviews     ON review_cycle TYPE array<record<interrogation>> DEFAULT [];

DEFINE TABLE review_finding SCHEMAFULL;
DEFINE FIELD cycle          ON review_finding TYPE record<review_cycle>;
DEFINE FIELD run_node       ON review_finding TYPE option<record<run_node>>;
DEFINE FIELD node_key       ON review_finding TYPE string;
-- the 1:1 this finding came out of. NONE only when the interview was evidence-only.
DEFINE FIELD interview      ON review_finding TYPE option<record<interrogation>>;
-- "resumed" = the teammate's own account via `claude --resume … --fork-session`;
-- "evidence_only" = the source session could not be resumed, so the finding is an
-- inference over the journal/checkpoints/usage rather than the teammate's answer.
DEFINE FIELD interview_mode ON review_finding TYPE string
       ASSERT $value IN ["resumed","evidence_only"] DEFAULT "evidence_only";
DEFINE FIELD level          ON review_finding TYPE string
       ASSERT $value IN ["prompt","structural"];
DEFINE FIELD verdict        ON review_finding TYPE string
       ASSERT $value IN ["keep","improve","replace"];
DEFINE FIELD rationale      ON review_finding TYPE string;
-- measured, not asserted: {attempts, watchdog_interventions, tokens, tool_uses,
--                          duration_ms, downstream_rework, schema_failures}
DEFINE FIELD evidence       ON review_finding TYPE object DEFAULT {};
-- the concrete change: prompt rewrite, or a node/edge delta
DEFINE FIELD proposed_change ON review_finding TYPE object DEFAULT {};
-- mandatory when verdict = "replace": a full replacement role definition
DEFINE FIELD replacement    ON review_finding TYPE option<object>;
DEFINE FIELD accepted       ON review_finding TYPE bool DEFAULT false;
DEFINE FIELD applied_in     ON review_finding TYPE option<record<kvdag_version>>;
DEFINE INDEX finding_by_cycle ON review_finding FIELDS cycle, node_key;
```

`verdict = "replace"` with a null `replacement` is rejected by the store: the
spec requires that firing a teammate always proposes a suitable better
replacement in the same step, so the schema makes the pairing structural rather
than a matter of prompt discipline.

---

## 5. Versioning model

```
workflow ──head_version──▶ kvdag_version v3
                              │ parent
                              ▼
                           kvdag_version v2 ──▶ kvdag_version v1 (parent = NONE)
```

- Every revision — authored edits, imports, and every accepted self-improvement
  change — creates a **new** `kvdag_version` with a fresh set of `kvdag_node` and
  `kvdag_edge` records. Nothing is ever mutated in place.
- `node_key` is **stable across versions**. That is what makes a semantic diff
  possible ("node `implement`'s prompt changed; edge `plan→review` added") and
  what lets a checkpoint from run@v2 be restored into run@v3 when the node's
  contract is unchanged.
- `spec_digest` = SHA-256 over the canonical serialisation of the version's nodes
  and edges. Two versions with equal digests are the same graph; used to skip
  writing a no-op version and to detect an unchanged node during restore.
- Checkpoint compatibility across versions is decided per node by comparing the
  node's `output_schema` digest and `prompt_template` digest. Same output schema
  and same prompt ⇒ checkpoint is restorable; otherwise the node is offered for
  restore with an explicit "definition changed" warning and defaults to re-run.
- Runs always pin `kvdag_version`, never `workflow`. A run's graph can never
  change under it.

## 6. Query shapes the design depends on

```surql
-- the graph of a version, for rendering or compiling
SELECT *, ->kvdag_edge->kvdag_node AS out FROM kvdag_node WHERE version = $version;

-- the live run graph (projection source when replaying, not the hot path)
SELECT *, ->run_edge->run_node AS downstream, ->spawned->run_node AS children
FROM run_node WHERE run = $run ORDER BY depth, instance_path;

-- ready-set candidates: nodes whose every inbound edge has fired
SELECT * FROM run_node WHERE run = $run AND status = "pending"
  AND array::len((SELECT VALUE id FROM <-run_edge WHERE fired_at = NONE)) = 0;

-- checkpoint restore source set
SELECT * FROM node_checkpoint
WHERE run = $source_run AND kind = "result" AND schema_valid = true
  AND node_key IN $selectors;

-- run history for a workflow, newest first
SELECT id, status, tier, started_at, ended_at, nodes_done, nodes_total, total_tokens
FROM workflow_run WHERE workflow = $workflow ORDER BY started_at DESC LIMIT $n;

-- replay a run exactly
SELECT * FROM run_event WHERE run = $run ORDER BY seq;
```

`RELATE`-based edges with native `->edge->table` traversal are the reason
SurrealDB is a good fit here rather than a KV store: "what does node X depend on"
and "what did node X spawn" are one-line queries instead of hand-rolled recursive
joins.

**`LIVE SELECT` is not used** as the DAG-view update mechanism. Its behaviour
under `SurrealKv` (as opposed to `Mem`/remote) was not verified in Phase 0, and
it is unnecessary: the engine is in-process and authoritative, so it emits
`workflow.*` events directly. Adding a second, weaker change-notification path
would create two sources of truth for no benefit.

## 7. Payload budgets (token efficiency is a schema property, not a hope)

| Field | Budget | Overflow behaviour |
|---|---|---|
| `node_checkpoint.summary` | ≤ 1,200 chars | truncated with an explicit marker; the full text stays in `payload`/artifacts |
| `node_checkpoint.payload` | ≤ 256 KB serialised | spills to a file under the run dir, path recorded in `artifact_paths` |
| `run_summary.text` | ≤ 4,000 chars | summariser node is prompted with the budget; over-budget output fails schema validation and is retried once |
| `run_event.payload` | ≤ 16 KB | truncated with `truncated: true` |
| Node transcripts | never stored in the DB | referenced by `run_node.transcript_path`; Claude owns the file, so it may vanish — every reader stats it first and degrades explicitly (§4.4) |

An edge with `payload: "summary"` (the default) passes only
`node_checkpoint.summary`. `payload: "full"` passes `payload`, and is an explicit
per-edge decision visible in the graph — never a default, never implicit.

## 8. What is deliberately *not* stored

- Raw pane scrollback and ANSI output. The pane owns it; the transcript file is
  the durable record.
- Any TUI presentation state: DAG layout, node rects, selection, scroll/pan,
  collapsed subgraphs, colours. These are `AppState`/`ViewState` only. Putting
  them in the store would violate the runtime/client boundary guardrail and make
  the store a UI dependency.
- Model credentials or environment secrets.

## 9. Retention

Configurable, defaults: keep the most recent **50 runs per workflow**; older runs
are pruned whole (run, run_nodes, run_edges, run_events, checkpoints) except that
`run_summary` records are **never** pruned — they are small, they are the
cross-run context feed, and losing them would silently degrade every future run.
Pruning goes through `prune_run_history` (§3) and is journalled at the workflow
level.

**Pruning must not leave dangling record references,** and two fields point into
a pruned run:

- `run_summary.generated_by` is `option<record<run_node>>` — the prune **nulls
  it** and keeps the summary. The summary text is the durable artifact; the
  identity of the node that wrote it is not worth retaining a whole run for.
- `interrogation.run_node` is a **non-optional** `record<run_node>`, so an
  interrogation cannot outlive its node. The prune **deletes** `interrogation`
  rows whose `run_node` belongs to a pruned run. (It cannot be nulled without
  making the field optional, and an interrogation without its node is not
  meaningful.) `review_finding.interview` is optional and is nulled to match;
  `review_finding.interview_mode` is left as recorded, since it describes how the
  finding was reached and stays true after the interview record is gone.

**Consequences, stated rather than discovered:** once a run is pruned, both
restore modes are unavailable for it. Checkpoint restore reads `node_checkpoint`
rows scoped to the source run (§6), and interrogation restore reads
`run_node.transcript_path` — the prune removes the first outright and drops the
record that held the second. The run's `run_summary` survives and remains usable
as cross-run context, which is the whole point of exempting it. The run browser
therefore renders a pruned run as summary-only, with restore actions disabled and
a reason, never as a run whose restore silently returns nothing.

## 10. Test plan for the store layer

Cases 1–11 run against `kv-mem` — no disk, no PTY. Cases 12–13 need a real
on-disk `SurrealKv` lock and are marked `#[ignore]` by default (they are the only
two that touch the filesystem, and the only two that are slow):

1. Migrations apply cleanly to a fresh DB; re-applying is a no-op; `schema_meta`
   records the applied set.
2. `create_version` from a node/edge set produces a correct `spec_digest`;
   creating an identical graph twice yields the same digest and does not write a
   new version.
3. Version chain: v1 → v2 → v3 with stable `node_key`s; loading v1 after v3
   exists returns v1's graph byte-identically (immutability).
4. `RELATE` traversal returns correct upstream/downstream sets, including for a
   diamond and a fan-out of 12.
5. Cycle rejection: `create_version` refuses a node/edge set with a cycle.
6. `run_event` `seq` uniqueness under concurrent appends.
7. Checkpoint spill: a 512 KB payload writes an artifact and stores the path.
8. `review_finding` with `verdict = "replace"` and no `replacement` is rejected.
9. Restore query returns only `schema_valid = true` result checkpoints.
10. `prune_run_history` deletes whole runs and preserves every `run_summary`.
11. `prune_run_history` leaves **no dangling `record<run_node>` reference**:
    after pruning a run that had a summary and an interrogation, every surviving
    `run_summary.generated_by` is `NONE`, no `interrogation` row references a
    deleted `run_node`, and every surviving `review_finding.interview` is either
    `NONE` or points at a live `interrogation`.
12. *(on-disk, `#[ignore]`)* Store-locked path: opening the same directory twice
    yields `Unavailable { reason: "store_locked" }` rather than a panic.
13. *(on-disk, `#[ignore]`)* One `SurrealKv` round-trip (create → close → reopen
    → read), mirroring the Phase 0 probe that verified persistence.
