-- Phase 2 retest fix: a node instance's own identity and its slot overrides
-- (`docs/design/workflow-builder/04-kvdag-and-execution.md` §3.4, 2026-08-08
-- amendment).
--
-- An expansion child is created from a template it shares with every sibling of
-- its generation, so the definition can say nothing about which of them is
-- which. The two facts that do distinguish them arrive with the proposal and
-- had nowhere to live:
--
--   * `label` — the `--label` the proposing node is *required* to supply. It
--     is the child's name in the DAG, in its pane title, in `claude --name`,
--     and at the top of its own `task.md`. A static node keeps the authored
--     kvdag label here, so every reader asks one column rather than joining
--     back to the definition and getting the template's name for all N.
--   * `inputs` — the accepted `--input k=v` slot overrides, already validated
--     against the template's declared `{{slot}}`s when the proposal was
--     accepted (§4 D3). The child's prompt is rendered at spawn time, after
--     the parent settles, so without a persisted copy the override is
--     validated, journalled, and then discarded before `task.md` is written.
--
-- Both are additive and defaulted, and both are backfilled for rows written by
-- an earlier karvex: a `DEFAULT` is applied at write time, not at read time.

DEFINE FIELD label  ON run_node TYPE string DEFAULT "";
-- flat string -> string; the proposal's `--input` pairs, never nested
DEFINE FIELD inputs ON run_node TYPE object FLEXIBLE DEFAULT {};

UPDATE run_node SET
    label  = IF label  = NONE THEN "" ELSE label  END,
    inputs = IF inputs = NONE THEN {} ELSE inputs END;
