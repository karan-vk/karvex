-- Phase 2: dynamic growth provenance and the measured node history `auto`
-- resolves from (`docs/design/workflow-builder/06-phase2-plan.md` WS-C).
--
-- Three additive `run_node` columns and one redefinition:
--
--   * `assignment_reason` — the §7.3 reason string behind the node's
--     (model, effort). Written verbatim from `NewRun.assignments`, so a
--     finished run can still be explained (§4 D9).
--   * `first_pass_succeeded` / `schema_failures` — the two `NodeHistory`
--     inputs that can be truthful today (§4 D8). `watchdog_interventions`
--     already exists (0001) and stays 0 until Phase 4 writes it;
--     `mean_tokens` is read from `total_tokens`, which is documented as
--     permanently 0 and is deliberately not consulted by `resolve_auto`.
--   * `workflow_run.started_at` loses `DEFAULT time::now()` (§4 D15 / H1).
--     `create_run` binds it explicitly from `NewRun.started_at_unix_ms`, so
--     the database must no longer be able to mint a competing clock. A plain
--     `DEFINE FIELD` over an existing schemafull field is an error rather
--     than an idempotent redefinition, hence `OVERWRITE`.

-- ─── run_node: assignment provenance and measured history ───────────────────
DEFINE FIELD assignment_reason    ON run_node TYPE string DEFAULT "";
-- true only when the node reached "succeeded" on attempt 1; recomputed on
-- every terminal node write, so a restart that succeeds on attempt 2 reads
-- false.
DEFINE FIELD first_pass_succeeded ON run_node TYPE bool DEFAULT false;
-- how many checkpoints this node wrote that failed schema validation
DEFINE FIELD schema_failures      ON run_node TYPE int DEFAULT 0;

-- A `DEFAULT` is applied at write time, not at read time, so rows written
-- under 0001 would decode with the columns absent. Backfill them explicitly.
UPDATE run_node SET
    assignment_reason    = IF assignment_reason    = NONE THEN "" ELSE assignment_reason END,
    first_pass_succeeded = IF first_pass_succeeded = NONE THEN false ELSE first_pass_succeeded END,
    schema_failures      = IF schema_failures      = NONE THEN 0 ELSE schema_failures END;

-- ─── workflow_run: one authority for started_at ─────────────────────────────
DEFINE FIELD OVERWRITE started_at ON workflow_run TYPE datetime;
