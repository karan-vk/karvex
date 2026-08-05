-- Initial schema: workflow definitions, immutable kvdag versioning, runs,
-- per-node checkpoints, run summaries, interrogation-restore records, and the
-- self-improvement review cycle.
--
-- Mirrors docs/design/workflow-builder/03-storage-schema.md §4 verbatim, plus
-- schema_meta (§2), which records which migrations have been applied.
--
-- kvdag_version, kvdag_node, kvdag_edge, node_checkpoint, and run_event are
-- append-only by construction (03 §3): the Rust store exposes no update or
-- delete method for them. There is no API to call.

-- ─── schema_meta: records which migrations have been applied ────────────────
DEFINE TABLE schema_meta SCHEMAFULL;
DEFINE FIELD version    ON schema_meta TYPE string;
DEFINE FIELD applied_at ON schema_meta TYPE datetime DEFAULT time::now();
DEFINE INDEX schema_meta_version ON schema_meta FIELDS version UNIQUE;

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
DEFINE FIELD args           ON kvdag_version TYPE array<object> FLEXIBLE DEFAULT [];
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
DEFINE FIELD output_schema  ON kvdag_node TYPE object FLEXIBLE;
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
DEFINE FIELD position       ON kvdag_node TYPE option<object> FLEXIBLE; -- authoring hint only
DEFINE INDEX kvdag_node_key ON kvdag_node FIELDS version, node_key UNIQUE;

-- ─── kvdag_edge: typed dependency between two nodes of one version ──────────
DEFINE TABLE kvdag_edge SCHEMAFULL TYPE RELATION FROM kvdag_node TO kvdag_node;
DEFINE FIELD kind           ON kvdag_edge TYPE string
       ASSERT $value IN ["sequence","data","conditional"];
-- typed, total, loop-free predicate over the source node's validated output
DEFINE FIELD condition      ON kvdag_edge TYPE option<object> FLEXIBLE;
-- how much of the source's checkpoint is handed to the target
DEFINE FIELD payload        ON kvdag_edge TYPE string
       ASSERT $value IN ["none","summary","full"] DEFAULT "summary";
-- names the slot the payload lands in inside the target's prompt template
DEFINE FIELD port           ON kvdag_edge TYPE option<string>;

-- ─── workflow_run: one execution of one kvdag_version ───────────────────────
DEFINE TABLE workflow_run SCHEMAFULL;
DEFINE FIELD workflow       ON workflow_run TYPE record<workflow>;
DEFINE FIELD kvdag_version  ON workflow_run TYPE record<kvdag_version>;
DEFINE FIELD tier           ON workflow_run TYPE string
       ASSERT $value IN ["auto","max","high","medium","low"];
DEFINE FIELD status         ON workflow_run TYPE string
       ASSERT $value IN ["pending","running","paused","succeeded","failed","cancelled"];
DEFINE FIELD args           ON workflow_run TYPE object FLEXIBLE DEFAULT {};
-- prior runs whose summaries are injected as context (spec assumption 3)
DEFINE FIELD context_runs   ON workflow_run TYPE array<record<workflow_run>> DEFAULT [];
-- checkpoint restore source, if any
DEFINE FIELD restore_from   ON workflow_run TYPE option<object> FLEXIBLE;
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
DEFINE FIELD failure        ON workflow_run TYPE option<object> FLEXIBLE;
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
DEFINE FIELD blocker        ON run_node TYPE option<object> FLEXIBLE;
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
DEFINE FIELD payload        ON run_event TYPE object FLEXIBLE DEFAULT {};
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
DEFINE FIELD payload        ON node_checkpoint TYPE object FLEXIBLE DEFAULT {};
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
DEFINE FIELD per_node       ON run_summary TYPE array<object> FLEXIBLE DEFAULT [];
DEFINE FIELD token_estimate ON run_summary TYPE int DEFAULT 0;
DEFINE FIELD generated_by   ON run_summary TYPE option<record<run_node>>;
DEFINE FIELD created_at     ON run_summary TYPE datetime DEFAULT time::now();
DEFINE INDEX run_summary_run ON run_summary FIELDS run UNIQUE;

-- ─── interrogation: a forked, read-only-by-intent revival of a past node's
--     Claude session ─────────────────────────────────────────────────────────
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
-- distinguishes a genuine resumed fork from an evidence-seeded reconstruction
DEFINE FIELD reconstructed  ON interrogation TYPE bool DEFAULT false;
DEFINE FIELD seeded_from    ON interrogation TYPE option<record<node_checkpoint>>;

-- ─── review_cycle / review_finding: self-improvement review ─────────────────
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
DEFINE FIELD evidence       ON review_finding TYPE object FLEXIBLE DEFAULT {};
-- the concrete change: prompt rewrite, or a node/edge delta
DEFINE FIELD proposed_change ON review_finding TYPE object FLEXIBLE DEFAULT {};
-- mandatory when verdict = "replace": a full replacement role definition.
DEFINE FIELD replacement    ON review_finding TYPE option<object> FLEXIBLE;
DEFINE FIELD accepted       ON review_finding TYPE bool DEFAULT false;
DEFINE FIELD applied_in     ON review_finding TYPE option<record<kvdag_version>>;
DEFINE INDEX finding_by_cycle ON review_finding FIELDS cycle, node_key;

-- verdict = "replace" with a null replacement is rejected by the store: firing
-- a teammate always proposes a suitable better replacement in the same step,
-- so the schema makes the pairing structural rather than a matter of prompt
-- discipline. This cannot be a field-level ASSERT on `replacement`: SurrealDB
-- skips a field's ASSERT entirely when an `option<T>` field's value is NONE —
-- exactly the "no replacement" case this needs to catch — and even when it
-- does run, a field's ASSERT context binds `$after`/`$value` to that field's
-- own value, not the whole record, so it cannot see `verdict` at all. An
-- event runs once the whole document is assembled, where `$after` genuinely
-- is the full record.
DEFINE EVENT review_finding_replace_requires_replacement ON TABLE review_finding
    WHEN $after.verdict = "replace" AND $after.replacement == NONE
    THEN { THROW "review_finding with verdict \"replace\" must set replacement" };
