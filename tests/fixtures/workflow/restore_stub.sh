#!/bin/sh
# Node stub for the Phase 3 restore end-to-end scenario
# (`docs/design/workflow-builder/07-phase3-plan.md` §WS-J scenario 2).
#
# Identical in contract to `node_stub.sh` — write `result.json`, self-report,
# stay alive — with one difference that the restore scenario is built on: the
# payload it writes **names the run that produced it**. A restored node is
# never executed in the new run, so the only way to prove the downstream node
# was fed run 1's checkpoint rather than a fresh one is for run 1's payload to
# be distinguishable from anything run 2 could have written.
#
# usage: restore_stub.sh <key>
#   <key>  the result key this node's `output_schema` requires

set -u

key="${1:-}"
if [ -z "$key" ]; then
  printf 'restore_stub: no result key given\n' >&2
  exit 64
fi

node_dir="${KARVEX_WORKFLOW_NODE_DIR:-}"
if [ -z "$node_dir" ]; then
  printf 'restore_stub: KARVEX_WORKFLOW_NODE_DIR is not set\n' >&2
  exit 64
fi

run_id="${KARVEX_WORKFLOW_RUN_ID:-unknown-run}"
marker="$key payload from $run_id"

printf 'restore-stub ready path=%s key=%s\n' "${KARVEX_WORKFLOW_NODE_PATH:-?}" "$key"

# An explicit `summary` key, so the edge's `payload = "summary"` carries this
# exact string into the downstream node's `inputs/` rather than a canonical
# rendering of the whole payload.
printf '{"%s":"%s","summary":"%s"}' "$key" "$marker" "$marker" > "$node_dir/result.json"

kvx workflow node complete || printf 'restore-stub: report exited non-zero\n'
printf 'restore-stub reported\n'

while :; do
  sleep 1
done
