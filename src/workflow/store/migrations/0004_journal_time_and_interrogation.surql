-- Phase 3 foundation: journal timestamp authority, interrogation's deferred
-- forked-session id, the new `summary` journal kind, the epilogue's optional
-- kvdag node, retention's workflow-level counter, and the summary-list index
-- (`docs/design/workflow-builder/07-phase3-plan.md` §4 D15). Seven statements,
-- nothing more: the six schema changes numbered below, plus the one-line
-- `workflow.pruned_runs` backfill `UPDATE` that item (5) needs because its
-- `DEFAULT` only applies at write time.

-- (1) `run_event.at` loses `DEFAULT time::now()` — the second clock migration
-- `0002` killed for `workflow_run.started_at`, applied to the journal (§4
-- D14). `write_run_event` now binds `at` explicitly from the producer's own
-- `at_unix_ms`, so the database must no longer be able to mint a competing,
-- later timestamp.
DEFINE FIELD OVERWRITE at ON run_event TYPE datetime;

-- (2) `interrogation.forked_session_id` becomes optional: the forked id is
-- only pre-assignable when `--session-id` combines with `--resume
-- --fork-session` (§4 D7); otherwise it is learned later from the pane's
-- session report and the record must be creatable before it is known.
DEFINE FIELD OVERWRITE forked_session_id ON interrogation TYPE option<string>;

-- (3) `run_event.kind` gains the `summary` journal entry the epilogue writes
-- when its result is accepted (§4 D10 — the read path is `run_summary`, not
-- this payload; the journal entry exists for replay and audit only).
DEFINE FIELD OVERWRITE kind ON run_event TYPE string
       ASSERT $value IN ["run_started","run_finished","node_created","node_started",
                         "node_status","node_output","tool_activity","plan",
                         "usage","message_delivered","steer","interrupt",
                         "expand_proposed","expand_accepted","expand_rejected",
                         "growth_limited","watchdog","checkpoint","succession",
                         "error","summary"];

-- (4) `run_node.kvdag_node` becomes optional: the engine-owned `.summary`
-- epilogue node is a `run_node` row with no kvdag definition behind it (§4
-- D5, B1). Only the reserved `.summary` path may ever write the NULL —
-- `write_epilogue_node_created` asserts the reserved path before binding
-- `NONE`, and a store test proves a non-reserved-path create with no kvdag
-- node is rejected with `StoreError::Invariant`, so this loosened column can
-- never be abused by an ordinary node write.
DEFINE FIELD OVERWRITE kvdag_node ON run_node TYPE option<record<kvdag_node>>;

-- (5) `workflow.pruned_runs` — the retention counter §4 D12 bumps instead of
-- a dedicated journal table; the `workflow` table is SCHEMAFULL, so the
-- write fails without this column. A `DEFAULT` is applied at write time, not
-- at read time (the `0002`/`0003` precedent), so a row written before this
-- migration decodes with the column absent; backfill it explicitly, or the
-- first prune against an untouched pre-0004 workflow fails to add an `int`
-- to `NONE`.
DEFINE FIELD pruned_runs ON workflow TYPE int DEFAULT 0;
UPDATE workflow SET pruned_runs = IF pruned_runs = NONE THEN 0 ELSE pruned_runs END;

-- (6) `run_summary` is the one never-pruned table (unbounded growth by
-- design), and §4 D9's summary listing filters it through
-- `kvdag_version.workflow` (M9) rather than the dangling `run_summary.run`
-- reference, so the cross-record filter gets an index instead of degrading
-- into a table scan as history grows.
DEFINE INDEX run_summary_version ON run_summary FIELDS kvdag_version, created_at;
